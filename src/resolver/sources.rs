use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::{Path, PathBuf},
};

use anyhow::{bail, ensure, Context};
use pubgrub::SemanticVersion;
use semver::VersionReq;
use tracing::warn;
use url::Url;

use crate::config::constants::MAGNET_PROJECT_FILE;
use crate::git_client::{
    models::GitDependency,
    source::{GitCache, GitReference},
};
use crate::models::{
    lock_file::LockSource,
    project_manifest::{
        MagnetManifestV1, MagnetManifestV1DependenciesValue, MagnetManifestV1PackageKind,
        NamedDependency,
    },
};

/// Who requires a dependency, for error messages
const PROJECT: &str = "the project";

/// A dependency which doesn't come from the index, but from a git repository or a local
/// folder. It has a single version, given by its `magnet.toml`, or `0.0.0` for a legacy quark
/// without one (DESIGN.md "Lock file").
#[derive(Debug, Clone)]
pub struct SourcePackage {
    pub version: SemanticVersion,
    pub kind: MagnetManifestV1PackageKind,
    pub source: LockSource,
    /// the dependencies of its `magnet.toml`, as requirements for the resolver
    pub dependencies: Vec<GitDependency>,
    /// the `sclang` range of its `magnet.toml`
    pub sclang: Option<VersionReq>,
    pub required_by: String,
}

/// Where a dependency is declared, which determines how its `path` is resolved
#[derive(Clone)]
enum Origin {
    /// the project, whose paths are relative to its root
    Project,
    /// a path dependency, whose paths are relative to its folder (given relative to the project)
    Folder(PathBuf),
    /// a git dependency, which can't point outside of its repository
    Git(Url),
}

enum Spec {
    Index,
    Git(Url, GitReference),
    Path(PathBuf),
}

fn spec(name: &str, dependency: &MagnetManifestV1DependenciesValue) -> anyhow::Result<Spec> {
    let MagnetManifestV1DependenciesValue::Object {
        git,
        path,
        legacy_quark,
        ..
    } = dependency
    else {
        return Ok(Spec::Index);
    };
    match (git, path) {
        (Some(_), Some(_)) => bail!("{name} can't be a git and a path dependency at once"),
        (Some(git), None) => {
            let (url, reference) = GitReference::from_manifest(git)?;
            Ok(Spec::Git(url, reference))
        }
        (None, Some(path)) => Ok(Spec::Path(PathBuf::from(path))),
        (None, None) if legacy_quark.unwrap_or(false) => bail!(
            "{name} is a legacy quark without a git URL, which can't be looked up in directory.txt yet. Add its repository, e.g. {name} = {{ legacy-quark = true, git = {{ url = \"https://github.com/supercollider-quarks/{name}\" }} }}"
        ),
        (None, None) => Ok(Spec::Index),
    }
}

/// Collects the git and path dependencies of `dependencies` (a manifest's) and, recursively,
/// those of their `magnet.toml`s. `locked` returns the locked source of a package if its
/// commit should be kept, so a git dependency whose URL and reference didn't change isn't
/// fetched again.
/// A package which is required from two sources, or from the index as well, is an error
/// (DESIGN.md "Backwards compatibility / Legacy Quarks").
pub fn collect_sources(
    root: &Path,
    dependencies: &HashMap<String, MagnetManifestV1DependenciesValue>,
    cache: &GitCache,
    locked: impl Fn(&str) -> Option<LockSource>,
) -> anyhow::Result<BTreeMap<String, SourcePackage>> {
    let mut pending: VecDeque<(String, MagnetManifestV1DependenciesValue, Origin, String)> =
        sorted(dependencies)
            .map(|(name, dependency)| (name, dependency, Origin::Project, PROJECT.to_string()))
            .collect();
    let mut packages: BTreeMap<String, SourcePackage> = BTreeMap::new();
    // the packages required from the index, with who requires them
    let mut from_index: BTreeMap<String, String> = BTreeMap::new();

    while let Some((spelled, dependency, origin, required_by)) = pending.pop_front() {
        let name = MagnetManifestV1::normalize_name(&spelled);
        let (source, folder) = match spec(&spelled, &dependency)? {
            Spec::Index => {
                from_index.entry(name).or_insert(required_by);
                continue;
            }
            Spec::Git(url, reference) => {
                let sha = match locked(&name) {
                    Some(LockSource::Git {
                        url: locked_url,
                        reference: locked_reference,
                        sha,
                    }) if locked_url == url && locked_reference == reference => sha,
                    _ => cache
                        .resolve(&url, &reference)
                        .with_context(|| format!("Could not check out {spelled}"))?,
                };
                (
                    LockSource::Git {
                        url,
                        reference,
                        sha,
                    },
                    None,
                )
            }
            Spec::Path(path) => {
                let path = match &origin {
                    Origin::Project => path,
                    Origin::Folder(folder) => folder.join(path),
                    Origin::Git(url) => bail!(
                        "{spelled} is a path dependency of {required_by} from {url}, which can't point outside of its repository"
                    ),
                };
                let folder = root.join(&path);
                ensure!(
                    folder.is_dir(),
                    "{spelled} is a path dependency of {required_by}, but {folder:?} is not a folder"
                );
                (LockSource::Path(path), Some(folder))
            }
        };

        if let Some(existing) = packages.get(&name) {
            ensure!(
                existing.source == source,
                "{name} is required from two sources: {} by {}, and {source} by {required_by}. Remove one of them, or use [overrides] in {MAGNET_PROJECT_FILE}",
                existing.source,
                existing.required_by
            );
            continue;
        }

        let manifest = match (&source, &folder) {
            (LockSource::Git { url, sha, .. }, _) => {
                cache.read_file(url, sha, MAGNET_PROJECT_FILE)?
            }
            (_, Some(folder)) => {
                let path = folder.join(MAGNET_PROJECT_FILE);
                match path.is_file() {
                    true => Some(
                        std::fs::read_to_string(&path)
                            .with_context(|| format!("Could not read {path:?}"))?,
                    ),
                    false => None,
                }
            }
            _ => None,
        };
        let manifest = manifest
            .map(|content| {
                MagnetManifestV1::from_string(&content).with_context(|| {
                    format!("Could not parse the {MAGNET_PROJECT_FILE} of {spelled} ({source})")
                })
            })
            .transpose()?;

        let package = match &manifest {
            Some(manifest) => {
                if MagnetManifestV1::normalize_name(&manifest.package.name) != name {
                    warn!(
                        "{spelled} ({source}) is the project {}",
                        manifest.package.name
                    );
                }
                let kind = manifest.package.kind.clone();
                match (&kind, &source) {
                    (MagnetManifestV1PackageKind::Quark, _) => {}
                    (MagnetManifestV1PackageKind::Lepton, LockSource::Path(_)) => {}
                    (MagnetManifestV1PackageKind::Lepton, _) => bail!(
                        "{spelled} ({source}) is a lepton, whose binaries need a build. Build it locally and use a path dependency instead, e.g. {spelled} = {{ path = \"../{name}\" }}"
                    ),
                    (MagnetManifestV1PackageKind::Gluon, _) => {
                        bail!("{spelled} ({source}) is a gluon, which can not be installed yet")
                    }
                }
                let nested_origin = match &source {
                    LockSource::Git { url, .. } => Origin::Git(url.clone()),
                    LockSource::Path(path) => Origin::Folder(path.clone()),
                    LockSource::Index => Origin::Project,
                };
                for (nested, dependency) in sorted(&manifest.dependencies) {
                    pending.push_back((nested, dependency, nested_origin.clone(), name.clone()));
                }
                SourcePackage {
                    version: SemanticVersion::try_from(manifest.package.version.clone())?,
                    kind,
                    source,
                    dependencies: sorted(&manifest.dependencies)
                        .map(|(name, dependency)| {
                            GitDependency::try_from(NamedDependency { name, dependency })
                        })
                        .collect::<anyhow::Result<_>>()?,
                    sclang: manifest
                        .package
                        .sclang
                        .as_ref()
                        .map(|sclang| sclang.parse())
                        .transpose()
                        .with_context(|| format!("{spelled} has an invalid sclang range"))?,
                    required_by,
                }
            }
            // a legacy quark, which is used as a whole
            None => SourcePackage {
                version: SemanticVersion::zero(),
                kind: MagnetManifestV1PackageKind::Quark,
                source,
                dependencies: vec![],
                sclang: None,
                required_by,
            },
        };
        packages.insert(name, package);
    }

    for (name, required_by) in &from_index {
        if let Some(package) = packages.get(name) {
            bail!(
                "{name} is required from two sources: the index by {required_by}, and {} by {}. Remove one of them, or use [overrides] in {MAGNET_PROJECT_FILE}",
                package.source,
                package.required_by
            );
        }
    }
    Ok(packages)
}

/// The dependencies sorted by name, so errors and fetches don't depend on the hash order
fn sorted(
    dependencies: &HashMap<String, MagnetManifestV1DependenciesValue>,
) -> impl Iterator<Item = (String, MagnetManifestV1DependenciesValue)> {
    let sorted: BTreeMap<&String, &MagnetManifestV1DependenciesValue> =
        dependencies.iter().collect();
    sorted
        .into_iter()
        .map(|(name, dependency)| (name.clone(), dependency.clone()))
        .collect::<Vec<_>>()
        .into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_client::source::tests::{commit_files, tag};

    fn manifest(name: &str, version: &str, kind: &str, dependencies: &str) -> String {
        format!(
            "magnet-version = 1\n\n[package]\nname = \"{name}\"\nversion = \"{version}\"\nkind = \"{kind}\"\n\n[dependencies]\n{dependencies}"
        )
    }

    fn dependencies(toml: &str) -> HashMap<String, MagnetManifestV1DependenciesValue> {
        MagnetManifestV1::from_string(&manifest("root", "0.1.0", "quark", toml))
            .unwrap()
            .dependencies
    }

    fn url(path: &Path) -> String {
        Url::from_directory_path(path).unwrap().to_string()
    }

    struct Fixture {
        dir: tempfile::TempDir,
        cache: GitCache,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cache = GitCache::new(dir.path().join("cache"));
            std::fs::create_dir(dir.path().join("project")).unwrap();
            Self { dir, cache }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn collect(&self, toml: &str) -> anyhow::Result<BTreeMap<String, SourcePackage>> {
            collect_sources(
                &self.path("project"),
                &dependencies(toml),
                &self.cache,
                |_| None,
            )
        }
    }

    #[test]
    fn test_git_dependency_with_manifest() {
        let fixture = Fixture::new();
        let repo = fixture.path("foo");
        let sha = commit_files(
            &repo,
            &[(
                "magnet.toml",
                &manifest("foo", "1.2.0", "quark", "alpha = \"^1\"\n"),
            )],
        );
        tag(&repo, "v1.2.0");
        let sources = fixture
            .collect(&format!(
                "Foo = {{ git = {{ url = \"{}\", tag = \"v1.2.0\" }} }}\nbeta = \"^1\"\n",
                url(&repo)
            ))
            .unwrap();
        assert_eq!(sources.len(), 1);
        let foo = &sources["foo"];
        assert_eq!(foo.version, SemanticVersion::new(1, 2, 0));
        assert_eq!(
            foo.source.to_string(),
            format!("git+{}?tag=v1.2.0#{sha}", url(&repo))
        );
        assert_eq!(foo.dependencies.len(), 1);
        assert_eq!(foo.dependencies[0].name, "alpha");
        assert_eq!(foo.required_by, PROJECT);
    }

    #[test]
    fn test_legacy_quark_from_git() {
        let fixture = Fixture::new();
        let repo = fixture.path("bar");
        commit_files(&repo, &[("Bar.sc", "Bar {}"), ("Bar.quark", "()")]);
        let sources = fixture
            .collect(&format!(
                "bar = {{ legacy-quark = true, git = {{ url = \"{}\" }} }}\n",
                url(&repo)
            ))
            .unwrap();
        assert_eq!(sources["bar"].version, SemanticVersion::zero());
        assert_eq!(sources["bar"].kind, MagnetManifestV1PackageKind::Quark);
        assert!(sources["bar"].dependencies.is_empty());
    }

    #[test]
    fn test_locked_commit_is_kept() {
        let fixture = Fixture::new();
        let repo = fixture.path("foo");
        let first = commit_files(&repo, &[("Foo.sc", "Foo {}")]);
        commit_files(&repo, &[("Foo.sc", "Foo { bar {} }")]);
        let toml = format!("foo = {{ git = {{ url = \"{}\" }} }}\n", url(&repo));
        let locked = |sha: &str, reference: GitReference| {
            let source = LockSource::Git {
                url: Url::from_directory_path(&repo).unwrap(),
                reference,
                sha: sha.to_string(),
            };
            move |_: &str| Some(source.clone())
        };
        let collect = |locked: &dyn Fn(&str) -> Option<LockSource>| {
            collect_sources(
                &fixture.path("project"),
                &dependencies(&toml),
                &fixture.cache,
                locked,
            )
            .unwrap()
        };
        let kept = collect(&locked(&first, GitReference::DefaultBranch));
        assert!(kept["foo"].source.to_string().ends_with(&first));
        // another reference in the lock file means the manifest changed
        let moved = collect(&locked(&first, GitReference::Branch("main".to_string())));
        assert!(!moved["foo"].source.to_string().ends_with(&first));
    }

    #[test]
    fn test_nested_dependencies() {
        let fixture = Fixture::new();
        let inner = fixture.path("inner");
        commit_files(&inner, &[("Inner.sc", "Inner {}")]);
        let outer = fixture.path("outer");
        commit_files(
            &outer,
            &[(
                "magnet.toml",
                &manifest(
                    "outer",
                    "0.1.0",
                    "quark",
                    &format!("inner = {{ git = {{ url = \"{}\" }} }}\n", url(&inner)),
                ),
            )],
        );
        // a path dependency, whose own path dependency is relative to its folder
        let local = fixture.path("libs/local");
        std::fs::create_dir_all(fixture.path("libs/other")).unwrap();
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(
            local.join("magnet.toml"),
            manifest(
                "local",
                "0.3.0",
                "quark",
                "other = { path = \"../other\" }\n",
            ),
        )
        .unwrap();

        let sources = fixture
            .collect(&format!(
                "outer = {{ git = {{ url = \"{}\" }} }}\nlocal = {{ path = \"../libs/local\" }}\n",
                url(&outer)
            ))
            .unwrap();
        let names: Vec<&String> = sources.keys().collect();
        assert_eq!(names, ["inner", "local", "other", "outer"]);
        assert_eq!(sources["inner"].required_by, "outer");
        assert_eq!(sources["local"].version, SemanticVersion::new(0, 3, 0));
        assert_eq!(
            sources["other"].source,
            LockSource::Path(PathBuf::from("../libs/local/../other"))
        );
    }

    #[test]
    fn test_two_sources_are_an_error() {
        let fixture = Fixture::new();
        let foo = fixture.path("foo");
        commit_files(
            &foo,
            &[(
                "magnet.toml",
                &manifest("foo", "0.1.0", "quark", "alpha = \"^1\"\n"),
            )],
        );
        let alpha = fixture.path("alpha");
        commit_files(&alpha, &[("Alpha.sc", "Alpha {}")]);
        let err = fixture
            .collect(&format!(
                "foo = {{ git = {{ url = \"{}\" }} }}\nalpha = {{ git = {{ url = \"{}\" }} }}\n",
                url(&foo),
                url(&alpha)
            ))
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("alpha is required from two sources: the index by foo"));
    }

    #[test]
    fn test_git_lepton_is_refused() {
        let fixture = Fixture::new();
        let repo = fixture.path("zeta");
        commit_files(
            &repo,
            &[("magnet.toml", &manifest("zeta", "0.1.0", "lepton", ""))],
        );
        let err = fixture
            .collect(&format!(
                "zeta = {{ git = {{ url = \"{}\" }} }}\n",
                url(&repo)
            ))
            .unwrap_err();
        assert!(err.to_string().contains("path dependency instead"));
    }

    #[test]
    fn test_path_inside_git_is_refused() {
        let fixture = Fixture::new();
        let repo = fixture.path("foo");
        commit_files(
            &repo,
            &[(
                "magnet.toml",
                &manifest("foo", "0.1.0", "quark", "bar = { path = \"../bar\" }\n"),
            )],
        );
        let err = fixture
            .collect(&format!(
                "foo = {{ git = {{ url = \"{}\" }} }}\n",
                url(&repo)
            ))
            .unwrap_err();
        assert!(err.to_string().contains("can't point outside"));
    }

    #[test]
    fn test_legacy_quark_without_git() {
        let fixture = Fixture::new();
        let err = fixture
            .collect("ddwCommon = { legacy-quark = true }\n")
            .unwrap_err();
        assert!(err.to_string().contains("directory.txt"));
    }
}
