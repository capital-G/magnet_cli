use clap::Parser;
use magnet::{
    config::constants::MAGNET_TOKEN_ENV,
    http_client::basic::{AuthToken, OAuthToken},
};
use tracing::{info, warn};

use crate::cli::{CliContext, MagnetSubCommand};

/// Logs in to the magnet server of the index via OAuth2
#[derive(Parser, Debug)]
pub struct LoginArgs {}

/// Logs out of the magnet server of the index by revoking the stored login
#[derive(Parser, Debug)]
pub struct LogoutArgs {}

impl MagnetSubCommand for LoginArgs {
    async fn run(self, cli_context: &CliContext) -> anyhow::Result<()> {
        if std::env::var_os(MAGNET_TOKEN_ENV).is_some() {
            warn!("{MAGNET_TOKEN_ENV} is set and takes precedence over this login");
        }
        let client = cli_context.http_client()?;
        let path = cli_context.credentials_path(&client)?;
        // a stored login is reused, and refreshed if it is expired
        if OAuthToken::load(&path).is_some() {
            match client.authenticate(None, &path).await {
                Ok(_) => {
                    info!("Already logged in to {}", client.base_url);
                    return Ok(());
                }
                Err(err) => warn!("{err:#}"),
            }
        }
        client.login(&path).await?;
        info!("Logged in to {}", client.base_url);
        Ok(())
    }
}

impl MagnetSubCommand for LogoutArgs {
    async fn run(self, cli_context: &CliContext) -> anyhow::Result<()> {
        let client = cli_context.http_client()?;
        let path = cli_context.credentials_path(&client)?;
        let Some(token) = OAuthToken::load(&path) else {
            info!("Not logged in to {}", client.base_url);
            return Ok(());
        };
        client
            .with_state(AuthToken::Login { token, path })
            .logout()
            .await?;
        info!("Logged out of {}", client.base_url);
        if std::env::var_os(MAGNET_TOKEN_ENV).is_some() {
            warn!("{MAGNET_TOKEN_ENV} is still set and gets used instead of a login");
        }
        Ok(())
    }
}
