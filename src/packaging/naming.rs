use pubgrub::SemanticVersion;

use crate::models::package::{BundleFormat, MagnetPackage, SourceBundle};

pub trait PackageNaming {
    fn package_file_name(&self, name: &str, version: &SemanticVersion) -> String;
}

impl<P: MagnetPackage> PackageNaming for BundleFormat<P> {
    fn package_file_name(&self, name: &str, version: &SemanticVersion) -> String {
        format!(
            "{}-{}-{}-{}-{}.tar.gz",
            name,
            version,
            format!("{:?}", P::KIND).to_lowercase(),
            format!("{:?}", self.abi).to_lowercase(),
            format!("{:?}", self.platform).to_lowercase(),
        )
    }
}

impl PackageNaming for SourceBundle {
    fn package_file_name(&self, name: &str, version: &SemanticVersion) -> String {
        format!("{}-{}-source.tar.gz", name, version,)
    }
}

#[cfg(test)]
mod tests {
    use pubgrub::SemanticVersion;

    use crate::models::package::{
        BinaryPlatformTag, Gluon, GluonAbiTag, Lepton, LeptonAbiTag, Quark, QuarkAbiTag,
        SourceBundle, SourcePlatformTag,
    };

    use super::*;

    #[test]
    fn test_quark_name() {
        let bundle = BundleFormat::<Quark> {
            abi: QuarkAbiTag::Sclang310,
            platform: SourcePlatformTag::Any,
        };

        assert_eq!(
            bundle.package_file_name("foobar", &SemanticVersion::one()),
            "foobar-1.0.0-quark-sclang310-any.tar.gz",
        );
    }

    #[test]
    fn test_lepton_name() {
        let bundle = BundleFormat::<Lepton> {
            abi: LeptonAbiTag::UniversalServer3,
            platform: BinaryPlatformTag::MacOSXArm64,
        };

        assert_eq!(
            bundle.package_file_name("foobar", &SemanticVersion::one()),
            "foobar-1.0.0-lepton-universalserver3-macosxarm64.tar.gz",
        );
    }

    #[test]
    fn test_gluon_name() {
        let bundle = BundleFormat::<Gluon> {
            abi: GluonAbiTag::GluonV1,
            platform: BinaryPlatformTag::Win32,
        };

        assert_eq!(
            bundle.package_file_name("foobar", &SemanticVersion::one()),
            "foobar-1.0.0-gluon-gluonv1-win32.tar.gz",
        );
    }

    #[test]
    fn test_source_name() {
        let bundle = SourceBundle;

        assert_eq!(
            bundle.package_file_name("foobar", &SemanticVersion::one()),
            "foobar-1.0.0-source.tar.gz",
        );
    }
}
