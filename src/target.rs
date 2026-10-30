//! The platform and abi tags of this machine, which select the binary artifacts of leptons,
//! see DESIGN.md "Packaging"

use std::{cell::OnceCell, fmt, str::FromStr};

use anyhow::{anyhow, bail, Context};
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
    pub const fn new(major: u32, minor: u32) -> Self {
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

/// The binary artifacts this machine can load: the platform and the plugin API of the server
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub platform: Platform,
    pub api: PluginApi,
}

impl Target {
    /// The tags of the artifact which gets built for this machine
    pub fn tags(&self) -> ArtifactTags {
        ArtifactTags::new(&self.api.to_string(), &self.platform.to_string())
            .expect("platform and plugin API tags are valid")
    }

    /// The artifact this machine installs out of `artifacts`: one of its platform whose
    /// plugin API the server loads, with the highest such minor version
    pub fn best_artifact<'a>(
        &self,
        artifacts: impl IntoIterator<Item = &'a ArtifactTags>,
    ) -> Option<&'a ArtifactTags> {
        let platform = self.platform.to_string();
        artifacts
            .into_iter()
            .filter(|tags| tags.platform == platform)
            .filter_map(|tags| Some((tags.abi.parse::<PluginApi>().ok()?, tags)))
            .filter(|(api, _)| self.api.loads(api))
            .max_by_key(|(api, _)| *api)
            .map(|(_, tags)| tags)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.platform, self.api)
    }
}

/// The target of this machine. Without an override, the platform comes from the target magnet
/// was built for and the plugin API from the version of the found sclang.
pub fn detect_target(
    platform: Option<&str>,
    abi: Option<&str>,
    find_sclang: impl FnOnce() -> anyhow::Result<Option<Sclang>>,
) -> anyhow::Result<Target> {
    let platform = match platform {
        Some(platform) => parse_platform(platform)?,
        None => current_platform()?,
    };
    let api = match abi {
        Some(abi) => abi.parse()?,
        None => {
            let sclang = find_sclang()?.with_context(|| {
                format!("Could not find sclang to derive the abi tag from its version, set {MAGNET_SCLANG_ENV} or the abi via --abi or {MAGNET_ABI_ENV}")
            })?;
            abi_for_version(&sclang.version()?)?
        }
    };
    let target = Target { platform, api };
    debug!(%target, "Target of the binary artifacts");
    Ok(target)
}

/// The target of this machine, determined on first use, since deriving the plugin API starts
/// sclang. Only leptons need it, so quark projects work without a SuperCollider installation.
pub struct LazyTarget {
    platform: Option<String>,
    abi: Option<String>,
    find_sclang: fn() -> anyhow::Result<Option<Sclang>>,
    target: OnceCell<Result<Target, String>>,
}

impl LazyTarget {
    /// With the overrides of `--platform`/`--abi`, otherwise detected
    pub fn new(platform: Option<String>, abi: Option<String>) -> Self {
        Self::with_sclang(platform, abi, Sclang::find)
    }

    pub fn with_sclang(
        platform: Option<String>,
        abi: Option<String>,
        find_sclang: fn() -> anyhow::Result<Option<Sclang>>,
    ) -> Self {
        Self {
            platform,
            abi,
            find_sclang,
            target: OnceCell::new(),
        }
    }

    /// The abi given via `--abi`/`MAGNET_ABI`, which may differ from the one of the found
    /// sclang, e.g. for an older scsynth or a development build
    pub fn abi_override(&self) -> anyhow::Result<Option<PluginApi>> {
        self.abi.as_deref().map(str::parse).transpose()
    }

    /// Only the platform, which doesn't need sclang
    pub fn platform(&self) -> anyhow::Result<Platform> {
        match &self.platform {
            Some(platform) => parse_platform(platform),
            None => current_platform(),
        }
    }

    pub fn get(&self) -> anyhow::Result<&Target> {
        self.target
            .get_or_init(|| {
                detect_target(
                    self.platform.as_deref(),
                    self.abi.as_deref(),
                    self.find_sclang,
                )
                .map_err(|err| format!("{err:#}"))
            })
            .as_ref()
            .map_err(|err| anyhow!("{err}"))
    }
}

impl Default for LazyTarget {
    fn default() -> Self {
        Self::new(None, None)
    }
}

fn parse_platform(platform: &str) -> anyhow::Result<Platform> {
    Platform::from_str(platform).ok().with_context(|| {
        format!(
            "{platform:?} is not a valid platform, use one of {}",
            platforms()
        )
    })
}

/// The platform tag of the target magnet was built for
pub fn current_platform() -> anyhow::Result<Platform> {
    detect_platform(std::env::consts::OS, std::env::consts::ARCH)
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
/// The plugin API of the released SuperCollider versions: `(major, first minor, last minor,
/// plugin API)`, see DESIGN.md "Packaging". The generated `MagnetServerOptions` class uses the
/// same table (`src/models/server_options.rs`).
pub const PLUGIN_APIS: [(u32, u32, u32, PluginApi); 2] = [
    (3, 6, 8, PluginApi::new(2, 0)),
    (3, 9, 14, PluginApi::new(3, 0)),
];

fn abi_for_version(version: &SemanticVersion) -> anyhow::Result<PluginApi> {
    let (major, minor, _) = (*version).into();
    PLUGIN_APIS
        .iter()
        .find(|(sc_major, first, last, _)| major == *sc_major && (*first..=*last).contains(&minor))
        .map(|(.., api)| *api)
        .with_context(|| {
            format!("The abi tag of SuperCollider {version} is not known, set it via --abi or {MAGNET_ABI_ENV}, e.g. api4_0 (the plugin API version in include/plugin_interface/SC_InterfaceTable.h of its sources)")
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
        let target = detect_target(Some("linux_armv7"), Some("api4_1"), no_sclang).unwrap();
        assert_eq!(
            target.tags(),
            ArtifactTags::new("api4_1", "linux_armv7").unwrap()
        );

        for (platform, abi) in [
            ("macos", "api3_0"),
            ("any", "api3_0"),
            ("linux_x86_64", "api3"),
            ("linux_x86_64", "any"),
        ] {
            assert!(
                detect_target(Some(platform), Some(abi), no_sclang).is_err(),
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
        let err = detect_target(Some("linux_x86_64"), None, || Ok(None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--abi"), "{err}");

        // the error is kept, so sclang is only looked up once
        let target = LazyTarget::with_sclang(Some("linux_x86_64".into()), None, || Ok(None));
        for _ in 0..2 {
            let err = target.get().unwrap_err().to_string();
            assert!(err.contains("--abi"), "{err}");
        }
    }

    #[test]
    fn test_best_artifact() {
        let tags = |key: &str| {
            let (abi, platform) = key.split_once('-').unwrap();
            ArtifactTags::new(abi, platform).unwrap()
        };
        let artifacts: Vec<ArtifactTags> = [
            "source-any",
            "api3_0-linux_x86_64",
            "api4_0-linux_x86_64",
            "api4_2-linux_x86_64",
            "api4_5-linux_x86_64",
            "api4_1-macos_arm64",
        ]
        .map(tags)
        .to_vec();
        for (platform, api, best) in [
            (
                Platform::LinuxX8664,
                PluginApi::new(4, 3),
                Some("api4_2-linux_x86_64"),
            ),
            (
                Platform::LinuxX8664,
                PluginApi::new(4, 1),
                Some("api4_0-linux_x86_64"),
            ),
            (
                Platform::LinuxX8664,
                PluginApi::new(3, 7),
                Some("api3_0-linux_x86_64"),
            ),
            (Platform::LinuxX8664, PluginApi::new(5, 0), None),
            (Platform::MacosArm64, PluginApi::new(4, 0), None),
            (
                Platform::MacosArm64,
                PluginApi::new(4, 1),
                Some("api4_1-macos_arm64"),
            ),
            (Platform::WindowsX8664, PluginApi::new(4, 1), None),
        ] {
            let target = Target { platform, api };
            assert_eq!(
                target
                    .best_artifact(&artifacts)
                    .map(|tags| tags.to_string()),
                best.map(String::from),
                "{target}"
            );
        }
    }
}
