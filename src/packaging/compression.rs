use std::{
    fs::{self, File},
    path::{Component, Path, PathBuf},
};

use anyhow::{ensure, Context};
use tracing::{debug, instrument, trace};

use crate::models::project::Project;

pub trait MagnetArchive {
    fn compress(
        &self,
        files: &Vec<PathBuf>,
        base_path: &PathBuf,
        target: &PathBuf,
    ) -> anyhow::Result<()>;
    fn extract(&self, source: &PathBuf, target: &PathBuf) -> anyhow::Result<()>;
}

impl MagnetArchive for Project {
    fn compress(
        &self,
        files: &Vec<PathBuf>,
        base_path: &PathBuf,
        target: &PathBuf,
    ) -> anyhow::Result<()> {
        compress(files, base_path, target)
    }

    fn extract(&self, source: &PathBuf, target: &PathBuf) -> anyhow::Result<()> {
        extract(source, target)
    }
}

/// Creates a `.tar.gz` archive of `files`, stored with their paths relative to `base_path`
#[instrument(skip(files, base_path))]
pub fn compress(files: &[PathBuf], base_path: &Path, target: &Path) -> anyhow::Result<()> {
    trace!("Create archive");
    let tar_file = File::create(target)?;
    let encoder = flate2::write::GzEncoder::new(&tar_file, flate2::Compression::default());
    let mut tar = tar::Builder::new(encoder);

    for path in files {
        let relative_path = path.strip_prefix(base_path)?;
        tar.append_path_with_name(path, relative_path)?;
    }

    tar.into_inner()?.finish()?;

    debug!("Created tar archive");

    Ok(())
}

/// Extracts a `.tar.gz` archive into `target`.
/// Only files and directories with relative paths are accepted, so an archive can not write
/// outside of `target`, neither via `..`, absolute paths nor links.
/// On an error, the already extracted files remain in `target`.
#[instrument]
pub fn extract(source: &Path, target: &Path) -> anyhow::Result<()> {
    let file = File::open(source).with_context(|| format!("Could not open archive {source:?}"))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    fs::create_dir_all(target).with_context(|| format!("Could not create directory {target:?}"))?;

    for entry in archive
        .entries()
        .with_context(|| format!("Could not read archive {source:?}"))?
    {
        let mut entry = entry.with_context(|| format!("Could not read archive {source:?}"))?;
        let path = entry.path()?.into_owned();
        ensure!(
            is_relative_path(&path),
            "Archive {source:?} contains the path {path:?}, which points outside of the package"
        );
        let entry_type = entry.header().entry_type();
        ensure!(
            entry_type.is_file() || entry_type.is_dir(),
            "Archive {source:?} contains {path:?}, which is neither a file nor a directory"
        );
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

    #[test]
    fn test_round_trip() {
        let source = tempfile::tempdir().unwrap();
        let files: Vec<(&str, &[u8])> = vec![
            ("Classes/Foo.sc", b"Foo {}"),
            ("HelpSource/Classes/Foo.schelp", b"title:: Foo"),
            ("sounds/a.wav", &[0, 159, 146, 150, 255]),
        ];
        let paths: Vec<PathBuf> = files
            .iter()
            .map(|(name, content)| {
                let path = source.path().join(name);
                write(&path, content);
                path
            })
            .collect();

        let archive_dir = tempfile::tempdir().unwrap();
        let archive = archive_dir.path().join("foo.tar.gz");
        compress(&paths, source.path(), &archive).unwrap();

        let target = tempfile::tempdir().unwrap();
        extract(&archive, target.path()).unwrap();

        for (name, content) in &files {
            assert_eq!(fs::read(target.path().join(name)).unwrap(), *content);
        }
        let extracted = walkdir::WalkDir::new(target.path())
            .into_iter()
            .filter(|entry| entry.as_ref().unwrap().file_type().is_file())
            .count();
        assert_eq!(extracted, files.len());
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
