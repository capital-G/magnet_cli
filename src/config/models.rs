use serde::{Deserialize, Serialize};

use crate::config::constants::MAGNET_CLI_VERSION;

/// Global config of Magnet CLI
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Semantic version string of the CLI version.
    /// Used to check if config is compatible at some point.
    pub version: String,
}

impl Config {
    pub fn new() -> Self {
        Self {
            version: MAGNET_CLI_VERSION.to_string(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}
