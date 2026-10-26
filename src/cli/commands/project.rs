use std::{env, error::Error, fs, path::PathBuf};

use crate::cli::{CliContext, MagnetSubCommand};
use anyhow::Context;
use clap::{arg, Args, Parser};
use inquire::{
    validator::{self, Validation},
    Select, Text,
};
use magnet::{
    config::constants::{magnet_home, CACHE_DIR},
    git_client::client::GitClient,
    models::{
        lock_file::LockDependency,
        project::Project,
        project_manifest::{MagnetManifestV1, MagnetManifestV1PackageKind},
    },
    resolver::package_downloader::PackageDownloader,
    sclang::Sclang,
    target::{LazyTarget, MAGNET_ABI_ENV, MAGNET_PLATFORM_ENV},
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
    async fn run(self, _ctx: &CliContext) -> anyhow::Result<()> {
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
                MagnetManifestV1PackageKind::Quark,
                // MagnetManifestV1PackageKind::Gluon,
                MagnetManifestV1PackageKind::Lepton,
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
            default_sclang_requirement(),
            project_kind,
        )?;

        Project::create(&project_path, project_manifest)?;

        Ok(())
    }
}

/// `>=<major>.<minor>` of the found sclang, or `None` without one (DESIGN.md
/// "SuperCollider version")
fn default_sclang_requirement() -> Option<String> {
    let version = match Sclang::find().and_then(|sclang| match sclang {
        Some(sclang) => sclang.version().map(Some),
        None => Ok(None),
    }) {
        Ok(Some(version)) => version,
        Ok(None) => {
            warn!("Could not find sclang, so the manifest gets no `sclang` range");
            return None;
        }
        Err(err) => {
            warn!("{err:#}, so the manifest gets no `sclang` range");
            return None;
        }
    };
    let (major, minor, _) = version.into();
    Some(format!(">={major}.{minor}"))
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

    /// Only update magnet.toml and magnet.lock, don't install into `magnet_packages/`
    #[arg(long)]
    no_install: bool,

    #[command(flatten)]
    target: TargetArgs,

    dependency: String,
}

/// Parses `foo`, `foo@1.2` or `foo@^1.2` into a name and an optional version requirement
fn parse_dependency(input: &str) -> anyhow::Result<(String, Option<VersionReq>)> {
    match input.split_once('@') {
        Some((name, constraint)) => Ok((
            name.to_string(),
            Some(
                constraint
                    .parse()
                    .with_context(|| format!("Invalid version requirement {constraint}"))?,
            ),
        )),
        None => Ok((input.to_string(), None)),
    }
}

impl MagnetSubCommand for ProjectAddDependencyArgs {
    async fn run(self, cli_context: &CliContext) -> anyhow::Result<()> {
        let project_path = match self.path {
            Some(path) => path,
            None => env::current_dir()?,
        };
        let mut project = Project::open(&project_path)?;
        project.set_target(self.target.lazy());
        let (name, constraint) = parse_dependency(&self.dependency)?;

        let index = cli_context.git_client()?;
        project.add_dependency(&index, name, constraint)?;
        if !self.no_install {
            install_locked(&mut project, &index).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dependency() {
        assert_eq!(parse_dependency("foo").unwrap(), ("foo".to_string(), None));
        for (input, constraint) in [
            ("foo@1.2", "^1.2"),
            ("foo@^1.2", "^1.2"),
            ("foo@~1.2.3", "~1.2.3"),
            ("foo@>=1.0, <2.0", ">=1.0, <2.0"),
        ] {
            let (name, parsed) = parse_dependency(input).unwrap();
            assert_eq!(name, "foo");
            assert_eq!(parsed.unwrap().to_string(), constraint);
        }
        assert!(parse_dependency("foo@latest").is_err());
    }
}

/// Removes a dependency from the current magnet project
#[derive(Debug, Parser)]
pub struct ProjectRemoveDependencyArgs {
    /// Path of the project.
    /// If not provided, the current working directory is used.
    #[arg(
        long,
        default_value = None,
    )]
    path: Option<PathBuf>,

    /// Only update magnet.toml and magnet.lock, don't install changed versions
    #[arg(long)]
    no_install: bool,

    #[command(flatten)]
    target: TargetArgs,

    dependency: String,
}

impl MagnetSubCommand for ProjectRemoveDependencyArgs {
    async fn run(self, cli_context: &CliContext) -> anyhow::Result<()> {
        let project_path = match self.path {
            Some(path) => path,
            None => env::current_dir()?,
        };
        let mut project = Project::open(&project_path)?;
        project.set_target(self.target.lazy());
        let index = cli_context.git_client()?;
        // removed packages are pruned right away, other versions only change if one vanished
        let changed = project.remove_dependency(&index, &self.dependency)?;
        if changed {
            match self.no_install {
                true => warn!(
                    "The versions of other dependencies changed, run `magnet install` to update them"
                ),
                false => install_locked(&mut project, &index).await?,
            }
        }
        Ok(())
    }
}

/// Moves locked dependencies to the newest versions their requirements allow
#[derive(Debug, Parser)]
pub struct ProjectUpdateArgs {
    /// Path of the project.
    /// If not provided, the current working directory is used.
    #[arg(
        long,
        default_value = None,
    )]
    path: Option<PathBuf>,

    /// Only update magnet.lock, don't install the new versions
    #[arg(long)]
    no_install: bool,

    #[command(flatten)]
    target: TargetArgs,

    /// Packages to update, all if none are given
    names: Vec<String>,
}

impl MagnetSubCommand for ProjectUpdateArgs {
    async fn run(self, cli_context: &CliContext) -> anyhow::Result<()> {
        let project_path = match self.path {
            Some(path) => path,
            None => env::current_dir()?,
        };
        let mut project = Project::open(&project_path)?;
        project.set_target(self.target.lazy());
        let index = cli_context.git_client()?;
        let changes = project.update(&index, &self.names)?;
        if changes.is_empty() {
            println!("All dependencies are up to date");
            return Ok(());
        }
        let version = |dependency: Option<LockDependency>| match dependency {
            Some(dependency) => dependency.label(),
            None => "-".to_string(),
        };
        for (name, old, new) in changes {
            println!("{name} {} -> {}", version(old), version(new));
        }
        match self.no_install {
            true => println!("Run `magnet install` to install the new versions"),
            false => install_locked(&mut project, &index).await?,
        }
        Ok(())
    }
}

/// Installs the locked dependencies of the current magnet project
#[derive(Debug, Parser)]
pub struct ProjectInstallArgs {
    /// Path of the project.
    /// If not provided, the current working directory is used.
    #[arg(
        long,
        default_value = None,
    )]
    path: Option<PathBuf>,

    /// Fail instead of writing magnet.lock, e.g. in CI
    #[arg(long)]
    locked: bool,

    #[command(flatten)]
    target: TargetArgs,
}

impl MagnetSubCommand for ProjectInstallArgs {
    async fn run(self, cli_context: &CliContext) -> anyhow::Result<()> {
        let project_path = match self.path {
            Some(path) => path,
            None => env::current_dir()?,
        };
        let mut project = Project::open(&project_path)?;
        project.set_target(self.target.lazy());
        let index = cli_context.git_client()?;
        project
            .install(&index, &downloader(&index)?, self.locked)
            .await
    }
}

fn downloader(index: &GitClient) -> anyhow::Result<PackageDownloader> {
    Ok(PackageDownloader::new(
        magnet_home()?.join(CACHE_DIR),
        index.config()?.dl,
    ))
}

/// Installs the lock file which `add`, `remove` or `update` just wrote
async fn install_locked(project: &mut Project, index: &GitClient) -> anyhow::Result<()> {
    project
        .install_locked(index, &downloader(index)?, false)
        .await
        .context("magnet.toml and magnet.lock are updated, but installing failed. Run `magnet install` to try again")
}

/// Runs an entrypoint of the current magnet project
#[derive(Debug, Parser)]
pub struct ProjectRunArgs {
    /// Path of the project.
    /// If not provided, the current working directory is used.
    #[arg(
        long,
        default_value = None,
    )]
    path: Option<PathBuf>,

    /// Name of the entrypoint. If not provided, the entrypoints get listed.
    name: Option<String>,

    /// Arguments which are passed on to the entrypoint
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

impl MagnetSubCommand for ProjectRunArgs {
    async fn run(self, _cli_context: &CliContext) -> anyhow::Result<()> {
        let project_path = match self.path {
            Some(path) => path,
            None => env::current_dir()?,
        };
        let project = Project::open(&project_path)?;
        let Some(name) = self.name else {
            for entrypoint in project.entrypoints() {
                println!(
                    "{} ({}): {}",
                    entrypoint.name, entrypoint.kind, entrypoint.command
                );
            }
            return Ok(());
        };

        let status = project.run_entrypoint(&name, &self.args, Sclang::find)?;
        if !status.success() {
            // exit with the code of the entrypoint, so scripts can rely on it
            std::process::exit(status.code().unwrap_or(1));
        }
        Ok(())
    }
}

/// Packages the current magnet project into `dist/`
#[derive(Debug, Parser)]
pub struct ProjectPackageArgs {
    /// Path of the project, the current directory by default
    #[arg(
        long,
        default_value = None,
    )]
    path: Option<PathBuf>,

    #[command(flatten)]
    target: TargetArgs,
}

/// The machine whose lepton binaries get packaged or installed, by default detected
#[derive(Debug, Args)]
pub struct TargetArgs {
    /// Platform tag of the binaries, e.g. linux_x86_64, detected by default
    #[arg(long, env = MAGNET_PLATFORM_ENV)]
    platform: Option<String>,

    /// abi tag of the binaries, e.g. api3_0, derived from the version of the found sclang by default
    #[arg(long, env = MAGNET_ABI_ENV)]
    abi: Option<String>,
}

impl TargetArgs {
    fn lazy(&self) -> LazyTarget {
        LazyTarget::new(self.platform.clone(), self.abi.clone())
    }
}

impl MagnetSubCommand for ProjectPackageArgs {
    async fn run(self, _cli_context: &CliContext) -> anyhow::Result<()> {
        let project_path = match self.path {
            Some(path) => path,
            None => env::current_dir()?,
        };
        let mut project = Project::open(&project_path)?;
        project.set_target(self.target.lazy());
        let archives = project.create_packages()?;
        for archive in &archives {
            let path = archive
                .path
                .strip_prefix(&project_path)
                .unwrap_or(&archive.path);
            println!("{} ({} files)", path.display(), archive.files.len());
            for file in &archive.files {
                println!("  {file}");
            }
        }
        if *project.kind() == MagnetManifestV1PackageKind::Gluon {
            warn!("The binaries of a gluon can't be packaged yet, only its source package");
        }
        println!(
            "Check these files before publishing, a published artifact can never be replaced."
        );
        Ok(())
    }
}
