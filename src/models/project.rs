use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

use anyhow::{bail, ensure, Context};
use pubgrub::SemanticVersion;
use semver::VersionReq;
use tracing::{info, instrument, trace, warn};

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
        package::{BundleFormat, BundleType, MagnetPackage, SourceBundle},
        project_manifest::{
            MagnetManifestV1, MagnetManifestV1EntrypointItem, MagnetManifestV1EntrypointItemKind,
            MagnetManifestV1PackageKind,
        },
    },
    packaging::{
        collect::collect_files,
        compression::{self, MagnetArchive},
        naming::PackageNaming,
    },
    resolver::{
        online_provider::{resolve_dependencies, RootProvider, ToRanges},
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
            ProjectDirectory::External,
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
    /// not needed anymore from the lock file and `external`.
    /// The manifest and the lock file are only written if the dependencies can be resolved.
    pub fn remove_dependency(&mut self, provider: &GitClient, name: &str) -> anyhow::Result<()> {
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
        self.prune_external(lock_file)?;
        self.write_language_config()?;

        // removing a dependency only changes other versions if a locked one is gone
        let changed = lock_file.dependencies.iter().any(|(name, dependency)| {
            previous
                .as_ref()
                .and_then(|previous| previous.dependencies.get(name))
                .is_some_and(|locked| locked.version != dependency.version)
        });
        if changed {
            warn!(
                "The versions of other dependencies changed, run `magnet install` to update them"
            );
        }
        Ok(())
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
        let preferred = self
            .lock_file
            .iter()
            .flat_map(|lock_file| &lock_file.dependencies)
            .map(|(name, dependency)| (name.clone(), dependency.version))
            .collect();
        let tree = resolve_dependencies(&RootProvider {
            git_provider: provider,
            dependencies,
            preferred,
        })?;
        trace!(?tree, "Resolved dependencies");
        Ok(tree.into_iter().collect())
    }

    /// Writes the lock file for a resolved tree, using the details of each release from the index
    fn write_lock(
        &mut self,
        provider: &GitClient,
        tree: &HashMap<String, SemanticVersion>,
    ) -> anyhow::Result<()> {
        let lock_file = LockFile::from_tree(provider.url().clone(), tree, |name, version| {
            provider
                .get_release(name, version)?
                .map(|release| LockDependency::from(&release))
                .with_context(|| format!("{name} {version} is missing in the index"))
        })?;
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

    /// Installs the locked dependencies into `external/<kind>s/<name>`, and removes the
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
        }
        let lock_file = self
            .lock_file
            .as_ref()
            .context("A lock file should exist after resolving")?;

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

        self.prune_external(lock_file)?;
        self.write_language_config()?;
        info!("Installed {} packages", artifacts.len());
        Ok(())
    }

    /// `external/<kind>s/<name>`
    fn package_dir(&self, kind: &MagnetManifestV1PackageKind, name: &str) -> PathBuf {
        self.project_dir(ProjectDirectory::External)
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

    /// Removes the package directories within `external/<kind>s` which are not locked,
    /// as well as left over staging directories. Other hidden directories, such as
    /// `external/.dev`, are left alone.
    fn prune_external(&self, lock_file: &LockFile) -> anyhow::Result<()> {
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
                .project_dir(ProjectDirectory::External)
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

    /// collects all files of the project and puts them into a .tar.gz
    /// file which can be distributed.
    /// It will be saved under with the upload identifier format in the project directory
    /// under `/dist`.
    /// Additionally, the source package will also be created or updated.
    #[instrument(skip(self, _artifact_kind, _bundle_format))]
    pub fn create_package<P: MagnetPackage>(
        &self,
        _artifact_kind: Option<MagnetManifestV1PackageKind>,
        _bundle_format: Option<BundleFormat<P>>,
    ) -> anyhow::Result<PathBuf> {
        // create tar file outside of directory and move it later back in to avoid recursive problems
        // while scanning the project directory
        if self.project_manifest.get_manifest().package.kind != MagnetManifestV1PackageKind::Quark {
            self.create_source_package()?;
        }

        let temp_tar = tempfile::NamedTempFile::new()?;
        let temp_tar_path = temp_tar.path();
        trace!(?temp_tar_path, "Create temporary tar file");

        let tar_file = File::create(temp_tar_path)?;
        let encoder = flate2::write::GzEncoder::new(&tar_file, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);

        Self::fill_archive(&mut tar, &self.path, &self.path)?;

        tar.into_inner()?.finish()?;

        let release_path = self.release_package_path();
        temp_tar
            .persist(&release_path)
            .context("Could not write package file to {release_path:?}")?;
        info!("Written package file to {:?}", release_path);

        Ok(release_path)
    }

    #[instrument(skip(self))]
    fn create_source_package(&self) -> anyhow::Result<PathBuf> {
        let bundle = SourceBundle {};
        let source_name = bundle.package_file_name(
            &self.project_manifest.get_manifest().package.name,
            &SemanticVersion::try_from(
                self.project_manifest.get_manifest().package.version.clone(),
            )?,
        );
        let archive_path = self.project_dir(ProjectDirectory::Dist).join(source_name);

        let files: Vec<PathBuf> = collect_files(
            &self.path,
            self.project_manifest.get_manifest(),
            &BundleType::SourceBundle,
        )?
        .files
        .iter()
        .map(|file| self.path.join(file))
        .collect();

        self.compress(&files, &self.path, &archive_path)?;

        Ok(archive_path)
    }

    fn release_package_path(&self) -> PathBuf {
        self.project_dir(ProjectDirectory::Dist).join(format!(
            "{}_{}.tar.gz",
            self.project_manifest.get_manifest().package.name,
            self.project_manifest
                .get_manifest()
                .package
                .version
                .to_string()
                .replace(".", "-")
        ))
    }

    /// Walks a the project directory recursively and adds the files accordingly
    fn fill_archive(
        tar: &mut tar::Builder<flate2::write::GzEncoder<&File>>,
        base: &Path,
        current: &Path,
    ) -> Result<(), anyhow::Error> {
        for entry in fs::read_dir(current).unwrap() {
            let entry = entry?;
            let path = entry.path();

            if exclude_path(base, current) {
                continue;
            }
            if path.is_dir() {
                Self::fill_archive(tar, base, &path)?;
            } else if path.is_file() {
                let relative_path: PathBuf = path.strip_prefix(base)?.into();
                trace!(?relative_path, "Include file");
                tar.append_path_with_name(path, relative_path)?;
            }
        }
        Ok(())
    }
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

static PROJECT_EXCLUDE_PATHS: &[&str] = &["build", ".git", ".vscode", ".DS_Store"];

/// determines if a given path within the project directory gets ignored or not
/// returns `true` if the given path should be rejected
fn exclude_path(_base: &Path, current: &Path) -> bool {
    let file_name = current.file_name().unwrap().to_str().unwrap();
    PROJECT_EXCLUDE_PATHS.iter().any(|name| name == &file_name)
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;
    use url::Url;

    #[test]
    fn test_exclude_path() {
        let path = env::current_dir().unwrap();

        for non_reject in ["foo.scd", "classes"] {
            assert!(!exclude_path(&path, &path.join(non_reject)));
            assert!(!exclude_path(&path, &path.join("sub_idr").join(non_reject)));
        }
    }

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
        LockFile::from_tree(index.url().clone(), &tree, |name, version| {
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

    /// all files within `external`, with their content
    fn external_files(dir: &tempfile::TempDir) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let external = dir.path().join("external");
        walkdir::WalkDir::new(&external)
            .into_iter()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    entry.path().strip_prefix(&external).unwrap().to_path_buf(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    fn read_lock(dir: &tempfile::TempDir) -> String {
        fs::read_to_string(dir.path().join(MAGNET_LOCK_FILE)).unwrap()
    }

    #[tokio::test]
    async fn test_install_reproduces_external() {
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
        let installed = external_files(&dir);
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
            std::path::absolute(dir.path().join("external"))
                .unwrap()
                .display()
        )));

        // a fresh clone contains no external directory
        fs::remove_dir_all(dir.path().join("external")).unwrap();
        install(&dir).await.unwrap();
        assert_eq!(external_files(&dir), installed);
        assert_eq!(read_lock(&dir), lock);
    }

    #[tokio::test]
    async fn test_install_uses_lock_file() {
        let (dir, _) = project(MANIFEST);
        write_lock_file(&dir, &[("beta", "1.0.0")]);
        let lock = read_lock(&dir);

        install(&dir).await.unwrap();
        // beta 1.2.0 would be newer, but the lock covers the manifest
        let beta = &external_files(&dir)[&PathBuf::from("quarks/beta/Classes/Beta.sc")];
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
        assert_eq!(external_files(&dir).len(), 2);
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
    async fn test_install_prunes_external() {
        let (dir, _) = project(MANIFEST);
        let external = dir.path().join("external");
        for path in [
            "quarks/stale/Classes/Stale.sc",
            "leptons/stale/plugins/Stale.scx",
            "quarks/.magnet-leftover/Classes/Half.sc",
            "quarks/.hidden/keep",
            ".dev/my-lepton/plugins/Dev.scx",
        ] {
            fs::create_dir_all(external.join(path).parent().unwrap()).unwrap();
            fs::write(external.join(path), b"").unwrap();
        }

        install(&dir).await.unwrap();
        assert!(!external.join("quarks/stale").exists());
        assert!(!external.join("leptons/stale").exists());
        assert!(!external.join("quarks/.magnet-leftover").exists());
        assert!(external.join("quarks/.hidden/keep").exists());
        assert!(external.join(".dev/my-lepton/plugins/Dev.scx").exists());
        assert!(external.join("quarks/beta").exists());
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
        let mut names: Vec<String> = fs::read_dir(dir.path().join("external/quarks"))
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
        assert!(config.contains("external"), "{config}");
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
        for entry in ["/dist/", "/external/", "/sclang_conf.yaml"] {
            assert!(gitignore.lines().any(|line| line == entry), "{gitignore}");
        }
    }
}
