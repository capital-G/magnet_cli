use std::{collections::HashMap, fs::File, hash::BuildHasher, path::PathBuf};

use anyhow::Context;
use pubgrub::SemanticVersion;
use serde::{Deserialize, Serialize};
use serde_json::to_writer_pretty;
use tracing::{error, instrument, trace};

use crate::git_client::models::LockDependency;
use crate::models::project_manifest::MagnetManifestV1;

#[derive(Serialize, Deserialize)]
pub struct LockFile {
    dependencies: Vec<LockDependency>,
}

impl LockFile {
    #[instrument]
    pub fn create_empty(path: PathBuf) -> anyhow::Result<Self> {
        let lock_file = Self {
            dependencies: vec![],
        };
        lock_file.write(&path)?;
        trace!("Init lock file");
        Ok(lock_file)
    }

    #[instrument]
    pub fn open(path: &PathBuf) -> anyhow::Result<Self> {
        let file = File::open(path).context(format!("Could not open lock file {:?}", path))?;

        match serde_json::from_reader(file) {
            Ok(lock_content) => Ok(lock_content),
            Err(err) => {
                error!(?err, "Failed to parse lock-file");
                Err(err.into())
            }
        }
    }

    pub fn from_tree<S: BuildHasher>(
        full_tree: &HashMap<String, SemanticVersion, S>,
    ) -> anyhow::Result<Self> {
        let dependencies: Vec<LockDependency> = full_tree
            .iter()
            .map(|(name, version)| LockDependency {
                name: MagnetManifestV1::normalize_name(name),
                version: *version,
            })
            .collect();
        Ok(Self { dependencies })
    }

    #[instrument(skip(self))]
    pub fn write(&self, path: &PathBuf) -> anyhow::Result<()> {
        let file = File::create(path)?;
        to_writer_pretty(file, &self)?;
        Ok(())
    }
}
