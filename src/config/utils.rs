use anyhow;
use std::{
    fs::{self},
    path::PathBuf,
};
use tracing::{error, info};

use crate::{config::constants::get_default_config_path, config::models::Config};

/// load config from disk
pub fn load_config(config_path: Option<PathBuf>) -> anyhow::Result<Config> {
    let path = match config_path {
        Some(path) => path,
        None => match search_config_in_default_paths() {
            Some(path) => path,
            None => create_empty_config_file()?,
        },
    };
    read_config(path)
}

fn read_config(path: PathBuf) -> anyhow::Result<Config> {
    let buf = std::fs::read_to_string(&path)?;
    let config: Config = toml::from_str(&buf)?;
    Ok(config)
}

fn search_config_in_default_paths() -> Option<PathBuf> {
    get_default_config_path()
        .ok()
        .filter(|config_path| config_path.exists())
}

fn create_empty_config_file() -> anyhow::Result<PathBuf> {
    info!("Create new config file");
    let config_path = get_default_config_path()?;

    // the directory may already exist, e.g. if the index has been cloned
    if let Some(config_dir) = config_path.parent() {
        fs::create_dir_all(config_dir)?
    }

    if config_path.exists() {
        error!("Config file already exists!");
    } else {
        let empty_config = Config::new();
        fs::write(&config_path, toml::to_string(&empty_config)?)?;
    }
    Ok(config_path)
}
