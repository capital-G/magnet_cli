use std::{
    collections::{BTreeMap, HashMap},
    fmt::Display,
    hash::BuildHasher,
    path::PathBuf,
    str::FromStr,
};

use anyhow::{bail, Context};
use pubgrub::SemanticVersion;
use serde::{Deserialize, Serialize};
use toml_edit::DocumentMut;
use tracing::{error, instrument, trace};
use url::Url;

use crate::git_client::models::{
    deserialize_semantic_version, serialize_semantic_version, GitRelease,
};
use crate::models::project_manifest::{MagnetManifestV1, MagnetManifestV1PackageKind};
use crate::resolver::online_provider::ROOT_PACKAGE;

pub const LOCK_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct LockFile {
    pub lock_version: u32,
    /// URL of the git index the dependencies were resolved against
    pub index: Url,
    /// keyed by the normalized package name - a BTreeMap keeps the entries
    /// sorted, so diffs of the lock file stay stable
    #[serde(default)]
    pub dependencies: BTreeMap<String, LockDependency>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockDependency {
    #[serde(
        deserialize_with = "deserialize_semantic_version",
        serialize_with = "serialize_semantic_version"
    )]
    pub version: SemanticVersion,
    /// determines the folder within `external`, so no index lookup is necessary on install
    pub kind: MagnetManifestV1PackageKind,
    pub source: LockSource,
    /// sha256 of every artifact of this version, keyed by `{abi}-{platform}`,
    /// so the same lock file can be installed on every platform.
    /// Empty for git (pinned by its commit) and path sources.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checksums: BTreeMap<String, String>,
}

/// Serialized as `index`, `git+<url>#<sha>` or a path
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum LockSource {
    Index,
    Git { url: Url, sha: String },
    Path(PathBuf),
}

impl Display for LockSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockSource::Index => write!(f, "index"),
            LockSource::Git { url, sha } => write!(f, "git+{url}#{sha}"),
            LockSource::Path(path) => write!(f, "{}", path.display()),
        }
    }
}

impl FromStr for LockSource {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "index" {
            return Ok(LockSource::Index);
        }
        if let Some(git) = value.strip_prefix("git+") {
            let (url, sha) = git
                .rsplit_once('#')
                .with_context(|| format!("git source {value} is missing a #<sha>"))?;
            return Ok(LockSource::Git {
                url: url.parse()?,
                sha: sha.to_string(),
            });
        }
        Ok(LockSource::Path(PathBuf::from(value)))
    }
}

impl TryFrom<String> for LockSource {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<LockSource> for String {
    fn from(value: LockSource) -> Self {
        value.to_string()
    }
}

impl From<&GitRelease> for LockDependency {
    fn from(release: &GitRelease) -> Self {
        Self {
            version: release.version,
            kind: release.kind.clone(),
            source: LockSource::Index,
            checksums: release
                .artifacts
                .iter()
                .map(|artifact| (artifact.key(), artifact.sha256.clone()))
                .collect(),
        }
    }
}

impl LockFile {
    #[instrument]
    pub fn create_empty(path: PathBuf, index: Url) -> anyhow::Result<Self> {
        let lock_file = Self {
            lock_version: LOCK_VERSION,
            index,
            dependencies: BTreeMap::new(),
        };
        lock_file.write(&path)?;
        trace!("Init lock file");
        Ok(lock_file)
    }

    #[instrument]
    pub fn open(path: &PathBuf) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .context(format!("Could not open lock file {:?}", path))?;
        Self::from_string(&content)
    }

    fn from_string(content: &str) -> anyhow::Result<Self> {
        let lock_file: Self = match toml::from_str(content) {
            Ok(lock_file) => lock_file,
            Err(err) => {
                error!(?err, "Failed to parse lock-file");
                return Err(err.into());
            }
        };
        if lock_file.lock_version != LOCK_VERSION {
            bail!(
                "Unsupported lock-version {} (this magnet supports {LOCK_VERSION})",
                lock_file.lock_version
            );
        }
        Ok(lock_file)
    }

    /// Creates the lock file from a resolved tree.
    /// `lookup` provides the details of a resolved package version from the index.
    pub fn from_tree<S: BuildHasher>(
        index: Url,
        full_tree: &HashMap<String, SemanticVersion, S>,
        lookup: impl Fn(&str, &SemanticVersion) -> anyhow::Result<LockDependency>,
    ) -> anyhow::Result<Self> {
        let dependencies = full_tree
            .iter()
            .filter(|(name, _)| name.as_str() != ROOT_PACKAGE)
            .map(|(name, version)| {
                Ok((
                    MagnetManifestV1::normalize_name(name),
                    lookup(name, version)?,
                ))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            lock_version: LOCK_VERSION,
            index,
            dependencies,
        })
    }

    fn to_string(&self) -> anyhow::Result<String> {
        let mut doc: DocumentMut = toml::to_string(self)?.parse()?;
        // checksums are written inline, so each dependency stays a single table
        if let Some(dependencies) = doc["dependencies"].as_table_mut() {
            dependencies.set_implicit(false);
            for (_, dependency) in dependencies.iter_mut() {
                let Some(dependency) = dependency.as_table_mut() else {
                    continue;
                };
                // re-insert the key, otherwise it keeps the formatting of a table header
                if let Some(toml_edit::Item::Table(checksums)) = dependency.remove("checksums") {
                    dependency.insert("checksums", toml_edit::value(checksums.into_inline_table()));
                }
            }
        }
        Ok(doc.to_string())
    }

    #[instrument(skip(self))]
    pub fn write(&self, path: &PathBuf) -> anyhow::Result<()> {
        std::fs::write(path, self.to_string()?).context("Could not write lock file")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Url {
        "https://github.com/supercollider/magnet-index.git"
            .parse()
            .unwrap()
    }

    fn lookup(name: &str, version: &SemanticVersion) -> anyhow::Result<LockDependency> {
        let (source, checksums) = match name {
            "bar" => (
                LockSource::Git {
                    url: "https://github.com/foo/bar.git".parse().unwrap(),
                    sha: "1a2b3c".to_string(),
                },
                BTreeMap::new(),
            ),
            _ => (
                LockSource::Index,
                BTreeMap::from([("any-any".to_string(), "abc".to_string())]),
            ),
        };
        Ok(LockDependency {
            version: *version,
            kind: MagnetManifestV1PackageKind::Quark,
            source,
            checksums,
        })
    }

    #[test]
    fn test_round_trip() {
        let tree = HashMap::from([
            (ROOT_PACKAGE.to_string(), SemanticVersion::zero()),
            ("foo".to_string(), SemanticVersion::new(0, 1, 0)),
            ("bar".to_string(), SemanticVersion::new(0, 2, 0)),
            ("DDW_Common".to_string(), SemanticVersion::new(1, 2, 3)),
        ]);
        let lock_file = LockFile::from_tree(index(), &tree, lookup).unwrap();
        let content = lock_file.to_string().unwrap();

        assert_eq!(
            content,
            "lock-version = 1\n\
             index = \"https://github.com/supercollider/magnet-index.git\"\n\
             \n\
             [dependencies]\n\
             \n\
             [dependencies.bar]\n\
             version = \"0.2.0\"\n\
             kind = \"quark\"\n\
             source = \"git+https://github.com/foo/bar.git#1a2b3c\"\n\
             \n\
             [dependencies.ddw-common]\n\
             version = \"1.2.3\"\n\
             kind = \"quark\"\n\
             source = \"index\"\n\
             checksums = { any-any = \"abc\" }\n\
             \n\
             [dependencies.foo]\n\
             version = \"0.1.0\"\n\
             kind = \"quark\"\n\
             source = \"index\"\n\
             checksums = { any-any = \"abc\" }\n"
        );
        assert!(!content.contains(ROOT_PACKAGE));
        assert_eq!(LockFile::from_string(&content).unwrap(), lock_file);
    }

    #[test]
    fn test_from_git_release() {
        let release: GitRelease = serde_json::from_str(
            r#"{"name": "gamma", "version": "0.3.0", "kind": "lepton", "artifacts": [
                {"abi": "api3", "platform": "linux_x86_64", "sha256": "abc"},
                {"abi": "source", "platform": "any", "sha256": "def"}]}"#,
        )
        .unwrap();
        let dependency = LockDependency::from(&release);
        assert_eq!(dependency.version, SemanticVersion::new(0, 3, 0));
        assert_eq!(dependency.kind, MagnetManifestV1PackageKind::Lepton);
        assert_eq!(dependency.source, LockSource::Index);
        assert_eq!(
            dependency.checksums,
            BTreeMap::from([
                ("api3-linux_x86_64".to_string(), "abc".to_string()),
                ("source-any".to_string(), "def".to_string()),
            ])
        );
    }

    #[test]
    fn test_empty_round_trip() {
        let lock_file = LockFile::from_tree(index(), &HashMap::new(), lookup).unwrap();
        let content = lock_file.to_string().unwrap();
        assert_eq!(LockFile::from_string(&content).unwrap(), lock_file);
    }

    #[test]
    fn test_unknown_lock_version() {
        let content = format!("lock-version = 2\nindex = \"{}\"\n", index());
        assert!(LockFile::from_string(&content).is_err());
    }

    #[test]
    fn test_lock_source() {
        for source in [
            LockSource::Index,
            LockSource::Git {
                url: "https://github.com/foo/bar.git".parse().unwrap(),
                sha: "1a2b3c".to_string(),
            },
            LockSource::Path(PathBuf::from("../baz")),
        ] {
            assert_eq!(source.to_string().parse::<LockSource>().unwrap(), source);
        }
        assert!("git+https://github.com/foo/bar.git"
            .parse::<LockSource>()
            .is_err());
    }
}
