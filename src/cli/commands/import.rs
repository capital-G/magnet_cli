use std::{fs, path::PathBuf, str::FromStr};

use anyhow::Context;
use clap::Parser;
use inquire::{Select, Text};
use magnet::{
    config::constants::{magnet_home, GIT_DIR},
    git_client::{
        client::GitClient,
        source::{CommitInfo, GitCache, GitReference},
    },
    legacy::{
        directory::{parse_refspec, quarks_directory_url, QuarksDirectory},
        quark_parser::{parse_dependencies, parse_fields, QuarkDependency},
    },
    models::{
        project::Project,
        project_manifest::{
            MagnetManifestV1, MagnetManifestV1DependenciesValue,
            MagnetManifestV1DependenciesValueObjectGit, ProjectLayout,
        },
    },
};
use pubgrub::SemanticVersion;
use tracing::warn;
use url::Url;

use crate::cli::{
    commands::project::{
        ask_create_options, ask_manifest, ask_project_name, check_project_dir, create_project,
        downloader, InitDefaults,
    },
    CliContext, MagnetSubCommand,
};

/// The keys of a `.quark` file which become answers of the init dialog or dependencies;
/// `path` is the folder name in the legacy directory, which magnet doesn't need
const IMPORTED_KEYS: [&str; 8] = [
    "name",
    "summary",
    "author",
    "license",
    "version",
    "url",
    "dependencies",
    "path",
];

#[derive(Debug, Parser)]
pub struct ImportLegacyQuarkArgs {
    /// The `.quark` file of the legacy quark. The project gets created in its folder, with
    /// the values of the file as answers of the `magnet init` dialog.
    quark_file: PathBuf,

    /// Only write magnet.toml, don't resolve and install the dependencies
    #[arg(long)]
    no_install: bool,
}

impl MagnetSubCommand for ImportLegacyQuarkArgs {
    async fn run(self, ctx: &CliContext) -> anyhow::Result<()> {
        let quark_file = std::path::absolute(&self.quark_file)?;
        let content = fs::read_to_string(&quark_file)
            .with_context(|| format!("Could not read {quark_file:?}"))?;
        let project_path = quark_file
            .parent()
            .context("The .quark file has no folder")?
            .to_path_buf();
        check_project_dir(&project_path, false)?;
        // before any question, since installing needs it; with --no-install, the dependencies
        // can only be legacy quarks without an index
        let index = match self.no_install {
            false => Some(ctx.git_client().context(
                "The dependencies get installed from the index, pass --no-install to only write magnet.toml",
            )?),
            true => match ctx.git_client() {
                Ok(index) => Some(index),
                Err(err) => {
                    warn!("{err:#}, so the dependencies can only be legacy quarks");
                    None
                }
            },
        };

        let fields = parse_fields(&content)
            .map_err(|err| anyhow::anyhow!("Could not read {quark_file:?}: {err}"))?;
        let field = |key: &str| fields.get(key).cloned().flatten();
        let file_stem = quark_file
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string());
        let defaults = InitDefaults {
            version: field("version").and_then(|version| legacy_version(&version)),
            summary: field("summary"),
            authors: field("author").map(|authors| legacy_authors(&authors)),
            license: field("license"),
            repository: field("url"),
        };

        let default_name = field("name")
            .into_iter()
            .chain(file_stem)
            .find(|name| MagnetManifestV1::is_valid_name(name));
        let project_name = ask_project_name(default_name.as_deref())?;
        let mut manifest = ask_manifest(project_name, &defaults)?;

        let dependencies = match parse_dependencies(&content) {
            Ok(dependencies) => dependencies,
            Err(err) => {
                warn!("Could not read the dependencies of the .quark file ({err}), add them to magnet.toml by hand");
                vec![]
            }
        };
        let cache = GitCache::new(magnet_home()?.join(GIT_DIR));
        let directory = QuarksDirectory::new(quarks_directory_url()?, &cache);
        for dependency in &dependencies {
            if manifest.dependencies.contains_key(&dependency.name) {
                continue;
            }
            if let Some(value) = ask_dependency(dependency, index.as_ref(), &directory, &cache)? {
                manifest.dependencies.insert(dependency.name.clone(), value);
            }
        }

        let options = ask_create_options(&project_path, ProjectLayout::LegacyQuark)?;
        create_project(&project_path, manifest, options)?;

        let skipped: Vec<&str> = fields
            .keys()
            .map(String::as_str)
            .filter(|key| !IMPORTED_KEYS.contains(key))
            .collect();
        if !skipped.is_empty() {
            println!(
                "Not carried over from the .quark file (magnet has no field for them, and doesn't run code like isCompatible): {}",
                skipped.join(", ")
            );
        }

        match index {
            Some(index) if !self.no_install => {
                let mut project = Project::open(&project_path)?;
                project
                    .install(&index, &downloader(&index)?, false)
                    .await
                    .context("The project is created, but installing failed. Run `magnet install` to try again")?;
            }
            _ if !dependencies.is_empty() => {
                println!("Run `magnet install` to install the dependencies")
            }
            _ => {}
        }
        Ok(())
    }
}

/// Where a dependency of the `.quark` file comes from
#[derive(Debug, Clone, Copy, PartialEq)]
enum DependencySource {
    MagnetPackage,
    LegacyQuark,
}

impl std::fmt::Display for DependencySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DependencySource::MagnetPackage => {
                write!(
                    f,
                    "the magnet package (recommended, has versions magnet can resolve)"
                )
            }
            DependencySource::LegacyQuark => write!(f, "the legacy quark, pinned to a commit"),
        }
    }
}

/// Asks how a dependency of the `.quark` file becomes a dependency of the manifest: the magnet
/// package of the index with a chosen version, if there is one, otherwise (or by choice) the
/// legacy quark pinned to a full commit SHA, which publishing needs. `None` leaves it out.
fn ask_dependency(
    dependency: &QuarkDependency,
    index: Option<&GitClient>,
    directory: &QuarksDirectory,
    cache: &GitCache,
) -> anyhow::Result<Option<MagnetManifestV1DependenciesValue>> {
    let name = &dependency.name;
    let mut versions: Vec<SemanticVersion> = match index {
        Some(index) => index
            .get_releases(name)?
            .unwrap_or_default()
            .into_iter()
            .filter(|release| !release.yanked)
            .map(|release| release.version)
            .collect(),
        None => vec![],
    };
    versions.sort_by(|a, b| b.cmp(a));
    if !versions.is_empty() {
        let source = Select::new(
            &format!("{name} is also a magnet package. Use"),
            vec![
                DependencySource::MagnetPackage,
                DependencySource::LegacyQuark,
            ],
        )
        .prompt()?;
        if source == DependencySource::MagnetPackage {
            let version = Select::new(&format!("Version of {name}:"), versions)
                .with_help_message("Written as ^<version>, like `magnet add` does")
                .prompt()?;
            return Ok(Some(MagnetManifestV1DependenciesValue::String(format!(
                "^{version}"
            ))));
        }
    }
    ask_legacy_quark(
        name,
        dependency.url.as_ref(),
        dependency.refspec.as_deref(),
        directory,
        cache,
    )
}

/// Asks for the commit a legacy quark gets pinned to, from a list of the newest commits of
/// its default branch, and returns it as `{ legacy-quark = true, git = { url, ref } }` (with a
/// full SHA, which publishing needs). The repository is `url`, otherwise the quarks
/// directory's, otherwise asked for (`None` if the author leaves it empty). Preselected is
/// the commit of `refspec` (`name@refspec` of a `.quark` file), the directory's refspec, or the
/// latest one.
pub(super) fn ask_legacy_quark(
    name: &str,
    url: Option<&Url>,
    refspec: Option<&str>,
    directory: &QuarksDirectory,
    cache: &GitCache,
) -> anyhow::Result<Option<MagnetManifestV1DependenciesValue>> {
    let entry = match url {
        Some(_) => None,
        None => directory.lookup(name)?,
    };
    let url: Url = match (url, &entry) {
        (Some(url), _) => url.clone(),
        (None, Some(entry)) => entry.url.clone(),
        (None, None) => {
            let answer = Text::new(&format!(
                "{name} is not in the quarks directory. Its git repository URL:"
            ))
            .with_help_message("Empty to leave the dependency out")
            .prompt()?;
            match optional_url(&answer)? {
                Some(url) => url,
                None => {
                    warn!("{name} is left out, add it to magnet.toml by hand");
                    return Ok(None);
                }
            }
        }
    };
    // the refspec of the .quark file wins over the directory's, like in SuperCollider
    let reference = refspec
        .map(parse_refspec)
        .or(entry.map(|entry| entry.reference))
        .unwrap_or(GitReference::DefaultBranch);
    let preselected = cache
        .resolve(&url, &reference)
        .with_context(|| format!("Could not check out {name}"))?;
    let mut commits = cache.history(&url, HISTORY_LENGTH)?;
    // e.g. an old tag of the .quark file, or a commit which isn't on the default branch
    if !commits.iter().any(|commit| commit.sha == preselected) {
        commits.insert(0, cache.commit_info(&url, &preselected)?);
    }
    let cursor = commits
        .iter()
        .position(|commit| commit.sha == preselected)
        .unwrap_or_default();
    let tag_width = commits
        .iter()
        .map(|commit| commit.tags.join(", ").len())
        .max()
        .unwrap_or_default();
    let mut choices: Vec<PinChoice> = commits
        .into_iter()
        .map(|commit| PinChoice::Commit { commit, tag_width })
        .collect();
    choices.push(PinChoice::Other);
    let help = match reference {
        GitReference::DefaultBranch => format!("{url}: the latest commit is preselected, type to filter"),
        reference => format!("{url}: {reference} (from the .quark file or the quarks directory) is preselected, type to filter"),
    };
    match Select::new(&format!("Pin {name} to:"), choices)
        .with_starting_cursor(cursor)
        .with_page_size(PAGE_SIZE)
        .with_help_message(&help)
        .prompt()?
    {
        PinChoice::Commit { commit, .. } => Ok(Some(pinned_legacy_quark(&url, &commit.sha))),
        PinChoice::Other => loop {
            let answer = Text::new(&format!("Tag or commit SHA of {name}:")).prompt()?;
            match cache.resolve(&url, &pin_reference(&answer)) {
                Ok(sha) => {
                    let commit = cache.commit_info(&url, &sha)?;
                    println!(
                        "{name}: {answer} is {} {} {}",
                        &sha[..7],
                        commit.date,
                        commit.summary
                    );
                    return Ok(Some(pinned_legacy_quark(&url, &sha)));
                }
                Err(err) => println!("{err:#}"),
            }
        },
    }
}

/// How many commits of the default branch the pin question lists
const HISTORY_LENGTH: usize = 100;
const PAGE_SIZE: usize = 12;

/// An entry of the pin question
enum PinChoice {
    Commit {
        commit: CommitInfo,
        tag_width: usize,
    },
    /// a tag or commit which isn't listed, typed by the author
    Other,
}

impl std::fmt::Display for PinChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PinChoice::Commit { commit, tag_width } => write!(
                f,
                "{}  {:tag_width$}  {} {}",
                &commit.sha[..7],
                commit.tags.join(", "),
                commit.date,
                commit.summary
            ),
            PinChoice::Other => write!(f, "Other tag or commit…"),
        }
    }
}

/// What the author typed as pin: a (possibly abbreviated) commit SHA, otherwise a refspec
/// like in a `.quark` file (a tag, with or without `tags/`)
fn pin_reference(input: &str) -> GitReference {
    let input = input.trim();
    match input.len() >= 7 && input.chars().all(|c| c.is_ascii_hexdigit()) {
        true => GitReference::Ref(input.to_lowercase()),
        false => parse_refspec(input),
    }
}

fn optional_url(input: &str) -> anyhow::Result<Option<Url>> {
    match input.trim() {
        "" => Ok(None),
        url => Ok(Some(
            url.parse()
                .with_context(|| format!("{url} is not a valid URL"))?,
        )),
    }
}

/// A legacy quark pinned to the full commit `sha` of `url`, which is what publishing needs
/// (the index line copies both)
fn pinned_legacy_quark(url: &Url, sha: &str) -> MagnetManifestV1DependenciesValue {
    MagnetManifestV1DependenciesValue::Object {
        git: Some(MagnetManifestV1DependenciesValueObjectGit {
            url: Some(url.to_string()),
            branch: None,
            ref_: Some(sha.to_string()),
            tag: None,
        }),
        ignore_quark_deps: None,
        index: None,
        legacy_quark: Some(true),
        path: None,
        version: None,
    }
}

/// A version like `1.0` or `v2` as semver (`1.0.0`, `2.0.0`), `None` if it isn't one
fn legacy_version(version: &str) -> Option<SemanticVersion> {
    let version = version.trim().trim_start_matches('v');
    let parts: Vec<&str> = version.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let padded: Vec<&str> = parts.into_iter().chain(["0", "0"]).take(3).collect();
    SemanticVersion::from_str(&padded.join(".")).ok()
}

/// Turns `Name (mail@example.org)`, a common form in `.quark` files, into the
/// `Name <mail@example.org>` of the init dialog
fn legacy_authors(authors: &str) -> String {
    authors
        .split(',')
        .map(|author| {
            let author = author.trim();
            match author
                .strip_suffix(')')
                .and_then(|author| author.rsplit_once('('))
            {
                Some((name, email)) if email.contains('@') => {
                    format!("{} <{}>", name.trim(), email.trim())
                }
                _ => author.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_legacy_version() {
        for (legacy, version) in [
            ("1.0.1", "1.0.1"),
            ("1.0", "1.0.0"),
            ("2", "2.0.0"),
            ("v0.3", "0.3.0"),
        ] {
            assert_eq!(
                legacy_version(legacy).map(|version| version.to_string()),
                Some(version.to_string()),
                "{legacy}"
            );
        }
        for legacy in ["", "1.0.0.1", "beta", "1.0-alpha"] {
            assert_eq!(legacy_version(legacy), None, "{legacy}");
        }
    }

    #[test]
    fn test_legacy_authors() {
        assert_eq!(
            legacy_authors("Hanns Holger Rutz (contact@sciss.de)"),
            "Hanns Holger Rutz <contact@sciss.de>"
        );
        assert_eq!(
            legacy_authors("A (a@x.org), B (the second)"),
            "A <a@x.org>, B (the second)"
        );
        assert_eq!(legacy_authors("scott@artificia.org"), "scott@artificia.org");
    }

    #[test]
    fn test_pinned_legacy_quark() {
        let sha = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let url: Url = "https://github.com/user/foo.git".parse().unwrap();
        // as a line of magnet.toml
        let mut table = toml_edit::Table::new();
        table.insert("foo", pinned_legacy_quark(&url, sha).to_toml_edit_item());
        assert_eq!(
            table.to_string(),
            format!(
                "foo = {{ legacy-quark = true, git = {{ ref = \"{sha}\", url = \"{url}\" }} }}\n"
            )
        );
        // publishable as it is, with the quark the import writes
        let manifest: MagnetManifestV1 = toml::from_str(&format!(
            "magnet-version = 1\n[package]\nname = \"x\"\nversion = \"0.1.0\"\n[artifact.quark]\npaths = [\"**\"]\n[dependencies]\n{table}"
        ))
        .unwrap();
        magnet::packaging::publish::check_publishable(&manifest).unwrap();
    }

    #[test]
    fn test_pin_reference() {
        let sha = "4B825DC642cb6eb9a060e54bf8d69288fbee4904";
        assert_eq!(pin_reference(sha), GitReference::Ref(sha.to_lowercase()));
        assert_eq!(
            pin_reference(" 4b825dc "),
            GitReference::Ref("4b825dc".to_string())
        );
        // like a refspec of a .quark file
        assert_eq!(
            pin_reference("tags/4.1.4"),
            GitReference::Tag("4.1.4".to_string())
        );
        assert_eq!(
            pin_reference("4.1.4"),
            GitReference::Tag("4.1.4".to_string())
        );
        assert_eq!(pin_reference("HEAD"), GitReference::DefaultBranch);
        // too short for a SHA
        assert_eq!(pin_reference("abc"), GitReference::Tag("abc".to_string()));
    }

    #[test]
    fn test_optional_url() {
        assert_eq!(optional_url("  ").unwrap(), None);
        assert_eq!(
            optional_url(" https://github.com/user/foo ")
                .unwrap()
                .unwrap()
                .as_str(),
            "https://github.com/user/foo"
        );
        assert!(optional_url("not a url").is_err());
    }
}
