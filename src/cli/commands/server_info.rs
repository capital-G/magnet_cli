use clap::Parser;
use magnet::http_client::endpoints::ServerInfo;

use crate::cli::{CliContext, MagnetSubCommand};

/// Shows the magnet server of the index and its version
#[derive(Parser, Debug)]
pub struct ServerInfoArgs {}

impl MagnetSubCommand for ServerInfoArgs {
    async fn run(self, cli_context: &CliContext) -> anyhow::Result<()> {
        let client = cli_context.http_client()?;
        let info = client.call(ServerInfo).await?;
        println!("{} (version {})", client.base_url, info.version);
        Ok(())
    }
}
