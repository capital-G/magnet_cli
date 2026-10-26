use std::collections::BTreeMap;

use pubgrub::SemanticVersion;
use semver::VersionReq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::Url;

use crate::models::{
    package::ArtifactTags,
    project_manifest::{
        MagnetManifestV1DependenciesValue, MagnetManifestV1PackageKind, NamedDependency,
    },
};
// This contains the structure of the json files stored in git

/// The `index-format` this client understands
pub const INDEX_FORMAT: u32 = 1;

/// `config.json` at the root of the index
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct GitIndexConfig {
    pub index_format: u32,
    /// URL template of an artifact, using the placeholders
    /// `{name}`, `{version}`, `{kind}`, `{abi}` and `{platform}`
    pub dl: String,
    /// base URL of the static HTTP mirror of this index, for clients without git
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<Url>,
    /// URL of the magnet server of this index, used to log in and publish
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<Url>,
}

/// abi and platform are kept as plain strings, so an index line which
/// contains a platform unknown to this client can still be read.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct GitArtifact {
    pub abi: String,
    pub platform: String,
    pub sha256: String,
}

impl GitArtifact {
    /// identifies the artifact within a release, e.g. `any-any` or `source-any`
    pub fn key(&self) -> String {
        format!("{}-{}", self.abi, self.platform)
    }
}

impl GitRelease {
    /// The tags of the release's artifacts, without those which aren't valid tags
    pub fn artifact_tags(&self) -> Vec<ArtifactTags> {
        self.artifacts
            .iter()
            .filter_map(|artifact| ArtifactTags::new(&artifact.abi, &artifact.platform).ok())
            .collect()
    }

    /// The sha256 of the artifact with these tags
    pub fn checksum(&self, tags: &ArtifactTags) -> Option<&str> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.key() == tags.to_string())
            .map(|artifact| artifact.sha256.as_str())
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(untagged)]
pub enum GitIndexDependency {
    Version(VersionReq),
    /// a git dependency (a magnet project or, with `legacy-quark`, a legacy quark), which
    /// published packages can only pin by its commit (DESIGN.md "Publishing a package")
    Git {
        #[serde(
            rename = "legacy-quark",
            default,
            skip_serializing_if = "std::ops::Not::not"
        )]
        legacy_quark: bool,
        git: GitIndexDependencySource,
    },
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct GitIndexDependencySource {
    pub url: Url,
    #[serde(rename = "ref")]
    pub ref_: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GitDependency {
    pub name: String,
    #[serde(
        deserialize_with = "deserialize_constraint",
        serialize_with = "serialize_constraint"
    )]
    pub constraint: VersionReq,
}

fn deserialize_constraint<'de, D>(deserializer: D) -> Result<VersionReq, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    let req: VersionReq = s.parse().map_err(serde::de::Error::custom)?;
    Ok(req)
}

fn serialize_constraint<S>(req: &VersionReq, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&req.to_string())
}

/// A single line of a package file in the index, i.e. one version of a package
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct GitRelease {
    pub name: String,
    #[serde(
        deserialize_with = "deserialize_semantic_version",
        serialize_with = "serialize_semantic_version"
    )]
    pub version: SemanticVersion,
    pub kind: MagnetManifestV1PackageKind,
    /// supported SuperCollider versions, see DESIGN.md "SuperCollider version"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sclang: Option<VersionReq>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, GitIndexDependency>,
    #[serde(default)]
    pub yanked: bool,
    #[serde(default)]
    pub artifacts: Vec<GitArtifact>,
}

impl GitRelease {
    /// The dependencies as requirements for the resolver: a git dependency accepts any
    /// version, since its commit determines it
    pub fn version_dependencies(&self) -> Vec<GitDependency> {
        self.dependencies
            .iter()
            .map(|(name, dependency)| GitDependency {
                name: name.clone(),
                constraint: match dependency {
                    GitIndexDependency::Version(constraint) => constraint.clone(),
                    GitIndexDependency::Git { .. } => VersionReq::STAR,
                },
            })
            .collect()
    }

    /// The git dependencies by their name as given
    pub fn git_dependencies(&self) -> impl Iterator<Item = (&String, &GitIndexDependencySource)> {
        self.dependencies
            .iter()
            .filter_map(|(name, dependency)| match dependency {
                GitIndexDependency::Git { git, .. } => Some((name, git)),
                GitIndexDependency::Version(_) => None,
            })
    }
}

pub fn deserialize_semantic_version<'de, D>(deserializer: D) -> Result<SemanticVersion, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    let version: SemanticVersion = s.parse().map_err(serde::de::Error::custom)?;
    Ok(version)
}

pub fn serialize_semantic_version<S>(
    version: &SemanticVersion,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&version.to_string())
}

impl TryFrom<NamedDependency> for GitDependency {
    type Error = anyhow::Error;

    fn try_from(value: NamedDependency) -> Result<Self, Self::Error> {
        let git_dependency = match value.dependency {
            MagnetManifestV1DependenciesValue::String(version) => Self {
                name: value.name,
                constraint: version.parse()?,
            },
            MagnetManifestV1DependenciesValue::Object {
                version: _version, ..
            } => Self {
                name: value.name,
                constraint: _version.unwrap_or("*".to_string()).parse()?,
            },
        };
        Ok(git_dependency)
    }
}

impl From<GitDependency> for MagnetManifestV1DependenciesValue {
    fn from(value: GitDependency) -> Self {
        MagnetManifestV1DependenciesValue::Object {
            git: None,
            ignore_quark_deps: None,
            index: None,
            legacy_quark: None,
            path: None,
            version: Some(value.constraint.to_string()),
        }
    }
}
