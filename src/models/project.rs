use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

use anyhow::{bail, ensure, Context};
use pubgrub::SemanticVersion;
use semver::VersionReq;
use tracing::{debug, info, instrument, trace, warn};
use url::Url;

use crate::models::project_manifest::{
    MagnetManifestV1DependenciesValue, NamedDependency, ProjectManifest,
};
use crate::{
    config::constants::{magnet_home, GIT_DIR, MAGNET_LOCK_FILE, MAGNET_PROJECT_FILE},
    git_client::{client::GitClient, models::GitDependency, source::GitCache},
    legacy::directory::{quarks_directory_url, QuarksDirectory},
    models::{
        directories::ProjectDirectory,
        gitignore::write_gitignore,
        language_config::{LanguageConfig, SCLANG_CONF_FILE},
        lock_file::{LockDependency, LockFile, LockSource},
        package::{ArtifactKey, ArtifactKind, ArtifactTags, PackageArchive},
        project_manifest::{
            MagnetManifestV1, MagnetManifestV1EntrypointItem, MagnetManifestV1EntrypointItemKind,
            ProjectLayout,
        },
        server_options::{write_server_options, DEV_DIR},
    },
    packaging::{
        collect::{collect_binaries, collect_files, PackageFiles},
        compression,
        naming::artifact_file_name,
    },
    resolver::{
        online_provider::{
            resolve_dependencies, sclang_lower_bound, RootProvider, ToRanges, ROOT_PACKAGE,
            SCLANG_PACKAGE,
        },
        package_downloader::{Artifact, PackageDownloader},
        sources::{collect_sources, IndexGitDependency, SourcePackage},
    },
    sclang::{Sclang, MAGNET_SCLANG_ENV},
    target::{LazyTarget, Target},
};

/// how often the git dependencies of index packages may change the resolution, see
/// [Project::resolve_preferring]
const MAX_RESOLVE_ROUNDS: usize = 8;

/// number of artifacts which get downloaded at the same time
const PARALLEL_DOWNLOADS: usize = 4;

/// prefix of the temporary directories used while extracting a package
const STAGING_PREFIX: &str = ".magnet-";

/// The Project struct is used to manage files of a project, such as
/// adding a dependency, creating a lockfile or packaging up for release.
pub struct Project {
    project_manifest: ProjectManifest,
    /// `None` if the project has no lock file yet
    lock_file: Option<LockFile>,
    path: PathBuf,
    /// this machine, for the binaries of leptons
    target: LazyTarget,
    /// clones of the git dependencies
    git_cache: GitCache,
    /// repository of the legacy quarks directory (`directory.txt`)
    quarks_directory: Url,
}

/// The manifest's dependencies by their spelling in it
type ManifestDependencies = HashMap<String, MagnetManifestV1DependenciesValue>;

/// The version of every resolved package, and the git and path packages among them
struct Resolution {
    versions: HashMap<String, SemanticVersion>,
    sources: BTreeMap<String, SourcePackage>,
}

/// What [Project::create] writes besides the manifest
#[derive(Debug, Clone, Copy)]
pub struct CreateOptions {
    /// the [ProjectDirectory::SCAFFOLD] folders
    pub directories: bool,
    /// the `.gitignore`, or the generated files added to an existing one
    pub gitignore: bool,
    /// decides the `[artifact]` template
    pub layout: ProjectLayout,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            directories: true,
            gitignore: true,
            layout: ProjectLayout::Magnet,
        }
    }
}

impl Project {
    #[instrument(skip(magnet_manifest))]
    /// The lock file gets created by the first `add` or `install`, since it needs an index
    pub fn create(
        path: &Path,
        magnet_manifest: MagnetManifestV1,
        options: CreateOptions,
    ) -> anyhow::Result<Self> {
        let project_manifest = ProjectManifest::write_new(
            path.join(MAGNET_PROJECT_FILE),
            &magnet_manifest,
            options.layout,
        )?;
        let project = Self {
            project_manifest,
            lock_file: None,
            path: path.into(),
            target: LazyTarget::default(),
            git_cache: GitCache::new(magnet_home()?.join(GIT_DIR)),
            quarks_directory: quarks_directory_url()?,
        };
        project.generate_project_directories(options.directories)?;
        if options.gitignore {
            write_gitignore(&project.path)?;
        }
        info!("Created project");
        Ok(project)
    }

    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            project_manifest: ProjectManifest::open(path.join(MAGNET_PROJECT_FILE))?,
            lock_file: Self::open_lock_file(path)?,
            path: path.to_path_buf(),
            target: LazyTarget::default(),
            git_cache: GitCache::new(magnet_home()?.join(GIT_DIR)),
            quarks_directory: quarks_directory_url()?,
        })
    }

    /// The machine whose binaries get packaged and installed, detected by default
    pub fn set_target(&mut self, target: LazyTarget) {
        self.target = target;
    }

    /// Where git dependencies get cloned, `~/.magnet/git` by default
    pub fn set_git_cache(&mut self, dir: PathBuf) {
        self.git_cache = GitCache::new(dir);
    }

    /// The repository of `directory.txt`, see [quarks_directory_url]
    pub fn set_quarks_directory(&mut self, url: Url) {
        self.quarks_directory = url;
    }

    fn open_lock_file(path: &Path) -> anyhow::Result<Option<LockFile>> {
        let lock_path = path.join(MAGNET_LOCK_FILE);
        match lock_path.exists() {
            true => Ok(Some(LockFile::open(&lock_path)?)),
            false => Ok(None),
        }
    }

    #[instrument(skip(self))]
    /// `magnet_packages/`, plus the [ProjectDirectory::SCAFFOLD] folders if `scaffold`
    fn generate_project_directories(&self, scaffold: bool) -> anyhow::Result<()> {
        let scaffold = match scaffold {
            true => ProjectDirectory::SCAFFOLD.as_slice(),
            false => &[],
        };
        for directory in scaffold.iter().cloned().chain([ProjectDirectory::Packages]) {
            trace!(?directory, "Create project folder");
            let path = self.path.join(String::from(directory));
            fs::create_dir_all(&path)
                .with_context(|| format!("Could not create folder {path:?}"))?;
        }
        Ok(())
    }

    fn project_dir(&self, directory: ProjectDirectory) -> PathBuf {
        self.path.join(String::from(directory))
    }

    /// Adds a dependency from the index, or changes the version requirement of an existing one.
    /// Without a `constraint`, the newest matching version is used as a caret requirement,
    /// e.g. `^1.1.0`.
    /// The manifest and the lock file are only written if the dependencies can be resolved.
    /// Adds a git or legacy quark dependency (e.g. a legacy quark pinned to a commit), or
    /// replaces the entry of that name, keeping its spelling. Like [Self::add_dependency],
    /// the manifest and the lock file are only written if the dependencies can be resolved.
    pub fn add_source_dependency(
        &mut self,
        provider: &GitClient,
        name: String,
        dependency: MagnetManifestV1DependenciesValue,
    ) -> anyhow::Result<()> {
        ensure!(
            MagnetManifestV1::is_valid_name(&name),
            "{name} is not a valid package name"
        );
        let normalized_name = MagnetManifestV1::normalize_name(&name);
        let name = self
            .project_manifest
            .get_manifest()
            .dependencies
            .keys()
            .find(|key| MagnetManifestV1::normalize_name(key) == normalized_name)
            .cloned()
            .unwrap_or(name);

        let mut dependencies = self.manifest_dependency_values();
        dependencies.insert(name.clone(), dependency.clone());
        let resolution = self
            .resolve(provider, dependencies)
            .with_context(|| format!("Failed to add dependency {name}"))?;
        info!("Add {name}");

        self.project_manifest
            .add_dependency(NamedDependency { name, dependency })?;
        self.write_lock(provider, &resolution)?;

        self.reload_from_file()?;
        self.write_language_config()?;
        Ok(())
    }

    pub fn add_dependency(
        &mut self,
        provider: &GitClient,
        name: String,
        constraint: Option<VersionReq>,
    ) -> anyhow::Result<()> {
        ensure!(
            MagnetManifestV1::is_valid_name(&name),
            "{name} is not a valid package name"
        );
        let normalized_name = MagnetManifestV1::normalize_name(&name);

        // an existing entry keeps its spelling, so it gets replaced instead of duplicated
        let existing = self
            .project_manifest
            .get_manifest()
            .dependencies
            .iter()
            .find(|(key, _)| MagnetManifestV1::normalize_name(key) == normalized_name);
        let name = match existing {
            Some((key, dependency)) => {
                ensure!(
                    is_index_dependency(dependency),
                    "{key} is already a git, path or legacy quark dependency, please edit it in {MAGNET_PROJECT_FILE}"
                );
                key.clone()
            }
            None => name,
        };

        let mut dependencies = self.manifest_dependency_values();
        dependencies.insert(
            name.clone(),
            MagnetManifestV1DependenciesValue::String(
                constraint.clone().unwrap_or(VersionReq::STAR).to_string(),
            ),
        );

        let resolution = self
            .resolve(provider, dependencies)
            .with_context(|| format!("Failed to add dependency {name}"))?;

        let constraint = match constraint {
            Some(constraint) => constraint,
            None => {
                let version = resolution
                    .versions
                    .get(&normalized_name)
                    .with_context(|| format!("{name} is missing in the resolved dependencies"))?;
                format!("^{version}").parse()?
            }
        };
        info!("Add {name} {constraint}");

        self.project_manifest.add_dependency(NamedDependency {
            name,
            dependency: MagnetManifestV1DependenciesValue::String(constraint.to_string()),
        })?;
        self.write_lock(provider, &resolution)?;

        self.reload_from_file()?;
        self.write_language_config()?;

        Ok(())
    }

    /// Writes the `sclang_conf.yaml` for the current state of the project
    /// Without a usable sclang, the config is still written, but without isolation
    pub fn write_language_config(&self) -> anyhow::Result<()> {
        let sclang = match Sclang::find() {
            Ok(Some(sclang)) => Some(sclang),
            Ok(None) => {
                warn!("Could not find sclang, so {SCLANG_CONF_FILE} does not exclude globally installed extensions. Set {MAGNET_SCLANG_ENV} to fix this.");
                None
            }
            Err(err) => {
                warn!(
                    "{err:#}, so {SCLANG_CONF_FILE} does not exclude globally installed extensions"
                );
                None
            }
        };
        self.write_language_config_for(sclang)
    }

    /// Writes `sclang_conf.yaml` and the class which isolates the server's plugins
    /// (DESIGN.md "Server plugins"), which is compiled via the include paths of the former
    fn write_language_config_for(&self, sclang: Option<Sclang>) -> anyhow::Result<()> {
        LanguageConfig::for_project(&self.path, self.project_manifest.get_manifest(), sclang)?
            .write(&self.path)?;
        write_server_options(
            &self.project_dir(ProjectDirectory::Packages),
            self.has_lepton(),
        )
    }

    /// If the project is a lepton, copies its binaries of `platform-paths` for this platform
    /// into `magnet_packages/.dev/<name>/plugins`, so the server loads the local build
    /// (DESIGN.md "Server plugins"). A missing build only warns, since it may not exist yet.
    fn copy_dev_plugins(&self) -> anyhow::Result<()> {
        if !self.has_lepton() {
            return Ok(());
        }
        let manifest = self.project_manifest.get_manifest();
        let name = MagnetManifestV1::normalize_name(&manifest.package.name);
        let dev_dir = self
            .project_dir(ProjectDirectory::Packages)
            .join(DEV_DIR)
            .join(&name);
        if dev_dir.exists() {
            fs::remove_dir_all(&dev_dir)
                .with_context(|| format!("Could not remove {dev_dir:?}"))?;
        }
        let platform = match self.target.platform() {
            Ok(platform) => platform,
            Err(err) => {
                warn!("{err:#}, so the binaries of {name} don't get loaded by the server");
                return Ok(());
            }
        };
        let (binaries, unmatched) = collect_binaries(&self.path, manifest, &platform)?;
        if binaries.is_empty() && unmatched.is_empty() {
            warn!("{MAGNET_PROJECT_FILE} has no `platform-paths` for {platform}, so the server doesn't load the binaries of {name}");
        }
        for glob in unmatched {
            warn!(
                "{glob} of `platform-paths` doesn't select any file, maybe {name} isn't built yet"
            );
        }
        let entries = package_entries(
            &self.path,
            &PackageFiles {
                files: vec![],
                binaries,
            },
            PLUGINS_DIR,
        )?;
        for (path, source) in &entries {
            let destination = dev_dir.join(path);
            fs::create_dir_all(destination.parent().context("A plugin has a folder")?)?;
            fs::copy(source, &destination)
                .with_context(|| format!("Could not copy {source:?} to {destination:?}"))?;
        }
        debug!(
            ?dev_dir,
            count = entries.len(),
            "Copied the binaries of the project"
        );
        Ok(())
    }

    pub fn manifest(&self) -> &MagnetManifestV1 {
        self.project_manifest.get_manifest()
    }

    /// The project root, which holds `magnet.toml`
    pub fn root(&self) -> &Path {
        &self.path
    }

    /// `true` if the project builds a lepton, i.e. has `[artifact.lepton]`
    pub fn has_lepton(&self) -> bool {
        self.project_manifest
            .get_manifest()
            .artifact_kinds()
            .contains(&ArtifactKind::Lepton)
    }

    /// The entrypoints of the project, which can be started via `magnet run`
    pub fn entrypoints(&self) -> &[MagnetManifestV1EntrypointItem] {
        &self.project_manifest.get_manifest().entrypoint
    }

    /// Runs an entrypoint of the project from the project root and returns its exit status.
    /// `args` are passed on to the script or command.
    /// `find_sclang` is only called for sclang entrypoints, which get started with a language
    /// config written for exactly this sclang.
    pub fn run_entrypoint(
        &self,
        name: &str,
        args: &[String],
        find_sclang: impl FnOnce() -> anyhow::Result<Option<Sclang>>,
    ) -> anyhow::Result<ExitStatus> {
        let entrypoint = self
            .entrypoints()
            .iter()
            .find(|entrypoint| entrypoint.name == name)
            .with_context(|| {
                let names: Vec<&str> = self
                    .entrypoints()
                    .iter()
                    .map(|entrypoint| entrypoint.name.as_str())
                    .collect();
                match names.is_empty() {
                    true => {
                        format!("Unknown entrypoint {name}, {MAGNET_PROJECT_FILE} defines none")
                    }
                    false => format!(
                        "Unknown entrypoint {name}, available are: {}",
                        names.join(", ")
                    ),
                }
            })?;
        self.ensure_installed()?;
        self.copy_dev_plugins()?;

        let root = std::path::absolute(&self.path)?;
        let mut command = match entrypoint.kind {
            MagnetManifestV1EntrypointItemKind::Sclang => {
                let sclang = find_sclang()?.with_context(|| {
                    format!("Could not find sclang, set {MAGNET_SCLANG_ENV} to its path")
                })?;
                let script = root.join(&entrypoint.command);
                ensure!(
                    script.is_file(),
                    "The script {} of the entrypoint {name} does not exist",
                    entrypoint.command
                );
                // the config must match the class library of the sclang which gets started
                self.write_language_config_for(Some(sclang.clone()))?;
                let mut command = Command::new(&sclang.binary);
                command
                    .arg("-l")
                    .arg(root.join(SCLANG_CONF_FILE))
                    // otherwise sclang parses args like `--foo` as its own options
                    .arg("--")
                    .arg(&entrypoint.command)
                    .args(args);
                command
            }
            MagnetManifestV1EntrypointItemKind::System => shell_command(&entrypoint.command, args),
        };
        info!("Run {name}");
        command
            .current_dir(&root)
            .status()
            .with_context(|| format!("Could not start the entrypoint {name}"))
    }

    /// Fails if the locked dependencies are not installed, e.g. in a fresh clone
    fn ensure_installed(&self) -> anyhow::Result<()> {
        if self.manifest_dependencies()?.is_empty() {
            return Ok(());
        }
        let installed = self.lock_covers_manifest()?
            && self.lock_file.iter().all(|lock_file| {
                lock_file
                    .dependencies
                    .iter()
                    .all(|(name, _)| self.package_dir(name).is_dir())
            });
        ensure!(
            installed,
            "The dependencies are not installed, run `magnet install` first"
        );
        Ok(())
    }

    /// Removes a dependency (in any spelling) from the manifest, and the packages which are
    /// not needed anymore from the lock file and `magnet_packages`.
    /// The manifest and the lock file are only written if the dependencies can be resolved.
    /// Returns `true` if the locked versions of other packages changed, which then need to be
    /// installed.
    pub fn remove_dependency(&mut self, provider: &GitClient, name: &str) -> anyhow::Result<bool> {
        let normalized_name = MagnetManifestV1::normalize_name(name);
        let key = self
            .project_manifest
            .get_manifest()
            .dependencies
            .keys()
            .find(|key| MagnetManifestV1::normalize_name(key) == normalized_name)
            .cloned()
            .with_context(|| format!("{name} is not a dependency of this project"))?;

        let mut dependencies = self.manifest_dependency_values();
        dependencies.remove(&key);
        let resolution = self
            .resolve(provider, dependencies)
            .with_context(|| format!("Failed to remove dependency {key}"))?;

        let previous = self.lock_file.clone();
        info!("Remove {key}");
        self.project_manifest.remove_dependency(&key)?;
        self.write_lock(provider, &resolution)?;
        self.reload_from_file()?;

        let lock_file = self
            .lock_file
            .as_ref()
            .context("A lock file should exist after resolving")?;
        self.prune_packages(lock_file)?;
        self.write_language_config()?;

        // removing a dependency only changes other versions if a locked one is gone
        let changed = lock_file.dependencies.iter().any(|(name, dependency)| {
            previous
                .as_ref()
                .and_then(|previous| previous.dependencies.get(name))
                .is_some_and(|locked| {
                    locked.version != dependency.version || locked.source != dependency.source
                })
        });
        Ok(changed)
    }

    fn manifest_dependency_values(&self) -> ManifestDependencies {
        self.project_manifest.get_manifest().dependencies.clone()
    }

    /// The dependencies of the manifest, as requirements for the resolver
    fn manifest_dependencies(&self) -> anyhow::Result<Vec<GitDependency>> {
        requirements(&self.manifest_dependency_values())
    }

    /// The git and path packages of `dependencies`, see [collect_sources]
    fn collect_sources(
        &self,
        dependencies: &ManifestDependencies,
        from_index_releases: &[IndexGitDependency],
        in_index: impl Fn(&str) -> anyhow::Result<bool>,
        locked: impl Fn(&str) -> Option<LockSource>,
    ) -> anyhow::Result<BTreeMap<String, SourcePackage>> {
        let directory = QuarksDirectory::new(self.quarks_directory.clone(), &self.git_cache);
        collect_sources(
            &self.path,
            dependencies,
            from_index_releases,
            &self.git_cache,
            &directory,
            in_index,
            locked,
        )
    }

    /// The locked source of `name`, if the lock file has one
    fn locked_source(&self, name: &str) -> Option<LockSource> {
        self.lock_file
            .as_ref()
            .and_then(|lock_file| lock_file.dependencies.get(name))
            .map(|dependency| dependency.source.clone())
    }

    fn resolve(
        &self,
        provider: &GitClient,
        dependencies: ManifestDependencies,
    ) -> anyhow::Result<Resolution> {
        // keep the locked versions where possible, so only the necessary changes are made
        self.resolve_preferring(provider, dependencies, |_| true)
    }

    /// Like [Self::resolve], but only prefers the locked versions (and commits of git
    /// dependencies) of the packages `keep` accepts.
    /// The git dependencies of index packages are only known once these are chosen, so this
    /// resolves in rounds: the git dependencies of the chosen index releases become sources of
    /// the next round, until a round chooses releases whose git dependencies are all collected.
    fn resolve_preferring(
        &self,
        provider: &GitClient,
        dependencies: ManifestDependencies,
        keep: impl Fn(&str) -> bool,
    ) -> anyhow::Result<Resolution> {
        let preferred: HashMap<String, SemanticVersion> = self
            .lock_file
            .iter()
            .flat_map(|lock_file| &lock_file.dependencies)
            .filter(|(name, _)| keep(name))
            .map(|(name, dependency)| (name.clone(), dependency.version))
            .collect();
        let mut from_index_releases: Vec<IndexGitDependency> = vec![];
        for _ in 0..MAX_RESOLVE_ROUNDS {
            let sources = self.collect_sources(
                &dependencies,
                &from_index_releases,
                |name| Ok(provider.get_releases(name)?.is_some()),
                |name| keep(name).then(|| self.locked_source(name)).flatten(),
            )?;
            let versions: HashMap<String, SemanticVersion> = resolve_dependencies(&RootProvider {
                git_provider: provider,
                dependencies: requirements(&dependencies)?,
                preferred: preferred.clone(),
                sclang: self.sclang_requirement()?,
                target: &self.target,
                sources: &sources,
            })?
            .into_iter()
            .filter(|(name, _)| name != SCLANG_PACKAGE)
            .collect();
            trace!(?versions, "Resolved dependencies");

            let from_index: HashMap<String, SemanticVersion> = versions
                .iter()
                .filter(|(name, _)| *name != ROOT_PACKAGE && !sources.contains_key(*name))
                .map(|(name, version)| (name.clone(), *version))
                .collect();
            let required = index_git_dependencies(provider, &from_index)?;
            if required == from_index_releases {
                self.warn_about_sclang(provider, &from_index)?;
                return Ok(Resolution { versions, sources });
            }
            from_index_releases = required;
        }
        bail!("The git dependencies of the index packages keep changing the resolution, after {MAX_RESOLVE_ROUNDS} rounds")
    }

    /// The `sclang` range of `[package]`
    fn sclang_requirement(&self) -> anyhow::Result<Option<VersionReq>> {
        self.project_manifest
            .get_manifest()
            .package
            .sclang
            .as_ref()
            .map(|sclang| {
                sclang.parse().with_context(|| {
                    format!("`sclang = \"{sclang}\"` in {MAGNET_PROJECT_FILE} is not a valid version range")
                })
            })
            .transpose()
    }

    /// Warns if the found sclang is outside of the project's `sclang` range, or, if the project
    /// has none, if a resolved version requires a newer sclang (DESIGN.md "SuperCollider version")
    fn warn_about_sclang(
        &self,
        provider: &GitClient,
        tree: &HashMap<String, SemanticVersion>,
    ) -> anyhow::Result<()> {
        // a missing sclang gets reported by write_language_config
        let Ok(Some(sclang)) = Sclang::find() else {
            return Ok(());
        };
        let version = match sclang.version() {
            Ok(version) => version,
            Err(err) => {
                warn!("{err:#}");
                return Ok(());
            }
        };
        if let Some(requirement) = self.sclang_requirement()? {
            if !requirement.to_ranges().contains(&version) {
                warn!(
                    "The project requires sclang {requirement}, but the found sclang {:?} is {version}",
                    sclang.binary
                );
            }
            return Ok(());
        }
        let sorted: BTreeMap<&String, &SemanticVersion> = tree.iter().collect();
        for (name, locked) in sorted {
            let requirement = provider
                .get_release(name, locked)?
                .and_then(|release| release.sclang);
            if let Some(requirement) = requirement {
                if !sclang_lower_bound(&requirement).contains(&version) {
                    warn!(
                        "{name} {locked} requires sclang {requirement}, but the found sclang is {version}. Set `sclang` in [package] of {MAGNET_PROJECT_FILE} to resolve versions which support it"
                    );
                }
            }
        }
        Ok(())
    }

    /// Resolves the dependencies again without the locked versions of `names` (in any spelling),
    /// or of all packages if `names` is empty, so they move to the newest versions the
    /// requirements allow. Writes the lock file, but installs nothing. Returns the changes as
    /// `(name, locked version, new version)`, sorted by name.
    pub fn update(
        &mut self,
        provider: &GitClient,
        names: &[String],
    ) -> anyhow::Result<Vec<(String, Option<LockDependency>, Option<LockDependency>)>> {
        let names: HashSet<String> = names
            .iter()
            .map(|name| MagnetManifestV1::normalize_name(name))
            .collect();
        let locked: BTreeMap<String, LockDependency> = self
            .lock_file
            .iter()
            .flat_map(|lock_file| lock_file.dependencies.clone())
            .collect();
        let mut unknown: Vec<&str> = names
            .iter()
            .map(|name| name.as_str())
            .filter(|name| !locked.contains_key(*name))
            .collect();
        unknown.sort();
        ensure!(
            unknown.is_empty(),
            "Not in {MAGNET_LOCK_FILE}: {}",
            unknown.join(", ")
        );

        let resolution =
            self.resolve_preferring(provider, self.manifest_dependency_values(), |name| {
                !names.is_empty() && !names.contains(name)
            })?;
        self.write_lock(provider, &resolution)?;
        self.write_language_config()?;

        let updated: BTreeMap<String, LockDependency> = self
            .lock_file
            .iter()
            .flat_map(|lock_file| lock_file.dependencies.clone())
            .collect();
        let all: std::collections::BTreeSet<&String> =
            locked.keys().chain(updated.keys()).collect();
        let pinned = |dependency: Option<&LockDependency>| {
            dependency.map(|dependency| (dependency.version, dependency.source.clone()))
        };
        Ok(all
            .into_iter()
            .filter(|name| pinned(locked.get(*name)) != pinned(updated.get(*name)))
            .map(|name| {
                (
                    name.clone(),
                    locked.get(name).cloned(),
                    updated.get(name).cloned(),
                )
            })
            .collect())
    }

    /// Warns about yanked locked versions, which still get installed, and fails for locked
    /// versions which are not in the index anymore (DESIGN.md "Deletion")
    fn check_locked_versions(
        &self,
        provider: &GitClient,
        lock_file: &LockFile,
    ) -> anyhow::Result<()> {
        let mut missing = vec![];
        for (name, dependency) in &lock_file.dependencies {
            if dependency.source != LockSource::Index {
                continue;
            }
            match provider.get_release(name, &dependency.version)? {
                Some(release) if release.yanked => warn!(
                    "{name} {} is yanked, it gets installed since it is locked. Run `magnet update {name}` to move to another version",
                    dependency.version
                ),
                Some(_) => {}
                None => missing.push((name.as_str(), dependency.version)),
            }
        }
        if !missing.is_empty() {
            let versions: Vec<String> = missing
                .iter()
                .map(|(name, version)| format!("{name} {version}"))
                .collect();
            let names: Vec<&str> = missing.iter().map(|(name, _)| *name).collect();
            bail!(
                "Not in the index anymore: {}. They were deleted, or the local copy of the index is outdated. Run `magnet update {}` to resolve them again",
                versions.join(", "),
                names.join(" ")
            );
        }
        Ok(())
    }

    /// Writes the lock file for a resolution, using the details of each release from the
    /// index, and the source of git and path packages
    fn write_lock(&mut self, provider: &GitClient, resolution: &Resolution) -> anyhow::Result<()> {
        let lock_file = LockFile::from_tree(
            provider.url().clone(),
            provider.http_url()?,
            &resolution.versions,
            |name, version| match resolution.sources.get(name) {
                Some(source) => Ok(LockDependency {
                    version: source.version,
                    kinds: source.kinds.clone(),
                    source: source.source.clone(),
                    checksums: BTreeMap::new(),
                }),
                None => provider
                    .get_release(name, version)?
                    .map(|release| LockDependency::from(&release))
                    .with_context(|| format!("{name} {version} is missing in the index")),
            },
        )?;
        lock_file.write(&self.path.join(MAGNET_LOCK_FILE))?;
        self.lock_file = Some(lock_file);
        Ok(())
    }

    fn reload_from_file(&mut self) -> anyhow::Result<()> {
        self.project_manifest = ProjectManifest::open(self.path.join(MAGNET_PROJECT_FILE))?;
        self.lock_file = Self::open_lock_file(&self.path)?;
        Ok(())
    }

    /// `true` if every dependency of the manifest, and of its git and path dependencies, is
    /// locked with a version which satisfies it, and the git and path dependencies are locked
    /// with their current source (for git: the same URL and reference)
    fn lock_covers_manifest(&self) -> anyhow::Result<bool> {
        let Some(lock_file) = &self.lock_file else {
            return Ok(false);
        };
        let dependencies = self.manifest_dependency_values();
        // without the index: a dependency of a .quark file which isn't locked from git counts
        // as one of the index, so an unknown one fails the check below instead of a lookup
        let sources = self.collect_sources(
            &dependencies,
            &[],
            |name| {
                Ok(!matches!(
                    self.locked_source(name),
                    Some(LockSource::Git { .. })
                ))
            },
            |name| self.locked_source(name),
        )?;
        let sources_locked = sources.iter().all(|(name, source)| {
            lock_file.dependencies.get(name).is_some_and(|locked| {
                locked.source == source.source
                    && locked.version == source.version
                    && locked.kinds == source.kinds
            })
        });
        let mut requirements = requirements(&dependencies)?;
        requirements.extend(
            sources
                .values()
                .flat_map(|source| source.dependencies.clone()),
        );
        Ok(sources_locked
            && requirements.iter().all(|dependency| {
                lock_file
                    .dependencies
                    .get(&MagnetManifestV1::normalize_name(&dependency.name))
                    .is_some_and(|locked| {
                        dependency.constraint.to_ranges().contains(&locked.version)
                    })
            }))
    }

    /// Installs the locked dependencies into `magnet_packages/<kind>s/<name>`, and removes the
    /// packages which are not locked (anymore).
    /// The dependencies only get resolved if the lock file does not cover the manifest.
    /// With `locked`, the lock file is never written, and anything which would change it fails.
    pub async fn install(
        &mut self,
        provider: &GitClient,
        downloader: &PackageDownloader,
        locked: bool,
    ) -> anyhow::Result<()> {
        if !self.lock_covers_manifest()? {
            ensure!(
                !locked,
                "{MAGNET_LOCK_FILE} is missing or doesn't match {MAGNET_PROJECT_FILE}, run `magnet install` without --locked to resolve the dependencies"
            );
            info!("{MAGNET_LOCK_FILE} is missing or outdated, resolving the dependencies");
            let resolution = self.resolve(provider, self.manifest_dependency_values())?;
            self.write_lock(provider, &resolution)?;
        } else if let Some(lock_file) = &self.lock_file {
            let locked = lock_file
                .dependencies
                .iter()
                .filter(|(_, dependency)| dependency.source == LockSource::Index)
                .map(|(name, dependency)| (name.clone(), dependency.version))
                .collect();
            self.warn_about_sclang(provider, &locked)?;
        }
        self.install_locked(provider, downloader, locked).await
    }

    /// Installs exactly what the lock file holds into `magnet_packages/` and removes what it doesn't,
    /// without resolving. `add`, `remove` and `update` call this after writing the lock.
    /// The lock file only changes if it lacks the checksum of a binary for this machine, which
    /// got published later (DESIGN.md "Missing binary"); with `locked`, this fails instead.
    pub async fn install_locked(
        &mut self,
        provider: &GitClient,
        downloader: &PackageDownloader,
        locked: bool,
    ) -> anyhow::Result<()> {
        let mut lock_file = self
            .lock_file
            .clone()
            .context("A lock file should exist after resolving")?;
        self.check_locked_versions(provider, &lock_file)?;

        let overrides = self.overrides();
        for name in overrides.keys() {
            if !lock_file.dependencies.contains_key(name) {
                warn!("{name} is overridden in [overrides], but it isn't a locked dependency, so the override is ignored. A package which isn't in the index needs to be a `path` dependency");
            }
        }

        let mut artifacts = vec![];
        let mut missing = vec![];
        for (name, dependency) in lock_file.dependencies.iter_mut() {
            if overrides.contains_key(name) || dependency.source != LockSource::Index {
                continue;
            }
            match self.select_artifacts(provider, name, dependency)? {
                Ok(selected) => artifacts.extend(selected),
                Err(message) => missing.push(message),
            }
        }
        ensure!(missing.is_empty(), "{}", missing.join("\n"));
        if Some(&lock_file) != self.lock_file.as_ref() {
            ensure!(
                !locked,
                "{MAGNET_LOCK_FILE} lacks the checksums of binaries for {}, which were published later. Run `magnet install` without --locked to add them",
                self.target.get()?
            );
            lock_file.write(&self.path.join(MAGNET_LOCK_FILE))?;
            self.lock_file = Some(lock_file.clone());
        }

        let archives = downloader
            .download_all(&artifacts, PARALLEL_DOWNLOADS)
            .await?;
        // every kind of a package goes into its folder
        let mut packages: BTreeMap<&str, Vec<PathBuf>> = BTreeMap::new();
        for (artifact, archive) in artifacts.iter().zip(archives) {
            packages.entry(&artifact.name).or_default().push(archive);
        }
        for (name, archives) in &packages {
            self.install_package(name, archives)?;
        }
        let mut installed = packages.len();
        for (name, dependency) in &lock_file.dependencies {
            if overrides.contains_key(name) {
                continue;
            }
            match &dependency.source {
                LockSource::Index => continue,
                LockSource::Git { url, sha, .. } => {
                    ensure!(
                        !dependency.kinds.contains(&ArtifactKind::Lepton),
                        "{name} from {url} contains a lepton, which can only be installed from the index or a path dependency"
                    );
                    // exported first, since a magnet project only gets its selected files
                    let checkout = tempfile::tempdir()?;
                    self.git_cache.export(url, sha, checkout.path())?;
                    self.install_folder(
                        name,
                        &dependency.kinds,
                        checkout.path(),
                        &format!("{url} at {}", &sha[..sha.len().min(7)]),
                    )?;
                }
                LockSource::Path(path) => {
                    self.install_folder(
                        name,
                        &dependency.kinds,
                        &self.path.join(path),
                        &format!("{path:?}"),
                    )?;
                }
            }
            installed += 1;
        }
        for (name, folder) in &overrides {
            if let Some(dependency) = lock_file.dependencies.get(name) {
                self.install_folder(name, &dependency.kinds, folder, "[overrides]")?;
                installed += 1;
            }
        }

        self.prune_packages(&lock_file)?;
        self.copy_dev_plugins()?;
        self.write_language_config()?;
        info!("Installed {installed} packages");
        Ok(())
    }

    /// The artifacts of a locked package which get installed: one for each of its kinds
    /// (DESIGN.md "Artifact kinds"), i.e. the quark and the binary of a lepton which this
    /// machine loads. If the lock file has no binary for this machine, it may have been
    /// published later, so its checksum gets taken from the index and added to `dependency`.
    /// Without any, the error message which suggests a version with a binary (DESIGN.md
    /// "Missing binary").
    fn select_artifacts(
        &self,
        provider: &GitClient,
        name: &str,
        dependency: &mut LockDependency,
    ) -> anyhow::Result<Result<Vec<Artifact>, String>> {
        ensure!(
            dependency.source == LockSource::Index,
            "{name} has the source {}, which can not be installed yet",
            dependency.source
        );
        let version = dependency.version;
        ensure!(
            !dependency.kinds.is_empty(),
            "{name} {version} contains neither a quark nor a lepton"
        );
        let mut keys = vec![];
        for kind in dependency.kinds.clone() {
            match kind {
                ArtifactKind::Quark => keys.push(ArtifactKey::any(kind)),
                ArtifactKind::Lepton => {
                    let target = self.target.get().with_context(|| {
                        format!("{name} contains a lepton, whose binary depends on the platform and abi of this machine")
                    })?;
                    let locked: Vec<ArtifactTags> = dependency
                        .checksums
                        .keys()
                        .filter_map(|key| key.parse::<ArtifactKey>().ok())
                        .filter(|key| key.kind == ArtifactKind::Lepton)
                        .map(|key| key.tags)
                        .collect();
                    let tags = match target.best_artifact(&locked) {
                        Some(tags) => tags.clone(),
                        None => match self.late_checksum(provider, name, dependency, target)? {
                            Some(tags) => tags,
                            None => {
                                return Ok(Err(
                                    self.missing_binary(provider, name, version, target)?
                                ))
                            }
                        },
                    };
                    keys.push(ArtifactKey::new(kind, tags));
                }
                // never installed, only a lock file written by hand could list it
                ArtifactKind::Source => {}
            }
        }
        let artifacts = keys
            .into_iter()
            .map(|key| {
                let sha256 = dependency
                    .checksums
                    .get(&key.to_string())
                    .with_context(|| {
                        format!("{name} {version} has no {key} artifact in {MAGNET_LOCK_FILE}")
                    })?;
                Ok(Artifact {
                    name: name.to_string(),
                    version,
                    kind: key.kind,
                    abi: key.tags.abi,
                    platform: key.tags.platform,
                    sha256: sha256.clone(),
                })
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Ok(artifacts))
    }

    /// Adds the checksum of a binary for `target` from the index to `dependency`, if the
    /// index has one for the locked version. The checksums of the lock file never change, so
    /// one which differs from the index is an error.
    fn late_checksum(
        &self,
        provider: &GitClient,
        name: &str,
        dependency: &mut LockDependency,
        target: &Target,
    ) -> anyhow::Result<Option<ArtifactTags>> {
        let version = dependency.version;
        let release = provider
            .get_release(name, &version)?
            .with_context(|| format!("{name} {version} is missing in the index"))?;
        for artifact in &release.artifacts {
            if let Some(locked) = dependency.checksums.get(&artifact.key()) {
                ensure!(
                    *locked == artifact.sha256,
                    "The checksum of {name} {version} {} in {MAGNET_LOCK_FILE} differs from the index, which never replaces artifacts. Check where {MAGNET_LOCK_FILE} and the index come from",
                    artifact.key()
                );
            }
        }
        let artifacts = release.artifact_tags(ArtifactKind::Lepton);
        let Some(tags) = target.best_artifact(&artifacts) else {
            return Ok(None);
        };
        let key = ArtifactKey::new(ArtifactKind::Lepton, tags.clone());
        let sha256 = release
            .checksum(&key)
            .with_context(|| format!("{name} {version} has no {key} artifact"))?;
        info!("Add the checksum of {name} {version} {key} to {MAGNET_LOCK_FILE}, which was published after locking");
        dependency
            .checksums
            .insert(key.to_string(), sha256.to_string());
        Ok(Some(tags.clone()))
    }

    /// The error for a lepton version without a binary for `target`, which names the newest
    /// version with one, without resolving again: `magnet update` if the manifest's
    /// requirement allows it, otherwise `magnet add` with that exact version.
    fn missing_binary(
        &self,
        provider: &GitClient,
        name: &str,
        version: SemanticVersion,
        target: &Target,
    ) -> anyhow::Result<String> {
        let message = format!("{name} {version} has no binary for {target}");
        let newest = provider
            .get_releases(name)?
            .unwrap_or_default()
            .into_iter()
            .filter(|release| {
                !release.yanked
                    && target
                        .best_artifact(&release.artifact_tags(ArtifactKind::Lepton))
                        .is_some()
            })
            .map(|release| release.version)
            .max();
        let Some(newest) = newest else {
            return Ok(format!("{message}, and no other version has one either. A local build can be installed via [overrides] in {MAGNET_PROJECT_FILE}, e.g. {name} = {{ path = \"../{name}\" }}"));
        };
        let allowed = self.manifest_dependencies()?.iter().any(|dependency| {
            MagnetManifestV1::normalize_name(&dependency.name) == name
                && dependency.constraint.to_ranges().contains(&newest)
        });
        Ok(match allowed {
            true => format!("{message}. The newest version with one is {newest}, run `magnet update {name}` to move to it"),
            false => format!("{message}. The newest version with one is {newest}, run `magnet add {name}@={newest}` to use it"),
        })
    }

    /// The folders of `[overrides]` by the normalized package name, relative to the project
    fn overrides(&self) -> BTreeMap<String, PathBuf> {
        self.project_manifest
            .get_manifest()
            .overrides
            .iter()
            .map(|(name, value)| {
                (
                    MagnetManifestV1::normalize_name(name),
                    self.path.join(&value.path),
                )
            })
            .collect()
    }

    /// Installs `name` from a local `folder` (a git checkout, a path dependency or an
    /// override): what `magnet package` would select for its kinds if it has a `magnet.toml`
    /// (with uncommitted changes, since it's a local build), otherwise the whole folder.
    /// The binaries of a lepton need the platform and abi of this machine; without them, they
    /// aren't installed.
    /// The kinds of the folder's `magnet.toml` may differ from `locked`, e.g. an override with
    /// a local build which adds a lepton, which only gets a warning.
    fn install_folder(
        &self,
        name: &str,
        locked: &[ArtifactKind],
        folder: &Path,
        origin: &str,
    ) -> anyhow::Result<()> {
        ensure!(
            folder.is_dir(),
            "{name} should be installed from {folder:?} ({origin}), which is not a folder"
        );
        let manifest_path = folder.join(MAGNET_PROJECT_FILE);
        let entries = match manifest_path.is_file() {
            true => {
                let manifest = ProjectManifest::open(manifest_path)?.get_manifest().clone();
                if MagnetManifestV1::normalize_name(&manifest.package.name) != name {
                    warn!(
                        "{name} from {folder:?} ({origin}) is the project {}",
                        manifest.package.name
                    );
                }
                let kinds = manifest.artifact_kinds();
                if kinds != locked {
                    warn!(
                        "{name} from {folder:?} ({origin}) contains {}, but the locked version contains {}",
                        kind_list(&kinds),
                        kind_list(locked)
                    );
                }
                let mut entries = BTreeMap::new();
                for kind in kinds {
                    let tags = match kind {
                        ArtifactKind::Lepton => match self.target.get() {
                            Ok(target) => target.tags(),
                            Err(err) => {
                                warn!("{err:#}, so the binaries of {name} from {origin} aren't installed");
                                continue;
                            }
                        },
                        _ => ArtifactTags::any(),
                    };
                    let files = collect_files(folder, &manifest, kind, &tags, true)?;
                    let plugins = format!("{PLUGINS_DIR}/{}/{}", tags.platform, tags.abi);
                    entries.extend(package_entries(folder, &files, &plugins)?);
                }
                entries
            }
            false => folder_entries(folder)?,
        };
        info!("Install {name} from {origin}");
        self.swap_in(name, |staging| {
            for (path, source) in &entries {
                let destination = staging.join(path);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("Could not create {parent:?}"))?;
                }
                fs::copy(source, &destination)
                    .with_context(|| format!("Could not copy {source:?}"))?;
            }
            Ok(())
        })
    }

    /// `magnet_packages/<name>`, which holds every kind of artifact of the package
    fn package_dir(&self, name: &str) -> PathBuf {
        self.project_dir(ProjectDirectory::Packages)
            .join(MagnetManifestV1::normalize_name(name))
    }

    /// Extracts the archives of a package (one per kind) next to its directory and swaps it
    /// in afterwards, so a broken archive never replaces a working package
    fn install_package(&self, name: &str, archives: &[PathBuf]) -> anyhow::Result<()> {
        self.swap_in(name, |staging| {
            for archive in archives {
                compression::extract(archive, staging)?;
            }
            Ok(())
        })
    }

    /// Fills a new directory via `fill` and replaces the package directory by it afterwards
    fn swap_in(
        &self,
        name: &str,
        fill: impl FnOnce(&Path) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let target = self.package_dir(name);
        let packages = self.project_dir(ProjectDirectory::Packages);
        fs::create_dir_all(&packages)
            .with_context(|| format!("Could not create directory {packages:?}"))?;

        let staging = tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .tempdir_in(&packages)?;
        fill(staging.path())?;
        if target.exists() {
            fs::remove_dir_all(&target)
                .with_context(|| format!("Could not remove old package {target:?}"))?;
        }
        fs::rename(staging.keep(), &target)
            .with_context(|| format!("Could not move package to {target:?}"))?;
        trace!(?target, "Installed package");
        Ok(())
    }

    /// Removes the package directories within `magnet_packages` which are not locked, e.g.
    /// the `quarks`/`leptons` folders of an older magnet, as well as left over staging
    /// directories. Other hidden directories, such as `magnet_packages/.dev`, and files, such
    /// as the generated classes, are left alone.
    fn prune_packages(&self, lock_file: &LockFile) -> anyhow::Result<()> {
        let locked: HashSet<PathBuf> = lock_file
            .dependencies
            .keys()
            .map(|name| self.package_dir(name))
            .collect();
        let packages = self.project_dir(ProjectDirectory::Packages);
        if !packages.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(&packages)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let is_hidden = name.starts_with('.') && !name.starts_with(STAGING_PREFIX);
            if !entry.file_type()?.is_dir() || is_hidden || locked.contains(&path) {
                continue;
            }
            info!("Remove {path:?}, which is not locked");
            fs::remove_dir_all(&path).with_context(|| format!("Could not remove {path:?}"))?;
        }
        Ok(())
    }

    /// Packages every artifact of the project's version which can be built on this machine
    /// into `dist/`: the source, the quark and the lepton for the target of this machine, as
    /// far as the manifest has their `[artifact]` tables (see DESIGN.md "Artifact kinds").
    /// `allow_dirty` packages the source from git despite uncommitted changes.
    pub fn create_packages(&self, allow_dirty: bool) -> anyhow::Result<Vec<PackageArchive>> {
        let kinds = self.project_manifest.get_manifest().artifact_kinds();
        ensure!(
            !kinds.is_empty(),
            "{MAGNET_PROJECT_FILE} has neither [artifact.quark] nor [artifact.lepton], so the package would only consist of its source"
        );
        let mut artifacts = vec![(ArtifactKind::Source, ArtifactTags::any())];
        for kind in kinds {
            let tags = match kind {
                ArtifactKind::Lepton => self.target.get()?.tags(),
                _ => ArtifactTags::any(),
            };
            artifacts.push((kind, tags));
        }
        artifacts
            .iter()
            .map(|(kind, tags)| self.create_package(*kind, tags, allow_dirty))
            .collect()
    }

    /// Packages the artifact of `kind` with the given tags into `dist/`, see DESIGN.md
    /// "Packaging". The selected files keep their path relative to the project root, only the
    /// binaries of a lepton go into `plugins/<platform>/<abi>/`.
    #[instrument(skip(self))]
    pub fn create_package(
        &self,
        kind: ArtifactKind,
        tags: &ArtifactTags,
        allow_dirty: bool,
    ) -> anyhow::Result<PackageArchive> {
        let manifest = self.project_manifest.get_manifest();
        ensure!(
            kind == ArtifactKind::Lepton || *tags == ArtifactTags::any(),
            "A {kind} artifact has the tags {}, not {tags}",
            ArtifactTags::any()
        );
        let files = collect_files(&self.path, manifest, kind, tags, allow_dirty)?;
        let plugins = format!("{PLUGINS_DIR}/{}/{}", tags.platform, tags.abi);
        let entries = package_entries(&self.path, &files, &plugins)?;

        let dist = self.project_dir(ProjectDirectory::Dist);
        fs::create_dir_all(&dist).with_context(|| format!("Could not create {dist:?}"))?;
        let package_path = dist.join(artifact_file_name(
            &manifest.package.name,
            &SemanticVersion::try_from(manifest.package.version.clone())?,
            &kind,
            tags,
        ));
        // written next to the target and renamed, so a failure leaves no half-written package
        let temp = tempfile::NamedTempFile::new_in(&dist)?;
        compression::compress(&entries, temp.path())?;
        temp.persist(&package_path)
            .with_context(|| format!("Could not write {package_path:?}"))?;
        debug!("Written package file to {package_path:?}");

        Ok(PackageArchive {
            path: package_path,
            kind,
            tags: tags.clone(),
            files: entries.into_keys().collect(),
        })
    }
}

/// binaries of `[artifact.lepton]` within a package
const PLUGINS_DIR: &str = "plugins";

/// e.g. `quark and lepton`, or `no kind`
fn kind_list(kinds: &[ArtifactKind]) -> String {
    match kinds.is_empty() {
        true => "no kind".to_string(),
        false => kinds
            .iter()
            .map(ArtifactKind::to_string)
            .collect::<Vec<_>>()
            .join(" and "),
    }
}

/// The files of a package by their `/`-separated path within it: the selected files keep
/// their path relative to `root`, the binaries go into `plugins` (a `/`-separated folder)
fn package_entries(
    root: &Path,
    files: &PackageFiles,
    plugins: &str,
) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    let mut entries = BTreeMap::new();
    for file in &files.files {
        entries.insert(archive_path(file)?, root.join(file));
    }
    for binary in &files.binaries {
        let name = binary
            .file_name()
            .with_context(|| format!("{binary:?} has no file name"))?
            .to_string_lossy();
        let path = format!("{plugins}/{name}");
        ensure!(
            entries.insert(path.clone(), root.join(binary)).is_none(),
            "{binary:?} would be packaged as {path}, which is already part of the package"
        );
    }
    Ok(entries)
}

/// All files within `folder` by their `/`-separated path relative to it, following links
fn folder_entries(folder: &Path) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    let mut entries = BTreeMap::new();
    for entry in walkdir::WalkDir::new(folder).follow_links(true) {
        let entry = entry.with_context(|| format!("Could not read {folder:?}"))?;
        if entry.file_type().is_file() {
            let relative = entry.path().strip_prefix(folder)?;
            entries.insert(archive_path(relative)?, entry.path().to_path_buf());
        }
    }
    Ok(entries)
}

/// The `/`-separated path of a file within a package
fn archive_path(file: &Path) -> anyhow::Result<String> {
    Ok(file
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .with_context(|| format!("{file:?} is not valid UTF-8"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .join("/"))
}

/// A shell command, which receives `args` as additional arguments
fn shell_command(command: &str, args: &[String]) -> Command {
    if cfg!(windows) {
        let mut shell = Command::new("cmd");
        shell
            .arg("/C")
            .arg([command.to_string(), args.join(" ")].join(" "));
        shell
    } else {
        // the args become "$@", so the shell does not split or expand them again
        let mut shell = Command::new("sh");
        shell
            .arg("-c")
            .arg(format!("{command} \"$@\""))
            .arg("magnet")
            .args(args);
        shell
    }
}

/// The git dependencies of the given index releases, sorted, so rounds can be compared
fn index_git_dependencies(
    provider: &GitClient,
    releases: &HashMap<String, SemanticVersion>,
) -> anyhow::Result<Vec<IndexGitDependency>> {
    let mut dependencies = vec![];
    for (name, version) in releases {
        let Some(release) = provider.get_release(name, version)? else {
            continue;
        };
        for (dependency, git, ignore_quark_deps) in release.git_dependencies() {
            dependencies.push(IndexGitDependency {
                name: dependency.clone(),
                url: git.url.clone(),
                sha: git.ref_.clone(),
                ignore_quark_deps,
                required_by: format!("{name} {version}"),
            });
        }
    }
    dependencies.sort_by(|a, b| (&a.name, &a.required_by).cmp(&(&b.name, &b.required_by)));
    Ok(dependencies)
}

/// Manifest dependencies as requirements for the resolver
fn requirements(dependencies: &ManifestDependencies) -> anyhow::Result<Vec<GitDependency>> {
    dependencies
        .iter()
        .map(|(name, dependency)| {
            GitDependency::try_from(NamedDependency {
                name: name.clone(),
                dependency: dependency.clone(),
            })
        })
        .collect()
}

/// `true` if the dependency is a plain version requirement against the index
fn is_index_dependency(dependency: &MagnetManifestV1DependenciesValue) -> bool {
    match dependency {
        MagnetManifestV1DependenciesValue::String(_) => true,
        MagnetManifestV1DependenciesValue::Object {
            git,
            index,
            legacy_quark,
            path,
            ..
        } => git.is_none() && index.is_none() && path.is_none() && !legacy_quark.unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;
    use crate::models::server_options::server_options_class;

    const MANIFEST: &str = r#"# my project
magnet-version = 1

[package]
name = "my-project"
version = "0.1.0"

[dependencies]
# keep this comment
beta = "^1"
"#;

    fn fixture_index() -> GitClient {
        GitClient::open_local(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/index"),
        )
        .unwrap()
    }

    /// a project in a temporary directory with the given manifest and an empty lock file
    fn project(manifest: &str) -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(MAGNET_PROJECT_FILE), manifest).unwrap();
        LockFile::create_empty(
            dir.path().join(MAGNET_LOCK_FILE),
            fixture_index().url().clone(),
        )
        .unwrap();
        let project = Project::open(dir.path()).unwrap();
        (dir, project)
    }

    fn locked_versions(project: &Project) -> Vec<(String, String)> {
        project
            .lock_file
            .as_ref()
            .unwrap()
            .dependencies
            .iter()
            .map(|(name, dependency)| (name.clone(), dependency.version.to_string()))
            .collect()
    }

    /// writes a lock file with the given packages of the fixture index
    fn write_lock_file(dir: &tempfile::TempDir, packages: &[(&str, &str)]) {
        let index = fixture_index();
        let tree: HashMap<String, SemanticVersion> = packages
            .iter()
            .map(|(name, version)| (name.to_string(), version.parse().unwrap()))
            .collect();
        LockFile::from_tree(index.url().clone(), None, &tree, |name, version| {
            Ok(LockDependency::from(
                &index.get_release(name, version)?.unwrap(),
            ))
        })
        .unwrap()
        .write(&dir.path().join(MAGNET_LOCK_FILE))
        .unwrap();
    }

    fn read_manifest(dir: &tempfile::TempDir) -> String {
        fs::read_to_string(dir.path().join(MAGNET_PROJECT_FILE)).unwrap()
    }

    #[test]
    fn test_add_dependency() {
        let (dir, mut project) = project(MANIFEST);
        project
            .add_dependency(
                &fixture_index(),
                "alpha".into(),
                Some("^1".parse().unwrap()),
            )
            .unwrap();

        assert!(dir.path().join(SCLANG_CONF_FILE).exists());

        let manifest = read_manifest(&dir);
        assert!(manifest.starts_with("# my project\n"), "{manifest}");
        assert!(
            manifest.contains("# keep this comment\nbeta = \"^1\"\nalpha = \"^1\"\n"),
            "{manifest}"
        );

        // includes the transitive dependencies
        assert_eq!(
            locked_versions(&project),
            [
                ("alpha".to_string(), "1.1.0".to_string()),
                ("beta".to_string(), "1.2.0".to_string()),
                ("ddw-common".to_string(), "0.1.0".to_string()),
            ]
        );
        let lock = fs::read_to_string(dir.path().join(MAGNET_LOCK_FILE)).unwrap();
        let alpha = fixture_index()
            .get_release("alpha", &SemanticVersion::new(1, 1, 0))
            .unwrap()
            .unwrap();
        assert!(lock.contains(&format!(
            "checksums = {{ quark-any-any = \"{}\" }}",
            alpha.artifacts[0].sha256
        )));
    }

    #[test]
    fn test_add_respects_sclang_range() {
        let with_sclang = |sclang: &str| {
            MANIFEST.replace(
                "version = \"0.1.0\"\n",
                &format!("version = \"0.1.0\"\nsclang = \"{sclang}\"\n"),
            )
        };

        let (dir, mut supported) = project(&with_sclang(">=3.14"));
        supported
            .add_dependency(&fixture_index(), "epsilon".into(), None)
            .unwrap();
        // epsilon 1.2.0 needs sclang >=3.15
        assert!(locked_versions(&supported).contains(&("epsilon".to_string(), "1.1.0".to_string())));
        assert!(read_manifest(&dir).contains("epsilon = \"^1.1.0\""));

        let (_dir, mut invalid) = project(&with_sclang("three"));
        let err = invalid
            .add_dependency(&fixture_index(), "epsilon".into(), None)
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("not a valid version range"),
            "{err:#}"
        );
    }

    #[test]
    fn test_lock_file_has_index_http() {
        let (dir, mut announced) = project(MANIFEST);
        announced
            .add_dependency(&fixture_index(), "alpha".into(), None)
            .unwrap();
        // from the config.json of the fixture index
        assert!(read_lock(&dir)
            .contains("index-http = \"https://magnet.supercollider.online/index/\"\n"));

        let (dir, mut overridden) = project(MANIFEST);
        let index =
            fixture_index().with_http_url(Some("http://localhost:8000/index/".parse().unwrap()));
        overridden
            .add_dependency(&index, "alpha".into(), None)
            .unwrap();
        assert!(read_lock(&dir).contains("index-http = \"http://localhost:8000/index/\"\n"));
    }

    #[test]
    fn test_add_dependency_keeps_locked_versions() {
        let (dir, _) = project(MANIFEST);
        let index = fixture_index();
        write_lock_file(&dir, &[("beta", "1.0.0")]);

        let mut project = Project::open(dir.path()).unwrap();
        project
            .add_dependency(&index, "alpha".into(), None)
            .unwrap();
        // beta 1.2.0 would be newer, but beta 1.0.0 satisfies alpha as well
        assert_eq!(
            locked_versions(&project),
            [
                ("alpha".to_string(), "1.1.0".to_string()),
                ("beta".to_string(), "1.0.0".to_string()),
            ]
        );
    }

    #[test]
    fn test_add_dependency_without_version() {
        let (dir, mut project) = project(MANIFEST);
        project
            .add_dependency(&fixture_index(), "alpha".into(), None)
            .unwrap();
        assert!(read_manifest(&dir).contains("alpha = \"^1.1.0\"\n"));
    }

    #[test]
    fn test_add_existing_dependency_replaces_it() {
        let (dir, mut project) = project(&format!("{MANIFEST}DDW_Common = \"*\"\n"));
        project
            .add_dependency(
                &fixture_index(),
                "ddw-common".into(),
                Some("=0.1.0".parse().unwrap()),
            )
            .unwrap();
        let manifest = read_manifest(&dir);
        assert!(manifest.contains("DDW_Common = \"=0.1.0\"\n"));
        assert!(!manifest.contains("ddw-common"));
    }

    #[test]
    fn test_add_dependency_conflict_keeps_files() {
        let original = format!("{MANIFEST}alpha = \"^1.1\"\n");
        let (dir, mut project) = project(&original);
        let lock = fs::read_to_string(dir.path().join(MAGNET_LOCK_FILE)).unwrap();

        let err = project
            .add_dependency(
                &fixture_index(),
                "delta".into(),
                Some("=0.1.0".parse().unwrap()),
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("beta"));
        assert_eq!(read_manifest(&dir), original);
        assert_eq!(
            fs::read_to_string(dir.path().join(MAGNET_LOCK_FILE)).unwrap(),
            lock
        );
    }

    #[test]
    fn test_add_dependency_refuses_git_dependency() {
        let (_dir, mut project) = project(&format!(
            "{MANIFEST}alpha = {{ git = {{ url = \"https://github.com/foo/alpha.git\" }} }}\n"
        ));
        assert!(project
            .add_dependency(&fixture_index(), "alpha".into(), None)
            .is_err());
    }

    #[test]
    fn test_add_dependency_invalid_name() {
        let (_dir, mut project) = project(MANIFEST);
        assert!(project
            .add_dependency(&fixture_index(), "foo@bar".into(), None)
            .is_err());
    }

    /// downloads from the fixture tarballs, into a temporary cache
    fn fixture_downloader(cache: &tempfile::TempDir) -> PackageDownloader {
        let dl = Url::from_directory_path(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dl"),
        )
        .unwrap();
        PackageDownloader::new(
            cache.path().to_path_buf(),
            format!("{dl}{{name}}-{{version}}-{{kind}}-{{abi}}-{{platform}}.tar.gz"),
        )
    }

    async fn install(dir: &tempfile::TempDir) -> anyhow::Result<Project> {
        let cache = tempfile::tempdir().unwrap();
        let mut project = Project::open(dir.path()).unwrap();
        project
            .install(&fixture_index(), &fixture_downloader(&cache), false)
            .await?;
        Ok(project)
    }

    /// all files within `magnet_packages`, with their content
    fn installed_files(dir: &tempfile::TempDir) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let packages = dir.path().join("magnet_packages");
        walkdir::WalkDir::new(&packages)
            .into_iter()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    entry.path().strip_prefix(&packages).unwrap().to_path_buf(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    fn read_lock(dir: &tempfile::TempDir) -> String {
        fs::read_to_string(dir.path().join(MAGNET_LOCK_FILE)).unwrap()
    }

    #[tokio::test]
    async fn test_install_reproduces_packages() {
        let (dir, mut project) = project(MANIFEST);
        project
            .add_dependency(
                &fixture_index(),
                "alpha".into(),
                Some("^1".parse().unwrap()),
            )
            .unwrap();
        let lock = read_lock(&dir);

        install(&dir).await.unwrap();
        let installed = installed_files(&dir);
        assert_eq!(
            installed.keys().cloned().collect::<Vec<_>>(),
            [
                "MagnetServerOptions.sc",
                "alpha/Classes/Alpha.sc",
                "alpha/HelpSource/Classes/Alpha.schelp",
                "beta/Classes/Beta.sc",
                "ddw-common/DDWCommon.sc",
            ]
            .map(PathBuf::from)
        );
        let alpha = &installed[&PathBuf::from("alpha/Classes/Alpha.sc")];
        assert!(String::from_utf8_lossy(alpha).contains("1.1.0"));

        let config = fs::read_to_string(dir.path().join(SCLANG_CONF_FILE)).unwrap();
        assert!(config.contains("excludeDefaultPaths: "));
        assert!(config.contains(&format!(
            "  - '{}'\n",
            std::path::absolute(dir.path().join("magnet_packages"))
                .unwrap()
                .display()
        )));

        // a fresh clone contains no magnet_packages directory
        fs::remove_dir_all(dir.path().join("magnet_packages")).unwrap();
        install(&dir).await.unwrap();
        assert_eq!(installed_files(&dir), installed);
        assert_eq!(read_lock(&dir), lock);
    }

    #[tokio::test]
    async fn test_install_uses_lock_file() {
        let (dir, _) = project(MANIFEST);
        write_lock_file(&dir, &[("beta", "1.0.0")]);
        let lock = read_lock(&dir);

        install(&dir).await.unwrap();
        // beta 1.2.0 would be newer, but the lock covers the manifest
        let beta = &installed_files(&dir)[&PathBuf::from("beta/Classes/Beta.sc")];
        assert!(String::from_utf8_lossy(beta).contains("1.0.0"));
        assert_eq!(read_lock(&dir), lock);
    }

    #[tokio::test]
    async fn test_install_without_lock_file() {
        let (dir, _) = project(MANIFEST);
        fs::remove_file(dir.path().join(MAGNET_LOCK_FILE)).unwrap();

        let project = install(&dir).await.unwrap();
        assert_eq!(
            locked_versions(&project),
            [
                ("beta".to_string(), "1.2.0".to_string()),
                ("ddw-common".to_string(), "0.1.0".to_string()),
            ]
        );
        assert!(dir.path().join(MAGNET_LOCK_FILE).exists());
        // the two packages and MagnetServerOptions.sc
        assert_eq!(installed_files(&dir).len(), 3);
    }

    #[tokio::test]
    async fn test_install_resolves_outdated_lock_file() {
        // ddw-common was added to the manifest by hand
        let (dir, _) = project(&format!("{MANIFEST}ddw-common = \"*\"\n"));
        write_lock_file(&dir, &[("beta", "1.0.0")]);

        let project = install(&dir).await.unwrap();
        assert_eq!(
            locked_versions(&project),
            [
                ("beta".to_string(), "1.0.0".to_string()),
                ("ddw-common".to_string(), "0.1.0".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn test_install_prunes_packages() {
        let (dir, _) = project(MANIFEST);
        let packages = dir.path().join("magnet_packages");
        for path in [
            "stale/Classes/Stale.sc",
            // the folders of an older magnet
            "quarks/beta/Classes/Beta.sc",
            "leptons/stale/plugins/Stale.scx",
            ".magnet-leftover/Classes/Half.sc",
            ".hidden/keep",
            ".dev/my-lepton/plugins/Dev.scx",
        ] {
            fs::create_dir_all(packages.join(path).parent().unwrap()).unwrap();
            fs::write(packages.join(path), b"").unwrap();
        }

        install(&dir).await.unwrap();
        for removed in ["stale", "quarks", "leptons", ".magnet-leftover"] {
            assert!(!packages.join(removed).exists(), "{removed}");
        }
        assert!(packages.join(".hidden/keep").exists());
        assert!(packages.join(".dev/my-lepton/plugins/Dev.scx").exists());
        assert!(packages.join("beta/Classes/Beta.sc").exists());
        assert!(packages.join("MagnetServerOptions.sc").exists());
    }

    const ZETA: &str =
        "magnet-version = 1\n[package]\nname = \"p\"\nversion = \"0.1.0\"\n[dependencies]\n";

    /// installs for the given target, see the `zeta` lepton of the fixture index
    async fn install_for(
        dir: &tempfile::TempDir,
        target: LazyTarget,
        locked: bool,
    ) -> anyhow::Result<Project> {
        let cache = tempfile::tempdir().unwrap();
        let mut project = Project::open(dir.path()).unwrap();
        project.set_target(target);
        project
            .install(&fixture_index(), &fixture_downloader(&cache), locked)
            .await?;
        Ok(project)
    }

    fn installed_plugin(dir: &tempfile::TempDir) -> String {
        fs::read_to_string(dir.path().join("magnet_packages/zeta/plugins/Zeta.scx")).unwrap()
    }

    #[tokio::test]
    async fn test_install_lepton_binary() {
        let (dir, _) = project(&format!("{ZETA}zeta = \"=1.0.0\"\n"));
        for (platform, abi, installed) in [
            ("macos_arm64", "api3_0", "zeta 1.0.0 api3_0 macos_arm64\n"),
            // the highest minor the server loads
            ("linux_x86_64", "api4_3", "zeta 1.0.0 api4_2 linux_x86_64\n"),
            ("linux_x86_64", "api4_1", "zeta 1.0.0 api4_0 linux_x86_64\n"),
        ] {
            install_for(&dir, target(platform, abi), false)
                .await
                .unwrap();
            assert_eq!(installed_plugin(&dir), installed, "{platform} {abi}");
            assert!(dir
                .path()
                .join("magnet_packages/zeta/classes/Zeta.sc")
                .is_file());
        }
    }

    #[test]
    fn test_add_prefers_versions_with_binary() {
        // 1.1.0 only has a linux binary, 1.2.0 is yanked
        for (platform, version) in [("macos_arm64", "1.0.0"), ("linux_x86_64", "1.1.0")] {
            let (dir, mut project) = project(ZETA);
            project.set_target(target(platform, "api3_0"));
            project
                .add_dependency(&fixture_index(), "zeta".into(), None)
                .unwrap();
            assert_eq!(
                locked_versions(&project),
                [("zeta".to_string(), version.to_string())]
            );
            assert!(read_manifest(&dir).contains(&format!("zeta = \"^{version}\"")));
        }

        // without a known target, the newest version gets chosen
        let (_dir, mut project) = project(ZETA);
        project.set_target(LazyTarget::with_sclang(None, None, no_sclang));
        project
            .add_dependency(&fixture_index(), "zeta".into(), None)
            .unwrap();
        assert_eq!(
            locked_versions(&project),
            [("zeta".to_string(), "1.1.0".to_string())]
        );
    }

    #[tokio::test]
    async fn test_missing_binary_suggests_version() {
        let macos = || target("macos_arm64", "api3_0");
        // the manifest allows 1.0.0, which has a binary
        let (dir, _) = project(&format!("{ZETA}zeta = \"^1\"\n"));
        write_lock_file(&dir, &[("zeta", "1.1.0")]);
        let err = install_for(&dir, macos(), false).await.err().unwrap();
        assert_eq!(
            err.to_string(),
            "zeta 1.1.0 has no binary for macos_arm64 (api3_0). The newest version with one is 1.0.0, run `magnet update zeta` to move to it"
        );
        let mut updated = Project::open(dir.path()).unwrap();
        updated.set_target(macos());
        updated.update(&fixture_index(), &["zeta".into()]).unwrap();
        assert_eq!(
            locked_versions(&updated),
            [("zeta".to_string(), "1.0.0".to_string())]
        );

        // the manifest doesn't allow it
        let (dir, _) = project(&format!("{ZETA}zeta = \"=1.1.0\"\n"));
        let err = install_for(&dir, macos(), false).await.err().unwrap();
        assert!(
            err.to_string().ends_with(
                "The newest version with one is 1.0.0, run `magnet add zeta@=1.0.0` to use it"
            ),
            "{err}"
        );

        // no version has a binary
        let err = install_for(&dir, target("linux_x86_64", "api5_0"), false)
            .await
            .err()
            .unwrap();
        assert_eq!(
            err.to_string(),
            "zeta 1.1.0 has no binary for linux_x86_64 (api5_0), and no other version has one either. A local build can be installed via [overrides] in magnet.toml, e.g. zeta = { path = \"../zeta\" }"
        );
        assert!(!dir.path().join("magnet_packages/zeta").exists());
    }

    /// a lock file of zeta 1.0.0 which was written before its macOS binary got published
    fn lock_without_macos(dir: &tempfile::TempDir) {
        write_lock_file(dir, &[("zeta", "1.0.0")]);
        let path = dir.path().join(MAGNET_LOCK_FILE);
        let lock = fs::read_to_string(&path).unwrap();
        let key = "lepton-api3_0-macos_arm64 = \"";
        let start = lock.find(key).unwrap();
        let end = start + lock[start..].find(", ").unwrap() + 2;
        fs::write(&path, format!("{}{}", &lock[..start], &lock[end..])).unwrap();
        assert!(!read_lock(dir).contains("macos"));
    }

    #[tokio::test]
    async fn test_install_adds_late_checksum() {
        let (dir, _) = project(&format!("{ZETA}zeta = \"=1.0.0\"\n"));
        lock_without_macos(&dir);
        let before = read_lock(&dir);

        // --locked never writes the lock file
        let err = install_for(&dir, target("macos_arm64", "api3_0"), true)
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("--locked"), "{err}");
        assert_eq!(read_lock(&dir), before);

        install_for(&dir, target("macos_arm64", "api3_0"), false)
            .await
            .unwrap();
        assert_eq!(installed_plugin(&dir), "zeta 1.0.0 api3_0 macos_arm64\n");
        // only the missing checksum got added
        let (full, _) = project(ZETA);
        write_lock_file(&full, &[("zeta", "1.0.0")]);
        assert_eq!(read_lock(&dir), read_lock(&full));
    }

    #[tokio::test]
    async fn test_install_refuses_changed_checksum() {
        let (dir, _) = project(&format!("{ZETA}zeta = \"=1.0.0\"\n"));
        lock_without_macos(&dir);
        let path = dir.path().join(MAGNET_LOCK_FILE);
        let lock = fs::read_to_string(&path).unwrap();
        let key = "lepton-api4_0-linux_x86_64 = \"";
        let start = lock.find(key).unwrap() + key.len();
        fs::write(
            &path,
            format!("{}0000{}", &lock[..start], &lock[start + 4..]),
        )
        .unwrap();
        let err = install_for(&dir, target("macos_arm64", "api3_0"), false)
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("differs from the index"), "{err}");
    }

    /// a project depending on zeta 1.1.0 (only a linux binary), with `zeta` overridden by
    /// the folder `local`
    fn overridden_project(override_name: &str) -> tempfile::TempDir {
        let (dir, _) = project(&format!(
            "{ZETA}zeta = \"=1.1.0\"\n[overrides]\n{override_name} = {{ path = \"local\" }}\n"
        ));
        write_lock_file(&dir, &[("zeta", "1.1.0")]);
        dir
    }

    #[tokio::test]
    async fn test_install_override_project() {
        let dir = overridden_project("Zeta");
        fs::create_dir(dir.path().join("local")).unwrap();
        fs::write(
            dir.path().join("local/magnet.toml"),
            "magnet-version = 1\n[package]\nname = \"zeta\"\nversion = \"1.1.0\"\n[artifact.quark]\npaths = [\"classes/*.sc\"]\n[artifact.lepton.platform-paths]\nmacos_arm64 = [\"build/*.scx\"]\n",
        )
        .unwrap();
        write_files(
            &dir,
            &[
                "local/classes/Zeta.sc",
                "local/build/Zeta.scx",
                "local/build/Zeta.o",
            ],
        );
        let lock = read_lock(&dir);

        // no version of zeta has a binary for macOS
        let macos = || target("macos_arm64", "api3_0");
        install_for(&dir, macos(), false).await.unwrap();
        let zeta = dir.path().join("magnet_packages/zeta");
        let plugin = zeta.join("plugins/macos_arm64/api3_0/Zeta.scx");
        assert_eq!(fs::read_to_string(&plugin).unwrap(), "local/build/Zeta.scx");
        let files = || {
            let mut files: Vec<_> = walkdir::WalkDir::new(&zeta)
                .into_iter()
                .map(|entry| entry.unwrap())
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| archive_path(entry.path().strip_prefix(&zeta).unwrap()).unwrap())
                .collect();
            files.sort();
            files
        };
        assert_eq!(
            files(),
            [
                "classes/Zeta.sc",
                "magnet.toml",
                "plugins/macos_arm64/api3_0/Zeta.scx"
            ]
        );
        // the lock file keeps the index entry
        assert_eq!(read_lock(&dir), lock);

        // every install copies the current files
        fs::write(dir.path().join("local/build/Zeta.scx"), "rebuilt").unwrap();
        install_for(&dir, macos(), true).await.unwrap();
        assert_eq!(fs::read_to_string(&plugin).unwrap(), "rebuilt");

        // without the abi of this machine, only the classes get installed
        let no_abi = LazyTarget::with_sclang(Some("macos_arm64".into()), None, no_sclang);
        install_for(&dir, no_abi, false).await.unwrap();
        assert_eq!(files(), ["classes/Zeta.sc", "magnet.toml"]);
    }

    #[tokio::test]
    async fn test_install_override_folder() {
        let dir = overridden_project("zeta");
        write_files(
            &dir,
            &[
                "local/Zeta.sc",
                "local/plugins/Zeta.scx",
                "local/HelpSource/Zeta.schelp",
            ],
        );
        install_for(&dir, target("macos_arm64", "api3_0"), false)
            .await
            .unwrap();
        let zeta = dir.path().join("magnet_packages/zeta");
        for file in ["Zeta.sc", "plugins/Zeta.scx", "HelpSource/Zeta.schelp"] {
            assert!(zeta.join(file).is_file(), "{file}");
        }
    }

    #[tokio::test]
    async fn test_install_override_errors() {
        // the folder is missing
        let dir = overridden_project("zeta");
        let err = install_for(&dir, target("macos_arm64", "api3_0"), false)
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("not a folder"), "{err}");

        // other kinds than the locked version only warn, the override decides what's installed
        write_files(&dir, &["local/Zeta.sc"]);
        fs::write(
            dir.path().join("local/magnet.toml"),
            "magnet-version = 1\n[package]\nname = \"zeta\"\nversion = \"1.1.0\"\n[artifact.quark]\npaths = [\"*.sc\"]\n",
        )
        .unwrap();
        install_for(&dir, target("macos_arm64", "api3_0"), false)
            .await
            .unwrap();
        assert!(dir.path().join("magnet_packages/zeta/Zeta.sc").is_file());
    }

    #[tokio::test]
    async fn test_override_of_other_package_is_ignored() {
        let (dir, _) = project(&format!(
            "{ZETA}zeta = \"=1.0.0\"\n[overrides]\nunknown = {{ path = \"local\" }}\n"
        ));
        install_for(&dir, target("macos_arm64", "api3_0"), false)
            .await
            .unwrap();
        assert_eq!(installed_plugin(&dir), "zeta 1.0.0 api3_0 macos_arm64\n");
        assert!(!dir.path().join("magnet_packages/unknown").exists());
    }

    #[tokio::test]
    async fn test_install_writes_server_options() {
        let class = |dir: &tempfile::TempDir| {
            fs::read_to_string(dir.path().join("magnet_packages/MagnetServerOptions.sc")).unwrap()
        };
        let (dir, _) = project(&format!("{ZETA}zeta = \"=1.0.0\"\n"));
        install_for(&dir, target("macos_arm64", "api3_0"), false)
            .await
            .unwrap();
        assert_eq!(class(&dir), server_options_class(false));

        // a lepton project also loads its own build
        let (dir, _) = project(&format!(
            "{LEPTON_MANIFEST}[dependencies]\nzeta = \"=1.0.0\"\n"
        ));
        write_files(&dir, &["Classes/Foo.sc", "build/Release/Foo.scx"]);
        install_for(&dir, target("macos_arm64", "api3_0"), false)
            .await
            .unwrap();
        assert_eq!(class(&dir), server_options_class(true));
        assert_eq!(
            fs::read_to_string(dir.path().join("magnet_packages/.dev/foo/plugins/Foo.scx"))
                .unwrap(),
            "build/Release/Foo.scx"
        );
        assert_eq!(installed_plugin(&dir), "zeta 1.0.0 api3_0 macos_arm64\n");
    }

    #[test]
    fn test_run_copies_dev_plugins() {
        let (dir, mut project) = project(&format!(
            "{LEPTON_MANIFEST}[[entrypoint]]\nname = \"noop\"\nkind = \"system\"\ncommand = \"exit 0\"\n"
        ));
        project.set_target(target("macos_arm64", "api3_0"));
        let dev = dir.path().join("magnet_packages/.dev/foo/plugins");
        let run = |project: &Project| {
            assert!(project
                .run_entrypoint("noop", &[], no_sclang)
                .unwrap()
                .success())
        };

        // not built yet, which only warns
        run(&project);
        assert!(!dev.join("Foo.scx").exists());

        write_files(&dir, &["build/Release/Foo.scx"]);
        run(&project);
        assert_eq!(
            fs::read_to_string(dev.join("Foo.scx")).unwrap(),
            "build/Release/Foo.scx"
        );

        // the copy follows the build, also for removed binaries
        fs::remove_file(dir.path().join("build/Release/Foo.scx")).unwrap();
        write_files(&dir, &["build/Release/Bar.scx"]);
        run(&project);
        assert!(!dev.join("Foo.scx").exists());
        assert!(dev.join("Bar.scx").is_file());
    }

    #[tokio::test]
    async fn test_install_lepton_needs_target() {
        let (dir, _) = project(&format!("{ZETA}zeta = \"=1.0.0\"\n"));
        let err = install_for(&dir, LazyTarget::with_sclang(None, None, no_sclang), false)
            .await
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("--abi"), "{err:#}");
    }

    #[tokio::test]
    async fn test_install_locked_needs_lock() {
        // the lock file is empty
        let (dir, _) = project(&format!("{ZETA}zeta = \"=1.0.0\"\n"));
        let before = read_lock(&dir);
        let err = install_for(&dir, target("macos_arm64", "api3_0"), true)
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("--locked"), "{err}");
        assert_eq!(read_lock(&dir), before);
    }

    /// a manifest without dependencies, with the given entrypoints
    fn manifest_with_entrypoints(entrypoints: &str) -> String {
        format!(
            "magnet-version = 1\n\n[package]\nname = \"runner\"\nversion = \"0.1.0\"\n\n{entrypoints}"
        )
    }

    /// a target with fixed tags, so tests don't depend on the machine
    fn target(platform: &str, abi: &str) -> LazyTarget {
        LazyTarget::with_sclang(Some(platform.into()), Some(abi.into()), no_sclang)
    }

    fn no_sclang() -> anyhow::Result<Option<Sclang>> {
        Ok(None)
    }

    #[cfg(unix)]
    #[test]
    fn test_run_system_entrypoint() {
        let (dir, project) = project(&manifest_with_entrypoints(
            "[[entrypoint]]\nname = \"hello\"\nkind = \"system\"\ncommand = \"printf '%s,' hello > out.txt\"\n",
        ));
        let status = project
            .run_entrypoint("hello", &["a b".to_string(), "-x".to_string()], no_sclang)
            .unwrap();
        assert!(status.success());
        // runs in the project root, and the args are passed on without being split
        assert_eq!(
            fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "hello,a b,-x,"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_run_returns_exit_code() {
        let (_dir, project) = project(&manifest_with_entrypoints(
            "[[entrypoint]]\nname = \"fail\"\nkind = \"system\"\ncommand = \"exit 3\"\n",
        ));
        let status = project.run_entrypoint("fail", &[], no_sclang).unwrap();
        assert_eq!(status.code(), Some(3));
    }

    #[test]
    fn test_run_unknown_entrypoint() {
        let (_dir, project) = project(&manifest_with_entrypoints(
            "[[entrypoint]]\nname = \"hello\"\ncommand = \"hello.scd\"\n\n[[entrypoint]]\nname = \"test\"\ncommand = \"test.scd\"\n",
        ));
        let err = project
            .run_entrypoint("missing", &[], no_sclang)
            .unwrap_err();
        assert!(err.to_string().contains("hello, test"), "{err:#}");
    }

    #[test]
    fn test_run_requires_installed_dependencies() {
        let (_dir, project) = project(&format!(
            "{MANIFEST}\n[[entrypoint]]\nname = \"hello\"\nkind = \"system\"\ncommand = \"true\"\n"
        ));
        let err = project.run_entrypoint("hello", &[], no_sclang).unwrap_err();
        assert!(err.to_string().contains("magnet install"), "{err:#}");
    }

    #[test]
    fn test_run_sclang_entrypoint_without_sclang() {
        let (dir, project) = project(&manifest_with_entrypoints(
            "[[entrypoint]]\nname = \"hello\"\ncommand = \"hello.scd\"\n",
        ));
        fs::write(dir.path().join("hello.scd"), "").unwrap();
        let err = project.run_entrypoint("hello", &[], no_sclang).unwrap_err();
        assert!(err.to_string().contains(MAGNET_SCLANG_ENV), "{err:#}");
    }

    /// an sclang installation whose binary records its working directory and arguments
    #[cfg(unix)]
    fn fake_sclang(root: &Path) -> Sclang {
        use std::os::unix::fs::PermissionsExt;
        let binary = root.join("bin/sclang");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(
            &binary,
            "#!/bin/sh\npwd -P > sclang-call.txt\nfor arg in \"$@\"; do echo \"$arg\" >> sclang-call.txt; done\n",
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let marker = root.join("share/SuperCollider/SCClassLibrary/Common/Core/Object.sc");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(marker, "Object {}").unwrap();
        Sclang::from_binary(&binary).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn test_run_sclang_entrypoint() {
        // the kind defaults to sclang
        let (dir, project) = project(&manifest_with_entrypoints(
            "[[entrypoint]]\nname = \"hello\"\ncommand = \"hello.scd\"\n",
        ));
        fs::write(dir.path().join("hello.scd"), "").unwrap();
        let installation = tempfile::tempdir().unwrap();
        let sclang = fake_sclang(installation.path());

        let status = project
            .run_entrypoint("hello", &["fast".to_string()], || Ok(Some(sclang.clone())))
            .unwrap();
        assert!(status.success());

        let root = std::path::absolute(dir.path()).unwrap();
        let call = fs::read_to_string(dir.path().join("sclang-call.txt")).unwrap();
        assert_eq!(
            call,
            format!(
                "{}\n-l\n{}\n--\nhello.scd\nfast\n",
                dir.path().canonicalize().unwrap().display(),
                root.join(SCLANG_CONF_FILE).display()
            )
        );
        // the config got written for exactly this sclang
        let config = fs::read_to_string(dir.path().join(SCLANG_CONF_FILE)).unwrap();
        assert!(config.contains("excludeDefaultPaths: true"), "{config}");
        assert!(config.contains(&sclang.class_library.display().to_string()));
    }

    #[test]
    fn test_run_missing_script() {
        let (_dir, project) = project(&manifest_with_entrypoints(
            "[[entrypoint]]\nname = \"hello\"\ncommand = \"hello.scd\"\n",
        ));
        let sclang = Sclang {
            binary: PathBuf::from("/bin/sclang"),
            class_library: PathBuf::from("/share/SCClassLibrary"),
        };
        let err = project
            .run_entrypoint("hello", &[], || Ok(Some(sclang)))
            .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err:#}");
    }

    fn installed_packages(dir: &tempfile::TempDir) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.path().join("magnet_packages"))
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().unwrap().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| !name.starts_with('.'))
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn test_remove_dependency() {
        let (dir, mut project) = project(&manifest_with_entrypoints(
            "# my dependencies\n[dependencies]\n",
        ));
        project
            .add_dependency(&fixture_index(), "alpha".into(), None)
            .unwrap();
        let mut project = install(&dir).await.unwrap();
        assert_eq!(installed_packages(&dir), ["alpha", "beta", "ddw-common"]);

        project
            .remove_dependency(&fixture_index(), "alpha")
            .unwrap();

        // the transitive dependencies are gone as well
        assert!(locked_versions(&project).is_empty());
        assert!(installed_packages(&dir).is_empty());
        let manifest = read_manifest(&dir);
        assert!(
            manifest.contains("# my dependencies\n[dependencies]\n"),
            "{manifest}"
        );
        assert!(!manifest.contains("alpha"), "{manifest}");
        let config = fs::read_to_string(dir.path().join(SCLANG_CONF_FILE)).unwrap();
        assert!(config.contains("magnet_packages"), "{config}");
    }

    #[tokio::test]
    async fn test_remove_dependency_keeps_others() {
        let (dir, mut project) = project(MANIFEST);
        project
            .add_dependency(&fixture_index(), "alpha".into(), None)
            .unwrap();
        let mut project = install(&dir).await.unwrap();

        // any spelling of the name works
        project
            .remove_dependency(&fixture_index(), "ALPHA")
            .unwrap();

        assert_eq!(
            locked_versions(&project),
            [
                ("beta".to_string(), "1.2.0".to_string()),
                ("ddw-common".to_string(), "0.1.0".to_string()),
            ]
        );
        assert_eq!(installed_packages(&dir), ["beta", "ddw-common"]);
        let manifest = read_manifest(&dir);
        assert!(
            manifest.contains("# keep this comment\nbeta = \"^1\"\n"),
            "{manifest}"
        );
        assert!(!manifest.contains("alpha"), "{manifest}");
    }

    #[test]
    fn test_remove_unknown_dependency() {
        let (dir, mut project) = project(MANIFEST);
        let lock = read_lock(&dir);
        let err = project
            .remove_dependency(&fixture_index(), "alpha")
            .unwrap_err();
        assert!(err.to_string().contains("not a dependency"), "{err:#}");
        assert_eq!(read_manifest(&dir), MANIFEST);
        assert_eq!(read_lock(&dir), lock);
    }

    #[test]
    fn test_create_project() {
        use crate::models::gitignore::missing_gitignore_entries;
        let dir = tempfile::tempdir().unwrap();
        // e.g. magnet init within an existing repository
        fs::write(dir.path().join(".gitignore"), "*.bak\n").unwrap();
        let manifest = MagnetManifestV1::new(
            "my-project".to_string(),
            SemanticVersion::new(0, 1, 0),
            None,
            Default::default(),
        )
        .unwrap();
        let missing = missing_gitignore_entries(dir.path()).unwrap();
        assert_eq!(
            missing,
            ["/dist/", "/magnet_packages/", "/sclang_conf.yaml"]
        );
        Project::create(dir.path(), manifest.clone(), CreateOptions::default()).unwrap();
        assert!(missing_gitignore_entries(dir.path()).unwrap().is_empty());

        assert!(dir.path().join(MAGNET_PROJECT_FILE).is_file());
        for folder in ["classes", "HelpSource", "tests"] {
            assert!(dir.path().join(folder).is_dir(), "{folder}");
        }
        // the lock file gets created by the first add or install
        assert!(!dir.path().join(MAGNET_LOCK_FILE).exists());
        let gitignore = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gitignore.starts_with("*.bak\n"), "{gitignore}");
        for entry in ["/dist/", "/magnet_packages/", "/sclang_conf.yaml"] {
            assert!(gitignore.lines().any(|line| line == entry), "{gitignore}");
        }

        // without the scaffold and the .gitignore, only the manifest and magnet_packages/
        let dir = tempfile::tempdir().unwrap();
        let options = CreateOptions {
            directories: false,
            gitignore: false,
            ..Default::default()
        };
        Project::create(dir.path(), manifest, options).unwrap();
        assert!(dir.path().join(MAGNET_PROJECT_FILE).is_file());
        for folder in ["classes", "HelpSource", "tests", ".gitignore"] {
            assert!(!dir.path().join(folder).exists(), "{folder}");
        }
    }

    /// An imported legacy quark packages all its files, wherever they are
    #[test]
    fn test_legacy_quark_layout_packages() {
        let dir = tempfile::tempdir().unwrap();
        write_files(
            &dir,
            &[
                "Foo.quark",
                "Classes/Foo.sc",
                "HelpSource/Classes/Foo.schelp",
                "sounds/a.wav",
                ".git/config",
            ],
        );
        let manifest = MagnetManifestV1::new(
            "foo".to_string(),
            SemanticVersion::new(1, 0, 0),
            None,
            Default::default(),
        )
        .unwrap();
        let options = CreateOptions {
            directories: false,
            gitignore: true,
            layout: ProjectLayout::LegacyQuark,
        };
        let project = Project::create(dir.path(), manifest, options).unwrap();
        let archive = project
            .create_package(ArtifactKind::Quark, &ArtifactTags::any(), false)
            .unwrap();
        assert_eq!(
            archive.files,
            [
                "Classes/Foo.sc",
                "Foo.quark",
                "HelpSource/Classes/Foo.schelp",
                "magnet.toml",
                "sounds/a.wav",
            ]
        );
    }
    /// `magnet init` writes an `[artifact]` template which `magnet package` accepts once the
    /// project has its files, also after uncommenting its `[artifact.lepton]`
    #[test]
    fn test_init_template_packages() {
        let platform = crate::target::current_platform().unwrap();
        let libraries = match platform.to_string().split('_').next().unwrap() {
            "macos" => "dylib",
            "windows" => "dll",
            _ => "so",
        };
        for lepton in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let manifest = MagnetManifestV1::new(
                "foo".to_string(),
                SemanticVersion::new(0, 1, 0),
                None,
                Default::default(),
            )
            .unwrap();
            let mut project =
                Project::create(dir.path(), manifest, CreateOptions::default()).unwrap();
            let library = format!("build/libdep.{libraries}");
            write_files(
                &dir,
                &[
                    "classes/Foo.sc",
                    "HelpSource/Classes/Foo.schelp",
                    "src/Foo.cpp",
                    "CMakeLists.txt",
                    "build/Foo.scx",
                    &library,
                ],
            );
            let classes = [
                "HelpSource/Classes/Foo.schelp".to_string(),
                "classes/Foo.sc".to_string(),
                "magnet.toml".to_string(),
            ];
            if lepton {
                let manifest_path = dir.path().join(MAGNET_PROJECT_FILE);
                let content = fs::read_to_string(&manifest_path).unwrap();
                let uncommented = content.replace(
                    "# [artifact.lepton.platform-paths]\n# ",
                    "[artifact.lepton.platform-paths]\n",
                );
                assert_ne!(content, uncommented, "{content}");
                fs::write(&manifest_path, uncommented).unwrap();
                project = Project::open(dir.path()).unwrap();
                project.set_target(target(&platform.to_string(), "api3_0"));
            }
            // the source takes the files committed to git
            crate::git_client::source::tests::commit_files(dir.path(), &[]);
            let archives = project.create_packages(false).unwrap();
            let kinds: Vec<_> = archives.iter().map(|archive| archive.kind).collect();
            let source = &archives[0].files;
            assert!(source.contains(&"src/Foo.cpp".to_string()), "{source:?}");
            assert!(source.contains(&"CMakeLists.txt".to_string()), "{source:?}");
            assert_eq!(archives[1].files, classes);
            if !lepton {
                assert_eq!(kinds, [ArtifactKind::Source, ArtifactKind::Quark]);
                continue;
            }
            assert_eq!(
                kinds,
                [
                    ArtifactKind::Source,
                    ArtifactKind::Quark,
                    ArtifactKind::Lepton
                ]
            );
            let plugins = format!("plugins/{platform}/api3_0");
            assert_eq!(
                archives[2].files,
                [
                    "magnet.toml".to_string(),
                    format!("{plugins}/Foo.scx"),
                    format!("{plugins}/libdep.{libraries}"),
                ]
            );
        }
    }

    /// the paths within a `.tar.gz` archive, in their order
    fn archive_paths(archive: &Path) -> Vec<String> {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(
            fs::File::open(archive).unwrap(),
        ));
        archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().to_string_lossy().to_string())
            .collect()
    }

    fn write_files(dir: &tempfile::TempDir, files: &[&str]) {
        for file in files {
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file).unwrap();
        }
    }

    #[test]
    fn test_create_quark_package() {
        let (dir, mut project) = project(
            "magnet-version = 1\n[package]\nname = \"My_Quark\"\nversion = \"0.2.0\"\n[artifact.quark]\npaths = [\"Classes/**.sc\", \"HelpSource/**/*\"]\n",
        );
        write_files(
            &dir,
            &[
                "Classes/Foo.sc",
                "Classes/Sub/Bar.sc",
                "HelpSource/Classes/Foo.schelp",
                "README.md",
                "test.scd",
            ],
        );

        crate::git_client::source::tests::commit_files(dir.path(), &[]);
        let package = project
            .create_package(ArtifactKind::Quark, &ArtifactTags::any(), false)
            .unwrap()
            .path;
        assert_eq!(
            package,
            dir.path().join("dist/my-quark-0.2.0-quark-any-any.tar.gz")
        );
        assert_eq!(
            archive_paths(&package),
            [
                "Classes/Foo.sc",
                "Classes/Sub/Bar.sc",
                "HelpSource/Classes/Foo.schelp",
                "README.md",
                "magnet.toml"
            ]
        );

        // packaging again (now with dist/ present) gives the same bytes
        let first = fs::read(&package).unwrap();
        project
            .create_package(ArtifactKind::Quark, &ArtifactTags::any(), false)
            .unwrap();
        assert_eq!(fs::read(&package).unwrap(), first);

        // only a lepton has other tags
        let tags = ArtifactTags::new("api3_0", "macos_arm64").unwrap();
        assert!(project
            .create_package(ArtifactKind::Quark, &tags, false)
            .is_err());
        assert!(project
            .create_package(ArtifactKind::Lepton, &tags, false)
            .is_err());

        // the reported files are the contents of the archive
        // a quark doesn't need the target, which is unknown without sclang
        project.set_target(LazyTarget::with_sclang(None, None, no_sclang));
        let archives = project.create_packages(false).unwrap();
        assert_eq!(archives.len(), 2);
        assert_eq!(archives[0].kind, ArtifactKind::Source);
        assert!(archives[0]
            .path
            .ends_with("my-quark-0.2.0-source-any-any.tar.gz"));
        // every committed file, unlike the quark
        assert!(archives[0].files.contains(&"test.scd".to_string()));
        assert_eq!(archives[1].kind, ArtifactKind::Quark);
        assert_eq!(archives[1].tags, ArtifactTags::any());
        assert_eq!(archives[1].files, archive_paths(&package));
    }

    const LEPTON_MANIFEST: &str = "magnet-version = 1\n[package]\nname = \"foo\"\nversion = \"1.0.0\"\n[artifact.source]\nfrom = \"paths\"\npaths = [\"Classes/**\", \"src/**\"]\n[artifact.quark]\npaths = [\"Classes/**.sc\"]\n[artifact.lepton]\nplatform-paths = { macos_arm64 = [\"build/**/*.scx\"] }\n";

    #[test]
    fn test_create_lepton_packages() {
        let (dir, mut project) = project(LEPTON_MANIFEST);
        write_files(
            &dir,
            &[
                "Classes/Foo.sc",
                "src/Foo.cpp",
                "build/Release/Foo.scx",
                "build/Foo.o",
            ],
        );

        let tags = ArtifactTags::new("api3_0", "macos_arm64").unwrap();
        let package = project
            .create_package(ArtifactKind::Lepton, &tags, false)
            .unwrap()
            .path;
        assert!(package.ends_with("foo-1.0.0-lepton-api3_0-macos_arm64.tar.gz"));
        assert_eq!(
            archive_paths(&package),
            ["magnet.toml", "plugins/macos_arm64/api3_0/Foo.scx"]
        );

        let source = project
            .create_package(ArtifactKind::Source, &ArtifactTags::any(), false)
            .unwrap()
            .path;
        assert!(source.ends_with("foo-1.0.0-source-any-any.tar.gz"));
        project.set_target(target("macos_arm64", "api3_0"));
        let archives = project.create_packages(false).unwrap();
        let paths: Vec<_> = archives.iter().map(|archive| &archive.path).collect();
        let quark = dir.path().join("dist/foo-1.0.0-quark-any-any.tar.gz");
        assert_eq!(paths, [&source, &quark, &package]);
        assert_eq!(
            archive_paths(&source),
            ["Classes/Foo.sc", "magnet.toml", "src/Foo.cpp"]
        );
        assert_eq!(archive_paths(&quark), ["Classes/Foo.sc", "magnet.toml"]);

        // packaging again gives the same bytes for every artifact
        let first: Vec<_> = paths.iter().map(|path| fs::read(path).unwrap()).collect();
        project.create_packages(false).unwrap();
        let again: Vec<_> = paths.iter().map(|path| fs::read(path).unwrap()).collect();
        assert_eq!(first, again);
    }

    #[test]
    fn test_binaries_with_the_same_name() {
        let (dir, project) = project(LEPTON_MANIFEST);
        write_files(
            &dir,
            &["Classes/Foo.sc", "build/a/Foo.scx", "build/b/Foo.scx"],
        );
        let tags = ArtifactTags::new("api3_0", "macos_arm64").unwrap();
        let err = project
            .create_package(ArtifactKind::Lepton, &tags, false)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("plugins/macos_arm64/api3_0/Foo.scx"),
            "{err}"
        );
    }
    const ALPHA_BETA: &str = "magnet-version = 1\n[package]\nname = \"p\"\nversion = \"0.1.0\"\n[dependencies]\nalpha = \"^1\"\nbeta = \"^1\"\n";

    /// The changes of `update` as `(name, old version, new version)`
    fn changed_versions(
        changes: &[(String, Option<LockDependency>, Option<LockDependency>)],
    ) -> Vec<(String, Option<SemanticVersion>, Option<SemanticVersion>)> {
        changes
            .iter()
            .map(|(name, old, new)| {
                let version = |dependency: &Option<LockDependency>| {
                    dependency.as_ref().map(|dependency| dependency.version)
                };
                (name.clone(), version(old), version(new))
            })
            .collect()
    }

    fn version(version: &str) -> Option<SemanticVersion> {
        Some(version.parse().unwrap())
    }

    #[tokio::test]
    async fn test_install_keeps_yanked_locked_version() {
        let (dir, _) = project("magnet-version = 1\n[package]\nname = \"p\"\nversion = \"0.1.0\"\n[dependencies]\nalpha = \"*\"\n");
        // alpha 2.0.0 is yanked
        write_lock_file(
            &dir,
            &[
                ("alpha", "2.0.0"),
                ("beta", "1.2.0"),
                ("ddw-common", "0.1.0"),
            ],
        );
        install(&dir).await.unwrap();
        assert_eq!(installed_packages(&dir), ["alpha", "beta", "ddw-common"]);
        let alpha =
            fs::read_to_string(dir.path().join("magnet_packages/alpha/Classes/Alpha.sc")).unwrap();
        assert!(alpha.contains("2.0.0"), "{alpha}");
    }

    #[tokio::test]
    async fn test_install_refuses_deleted_locked_version() {
        let (dir, _) = project(MANIFEST);
        write_lock_file(&dir, &[("beta", "1.2.0"), ("ddw-common", "0.1.0")]);
        // ddw-common 0.9.0 is not in the index
        let lock = read_lock(&dir).replace("version = \"0.1.0\"", "version = \"0.9.0\"");
        fs::write(dir.path().join(MAGNET_LOCK_FILE), lock).unwrap();

        let err = install(&dir).await.err().unwrap().to_string();
        assert!(err.contains("ddw-common 0.9.0"), "{err}");
        assert!(err.contains("magnet update ddw-common"), "{err}");
        assert!(!dir.path().join("magnet_packages").exists());

        // updating resolves it again
        let mut project = Project::open(dir.path()).unwrap();
        let changes = project
            .update(&fixture_index(), &["DDW_Common".to_string()])
            .unwrap();
        assert_eq!(
            changed_versions(&changes),
            [("ddw-common".to_string(), version("0.9.0"), version("0.1.0"))]
        );
    }

    #[test]
    fn test_update_single_package() {
        let (dir, _) = project(ALPHA_BETA);
        write_lock_file(&dir, &[("alpha", "1.0.0"), ("beta", "1.0.0")]);
        let mut project = Project::open(dir.path()).unwrap();

        let changes = project
            .update(&fixture_index(), &["Beta".to_string()])
            .unwrap();
        // alpha 1.1.0 would be newer, but only beta (and its new dependency) changes
        assert_eq!(
            changed_versions(&changes),
            [
                ("beta".to_string(), version("1.0.0"), version("1.2.0")),
                ("ddw-common".to_string(), None, version("0.1.0")),
            ]
        );
        assert_eq!(
            locked_versions(&project),
            [
                ("alpha".to_string(), "1.0.0".to_string()),
                ("beta".to_string(), "1.2.0".to_string()),
                ("ddw-common".to_string(), "0.1.0".to_string()),
            ]
        );
        assert!(project
            .update(&fixture_index(), &["beta".to_string()])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_update_all_packages() {
        let (dir, _) = project(ALPHA_BETA);
        write_lock_file(&dir, &[("alpha", "1.0.0"), ("beta", "1.0.0")]);
        let mut project = Project::open(dir.path()).unwrap();

        project.update(&fixture_index(), &[]).unwrap();
        // alpha 2.0.0 is yanked and out of range
        assert_eq!(
            locked_versions(&project),
            [
                ("alpha".to_string(), "1.1.0".to_string()),
                ("beta".to_string(), "1.2.0".to_string()),
                ("ddw-common".to_string(), "0.1.0".to_string()),
            ]
        );
    }

    #[test]
    fn test_update_unknown_package() {
        let (dir, _) = project(ALPHA_BETA);
        write_lock_file(&dir, &[("alpha", "1.0.0"), ("beta", "1.0.0")]);
        let mut project = Project::open(dir.path()).unwrap();
        let err = project
            .update(
                &fixture_index(),
                &["gamma".to_string(), "delta".to_string()],
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("delta, gamma"), "{err}");
    }
    #[tokio::test]
    async fn test_install_locked_after_add() {
        let (dir, mut project) = project(MANIFEST);
        let index = fixture_index();
        project
            .add_dependency(&index, "alpha".into(), Some("^1".parse().unwrap()))
            .unwrap();
        let cache = tempfile::tempdir().unwrap();
        project
            .install_locked(&index, &fixture_downloader(&cache), false)
            .await
            .unwrap();
        assert_eq!(installed_packages(&dir), ["alpha", "beta", "ddw-common"]);
    }

    /// a project at `<root>/project` with the given dependencies, whose git clones go into
    /// `<root>/git-cache`
    fn sources_project(root: &Path, dependencies: &str) -> Project {
        let path = root.join("project");
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join(MAGNET_PROJECT_FILE),
            format!("magnet-version = 1\n[package]\nname = \"p\"\nversion = \"0.1.0\"\n[dependencies]\n{dependencies}"),
        )
        .unwrap();
        open_sources_project(root)
    }

    fn open_sources_project(root: &Path) -> Project {
        let mut project = Project::open(&root.join("project")).unwrap();
        project.set_git_cache(root.join("git-cache"));
        project
    }

    async fn install_sources(project: &mut Project, locked: bool) -> anyhow::Result<()> {
        let cache = tempfile::tempdir().unwrap();
        project
            .install(&fixture_index(), &fixture_downloader(&cache), locked)
            .await
    }

    fn quark_manifest(name: &str, version: &str, dependencies: &str) -> String {
        format!("magnet-version = 1\n[package]\nname = \"{name}\"\nversion = \"{version}\"\n[dependencies]\n{dependencies}\n[artifact.quark]\npaths = [\"Classes/**.sc\"]")
    }

    fn file_url(path: &Path) -> Url {
        Url::from_directory_path(path).unwrap()
    }

    /// `magnet add foo --legacy-quark`: the pinned legacy quark gets into the manifest and the
    /// lock, and adding it again replaces the entry (keeping its spelling)
    #[test]
    fn test_add_source_dependency() {
        use crate::git_client::source::tests::commit_files;
        use crate::models::project_manifest::MagnetManifestV1DependenciesValueObjectGit;
        let root = tempfile::tempdir().unwrap();
        let ddw = root.path().join("ddw");
        let first = commit_files(&ddw, &[("DDW.sc", "DDW {}"), ("DDW.quark", "()")]);
        let second = commit_files(&ddw, &[("DDW.sc", "DDW { new {} }")]);
        let mut project = sources_project(root.path(), "alpha = \"^1\"\n");
        let pinned = |sha: &str| MagnetManifestV1DependenciesValue::Object {
            git: Some(MagnetManifestV1DependenciesValueObjectGit {
                url: Some(file_url(&ddw).to_string()),
                branch: None,
                ref_: Some(sha.to_string()),
                tag: None,
            }),
            ignore_quark_deps: None,
            index: None,
            legacy_quark: Some(true),
            path: None,
            version: None,
        };

        project
            .add_source_dependency(&fixture_index(), "ddwCommon".into(), pinned(&first))
            .unwrap();
        let manifest =
            fs::read_to_string(root.path().join("project").join(MAGNET_PROJECT_FILE)).unwrap();
        assert!(
            manifest.contains(&format!(
                "ddwCommon = {{ legacy-quark = true, git = {{ ref = \"{first}\""
            )),
            "{manifest}"
        );
        let lock = fs::read_to_string(root.path().join("project").join(MAGNET_LOCK_FILE)).unwrap();
        assert!(lock.contains(&format!("?ref={first}#{first}")), "{lock}");
        assert!(lock.contains("[dependencies.alpha]"), "{lock}");

        project
            .add_source_dependency(&fixture_index(), "ddwcommon".into(), pinned(&second))
            .unwrap();
        let manifest =
            fs::read_to_string(root.path().join("project").join(MAGNET_PROJECT_FILE)).unwrap();
        assert!(
            manifest.contains(&format!(
                "ddwCommon = {{ legacy-quark = true, git = {{ ref = \"{second}\""
            )),
            "{manifest}"
        );
        assert!(!manifest.contains("ddwcommon"), "{manifest}");

        // nothing gets written if it can't be resolved
        let err = project
            .add_source_dependency(
                &fixture_index(),
                "broken".into(),
                pinned("0000000000000000000000000000000000000000"),
            )
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("Failed to add dependency broken"),
            "{err:#}"
        );
        let after =
            fs::read_to_string(root.path().join("project").join(MAGNET_PROJECT_FILE)).unwrap();
        assert_eq!(after, manifest);
    }

    #[tokio::test]
    async fn test_install_git_dependencies() {
        use crate::git_client::source::tests::{commit_files, tag};
        let root = tempfile::tempdir().unwrap();
        let foo = root.path().join("foo");
        let foo_sha = commit_files(
            &foo,
            &[
                (
                    "magnet.toml",
                    &quark_manifest("foo", "0.2.0", "alpha = \"^1\""),
                ),
                ("Classes/Foo.sc", "Foo {}"),
                ("notes.txt", "not selected"),
            ],
        );
        tag(&foo, "v0.2.0");
        let bar = root.path().join("bar");
        let bar_sha = commit_files(&bar, &[("Bar.sc", "Bar {}"), ("Bar.quark", "()")]);
        let mut project = sources_project(
            root.path(),
            &format!(
                "foo = {{ git = {{ url = \"{}\", tag = \"v0.2.0\" }} }}\nbar = {{ legacy-quark = true, git = {{ url = \"{}\" }} }}\n",
                file_url(&foo),
                file_url(&bar)
            ),
        );
        install_sources(&mut project, false).await.unwrap();

        let dir = root.path().join("project");
        let packages = dir.join("magnet_packages");
        assert!(packages.join("foo/Classes/Foo.sc").is_file());
        assert!(!packages.join("foo/notes.txt").exists());
        assert!(packages.join("bar/Bar.sc").is_file());
        assert!(packages.join("bar/Bar.quark").is_file());
        assert!(packages.join("alpha").is_dir());
        let lock = fs::read_to_string(dir.join(MAGNET_LOCK_FILE)).unwrap();
        assert!(
            lock.contains(&format!(
                "source = \"git+{}?tag=v0.2.0#{foo_sha}\"",
                file_url(&foo)
            )),
            "{lock}"
        );
        assert!(
            lock.contains(&format!(
                "[dependencies.bar]\nversion = \"0.0.0\"\nkinds = [\"quark\"]\nsource = \"git+{}#{bar_sha}\"\n",
                file_url(&bar)
            )),
            "{lock}"
        );

        // a new commit on the branch doesn't move the lock file, only `update` does
        let newer = commit_files(&bar, &[("Bar.sc", "Bar { baz {} }")]);
        let mut project = open_sources_project(root.path());
        install_sources(&mut project, false).await.unwrap();
        assert_eq!(
            fs::read_to_string(dir.join(MAGNET_LOCK_FILE)).unwrap(),
            lock
        );
        let changes = project
            .update(&fixture_index(), &["bar".to_string()])
            .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, "bar");
        assert_eq!(
            changes[0].2.as_ref().unwrap().label(),
            format!("0.0.0 (git {})", &newer[..7])
        );
        install_sources(&mut project, false).await.unwrap();
        assert_eq!(
            fs::read_to_string(packages.join("bar/Bar.sc")).unwrap(),
            "Bar { baz {} }"
        );

        // a fresh checkout without the cached clones installs the same files
        let installed = installed_files_in(&dir);
        fs::remove_dir_all(dir.join("magnet_packages")).unwrap();
        fs::remove_dir_all(root.path().join("git-cache")).unwrap();
        let mut project = open_sources_project(root.path());
        install_sources(&mut project, true).await.unwrap();
        assert_eq!(installed_files_in(&dir), installed);
    }

    fn installed_files_in(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let packages = dir.join("magnet_packages");
        walkdir::WalkDir::new(&packages)
            .into_iter()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    entry.path().strip_prefix(&packages).unwrap().to_path_buf(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn test_install_path_dependency() {
        let root = tempfile::tempdir().unwrap();
        let local = root.path().join("local");
        fs::create_dir_all(local.join("Classes")).unwrap();
        fs::write(local.join("Classes/Local.sc"), "Local {}").unwrap();
        fs::write(local.join("notes.txt"), "not selected").unwrap();
        fs::write(
            local.join(MAGNET_PROJECT_FILE),
            quark_manifest("local", "0.3.0", "alpha = \"^1\""),
        )
        .unwrap();
        let mut project = sources_project(root.path(), "local = { path = \"../local\" }\n");
        install_sources(&mut project, false).await.unwrap();

        let dir = root.path().join("project");
        let packages = dir.join("magnet_packages");
        assert!(packages.join("local/Classes/Local.sc").is_file());
        assert!(!packages.join("local/notes.txt").exists());
        assert!(packages.join("alpha").is_dir());
        let lock = fs::read_to_string(dir.join(MAGNET_LOCK_FILE)).unwrap();
        assert!(
            lock.contains("[dependencies.local]\nversion = \"0.3.0\"\nkinds = [\"quark\"]\nsource = \"../local\"\n"),
            "{lock}"
        );

        // a new version of the folder gets locked by the next install, and its files copied
        fs::write(
            local.join(MAGNET_PROJECT_FILE),
            quark_manifest("local", "0.4.0", "alpha = \"^1\""),
        )
        .unwrap();
        fs::write(local.join("Classes/Local.sc"), "Local { new {} }").unwrap();
        let mut project = open_sources_project(root.path());
        install_sources(&mut project, false).await.unwrap();
        assert!(fs::read_to_string(dir.join(MAGNET_LOCK_FILE))
            .unwrap()
            .contains("version = \"0.4.0\""));
        assert_eq!(
            fs::read_to_string(packages.join("local/Classes/Local.sc")).unwrap(),
            "Local { new {} }"
        );
    }

    #[test]
    fn test_index_package_requiring_git_dependency() {
        use crate::git_client::source::tests::commit_files;
        let root = tempfile::tempdir().unwrap();
        let ddw = root.path().join("ddw");
        commit_files(&ddw, &[("Ddw.sc", "Ddw {}")]);
        let mut project = sources_project(
            root.path(),
            &format!(
                "beta = \"^1.2\"\nddw-common = {{ git = {{ url = \"{}\" }} }}\n",
                file_url(&ddw)
            ),
        );
        let err = project
            .update(&fixture_index(), &[])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("beta 1.2.0 requires ddw-common from the index, but the project requires it from git+"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn test_install_quark_file_dependencies() {
        use crate::git_client::source::tests::commit_files;
        let root = tempfile::tempdir().unwrap();
        let baz = root.path().join("baz");
        commit_files(&baz, &[("Baz.sc", "Baz {}")]);
        let quarks = root.path().join("quarks");
        commit_files(
            &quarks,
            &[(
                "directory.txt",
                &format!("Baz={}\n", file_url(&baz).as_str().trim_end_matches('/')),
            )],
        );
        let bar = root.path().join("bar");
        commit_files(
            &bar,
            &[
                ("Bar.sc", "Bar {}"),
                ("Bar.quark", "(dependencies: [\"Alpha\", \"Baz\"])"),
            ],
        );
        let mut project = sources_project(
            root.path(),
            &format!(
                "bar = {{ legacy-quark = true, git = {{ url = \"{}\" }} }}\n",
                file_url(&bar)
            ),
        );
        project.set_quarks_directory(file_url(&quarks));
        install_sources(&mut project, false).await.unwrap();
        assert_eq!(
            installed_packages_in(&root.path().join("project")),
            // `*` resolves to alpha 1.1.0, which needs beta and ddw-common of the index
            ["alpha", "bar", "baz", "beta", "ddw-common"]
        );

        // the lock covers the .quark dependencies, so the directory isn't needed again
        let lock = fs::read_to_string(root.path().join("project").join(MAGNET_LOCK_FILE)).unwrap();
        fs::remove_dir_all(&quarks).unwrap();
        let mut project = open_sources_project(root.path());
        project.set_quarks_directory(file_url(&quarks));
        install_sources(&mut project, true).await.unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("project").join(MAGNET_LOCK_FILE)).unwrap(),
            lock
        );
    }

    fn installed_packages_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.join("magnet_packages"))
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().unwrap().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| !name.starts_with('.'))
            .collect();
        names.sort();
        names
    }

    /// The fixture index with an additional package `eta` 0.1.0 (the files of alpha 1.0.0),
    /// whose index line has `dependency` as only dependency, and a downloader for it
    fn index_with_eta(root: &Path, dependency: &str) -> (GitClient, PackageDownloader) {
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let index = root.join("index");
        for entry in walkdir::WalkDir::new(fixtures.join("index")) {
            let entry = entry.unwrap();
            let target = index.join(entry.path().strip_prefix(fixtures.join("index")).unwrap());
            match entry.file_type().is_dir() {
                true => fs::create_dir_all(&target).unwrap(),
                false => {
                    fs::copy(entry.path(), &target).unwrap();
                }
            }
        }
        let dl = root.join("dl");
        fs::create_dir_all(&dl).unwrap();
        for entry in fs::read_dir(fixtures.join("dl")).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), dl.join(entry.file_name())).unwrap();
        }
        let archive = dl.join("eta-0.1.0-quark-any-any.tar.gz");
        fs::copy(dl.join("alpha-1.0.0-quark-any-any.tar.gz"), &archive).unwrap();
        let sha256 = format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(fs::read(&archive).unwrap())
        );
        fs::write(
            index.join("e/eta.jsonl"),
            format!("{{\"name\": \"eta\", \"version\": \"0.1.0\", \"kinds\": [\"quark\"], \"dependencies\": {{{dependency}}}, \"artifacts\": [{{\"kind\": \"quark\", \"abi\": \"any\", \"platform\": \"any\", \"sha256\": \"{sha256}\"}}]}}\n"),
        )
        .unwrap();
        let downloader = PackageDownloader::new(
            root.join("download-cache"),
            format!(
                "{}{{name}}-{{version}}-{{kind}}-{{abi}}-{{platform}}.tar.gz",
                Url::from_directory_path(&dl).unwrap()
            ),
        );
        (GitClient::open_local(index).unwrap(), downloader)
    }

    /// An index with `theta` 1.0.0, a quark and a lepton (only for macos_arm64), packaged by
    /// `magnet package`, and a downloader for it
    fn index_with_theta(root: &Path) -> (GitClient, PackageDownloader) {
        let source = root.join("theta");
        fs::create_dir_all(source.join("classes")).unwrap();
        fs::create_dir_all(source.join("build")).unwrap();
        fs::write(source.join("classes/Theta.sc"), "Theta {}").unwrap();
        fs::write(source.join("build/Theta.scx"), "theta binary").unwrap();
        fs::write(
            source.join(MAGNET_PROJECT_FILE),
            "magnet-version = 1\n[package]\nname = \"theta\"\nversion = \"1.0.0\"\n[artifact.quark]\npaths = [\"classes/*.sc\"]\n[artifact.lepton.platform-paths]\nmacos_arm64 = [\"build/*.scx\"]\n",
        )
        .unwrap();
        let theta = Project::open(&source).unwrap();
        let tags = ArtifactTags::new("api3_0", "macos_arm64").unwrap();
        let mut artifacts = vec![];
        for (kind, tags) in [
            (ArtifactKind::Quark, ArtifactTags::any()),
            (ArtifactKind::Lepton, tags),
        ] {
            let archive = theta.create_package(kind, &tags, false).unwrap();
            let sha256 = format!(
                "{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(fs::read(&archive.path).unwrap())
            );
            artifacts.push(format!(
                "{{\"kind\": \"{kind}\", \"abi\": \"{}\", \"platform\": \"{}\", \"sha256\": \"{sha256}\"}}",
                tags.abi, tags.platform
            ));
        }
        let index = root.join("index");
        fs::create_dir_all(index.join("t")).unwrap();
        fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/index/config.json"),
            index.join("config.json"),
        )
        .unwrap();
        fs::write(
            index.join("t/theta.jsonl"),
            format!(
                "{{\"name\": \"theta\", \"version\": \"1.0.0\", \"kinds\": [\"quark\", \"lepton\"], \"dependencies\": {{}}, \"artifacts\": [{}]}}\n",
                artifacts.join(", ")
            ),
        )
        .unwrap();
        let downloader = PackageDownloader::new(
            root.join("download-cache"),
            format!(
                "{}{{name}}-{{version}}-{{kind}}-{{abi}}-{{platform}}.tar.gz",
                Url::from_directory_path(source.join("dist")).unwrap()
            ),
        );
        (GitClient::open_local(index).unwrap(), downloader)
    }

    /// every kind of a version goes into the package's folder
    #[tokio::test]
    async fn test_install_quark_and_lepton() {
        let root = tempfile::tempdir().unwrap();
        let (index, downloader) = index_with_theta(root.path());
        let mut project = sources_project(root.path(), "theta = \"^1\"\n");
        project.set_target(target("macos_arm64", "api3_0"));
        project.install(&index, &downloader, false).await.unwrap();

        let dir = root.path().join("project");
        let lock = fs::read_to_string(dir.join(MAGNET_LOCK_FILE)).unwrap();
        assert!(lock.contains("kinds = [\"quark\", \"lepton\"]"), "{lock}");
        let theta = dir.join("magnet_packages/theta");
        let mut files: Vec<_> = walkdir::WalkDir::new(&theta)
            .into_iter()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| archive_path(entry.path().strip_prefix(&theta).unwrap()).unwrap())
            .collect();
        files.sort();
        assert_eq!(
            files,
            [
                "classes/Theta.sc",
                "magnet.toml",
                "plugins/macos_arm64/api3_0/Theta.scx"
            ]
        );

        // without a binary for this machine nothing gets installed, also not the quark
        let other = root.path().join("other");
        let mut project = sources_project(&other, "theta = \"^1\"\n");
        project.set_target(target("linux_x86_64", "api3_0"));
        let err = project
            .install(&index, &downloader, false)
            .await
            .err()
            .unwrap();
        assert!(
            err.to_string().contains("no binary for linux_x86_64"),
            "{err}"
        );
        assert!(!other.join("project/magnet_packages/theta").exists());
    }

    #[tokio::test]
    async fn test_install_index_package_with_legacy_dependency() {
        use crate::git_client::source::tests::commit_files;
        let root = tempfile::tempdir().unwrap();
        let bar = root.path().join("bar");
        let sha = commit_files(
            &bar,
            &[
                ("Bar.sc", "Bar {}"),
                ("Bar.quark", "(dependencies: [\"Beta\"])"),
            ],
        );
        // a later commit, which the pinned ref must not pick up
        commit_files(&bar, &[("Bar.sc", "Bar { newer {} }")]);
        let (index, downloader) = index_with_eta(
            root.path(),
            &format!(
                "\"Bar\": {{\"legacy-quark\": true, \"git\": {{\"url\": \"{}\", \"ref\": \"{sha}\"}}}}",
                file_url(&bar)
            ),
        );
        let mut project = sources_project(root.path(), "eta = \"^0.1\"\n");
        project.install(&index, &downloader, false).await.unwrap();

        let dir = root.path().join("project");
        // bar's .quark file adds beta of the index (and beta 1.2.0 needs ddw-common)
        assert_eq!(
            installed_packages_in(&dir),
            ["bar", "beta", "ddw-common", "eta"]
        );
        assert_eq!(
            fs::read_to_string(dir.join("magnet_packages/bar/Bar.sc")).unwrap(),
            "Bar {}"
        );
        let lock = fs::read_to_string(dir.join(MAGNET_LOCK_FILE)).unwrap();
        assert!(
            lock.contains(&format!(
                "source = \"git+{}?ref={sha}#{sha}\"",
                file_url(&bar)
            )),
            "{lock}"
        );
    }

    #[test]
    fn test_index_package_with_conflicting_git_dependency() {
        use crate::git_client::source::tests::commit_files;
        let root = tempfile::tempdir().unwrap();
        let bar = root.path().join("bar");
        let pinned = commit_files(&bar, &[("Bar.sc", "Bar {}")]);
        commit_files(&bar, &[("Bar.sc", "Bar { newer {} }")]);
        let (index, _) = index_with_eta(
            root.path(),
            &format!(
                "\"bar\": {{\"git\": {{\"url\": \"{}\", \"ref\": \"{pinned}\"}}}}",
                file_url(&bar)
            ),
        );
        // the project wants the newest commit of bar, eta another one
        let mut project = sources_project(
            root.path(),
            &format!(
                "eta = \"^0.1\"\nbar = {{ git = {{ url = \"{}\" }} }}\n",
                file_url(&bar)
            ),
        );
        let err = project.update(&index, &[]).unwrap_err().to_string();
        assert!(
            err.contains(&format!(
                "eta 0.1.0 requires bar from {}#{pinned}",
                file_url(&bar)
            )),
            "{err}"
        );
    }

    #[tokio::test]
    async fn test_install_index_package_ignoring_quark_deps() {
        use crate::git_client::source::tests::commit_files;
        let root = tempfile::tempdir().unwrap();
        let bar = root.path().join("bar");
        let sha = commit_files(
            &bar,
            &[
                ("Bar.sc", "Bar {}"),
                ("Bar.quark", "(dependencies: [\"Beta\"])"),
            ],
        );
        let (index, downloader) = index_with_eta(
            root.path(),
            &format!(
                "\"Bar\": {{\"legacy-quark\": true, \"ignore-quark-deps\": true, \"git\": {{\"url\": \"{}\", \"ref\": \"{sha}\"}}}}",
                file_url(&bar)
            ),
        );
        let mut project = sources_project(root.path(), "eta = \"^0.1\"\n");
        project.install(&index, &downloader, false).await.unwrap();
        // beta of bar's .quark file isn't installed
        assert_eq!(
            installed_packages_in(&root.path().join("project")),
            ["bar", "eta"]
        );
    }
}
