use anyhow::{anyhow, ensure, Context};
use git2::Repository;
use pubgrub::SemanticVersion;
use sha2::{Digest, Sha256};
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
    /// replaces the `http` mirror announced by the index, e.g. for a local test server
    http_url: Option<Url>,
}

impl GitClient {
    /// Opens the index at `local_destination` and updates it, or clones it if it does not exist yet.
    /// If the update fails (e.g. offline), the cached copy is used.
    pub fn new(url: Url, local_destination: PathBuf) -> anyhow::Result<Self> {
        let client = match local_destination.exists() {
            true => {
                let repo = Repository::open(&local_destination)?;
                let client = Self {
                    url,
                    local_destination,
                    repo: Some(repo),
                    http_url: None,
                };
                if let Err(err) = client.update() {
                    warn!(
                        "Could not update the index {}, using the cached copy: {err:#}",
                        client.url
                    );
                }
                client
            }
            false => Self::clone(url, local_destination)?,
        };
        client.config()?;
        Ok(client)
    }

    fn clone(url: Url, local_destination: PathBuf) -> anyhow::Result<Self> {
        info!("Cloning index repo {url} to {local_destination:?}");
        let repo = Repository::clone(url.as_str(), &local_destination)
            .with_context(|| format!("Could not clone the index {url}"))?;
        Ok(Self {
            url,
            local_destination,
            repo: Some(repo),
            http_url: None,
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
            http_url: None,
        };
        client.config()?;
        Ok(client)
    }

    /// Directory name of the clone of an index, e.g. `github.com-1a2b3c4d5e6f7a8b`.
    /// Each URL gets its own clone, so switching the index never resolves against
    /// the clone of another one.
    pub fn cache_dir_name(url: &Url) -> String {
        let hash = format!("{:x}", Sha256::digest(url.as_str().as_bytes()));
        format!("{}-{}", url.host_str().unwrap_or("local"), &hash[..16])
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Replaces the `http` mirror of `config.json`, see [Self::http_url]
    pub fn with_http_url(mut self, http_url: Option<Url>) -> Self {
        self.http_url = http_url;
        self
    }

    /// The static HTTP mirror of this index, which gets written into the lock file
    pub fn http_url(&self) -> anyhow::Result<Option<Url>> {
        match &self.http_url {
            Some(url) => Ok(Some(url.clone())),
            None => Ok(self.config()?.http),
        }
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

    /// Fetches the index and resets it to the state of the remote
    pub fn update(&self) -> anyhow::Result<()> {
        match self.repo {
            Some(_) => {
                self.fetch_origin()?;
                self.hard_reset_to_remote()
            }
            None => Ok(()),
        }
    }

    fn repo(&self) -> anyhow::Result<&Repository> {
        self.repo
            .as_ref()
            .context("A local index is not a git repository")
    }

    fn fetch_origin(&self) -> anyhow::Result<()> {
        self.repo()?
            .find_remote("origin")?
            .fetch(&["+refs/heads/main:refs/remotes/origin/main"], None, None)
            .with_context(|| format!("Could not fetch the index {}", self.url))?;
        Ok(())
    }

    fn hard_reset_to_remote(&self) -> anyhow::Result<()> {
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

    fn fixture_path(file: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/index")
            .join(file)
    }

    /// copies the given fixture files into the repo and commits them on `main`
    fn commit_fixtures(repo: &Repository, files: &[&str]) {
        let workdir = repo.workdir().unwrap();
        for file in files {
            let destination = workdir.join(file);
            std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
            std::fs::copy(fixture_path(file), destination).unwrap();
        }
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("magnet", "magnet@localhost").unwrap();
        let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "update",
            &tree,
            &parents,
        )
        .unwrap();
    }

    #[test]
    fn test_update_and_offline() {
        let remote_dir = tempfile::tempdir().unwrap();
        let mut options = git2::RepositoryInitOptions::new();
        options.initial_head("main");
        let remote = Repository::init_opts(remote_dir.path(), &options).unwrap();
        commit_fixtures(&remote, &["config.json", "a/alpha.jsonl"]);

        let url = Url::from_directory_path(remote_dir.path()).unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let local_destination = cache_dir.path().join("index");

        // clone
        let index = GitClient::new(url.clone(), local_destination.clone()).unwrap();
        assert!(index.get_releases("alpha").unwrap().is_some());
        assert!(index.get_releases("beta").unwrap().is_none());

        // a new release on the remote gets picked up on the next start
        commit_fixtures(&remote, &["b/beta.jsonl"]);
        let index = GitClient::new(url.clone(), local_destination.clone()).unwrap();
        assert!(index.get_releases("beta").unwrap().is_some());

        // offline: the update fails, but the cached copy can still be used
        drop(remote);
        remote_dir.close().unwrap();
        assert!(index.update().is_err());
        let index = GitClient::new(url, local_destination).unwrap();
        assert!(index.get_releases("beta").unwrap().is_some());
    }

    #[test]
    fn test_clone_offline() {
        let cache_dir = tempfile::tempdir().unwrap();
        let url = Url::from_directory_path(cache_dir.path().join("missing")).unwrap();
        assert!(GitClient::new(url, cache_dir.path().join("index")).is_err());
    }

    #[test]
    fn test_cache_dir_name() {
        let official: Url = "https://github.com/supercollider/magnet-index.git"
            .parse()
            .unwrap();
        let testing: Url = "https://github.com/supercollider/magnet-index-testing.git"
            .parse()
            .unwrap();
        let name = GitClient::cache_dir_name(&official);
        assert!(name.starts_with("github.com-"), "{name}");
        assert_eq!(name.len(), "github.com-".len() + 16);
        assert_eq!(name, GitClient::cache_dir_name(&official));
        assert_ne!(name, GitClient::cache_dir_name(&testing));

        let local = Url::from_directory_path(std::env::temp_dir()).unwrap();
        assert!(GitClient::cache_dir_name(&local).starts_with("local-"));
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
            ["api3_0-linux_x86_64", "api3_0-macos_arm64", "source-any"]
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
