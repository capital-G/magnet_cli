use std::{env, path::PathBuf};

use anyhow::Context;

pub const CONFIG_FILE_NAME: &str = "config.toml";
pub const CONFIG_PATH: &str = ".magnet";
/// Relocates the magnet home directory, e.g. for CI or tests
pub const MAGNET_HOME_ENV: &str = "MAGNET_HOME";
/// clone of the git index within the magnet home directory
pub const INDEX_DIR: &str = "index";
/// downloaded artifacts within the magnet home directory
pub const CACHE_DIR: &str = "cache";

pub const MAGNET_PROJECT_FILE: &str = "magnet.toml";
pub const MAGNET_LOCK_FILE: &str = "magnet.lock";

// client config
pub const OAUTH_APP_PUBLIC_KEY: &str = "8bMJG5cTIk5gdINwQWbYHb0NAflBSr6dw57mANDn";
pub const OAUTH_TOKEN_FILE_NAME: &str = ".token.json";

pub const CLIENT_NAME: &str = "magnet_cli";

pub const MAGNET_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `~/.magnet` (or `$MAGNET_HOME`), which holds the config, the index and the download cache
pub fn magnet_home() -> anyhow::Result<PathBuf> {
    if let Some(path) = env::var_os(MAGNET_HOME_ENV) {
        return Ok(PathBuf::from(path));
    }
    Ok(env::home_dir()
        .with_context(|| "Could not obtain home directory")?
        .join(CONFIG_PATH))
}

pub fn get_default_config_path() -> anyhow::Result<PathBuf> {
    Ok(magnet_home()?.join(CONFIG_FILE_NAME))
}
