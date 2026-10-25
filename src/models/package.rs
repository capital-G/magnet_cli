use std::{fmt, path::PathBuf};

use anyhow::ensure;

/// The tags of an artifact, which are part of its filename and its `dl` URL,
/// see DESIGN.md "Packaging".
/// The lists of valid lepton and gluon tags are not fixed yet (D3.2).
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

    /// The source package of a lepton or gluon
    pub fn source() -> Self {
        Self::new("source", "any").unwrap()
    }

    pub fn is_source(&self) -> bool {
        *self == Self::source()
    }
}

/// `{abi}-{platform}`, as used for the checksums of the lock file
impl fmt::Display for ArtifactTags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.abi, self.platform)
    }
}

/// An archive written by `magnet package`
#[derive(Debug, Clone, PartialEq)]
pub struct PackageArchive {
    pub path: PathBuf,
    pub tags: ArtifactTags,
    /// the paths within the archive, sorted
    pub files: Vec<String>,
}

/// Which archive of a package gets built, see DESIGN.md "Packaging"
#[derive(Debug, Clone)]
pub enum BundleType {
    /// source package of a lepton or gluon, selected by `source-paths`
    SourceBundle,
    /// artifact selected by `global-paths` (+ `platform-paths`)
    ArtifactBundle,
}

impl From<&ArtifactTags> for BundleType {
    fn from(tags: &ArtifactTags) -> Self {
        if tags.is_source() {
            BundleType::SourceBundle
        } else {
            BundleType::ArtifactBundle
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tags() {
        assert_eq!(ArtifactTags::any().to_string(), "any-any");
        assert_eq!(ArtifactTags::source().to_string(), "source-any");
        assert!(ArtifactTags::source().is_source());
        assert!(ArtifactTags::new("api3", "macos_arm64").is_ok());
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
}
