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
use crate::legacy::{
    directory::{parse_refspec, QuarksDirectory},
    quark_parser::{parse_dependencies, QuarkDependency},
};
use crate::models::{
    lock_file::LockSource,
    package::ArtifactKind,
    project_manifest::{MagnetManifestV1, MagnetManifestV1DependenciesValue, NamedDependency},
};

/// Who requires a dependency, for error messages
const PROJECT: &str = "the project";

/// A dependency which doesn't come from the index, but from a git repository or a local
/// folder. It has a single version, given by its `magnet.toml`, or `0.0.0` for a legacy quark
/// without one (DESIGN.md "Lock file").
#[derive(Debug, Clone)]
pub struct SourcePackage {
    pub version: SemanticVersion,
    /// the kinds of its `magnet.toml`, a quark for a legacy quark
    pub kinds: Vec<ArtifactKind>,
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
    /// a legacy quark without URL, which is looked up in the quarks directory, optionally
    /// with another reference than the directory's (from a `.quark` file's `name@refspec`)
    Legacy(Option<GitReference>),
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
        (Some(git), None) => match GitReference::from_manifest(git)? {
            (Some(url), reference) => Ok(Spec::Git(
                url,
                reference.unwrap_or(GitReference::DefaultBranch),
            )),
            // a legacy quark pinned by name, like `name@refspec` in a .quark file
            (None, reference) if legacy_quark.unwrap_or(false) => Ok(Spec::Legacy(reference)),
            (None, _) => bail!(
                "{name} is a git dependency without url, only a legacy quark (legacy-quark = true) can be pinned by name"
            ),
        },
        (None, Some(path)) => Ok(Spec::Path(PathBuf::from(path))),
        (None, None) if legacy_quark.unwrap_or(false) => Ok(Spec::Legacy(None)),
        (None, None) => Ok(Spec::Index),
    }
}

/// A git dependency of a package of the index, which is pinned by its commit
#[derive(Debug, Clone, PartialEq)]
pub struct IndexGitDependency {
    pub name: String,
    pub url: Url,
    pub sha: String,
    /// `ignore-quark-deps` of the release's manifest
    pub ignore_quark_deps: bool,
    /// the release which requires it, e.g. `delta 0.2.0`
    pub required_by: String,
}

/// A dependency which still needs to be collected
struct Pending {
    spelled: String,
    spec: Spec,
    origin: Origin,
    required_by: String,
    /// named by a `.quark` file, where the first source of a name wins (like in
    /// `Quark.prCollectDependencies`) instead of being an error
    from_quark: bool,
    /// `ignore-quark-deps = true` of a direct dependency of the project
    ignore_quark_deps: bool,
}

impl Pending {
    fn from_manifest(
        spelled: String,
        dependency: &MagnetManifestV1DependenciesValue,
        origin: Origin,
        required_by: String,
    ) -> anyhow::Result<Self> {
        let ignore_quark_deps = matches!(origin, Origin::Project)
            && matches!(
                dependency,
                MagnetManifestV1DependenciesValue::Object {
                    ignore_quark_deps: Some(true),
                    ..
                }
            );
        Ok(Self {
            spec: spec(&spelled, dependency)?,
            spelled,
            origin,
            required_by,
            from_quark: false,
            ignore_quark_deps,
        })
    }
}

/// Collects the git and path dependencies of `dependencies` (a manifest's) and, recursively,
/// those of their `magnet.toml`s, or of the `.quark` file of a legacy quark (DESIGN.md
/// "Backwards compatibility / Legacy Quarks"): a dependency named there is a `*` requirement
/// against the index if `in_index` knows it, otherwise a legacy quark.
/// `locked` returns the locked source of a package if its commit should be kept, so a git
/// dependency whose URL and reference didn't change isn't fetched again. A legacy quark
/// without URL is looked up in `directory` (its repository and refspec), unless it's locked.
/// `from_index` are the git dependencies of the resolved index packages, which get collected
/// after the project's own (see `Project::resolve`).
/// A package which is required from two sources, or from the index as well, is an error,
/// unless the second one comes from a `.quark` file.
pub fn collect_sources(
    root: &Path,
    dependencies: &HashMap<String, MagnetManifestV1DependenciesValue>,
    from_index_releases: &[IndexGitDependency],
    cache: &GitCache,
    directory: &QuarksDirectory,
    in_index: impl Fn(&str) -> anyhow::Result<bool>,
    locked: impl Fn(&str) -> Option<LockSource>,
) -> anyhow::Result<BTreeMap<String, SourcePackage>> {
    let mut pending: VecDeque<Pending> = sorted(dependencies)
        .map(|(name, dependency)| {
            Pending::from_manifest(name, &dependency, Origin::Project, PROJECT.to_string())
        })
        .collect::<anyhow::Result<_>>()?;
    // the .quark file of a commit is only ignored if every index release requiring it says
    // so; a direct dependency of the project is collected first, so its flag wins anyway
    let mut ignored: HashMap<(String, &Url, &str), bool> = HashMap::new();
    for dependency in from_index_releases {
        let key = (
            MagnetManifestV1::normalize_name(&dependency.name),
            &dependency.url,
            dependency.sha.as_str(),
        );
        *ignored.entry(key).or_insert(true) &= dependency.ignore_quark_deps;
    }
    pending.extend(from_index_releases.iter().map(|dependency| Pending {
        spelled: dependency.name.clone(),
        spec: Spec::Git(
            dependency.url.clone(),
            GitReference::Ref(dependency.sha.clone()),
        ),
        origin: Origin::Project,
        required_by: dependency.required_by.clone(),
        from_quark: false,
        ignore_quark_deps: ignored[&(
            MagnetManifestV1::normalize_name(&dependency.name),
            &dependency.url,
            dependency.sha.as_str(),
        )],
    }));
    let mut packages: BTreeMap<String, SourcePackage> = BTreeMap::new();
    // the packages required from the index, with who requires them
    let mut from_index: BTreeMap<String, String> = BTreeMap::new();

    while let Some(item) = pending.pop_front() {
        let Pending {
            spelled,
            spec,
            origin,
            required_by,
            from_quark,
            ignore_quark_deps,
        } = item;
        let name = MagnetManifestV1::normalize_name(&spelled);
        let (source, folder) = match spec {
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
            Spec::Legacy(reference) => match locked(&name) {
                // a pinned one keeps its lock only while the pin stays the same
                Some(source @ LockSource::Git { .. })
                    if reference.as_ref().is_none_or(|pin| {
                        matches!(&source, LockSource::Git { reference: locked, .. } if locked == pin)
                    }) =>
                {
                    (source, None)
                }
                _ if from_quark && packages.contains_key(&name) => {
                    // already collected, so there is nothing to look up
                    continue;
                }
                _ => {
                    let entry = directory.lookup(&spelled)?.with_context(|| {
                        format!(
                            "{spelled} (required by {required_by}) is not in the quarks directory {}. Add its repository, e.g. {spelled} = {{ legacy-quark = true, git = {{ url = \"https://github.com/<user>/{spelled}\" }} }}",
                            directory.url()
                        )
                    })?;
                    let reference = reference.unwrap_or(entry.reference);
                    let sha = cache
                        .resolve(&entry.url, &reference)
                        .with_context(|| format!("Could not check out {spelled}"))?;
                    (
                        LockSource::Git {
                            url: entry.url,
                            reference,
                            sha,
                        },
                        None,
                    )
                }
            },
        };

        if let Some(existing) = packages.get(&name) {
            if !existing.source.same_as(&source) && from_quark {
                warn!(
                    "{required_by} requires {spelled} from {source}, but {} already requires it from {}, which is kept",
                    existing.required_by, existing.source
                );
                continue;
            }
            ensure!(
                existing.source.same_as(&source),
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
            (_, Some(folder)) => read_optional(&folder.join(MAGNET_PROJECT_FILE))?,
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
                let kinds = manifest.artifact_kinds();
                if kinds.contains(&ArtifactKind::Lepton) && !matches!(source, LockSource::Path(_)) {
                    bail!(
                        "{spelled} ({source}) contains a lepton, whose binaries need a build. Build it locally and use a path dependency instead, e.g. {spelled} = {{ path = \"../{name}\" }}"
                    );
                }
                let nested_origin = match &source {
                    LockSource::Git { url, .. } => Origin::Git(url.clone()),
                    LockSource::Path(path) => Origin::Folder(path.clone()),
                    LockSource::Index => Origin::Project,
                };
                for (nested, dependency) in sorted(&manifest.dependencies) {
                    pending.push_back(Pending::from_manifest(
                        nested,
                        &dependency,
                        nested_origin.clone(),
                        name.clone(),
                    )?);
                }
                SourcePackage {
                    version: SemanticVersion::try_from(manifest.package.version.clone())?,
                    kinds,
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
            None => {
                let quark_dependencies = match ignore_quark_deps {
                    true => vec![],
                    false => quark_dependencies(&spelled, &source, folder.as_deref(), cache)?,
                };
                let mut dependencies = vec![];
                for dependency in quark_dependencies {
                    let nested = MagnetManifestV1::normalize_name(&dependency.name);
                    // the version of a .quark dependency is ignored, like SuperCollider does
                    dependencies.push(GitDependency {
                        name: nested.clone(),
                        constraint: VersionReq::STAR,
                    });
                    if from_index.contains_key(&nested) || in_index(&nested)? {
                        from_index.entry(nested).or_insert(name.clone());
                        continue;
                    }
                    let reference = dependency.refspec.as_deref().map(parse_refspec);
                    let spec = match dependency.url {
                        Some(url) => {
                            Spec::Git(url, reference.unwrap_or(GitReference::DefaultBranch))
                        }
                        None => Spec::Legacy(reference),
                    };
                    pending.push_back(Pending {
                        spelled: dependency.name,
                        spec,
                        origin: Origin::Project,
                        required_by: name.clone(),
                        from_quark: true,
                        ignore_quark_deps: false,
                    });
                }
                SourcePackage {
                    version: SemanticVersion::zero(),
                    kinds: vec![ArtifactKind::Quark],
                    source,
                    dependencies,
                    sclang: None,
                    required_by,
                }
            }
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

fn read_optional(path: &Path) -> anyhow::Result<Option<String>> {
    match path.is_file() {
        true => Ok(Some(
            std::fs::read_to_string(path).with_context(|| format!("Could not read {path:?}"))?,
        )),
        false => Ok(None),
    }
}

/// The `.quark` file among the files at the root of a legacy quark: `<name>.quark` (in any
/// spelling), otherwise the only one
fn find_quark_file(name: &str, files: &[String]) -> Option<String> {
    let quark_files: Vec<&String> = files
        .iter()
        .filter(|file| file.ends_with(".quark"))
        .collect();
    let normalized = MagnetManifestV1::normalize_name(&format!("{name}.quark"));
    quark_files
        .iter()
        .find(|file| MagnetManifestV1::normalize_name(file) == normalized)
        .or(match quark_files.as_slice() {
            [only] => Some(only),
            _ => None,
        })
        .map(|file| file.to_string())
}

/// The dependencies of a legacy quark's `.quark` file, which is read from its commit or
/// folder. A file which isn't a plain literal gives a warning and no dependencies (D4.1).
fn quark_dependencies(
    spelled: &str,
    source: &LockSource,
    folder: Option<&Path>,
    cache: &GitCache,
) -> anyhow::Result<Vec<QuarkDependency>> {
    let (file, content) = match (source, folder) {
        (LockSource::Git { url, sha, .. }, _) => {
            let Some(file) = find_quark_file(spelled, &cache.root_files(url, sha)?) else {
                return Ok(vec![]);
            };
            let content = cache.read_file(url, sha, &file)?;
            (file, content)
        }
        (_, Some(folder)) => {
            let files: Vec<String> = std::fs::read_dir(folder)?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect();
            let Some(file) = find_quark_file(spelled, &files) else {
                return Ok(vec![]);
            };
            let content = read_optional(&folder.join(&file))?;
            (file, content)
        }
        _ => return Ok(vec![]),
    };
    match parse_dependencies(&content.unwrap_or_default()) {
        Ok(dependencies) => Ok(dependencies),
        Err(reason) => {
            warn!(
                "Could not read the dependencies of {spelled} from {file} ({reason}), so it gets installed without them. Set `ignore-quark-deps = true` on {spelled} in {MAGNET_PROJECT_FILE} and add its dependencies there to fix this"
            );
            Ok(vec![])
        }
    }
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

    /// a manifest whose `[artifact]` tables make it a package of `kind`
    fn manifest(name: &str, version: &str, kind: &str, dependencies: &str) -> String {
        let artifact = match kind {
            "lepton" => "[artifact.lepton]\nplatform-paths = {}\n\n",
            _ => "",
        };
        format!(
            "magnet-version = 1\n\n[package]\nname = \"{name}\"\nversion = \"{version}\"\n\n{artifact}[dependencies]\n{dependencies}"
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

        /// the quarks directory, a repository at `quarks` (created by the tests which use it)
        fn directory(&self) -> QuarksDirectory<'_> {
            QuarksDirectory::new(
                Url::from_directory_path(self.path("quarks")).unwrap(),
                &self.cache,
            )
        }

        fn collect(&self, toml: &str) -> anyhow::Result<BTreeMap<String, SourcePackage>> {
            self.collect_locked(toml, &|_| None)
        }

        fn collect_locked(
            &self,
            toml: &str,
            locked: &dyn Fn(&str) -> Option<LockSource>,
        ) -> anyhow::Result<BTreeMap<String, SourcePackage>> {
            self.collect_all(toml, &[], locked)
        }

        fn collect_all(
            &self,
            toml: &str,
            from_index_releases: &[IndexGitDependency],
            locked: &dyn Fn(&str) -> Option<LockSource>,
        ) -> anyhow::Result<BTreeMap<String, SourcePackage>> {
            collect_sources(
                &self.path("project"),
                &dependencies(toml),
                from_index_releases,
                &self.cache,
                &self.directory(),
                // the fixture index has alpha, and other names which these tests don't use
                |name| Ok(name == "alpha"),
                locked,
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
        assert_eq!(sources["bar"].kinds, [ArtifactKind::Quark]);
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
            fixture.collect_locked(&toml, locked).unwrap()
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
    fn test_legacy_quark_from_directory() {
        let fixture = Fixture::new();
        let ddw = fixture.path("ddwCommon");
        let first = commit_files(&ddw, &[("DDW.sc", "DDW {}")]);
        tag(&ddw, "v1");
        commit_files(&ddw, &[("DDW.sc", "DDW { new {} }")]);
        commit_files(
            &fixture.path("quarks"),
            &[(
                "directory.txt",
                &format!("ddwCommon={}@tags/v1\n", url(&ddw).trim_end_matches('/')),
            )],
        );
        let sources = fixture
            .collect("DDWCommon = { legacy-quark = true }\n")
            .unwrap();
        let ddw_common = &sources["ddwcommon"];
        assert_eq!(ddw_common.version, SemanticVersion::zero());
        assert_eq!(
            ddw_common.source.to_string(),
            format!("git+{}?tag=v1#{first}", url(&ddw).trim_end_matches('/'))
        );

        let err = fixture
            .collect("unknown = { legacy-quark = true }\n")
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("unknown (required by the project) is not in the quarks directory"));
    }

    /// `cruciallib = { legacy-quark = true, git = { tag = "…" } }`, like `cruciallib@tags/…`
    /// in a .quark file: the URL comes from the directory, the pin from the manifest
    #[test]
    fn test_legacy_quark_pinned_by_name() {
        let fixture = Fixture::new();
        let crucial = fixture.path("crucial");
        let first = commit_files(&crucial, &[("Crucial.sc", "Crucial {}")]);
        tag(&crucial, "4.1.4");
        let second = commit_files(&crucial, &[("Crucial.sc", "Crucial { new {} }")]);
        tag(&crucial, "4.1.5");
        let crucial_url = url(&crucial).trim_end_matches('/').to_string();
        // the directory names no refspec, so it would be the default branch
        commit_files(
            &fixture.path("quarks"),
            &[("directory.txt", &format!("cruciallib={crucial_url}\n"))],
        );
        let pinned = |tag: &str| {
            format!("cruciallib = {{ legacy-quark = true, git = {{ tag = \"{tag}\" }} }}\n")
        };

        let sources = fixture.collect(&pinned("4.1.4")).unwrap();
        let source = sources["cruciallib"].source.clone();
        assert_eq!(
            source.to_string(),
            format!("git+{crucial_url}?tag=4.1.4#{first}")
        );

        // the lock is kept while the pin stays the same, a changed pin resolves again
        let locked = |_: &str| Some(source.clone());
        let kept = fixture.collect_locked(&pinned("4.1.4"), &locked).unwrap();
        assert_eq!(kept["cruciallib"].source, source);
        let moved = fixture.collect_locked(&pinned("4.1.5"), &locked).unwrap();
        assert_eq!(
            moved["cruciallib"].source.to_string(),
            format!("git+{crucial_url}?tag=4.1.5#{second}")
        );
        // without a pin, any locked commit is kept, like before
        let unpinned = fixture
            .collect_locked("cruciallib = { legacy-quark = true }\n", &locked)
            .unwrap();
        assert_eq!(unpinned["cruciallib"].source, source);

        // only a legacy quark can leave out the URL
        let err = fixture
            .collect("cruciallib = { git = { tag = \"4.1.4\" } }\n")
            .unwrap_err();
        assert!(
            err.to_string().contains("git dependency without url"),
            "{err:#}"
        );
    }

    #[test]
    fn test_locked_legacy_quark_skips_directory() {
        // there is no quarks directory, so a lookup would fail
        let fixture = Fixture::new();
        let ddw = fixture.path("ddw");
        let sha = commit_files(&ddw, &[("DDW.sc", "DDW {}")]);
        let source = LockSource::Git {
            url: Url::from_directory_path(&ddw).unwrap(),
            reference: GitReference::DefaultBranch,
            sha,
        };
        let sources = fixture
            .collect_locked("ddwCommon = { legacy-quark = true }\n", &|_| {
                Some(source.clone())
            })
            .unwrap();
        assert_eq!(sources["ddwcommon"].source, source);
        assert!(fixture
            .collect("ddwCommon = { legacy-quark = true }\n")
            .is_err());
    }

    /// a repository `name` at the fixture's root with `files`, returns its URL without `/`
    fn repo(fixture: &Fixture, name: &str, files: &[(&str, &str)]) -> String {
        let path = fixture.path(name);
        commit_files(&path, files);
        url(&path).trim_end_matches('/').to_string()
    }

    fn dependency_names(package: &SourcePackage) -> Vec<String> {
        package
            .dependencies
            .iter()
            .map(|dependency| dependency.name.clone())
            .collect()
    }

    #[test]
    fn test_quark_file_dependencies() {
        let fixture = Fixture::new();
        let baz = repo(&fixture, "baz", &[("Baz.sc", "Baz {}")]);
        let qux = repo(&fixture, "qux", &[("Qux.sc", "Qux {}")]);
        tag(&fixture.path("qux"), "v1");
        repo(
            &fixture,
            "quarks",
            &[("directory.txt", &format!("Baz={baz}\n"))],
        );
        let bar = repo(
            &fixture,
            "bar",
            &[(
                "Bar.quark",
                &format!("(name: \"Bar\", dependencies: [\"alpha\", \"Baz\", \"{qux}@tags/v1\"])"),
            )],
        );
        let sources = fixture
            .collect(&format!(
                "bar = {{ legacy-quark = true, git = {{ url = \"{bar}\" }} }}\n"
            ))
            .unwrap();
        // alpha is in the index, so it's a requirement against it, not a source
        assert_eq!(sources.keys().collect::<Vec<_>>(), ["bar", "baz", "qux"]);
        assert_eq!(dependency_names(&sources["bar"]), ["alpha", "baz", "qux"]);
        assert_eq!(sources["baz"].required_by, "bar");
        assert!(sources["qux"]
            .source
            .to_string()
            .starts_with(&format!("git+{qux}?tag=v1#")));
    }

    #[test]
    fn test_ignore_quark_deps() {
        let fixture = Fixture::new();
        let bar = repo(
            &fixture,
            "bar",
            &[("bar.quark", "(dependencies: [\"Unknown\"])")],
        );
        let sources = fixture
            .collect(&format!(
                "bar = {{ legacy-quark = true, ignore-quark-deps = true, git = {{ url = \"{bar}\" }} }}\n"
            ))
            .unwrap();
        assert!(sources["bar"].dependencies.is_empty());
        // without the flag, Unknown is looked up in the quarks directory
        let err = fixture
            .collect(&format!(
                "bar = {{ legacy-quark = true, git = {{ url = \"{bar}\" }} }}\n"
            ))
            .unwrap_err();
        assert!(format!("{err:#}").contains("quarks directory"), "{err:#}");
    }

    #[test]
    fn test_unreadable_quark_file() {
        let fixture = Fixture::new();
        let bar = repo(
            &fixture,
            "bar",
            &[(
                "Bar.quark",
                "(dependencies: [] ++ if(true) { [\"Unknown\"] })",
            )],
        );
        let sources = fixture
            .collect(&format!(
                "bar = {{ legacy-quark = true, git = {{ url = \"{bar}\" }} }}\n"
            ))
            .unwrap();
        assert!(sources["bar"].dependencies.is_empty());
    }

    #[test]
    fn test_first_quark_dependency_wins() {
        let fixture = Fixture::new();
        let foo = repo(&fixture, "foo", &[("Foo.sc", "Foo {}")]);
        tag(&fixture.path("foo"), "v1");
        repo(
            &fixture,
            "quarks",
            &[("directory.txt", &format!("Foo={foo}\n"))],
        );
        let a = repo(
            &fixture,
            "a",
            &[("a.quark", "(dependencies: [\"Foo@tags/v1\"])")],
        );
        let b = repo(&fixture, "b", &[("b.quark", "(dependencies: [\"Foo\"])")]);
        let sources = fixture
            .collect(&format!(
                "a = {{ legacy-quark = true, git = {{ url = \"{a}\" }} }}\nb = {{ legacy-quark = true, git = {{ url = \"{b}\" }} }}\n"
            ))
            .unwrap();
        assert_eq!(sources["foo"].required_by, "a");
        assert!(sources["foo"].source.to_string().contains("?tag=v1#"));
        assert_eq!(dependency_names(&sources["b"]), ["foo"]);
    }

    #[test]
    fn test_find_quark_file() {
        let files = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            find_quark_file("ddwCommon", &files(&["README.md", "ddwCommon.quark"])),
            Some("ddwCommon.quark".to_string())
        );
        // e.g. Modality-toolkit has Modality.quark
        assert_eq!(
            find_quark_file("Modality-toolkit", &files(&["Modality.quark"])),
            Some("Modality.quark".to_string())
        );
        assert_eq!(find_quark_file("x", &files(&["a.quark", "b.quark"])), None);
        assert_eq!(find_quark_file("x", &files(&["x.sc"])), None);
    }

    #[test]
    fn test_ignore_quark_deps_of_index_releases() {
        let fixture = Fixture::new();
        let bar_path = fixture.path("bar");
        let sha = commit_files(&bar_path, &[("Bar.quark", "(dependencies: [\"alpha\"])")]);
        let url = Url::from_directory_path(&bar_path).unwrap();
        let required = |required_by: &str, ignore_quark_deps: bool| IndexGitDependency {
            name: "Bar".to_string(),
            url: url.clone(),
            sha: sha.clone(),
            ignore_quark_deps,
            required_by: required_by.to_string(),
        };
        let bar_dependencies = |from_index: &[IndexGitDependency], toml: &str| {
            dependency_names(&fixture.collect_all(toml, from_index, &|_| None).unwrap()["bar"])
        };
        // only if every release says so
        assert_eq!(
            bar_dependencies(&[required("a 1.0.0", true), required("b 1.0.0", false)], ""),
            ["alpha"]
        );
        assert!(
            bar_dependencies(&[required("a 1.0.0", true), required("b 1.0.0", true)], "")
                .is_empty()
        );
        // a direct dependency of the project decides
        let direct = format!(
            "bar = {{ legacy-quark = true, ignore-quark-deps = true, git = {{ url = \"{url}\", ref = \"{sha}\" }} }}\n"
        );
        assert!(bar_dependencies(&[required("a 1.0.0", false)], &direct).is_empty());
    }
}
