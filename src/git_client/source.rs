use std::{
    fmt::Display,
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, ensure, Context};
use git2::{FetchOptions, ObjectType, Oid, Repository};
use tracing::{info, warn};
use url::Url;

use crate::git_client::client::GitClient;
use crate::models::project_manifest::MagnetManifestV1DependenciesValueObjectGit;

/// What a git dependency checks out (DESIGN.md "Lock file"), written into the lock file's
/// source as `?branch=`, `?tag=` or `?ref=`, so a changed manifest is noticed without a fetch
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitReference {
    DefaultBranch,
    Branch(String),
    Tag(String),
    Ref(String),
}

impl GitReference {
    /// The URL and reference of a git dependency of a manifest
    pub fn from_manifest(
        git: &MagnetManifestV1DependenciesValueObjectGit,
    ) -> anyhow::Result<(Url, Self)> {
        let url: Url = git
            .url
            .parse()
            .with_context(|| format!("{} is not a valid git URL", git.url))?;
        let references = [
            git.branch.clone().map(Self::Branch),
            git.tag.clone().map(Self::Tag),
            git.ref_.clone().map(Self::Ref),
        ];
        let mut given = references.into_iter().flatten();
        let reference = given.next().unwrap_or(Self::DefaultBranch);
        ensure!(
            given.next().is_none(),
            "The git dependency {url} can only have one of branch, tag or ref"
        );
        Ok((url, reference))
    }

    /// The `(key, value)` of the query in the lock file's source
    pub fn query(&self) -> Option<(&str, &str)> {
        match self {
            Self::DefaultBranch => None,
            Self::Branch(branch) => Some(("branch", branch)),
            Self::Tag(tag) => Some(("tag", tag)),
            Self::Ref(rev) => Some(("ref", rev)),
        }
    }

    /// Splits the reference off an URL with a `?branch=`, `?tag=` or `?ref=` query
    pub fn from_url(mut url: Url) -> (Url, Self) {
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        let reference = match pairs.as_slice() {
            [(key, value)] => match key.as_str() {
                "branch" => Some(Self::Branch(value.clone())),
                "tag" => Some(Self::Tag(value.clone())),
                "ref" => Some(Self::Ref(value.clone())),
                _ => None,
            },
            _ => None,
        };
        match reference {
            Some(reference) => {
                url.set_query(None);
                (url, reference)
            }
            None => (url, Self::DefaultBranch),
        }
    }

    /// The URL with this reference as query
    pub fn to_url(&self, url: &Url) -> Url {
        let mut url = url.clone();
        if let Some((key, value)) = self.query() {
            url.query_pairs_mut().clear().append_pair(key, value);
        }
        url
    }
}

impl Display for GitReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.query() {
            Some((key, value)) => write!(f, "{key} {value}"),
            None => write!(f, "the default branch"),
        }
    }
}

/// The clones of git dependencies in `~/.magnet/git/<host>-<hash>`, one bare repository per URL
pub struct GitCache {
    dir: PathBuf,
}

impl GitCache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Opens the clone of `url`, or creates an empty one which needs a fetch
    fn repo(&self, url: &Url) -> anyhow::Result<Repository> {
        let path = self.dir.join(GitClient::cache_dir_name(url));
        if path.exists() {
            return Repository::open_bare(&path)
                .with_context(|| format!("Could not open the clone of {url} at {path:?}"));
        }
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("Could not create {:?}", self.dir))?;
        let repo = Repository::init_bare(&path)
            .with_context(|| format!("Could not create a clone of {url} at {path:?}"))?;
        repo.remote("origin", url.as_str())?;
        Ok(repo)
    }

    fn fetch(repo: &Repository, url: &Url) -> anyhow::Result<()> {
        info!("Fetching {url}");
        let mut options = FetchOptions::new();
        options.download_tags(git2::AutotagOption::All);
        repo.find_remote("origin")?
            .fetch(
                &[
                    "+HEAD:refs/remotes/origin/HEAD",
                    "+refs/heads/*:refs/remotes/origin/*",
                    "+refs/tags/*:refs/tags/*",
                ],
                Some(&mut options),
                None,
            )
            .with_context(|| format!("Could not fetch {url}"))?;
        Ok(())
    }

    /// The commit of `reference` in the fetched clone
    fn find(repo: &Repository, reference: &GitReference) -> anyhow::Result<Option<Oid>> {
        let object = match reference {
            GitReference::DefaultBranch => repo.revparse_single("refs/remotes/origin/HEAD"),
            GitReference::Branch(branch) => {
                repo.revparse_single(&format!("refs/remotes/origin/{branch}"))
            }
            GitReference::Tag(tag) => repo.revparse_single(&format!("refs/tags/{tag}")),
            GitReference::Ref(rev) => repo.revparse_single(rev),
        };
        match object {
            Ok(object) => Ok(Some(object.peel_to_commit()?.id())),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Fetches `url` and returns the SHA of the commit `reference` points to. A `ref` which
    /// is already in the clone is taken without fetching. If the fetch fails (e.g. offline),
    /// the clone is used as it is.
    pub fn resolve(&self, url: &Url, reference: &GitReference) -> anyhow::Result<String> {
        let repo = self.repo(url)?;
        if let GitReference::Ref(_) = reference {
            if let Some(oid) = Self::find(&repo, reference)? {
                return Ok(oid.to_string());
            }
        }
        if let Err(err) = Self::fetch(&repo, url) {
            warn!("{err:#}, using the cached clone");
        }
        match Self::find(&repo, reference)? {
            Some(oid) => Ok(oid.to_string()),
            None => bail!("{url} has no {reference}"),
        }
    }

    /// Makes sure the clone of `url` has the commit `sha`, fetching it if necessary
    pub fn ensure(&self, url: &Url, sha: &str) -> anyhow::Result<()> {
        let repo = self.repo(url)?;
        let oid = Oid::from_str(sha).with_context(|| format!("{sha} is not a git commit SHA"))?;
        if repo.find_commit(oid).is_ok() {
            return Ok(());
        }
        Self::fetch(&repo, url)?;
        ensure!(
            repo.find_commit(oid).is_ok(),
            "{url} has no commit {sha}, maybe it was force-pushed away. Run `magnet update` to resolve it again"
        );
        Ok(())
    }

    /// The content of the file at `path` of the commit `sha`, or `None` if it doesn't exist
    pub fn read_file(&self, url: &Url, sha: &str, path: &str) -> anyhow::Result<Option<String>> {
        self.ensure(url, sha)?;
        let repo = self.repo(url)?;
        let tree = repo.find_commit(Oid::from_str(sha)?)?.tree()?;
        let entry = match tree.get_path(Path::new(path)) {
            Ok(entry) => entry,
            Err(err) if err.code() == git2::ErrorCode::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let blob = entry.to_object(&repo)?.peel_to_blob()?;
        let content = std::str::from_utf8(blob.content())
            .with_context(|| format!("{path} of {url} at {sha} is not valid UTF-8"))?;
        Ok(Some(content.to_string()))
    }

    /// Writes the files of the commit `sha` into `destination`, without the `.git` folder.
    /// Links and submodules are skipped with a warning.
    pub fn export(&self, url: &Url, sha: &str, destination: &Path) -> anyhow::Result<()> {
        self.ensure(url, sha)?;
        let repo = self.repo(url)?;
        let tree = repo.find_commit(Oid::from_str(sha)?)?.tree()?;
        export_tree(&repo, &tree, destination, Path::new(""))
    }
}

/// Mode of a link within a git tree
const LINK_MODE: i32 = 0o120000;
/// Mode of an executable file within a git tree
const EXECUTABLE_MODE: i32 = 0o100755;

fn export_tree(
    repo: &Repository,
    tree: &git2::Tree,
    destination: &Path,
    relative: &Path,
) -> anyhow::Result<()> {
    for entry in tree.iter() {
        let name = entry
            .name()
            .with_context(|| format!("A file name in {relative:?} is not valid UTF-8"))?;
        // git never stores these, but a crafted tree could, which must not escape `destination`
        ensure!(
            matches!(
                Path::new(name).components().collect::<Vec<_>>().as_slice(),
                [Component::Normal(_)]
            ),
            "The git tree contains the invalid name {name:?}"
        );
        let path = relative.join(name);
        match entry.kind() {
            Some(ObjectType::Tree) => {
                fs::create_dir_all(destination.join(&path))?;
                export_tree(
                    repo,
                    &entry.to_object(repo)?.peel_to_tree()?,
                    destination,
                    &path,
                )?;
            }
            Some(ObjectType::Blob) if entry.filemode() == LINK_MODE => {
                warn!("Skipping the link {path:?}, links are not installed")
            }
            Some(ObjectType::Blob) => {
                let file = destination.join(&path);
                if let Some(parent) = file.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&file, entry.to_object(repo)?.peel_to_blob()?.content())
                    .with_context(|| format!("Could not write {file:?}"))?;
                #[cfg(unix)]
                if entry.filemode() == EXECUTABLE_MODE {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&file, fs::Permissions::from_mode(0o755))?;
                }
            }
            Some(ObjectType::Commit) => {
                warn!("Skipping the submodule {path:?}, submodules are not installed")
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Commits `files` to the branch `main` of a (new) repository at `path`, returns the SHA
    pub(crate) fn commit_files(path: &Path, files: &[(&str, &str)]) -> String {
        let repo = match Repository::open(path) {
            Ok(repo) => repo,
            Err(_) => {
                let mut options = git2::RepositoryInitOptions::new();
                options.initial_head("main");
                Repository::init_opts(path, &options).unwrap()
            }
        };
        for (file, content) in files {
            let file_path = path.join(file);
            fs::create_dir_all(file_path.parent().unwrap()).unwrap();
            fs::write(file_path, content).unwrap();
        }
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("t", "t@t").unwrap();
        let parent = repo.head().ok().map(|head| head.peel_to_commit().unwrap());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "commit",
            &tree,
            &parents,
        )
        .unwrap()
        .to_string()
    }

    pub(crate) fn tag(path: &Path, name: &str) {
        let repo = Repository::open(path).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.tag_lightweight(name, head.as_object(), false).unwrap();
    }

    fn file_url(path: &Path) -> Url {
        Url::from_directory_path(path).unwrap()
    }

    #[test]
    fn test_reference_from_manifest() {
        let git =
            |branch: Option<&str>, tag: Option<&str>| MagnetManifestV1DependenciesValueObjectGit {
                url: "https://x.org/foo.git".to_string(),
                branch: branch.map(String::from),
                tag: tag.map(String::from),
                ref_: None,
            };
        let (url, reference) = GitReference::from_manifest(&git(None, Some("v1"))).unwrap();
        assert_eq!(url.as_str(), "https://x.org/foo.git");
        assert_eq!(reference, GitReference::Tag("v1".to_string()));
        assert_eq!(
            GitReference::from_manifest(&git(None, None)).unwrap().1,
            GitReference::DefaultBranch
        );
        assert!(GitReference::from_manifest(&git(Some("main"), Some("v1"))).is_err());
    }

    #[test]
    fn test_reference_url_round_trip() {
        let url: Url = "https://x.org/foo.git".parse().unwrap();
        for reference in [
            GitReference::DefaultBranch,
            GitReference::Branch("feature/x".to_string()),
            GitReference::Tag("v1.0".to_string()),
            GitReference::Ref("4b825dc".to_string()),
        ] {
            let with_query = reference.to_url(&url);
            assert_eq!(GitReference::from_url(with_query), (url.clone(), reference));
        }
        assert_eq!(
            GitReference::Tag("v1".to_string()).to_url(&url).as_str(),
            "https://x.org/foo.git?tag=v1"
        );
    }

    #[test]
    fn test_resolve_and_export() {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote");
        let first = commit_files(&remote, &[("Classes/Foo.sc", "Foo {}")]);
        tag(&remote, "v1");
        let second = commit_files(&remote, &[("Classes/Bar.sc", "Bar {}")]);
        let url = file_url(&remote);
        let cache = GitCache::new(dir.path().join("cache"));

        assert_eq!(
            cache.resolve(&url, &GitReference::DefaultBranch).unwrap(),
            second
        );
        assert_eq!(
            cache
                .resolve(&url, &GitReference::Branch("main".to_string()))
                .unwrap(),
            second
        );
        assert_eq!(
            cache
                .resolve(&url, &GitReference::Tag("v1".to_string()))
                .unwrap(),
            first
        );
        assert_eq!(
            cache
                .resolve(&url, &GitReference::Ref(first[..7].to_string()))
                .unwrap(),
            first
        );
        let missing = cache
            .resolve(&url, &GitReference::Tag("v2".to_string()))
            .unwrap_err();
        assert!(format!("{missing:#}").contains("has no tag v2"));

        let export = dir.path().join("export");
        cache.export(&url, &first, &export).unwrap();
        assert!(export.join("Classes/Foo.sc").is_file());
        assert!(!export.join("Classes/Bar.sc").exists());
        assert!(!export.join(".git").exists());
        assert_eq!(
            cache.read_file(&url, &second, "Classes/Bar.sc").unwrap(),
            Some("Bar {}".to_string())
        );
        assert_eq!(cache.read_file(&url, &first, "magnet.toml").unwrap(), None);
    }

    #[test]
    fn test_ensure_fetches_new_commits() {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote");
        commit_files(&remote, &[("a.sc", "A {}")]);
        let url = file_url(&remote);
        let cache = GitCache::new(dir.path().join("cache"));
        cache.resolve(&url, &GitReference::DefaultBranch).unwrap();

        let newer = commit_files(&remote, &[("b.sc", "B {}")]);
        cache.ensure(&url, &newer).unwrap();
        let unknown = cache
            .ensure(&url, "0123456789012345678901234567890123456789")
            .unwrap_err();
        assert!(unknown.to_string().contains("magnet update"));
    }
}
