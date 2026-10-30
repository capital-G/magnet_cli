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
use crate::git_client::source::GitReference;
use crate::models::package::ArtifactKind;
use crate::models::project_manifest::MagnetManifestV1;
use crate::resolver::online_provider::{ROOT_PACKAGE, SCLANG_PACKAGE};

pub const LOCK_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct LockFile {
    pub lock_version: u32,
    /// URL of the git index the dependencies were resolved against
    pub index: Url,
    /// static HTTP mirror of the index (its `config.json`'s `http`), for clients without git
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_http: Option<Url>,
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
    /// the kinds of artifacts of this version (DESIGN.md "Artifact kinds"), so installing
    /// knows which artifacts it needs without an index lookup, also when a binary is missing
    pub kinds: Vec<ArtifactKind>,
    pub source: LockSource,
    /// sha256 of every artifact of this version, keyed by `{kind}-{abi}-{platform}`
    /// ([ArtifactKey](crate::models::package::ArtifactKey)), so the same lock file can be
    /// installed on every platform.
    /// Empty for git (pinned by its commit) and path sources.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checksums: BTreeMap<String, String>,
}

/// Serialized as `index`, `git+<url>?<reference>#<sha>` (e.g. `?tag=v1`, nothing for the
/// default branch) or a path relative to the project
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum LockSource {
    Index,
    Git {
        url: Url,
        reference: GitReference,
        sha: String,
    },
    Path(PathBuf),
}

impl Display for LockSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockSource::Index => write!(f, "index"),
            LockSource::Git {
                url,
                reference,
                sha,
            } => write!(f, "git+{}#{sha}", reference.to_url(url)),
            LockSource::Path(path) => write!(f, "{}", path.display()),
        }
    }
}

impl LockSource {
    /// `true` for a git source of the commit `sha` of `url`, whatever reference led to it
    pub fn is_commit(&self, url: &Url, sha: &str) -> bool {
        matches!(self, LockSource::Git { url: own, sha: own_sha, .. } if own == url && own_sha == sha)
    }

    /// `true` if both are the same package: the same commit of the same repository (also if
    /// one was given as tag and the other as `ref`), or otherwise equal
    pub fn same_as(&self, other: &LockSource) -> bool {
        match other {
            LockSource::Git { url, sha, .. } => self.is_commit(url, sha),
            _ => self == other,
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
            let (url, reference) = GitReference::from_url(url.parse()?);
            return Ok(LockSource::Git {
                url,
                reference,
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
            kinds: release.kinds(),
            source: LockSource::Index,
            // artifacts of unknown kinds can't be installed, so they're left out
            checksums: release
                .artifacts
                .iter()
                .filter_map(|artifact| {
                    Some((
                        artifact.artifact_key()?.to_string(),
                        artifact.sha256.clone(),
                    ))
                })
                .collect(),
        }
    }
}

impl LockDependency {
    /// The version, and the short commit of a git source, e.g. `1.2.0 (git 4b825dc)`
    pub fn label(&self) -> String {
        match &self.source {
            LockSource::Git { sha, .. } => {
                format!("{} (git {})", self.version, &sha[..sha.len().min(7)])
            }
            _ => self.version.to_string(),
        }
    }
}

impl LockFile {
    #[instrument]
    pub fn create_empty(path: PathBuf, index: Url) -> anyhow::Result<Self> {
        let lock_file = Self {
            lock_version: LOCK_VERSION,
            index,
            index_http: None,
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
        index_http: Option<Url>,
        full_tree: &HashMap<String, SemanticVersion, S>,
        lookup: impl Fn(&str, &SemanticVersion) -> anyhow::Result<LockDependency>,
    ) -> anyhow::Result<Self> {
        let dependencies = full_tree
            .iter()
            .filter(|(name, _)| ![ROOT_PACKAGE, SCLANG_PACKAGE].contains(&name.as_str()))
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
            index_http,
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
                    reference: GitReference::Tag("v1".to_string()),
                    sha: "1a2b3c".to_string(),
                },
                BTreeMap::new(),
            ),
            _ => (
                LockSource::Index,
                BTreeMap::from([("quark-any-any".to_string(), "abc".to_string())]),
            ),
        };
        Ok(LockDependency {
            version: *version,
            kinds: vec![ArtifactKind::Quark],
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
            (SCLANG_PACKAGE.to_string(), SemanticVersion::new(3, 13, 0)),
        ]);
        let lock_file = LockFile::from_tree(
            index(),
            Some("https://magnet.example/index/".parse().unwrap()),
            &tree,
            lookup,
        )
        .unwrap();
        let content = lock_file.to_string().unwrap();

        assert_eq!(
            content,
            "lock-version = 1\n\
             index = \"https://github.com/supercollider/magnet-index.git\"\n\
             index-http = \"https://magnet.example/index/\"\n\
             \n\
             [dependencies]\n\
             \n\
             [dependencies.bar]\n\
             version = \"0.2.0\"\n\
             kinds = [\"quark\"]\n\
             source = \"git+https://github.com/foo/bar.git?tag=v1#1a2b3c\"\n\
             \n\
             [dependencies.ddw-common]\n\
             version = \"1.2.3\"\n\
             kinds = [\"quark\"]\n\
             source = \"index\"\n\
             checksums = { quark-any-any = \"abc\" }\n\
             \n\
             [dependencies.foo]\n\
             version = \"0.1.0\"\n\
             kinds = [\"quark\"]\n\
             source = \"index\"\n\
             checksums = { quark-any-any = \"abc\" }\n"
        );
        assert!(!content.contains(ROOT_PACKAGE));
        assert!(!content.contains(SCLANG_PACKAGE));
        assert_eq!(LockFile::from_string(&content).unwrap(), lock_file);
    }

    #[test]
    fn test_from_git_release() {
        let release: GitRelease = serde_json::from_str(
            r#"{"name": "gamma", "version": "0.3.0", "kinds": ["quark", "lepton"], "artifacts": [
                {"kind": "source", "abi": "any", "platform": "any", "sha256": "def"},
                {"kind": "quark", "abi": "any", "platform": "any", "sha256": "ghi"},
                {"kind": "lepton", "abi": "api3_0", "platform": "linux_x86_64", "sha256": "abc"},
                {"kind": "gluon", "abi": "lang1_0", "platform": "linux_x86_64", "sha256": "jkl"}]}"#,
        )
        .unwrap();
        let dependency = LockDependency::from(&release);
        assert_eq!(dependency.version, SemanticVersion::new(0, 3, 0));
        assert_eq!(
            dependency.kinds,
            [ArtifactKind::Quark, ArtifactKind::Lepton]
        );
        assert_eq!(dependency.source, LockSource::Index);
        // the artifact of an unknown kind is left out
        assert_eq!(
            dependency.checksums,
            BTreeMap::from([
                ("lepton-api3_0-linux_x86_64".to_string(), "abc".to_string()),
                ("quark-any-any".to_string(), "ghi".to_string()),
                ("source-any-any".to_string(), "def".to_string()),
            ])
        );
    }

    #[test]
    fn test_empty_round_trip() {
        let lock_file = LockFile::from_tree(index(), None, &HashMap::new(), lookup).unwrap();
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
                reference: GitReference::DefaultBranch,
                sha: "1a2b3c".to_string(),
            },
            LockSource::Git {
                url: "https://github.com/foo/bar.git".parse().unwrap(),
                reference: GitReference::Branch("main".to_string()),
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
