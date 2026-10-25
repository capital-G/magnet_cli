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

use crate::models::project_manifest::{
    MagnetManifestV1DependenciesValue, NamedDependency, ProjectManifest,
};
use crate::{
    config::constants::{MAGNET_LOCK_FILE, MAGNET_PROJECT_FILE},
    git_client::{client::GitClient, models::GitDependency},
    models::{
        directories::ProjectDirectory,
        gitignore::write_gitignore,
        language_config::{LanguageConfig, SCLANG_CONF_FILE},
        lock_file::{LockDependency, LockFile, LockSource},
        package::{ArtifactTags, BundleType, PackageArchive},
        project_manifest::{
            MagnetManifestV1, MagnetManifestV1EntrypointItem, MagnetManifestV1EntrypointItemKind,
            MagnetManifestV1PackageKind,
        },
    },
    packaging::{collect::collect_files, compression, naming::artifact_file_name},
    resolver::{
        online_provider::{
            resolve_dependencies, sclang_lower_bound, RootProvider, ToRanges, SCLANG_PACKAGE,
        },
        package_downloader::{Artifact, PackageDownloader},
    },
    sclang::{Sclang, MAGNET_SCLANG_ENV},
};

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
}

impl Project {
    #[instrument(skip(magnet_manifest))]
    /// The lock file gets created by the first `add` or `install`, since it needs an index
    pub fn create(path: &Path, magnet_manifest: MagnetManifestV1) -> anyhow::Result<Self> {
        let project_manifest =
            ProjectManifest::write_new(path.join(MAGNET_PROJECT_FILE), &magnet_manifest)?;
        let project = Self {
            project_manifest,
            lock_file: None,
            path: path.into(),
        };
        project.generate_project_directories()?;
        write_gitignore(&project.path)?;
        info!("Created project");
        Ok(project)
    }

    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            project_manifest: ProjectManifest::open(path.join(MAGNET_PROJECT_FILE))?,
            lock_file: Self::open_lock_file(path)?,
            path: path.to_path_buf(),
        })
    }

    fn open_lock_file(path: &Path) -> anyhow::Result<Option<LockFile>> {
        let lock_path = path.join(MAGNET_LOCK_FILE);
        match lock_path.exists() {
            true => Ok(Some(LockFile::open(&lock_path)?)),
            false => Ok(None),
        }
    }

    #[instrument(skip(self))]
    pub fn generate_project_directories(&self) -> anyhow::Result<()> {
        for directory in [
            ProjectDirectory::Classes,
            ProjectDirectory::Tests,
            ProjectDirectory::Docs,
            ProjectDirectory::Packages,
        ] {
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

        let mut dependencies: Vec<GitDependency> = self
            .manifest_dependencies()?
            .into_iter()
            .filter(|dependency| dependency.name != name)
            .collect();
        dependencies.push(GitDependency {
            name: name.clone(),
            constraint: constraint.clone().unwrap_or(VersionReq::STAR),
        });

        let tree = self
            .resolve(provider, dependencies)
            .with_context(|| format!("Failed to add dependency {name}"))?;

        let constraint = match constraint {
            Some(constraint) => constraint,
            None => {
                let version = tree
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
        self.write_lock(provider, &tree)?;

        self.reload_from_file()?;
        self.write_language_config()?;

        Ok(())
    }

    /// Writes the `sclang_conf.yaml` for the current state of the project
    /// Without a usable sclang, the config is still written, but without isolation
    fn write_language_config(&self) -> anyhow::Result<()> {
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

    fn write_language_config_for(&self, sclang: Option<Sclang>) -> anyhow::Result<()> {
        LanguageConfig::for_project(&self.path, self.project_manifest.get_manifest(), sclang)?
            .write(&self.path)
    }

    pub fn manifest(&self) -> &MagnetManifestV1 {
        self.project_manifest.get_manifest()
    }

    /// The project root, which holds `magnet.toml`
    pub fn root(&self) -> &Path {
        &self.path
    }

    pub fn kind(&self) -> &MagnetManifestV1PackageKind {
        &self.project_manifest.get_manifest().package.kind
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
                    .all(|(name, dependency)| self.package_dir(&dependency.kind, name).is_dir())
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

        let dependencies: Vec<GitDependency> = self
            .manifest_dependencies()?
            .into_iter()
            .filter(|dependency| dependency.name != key)
            .collect();
        let tree = self
            .resolve(provider, dependencies)
            .with_context(|| format!("Failed to remove dependency {key}"))?;

        let previous = self.lock_file.clone();
        info!("Remove {key}");
        self.project_manifest.remove_dependency(&key)?;
        self.write_lock(provider, &tree)?;
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
                .is_some_and(|locked| locked.version != dependency.version)
        });
        Ok(changed)
    }

    /// The dependencies of the manifest, as requirements for the resolver
    fn manifest_dependencies(&self) -> anyhow::Result<Vec<GitDependency>> {
        self.project_manifest
            .get_manifest()
            .dependencies
            .clone()
            .into_iter()
            .map(|(name, dependency)| GitDependency::try_from(NamedDependency { name, dependency }))
            .collect()
    }

    fn resolve(
        &self,
        provider: &GitClient,
        dependencies: Vec<GitDependency>,
    ) -> anyhow::Result<HashMap<String, SemanticVersion>> {
        // keep the locked versions where possible, so only the necessary changes are made
        self.resolve_preferring(provider, dependencies, |_| true)
    }

    /// Like [Self::resolve], but only prefers the locked versions of the packages `keep` accepts
    fn resolve_preferring(
        &self,
        provider: &GitClient,
        dependencies: Vec<GitDependency>,
        keep: impl Fn(&str) -> bool,
    ) -> anyhow::Result<HashMap<String, SemanticVersion>> {
        let preferred = self
            .lock_file
            .iter()
            .flat_map(|lock_file| &lock_file.dependencies)
            .filter(|(name, _)| keep(name))
            .map(|(name, dependency)| (name.clone(), dependency.version))
            .collect();
        let tree: HashMap<String, SemanticVersion> = resolve_dependencies(&RootProvider {
            git_provider: provider,
            dependencies,
            preferred,
            sclang: self.sclang_requirement()?,
        })?
        .into_iter()
        .filter(|(name, _)| name != SCLANG_PACKAGE)
        .collect();
        trace!(?tree, "Resolved dependencies");
        self.warn_about_sclang(provider, &tree)?;
        Ok(tree)
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
    ) -> anyhow::Result<Vec<(String, Option<SemanticVersion>, Option<SemanticVersion>)>> {
        let names: HashSet<String> = names
            .iter()
            .map(|name| MagnetManifestV1::normalize_name(name))
            .collect();
        let locked: BTreeMap<String, SemanticVersion> = self
            .lock_file
            .iter()
            .flat_map(|lock_file| &lock_file.dependencies)
            .map(|(name, dependency)| (name.clone(), dependency.version))
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

        let tree = self.resolve_preferring(provider, self.manifest_dependencies()?, |name| {
            !names.is_empty() && !names.contains(name)
        })?;
        self.write_lock(provider, &tree)?;
        self.write_language_config()?;

        let updated: BTreeMap<String, SemanticVersion> = self
            .lock_file
            .iter()
            .flat_map(|lock_file| &lock_file.dependencies)
            .map(|(name, dependency)| (name.clone(), dependency.version))
            .collect();
        let all: std::collections::BTreeSet<&String> =
            locked.keys().chain(updated.keys()).collect();
        Ok(all
            .into_iter()
            .filter_map(|name| {
                let (old, new) = (locked.get(name).copied(), updated.get(name).copied());
                (old != new).then(|| (name.clone(), old, new))
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

    /// Writes the lock file for a resolved tree, using the details of each release from the index
    fn write_lock(
        &mut self,
        provider: &GitClient,
        tree: &HashMap<String, SemanticVersion>,
    ) -> anyhow::Result<()> {
        let lock_file = LockFile::from_tree(
            provider.url().clone(),
            provider.http_url()?,
            tree,
            |name, version| {
                provider
                    .get_release(name, version)?
                    .map(|release| LockDependency::from(&release))
                    .with_context(|| format!("{name} {version} is missing in the index"))
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

    /// `true` if every dependency of the manifest is locked with a version which satisfies it
    fn lock_covers_manifest(&self) -> anyhow::Result<bool> {
        let Some(lock_file) = &self.lock_file else {
            return Ok(false);
        };
        Ok(self.manifest_dependencies()?.iter().all(|dependency| {
            lock_file
                .dependencies
                .get(&MagnetManifestV1::normalize_name(&dependency.name))
                .is_some_and(|locked| dependency.constraint.to_ranges().contains(&locked.version))
        }))
    }

    /// Installs the locked dependencies into `magnet_packages/<kind>s/<name>`, and removes the
    /// packages which are not locked (anymore).
    /// The dependencies only get resolved if the lock file does not cover the manifest.
    pub async fn install(
        &mut self,
        provider: &GitClient,
        downloader: &PackageDownloader,
    ) -> anyhow::Result<()> {
        if !self.lock_covers_manifest()? {
            info!("{MAGNET_LOCK_FILE} is missing or outdated, resolving the dependencies");
            let tree = self.resolve(provider, self.manifest_dependencies()?)?;
            self.write_lock(provider, &tree)?;
        } else if let Some(lock_file) = &self.lock_file {
            let locked = lock_file
                .dependencies
                .iter()
                .map(|(name, dependency)| (name.clone(), dependency.version))
                .collect();
            self.warn_about_sclang(provider, &locked)?;
        }
        self.install_locked(provider, downloader).await
    }

    /// Installs exactly what the lock file holds into `magnet_packages/` and removes what it doesn't,
    /// without resolving. `add`, `remove` and `update` call this after writing the lock.
    pub async fn install_locked(
        &self,
        provider: &GitClient,
        downloader: &PackageDownloader,
    ) -> anyhow::Result<()> {
        let lock_file = self
            .lock_file
            .as_ref()
            .context("A lock file should exist after resolving")?;
        self.check_locked_versions(provider, lock_file)?;

        let artifacts = lock_file
            .dependencies
            .iter()
            .map(|(name, dependency)| locked_artifact(name, dependency))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let archives = downloader
            .download_all(&artifacts, PARALLEL_DOWNLOADS)
            .await?;
        for (artifact, archive) in artifacts.iter().zip(archives) {
            self.install_package(artifact, &archive)?;
        }

        self.prune_packages(lock_file)?;
        self.write_language_config()?;
        info!("Installed {} packages", artifacts.len());
        Ok(())
    }

    /// `magnet_packages/<kind>s/<name>`
    fn package_dir(&self, kind: &MagnetManifestV1PackageKind, name: &str) -> PathBuf {
        self.project_dir(ProjectDirectory::Packages)
            .join(format!("{kind}s"))
            .join(MagnetManifestV1::normalize_name(name))
    }

    /// Extracts the archive of a package next to its directory and swaps it in afterwards,
    /// so a broken archive never replaces a working package
    fn install_package(&self, artifact: &Artifact, archive: &Path) -> anyhow::Result<()> {
        let target = self.package_dir(&artifact.kind, &artifact.name);
        let kind_dir = target
            .parent()
            .context("A package directory has a parent")?;
        fs::create_dir_all(kind_dir)
            .with_context(|| format!("Could not create directory {kind_dir:?}"))?;

        let staging = tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .tempdir_in(kind_dir)?;
        compression::extract(archive, staging.path())?;
        if target.exists() {
            fs::remove_dir_all(&target)
                .with_context(|| format!("Could not remove old package {target:?}"))?;
        }
        fs::rename(staging.keep(), &target)
            .with_context(|| format!("Could not move package to {target:?}"))?;
        trace!(?target, "Installed package");
        Ok(())
    }

    /// Removes the package directories within `magnet_packages/<kind>s` which are not locked,
    /// as well as left over staging directories. Other hidden directories, such as
    /// `magnet_packages/.dev`, are left alone.
    fn prune_packages(&self, lock_file: &LockFile) -> anyhow::Result<()> {
        let locked: HashSet<PathBuf> = lock_file
            .dependencies
            .iter()
            .map(|(name, dependency)| self.package_dir(&dependency.kind, name))
            .collect();

        for kind in [
            MagnetManifestV1PackageKind::Quark,
            MagnetManifestV1PackageKind::Lepton,
            MagnetManifestV1PackageKind::Gluon,
        ] {
            let kind_dir = self
                .project_dir(ProjectDirectory::Packages)
                .join(format!("{kind}s"));
            if !kind_dir.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&kind_dir)? {
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
        }
        Ok(())
    }

    /// Packages every archive of the project's version which can be built on this machine
    /// into `dist/`: the `any-any` artifact of a quark, the source package of a lepton or gluon.
    /// Their binaries need the platform and ABI tags (C3.2).
    pub fn create_packages(&self) -> anyhow::Result<Vec<PackageArchive>> {
        let tags = match self.kind() {
            MagnetManifestV1PackageKind::Quark => vec![ArtifactTags::any()],
            _ => vec![ArtifactTags::source()],
        };
        tags.iter().map(|tags| self.create_package(tags)).collect()
    }

    /// Packages the project as the artifact with the given tags into `dist/`, see DESIGN.md
    /// "Packaging". The selected files keep their path relative to the project root, only the
    /// `platform-paths` binaries go into `plugins/`.
    /// [ArtifactTags::source] creates the source package of a lepton or gluon.
    #[instrument(skip(self))]
    pub fn create_package(&self, tags: &ArtifactTags) -> anyhow::Result<PackageArchive> {
        let manifest = self.project_manifest.get_manifest();
        let kind = &manifest.package.kind;
        ensure!(
            *kind != MagnetManifestV1PackageKind::Quark || *tags == ArtifactTags::any(),
            "A quark only has an {} artifact, not {tags}",
            ArtifactTags::any()
        );
        let files = collect_files(&self.path, manifest, &BundleType::from(tags))?;

        let mut entries = BTreeMap::new();
        for file in &files.files {
            entries.insert(archive_path(file)?, self.path.join(file));
        }
        for binary in &files.binaries {
            let name = binary
                .file_name()
                .with_context(|| format!("{binary:?} has no file name"))?
                .to_string_lossy();
            let path = format!("{PLUGINS_DIR}/{name}");
            ensure!(
                entries
                    .insert(path.clone(), self.path.join(binary))
                    .is_none(),
                "{binary:?} would be packaged as {path}, which is already part of the package"
            );
        }

        let dist = self.project_dir(ProjectDirectory::Dist);
        fs::create_dir_all(&dist).with_context(|| format!("Could not create {dist:?}"))?;
        let package_path = dist.join(artifact_file_name(
            &manifest.package.name,
            &SemanticVersion::try_from(manifest.package.version.clone())?,
            kind,
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
            tags: tags.clone(),
            files: entries.into_keys().collect(),
        })
    }
}

/// binaries of `platform-paths` within a package
const PLUGINS_DIR: &str = "plugins";

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

/// The artifact of a locked package which gets installed
fn locked_artifact(name: &str, dependency: &LockDependency) -> anyhow::Result<Artifact> {
    if dependency.source != LockSource::Index {
        bail!(
            "{name} has the source {}, which can not be installed yet",
            dependency.source
        );
    }
    if dependency.kind != MagnetManifestV1PackageKind::Quark {
        bail!(
            "{name} is a {}, which can not be installed yet",
            dependency.kind
        );
    }
    let (abi, platform) = ("any", "any");
    let sha256 = dependency
        .checksums
        .get(&format!("{abi}-{platform}"))
        .with_context(|| {
            format!(
                "{name} {} has no {abi}-{platform} artifact in {MAGNET_LOCK_FILE}",
                dependency.version
            )
        })?;
    Ok(Artifact {
        name: name.to_string(),
        version: dependency.version,
        kind: dependency.kind.clone(),
        abi: abi.to_string(),
        platform: platform.to_string(),
        sha256: sha256.clone(),
    })
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
    use url::Url;

    const MANIFEST: &str = r#"# my project
magnet-version = 1

[package]
name = "my-project"
version = "0.1.0"
kind = "quark"

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
            "checksums = {{ any-any = \"{}\" }}",
            alpha.artifacts[0].sha256
        )));
    }

    #[test]
    fn test_add_respects_sclang_range() {
        let with_sclang = |sclang: &str| {
            MANIFEST.replace(
                "kind = \"quark\"\n",
                &format!("kind = \"quark\"\nsclang = \"{sclang}\"\n"),
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
            .install(&fixture_index(), &fixture_downloader(&cache))
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
                "quarks/alpha/Classes/Alpha.sc",
                "quarks/alpha/HelpSource/Classes/Alpha.schelp",
                "quarks/beta/Classes/Beta.sc",
                "quarks/ddw-common/DDWCommon.sc",
            ]
            .map(PathBuf::from)
        );
        let alpha = &installed[&PathBuf::from("quarks/alpha/Classes/Alpha.sc")];
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
        let beta = &installed_files(&dir)[&PathBuf::from("quarks/beta/Classes/Beta.sc")];
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
        assert_eq!(installed_files(&dir).len(), 2);
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
            "quarks/stale/Classes/Stale.sc",
            "leptons/stale/plugins/Stale.scx",
            "quarks/.magnet-leftover/Classes/Half.sc",
            "quarks/.hidden/keep",
            ".dev/my-lepton/plugins/Dev.scx",
        ] {
            fs::create_dir_all(packages.join(path).parent().unwrap()).unwrap();
            fs::write(packages.join(path), b"").unwrap();
        }

        install(&dir).await.unwrap();
        assert!(!packages.join("quarks/stale").exists());
        assert!(!packages.join("leptons/stale").exists());
        assert!(!packages.join("quarks/.magnet-leftover").exists());
        assert!(packages.join("quarks/.hidden/keep").exists());
        assert!(packages.join(".dev/my-lepton/plugins/Dev.scx").exists());
        assert!(packages.join("quarks/beta").exists());
    }

    #[tokio::test]
    async fn test_install_refuses_lepton() {
        let (dir, _) = project(&format!("{MANIFEST}gamma = \"*\"\n"));
        let err = install(&dir).await.err().unwrap();
        assert!(err.to_string().contains("lepton"), "{err:#}");
    }

    /// a manifest without dependencies, with the given entrypoints
    fn manifest_with_entrypoints(entrypoints: &str) -> String {
        format!(
            "magnet-version = 1\n\n[package]\nname = \"runner\"\nversion = \"0.1.0\"\nkind = \"quark\"\n\n{entrypoints}"
        )
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
        let mut names: Vec<String> = fs::read_dir(dir.path().join("magnet_packages/quarks"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
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
        let dir = tempfile::tempdir().unwrap();
        // e.g. magnet init within an existing repository
        fs::write(dir.path().join(".gitignore"), "*.bak\n").unwrap();
        let manifest = MagnetManifestV1::new(
            "my-project".to_string(),
            None,
            None,
            SemanticVersion::new(0, 1, 0),
            None,
            MagnetManifestV1PackageKind::Quark,
        )
        .unwrap();
        Project::create(dir.path(), manifest).unwrap();

        assert!(dir.path().join(MAGNET_PROJECT_FILE).is_file());
        assert!(dir.path().join("classes").is_dir());
        // the lock file gets created by the first add or install
        assert!(!dir.path().join(MAGNET_LOCK_FILE).exists());
        let gitignore = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gitignore.starts_with("*.bak\n"), "{gitignore}");
        for entry in ["/dist/", "/magnet_packages/", "/sclang_conf.yaml"] {
            assert!(gitignore.lines().any(|line| line == entry), "{gitignore}");
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
        let (dir, project) = project(
            "magnet-version = 1\n[package]\nname = \"My_Quark\"\nversion = \"0.2.0\"\nkind = \"quark\"\n[artifact]\nglobal-paths = [\"Classes/**.sc\", \"HelpSource/**/*\"]\n",
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

        let package = project.create_package(&ArtifactTags::any()).unwrap().path;
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
        project.create_package(&ArtifactTags::any()).unwrap();
        assert_eq!(fs::read(&package).unwrap(), first);

        assert!(project.create_package(&ArtifactTags::source()).is_err());

        // the reported files are the contents of the archive
        let archives = project.create_packages().unwrap();
        assert_eq!(archives.len(), 1);
        assert_eq!(archives[0].tags, ArtifactTags::any());
        assert_eq!(archives[0].files, archive_paths(&package));
    }

    const LEPTON_MANIFEST: &str = "magnet-version = 1\n[package]\nname = \"foo\"\nversion = \"1.0.0\"\nkind = \"lepton\"\n[artifact]\nglobal-paths = [\"Classes/**.sc\"]\nplatform-paths = { platform = \"macos\", paths = [\"build/**/*.scx\"] }\nsource-paths = [\"Classes/**\", \"src/**\"]\n";

    #[test]
    fn test_create_lepton_packages() {
        let (dir, project) = project(LEPTON_MANIFEST);
        write_files(
            &dir,
            &[
                "Classes/Foo.sc",
                "src/Foo.cpp",
                "build/Release/Foo.scx",
                "build/Foo.o",
            ],
        );

        let tags = ArtifactTags::new("api3", "macos_arm64").unwrap();
        let package = project.create_package(&tags).unwrap().path;
        assert!(package.ends_with("foo-1.0.0-lepton-api3-macos_arm64.tar.gz"));
        assert_eq!(
            archive_paths(&package),
            ["Classes/Foo.sc", "magnet.toml", "plugins/Foo.scx"]
        );

        let source = project
            .create_package(&ArtifactTags::source())
            .unwrap()
            .path;
        assert!(source.ends_with("foo-1.0.0-lepton-source-any.tar.gz"));
        // binaries need platform tags (C3.2), so only the source package is built for now
        let archives = project.create_packages().unwrap();
        assert_eq!(archives.len(), 1);
        assert_eq!(archives[0].path, source);
        assert_eq!(
            archive_paths(&source),
            ["Classes/Foo.sc", "magnet.toml", "src/Foo.cpp"]
        );
    }

    #[test]
    fn test_binaries_with_the_same_name() {
        let (dir, project) = project(LEPTON_MANIFEST);
        write_files(
            &dir,
            &["Classes/Foo.sc", "build/a/Foo.scx", "build/b/Foo.scx"],
        );
        let err = project
            .create_package(&ArtifactTags::new("api3", "macos_arm64").unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("plugins/Foo.scx"), "{err}");
    }
    const ALPHA_BETA: &str = "magnet-version = 1\n[package]\nname = \"p\"\nversion = \"0.1.0\"\nkind = \"quark\"\n[dependencies]\nalpha = \"^1\"\nbeta = \"^1\"\n";

    fn version(version: &str) -> Option<SemanticVersion> {
        Some(version.parse().unwrap())
    }

    #[tokio::test]
    async fn test_install_keeps_yanked_locked_version() {
        let (dir, _) = project("magnet-version = 1\n[package]\nname = \"p\"\nversion = \"0.1.0\"\nkind = \"quark\"\n[dependencies]\nalpha = \"*\"\n");
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
        let alpha = fs::read_to_string(
            dir.path()
                .join("magnet_packages/quarks/alpha/Classes/Alpha.sc"),
        )
        .unwrap();
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
            changes,
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
            changes,
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
            .install_locked(&index, &fixture_downloader(&cache))
            .await
            .unwrap();
        assert_eq!(installed_packages(&dir), ["alpha", "beta", "ddw-common"]);
    }
}
