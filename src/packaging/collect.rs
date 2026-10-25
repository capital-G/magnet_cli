use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, ensure, Context};
use globset::{GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};
use tracing::{debug, instrument, trace};
use walkdir::WalkDir;

use crate::{
    config::constants::MAGNET_PROJECT_FILE,
    models::{
        directories::ProjectDirectory,
        language_config::SCLANG_CONF_FILE,
        package::BundleType,
        project_manifest::{MagnetManifestV1, MagnetManifestV1PackageKind},
    },
};

/// readme which gets packaged if the manifest doesn't name one
const DEFAULT_README: &str = "README.md";

/// The files of one archive, relative to the project root and sorted
#[derive(Debug, Default, PartialEq)]
pub struct PackageFiles {
    /// files which keep their path within the archive
    pub files: Vec<PathBuf>,
    /// binaries of `platform-paths`, which go into `plugins/`
    pub binaries: Vec<PathBuf>,
}

/// Selects the files of a package via the globs of the manifest's `[artifact]` table,
/// see DESIGN.md "Packaging".
/// Hidden files, `dist/`, `external/` and `sclang_conf.yaml` are never selected, while
/// `magnet.toml` and the readme are always part of the package.
#[instrument(skip(manifest))]
pub fn collect_files(
    root: &Path,
    manifest: &MagnetManifestV1,
    bundle_type: &BundleType,
) -> anyhow::Result<PackageFiles> {
    let artifact = manifest.artifact.clone().unwrap_or_default();
    let kind = &manifest.package.kind;

    let (file_globs, binary_globs) = match bundle_type {
        BundleType::SourceBundle => {
            ensure!(
                *kind != MagnetManifestV1PackageKind::Quark,
                "A quark has no source package, since its artifact already is the source"
            );
            ensure!(
                !artifact.source_paths.is_empty(),
                "A {kind} needs `source-paths` in [artifact], which select the files of its source package"
            );
            (artifact.source_paths, vec![])
        }
        BundleType::ArtifactBundle => {
            ensure!(
                !artifact.global_paths.is_empty(),
                "There are no `global-paths` in [artifact], which select the files of the package"
            );
            let binary_globs = artifact
                .platform_paths
                .map(|platform_paths| platform_paths.paths)
                .unwrap_or_default();
            ensure!(
                *kind != MagnetManifestV1PackageKind::Quark || binary_globs.is_empty(),
                "A quark can not have `platform-paths`, since it has no binaries"
            );
            (artifact.global_paths, binary_globs)
        }
    };

    let candidates = project_files(root)?;
    let exclude = glob_set(&artifact.exclude)?;
    let mut unmatched = vec![];
    let mut files = select(&candidates, &file_globs, &exclude, &mut unmatched)?;
    let binaries = select(&candidates, &binary_globs, &exclude, &mut unmatched)?;
    ensure!(
        unmatched.is_empty(),
        "The globs {} don't select any file (hidden files, dist/, external/ and {SCLANG_CONF_FILE} are never packaged)",
        unmatched.join(", ")
    );
    if let Some(path) = files.intersection(&binaries).next() {
        bail!("{path} is selected by `global-paths` and `platform-paths`, but can only be in one of them");
    }
    for path in files.iter().chain(&binaries) {
        ensure!(
            !root.join(path).is_symlink(),
            "{path} is a symbolic link, which can't be packaged"
        );
    }

    files.extend(always_packaged(root, manifest)?);

    let package_files = PackageFiles {
        files: files.into_iter().map(PathBuf::from).collect(),
        binaries: binaries.into_iter().map(PathBuf::from).collect(),
    };
    debug!(
        files = package_files.files.len(),
        binaries = package_files.binaries.len(),
        "Collected package files"
    );
    Ok(package_files)
}

/// All files (and links) of the project which may be packaged, as `/`-separated paths
/// relative to `root`
fn project_files(root: &Path) -> anyhow::Result<Vec<String>> {
    let never_packaged = [
        String::from(ProjectDirectory::Dist),
        String::from(ProjectDirectory::External),
        SCLANG_CONF_FILE.to_string(),
    ];
    let walker = WalkDir::new(root).sort_by_file_name().into_iter();
    let mut files = vec![];
    for entry in walker.filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        entry.depth() == 0
            || !(name.starts_with('.')
                || entry.depth() == 1 && never_packaged.contains(&name.to_string()))
    }) {
        let entry = entry.with_context(|| format!("Could not read the project at {root:?}"))?;
        if entry.file_type().is_dir() {
            continue;
        }
        files.push(relative_path(entry.path().strip_prefix(root)?)?);
    }
    Ok(files)
}

/// The paths of `candidates` which match any of `globs` but none of `exclude`.
/// Globs which match no candidate at all are added to `unmatched`.
fn select(
    candidates: &[String],
    globs: &[String],
    exclude: &GlobSet,
    unmatched: &mut Vec<String>,
) -> anyhow::Result<BTreeSet<String>> {
    let mut selected = BTreeSet::new();
    for glob in globs {
        let matcher = compile(glob)?;
        let matches: Vec<&String> = candidates
            .iter()
            .filter(|path| matcher.is_match(path))
            .collect();
        if matches.is_empty() {
            unmatched.push(format!("`{glob}`"));
        }
        for path in matches {
            if exclude.is_match(path) {
                trace!(path, "Excluded");
            } else {
                selected.insert(path.clone());
            }
        }
    }
    Ok(selected)
}

/// `magnet.toml` and the readme, see DESIGN.md "Packaging"
fn always_packaged(root: &Path, manifest: &MagnetManifestV1) -> anyhow::Result<Vec<String>> {
    ensure!(
        root.join(MAGNET_PROJECT_FILE).is_file(),
        "There is no {MAGNET_PROJECT_FILE} in {root:?}"
    );
    let mut files = vec![MAGNET_PROJECT_FILE.to_string()];

    let readme = manifest
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.readme.clone());
    match readme {
        Some(readme) => {
            let path = relative_path(Path::new(&readme))?;
            ensure!(
                root.join(&path).is_file(),
                "The readme {readme} of [metadata] does not exist"
            );
            files.push(path);
        }
        None if root.join(DEFAULT_README).is_file() => files.push(DEFAULT_README.to_string()),
        None => {}
    }
    Ok(files)
}

/// `path` as `/`-separated string, which must stay within the project
fn relative_path(path: &Path) -> anyhow::Result<String> {
    let mut components = vec![];
    for component in path.components() {
        match component {
            Component::Normal(name) => components.push(
                name.to_str()
                    .with_context(|| format!("{path:?} is not valid UTF-8"))?,
            ),
            Component::CurDir => {}
            _ => bail!("{path:?} points outside of the project"),
        }
    }
    Ok(components.join("/"))
}

fn glob_set(globs: &[String]) -> anyhow::Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for glob in globs {
        builder.add(glob_builder(glob)?);
    }
    Ok(builder.build()?)
}

fn compile(glob: &str) -> anyhow::Result<GlobMatcher> {
    Ok(glob_builder(glob)?.compile_matcher())
}

/// Globs work like in `.gitignore`: `*` stays within a folder and `**/` spans folders.
/// Additionally, `Classes/**.sc` means `Classes/**/*.sc`, since gitignore would only match
/// the direct children.
fn glob_builder(glob: &str) -> anyhow::Result<globset::Glob> {
    let expanded = glob
        .split('/')
        .filter(|component| *component != ".")
        .map(|component| match component.strip_prefix("**") {
            Some(rest) if !rest.is_empty() && !rest.starts_with('*') => format!("**/*{rest}"),
            _ => component.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/");
    GlobBuilder::new(&expanded)
        .literal_separator(true)
        .build()
        .with_context(|| format!("`{glob}` is not a valid glob"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn project(manifest: &str, files: &[&str]) -> (tempfile::TempDir, MagnetManifestV1) {
        let dir = tempfile::tempdir().unwrap();
        let manifest = format!(
            "magnet-version = 1\n\n[package]\nname = \"foo\"\nversion = \"0.1.0\"\n{manifest}"
        );
        fs::write(dir.path().join(MAGNET_PROJECT_FILE), &manifest).unwrap();
        for file in files {
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file).unwrap();
        }
        (dir, MagnetManifestV1::from_string(&manifest).unwrap())
    }

    fn paths(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    const QUARK_FILES: &[&str] = &[
        "Classes/Foo.sc",
        "Classes/Nested/Bar.sc",
        "Classes/notes.txt",
        "HelpSource/Classes/Foo.schelp",
        "Root.sc",
        "test.scd",
    ];

    #[test]
    fn test_quark_mirrors_global_paths() {
        let (dir, manifest) = project(
            "kind = \"quark\"\n[artifact]\nglobal-paths = [\"Classes/**.sc\", \"HelpSource/**/*\"]\n",
            QUARK_FILES,
        );
        let files = collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).unwrap();
        assert_eq!(
            files,
            PackageFiles {
                files: paths(&[
                    "Classes/Foo.sc",
                    "Classes/Nested/Bar.sc",
                    "HelpSource/Classes/Foo.schelp",
                    "magnet.toml",
                ]),
                binaries: vec![],
            }
        );
    }

    #[test]
    fn test_star_stays_within_folder() {
        let (dir, manifest) = project(
            "kind = \"quark\"\n[artifact]\nglobal-paths = [\"*.sc\", \"Classes/*.sc\"]\n",
            QUARK_FILES,
        );
        let files = collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).unwrap();
        assert_eq!(
            files.files,
            paths(&["Classes/Foo.sc", "Root.sc", "magnet.toml"])
        );
    }

    #[test]
    fn test_overlapping_global_paths_are_packaged_once() {
        let (dir, manifest) = project(
            "kind = \"quark\"\n[artifact]\nglobal-paths = [\"Classes/**.sc\", \"Classes/Foo.sc\", \"./Classes/*\"]\n",
            QUARK_FILES,
        );
        let files = collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).unwrap();
        assert_eq!(
            files.files,
            paths(&[
                "Classes/Foo.sc",
                "Classes/Nested/Bar.sc",
                "Classes/notes.txt",
                "magnet.toml"
            ])
        );
    }

    #[test]
    fn test_exclude() {
        let (dir, manifest) = project(
            "kind = \"quark\"\n[artifact]\nglobal-paths = [\"Classes/**\"]\nexclude = [\"**/*.txt\", \"Classes/Nested/**\"]\n",
            QUARK_FILES,
        );
        let files = collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).unwrap();
        assert_eq!(files.files, paths(&["Classes/Foo.sc", "magnet.toml"]));
    }

    #[test]
    fn test_never_packaged() {
        let (dir, manifest) = project(
            "kind = \"quark\"\n[artifact]\nglobal-paths = [\"**\"]\n",
            &[
                "Foo.sc",
                ".git/config",
                ".DS_Store",
                "Classes/.DS_Store",
                ".github/workflows/ci.yml",
                "dist/foo-0.1.0-quark-any-any.tar.gz",
                "external/quarks/bar/Bar.sc",
                "sclang_conf.yaml",
                // only the project's own dist/ and external/ are skipped
                "Classes/external/Baz.sc",
            ],
        );
        let files = collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).unwrap();
        assert_eq!(
            files.files,
            paths(&["Classes/external/Baz.sc", "Foo.sc", "magnet.toml"])
        );
    }

    #[test]
    fn test_unmatched_glob_fails() {
        let (dir, manifest) = project(
            "kind = \"quark\"\n[artifact]\nglobal-paths = [\"Classes/**.sc\", \"Clases/*.sc\", \".github/*\"]\n",
            QUARK_FILES,
        );
        let err = collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).unwrap_err();
        assert!(
            err.to_string().contains("`Clases/*.sc`, `.github/*`"),
            "{err}"
        );
    }

    #[test]
    fn test_glob_matching_only_excluded_files_is_fine() {
        let (dir, manifest) = project(
            "kind = \"quark\"\n[artifact]\nglobal-paths = [\"Classes/**.sc\", \"*.scd\"]\nexclude = [\"test.scd\"]\n",
            QUARK_FILES,
        );
        let files = collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).unwrap();
        assert!(!files.files.contains(&PathBuf::from("test.scd")));
    }

    #[test]
    fn test_readme() {
        let (dir, manifest) = project(
            "kind = \"quark\"\n[artifact]\nglobal-paths = [\"*.sc\"]\n",
            &["Root.sc", "README.md"],
        );
        let files = collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).unwrap();
        assert_eq!(files.files, paths(&["README.md", "Root.sc", "magnet.toml"]));

        let (dir, manifest) = project(
            "kind = \"quark\"\n[metadata]\nreadme = \"docs/intro.md\"\n[artifact]\nglobal-paths = [\"*.sc\"]\nexclude = [\"docs/**\"]\n",
            &["Root.sc", "README.md", "docs/intro.md"],
        );
        let files = collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).unwrap();
        assert_eq!(
            files.files,
            paths(&["Root.sc", "docs/intro.md", "magnet.toml"])
        );

        let (dir, manifest) = project(
            "kind = \"quark\"\n[metadata]\nreadme = \"MISSING.md\"\n[artifact]\nglobal-paths = [\"*.sc\"]\n",
            &["Root.sc"],
        );
        assert!(collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).is_err());
    }

    const LEPTON: &str = "kind = \"lepton\"\n[artifact]\nglobal-paths = [\"Classes/**.sc\"]\nplatform-paths = { platform = \"macos\", paths = [\"build/*.scx\"] }\nsource-paths = [\"Classes/**\", \"src/**\", \"CMakeLists.txt\"]\n";
    const LEPTON_FILES: &[&str] = &[
        "Classes/Foo.sc",
        "src/Foo.cpp",
        "CMakeLists.txt",
        "build/Foo.scx",
        "build/CMakeCache.txt",
    ];

    #[test]
    fn test_lepton_artifact() {
        let (dir, manifest) = project(LEPTON, LEPTON_FILES);
        let files = collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).unwrap();
        assert_eq!(
            files,
            PackageFiles {
                files: paths(&["Classes/Foo.sc", "magnet.toml"]),
                binaries: paths(&["build/Foo.scx"]),
            }
        );
    }

    #[test]
    fn test_lepton_source() {
        let (dir, manifest) = project(LEPTON, LEPTON_FILES);
        let files = collect_files(dir.path(), &manifest, &BundleType::SourceBundle).unwrap();
        assert_eq!(
            files,
            PackageFiles {
                files: paths(&[
                    "CMakeLists.txt",
                    "Classes/Foo.sc",
                    "magnet.toml",
                    "src/Foo.cpp"
                ]),
                binaries: vec![],
            }
        );
    }

    #[test]
    fn test_source_paths_required() {
        let (dir, manifest) = project(
            "kind = \"lepton\"\n[artifact]\nglobal-paths = [\"Classes/**.sc\"]\n",
            LEPTON_FILES,
        );
        let err = collect_files(dir.path(), &manifest, &BundleType::SourceBundle).unwrap_err();
        assert!(err.to_string().contains("source-paths"), "{err}");
    }

    #[test]
    fn test_quark_has_no_source_or_binaries() {
        let (dir, manifest) = project(
            "kind = \"quark\"\n[artifact]\nglobal-paths = [\"Classes/**.sc\"]\n",
            QUARK_FILES,
        );
        assert!(collect_files(dir.path(), &manifest, &BundleType::SourceBundle).is_err());

        let (dir, manifest) = project(
            "kind = \"quark\"\n[artifact]\nglobal-paths = [\"Classes/**.sc\"]\nplatform-paths = { platform = \"macos\", paths = [\"build/*.scx\"] }\n",
            LEPTON_FILES,
        );
        let err = collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).unwrap_err();
        assert!(err.to_string().contains("platform-paths"), "{err}");
    }

    #[test]
    fn test_global_and_platform_paths_overlap() {
        let (dir, manifest) = project(
            "kind = \"lepton\"\n[artifact]\nglobal-paths = [\"Classes/**.sc\", \"build/**\"]\nplatform-paths = { platform = \"macos\", paths = [\"build/*.scx\"] }\n",
            LEPTON_FILES,
        );
        let err = collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).unwrap_err();
        assert!(err.to_string().contains("build/Foo.scx"), "{err}");
    }

    #[test]
    fn test_missing_global_paths() {
        let (dir, manifest) = project("kind = \"quark\"\n", QUARK_FILES);
        assert!(collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_symlinks_are_rejected() {
        let (dir, manifest) = project(
            "kind = \"quark\"\n[artifact]\nglobal-paths = [\"Classes/**.sc\"]\n",
            QUARK_FILES,
        );
        std::os::unix::fs::symlink(
            dir.path().join("Root.sc"),
            dir.path().join("Classes/Link.sc"),
        )
        .unwrap();
        let err = collect_files(dir.path(), &manifest, &BundleType::ArtifactBundle).unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err}");
    }
}
