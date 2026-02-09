use std::{env, error::Error, fs, path::PathBuf};

use crate::cli::MagnetSubCommand;
use anyhow::Context;
use clap::{arg, Parser};
use inquire::{
    validator::{self, Validation},
    Select, Text,
};
use magnet::models::package::{BundleFormat, Quark, QuarkAbiTag, SourcePlatformTag};
use magnet::{
    git_client::models::GitDependency,
    models::{
        project::{Project, ProjectTypeEnum},
        project_manifest::{MagnetManifestV1, MagnetManifestV1ArtifactsItemKind},
    },
};
use pubgrub::SemanticVersion;
use semver::VersionReq;
use tracing::{trace, warn};

#[derive(Debug, Parser)]
pub struct ProjectInitArgs {
    /// Path of the project.
    /// If not provided, a new folder with the name of the project
    /// will be created in the current working directory.
    #[arg(
        long,
        default_value = None,
    )]
    path: Option<PathBuf>,
    // further commands are not available via command line b/c
    // projects will only be created through users and not automated,
    // so we don't have to maintain/implement two paths.
}

impl MagnetSubCommand for ProjectInitArgs {
    async fn run(
        self,
        _git_client: magnet::git_client::client::GitClient,
        _http_client: magnet::http_client::basic::MagnetClient<magnet::http_client::basic::NoAuth>,
    ) -> anyhow::Result<()> {
        if let Some(ref path) = self.path {
            if path.read_dir().iter().len() > 0 {
                warn!(?path, "Specified project directory is not empty!");
            }
        }

        let project_name = Text::new("Name of the project:")
            .with_validator(validator::ValueRequiredValidator::default())
            .with_validator(
                |input: &str| -> Result<Validation, Box<dyn Error + Send + Sync>> {
                    match MagnetManifestV1::is_valid_name(input) {
                        true => Ok(Validation::Valid),
                        false => Ok(Validation::Invalid(
                            "Project name can only consist of alphanumeric characters and -_"
                                .into(),
                        )),
                    }
                },
            )
            .prompt()?;

        let project_kind = Select::new(
            "Project kind:",
            vec![
                MagnetManifestV1ArtifactsItemKind::Quark,
                // MagnetManifestV1ArtifactsItemKind::Gluon,
                MagnetManifestV1ArtifactsItemKind::Lepton,
            ],
        )
        .prompt()?;

        // @todo select license?

        let project_path = match self.path {
            Some(path) => path,
            None => std::env::current_dir()
                .context("Could not obtain current working directory")?
                .join(&project_name),
        };
        trace!(?project_path, "Use path");

        if !project_path.exists() {
            trace!(?project_path, "Create project directory");
            fs::create_dir_all(&project_path).context("Could not create project directory")?;
        }

        let project_manifest = MagnetManifestV1::new(
            project_name,
            None,
            None,
            SemanticVersion::new(0, 1, 0),
            None,
            project_kind,
        )?;

        Project::create(&project_path, project_manifest)?;

        Ok(())
    }
}

/// Adds a dependency to the current magnet project
#[derive(Debug, Parser)]
pub struct ProjectAddDependencyArgs {
    /// Path of the project.
    /// If not provided, a new folder with the name of the project
    /// will be created in the current working directory.
    #[arg(
        long,
        default_value = None,
    )]
    path: Option<PathBuf>,

    dependency: String,
}

fn parse_dependency(input: String) -> anyhow::Result<GitDependency> {
    // @todo parse optional version requirement
    Ok(GitDependency {
        name: input,
        constraint: VersionReq::STAR,
    })
}

impl MagnetSubCommand for ProjectAddDependencyArgs {
    async fn run(
        self,
        git_client: magnet::git_client::client::GitClient,
        _http_client: magnet::http_client::basic::MagnetClient<magnet::http_client::basic::NoAuth>,
    ) -> anyhow::Result<()> {
        let project_path = match self.path {
            Some(path) => path,
            None => env::current_dir()?,
        };
        let mut project_manager = Project::open(&project_path)?;
        let dependency = parse_dependency(self.dependency)?;

        project_manager.add_dependency(git_client, dependency)
    }
}

#[derive(Debug, Parser)]
pub struct ProjectBundleArgs {
    /// Path of the project.
    /// If not provided, a new folder with the name of the project
    /// will be created in the current working directory.
    #[arg(
        long,
        default_value = None,
    )]
    path: Option<PathBuf>,
}

impl MagnetSubCommand for ProjectBundleArgs {
    async fn run(
        self,
        _git_client: magnet::git_client::client::GitClient,
        _http_client: magnet::http_client::basic::MagnetClient<magnet::http_client::basic::NoAuth>,
    ) -> anyhow::Result<()> {
        let project_path = match self.path {
            Some(path) => path,
            None => env::current_dir()?,
        };
        let project = Project::open(&project_path)?;
        let bundle_format: BundleFormat<Quark> = BundleFormat {
            abi: QuarkAbiTag::Sclang314,
            platform: SourcePlatformTag::Any,
        };
        project.create_package(
            Some(MagnetManifestV1ArtifactsItemKind::Gluon),
            Some(bundle_format),
        )?;
        Ok(())
    }
}
