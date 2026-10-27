use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use crate::cli::{CliContext, MagnetSubCommand};
use anyhow::{ensure, Context};
use clap::{arg, Args, Parser};
use inquire::{
    validator::{self, Validation},
    Confirm, CustomUserError, Select, Text,
};
use magnet::{
    config::constants::{magnet_home, CACHE_DIR, MAGNET_PROJECT_FILE},
    git_client::client::GitClient,
    models::{
        directories::ProjectDirectory,
        gitignore::{missing_gitignore_entries, GITIGNORE_FILE},
        lock_file::LockDependency,
        project::{CreateOptions, Project},
        project_manifest::{
            MagnetManifestV1, MagnetManifestV1Metadata, MagnetManifestV1MetadataAuthorsItem,
            MagnetManifestV1PackageKind, ProjectLayout,
        },
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
    /// Path of the project, created if it doesn't exist.
    /// If not provided, magnet asks whether to create a new folder with the name
    /// of the project or to use the current working directory.
    #[arg(
        long,
        default_value = None,
    )]
    path: Option<PathBuf>,
    // further commands are not available via command line b/c
    // projects will only be created through users and not automated,
    // so we don't have to maintain/implement two paths.
}

/// Where `magnet init` creates the project if no `--path` is given
#[derive(Debug, Clone, Copy, PartialEq)]
enum InitLocation {
    NewFolder,
    CurrentDirectory,
}

impl std::fmt::Display for InitLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InitLocation::NewFolder => write!(f, "In a new folder named after the project"),
            InitLocation::CurrentDirectory => write!(f, "In the current directory"),
        }
    }
}

/// Fails if `path` already holds a magnet project, or if it should be a new folder
/// but exists with content
pub(super) fn check_project_dir(path: &Path, new_folder: bool) -> anyhow::Result<()> {
    ensure!(
        !path.join(MAGNET_PROJECT_FILE).exists(),
        "{path:?} already contains a {MAGNET_PROJECT_FILE}"
    );
    if new_folder && path.exists() {
        let mut entries = fs::read_dir(path).with_context(|| format!("Could not read {path:?}"))?;
        ensure!(
            entries.next().is_none(),
            "{path:?} already exists and isn't empty"
        );
    }
    Ok(())
}

/// The folder name of `dir` if it's a valid package name, as default name of a project
/// created in it
fn default_project_name(dir: &Path) -> Option<String> {
    dir.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| MagnetManifestV1::is_valid_name(name))
}

impl MagnetSubCommand for ProjectInitArgs {
    async fn run(self, _ctx: &CliContext) -> anyhow::Result<()> {
        let current_dir =
            env::current_dir().context("Could not obtain current working directory")?;
        // `None` means a new folder within the current directory, named after the project
        let fixed_path = match self.path {
            Some(path) => Some(path),
            None => match Select::new(
                "Where should the project be created?",
                vec![InitLocation::NewFolder, InitLocation::CurrentDirectory],
            )
            .prompt()?
            {
                InitLocation::NewFolder => None,
                InitLocation::CurrentDirectory => Some(current_dir.clone()),
            },
        };
        if let Some(path) = &fixed_path {
            check_project_dir(path, false)?;
        }

        let default_name = fixed_path
            .as_deref()
            .map(|path| std::path::absolute(path).unwrap_or(path.to_path_buf()))
            .and_then(|path| default_project_name(&path));
        let project_name = ask_project_name(default_name.as_deref())?;

        let project_path = match fixed_path {
            Some(path) => path,
            None => {
                let path = current_dir.join(&project_name);
                check_project_dir(&path, true)?;
                path
            }
        };
        trace!(?project_path, "Use path");

        let project_manifest = ask_manifest(project_name, &InitDefaults::default())?;
        let options = ask_create_options(&project_path, ProjectLayout::Magnet)?;
        create_project(&project_path, project_manifest, options)
    }
}

/// Suggested answers of the init dialog, e.g. from a legacy `.quark` file
#[derive(Debug, Default)]
pub(super) struct InitDefaults {
    /// a fixed kind, which isn't asked (a legacy quark is always a quark)
    pub kind: Option<MagnetManifestV1PackageKind>,
    pub version: Option<SemanticVersion>,
    pub summary: Option<String>,
    /// comma separated, like the answer
    pub authors: Option<String>,
    pub license: Option<String>,
    pub repository: Option<String>,
}

pub(super) fn ask_project_name(default: Option<&str>) -> anyhow::Result<String> {
    let mut name_prompt = Text::new("Name of the project:");
    if let Some(name) = default {
        name_prompt = name_prompt.with_default(name);
    }
    Ok(name_prompt
        .with_validator(validator::ValueRequiredValidator::default())
        .with_validator(
            |input: &str| -> Result<Validation, Box<dyn Error + Send + Sync>> {
                match MagnetManifestV1::is_valid_name(input) {
                    true => Ok(Validation::Valid),
                    false => Ok(Validation::Invalid(
                        "Project name can only consist of alphanumeric characters and -_".into(),
                    )),
                }
            },
        )
        .prompt()?)
}

/// Asks for the kind, version, sclang range and metadata of a new project, every
/// question can be skipped with Enter
pub(super) fn ask_manifest(
    project_name: String,
    defaults: &InitDefaults,
) -> anyhow::Result<MagnetManifestV1> {
    let project_kind = match &defaults.kind {
        Some(kind) => kind.clone(),
        None => Select::new(
            "Project kind:",
            vec![
                MagnetManifestV1PackageKind::Quark,
                // MagnetManifestV1PackageKind::Gluon,
                MagnetManifestV1PackageKind::Lepton,
            ],
        )
        .prompt()?,
    };

    let default_version = defaults
        .version
        .unwrap_or(SemanticVersion::new(0, 1, 0))
        .to_string();
    let version = Text::new("Version:")
        .with_default(&default_version)
        .with_validator(|input: &str| -> Result<Validation, CustomUserError> {
            Ok(match SemanticVersion::from_str(input) {
                Ok(_) => Validation::Valid,
                Err(_) => Validation::Invalid("Expected a version like 0.1.0".into()),
            })
        })
        .prompt()?;
    let version = SemanticVersion::from_str(&version)
        .map_err(|err| anyhow::anyhow!("Invalid version {version}: {err}"))?;

    // prefilled instead of defaults, so the author can remove them
    let default_sclang = default_sclang_requirement().unwrap_or_default();
    let sclang = Text::new("Supported SuperCollider versions:")
        .with_initial_value(&default_sclang)
        .with_help_message("A version range, e.g. >=3.13, empty for any version")
        .with_validator(|input: &str| -> Result<Validation, CustomUserError> {
            Ok(
                match optional(input).map(|range| VersionReq::parse(&range)) {
                    Some(Err(err)) => Validation::Invalid(format!("{err}").into()),
                    _ => Validation::Valid,
                },
            )
        })
        .prompt()?;

    let summary = Text::new("Summary:")
        .with_initial_value(defaults.summary.as_deref().unwrap_or_default())
        .with_help_message("A single line, shown in lists and search results")
        .prompt()?;
    let description = Text::new("Description:").prompt()?;
    let default_authors = defaults
        .authors
        .clone()
        .or_else(git_author)
        .unwrap_or_default();
    let authors = Text::new("Authors:")
        .with_initial_value(&default_authors)
        .with_help_message("Comma separated, e.g. Jane Doe <jane@example.org>")
        .with_validator(|input: &str| -> Result<Validation, CustomUserError> {
            Ok(match parse_authors(input) {
                Ok(_) => Validation::Valid,
                Err(err) => Validation::Invalid(format!("{err}").into()),
            })
        })
        .prompt()?;
    let license = Text::new("License:")
        .with_initial_value(defaults.license.as_deref().unwrap_or_default())
        .with_help_message("An SPDX identifier, e.g. GPL-3.0-or-later like SuperCollider")
        .with_autocomplete(|input: &str| -> Result<Vec<String>, CustomUserError> {
            let input = input.to_lowercase();
            Ok(LICENSES
                .iter()
                .filter(|license| license.to_lowercase().contains(&input))
                .map(|license| license.to_string())
                .collect())
        })
        .prompt()?;
    let repository = Text::new("Repository URL:")
        .with_initial_value(defaults.repository.as_deref().unwrap_or_default())
        .with_help_message("Needed for publishing")
        .prompt()?;
    let tags = Text::new("Tags:")
        .with_help_message("Comma separated, e.g. synthesis, granular")
        .prompt()?;

    let metadata = MagnetManifestV1Metadata {
        authors: parse_authors(&authors)?,
        description: optional(&description),
        license: optional(&license),
        readme: None,
        repository: optional(&repository),
        summary: optional(&summary),
        tags: split_list(&tags),
    };
    MagnetManifestV1::new(
        project_name,
        version,
        optional(&sclang),
        project_kind,
        metadata,
    )
}

/// Asks which files besides the manifest get created. The folders are suggested for a new
/// project, but not for an existing legacy quark.
pub(super) fn ask_create_options(
    project_path: &Path,
    layout: ProjectLayout,
) -> anyhow::Result<CreateOptions> {
    let scaffold = ProjectDirectory::SCAFFOLD
        .map(|directory| format!("{}/", String::from(directory)))
        .join(", ");
    let directories = Confirm::new(&format!("Create the folders {scaffold}?"))
        .with_default(layout == ProjectLayout::Magnet)
        .prompt()?;

    // only asked if magnet would change the .gitignore, e.g. not in an existing project
    let ignored = missing_gitignore_entries(project_path)?;
    let gitignore = match ignored.is_empty() {
        true => false,
        false => {
            let question = match project_path.join(GITIGNORE_FILE).exists() {
                true => format!(
                    "Add {} to the existing {GITIGNORE_FILE}?",
                    ignored.join(", ")
                ),
                false => format!("Create a {GITIGNORE_FILE} ignoring {}?", ignored.join(", ")),
            };
            Confirm::new(&question).with_default(true).prompt()?
        }
    };
    Ok(CreateOptions {
        directories,
        gitignore,
        layout,
    })
}

pub(super) fn create_project(
    project_path: &Path,
    manifest: MagnetManifestV1,
    options: CreateOptions,
) -> anyhow::Result<()> {
    if !project_path.exists() {
        trace!(?project_path, "Create project directory");
        fs::create_dir_all(project_path).context("Could not create project directory")?;
    }
    let project = Project::create(project_path, manifest, options)?;
    // so the IDE can use the project right away, before the first install
    project.write_language_config()
}

/// Suggestions for the license of a new project, SuperCollider's own first
const LICENSES: [&str; 9] = [
    "GPL-3.0-or-later",
    "GPL-2.0-or-later",
    "LGPL-3.0-or-later",
    "MIT",
    "Apache-2.0",
    "BSD-3-Clause",
    "MPL-2.0",
    "CC0-1.0",
    "Unlicense",
];

/// `None` for an empty (skipped) answer
fn optional(input: &str) -> Option<String> {
    Some(input.trim().to_string()).filter(|input| !input.is_empty())
}

/// The non-empty entries of a comma separated answer
fn split_list(input: &str) -> Vec<String> {
    input.split(',').filter_map(optional).collect()
}

/// Parses comma separated authors like `Jane Doe <jane@example.org>, John`
fn parse_authors(input: &str) -> anyhow::Result<Vec<MagnetManifestV1MetadataAuthorsItem>> {
    split_list(input)
        .iter()
        .map(|author| {
            let (name, email) = match author.split_once('<') {
                Some((name, email)) => {
                    let email = email
                        .strip_suffix('>')
                        .with_context(|| format!("{author}: the email lacks a closing >"))?;
                    ensure!(
                        email.contains('@'),
                        "{author}: {email} is not an email address"
                    );
                    (name.trim(), Some(email.trim().to_string()))
                }
                None => (author.as_str(), None),
            };
            ensure!(!name.is_empty(), "{author}: the name is missing");
            Ok(MagnetManifestV1MetadataAuthorsItem {
                name: name.to_string(),
                email,
            })
        })
        .collect()
}

/// `user.name <user.email>` of the git config, as default author
fn git_author() -> Option<String> {
    let config = git2::Config::open_default().ok()?;
    let name = config.get_string("user.name").ok()?;
    Some(match config.get_string("user.email") {
        Ok(email) => format!("{name} <{email}>"),
        Err(_) => name,
    })
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
            warn!("Could not find sclang, so there is no default `sclang` range");
            return None;
        }
        Err(err) => {
            warn!("{err:#}, so there is no default `sclang` range");
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
        // a name may start with a digit, like the legacy quark 3Dj
        let (name, parsed) = parse_dependency("3Dj@^1").unwrap();
        assert_eq!(
            (name.as_str(), parsed.unwrap().to_string()),
            ("3Dj", "^1".to_string())
        );
    }

    #[test]
    fn test_check_project_dir() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(check_project_dir(&missing, true).is_ok());
        assert!(check_project_dir(&missing, false).is_ok());

        // an empty folder works both ways, a non-empty one only as the current directory
        assert!(check_project_dir(dir.path(), true).is_ok());
        fs::write(dir.path().join("README.md"), "").unwrap();
        assert!(check_project_dir(dir.path(), true).is_err());
        assert!(check_project_dir(dir.path(), false).is_ok());

        // an existing manifest never gets overwritten
        fs::write(dir.path().join(MAGNET_PROJECT_FILE), "").unwrap();
        assert!(check_project_dir(dir.path(), false).is_err());
    }

    #[test]
    fn test_init_answers() {
        assert_eq!(optional("  "), None);
        assert_eq!(optional(" MIT "), Some("MIT".to_string()));
        assert_eq!(split_list("synth, fx,, "), ["synth", "fx"]);
        assert!(split_list("").is_empty());

        assert!(parse_authors("").unwrap().is_empty());
        let authors = parse_authors("Jane Doe <jane@example.org>, John").unwrap();
        assert_eq!(
            authors
                .iter()
                .map(|author| (author.name.as_str(), author.email.as_deref()))
                .collect::<Vec<_>>(),
            [("Jane Doe", Some("jane@example.org")), ("John", None)]
        );
        for invalid in [
            "Jane <jane@example.org",
            "Jane <jane>",
            "<jane@example.org>",
        ] {
            assert!(parse_authors(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn test_default_project_name() {
        assert_eq!(
            default_project_name(Path::new("/home/me/MyQuark")).as_deref(),
            Some("MyQuark")
        );
        assert_eq!(default_project_name(Path::new("/home/me/my.quark")), None);
        assert_eq!(default_project_name(Path::new("/")), None);
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
