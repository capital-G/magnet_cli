use std::{fmt, path::PathBuf, str::FromStr};

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};

/// The kind of a package in the publish flow, until it follows the kinds of a version
/// (new_implementation_plan.md C8.6): a lepton if the version contains one, otherwise a quark
/// (`MagnetManifestV1::package_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageKind {
    Quark,
    Lepton,
    Gluon,
}

impl fmt::Display for PackageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PackageKind::Quark => "quark",
            PackageKind::Lepton => "lepton",
            PackageKind::Gluon => "gluon",
        })
    }
}

/// The tags of an artifact, which are part of its filename and its `dl` URL,
/// see DESIGN.md "Packaging".
/// Binary artifacts use a platform of the schema's list (`Platform`) and the plugin API as abi
/// (`PluginApi`, `src/target.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactTags {
    pub abi: String,
    pub platform: String,
}

impl ArtifactTags {
    /// The tags of a lepton/gluon artifact. Tags use `_` instead of `-`, so they are URL-safe
    /// and the parts of a filename stay apart.
    pub fn new(abi: &str, platform: &str) -> anyhow::Result<Self> {
        for tag in [abi, platform] {
            ensure!(
                !tag.is_empty()
                    && tag
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{tag:?} is not a valid tag, which may only contain a-z, 0-9 and _"
            );
        }
        Ok(Self {
            abi: abi.to_string(),
            platform: platform.to_string(),
        })
    }

    /// The only artifact of a quark
    pub fn any() -> Self {
        Self::new("any", "any").unwrap()
    }

    /// The tags of the source in the publish API, until it takes the kind of an artifact
    /// (new_implementation_plan.md C8.6)
    pub fn source() -> Self {
        Self::new("source", "any").unwrap()
    }
}

/// `{abi}-{platform}`, the end of an artifact's filename
impl fmt::Display for ArtifactTags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.abi, self.platform)
    }
}

/// Parses `{abi}-{platform}`
impl FromStr for ArtifactTags {
    type Err = anyhow::Error;

    fn from_str(key: &str) -> anyhow::Result<Self> {
        let (abi, platform) = key
            .split_once('-')
            .with_context(|| format!("{key:?} is not of the form {{abi}}-{{platform}}"))?;
        Self::new(abi, platform)
    }
}

/// The kind of an artifact, which is part of its filename and its `dl` URL, see DESIGN.md
/// "Artifact kinds". The source is no kind a version contains, but every version has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    Source,
    Quark,
    Lepton,
}

impl fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ArtifactKind::Source => "source",
            ArtifactKind::Quark => "quark",
            ArtifactKind::Lepton => "lepton",
        })
    }
}

impl FromStr for ArtifactKind {
    type Err = anyhow::Error;

    fn from_str(kind: &str) -> anyhow::Result<Self> {
        match kind {
            "source" => Ok(ArtifactKind::Source),
            "quark" => Ok(ArtifactKind::Quark),
            "lepton" => Ok(ArtifactKind::Lepton),
            _ => anyhow::bail!("{kind:?} is no kind of artifact this version of magnet knows"),
        }
    }
}

/// Identifies an artifact within a version: `{kind}-{abi}-{platform}`, the keys of the lock
/// file's checksums (DESIGN.md "Checksums")
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactKey {
    pub kind: ArtifactKind,
    pub tags: ArtifactTags,
}

impl ArtifactKey {
    pub fn new(kind: ArtifactKind, tags: ArtifactTags) -> Self {
        Self { kind, tags }
    }

    /// The artifact of a platform-independent kind, i.e. the source or the quark
    pub fn any(kind: ArtifactKind) -> Self {
        Self::new(kind, ArtifactTags::any())
    }
}

impl fmt::Display for ArtifactKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.kind, self.tags)
    }
}

impl FromStr for ArtifactKey {
    type Err = anyhow::Error;

    fn from_str(key: &str) -> anyhow::Result<Self> {
        let (kind, tags) = key
            .split_once('-')
            .with_context(|| format!("{key:?} is not of the form {{kind}}-{{abi}}-{{platform}}"))?;
        Ok(Self::new(kind.parse()?, tags.parse()?))
    }
}

/// An archive written by `magnet package`
#[derive(Debug, Clone, PartialEq)]
pub struct PackageArchive {
    pub path: PathBuf,
    pub kind: ArtifactKind,
    pub tags: ArtifactTags,
    /// the paths within the archive, sorted
    pub files: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tags() {
        assert_eq!(ArtifactTags::any().to_string(), "any-any");
        assert_eq!(ArtifactTags::source().to_string(), "source-any");
        assert!(ArtifactTags::new("api3_0", "macos_arm64").is_ok());
        assert_eq!(
            "api3_0-macos_arm64".parse::<ArtifactTags>().unwrap(),
            ArtifactTags::new("api3_0", "macos_arm64").unwrap()
        );
        assert!("api3_0".parse::<ArtifactTags>().is_err());
        assert!("api3_0-macos-arm64".parse::<ArtifactTags>().is_err());
        for (abi, platform) in [
            ("api-3", "any"),
            ("any", "macOS"),
            ("", "any"),
            ("a/b", "any"),
        ] {
            assert!(
                ArtifactTags::new(abi, platform).is_err(),
                "{abi} {platform}"
            );
        }
    }

    #[test]
    fn test_keys() {
        let lepton = ArtifactKey::new(
            ArtifactKind::Lepton,
            ArtifactTags::new("api3_0", "macos_arm64").unwrap(),
        );
        assert_eq!(lepton.to_string(), "lepton-api3_0-macos_arm64");
        assert_eq!(
            "lepton-api3_0-macos_arm64".parse::<ArtifactKey>().unwrap(),
            lepton
        );
        assert_eq!(
            ArtifactKey::any(ArtifactKind::Source).to_string(),
            "source-any-any"
        );
        // keys of other kinds or of the old `{abi}-{platform}` form
        for key in ["gluon-any-any", "any-any", "api3_0-macos_arm64"] {
            assert!(key.parse::<ArtifactKey>().is_err(), "{key}");
        }
    }
}
