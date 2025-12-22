use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
    str::FromStr,
};

use anyhow::Context;
use pubgrub::SemanticVersion;
use regex::Regex;
use tracing::{error, instrument, trace};

include!(concat!(env!("OUT_DIR"), "/manifest_models.rs"));

impl TryFrom<MagnetManifestV1Version> for SemanticVersion {
    type Error = <SemanticVersion as FromStr>::Err;

    fn try_from(value: MagnetManifestV1Version) -> Result<Self, Self::Error> {
        SemanticVersion::from_str(&value)
    }
}

impl MagnetManifestV1 {
    pub fn new(
        name: String,
        description: Option<String>,
        license: Option<String>,
        version: SemanticVersion,
        min_sclang_version: Option<SemanticVersion>,
        project_kind: MagnetManifestV1ArtifactsItemKind,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            Self::is_valid_name(&name),
            format!("{} is not a valid project name!", name)
        );

        let min_sclang_version = match min_sclang_version {
            Some(version) => Some(MagnetManifestV1MinSclangVersion::from_str(
                &version.to_string(),
            )?),
            None => None,
        };
        let version = MagnetManifestV1Version::from_str(&version.to_string())?;

        let metadata = MagnetManifestV1Metadata {
            authors: vec![],
            description,
            license,
            readme: None,
            repository: None,
            tags: vec![],
        };

        Ok(MagnetManifestV1 {
            artifacts: vec![MagnetManifestV1ArtifactsItem {
                global_paths: vec![],
                kind: project_kind,
                platform_paths: None,
            }],
            dependencies: vec![],
            entrypoints: vec![],
            magnet: 1.0,
            metadata: Some(metadata),
            min_sclang_version,
            name,
            version,
        })
    }

    pub fn is_valid_name(name: &str) -> bool {
        let re = Regex::new(r"^[A-Za-z][A-Za-z0-9_-]*$").unwrap();
        re.is_match(name)
    }

    #[instrument(fields(path))]
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let file =
            File::open(path).context(format!("Could not open magnet project file {:?}", path))?;
        let value = serde_yaml::from_reader(file);
        match value {
            Ok(project) => Ok(project),
            Err(err) => {
                error!(?err, "Could not parse project file");
                Err(err.into())
            }
        }
    }

    #[instrument(skip(self))]
    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        let file = File::create(path).context("Could not create project file")?;
        let mut writer = BufWriter::new(file);
        serde_yaml::to_writer(&mut writer, self).context("Could not write project file")?;
        writer.flush()?;
        trace!("Init project file");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_project_name() {
        for name in ["foobar", "hello42", "hello-foo", "hello_bar"] {
            assert!(MagnetManifestV1::is_valid_name(name))
        }

        for name in ["foo@bar", "42hello", "_hello"] {
            assert!(!MagnetManifestV1::is_valid_name(name))
        }
    }
}
