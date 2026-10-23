use anyhow::{anyhow, ensure, Context};
use git2::Repository;
use pubgrub::SemanticVersion;
use std::{fs::read_to_string, path::PathBuf};

use tracing::{info, instrument, trace, warn};
use url::Url;

use crate::git_client::models::{GitIndexConfig, GitRelease, INDEX_FORMAT};
use crate::models::project_manifest::MagnetManifestV1;

pub const INDEX_CONFIG_FILE: &str = "config.json";

pub struct GitClient {
    url: Url,
    local_destination: PathBuf,
    /// `None` if the index is a plain local directory, which is never updated
    repo: Option<Repository>,
}

impl GitClient {
    pub fn new(url: Url, local_destination: PathBuf) -> anyhow::Result<Self> {
        let client = match local_destination.exists() {
            true => {
                let repo = Repository::open(&local_destination)?;
                // @todo check if repo matches the designated URL - otherwise delete it and clone
                Self {
                    url,
                    local_destination,
                    repo: Some(repo),
                }
            }
            false => Self::clone(url, local_destination)?,
        };
        client.config()?;
        Ok(client)
    }

    fn clone(url: Url, local_destination: PathBuf) -> anyhow::Result<Self> {
        info!("Cloning index repo {url:?} to {local_destination:?}");
        let repo = Repository::clone(url.as_str(), &local_destination)?;
        Ok(Self {
            url,
            local_destination,
            repo: Some(repo),
        })
    }

    /// Uses an index from a local directory as is, without cloning or updating it.
    pub fn open_local(path: PathBuf) -> anyhow::Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("Could not find index directory {path:?}"))?;
        let client = Self {
            url: Url::from_directory_path(&path)
                .map_err(|_| anyhow!("Could not convert {path:?} to an URL"))?,
            local_destination: path,
            repo: None,
        };
        client.config()?;
        Ok(client)
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Reads the `config.json` of the index and refuses an unknown `index-format`
    pub fn config(&self) -> anyhow::Result<GitIndexConfig> {
        let config_path = self.local_destination.join(INDEX_CONFIG_FILE);
        let config: GitIndexConfig = serde_json::from_str(
            &read_to_string(&config_path)
                .with_context(|| format!("Could not open index config {config_path:?}"))?,
        )
        .with_context(|| format!("Could not parse index config {config_path:?}"))?;
        ensure!(
            config.index_format == INDEX_FORMAT,
            "The index {} uses index-format {}, but this magnet only supports {INDEX_FORMAT} - please update magnet",
            self.url,
            config.index_format
        );
        Ok(config)
    }

    pub fn update(&self) -> anyhow::Result<()> {
        match self.repo {
            Some(_) => self.hard_reset_to_remote(),
            None => Ok(()),
        }
    }

    fn repo(&self) -> anyhow::Result<&Repository> {
        self.repo
            .as_ref()
            .context("A local index is not a git repository")
    }

    fn fetch_origin(&self) -> anyhow::Result<()> {
        let _ = self
            .repo()?
            .find_remote("origin")?
            .fetch(&["main"], None, None);
        Ok(())
    }

    fn hard_reset_to_remote(&self) -> anyhow::Result<()> {
        let _ = self.fetch_origin();

        let repo = self.repo()?;
        let oid = repo.refname_to_id("refs/remotes/origin/main")?;
        let object = repo.find_object(oid, None)?;
        repo.reset(&object, git2::ResetType::Hard, None)?;
        Ok(())
    }

    /// All releases of a package, read from `<first letter>/<normalized name>.jsonl`
    #[instrument(skip(self), fields(release_path=tracing::field::Empty))]
    pub fn get_releases(&self, name: &str) -> anyhow::Result<Option<Vec<GitRelease>>> {
        let name = MagnetManifestV1::normalize_name(name);
        let release_path = self
            .local_destination
            .join(
                name.get(0..1)
                    .with_context(|| "A dependency name must have at least one character")?,
            )
            .join(format!("{name}.jsonl"));

        tracing::Span::current().record("release_path", release_path.to_str());

        match release_path.is_file() {
            true => {
                let releases = read_to_string(&release_path)
                    .with_context(|| format!("Could not open git index file {release_path:?}"))?
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .filter_map(|line| match serde_json::from_str(line) {
                        Ok(release) => Some(release),
                        Err(err) => {
                            warn!(
                                "Could not parse entry in git index file {release_path:?}: {err}"
                            );
                            None
                        }
                    })
                    .collect();
                Ok(Some(releases))
            }
            false => {
                trace!("Could not find a matching file in the git index");
                Ok(None)
            }
        }
    }

    /// A specific release of a package
    pub fn get_release(
        &self,
        name: &str,
        version: &SemanticVersion,
    ) -> anyhow::Result<Option<GitRelease>> {
        Ok(self
            .get_releases(name)?
            .and_then(|releases| releases.into_iter().find(|r| r.version == *version)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_client::models::GitIndexDependency;
    use crate::models::project_manifest::MagnetManifestV1PackageKind;

    fn fixture_index() -> GitClient {
        GitClient::open_local(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/index"),
        )
        .unwrap()
    }

    fn index_with_config(config: &str) -> anyhow::Result<GitClient> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join(INDEX_CONFIG_FILE), config)?;
        GitClient::open_local(dir.path().to_path_buf())
    }

    #[test]
    fn test_config() {
        let config = fixture_index().config().unwrap();
        assert_eq!(config.index_format, INDEX_FORMAT);
        assert!(config.dl.contains("{platform}"));
    }

    #[test]
    fn test_unknown_index_format() {
        let err = index_with_config(r#"{"index-format": 2, "dl": ""}"#)
            .err()
            .unwrap();
        assert!(err.to_string().contains("index-format 2"));
    }

    #[test]
    fn test_missing_config() {
        let dir = tempfile::tempdir().unwrap();
        assert!(GitClient::open_local(dir.path().to_path_buf()).is_err());
    }

    #[test]
    fn test_get_releases() {
        let releases = fixture_index().get_releases("alpha").unwrap().unwrap();
        assert_eq!(releases.len(), 3);

        let release = &releases[1];
        assert_eq!(release.version, SemanticVersion::new(1, 1, 0));
        assert_eq!(release.kind, MagnetManifestV1PackageKind::Quark);
        assert!(!release.yanked);
        assert_eq!(
            release.dependencies.get("beta"),
            Some(&GitIndexDependency::Version("^1.0.0".parse().unwrap()))
        );
        assert_eq!(release.artifacts[0].key(), "any-any");

        assert!(releases[2].yanked);
    }

    #[test]
    fn test_get_releases_normalized_name() {
        let index = fixture_index();
        for name in ["ddw-common", "DDW_Common", "ddw_common"] {
            let releases = index.get_releases(name).unwrap().unwrap();
            assert_eq!(releases[0].name, "ddw-common");
        }
    }

    #[test]
    fn test_get_releases_unknown_package() {
        assert!(fixture_index().get_releases("unknown").unwrap().is_none());
    }

    #[test]
    fn test_lepton_artifacts() {
        let release = fixture_index()
            .get_release("gamma", &SemanticVersion::new(0, 3, 0))
            .unwrap()
            .unwrap();
        assert_eq!(release.kind, MagnetManifestV1PackageKind::Lepton);
        let keys: Vec<String> = release.artifacts.iter().map(|a| a.key()).collect();
        assert_eq!(
            keys,
            ["api3-linux_x86_64", "api3-macos_arm64", "source-any"]
        );
    }

    #[test]
    fn test_legacy_quark_dependency() {
        let release = fixture_index()
            .get_release("delta", &SemanticVersion::new(0, 2, 0))
            .unwrap()
            .unwrap();
        assert!(matches!(
            release.dependencies.get("ddwCommon"),
            Some(GitIndexDependency::LegacyQuark { .. })
        ));
        assert!(release.version_dependencies().is_err());
    }
}
