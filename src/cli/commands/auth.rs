use clap::{command, Parser, Subcommand};
use magnet::http_client::basic::OAuthToken;
use tracing::{error, info};

use crate::cli::{CliContext, MagnetSubCommand};

#[derive(Parser, Debug)]
#[command(name = "auth", about = "Authentication on Magnet Package Index Server")]
pub struct AuthArgs {
    #[command(subcommand)]
    command: AuthCommand,
}

#[derive(Subcommand, Debug)]
pub enum AuthCommand {
    /// Login to the package index using OAuth2
    Login,
    /// Logout of the package index by removing local OAuth2 token
    Logout,
}

impl MagnetSubCommand for AuthArgs {
    async fn run(self, cli_context: &CliContext) -> anyhow::Result<()> {
        match self.command {
            AuthCommand::Login => match OAuthToken::local_token_path()?.exists() {
                true => {
                    info!("Already authenticated");
                    Ok(())
                }
                false => match cli_context
                    .http_client()
                    .authenticate(&OAuthToken::local_token_path()?)
                    .await
                {
                    Ok(_) => {
                        info!("Successfully authenticated");
                        anyhow::Ok(())
                    }
                    Err(err) => {
                        error!("Could not authenticate: {err:?}");
                        Err(err)
                    }
                },
            },
            AuthCommand::Logout => match OAuthToken::delete_token(&OAuthToken::local_token_path()?)
            {
                Ok(actually_deleted) => {
                    if actually_deleted {
                        info!("Logged out");
                    } else {
                        info!("Could not find existing auth token file - already logged out");
                    }
                    Ok(())
                }
                Err(err) => Err(err),
            },
        }
    }
}
