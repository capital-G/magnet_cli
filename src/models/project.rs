use std::{
    fmt::Display,
    fs::{self, File},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context};
use pubgrub::{resolve, SemanticVersion};
use serde::{Deserialize, Serialize};
use tracing::{info, instrument, trace};

use crate::models::project_manifest::{
    MagnetManifestV1DependenciesValue, NamedDependency, ProjectManifest,
};
use crate::{
    config::constants::{MAGNET_LOCK_FILE, MAGNET_PROJECT_FILE},
    git_client::{client::GitClient, models::GitDependency},
    models::{
        directories::ProjectDirectory,
        lock_file::LockFile,
        package::{BundleFormat, MagnetPackage, SourceBundle},
        project_manifest::{MagnetManifestV1, MagnetManifestV1ArtifactsItemKind},
    },
    packaging::{collect::CollectFiles, compression::MagnetArchive, naming::PackageNaming},
    resolver::online_provider::RootProvider,
};

#[derive(Deserialize, Serialize, Debug)]
pub enum ArchitectureEnum {}

#[derive(Deserialize, Serialize, Debug)]
pub enum AssetFormatEnum {}

#[derive(Deserialize, Serialize, Debug)]
pub enum ServerAbiVersionEnum {}

#[derive(Deserialize, Serialize, Debug)]
pub enum ProjectTypeEnum {
    Quark,
    ServerExtension,
}

impl Display for ProjectTypeEnum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            ProjectTypeEnum::Quark => "Quark",
            ProjectTypeEnum::ServerExtension => "Lepton",
        };
        write!(f, "{}", name)
    }
}

/// The Project struct is used to manage files of a project, such as
/// adding a dependency, creating a lockfile or packaging up for release.
pub struct Project {
    project_manifest: ProjectManifest,
    lock_file: LockFile,
    path: PathBuf,
}

impl Project {
    #[instrument(skip(magnet_manifest))]
    pub fn create(path: &Path, magnet_manifest: MagnetManifestV1) -> anyhow::Result<Self> {
        let project_manifest =
            ProjectManifest::write_new(path.join(MAGNET_PROJECT_FILE), &magnet_manifest)?;
        let project = Self {
            project_manifest,
            lock_file: LockFile::create_empty(path.join(MAGNET_LOCK_FILE))?,
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

    pub fn add_dependency(
        &mut self,
        provider: GitClient,
        dependency: GitDependency,
    ) -> anyhow::Result<()> {
        let root_provider = RootProvider {
            git_provider: provider,
            dependencies: self
                .project_manifest
                .get_manifest()
                .dependencies
                .clone()
                .into_iter()
                .map(|(name, dependency)| {
                    GitDependency::try_from(NamedDependency { name, dependency })
                })
                .collect::<Result<Vec<_>, _>>()?,
        };

        match resolve(&root_provider, "root".to_string(), SemanticVersion::zero()) {
            Ok(tree) => {
                info!("Successfully resolved the tree! {:?}", tree);
                let magnet_dependency = MagnetManifestV1DependenciesValue::Object {
                    branch: None,
                    git: None,
                    index: None,
                    legacy_quark: None,
                    path: None,
                    ref_: None,
                    tag: None,
                    version: Some(dependency.constraint.to_string()),
                };
                self.project_manifest.add_dependency(NamedDependency {
                    name: dependency.name,
                    dependency: magnet_dependency,
                })?;
                self.lock_file = LockFile::from_tree(&tree)?;
                self.lock_file.write(&self.path.join(MAGNET_LOCK_FILE))?;
            }
            Err(err) => {
                bail!("Failed to add dependency: {:?}", err);
            }
        }

        self.reload_from_file()?;

        Ok(())
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
        _artifact_kind: Option<MagnetManifestV1ArtifactsItemKind>,
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
}
