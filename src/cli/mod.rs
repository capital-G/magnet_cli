mod commands;
mod logging;

use crate::cli::commands::auth::{LoginArgs, LogoutArgs};
use crate::cli::commands::project::{
    ProjectAddDependencyArgs, ProjectInitArgs, ProjectInstallArgs, ProjectPackageArgs,
    ProjectRemoveDependencyArgs, ProjectRunArgs, ProjectUpdateArgs,
};
use crate::cli::commands::publish::PublishArgs;
use crate::cli::commands::server_info::ServerInfoArgs;
use anyhow::Context;
use clap::{Parser, Subcommand};
use magnet::{
    config::constants::{magnet_home, INDEX_DIR, MAGNET_TOKEN_ENV},
    git_client::client::GitClient,
    http_client::{
        auth::credentials_path,
        basic::{AuthToken, MagnetClient, NoAuth},
    },
};
use std::path::PathBuf;
use url::Url;

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
    // @todo add the official index as default once it exists
    #[arg(long, env = "MAGNET_GIT_INDEX_URL", help = "Package git index to use")]
    pub git_index_url: Option<Url>,

    /// replaces the `http` mirror announced by the index, which gets written into the lock file
    #[arg(
        long,
        env = "MAGNET_HTTP_INDEX_URL",
        help = "Static HTTP mirror of the index"
    )]
    pub http_index_url: Option<Url>,

    /// replaces the `api` announced by the index, e.g. for a local test server
    #[arg(long, env = "MAGNET_API_URL", help = "URL of the magnet server")]
    pub api_url: Option<Url>,

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
    /// Removes a dependency from the current magnet project
    Remove(ProjectRemoveDependencyArgs),
    /// Installs the dependencies of the current magnet project into `external`
    Install(ProjectInstallArgs),
    /// Moves locked dependencies to the newest versions their requirements allow
    Update(ProjectUpdateArgs),
    /// Runs an entrypoint of the current magnet project
    Run(ProjectRunArgs),
    /// Packages the current magnet project into `dist/` for publishing
    Package(ProjectPackageArgs),
    /// Publishes packages of the current magnet project on the magnet server
    Publish(PublishArgs),
    /// Logs in to the magnet server of the index
    Login(LoginArgs),
    /// Logs out of the magnet server of the index
    Logout(LogoutArgs),
    /// Shows the magnet server of the index and its version
    ServerInfo(ServerInfoArgs),
}

pub struct CliContext {
    global_args: GlobalArgs,
}

impl CliContext {
    pub fn git_client(&self) -> anyhow::Result<GitClient> {
        let url =
            self.global_args.git_index_url.clone().context(
                "No git index configured, set MAGNET_GIT_INDEX_URL or pass --git-index-url",
            )?;
        let local_destination = magnet_home()?
            .join(INDEX_DIR)
            .join(GitClient::cache_dir_name(&url));
        Ok(GitClient::new(url, local_destination)?
            .with_http_url(self.global_args.http_index_url.clone()))
    }

    /// The magnet server: `--api-url`, otherwise the `api` of the index's `config.json`
    pub fn api_url(&self) -> anyhow::Result<Url> {
        if let Some(url) = &self.global_args.api_url {
            return Ok(url.clone());
        }
        self.git_client()?.config()?.api.context(
            "The index names no magnet server (`api` in config.json), set MAGNET_API_URL or pass --api-url",
        )
    }

    pub fn http_client(&self) -> anyhow::Result<MagnetClient<NoAuth>> {
        Ok(MagnetClient::<NoAuth>::new(self.api_url()?))
    }

    /// Where the login to the magnet server is stored
    pub fn credentials_path(&self, client: &MagnetClient<NoAuth>) -> anyhow::Result<PathBuf> {
        Ok(credentials_path(&magnet_home()?, &client.base_url))
    }

    /// A client which is logged in via `MAGNET_TOKEN` or `magnet login`
    pub async fn authenticated_client(&self) -> anyhow::Result<MagnetClient<AuthToken>> {
        let client = self.http_client()?;
        let path = self.credentials_path(&client)?;
        client
            .authenticate(std::env::var(MAGNET_TOKEN_ENV).ok(), &path)
            .await
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
        Commands::Remove(project_remove_dependency_args) => {
            project_remove_dependency_args.run(&cli_context).await
        }
        Commands::Install(project_install_args) => project_install_args.run(&cli_context).await,
        Commands::Update(project_update_args) => project_update_args.run(&cli_context).await,
        Commands::Run(project_run_args) => project_run_args.run(&cli_context).await,
        Commands::Package(project_package_args) => project_package_args.run(&cli_context).await,
        Commands::Publish(publish_args) => publish_args.run(&cli_context).await,
        Commands::Login(login_args) => login_args.run(&cli_context).await,
        Commands::Logout(logout_args) => logout_args.run(&cli_context).await,
        Commands::ServerInfo(server_info_args) => server_info_args.run(&cli_context).await,
    }
}
