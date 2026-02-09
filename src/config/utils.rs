use std::{
    env,
    fs::{self, File},
    io::{BufReader, BufWriter},
    path::PathBuf,
};

use anyhow;
use tracing::{error, info};

use crate::{
    config::constants::{CONFIG_FILE_NAME, CONFIG_PATH},
    config::models::Config,
};

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
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let config: Config = serde_saphyr::from_reader(reader)?;
    Ok(config)
}

fn default_config_path() -> PathBuf {
    if let Some(home_path) = env::home_dir() {
        home_path.join(CONFIG_PATH).join(CONFIG_FILE_NAME)
    } else {
        todo!("Not implemented yet!")
    }
}

fn search_config_in_default_paths() -> Option<PathBuf> {
    if let Some(home_path) = env::home_dir() {
        let config_path = home_path.join(CONFIG_PATH).join(CONFIG_FILE_NAME);
        if config_path.exists() {
            return Some(config_path);
        }
    };
    None
}

fn create_empty_config_file() -> anyhow::Result<PathBuf> {
    info!("Create new config file");
    let config_path = default_config_path();

    if let Some(config_dir) = config_path.parent() {
        fs::create_dir(config_dir)?
    }

    if config_path.exists() {
        error!("Config file already exists!");
    } else {
        let empty_config = Config::new();
        let file = File::create(&config_path)?;
        let mut writer = BufWriter::new(file);
        let _ = serde_saphyr::to_io_writer(&mut writer, &empty_config);
    }
    Ok(config_path)
}
