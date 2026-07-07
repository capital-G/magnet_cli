use pubgrub::SemanticVersion;
use semver::VersionReq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::models::project::{ArchitectureEnum, AssetFormatEnum, ServerAbiVersionEnum};
use crate::models::project_manifest::{MagnetManifestV1DependenciesValue, NamedDependency};
// This contains the structure of the json files stored in git

#[derive(Debug, Deserialize, Serialize)]
pub struct GitServerExtensionBinaryAsset {
    sha_256: String,
    asset_format: AssetFormatEnum,
    architecture: ArchitectureEnum,
    abi: ServerAbiVersionEnum,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GitAsset {
    Binary(GitServerExtensionBinaryAsset),
    Quark(GitQuarkSourceAsset),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GitQuarkSourceAsset {
    pub sha_256: String,
    pub asset_format: AssetFormatEnum,
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

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LockDependency {
    pub name: String,
    #[serde(
        deserialize_with = "deserialize_semantic_version",
        serialize_with = "serialize_semantic_version"
    )]
    pub version: SemanticVersion,
    // @todo add this
    // pub sha256: String,
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

#[derive(Debug, Deserialize, Serialize)]
pub struct GitRelease {
    pub name: String,
    #[serde(
        deserialize_with = "deserialize_semantic_version",
        serialize_with = "serialize_semantic_version"
    )]
    pub version: SemanticVersion,
    pub yanked: bool,
    pub dependencies: Vec<GitDependency>,
    pub assets: Vec<GitAsset>,
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
            branch: None,
            git: None,
            index: None,
            legacy_quark: None,
            path: None,
            ref_: None,
            tag: None,
            version: Some(value.constraint.to_string()),
        }
    }
}
