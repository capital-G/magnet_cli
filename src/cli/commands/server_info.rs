use magnet::http_client::{
    basic::{MagnetClient, NoAuth},
    endpoints::ServerInfo,
};

use crate::cli::GlobalArgs;

pub async fn get_server_info(
    _global_args: &GlobalArgs,
    client: MagnetClient<NoAuth>,
) -> anyhow::Result<()> {
    let response = client.call(ServerInfo).await?;
    let pretty = serde_json::to_string_pretty(&response)?;
    println!("{pretty}");
    Ok(())
}
