use pubgrub::SemanticVersion;

use crate::models::{
    package::{ArtifactTags, PackageKind},
    project_manifest::MagnetManifestV1,
};

/// `{name}-{version}-{kind}-{abi}-{platform}.tar.gz` with the normalised name, see DESIGN.md
/// "Packaging". The filename is only ever built, never parsed.
pub fn artifact_file_name(
    name: &str,
    version: &SemanticVersion,
    kind: &PackageKind,
    tags: &ArtifactTags,
) -> String {
    format!(
        "{}-{version}-{kind}-{tags}.tar.gz",
        MagnetManifestV1::normalize_name(name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quark_name() {
        assert_eq!(
            artifact_file_name(
                "Foo_Bar",
                &SemanticVersion::new(1, 2, 0),
                &PackageKind::Quark,
                &ArtifactTags::any()
            ),
            "foo-bar-1.2.0-quark-any-any.tar.gz",
        );
    }

    #[test]
    fn test_lepton_names() {
        let version = SemanticVersion::one();
        let kind = PackageKind::Lepton;
        assert_eq!(
            artifact_file_name(
                "foobar",
                &version,
                &kind,
                &ArtifactTags::new("api3_0", "macos_arm64").unwrap()
            ),
            "foobar-1.0.0-lepton-api3_0-macos_arm64.tar.gz",
        );
        assert_eq!(
            artifact_file_name("foobar", &version, &kind, &ArtifactTags::source()),
            "foobar-1.0.0-lepton-source-any.tar.gz",
        );
    }
}
