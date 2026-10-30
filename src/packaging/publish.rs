//! `magnet publish`: which archives get published, and the draft → upload → finalise flow,
//! see DESIGN.md "Publishing a package"

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{ensure, Context};
use pubgrub::SemanticVersion;
use sha2::{Digest, Sha256};
use tracing::info;

use crate::{
    config::constants::{MAGNET_LOCK_FILE, MAGNET_PROJECT_FILE},
    git_client::models::GitArtifact,
    http_client::{
        basic::{AuthToken, MagnetClient},
        models::{PublishedVersion, VersionState},
        publish::Upload,
    },
    models::{
        directories::ProjectDirectory,
        package::{ArtifactKey, ArtifactKind, ArtifactTags},
        project_manifest::{MagnetManifestV1, MagnetManifestV1DependenciesValue},
    },
    packaging::naming::artifact_file_name,
};

/// An archive in `dist/` which gets published
#[derive(Debug, Clone, PartialEq)]
pub struct PublishArchive {
    pub path: PathBuf,
    pub kind: ArtifactKind,
    pub tags: ArtifactTags,
}

impl PublishArchive {
    pub fn key(&self) -> ArtifactKey {
        ArtifactKey::new(self.kind, self.tags.clone())
    }
}

/// Refuses manifests which others couldn't reproduce: `path` dependencies, `[overrides]`,
/// dependencies of another index, and git or legacy quark dependencies which aren't pinned via
/// a full commit SHA as `ref`, which the index line copies (DESIGN.md "Publishing a package")
/// `true` for a full (40 hex digits) git commit SHA
fn is_full_sha(rev: &str) -> bool {
    rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn check_publishable(manifest: &MagnetManifestV1) -> anyhow::Result<()> {
    ensure!(
        !manifest.artifact_kinds().is_empty(),
        "{MAGNET_PROJECT_FILE} has neither [artifact.quark] nor [artifact.lepton], so the version would contain no kind of artifact"
    );
    let mut problems = vec![];
    for (name, dependency) in &manifest.dependencies {
        let MagnetManifestV1DependenciesValue::Object {
            git,
            index,
            legacy_quark,
            path,
            ..
        } = dependency
        else {
            continue;
        };
        if path.is_some() {
            problems.push(format!("{name} is a local path dependency"));
        }
        if index.is_some() {
            problems.push(format!("{name} comes from another index"));
        }
        let hint = format!(
            "pin it via `ref = \"<commit SHA>\"`, e.g. the one of {name} in {MAGNET_LOCK_FILE}"
        );
        match git {
            // the index line needs the repository, which only the lock file knows then
            Some(git) if git.url.is_none() => problems.push(format!(
                "{name} is a legacy quark pinned by name, add `url = \"…\"` and `ref = \"<commit SHA>\"` to its git table, e.g. from {MAGNET_LOCK_FILE}"
            )),
            Some(git) if git.ref_.as_deref().is_some_and(is_full_sha) => {}
            Some(git) if git.ref_.is_some() => {
                problems.push(format!("{name} is pinned to an abbreviated commit, {hint}"))
            }
            Some(git) if git.tag.is_some() => problems.push(format!(
                "{name} is pinned to a git tag, which can move, {hint}"
            )),
            Some(git) if git.branch.is_some() => {
                problems.push(format!("{name} is pinned to a git branch, {hint}"))
            }
            Some(_) => problems.push(format!("{name} is a git dependency without a ref, {hint}")),
            None if legacy_quark.unwrap_or(false) => problems.push(format!(
                "{name} is a legacy quark without a git URL and ref, add `git = {{ url = \"…\", ref = \"<commit SHA>\" }}`, e.g. from {MAGNET_LOCK_FILE}"
            )),
            None => {}
        }
    }
    let mut overrides: Vec<_> = manifest.overrides.keys().collect();
    overrides.sort();
    for name in overrides {
        problems.push(format!("{name} is overridden by a local folder"));
    }
    ensure!(
        problems.is_empty(),
        "Others couldn't reproduce the dependencies of {MAGNET_PROJECT_FILE}, so it can't be published:\n  {}",
        problems.join("\n  ")
    );
    Ok(())
}

/// The archives to publish: the given `files`, or without any, all archives of the manifest's
/// version in `dist/`. Their kind and tags follow the known `{name}-{version}-` prefix of the
/// file name, since kinds and tags never contain a `-`. The source comes first, as the server
/// needs it before a version can be finalised.
pub fn publish_archives(
    root: &Path,
    manifest: &MagnetManifestV1,
    files: &[PathBuf],
) -> anyhow::Result<Vec<PublishArchive>> {
    let prefix = file_name_prefix(manifest)?;
    let mut archives = match files.is_empty() {
        true => {
            let dist = root.join(String::from(ProjectDirectory::Dist));
            let mut archives = vec![];
            if dist.is_dir() {
                for entry in
                    fs::read_dir(&dist).with_context(|| format!("Could not read {dist:?}"))?
                {
                    let path = entry?.path();
                    if let Some((kind, tags)) = file_name_artifact(&path, &prefix) {
                        archives.push(PublishArchive { path, kind, tags });
                    }
                }
            }
            ensure!(
                !archives.is_empty(),
                "There are no packages of {} {} in {dist:?}, run `magnet package` first",
                manifest.package.name,
                manifest.package.version.as_str()
            );
            archives
        }
        false => files
            .iter()
            .map(|path| {
                ensure!(path.is_file(), "{path:?} does not exist");
                let (kind, tags) = file_name_artifact(path, &prefix).with_context(|| {
                    format!(
                        "{path:?} is no package of {} {}, its name has to start with {prefix}",
                        manifest.package.name,
                        manifest.package.version.as_str()
                    )
                })?;
                Ok(PublishArchive {
                    path: path.clone(),
                    kind,
                    tags,
                })
            })
            .collect::<anyhow::Result<_>>()?,
    };
    archives.sort_by_key(|archive| (archive.kind, archive.path.clone()));
    archives.dedup();
    Ok(archives)
}

/// `{name}-{version}-`
fn file_name_prefix(manifest: &MagnetManifestV1) -> anyhow::Result<String> {
    let version = SemanticVersion::try_from(manifest.package.version.clone())?;
    let kind = ArtifactKind::Source;
    let tags = ArtifactTags::any();
    let source = artifact_file_name(&manifest.package.name, &version, &kind, &tags);
    Ok(source
        .strip_suffix(&format!("{kind}-{tags}.tar.gz"))
        .context("Unexpected package file name")?
        .to_string())
}

fn file_name_artifact(path: &Path, prefix: &str) -> Option<(ArtifactKind, ArtifactTags)> {
    let rest = path
        .file_name()?
        .to_str()?
        .strip_prefix(prefix)?
        .strip_suffix(".tar.gz")?;
    let (kind, tags) = rest.split_once('-')?;
    let (abi, platform) = tags.split_once('-')?;
    Some((kind.parse().ok()?, ArtifactTags::new(abi, platform).ok()?))
}

/// What a version with `kinds` lacks before it can be finalised (DESIGN.md "Publishing a
/// package"): its source, its quark if it contains one, and at least one binary of a lepton
/// if it contains one, which other platforms can be added to later
fn missing_for_finalise(kinds: &[ArtifactKind], artifacts: &[GitArtifact]) -> Vec<String> {
    let keys: Vec<ArtifactKey> = artifacts
        .iter()
        .filter_map(GitArtifact::artifact_key)
        .collect();
    let has = |kind: ArtifactKind| keys.iter().any(|key| key.kind == kind);
    let mut missing = vec![];
    if !has(ArtifactKind::Source) {
        missing.push("the source".to_string());
    }
    for kind in kinds {
        match kind {
            ArtifactKind::Quark if !has(*kind) => missing.push("the quark".to_string()),
            ArtifactKind::Lepton if !has(*kind) => {
                missing.push("a binary of the lepton".to_string())
            }
            _ => {}
        }
    }
    missing
}

/// Creates (or reuses) the draft, uploads the archives and finalises the version once it has
/// the artifacts it needs ([missing_for_finalise]). Every step can be repeated, e.g. by the jobs
/// of a CI matrix.
pub async fn publish(
    client: &MagnetClient<AuthToken>,
    manifest: &MagnetManifestV1,
    archives: &[PublishArchive],
) -> anyhow::Result<PublishedVersion> {
    let name = MagnetManifestV1::normalize_name(&manifest.package.name);
    let version = manifest.package.version.to_string();

    let mut published = client.create_draft(&name, &version).await?;
    for archive in archives {
        let content = fs::read(&archive.path)
            .with_context(|| format!("Could not read {:?}", archive.path))?;
        let sha256 = format!("{:x}", Sha256::digest(&content));
        let file = archive
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let key = archive.key();
        match client
            .upload_artifact(&name, &version, &key, content, &sha256)
            .await
            .with_context(|| format!("Could not upload {file}"))?
        {
            Upload::Created => info!("Uploaded {file}"),
            Upload::AlreadyPresent => info!("{file} was already uploaded"),
        }
        published
            .artifacts
            .retain(|artifact| artifact.key() != key.to_string());
        published.artifacts.push(GitArtifact {
            kind: key.kind.to_string(),
            abi: key.tags.abi,
            platform: key.tags.platform,
            sha256,
        });
    }

    if published.state == VersionState::Draft {
        let missing = missing_for_finalise(&manifest.artifact_kinds(), &published.artifacts);
        if missing.is_empty() {
            published = client.finalise(&name, &version).await?;
        } else {
            info!(
                "{name} {version} stays a draft until {} got published",
                missing.join(" and ")
            );
        }
    }
    Ok(published)
}

#[cfg(test)]
mod tests {
    use wiremock::{
        matchers::{body_bytes, body_json, header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

    /// a manifest whose `[artifact]` tables declare `kinds` (`quark`, `lepton` or both)
    fn manifest(kinds: &str, dependencies: &str) -> MagnetManifestV1 {
        let mut artifact = String::new();
        if kinds.contains("quark") {
            artifact.push_str("[artifact.quark]\npaths = [\"*.sc\"]\n");
        }
        if kinds.contains("lepton") {
            artifact.push_str("[artifact.lepton]\nplatform-paths = {}\n");
        }
        MagnetManifestV1::from_string(&format!(
            "magnet-version = 1\n[package]\nname = \"Foo_Bar\"\nversion = \"1.2.0\"\n{artifact}[dependencies]\n{dependencies}"
        ))
        .unwrap()
    }

    #[test]
    fn test_publishable_needs_a_kind() {
        let err = check_publishable(&manifest("", "")).unwrap_err();
        assert!(err.to_string().contains("no kind of artifact"), "{err}");
    }

    #[test]
    fn test_publishable_dependencies() {
        check_publishable(&manifest(
            "quark",
            "a = \"^1\"\nb = { version = \"1.0\" }\nc = { git = { url = \"https://x.org/c.git\", ref = \"4b825dc642cb6eb9a060e54bf8d69288fbee4904\" } }\nd = { legacy-quark = true, git = { url = \"https://x.org/d.git\", ref = \"4b825dc642cb6eb9a060e54bf8d69288fbee4904\" } }\n",
        ))
        .unwrap();
    }

    #[test]
    fn test_refused_dependencies() {
        let err = check_publishable(&manifest(
            "quark",
            "a = { path = \"../a\" }\nb = { index = \"https://other.org/index\" }\nc = { git = { url = \"https://x.org/c.git\", branch = \"main\" } }\nd = { git = { url = \"https://x.org/d.git\" } }\ne = { legacy-quark = true }\ng = { git = { url = \"https://x.org/g.git\", tag = \"v1\" } }\nh = { legacy-quark = true, git = { url = \"https://x.org/h.git\", ref = \"4b825dc\" } }\ni = { legacy-quark = true, git = { ref = \"4b825dc642cb6eb9a060e54bf8d69288fbee4904\" } }\n[overrides]\nf = { path = \"../f\" }\n",
        ))
        .unwrap_err()
        .to_string();
        for expected in [
            "a is a local path dependency",
            "b comes from another index",
            "c is pinned to a git branch",
            "d is a git dependency without a ref",
            "e is a legacy quark without a git URL and ref",
            "g is pinned to a git tag",
            "h is pinned to an abbreviated commit",
            "i is a legacy quark pinned by name, add `url",
            "f is overridden by a local folder",
        ] {
            assert!(err.contains(expected), "{expected}: {err}");
        }
    }

    fn dist(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("dist")).unwrap();
        for file in files {
            fs::write(dir.path().join("dist").join(file), file).unwrap();
        }
        dir
    }

    fn names(archives: &[PublishArchive]) -> Vec<(String, String)> {
        archives
            .iter()
            .map(|archive| {
                (
                    archive
                        .path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                    format!("{}-{}", archive.kind, archive.tags),
                )
            })
            .collect()
    }

    #[test]
    fn test_archives_of_the_current_version() {
        let dir = dist(&[
            "foo-bar-1.2.0-lepton-api3_0-macos_arm64.tar.gz",
            "foo-bar-1.2.0-quark-any-any.tar.gz",
            "foo-bar-1.2.0-source-any-any.tar.gz",
            // other versions, unknown kinds or other files are ignored
            "foo-bar-1.1.0-source-any-any.tar.gz",
            "foo-bar-1.2.0-gluon-any-any.tar.gz",
            "foo-bar-1.2.0-lepton-api-3-macos.tar.gz",
            ".tmpXyZ",
        ]);
        let archives = publish_archives(dir.path(), &manifest("lepton", ""), &[]).unwrap();
        // the source comes first
        assert_eq!(
            names(&archives),
            [
                (
                    "foo-bar-1.2.0-source-any-any.tar.gz".to_string(),
                    "source-any-any".to_string()
                ),
                (
                    "foo-bar-1.2.0-quark-any-any.tar.gz".to_string(),
                    "quark-any-any".to_string()
                ),
                (
                    "foo-bar-1.2.0-lepton-api3_0-macos_arm64.tar.gz".to_string(),
                    "lepton-api3_0-macos_arm64".to_string()
                ),
            ]
        );
    }

    #[test]
    fn test_explicit_archives() {
        let dir = dist(&[
            "foo-bar-1.2.0-lepton-api3_0-macos_arm64.tar.gz",
            "foo-bar-1.2.0-source-any-any.tar.gz",
            "foo-bar-1.1.0-source-any-any.tar.gz",
        ]);
        let file = dir
            .path()
            .join("dist/foo-bar-1.2.0-lepton-api3_0-macos_arm64.tar.gz");
        let archives =
            publish_archives(dir.path(), &manifest("lepton", ""), &[file.clone()]).unwrap();
        assert_eq!(
            archives,
            [PublishArchive {
                path: file,
                kind: ArtifactKind::Lepton,
                tags: ArtifactTags::new("api3_0", "macos_arm64").unwrap()
            }]
        );

        let other = dir.path().join("dist/foo-bar-1.1.0-source-any-any.tar.gz");
        let err = publish_archives(dir.path(), &manifest("lepton", ""), &[other]).unwrap_err();
        assert!(err.to_string().contains("foo-bar-1.2.0-"), "{err}");

        let missing = dir.path().join("dist/missing.tar.gz");
        assert!(publish_archives(dir.path(), &manifest("lepton", ""), &[missing]).is_err());
    }

    #[test]
    fn test_no_archives() {
        let dir = tempfile::tempdir().unwrap();
        let err = publish_archives(dir.path(), &manifest("quark", ""), &[]).unwrap_err();
        assert!(err.to_string().contains("magnet package"), "{err}");
    }

    const VERSIONS: &str = "/api/v1/packages/foo-bar/versions/";

    fn version(state: &str, artifacts: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"name": "foo-bar", "version": "1.2.0", "state": state, "artifacts": artifacts})
    }

    async fn server(draft: (u16, serde_json::Value)) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(VERSIONS))
            .and(header("authorization", "Bearer token"))
            .and(body_json(serde_json::json!({"version": "1.2.0"})))
            .respond_with(ResponseTemplate::new(draft.0).set_body_json(draft.1))
            .expect(1)
            .mount(&server)
            .await;
        server
    }

    fn client(server: &MockServer) -> MagnetClient<AuthToken> {
        MagnetClient::new(server.uri().parse::<url::Url>().unwrap())
            .with_state(AuthToken::Env("token".to_string()))
    }

    async fn expect_upload(server: &MockServer, key: &str, content: &[u8], status: u16) {
        let key: ArtifactKey = key.parse().unwrap();
        Mock::given(method("PUT"))
            .and(path(format!(
                "{VERSIONS}1.2.0/artifacts/{}/{}/{}/",
                key.kind, key.tags.abi, key.tags.platform
            )))
            .and(header("authorization", "Bearer token"))
            .and(header(
                crate::http_client::publish::SHA256_HEADER,
                format!("{:x}", Sha256::digest(content)).as_str(),
            ))
            .and(body_bytes(content.to_vec()))
            .respond_with(ResponseTemplate::new(status).set_body_json(
                serde_json::json!({"detail": "an artifact with another checksum exists"}),
            ))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn expect_finalise(server: &MockServer, times: u64, artifacts: serde_json::Value) {
        Mock::given(method("POST"))
            .and(path(format!("{VERSIONS}1.2.0/finalise/")))
            .respond_with(ResponseTemplate::new(200).set_body_json(version("final", artifacts)))
            .expect(times)
            .mount(server)
            .await;
    }

    fn quark_archive(dir: &tempfile::TempDir) -> Vec<PublishArchive> {
        publish_archives(dir.path(), &manifest("quark", ""), &[]).unwrap()
    }

    const QUARK: &str = "foo-bar-1.2.0-quark-any-any.tar.gz";
    const SOURCE: &str = "foo-bar-1.2.0-source-any-any.tar.gz";

    #[tokio::test]
    async fn test_fresh_publish() {
        let dir = dist(&[QUARK, SOURCE]);
        let server = server((201, version("draft", serde_json::json!([])))).await;
        expect_upload(&server, "source-any-any", SOURCE.as_bytes(), 201).await;
        expect_upload(&server, "quark-any-any", QUARK.as_bytes(), 201).await;
        expect_finalise(&server, 1, serde_json::json!([])).await;

        let published = publish(
            &client(&server),
            &manifest("quark", ""),
            &quark_archive(&dir),
        )
        .await
        .unwrap();
        assert_eq!(published.state, VersionState::Final);
    }

    #[tokio::test]
    async fn test_publish_again_is_fine() {
        let dir = dist(&[QUARK, SOURCE]);
        let sha = |file: &str| format!("{:x}", Sha256::digest(file.as_bytes()));
        let artifacts = serde_json::json!([
            {"kind": "source", "abi": "any", "platform": "any", "sha256": sha(SOURCE)},
            {"kind": "quark", "abi": "any", "platform": "any", "sha256": sha(QUARK)},
        ]);
        let server = server((200, version("final", artifacts))).await;
        expect_upload(&server, "source-any-any", SOURCE.as_bytes(), 200).await;
        expect_upload(&server, "quark-any-any", QUARK.as_bytes(), 200).await;
        // a final version is not finalised again
        expect_finalise(&server, 0, serde_json::json!([])).await;

        let published = publish(
            &client(&server),
            &manifest("quark", ""),
            &quark_archive(&dir),
        )
        .await
        .unwrap();
        assert_eq!(published.state, VersionState::Final);
    }

    #[tokio::test]
    async fn test_append_to_final_version() {
        let binary = "foo-bar-1.2.0-lepton-api3_0-linux_x86_64.tar.gz";
        let dir = dist(&[binary]);
        let artifacts = serde_json::json!([{"kind": "source", "abi": "any", "platform": "any", "sha256": "abc"}]);
        let server = server((200, version("final", artifacts))).await;
        expect_upload(
            &server,
            "lepton-api3_0-linux_x86_64",
            binary.as_bytes(),
            201,
        )
        .await;
        expect_finalise(&server, 0, serde_json::json!([])).await;

        let archives = publish_archives(dir.path(), &manifest("lepton", ""), &[]).unwrap();
        let published = publish(&client(&server), &manifest("lepton", ""), &archives)
            .await
            .unwrap();
        assert_eq!(published.artifacts.len(), 2);
    }

    #[tokio::test]
    async fn test_binary_without_source_stays_draft() {
        let binary = "foo-bar-1.2.0-lepton-api3_0-linux_x86_64.tar.gz";
        let dir = dist(&[binary]);
        let server = server((201, version("draft", serde_json::json!([])))).await;
        expect_upload(
            &server,
            "lepton-api3_0-linux_x86_64",
            binary.as_bytes(),
            201,
        )
        .await;
        expect_finalise(&server, 0, serde_json::json!([])).await;

        let archives = publish_archives(dir.path(), &manifest("lepton", ""), &[]).unwrap();
        let published = publish(&client(&server), &manifest("lepton", ""), &archives)
            .await
            .unwrap();
        assert_eq!(published.state, VersionState::Draft);
    }

    /// e.g. the jobs of a CI matrix: the first one finalises once the version has everything
    #[tokio::test]
    async fn test_quark_and_lepton_in_several_jobs() {
        let binary = "foo-bar-1.2.0-lepton-api3_0-linux_x86_64.tar.gz";
        let dir = dist(&[SOURCE, QUARK, binary]);
        let server = server((201, version("draft", serde_json::json!([])))).await;
        for (key, file) in [
            ("source-any-any", SOURCE),
            ("quark-any-any", QUARK),
            ("lepton-api3_0-linux_x86_64", binary),
        ] {
            expect_upload(&server, key, file.as_bytes(), 201).await;
        }
        expect_finalise(&server, 1, serde_json::json!([])).await;
        let manifest = manifest("quark lepton", "");
        let archives = publish_archives(dir.path(), &manifest, &[]).unwrap();
        let published = publish(&client(&server), &manifest, &archives)
            .await
            .unwrap();
        assert_eq!(published.state, VersionState::Final);
    }

    #[test]
    fn test_missing_for_finalise() {
        let artifact = |key: &str| {
            let key: ArtifactKey = key.parse().unwrap();
            GitArtifact {
                kind: key.kind.to_string(),
                abi: key.tags.abi,
                platform: key.tags.platform,
                sha256: "abc".to_string(),
            }
        };
        let both = [ArtifactKind::Quark, ArtifactKind::Lepton];
        assert_eq!(
            missing_for_finalise(&both, &[]),
            ["the source", "the quark", "a binary of the lepton"]
        );
        assert_eq!(
            missing_for_finalise(
                &both,
                &[artifact("source-any-any"), artifact("quark-any-any")]
            ),
            ["a binary of the lepton"]
        );
        // any platform will do, others can follow
        assert!(missing_for_finalise(
            &both,
            &[
                artifact("source-any-any"),
                artifact("quark-any-any"),
                artifact("lepton-api3_0-macos_arm64"),
            ]
        )
        .is_empty());
        assert!(
            missing_for_finalise(&[ArtifactKind::Quark], &[artifact("quark-any-any")])
                == ["the source"]
        );
    }

    #[tokio::test]
    async fn test_checksum_conflict() {
        let dir = dist(&[QUARK]);
        let server = server((200, version("final", serde_json::json!([])))).await;
        expect_upload(&server, "quark-any-any", QUARK.as_bytes(), 409).await;
        expect_finalise(&server, 0, serde_json::json!([])).await;

        let err = publish(
            &client(&server),
            &manifest("quark", ""),
            &quark_archive(&dir),
        )
        .await
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains(QUARK), "{err}");
        assert!(err.contains("409"), "{err}");
        assert!(err.contains("another checksum"), "{err}");
    }

    /// e.g. after the server was reset: the stored login is removed, so `magnet login` doesn't
    /// consider itself logged in anymore
    #[tokio::test]
    async fn test_rejected_login() {
        let dir = dist(&[QUARK]);
        let server = server((
            401,
            serde_json::json!({"detail": "Authentication credentials were not provided."}),
        ))
        .await;
        let path = dir.path().join("credentials.json");
        let token = crate::http_client::basic::OAuthToken {
            // the mock server expects `Bearer token`
            access_token: "token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: u64::MAX,
            scope: "read publish".to_string(),
            refresh_token: "stale-refresh".to_string(),
            pkce_verifier: "verifier".to_string(),
        };
        token.store(&path).unwrap();
        let client = MagnetClient::new(server.uri().parse::<url::Url>().unwrap()).with_state(
            AuthToken::Login {
                token,
                path: path.clone(),
            },
        );
        let err = publish(&client, &manifest("quark", ""), &quark_archive(&dir))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "The server rejected the stored login, so it was removed. Run `magnet login` again: Authentication credentials were not provided."
        );
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn test_rejected_token() {
        let dir = dist(&[QUARK]);
        let server = server((401, serde_json::json!({"detail": "Invalid token."}))).await;
        let err = publish(
            &client(&server),
            &manifest("quark", ""),
            &quark_archive(&dir),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("MAGNET_TOKEN"), "{err}");
        assert!(err.contains("Invalid token."), "{err}");
    }
}
