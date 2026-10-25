//! Locating the sclang installation which is used for a project

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{ensure, Context};
use pubgrub::SemanticVersion;
use tracing::trace;

/// Path of the sclang binary to use, takes precedence over `PATH`
pub const MAGNET_SCLANG_ENV: &str = "MAGNET_SCLANG";

/// Locations which are tried if sclang is neither set via `MAGNET_SCLANG` nor on the `PATH`
const DEFAULT_SCLANG_PATHS: &[&str] = &["/Applications/SuperCollider.app/Contents/MacOS/sclang"];

/// Layouts of an installation, relative to the directory of the sclang binary
const CLASS_LIBRARY_LOCATIONS: &[&str] = &[
    // macOS app bundle: Contents/MacOS/sclang → Contents/Resources/SCClassLibrary
    "../Resources/SCClassLibrary",
    // Windows: next to sclang.exe
    "SCClassLibrary",
    // Linux prefix install: bin/sclang → share/SuperCollider/SCClassLibrary
    "../share/SuperCollider/SCClassLibrary",
];

/// A file every class library contains, to verify a location
const CLASS_LIBRARY_MARKER: &str = "Common/Core/Object.sc";

/// An sclang binary together with its class library
#[derive(Debug, Clone, PartialEq)]
pub struct Sclang {
    pub binary: PathBuf,
    pub class_library: PathBuf,
}

impl Sclang {
    /// Finds sclang via `MAGNET_SCLANG`, the `PATH` or the default locations.
    /// Returns `None` if no sclang could be found.
    pub fn find() -> anyhow::Result<Option<Self>> {
        let candidates = DEFAULT_SCLANG_PATHS
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        match find_binary(
            std::env::var_os(MAGNET_SCLANG_ENV),
            std::env::var_os("PATH"),
            &candidates,
        )? {
            Some(binary) => Ok(Some(Self::from_binary(&binary)?)),
            None => Ok(None),
        }
    }

    pub fn from_binary(binary: &Path) -> anyhow::Result<Self> {
        // e.g. /usr/local/bin/sclang may be a symlink into an app bundle
        let binary = binary
            .canonicalize()
            .with_context(|| format!("Could not find sclang at {binary:?}"))?;
        let class_library = class_library_dir(&binary).with_context(|| {
            format!("Could not find the class library (SCClassLibrary) of {binary:?}")
        })?;
        Ok(Self {
            binary,
            class_library,
        })
    }
}

impl Sclang {
    /// The version of sclang via `sclang -v`, which prints e.g.
    /// `sclang 3.14.1 (Built from tag 'Version-3.14.1' [426edf6])` and exits.
    /// Only the release numbers are kept, so `3.15.0-dev` becomes `3.15.0`.
    pub fn version(&self) -> anyhow::Result<SemanticVersion> {
        let output = Command::new(&self.binary)
            .arg("-v")
            .output()
            .with_context(|| format!("Could not start {:?}", self.binary))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_version(&stdout).with_context(|| {
            format!(
                "Could not read the version of {:?} from {stdout:?}",
                self.binary
            )
        })
    }
}

fn parse_version(output: &str) -> Option<SemanticVersion> {
    let version = output.split_whitespace().nth(1)?;
    let release: String = version
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut numbers = release.split('.').map(|number| number.parse::<u32>());
    let major = numbers.next()?.ok()?;
    let minor = numbers.next().unwrap_or(Ok(0)).ok()?;
    let patch = numbers.next().unwrap_or(Ok(0)).ok()?;
    Some(SemanticVersion::new(major, minor, patch))
}

fn find_binary(
    env_override: Option<OsString>,
    path_var: Option<OsString>,
    defaults: &[PathBuf],
) -> anyhow::Result<Option<PathBuf>> {
    if let Some(binary) = env_override {
        let binary = PathBuf::from(binary);
        ensure!(
            binary.is_file(),
            "{MAGNET_SCLANG_ENV} points to {binary:?}, which does not exist"
        );
        return Ok(Some(binary));
    }

    let name = format!("sclang{}", std::env::consts::EXE_SUFFIX);
    let on_path = path_var
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(&name));
    let found = on_path
        .chain(defaults.iter().cloned())
        .find(|binary| binary.is_file());
    trace!(?found, "Looked up sclang");
    Ok(found)
}

/// The SCClassLibrary which belongs to an sclang binary
fn class_library_dir(binary: &Path) -> Option<PathBuf> {
    let binary_dir = binary.parent()?;
    CLASS_LIBRARY_LOCATIONS
        .iter()
        .map(|location| binary_dir.join(location))
        .find(|dir| dir.join(CLASS_LIBRARY_MARKER).is_file())
        .and_then(|dir| dir.canonicalize().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_parse_version() {
        for (output, version) in [
            (
                "sclang 3.14.1 (Built from tag 'Version-3.14.1' [426edf6])\n",
                SemanticVersion::new(3, 14, 1),
            ),
            (
                "sclang 3.15.0-dev (Built from branch 'develop' [12b0aad2a6])\n",
                SemanticVersion::new(3, 15, 0),
            ),
            ("sclang 3.14", SemanticVersion::new(3, 14, 0)),
        ] {
            assert_eq!(parse_version(output), Some(version), "{output}");
        }
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("sclang unknown"), None);
    }

    /// creates an installation with the given binary and class library paths
    fn installation(root: &Path, binary: &str, class_library: &str) -> PathBuf {
        let binary = root.join(binary);
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, b"").unwrap();
        let marker = root.join(class_library).join(CLASS_LIBRARY_MARKER);
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(marker, b"Object {}").unwrap();
        binary
    }

    #[test]
    fn test_class_library_layouts() {
        for (binary, class_library) in [
            (
                "SuperCollider.app/Contents/MacOS/sclang",
                "SuperCollider.app/Contents/Resources/SCClassLibrary",
            ),
            ("SuperCollider/sclang.exe", "SuperCollider/SCClassLibrary"),
            ("usr/bin/sclang", "usr/share/SuperCollider/SCClassLibrary"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let binary = installation(root.path(), binary, class_library);
            let sclang = Sclang::from_binary(&binary).unwrap();
            assert_eq!(
                sclang.class_library,
                root.path().join(class_library).canonicalize().unwrap()
            );
        }
    }

    #[test]
    fn test_missing_class_library() {
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("bin/sclang");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, b"").unwrap();
        assert!(Sclang::from_binary(&binary).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_symlinked_binary() {
        let root = tempfile::tempdir().unwrap();
        let binary = installation(
            root.path(),
            "SuperCollider.app/Contents/MacOS/sclang",
            "SuperCollider.app/Contents/Resources/SCClassLibrary",
        );
        let link = root.path().join("bin/sclang");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&binary, &link).unwrap();
        let sclang = Sclang::from_binary(&link).unwrap();
        assert_eq!(sclang.binary, binary.canonicalize().unwrap());
    }

    #[test]
    fn test_find_binary() {
        let root = tempfile::tempdir().unwrap();
        let name = format!("sclang{}", std::env::consts::EXE_SUFFIX);
        let on_path = root.path().join("bin").join(&name);
        let default = root.path().join("default").join(&name);
        for binary in [&on_path, &default] {
            fs::create_dir_all(binary.parent().unwrap()).unwrap();
            fs::write(binary, b"").unwrap();
        }
        let path_var =
            std::env::join_paths([root.path().join("empty"), root.path().join("bin")]).unwrap();

        // the environment variable takes precedence
        assert_eq!(
            find_binary(Some(default.clone().into()), Some(path_var.clone()), &[]).unwrap(),
            Some(default.clone())
        );
        assert!(find_binary(Some(root.path().join("missing").into()), None, &[]).is_err());
        // then the PATH
        assert_eq!(
            find_binary(None, Some(path_var), &[default.clone()]).unwrap(),
            Some(on_path)
        );
        // then the defaults
        assert_eq!(
            find_binary(None, None, &[root.path().join("missing"), default.clone()]).unwrap(),
            Some(default)
        );
        assert_eq!(find_binary(None, None, &[]).unwrap(), None);
    }
}
