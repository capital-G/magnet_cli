use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, ensure, Context};
use tracing::{debug, instrument, trace};

/// Limits of [extract] against archive bombs, the same as the server's upload checks
/// (DESIGN.md "Checks of the server"). The lock checksum only proves that it's the uploaded
/// file, not that it's harmless.
#[derive(Debug, Clone)]
pub struct ExtractLimits {
    /// sum of the sizes of all files
    pub unpacked_size: u64,
    pub entries: usize,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            unpacked_size: 500 * 1024 * 1024,
            entries: 10_000,
        }
    }
}

/// Creates a reproducible `.tar.gz` archive (DESIGN.md "Packaging"): plain ustar headers,
/// entries sorted by their path, a fixed mtime, uid/gid 0 and mode 0644 (0755 for
/// executables). `entries` maps the path within the archive to the file on disk.
/// Only files get stored, the directories are implied by their paths.
#[instrument(skip(entries))]
pub fn compress(entries: &BTreeMap<String, PathBuf>, target: &Path) -> anyhow::Result<()> {
    trace!("Create archive");
    let tar_file = File::create(target).with_context(|| format!("Could not create {target:?}"))?;
    // the gzip header of flate2 has no timestamp and no file name, so it's reproducible too
    let encoder = flate2::write::GzEncoder::new(tar_file, flate2::Compression::default());
    let mut tar = tar::Builder::new(encoder);

    for (path, source) in entries {
        let file = File::open(source).with_context(|| format!("Could not open {source:?}"))?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "{source:?} is not a file");

        let mut header = tar::Header::new_ustar();
        // splits long paths into the prefix and name fields, and fails if that's not possible
        header
            .set_path(path)
            .with_context(|| format!("The path {path} is too long for the archive"))?;
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(metadata.len());
        header.set_mode(if is_executable(&metadata) {
            0o755
        } else {
            0o644
        });
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_cksum();
        tar.append(&header, file)
            .with_context(|| format!("Could not add {source:?} to the archive"))?;
    }

    tar.into_inner()?.finish()?;

    debug!("Created tar archive");

    Ok(())
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

/// Extracts a `.tar.gz` archive into `target`, see [extract_with_limits]
pub fn extract(source: &Path, target: &Path) -> anyhow::Result<()> {
    extract_with_limits(source, target, &ExtractLimits::default())
}

/// Extracts a `.tar.gz` archive into `target`.
/// Only files and directories with relative paths are accepted, so an archive can not write
/// outside of `target`, neither via `..`, absolute paths nor links.
/// Paths which can't exist on every platform (names that only differ in case, reserved names
/// on Windows) are rejected, and so are archives beyond the `limits`.
/// On an error, the already extracted files remain in `target`.
#[instrument]
pub fn extract_with_limits(
    source: &Path,
    target: &Path,
    limits: &ExtractLimits,
) -> anyhow::Result<()> {
    let file = File::open(source).with_context(|| format!("Could not open archive {source:?}"))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    archive.set_preserve_mtime(false);
    fs::create_dir_all(target).with_context(|| format!("Could not create directory {target:?}"))?;

    let mut unpacked_size: u64 = 0;
    let mut seen_paths = HashSet::new();
    for (index, entry) in archive
        .entries()
        .with_context(|| format!("Could not read archive {source:?}"))?
        .enumerate()
    {
        ensure!(
            index < limits.entries,
            "Archive {source:?} has more than {} entries",
            limits.entries
        );
        let mut entry = entry.with_context(|| format!("Could not read archive {source:?}"))?;
        let path = entry.path()?.into_owned();
        ensure!(
            is_relative_path(&path),
            "Archive {source:?} contains the path {path:?}, which points outside of the package"
        );
        check_portable(&path)
            .with_context(|| format!("Archive {source:?} contains the path {path:?}"))?;
        let entry_type = entry.header().entry_type();
        ensure!(
            entry_type.is_file() || entry_type.is_dir(),
            "Archive {source:?} contains {path:?}, which is neither a file nor a directory"
        );
        if entry_type.is_file() {
            // `a/b` and `A/b` would end up in the same file on macOS and Windows
            let folded = path.to_string_lossy().to_lowercase();
            ensure!(
                seen_paths.insert(folded),
                "Archive {source:?} contains {path:?} more than once (ignoring the case)"
            );
            unpacked_size = unpacked_size.saturating_add(entry.size());
            ensure!(
                unpacked_size <= limits.unpacked_size,
                "Archive {source:?} unpacks to more than {} bytes",
                limits.unpacked_size
            );
        }
        trace!(?path, "Extract");
        // unpack_in additionally verifies that the parent directories lie within target
        ensure!(
            entry
                .unpack_in(target)
                .with_context(|| format!("Could not extract {path:?} from {source:?}"))?,
            "Could not extract {path:?} from {source:?}"
        );
    }

    debug!("Extracted tar archive");

    Ok(())
}

/// `true` if the path stays within the directory it gets joined to
fn is_relative_path(path: &Path) -> bool {
    path.components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

/// Names which Windows can't create: reserved device names (also with an extension) and
/// names which end with a dot or a space
fn check_portable(path: &Path) -> anyhow::Result<()> {
    const RESERVED: &[&str] = &["con", "prn", "aux", "nul"];
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = name.to_string_lossy();
        if name.ends_with('.') || name.ends_with(' ') {
            bail!("{name:?} ends with a dot or a space, which is not possible on Windows");
        }
        let stem = name.split('.').next().unwrap_or_default().to_lowercase();
        let numbered = ["com", "lpt"].iter().any(|prefix| {
            stem.strip_prefix(prefix)
                .is_some_and(|n| n.len() == 1 && n.chars().all(|c| c.is_ascii_digit()))
        });
        if RESERVED.contains(&stem.as_str()) || numbered {
            bail!("{name:?} is a reserved name on Windows");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// an archive with a single entry, whose raw name bypasses the checks of the tar crate
    fn archive_with_raw_entry(
        path: &Path,
        name: &str,
        entry_type: tar::EntryType,
        link_name: Option<&str>,
    ) {
        let mut header = tar::Header::new_gnu();
        let raw_name = &mut header.as_old_mut().name;
        raw_name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_entry_type(entry_type);
        if let Some(link_name) = link_name {
            header.set_link_name(link_name).unwrap();
        }
        let content = b"evil";
        header.set_size(if entry_type.is_file() {
            content.len() as u64
        } else {
            0
        });
        header.set_mode(0o644);
        header.set_cksum();

        let file = File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let data: &[u8] = if entry_type.is_file() { content } else { &[] };
        tar.append(&header, data).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }

    /// writes `files` below `dir` and maps their names to the written paths
    fn entries(dir: &Path, files: &[(&str, &[u8])]) -> BTreeMap<String, PathBuf> {
        files
            .iter()
            .enumerate()
            .map(|(index, (name, content))| {
                // numbered sources, so names which only differ in case don't collide on disk
                let path = dir.join(index.to_string());
                write(&path, content);
                (name.to_string(), path)
            })
            .collect()
    }

    const FILES: &[(&str, &[u8])] = &[
        ("Classes/Foo.sc", b"Foo {}"),
        ("HelpSource/Classes/Foo.schelp", b"title:: Foo"),
        ("magnet.toml", b"magnet-version = 1"),
        ("sounds/a.wav", &[0, 159, 146, 150, 255]),
    ];

    #[test]
    fn test_round_trip() {
        let source = tempfile::tempdir().unwrap();
        let archive_dir = tempfile::tempdir().unwrap();
        let archive = archive_dir.path().join("foo.tar.gz");
        compress(&entries(source.path(), FILES), &archive).unwrap();

        let target = tempfile::tempdir().unwrap();
        extract(&archive, target.path()).unwrap();

        for (name, content) in FILES {
            assert_eq!(fs::read(target.path().join(name)).unwrap(), *content);
        }
        let extracted = walkdir::WalkDir::new(target.path())
            .into_iter()
            .filter(|entry| entry.as_ref().unwrap().file_type().is_file())
            .count();
        assert_eq!(extracted, FILES.len());
    }

    #[test]
    fn test_archive_is_reproducible_ustar() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.tar.gz");
        compress(&entries(&dir.path().join("a"), FILES), &first).unwrap();
        // other source files with other mtimes and permissions
        let other = entries(&dir.path().join("b"), FILES);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in other.values() {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write(&other["Classes/Foo.sc"], b"Foo {}");
        let second = dir.path().join("second.tar.gz");
        compress(&other, &second).unwrap();
        assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());

        let mut archive =
            tar::Archive::new(flate2::read::GzDecoder::new(File::open(&first).unwrap()));
        let mut names = vec![];
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let header = entry.header();
            assert!(header.as_ustar().is_some(), "not a ustar header");
            assert_eq!(header.entry_type(), tar::EntryType::Regular);
            assert_eq!(header.mtime().unwrap(), 0);
            assert_eq!(header.uid().unwrap(), 0);
            assert_eq!(header.mode().unwrap(), 0o644);
            names.push(entry.path().unwrap().to_string_lossy().to_string());
        }
        let expected: Vec<&str> = FILES.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, expected);
    }

    #[cfg(unix)]
    #[test]
    fn test_executables_keep_their_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let files = entries(dir.path(), &[("plugins/run.sh", b"#!/bin/sh")]);
        fs::set_permissions(&files["plugins/run.sh"], fs::Permissions::from_mode(0o700)).unwrap();
        let archive = dir.path().join("x.tar.gz");
        compress(&files, &archive).unwrap();

        let mut archive =
            tar::Archive::new(flate2::read::GzDecoder::new(File::open(&archive).unwrap()));
        let entry = archive.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(entry.header().mode().unwrap(), 0o755);
    }

    #[test]
    fn test_long_paths() {
        let dir = tempfile::tempdir().unwrap();
        // 150 characters of folders + 90 characters of name fit into prefix + name
        let long = format!("{}/{}.sc", "d".repeat(150), "f".repeat(87));
        let files = entries(dir.path(), &[(long.as_str(), b"x")]);
        let archive = dir.path().join("long.tar.gz");
        compress(&files, &archive).unwrap();
        let target = dir.path().join("target");
        extract(&archive, &target).unwrap();
        assert!(target.join(&long).is_file());

        let too_long = format!("{}/{}.sc", "d".repeat(150), "f".repeat(120));
        let files = entries(&dir.path().join("too-long"), &[(too_long.as_str(), b"x")]);
        let err = compress(&files, &dir.path().join("too-long.tar.gz")).unwrap_err();
        assert!(err.to_string().contains("too long"), "{err:#}");
    }

    #[test]
    fn test_case_collisions_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let files = entries(
            dir.path(),
            &[("Classes/Foo.sc", b"a"), ("classes/foo.sc", b"b")],
        );
        let archive = dir.path().join("case.tar.gz");
        compress(&files, &archive).unwrap();
        let err = extract(&archive, &dir.path().join("target")).unwrap_err();
        assert!(err.to_string().contains("ignoring the case"), "{err:#}");
    }

    #[test]
    fn test_windows_names_are_rejected() {
        for name in [
            "CON",
            "Classes/nul.sc",
            "com1.txt",
            "LPT9",
            "Foo./a.sc",
            "a ",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let files = entries(dir.path(), &[(name, b"x")]);
            let archive = dir.path().join("windows.tar.gz");
            compress(&files, &archive).unwrap();
            assert!(
                extract(&archive, &dir.path().join("target")).is_err(),
                "{name}"
            );
        }
        for name in ["console.sc", "com10", "Classes/nul_device.sc"] {
            assert!(check_portable(Path::new(name)).is_ok(), "{name}");
        }
    }

    #[test]
    fn test_limits() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("foo.tar.gz");
        compress(&entries(dir.path(), FILES), &archive).unwrap();

        let size: u64 = FILES.iter().map(|(_, content)| content.len() as u64).sum();
        let fits = ExtractLimits {
            unpacked_size: size,
            entries: FILES.len(),
        };
        extract_with_limits(&archive, &dir.path().join("fits"), &fits).unwrap();

        let too_big = ExtractLimits {
            unpacked_size: size - 1,
            ..fits.clone()
        };
        let err = extract_with_limits(&archive, &dir.path().join("big"), &too_big).unwrap_err();
        assert!(err.to_string().contains("bytes"), "{err:#}");

        let too_many = ExtractLimits {
            entries: FILES.len() - 1,
            ..fits
        };
        let err = extract_with_limits(&archive, &dir.path().join("many"), &too_many).unwrap_err();
        assert!(err.to_string().contains("entries"), "{err:#}");
    }

    #[test]
    fn test_path_traversal() {
        for name in ["../evil.sc", "Classes/../../evil.sc", "/tmp/magnet-evil.sc"] {
            let dir = tempfile::tempdir().unwrap();
            let archive = dir.path().join("evil.tar.gz");
            archive_with_raw_entry(&archive, name, tar::EntryType::Regular, None);

            let target = dir.path().join("package");
            let err = extract(&archive, &target).unwrap_err();
            assert!(err.to_string().contains("outside"), "{name}: {err:#}");
            assert!(!dir.path().join("evil.sc").exists());
        }
        assert!(!Path::new("/tmp/magnet-evil.sc").exists());
    }

    #[test]
    fn test_links_are_rejected() {
        for entry_type in [tar::EntryType::Symlink, tar::EntryType::Link] {
            let dir = tempfile::tempdir().unwrap();
            let archive = dir.path().join("link.tar.gz");
            archive_with_raw_entry(&archive, "Classes", entry_type, Some("/"));

            let err = extract(&archive, &dir.path().join("package")).unwrap_err();
            assert!(err.to_string().contains("neither a file nor a directory"));
        }
    }

    #[test]
    fn test_missing_archive() {
        let dir = tempfile::tempdir().unwrap();
        assert!(extract(&dir.path().join("missing.tar.gz"), dir.path()).is_err());
    }
}
