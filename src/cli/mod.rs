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
    async fn run(
        self,
        git_client: GitClient,
        http_client: MagnetClient<NoAuth>,
    ) -> anyhow::Result<()>;
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

pub(crate) async fn cli() -> anyhow::Result<()> {
    let cli = Cli::parse();

    logging::init_tracing(cli.global.verbosity)?;

    // @todo turn these and args into a struct which then can be passed such that
    // the the contract can be modifed w/o touching the signature
    let http_client = MagnetClient::<NoAuth>::new(cli.global.index_url.clone());

    let git_client = GitClient::new(
        cli.global.git_index_url.clone(),
        env::home_dir().unwrap().join(".magnet").join("index"),
    )?;

    match cli.command {
        Commands::Init(init_args) => init_args.run(git_client, http_client).await,
        Commands::Add(project_add_dependency_args) => {
            project_add_dependency_args
                .run(git_client, http_client)
                .await
        }
        Commands::Bundle(project_bundle_args) => {
            project_bundle_args.run(git_client, http_client).await
        }
    }
}
