mod commands;
mod logging;

use crate::cli::commands::project::{ProjectAddDependencyArgs, ProjectBundleArgs, ProjectInitArgs};
use clap::{Parser, Subcommand};
use magnet::{
    git_client::client::GitClient,
    http_client::basic::{MagnetClient, NoAuth},
};
use url::Url;

use std::env;

pub trait MagnetSubCommand {
    async fn run(self, ctx: &CliContext) -> anyhow::Result<()>;
}

#[derive(Parser)]
#[command(name = "Magnet", version, about = "Magnet Package Manager")]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Parser)]
struct GlobalArgs {
    /// URL of the Index
    #[arg(
        long,
        env = "MAGNET_INDEX_URL",
        default_value = "http://127.0.0.1:8000",
        help = "Package index to use"
    )]
    pub index_url: Url,

    #[arg(
        long,
        env = "MAGNET_GIT_INDEX_URL",
        default_value = "https://github.com/capital-G/slurm_rave.git",
        help = "Package git index to use"
    )]
    pub git_index_url: Url,

    // @todo make magnet config path configurable
    /// Increase verbosity level of log
    #[arg(
        short,
        long("verbose"),
        action = clap::ArgAction::Count
    )]
    pub verbosity: u8,
}

#[derive(Subcommand)]
enum Commands {
    /// Initiates a new magnet project
    Init(ProjectInitArgs),
    /// Adds a new dependency to the current magnet project
    Add(ProjectAddDependencyArgs),
    Bundle(ProjectBundleArgs),
}

pub struct CliContext {
    global_args: GlobalArgs,
}

impl CliContext {
    pub fn git_client(&self) -> anyhow::Result<GitClient> {
        GitClient::new(
            self.global_args.git_index_url.clone(),
            env::home_dir().unwrap().join(".magnet").join("index"),
        )
    }
    pub fn http_client(&self) -> MagnetClient<NoAuth> {
        MagnetClient::<NoAuth>::new(self.global_args.index_url.clone())
    }
}

pub(crate) async fn cli() -> anyhow::Result<()> {
    let cli = Cli::parse();

    logging::init_tracing(cli.global.verbosity)?;

    let cli_context = CliContext {
        global_args: cli.global,
    };

    match cli.command {
        Commands::Init(init_args) => init_args.run(&cli_context).await,
        Commands::Add(project_add_dependency_args) => {
            project_add_dependency_args.run(&cli_context).await
        }
        Commands::Bundle(project_bundle_args) => project_bundle_args.run(&cli_context).await,
    }
}
