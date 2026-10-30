use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    str::FromStr,
};

use anyhow::{bail, ensure, Context};
use git2::{ObjectType, Repository, Status, StatusOptions};
use globset::{GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};
use tracing::{debug, instrument, trace, warn};
use walkdir::WalkDir;

use crate::{
    config::constants::MAGNET_PROJECT_FILE,
    models::{
        directories::ProjectDirectory,
        language_config::SCLANG_CONF_FILE,
        package::{ArtifactKind, ArtifactTags},
        project_manifest::{MagnetManifestV1, MagnetManifestV1ArtifactSourceFrom, Platform},
    },
};

/// readme which gets packaged if the manifest doesn't name one
const DEFAULT_README: &str = "README.md";

/// The files of one archive, relative to the project root and sorted
#[derive(Debug, Default, PartialEq)]
pub struct PackageFiles {
    /// files which keep their path within the archive
    pub files: Vec<PathBuf>,
    /// binaries of `[artifact.lepton]`, which go into `plugins/<platform>/<abi>/`
    pub binaries: Vec<PathBuf>,
}

/// Selects the files of the artifact of `kind` via the manifest's `[artifact]` tables,
/// see DESIGN.md "Packaging": the files of the source (tracked by git, or its `paths`), of the
/// quark, or the binaries of the lepton for the platform of `tags`.
/// Hidden files (except the ones of a source from git), `dist/`, `magnet_packages/` and
/// `sclang_conf.yaml` are never selected, while `magnet.toml` and the readme are always part of
/// the artifact.
#[instrument(skip(manifest))]
pub fn collect_files(
    root: &Path,
    manifest: &MagnetManifestV1,
    kind: ArtifactKind,
    tags: &ArtifactTags,
    allow_dirty: bool,
) -> anyhow::Result<PackageFiles> {
    let exclude = glob_set(manifest.exclude())?;
    let (mut files, binaries) = match kind {
        ArtifactKind::Source => {
            let source = manifest.source();
            let files = match source.from {
                MagnetManifestV1ArtifactSourceFrom::Git => {
                    git_files(root, source.submodules, &exclude, allow_dirty)?
                }
                MagnetManifestV1ArtifactSourceFrom::Paths => {
                    ensure!(
                        !source.paths.is_empty(),
                        "[artifact.source] needs `paths` with `from = \"paths\"`, which select the files of the source artifact"
                    );
                    select_all(root, &source.paths, &exclude)?
                }
            };
            (files, BTreeSet::new())
        }
        ArtifactKind::Quark => {
            ensure!(
                manifest.artifact_kinds().contains(&ArtifactKind::Quark),
                "There is no [artifact.quark] in {MAGNET_PROJECT_FILE}, so the package has no quark artifact"
            );
            (
                select_all(root, manifest.quark_paths(), &exclude)?,
                BTreeSet::new(),
            )
        }
        ArtifactKind::Lepton => {
            let platform_paths = manifest.lepton_platform_paths().with_context(|| {
                format!("There is no [artifact.lepton] in {MAGNET_PROJECT_FILE}, so the package has no lepton artifact")
            })?;
            let globs = Platform::from_str(&tags.platform)
                .ok()
                .and_then(|platform| platform_paths.get(&platform).cloned())
                .with_context(|| {
                    format!(
                        "There are no `platform-paths` for {} in [artifact.lepton], which select the binaries of the package",
                        tags.platform
                    )
                })?;
            (BTreeSet::new(), select_all(root, &globs, &exclude)?)
        }
    };
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

/// The project files which `globs` select, where every glob needs to select a file
fn select_all(
    root: &Path,
    globs: &[String],
    exclude: &GlobSet,
) -> anyhow::Result<BTreeSet<String>> {
    let mut unmatched = vec![];
    let files = select(&project_files(root)?, globs, exclude, &mut unmatched)?;
    ensure!(
        unmatched.is_empty(),
        "The globs {} don't select any file (hidden files, dist/, magnet_packages/ and {SCLANG_CONF_FILE} are never packaged)",
        unmatched.join(", ")
    );
    Ok(files)
}

/// The files below `root` which are committed to git (`HEAD`), relative to `root`, with the
/// files of its submodules if `submodules` is set. Hidden files are part of it, `.git` never is.
/// Uncommitted changes of these files are refused unless `allow_dirty`, since the source should
/// be what the repository holds; the files are read from the working tree.
fn git_files(
    root: &Path,
    submodules: bool,
    exclude: &GlobSet,
    allow_dirty: bool,
) -> anyhow::Result<BTreeSet<String>> {
    let repo = Repository::discover(root).with_context(|| {
        format!("{root:?} is not within a git repository, so the source artifact can't take the files tracked by git. Set `from = \"paths\"` and its `paths` in [artifact.source]")
    })?;
    let workdir = repo
        .workdir()
        .context("The git repository has no working tree")?
        .canonicalize()?;
    let root = root
        .canonicalize()
        .with_context(|| format!("Could not resolve {root:?}"))?;
    let prefix = relative_path(root.strip_prefix(&workdir)?)?;

    if !allow_dirty {
        let mut options = StatusOptions::new();
        options
            .include_untracked(false)
            .include_ignored(false)
            .exclude_submodules(!submodules);
        if !prefix.is_empty() {
            options.pathspec(&prefix);
        }
        let dirty: Vec<String> = repo
            .statuses(Some(&mut options))?
            .iter()
            .filter(|entry| entry.status() != Status::CURRENT)
            .filter_map(|entry| entry.path().map(String::from))
            .collect();
        ensure!(
            dirty.is_empty(),
            "The source artifact takes the files committed to git, but these have uncommitted changes: {}. Commit them, or package anyway with --allow-dirty",
            dirty.join(", ")
        );
    }

    let head = repo
        .head()
        .and_then(|head| head.peel_to_tree())
        .context("The git repository has no commit yet, so the source artifact would be empty")?;
    let tree = match prefix.is_empty() {
        true => head,
        false => head
            .get_path(Path::new(&prefix))
            .and_then(|entry| entry.to_object(&repo))
            .and_then(|object| object.peel_to_tree())
            .with_context(|| {
                format!(
                    "{root:?} has no files committed to the git repository at {workdir:?} (is it ignored there?). Commit them, or set `from = \"paths\"` and its `paths` in [artifact.source]"
                )
            })?,
    };
    let mut tracked = vec![];
    tracked_files(&repo, &tree, &prefix, "", submodules, &mut tracked)?;

    let mut files = BTreeSet::new();
    for path in tracked {
        if is_never_packaged(&path) || exclude.is_match(&path) {
            trace!(path, "Not part of the source");
            continue;
        }
        if !root.join(&path).symlink_metadata().is_ok() {
            // only possible with --allow-dirty
            warn!("{path} is committed to git but missing, so it isn't part of the source");
            continue;
        }
        files.insert(path);
    }
    Ok(files)
}

/// The blobs of `tree` as `/`-separated paths with `dir` in front, descending into submodules
/// (whose checked out commit counts) if `submodules` is set. `repo_dir` is the path of `tree`
/// within `repo`, which submodules are looked up by.
fn tracked_files(
    repo: &Repository,
    tree: &git2::Tree,
    repo_dir: &str,
    dir: &str,
    submodules: bool,
    files: &mut Vec<String>,
) -> anyhow::Result<()> {
    let join = |base: &str, name: &str| match base.is_empty() {
        true => name.to_string(),
        false => format!("{base}/{name}"),
    };
    for entry in tree.iter() {
        let name = entry
            .name()
            .with_context(|| format!("A file in {dir:?} has a name which is not valid UTF-8"))?;
        let path = join(dir, name);
        match entry.kind() {
            Some(ObjectType::Blob) => files.push(path),
            Some(ObjectType::Tree) => {
                let subtree = entry.to_object(repo)?.peel_to_tree()?;
                tracked_files(
                    repo,
                    &subtree,
                    &join(repo_dir, name),
                    &path,
                    submodules,
                    files,
                )?;
            }
            // a submodule, whose files are in its own repository
            Some(ObjectType::Commit) if submodules => {
                let submodule_path = join(repo_dir, name);
                let submodule = repo
                    .find_submodule(&submodule_path)
                    .and_then(|submodule| submodule.open())
                    .with_context(|| {
                        format!("Could not open the submodule {submodule_path}, is it checked out?")
                    })?;
                let subtree = submodule.head()?.peel_to_tree()?;
                tracked_files(&submodule, &subtree, "", &path, submodules, files)?;
            }
            _ => trace!(path, "Skipped"),
        }
    }
    Ok(())
}

/// The binaries of `platform-paths` for `platform`, as for an artifact, and the globs which
/// select no file (e.g. if the plugin isn't built yet)
pub fn collect_binaries(
    root: &Path,
    manifest: &MagnetManifestV1,
    platform: &Platform,
) -> anyhow::Result<(Vec<PathBuf>, Vec<String>)> {
    let globs = manifest
        .lepton_platform_paths()
        .and_then(|platform_paths| platform_paths.get(platform).cloned())
        .unwrap_or_default();
    let mut unmatched = vec![];
    let binaries = select(
        &project_files(root)?,
        &globs,
        &glob_set(manifest.exclude())?,
        &mut unmatched,
    )?;
    Ok((binaries.into_iter().map(PathBuf::from).collect(), unmatched))
}

/// The files and folders at the project root which are never packaged
fn never_packaged() -> [String; 3] {
    [
        String::from(ProjectDirectory::Dist),
        String::from(ProjectDirectory::Packages),
        SCLANG_CONF_FILE.to_string(),
    ]
}

/// `true` for a `/`-separated path within one of [never_packaged]
fn is_never_packaged(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or_default();
    never_packaged().iter().any(|name| name == first)
}

/// All files (and links) of the project which may be packaged, as `/`-separated paths
/// relative to `root`
fn project_files(root: &Path) -> anyhow::Result<Vec<String>> {
    let never_packaged = never_packaged();
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

    fn quark(dir: &tempfile::TempDir, manifest: &MagnetManifestV1) -> anyhow::Result<PackageFiles> {
        collect_files(
            dir.path(),
            manifest,
            ArtifactKind::Quark,
            &ArtifactTags::any(),
            false,
        )
    }

    fn lepton(
        dir: &tempfile::TempDir,
        manifest: &MagnetManifestV1,
        tags: &ArtifactTags,
    ) -> anyhow::Result<PackageFiles> {
        collect_files(dir.path(), manifest, ArtifactKind::Lepton, tags, false)
    }

    fn source(
        root: &Path,
        manifest: &MagnetManifestV1,
        allow_dirty: bool,
    ) -> anyhow::Result<PackageFiles> {
        collect_files(
            root,
            manifest,
            ArtifactKind::Source,
            &ArtifactTags::any(),
            allow_dirty,
        )
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
    fn test_quark_mirrors_its_paths() {
        let (dir, manifest) = project(
            "[artifact.quark]\npaths = [\"Classes/**.sc\", \"HelpSource/**/*\"]\n",
            QUARK_FILES,
        );
        let files = quark(&dir, &manifest).unwrap();
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
            "[artifact.quark]\npaths = [\"*.sc\", \"Classes/*.sc\"]\n",
            QUARK_FILES,
        );
        let files = quark(&dir, &manifest).unwrap();
        assert_eq!(
            files.files,
            paths(&["Classes/Foo.sc", "Root.sc", "magnet.toml"])
        );
    }

    #[test]
    fn test_overlapping_quark_paths_are_packaged_once() {
        let (dir, manifest) = project(
            "[artifact.quark]\npaths = [\"Classes/**.sc\", \"Classes/Foo.sc\", \"./Classes/*\"]\n",
            QUARK_FILES,
        );
        let files = quark(&dir, &manifest).unwrap();
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
            "[artifact]\nexclude = [\"**/*.txt\", \"Classes/Nested/**\"]\n[artifact.quark]\npaths = [\"Classes/**\"]\n",
            QUARK_FILES,
        );
        let files = quark(&dir, &manifest).unwrap();
        assert_eq!(files.files, paths(&["Classes/Foo.sc", "magnet.toml"]));
    }

    #[test]
    fn test_never_packaged() {
        let (dir, manifest) = project(
            "[artifact.quark]\npaths = [\"**\"]\n",
            &[
                "Foo.sc",
                ".git/config",
                ".DS_Store",
                "Classes/.DS_Store",
                ".github/workflows/ci.yml",
                "dist/foo-0.1.0-quark-any-any.tar.gz",
                "magnet_packages/quarks/bar/Bar.sc",
                "sclang_conf.yaml",
                // only the project's own dist/ and magnet_packages/ are skipped
                "Classes/magnet_packages/Baz.sc",
                // e.g. the C++ dependencies of a lepton
                "external/libfoo/foo.cpp",
            ],
        );
        let files = quark(&dir, &manifest).unwrap();
        assert_eq!(
            files.files,
            paths(&[
                "Classes/magnet_packages/Baz.sc",
                "Foo.sc",
                "external/libfoo/foo.cpp",
                "magnet.toml"
            ])
        );
    }

    #[test]
    fn test_unmatched_glob_fails() {
        let (dir, manifest) = project(
            "[artifact.quark]\npaths = [\"Classes/**.sc\", \"Clases/*.sc\", \".github/*\"]\n",
            QUARK_FILES,
        );
        let err = quark(&dir, &manifest).unwrap_err();
        assert!(
            err.to_string().contains("`Clases/*.sc`, `.github/*`"),
            "{err}"
        );
    }

    #[test]
    fn test_glob_matching_only_excluded_files_is_fine() {
        let (dir, manifest) = project(
            "[artifact]\nexclude = [\"test.scd\"]\n[artifact.quark]\npaths = [\"Classes/**.sc\", \"*.scd\"]\n",
            QUARK_FILES,
        );
        let files = quark(&dir, &manifest).unwrap();
        assert!(!files.files.contains(&PathBuf::from("test.scd")));
    }

    #[test]
    fn test_readme() {
        let (dir, manifest) = project(
            "[artifact.quark]\npaths = [\"*.sc\"]\n",
            &["Root.sc", "README.md"],
        );
        let files = quark(&dir, &manifest).unwrap();
        assert_eq!(files.files, paths(&["README.md", "Root.sc", "magnet.toml"]));

        let (dir, manifest) = project(
            "[metadata]\nreadme = \"docs/intro.md\"\n[artifact]\nexclude = [\"docs/**\"]\n[artifact.quark]\npaths = [\"*.sc\"]\n",
            &["Root.sc", "README.md", "docs/intro.md"],
        );
        let files = quark(&dir, &manifest).unwrap();
        assert_eq!(
            files.files,
            paths(&["Root.sc", "docs/intro.md", "magnet.toml"])
        );

        let (dir, manifest) = project(
            "[metadata]\nreadme = \"MISSING.md\"\n[artifact.quark]\npaths = [\"*.sc\"]\n",
            &["Root.sc"],
        );
        assert!(quark(&dir, &manifest).is_err());
    }

    const LEPTON: &str = "[artifact.source]\nfrom = \"paths\"\npaths = [\"Classes/**\", \"src/**\", \"CMakeLists.txt\"]\n[artifact.quark]\npaths = [\"Classes/**.sc\"]\n[artifact.lepton]\nplatform-paths = { macos_arm64 = [\"build/*.scx\"] }\n";
    const LEPTON_FILES: &[&str] = &[
        "Classes/Foo.sc",
        "src/Foo.cpp",
        "CMakeLists.txt",
        "build/Foo.scx",
        "build/CMakeCache.txt",
    ];

    fn macos_arm64() -> ArtifactTags {
        ArtifactTags::new("api3_0", "macos_arm64").unwrap()
    }

    /// the classes are part of the quark artifact, the lepton only has the binaries
    #[test]
    fn test_lepton_artifact() {
        let (dir, manifest) = project(LEPTON, LEPTON_FILES);
        let files = lepton(&dir, &manifest, &macos_arm64()).unwrap();
        assert_eq!(
            files,
            PackageFiles {
                files: paths(&["magnet.toml"]),
                binaries: paths(&["build/Foo.scx"]),
            }
        );
        let files = quark(&dir, &manifest).unwrap();
        assert_eq!(files.files, paths(&["Classes/Foo.sc", "magnet.toml"]));
        assert!(files.binaries.is_empty());
    }

    #[test]
    fn test_binaries_of_the_platform() {
        let (dir, manifest) = project(
            "[artifact.quark]\npaths = [\"Classes/**.sc\"]\n[artifact.lepton.platform-paths]\nmacos_arm64 = [\"build/*.scx\"]\nlinux_x86_64 = [\"build/linux/*.{scx,so}\"]",
            &["Classes/Foo.sc", "build/Foo.scx", "build/linux/Foo.scx", "build/linux/libdep.so"],
        );
        let files = lepton(&dir, &manifest, &macos_arm64()).unwrap();
        assert_eq!(files.binaries, paths(&["build/Foo.scx"]));
        let linux = ArtifactTags::new("api3_0", "linux_x86_64").unwrap();
        let files = lepton(&dir, &manifest, &linux).unwrap();
        assert_eq!(
            files.binaries,
            paths(&["build/linux/Foo.scx", "build/linux/libdep.so"])
        );

        for platform in ["windows_x86_64", "any"] {
            let tags = ArtifactTags::new("api3_0", platform).unwrap();
            let err = lepton(&dir, &manifest, &tags).unwrap_err();
            assert!(
                err.to_string()
                    .contains(&format!("no `platform-paths` for {platform}")),
                "{err}"
            );
        }
    }

    #[test]
    fn test_unknown_platform_in_manifest() {
        let manifest = "magnet-version = 1\n[package]\nname = \"foo\"\nversion = \"1.0.0\"\n[artifact.lepton]\nplatform-paths = { macos = [\"build/*.scx\"] }\n";
        let err = format!("{:#}", MagnetManifestV1::from_string(manifest).unwrap_err());
        assert!(err.contains("macos_arm64"), "{err}");
    }

    #[test]
    fn test_source_of_paths() {
        let (dir, manifest) = project(LEPTON, LEPTON_FILES);
        let files = source(dir.path(), &manifest, false).unwrap();
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
            "[artifact.source]\nfrom = \"paths\"\n[artifact.quark]\npaths = [\"Classes/**.sc\"]\n",
            QUARK_FILES,
        );
        let err = source(dir.path(), &manifest, false).unwrap_err();
        assert!(err.to_string().contains("needs `paths`"), "{err}");
    }

    /// an artifact needs the table of its kind
    #[test]
    fn test_undeclared_kinds() {
        let (dir, manifest) = project(
            "[artifact.quark]\npaths = [\"Classes/**.sc\"]\n",
            QUARK_FILES,
        );
        let err = lepton(&dir, &manifest, &macos_arm64()).unwrap_err();
        assert!(err.to_string().contains("no [artifact.lepton]"), "{err}");

        // a lepton without classes
        let (dir, manifest) = project(
            "[artifact.lepton]\nplatform-paths = { macos_arm64 = [\"build/*.scx\"] }\n",
            LEPTON_FILES,
        );
        let err = quark(&dir, &manifest).unwrap_err();
        assert!(err.to_string().contains("no [artifact.quark]"), "{err}");
        let files = lepton(&dir, &manifest, &macos_arm64()).unwrap();
        assert_eq!(files.binaries, paths(&["build/Foo.scx"]));
    }

    #[test]
    fn test_missing_quark_paths() {
        let (dir, manifest) = project("", QUARK_FILES);
        assert!(quark(&dir, &manifest).is_err());
    }

    const GIT_QUARK: &str = "[artifact.quark]\npaths = [\"Classes/**.sc\"]\n";

    /// a git repository at `repo` with the project in `project` (relative to it), whose files
    /// are all committed
    fn git_project(repo: &Path, project: &str, files: &[(&str, &str)]) -> MagnetManifestV1 {
        let manifest = format!(
            "magnet-version = 1\n[package]\nname = \"foo\"\nversion = \"0.1.0\"\n{GIT_QUARK}"
        );
        let mut all: Vec<(String, String)> = files
            .iter()
            .map(|(file, content)| (file.to_string(), content.to_string()))
            .collect();
        let toml = match project.is_empty() {
            true => MAGNET_PROJECT_FILE.to_string(),
            false => format!("{project}/{MAGNET_PROJECT_FILE}"),
        };
        all.push((toml, manifest.clone()));
        let all: Vec<(&str, &str)> = all.iter().map(|(f, c)| (f.as_str(), c.as_str())).collect();
        crate::git_client::source::tests::commit_files(repo, &all);
        MagnetManifestV1::from_string(&manifest).unwrap()
    }

    #[test]
    fn test_source_from_git() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = git_project(
            dir.path(),
            "",
            &[
                ("Classes/Foo.sc", "Foo {}"),
                ("src/Foo.cpp", ""),
                ("CMakeLists.txt", ""),
                (".gitignore", "build/\n"),
                (".github/workflows/ci.yml", ""),
                ("build/Foo.scx", ""),
            ],
        );
        // neither untracked nor ignored files belong to the source, nor make it dirty
        fs::write(dir.path().join("notes.txt"), "").unwrap();
        let files = source(dir.path(), &manifest, false).unwrap();
        assert_eq!(
            files.files,
            paths(&[
                ".github/workflows/ci.yml",
                ".gitignore",
                "CMakeLists.txt",
                "Classes/Foo.sc",
                "magnet.toml",
                "src/Foo.cpp",
            ])
        );
        assert!(files.binaries.is_empty());
    }

    #[test]
    fn test_source_from_git_refuses_uncommitted_changes() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = git_project(dir.path(), "", &[("Classes/Foo.sc", "Foo {}")]);
        fs::write(dir.path().join("Classes/Foo.sc"), "Foo { bar {} }").unwrap();
        let err = source(dir.path(), &manifest, false).unwrap_err();
        assert!(err.to_string().contains("Classes/Foo.sc"), "{err}");
        assert!(err.to_string().contains("--allow-dirty"), "{err}");

        // with --allow-dirty the files of the working tree get packaged, deleted ones not
        fs::remove_file(dir.path().join("Classes/Foo.sc")).unwrap();
        let files = source(dir.path(), &manifest, true).unwrap();
        assert_eq!(files.files, paths(&["magnet.toml"]));
    }

    /// a project within a bigger repository only takes its own folder, and only its changes
    /// count
    #[test]
    fn test_source_from_git_in_sub_folder() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = git_project(
            dir.path(),
            "plugins/foo",
            &[
                ("plugins/foo/Classes/Foo.sc", "Foo {}"),
                ("plugins/bar/Bar.sc", "Bar {}"),
                ("README.md", ""),
            ],
        );
        fs::write(dir.path().join("plugins/bar/Bar.sc"), "changed").unwrap();
        let files = source(&dir.path().join("plugins/foo"), &manifest, false).unwrap();
        assert_eq!(files.files, paths(&["Classes/Foo.sc", "magnet.toml"]));

        // a project in a folder of the repository which isn't committed, e.g. an ignored one
        let uncommitted = dir.path().join("build/foo");
        fs::create_dir_all(&uncommitted).unwrap();
        let err = source(&uncommitted, &manifest, false).unwrap_err();
        assert!(err.to_string().contains("has no files committed"), "{err}");
    }

    #[test]
    fn test_source_from_git_needs_a_repository() {
        let (dir, manifest) = project(GIT_QUARK, QUARK_FILES);
        let err = source(dir.path(), &manifest, false).unwrap_err();
        assert!(err.to_string().contains("from = \"paths\""), "{err}");
    }

    #[test]
    fn test_source_from_git_with_submodules() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library");
        crate::git_client::source::tests::commit_files(&library, &[("lib.h", "")]);
        let project = dir.path().join("project");
        let manifest = git_project(&project, "", &[("Classes/Foo.sc", "Foo {}")]);
        let repo = Repository::open(&project).unwrap();
        let url = format!("file://{}", library.display());
        let mut submodule = repo
            .submodule(&url, Path::new("external/library"), true)
            .unwrap();
        submodule.clone(None).unwrap();
        submodule.add_finalize().unwrap();
        crate::git_client::source::tests::commit_files(&project, &[]);

        let files = source(&project, &manifest, false).unwrap();
        assert!(
            !files
                .files
                .contains(&PathBuf::from("external/library/lib.h")),
            "{files:?}"
        );
        let with_submodules = MagnetManifestV1::from_string(&format!(
            "magnet-version = 1\n[package]\nname = \"foo\"\nversion = \"0.1.0\"\n{GIT_QUARK}[artifact.source]\nsubmodules = true\n"
        ))
        .unwrap();
        let files = source(&project, &with_submodules, false).unwrap();
        assert!(
            files
                .files
                .contains(&PathBuf::from("external/library/lib.h")),
            "{files:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_symlinks_are_rejected() {
        let (dir, manifest) = project(
            "[artifact.quark]\npaths = [\"Classes/**.sc\"]\n",
            QUARK_FILES,
        );
        std::os::unix::fs::symlink(
            dir.path().join("Root.sc"),
            dir.path().join("Classes/Link.sc"),
        )
        .unwrap();
        let err = quark(&dir, &manifest).unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err}");
    }
}
