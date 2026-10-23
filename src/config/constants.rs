use std::{env, path::PathBuf};

use anyhow::Context;

pub const CONFIG_FILE_NAME: &str = "config.toml";
pub const CONFIG_PATH: &str = ".magnet";

pub const MAGNET_PROJECT_FILE: &str = "magnet.toml";
pub const MAGNET_LOCK_FILE: &str = "magnet.lock";

// client config
pub const OAUTH_APP_PUBLIC_KEY: &str = "8bMJG5cTIk5gdINwQWbYHb0NAflBSr6dw57mANDn";
pub const OAUTH_TOKEN_FILE_NAME: &str = ".token.json";

pub const CLIENT_NAME: &str = "magnet_cli";

pub const MAGNET_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn get_default_config_path() -> anyhow::Result<PathBuf> {
    Ok(env::home_dir()
        .with_context(|| "Could not obtain home directory")?
        .join(CONFIG_PATH)
        .join(CONFIG_FILE_NAME))
}
