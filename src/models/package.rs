use crate::models::project_manifest::MagnetManifestV1ArtifactsItemKind;

/// Describes the ABI which of the server it was compiled against.
/// Since some plugins are not compatible with Supernova (e.g. FluCoMa)
/// there is a not just the option to specify the ABI but also the
/// abi
///
/// Do not rename these after creating, since those are
/// used to identify packages on the server and on the client.
/// Should be at some point derived from the JSON spec.
#[derive(Debug, Clone)]
pub enum LeptonAbiTag {
    UniversalServer3,
    UniversalServer4,
    UniversalServer5,

    ScsynthServer3,
    ScsynthServer4,
    ScsynthServer5,

    SupernovaServer3,
    SupernovaServer4,
    SupernovaServer5,
}

/// Since the Gluon ABI is not existing yet we create a dummy entry V1
#[derive(Debug, Clone)]
pub enum GluonAbiTag {
    GluonV1,
}

/// Minimum sclang class library version required.
#[derive(Debug, Clone)]
pub enum QuarkAbiTag {
    Sclang310,
    Sclang311,
    Sclang312,
    Sclang313,
    Sclang314,
    Sclang315,
}

#[derive(Debug, Clone)]
pub enum SourcePlatformTag {
    Any,
}

#[derive(Debug, Clone)]
pub enum BinaryPlatformTag {
    WinAmd64,
    WinArm64,
    Win32,

    LinuxX86_64,
    LinuxArm64,

    MacOSXArm64,
    MacOSXAmd64,
    MacOSXUniversal,
}

pub struct Quark;
pub struct Lepton;
pub struct Gluon;

pub trait MagnetPackage {
    const KIND: MagnetManifestV1ArtifactsItemKind;
    type Abi: std::fmt::Debug;
    type Platform: std::fmt::Debug;
}

impl MagnetPackage for Quark {
    const KIND: MagnetManifestV1ArtifactsItemKind = MagnetManifestV1ArtifactsItemKind::Quark;
    type Abi = QuarkAbiTag;
    type Platform = SourcePlatformTag;
}

impl MagnetPackage for Lepton {
    const KIND: MagnetManifestV1ArtifactsItemKind = MagnetManifestV1ArtifactsItemKind::Lepton;
    type Abi = LeptonAbiTag;
    type Platform = BinaryPlatformTag;
}

impl MagnetPackage for Gluon {
    const KIND: MagnetManifestV1ArtifactsItemKind = MagnetManifestV1ArtifactsItemKind::Gluon;
    type Abi = GluonAbiTag;
    type Platform = BinaryPlatformTag;
}

pub struct SourceBundle;

#[derive(Debug, Clone)]
pub struct Bundle<P: MagnetPackage> {
    pub abi: P::Abi,
    pub platform: P::Platform,
}
