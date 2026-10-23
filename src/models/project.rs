use std::{
    collections::HashMap,
    fs::{self, File},
    path::{Path, PathBuf},
};

use anyhow::{ensure, Context};
use pubgrub::SemanticVersion;
use semver::VersionReq;
use tracing::{info, instrument, trace};
use url::Url;

use crate::models::project_manifest::{
    MagnetManifestV1DependenciesValue, NamedDependency, ProjectManifest,
};
use crate::{
    config::constants::{MAGNET_LOCK_FILE, MAGNET_PROJECT_FILE},
    git_client::{client::GitClient, models::GitDependency},
    models::{
        directories::ProjectDirectory,
        lock_file::{LockDependency, LockFile},
        package::{BundleFormat, MagnetPackage, SourceBundle},
        project_manifest::{MagnetManifestV1, MagnetManifestV1PackageKind},
    },
    packaging::{collect::CollectFiles, compression::MagnetArchive, naming::PackageNaming},
    resolver::online_provider::{resolve_dependencies, RootProvider},
};

/// The Project struct is used to manage files of a project, such as
/// adding a dependency, creating a lockfile or packaging up for release.
pub struct Project {
    project_manifest: ProjectManifest,
    lock_file: LockFile,
    path: PathBuf,
}

impl Project {
    #[instrument(skip(magnet_manifest))]
    pub fn create(
        path: &Path,
        magnet_manifest: MagnetManifestV1,
        index: Url,
    ) -> anyhow::Result<Self> {
        let project_manifest =
            ProjectManifest::write_new(path.join(MAGNET_PROJECT_FILE), &magnet_manifest)?;
        let project = Self {
            project_manifest,
            lock_file: LockFile::create_empty(path.join(MAGNET_LOCK_FILE), index)?,
            path: path.into(),
        };
        project.generate_project_directories()?;
        info!("Created project");
        Ok(project)
    }

    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            project_manifest: ProjectManifest::open(path.join(MAGNET_PROJECT_FILE))?,
            lock_file: LockFile::open(&path.join(MAGNET_LOCK_FILE))?,
            path: path.to_path_buf(),
        })
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
            fs::create_dir_all(&path).context("Could not create folder {?:folder}")?;
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
            .dependencies
            .iter()
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
        self.lock_file = LockFile::from_tree(provider.url().clone(), tree, |name, version| {
            provider
                .get_release(name, version)?
                .map(|release| LockDependency::from(&release))
                .with_context(|| format!("{name} {version} is missing in the index"))
        })?;
        self.lock_file.write(&self.path.join(MAGNET_LOCK_FILE))
    }

    fn reload_from_file(&mut self) -> anyhow::Result<()> {
        self.project_manifest = ProjectManifest::open(self.path.join(MAGNET_PROJECT_FILE))?;
        self.lock_file = LockFile::open(&self.path.join(MAGNET_LOCK_FILE))?;
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
        self.create_source_package()?;

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

        let files = self
            .path
            .get_package_files(super::package::BundleType::SourceBundle)?;

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
            .dependencies
            .iter()
            .map(|(name, dependency)| (name.clone(), dependency.version.to_string()))
            .collect()
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
        assert!(lock.contains("checksums = { any-any = \"sha-alpha-1.1.0\" }"));
    }

    #[test]
    fn test_add_dependency_keeps_locked_versions() {
        let (dir, _) = project(MANIFEST);
        let index = fixture_index();
        let tree = HashMap::from([("beta".to_string(), SemanticVersion::new(1, 0, 0))]);
        LockFile::from_tree(index.url().clone(), &tree, |name, version| {
            Ok(LockDependency::from(
                &index.get_release(name, version)?.unwrap(),
            ))
        })
        .unwrap()
        .write(&dir.path().join(MAGNET_LOCK_FILE))
        .unwrap();

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
}
