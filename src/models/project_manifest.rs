use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

use anyhow::Context;
use pubgrub::SemanticVersion;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::{error, instrument, trace};
use url::Url;

use crate::git_client::models::{
    deserialize_semantic_version, serialize_semantic_version, GitDependency,
};

/// Translates to the `magnet.yaml` file which acts as meta-information
/// storage of the project. If the project is not a library, there should also
/// be a lock file included.
#[derive(Debug, Deserialize, Serialize)]
pub struct ProjectManifest {
    magnet_format: u32,
    pub name: String, // verify slug?
    #[serde(skip_serializing_if = "Option::is_none", default)]
    summary: Option<String>,
    // project_type: ProjectTypeEnum,
    #[serde(
        deserialize_with = "deserialize_semantic_version",
        serialize_with = "serialize_semantic_version"
    )]
    pub version: SemanticVersion,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    url: Option<Url>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    repo: Option<Url>,
    pub dependencies: Vec<GitDependency>,
}

impl ProjectManifest {
    pub fn new(
        name: String,
        // project_type: ProjectTypeEnum,
        summary: Option<String>,
        license: Option<String>,
        url: Option<Url>,
        repo: Option<Url>,
    ) -> Self {
        ProjectManifest {
            magnet_format: 1,
            name,
            // project_type,
            dependencies: vec![],
            version: SemanticVersion::new(0, 1, 0),
            license,
            url,
            repo,
            summary,
        }
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
            assert!(ProjectManifest::is_valid_name(name))
        }

        for name in ["foo@bar", "42hello", "_hello"] {
            assert!(!ProjectManifest::is_valid_name(name))
        }
    }
}
