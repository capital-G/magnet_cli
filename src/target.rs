//! The platform and abi tags of this machine, which select the binary artifacts of leptons,
//! see DESIGN.md "Packaging"

use std::{fmt, str::FromStr};

use anyhow::{bail, Context};
use pubgrub::SemanticVersion;
use tracing::debug;

use crate::{
    models::{package::ArtifactTags, project_manifest::Platform},
    sclang::{Sclang, MAGNET_SCLANG_ENV},
};

/// Overrides the detected platform tag, e.g. for cross-compiled binaries
pub const MAGNET_PLATFORM_ENV: &str = "MAGNET_PLATFORM";
/// Overrides the abi tag derived from the found sclang
pub const MAGNET_ABI_ENV: &str = "MAGNET_ABI";

/// The plugin API version of scsynth/supernova, which is the abi tag of a lepton artifact,
/// written as `api<major>_<minor>`, e.g. `api3_0`, see DESIGN.md "Packaging".
/// A plugin is built against one version and works with servers of the same major version
/// and the same or a newer minor version (the patch version never changes the interface).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PluginApi {
    pub major: u32,
    pub minor: u32,
}

impl PluginApi {
    pub fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// Whether a server with this plugin API loads a plugin built against `plugin`
    pub fn loads(&self, plugin: &PluginApi) -> bool {
        self.major == plugin.major && plugin.minor <= self.minor
    }
}

impl FromStr for PluginApi {
    type Err = anyhow::Error;

    fn from_str(tag: &str) -> anyhow::Result<Self> {
        // no leading zeros, so every version has exactly one tag
        let number = |part: &str| {
            (!part.is_empty()
                && part.chars().all(|c| c.is_ascii_digit())
                && (part == "0" || !part.starts_with('0')))
            .then(|| part.parse::<u32>().ok())
            .flatten()
        };
        tag.strip_prefix("api")
            .and_then(|version| version.split_once('_'))
            .and_then(|(major, minor)| Some(Self::new(number(major)?, number(minor)?)))
            .with_context(|| {
                format!("{tag:?} is not a valid abi, which is `api<major>_<minor>` of the plugin API version of scsynth, e.g. api3_0")
            })
    }
}

impl fmt::Display for PluginApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "api{}_{}", self.major, self.minor)
    }
}

/// The tags of the binary artifact for this machine. Without an override, the platform comes
/// from the target magnet was built for and the abi from the version of the found sclang.
pub fn binary_tags(
    platform: Option<&str>,
    abi: Option<&str>,
    find_sclang: impl FnOnce() -> anyhow::Result<Option<Sclang>>,
) -> anyhow::Result<ArtifactTags> {
    let platform = match platform {
        Some(platform) => Platform::from_str(platform).ok().with_context(|| {
            format!(
                "{platform:?} is not a valid platform, use one of {}",
                platforms()
            )
        })?,
        None => detect_platform(std::env::consts::OS, std::env::consts::ARCH)?,
    };
    let abi = match abi {
        Some(abi) => abi.parse()?,
        None => {
            let sclang = find_sclang()?.with_context(|| {
                format!("Could not find sclang to derive the abi tag from its version, set {MAGNET_SCLANG_ENV} or the abi via --abi or {MAGNET_ABI_ENV}")
            })?;
            abi_for_version(&sclang.version()?)?
        }
    };
    debug!(%platform, %abi, "Tags of the binary artifact");
    ArtifactTags::new(&abi.to_string(), &platform.to_string())
}

/// The platform tag for an operating system and architecture as named by
/// `std::env::consts::{OS, ARCH}`
fn detect_platform(os: &str, arch: &str) -> anyhow::Result<Platform> {
    Ok(match (os, arch) {
        ("linux", "x86_64") => Platform::LinuxX8664,
        ("linux", "aarch64") => Platform::LinuxArm64,
        ("linux", "arm") => Platform::LinuxArmv7,
        ("macos", "x86_64") => Platform::MacosX8664,
        ("macos", "aarch64") => Platform::MacosArm64,
        ("windows", "x86_64") => Platform::WindowsX8664,
        ("windows", "aarch64") => Platform::WindowsArm64,
        _ => bail!(
            "{os} on {arch} is not a known platform, set it via --platform or {MAGNET_PLATFORM_ENV} to one of {}",
            platforms()
        ),
    })
}

/// The plugin API of a released SuperCollider version, see the table in DESIGN.md "Packaging".
/// Releases before the plugin API was versioned semantically count as minor version 0.
/// Other versions (e.g. development builds) can't be derived.
fn abi_for_version(version: &SemanticVersion) -> anyhow::Result<PluginApi> {
    let (major, minor, _) = (*version).into();
    Ok(match (major, minor) {
        (3, 6..=8) => PluginApi::new(2, 0),
        (3, 9..=14) => PluginApi::new(3, 0),
        _ => bail!(
            "The abi tag of SuperCollider {version} is not known, set it via --abi or {MAGNET_ABI_ENV}, e.g. api4_0 (the plugin API version in include/plugin_interface/SC_InterfaceTable.h of its sources)"
        ),
    })
}

fn platforms() -> String {
    [
        Platform::LinuxX8664,
        Platform::LinuxArm64,
        Platform::LinuxArmv7,
        Platform::MacosX8664,
        Platform::MacosArm64,
        Platform::WindowsX8664,
        Platform::WindowsArm64,
        Platform::Wasm32Emscripten,
    ]
    .map(|platform| platform.to_string())
    .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_platform() {
        for (os, arch, platform) in [
            ("linux", "x86_64", "linux_x86_64"),
            ("linux", "aarch64", "linux_arm64"),
            ("linux", "arm", "linux_armv7"),
            ("macos", "x86_64", "macos_x86_64"),
            ("macos", "aarch64", "macos_arm64"),
            ("windows", "x86_64", "windows_x86_64"),
            ("windows", "aarch64", "windows_arm64"),
        ] {
            assert_eq!(
                detect_platform(os, arch).unwrap().to_string(),
                platform,
                "{os} {arch}"
            );
        }
        for (os, arch) in [("windows", "x86"), ("freebsd", "x86_64")] {
            let err = detect_platform(os, arch).unwrap_err().to_string();
            assert!(err.contains("--platform"), "{err}");
        }
    }

    #[test]
    fn test_abi_for_version() {
        for (version, abi) in [
            ((3, 6, 0), "api2_0"),
            ((3, 8, 1), "api2_0"),
            ((3, 9, 0), "api3_0"),
            ((3, 14, 1), "api3_0"),
        ] {
            assert_eq!(abi_for_version(&version.into()).unwrap().to_string(), abi);
        }
        for version in [(3, 5, 1), (3, 15, 0), (4, 0, 0)] {
            let err = abi_for_version(&version.into()).unwrap_err().to_string();
            assert!(err.contains("--abi"), "{err}");
        }
    }

    #[test]
    fn test_overrides() {
        let no_sclang = || -> anyhow::Result<Option<Sclang>> { panic!("sclang is not needed") };
        let tags = binary_tags(Some("linux_armv7"), Some("api4_1"), no_sclang).unwrap();
        assert_eq!(tags, ArtifactTags::new("api4_1", "linux_armv7").unwrap());

        for (platform, abi) in [
            ("macos", "api3_0"),
            ("any", "api3_0"),
            ("linux_x86_64", "api3"),
            ("linux_x86_64", "any"),
        ] {
            assert!(
                binary_tags(Some(platform), Some(abi), no_sclang).is_err(),
                "{platform} {abi}"
            );
        }
    }

    #[test]
    fn test_plugin_api_tags() {
        for (tag, major, minor) in [("api3_0", 3, 0), ("api4_12", 4, 12), ("api10_0", 10, 0)] {
            let api: PluginApi = tag.parse().unwrap();
            assert_eq!(api, PluginApi::new(major, minor));
            assert_eq!(api.to_string(), tag);
        }
        for tag in [
            "api3", "api4_", "api_1", "3_0", "api3_0_1", "api03_0", "api3_01", "api3_f64", "any",
            "source",
        ] {
            assert!(tag.parse::<PluginApi>().is_err(), "{tag}");
        }
    }

    #[test]
    fn test_server_loads_plugin() {
        let server = PluginApi::new(4, 2);
        for (plugin, loads) in [
            (PluginApi::new(4, 0), true),
            (PluginApi::new(4, 2), true),
            (PluginApi::new(4, 3), false),
            (PluginApi::new(3, 0), false),
            (PluginApi::new(5, 0), false),
        ] {
            assert_eq!(server.loads(&plugin), loads, "{plugin}");
        }
    }

    #[test]
    fn test_abi_needs_sclang() {
        let err = binary_tags(Some("linux_x86_64"), None, || Ok(None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--abi"), "{err}");
    }
}
