use std::{fs::File, path::PathBuf};

use tracing::{debug, info, instrument, trace};

use crate::models::project::Project;

pub trait MagnetArchive {
    fn compress(
        &self,
        files: &Vec<PathBuf>,
        base_path: &PathBuf,
        target: &PathBuf,
    ) -> anyhow::Result<()>;
    fn extract(&self, source: &PathBuf, target: &PathBuf) -> anyhow::Result<()>;
}

impl MagnetArchive for Project {
    #[instrument(skip(self, files, base_path))]
    fn compress(
        &self,
        files: &Vec<PathBuf>,
        base_path: &PathBuf,
        target: &PathBuf,
    ) -> anyhow::Result<()> {
        trace!("Create archive");
        let tar_file = File::create(target)?;
        let encoder = flate2::write::GzEncoder::new(&tar_file, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);

        for path in files {
            let relative_path = path.strip_prefix(base_path)?;
            tar.append_path_with_name(path, relative_path)?;
        }

        tar.into_inner()?.finish()?;

        debug!("Created tar archive");

        Ok(())
    }

    fn extract(&self, source: &PathBuf, target: &PathBuf) -> anyhow::Result<()> {
        todo!()
    }
}
