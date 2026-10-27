use std::{fs, path::PathBuf, str::FromStr};

use anyhow::Context;
use clap::Parser;
use magnet::{
    git_client::source::GitReference,
    legacy::{
        directory::parse_refspec,
        quark_parser::{parse_dependencies, parse_fields, QuarkDependency},
    },
    models::project_manifest::{
        MagnetManifestV1, MagnetManifestV1DependenciesValue,
        MagnetManifestV1DependenciesValueObjectGit, MagnetManifestV1PackageKind, ProjectLayout,
    },
};
use pubgrub::SemanticVersion;
use tracing::warn;

use crate::cli::{
    commands::project::{
        ask_create_options, ask_manifest, ask_project_name, check_project_dir, create_project,
        InitDefaults,
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
}

impl MagnetSubCommand for ImportLegacyQuarkArgs {
    async fn run(self, _ctx: &CliContext) -> anyhow::Result<()> {
        let quark_file = std::path::absolute(&self.quark_file)?;
        let content = fs::read_to_string(&quark_file)
            .with_context(|| format!("Could not read {quark_file:?}"))?;
        let project_path = quark_file
            .parent()
            .context("The .quark file has no folder")?
            .to_path_buf();
        check_project_dir(&project_path, false)?;

        let fields = parse_fields(&content)
            .map_err(|err| anyhow::anyhow!("Could not read {quark_file:?}: {err}"))?;
        let field = |key: &str| fields.get(key).cloned().flatten();
        let file_stem = quark_file
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string());
        let defaults = InitDefaults {
            kind: Some(MagnetManifestV1PackageKind::Quark),
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
        for dependency in &dependencies {
            manifest
                .dependencies
                .entry(dependency.name.clone())
                .or_insert_with(|| legacy_dependency(dependency));
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
        if !dependencies.is_empty() {
            let names: Vec<&str> = dependencies.iter().map(|d| d.name.as_str()).collect();
            println!(
                "Added the dependencies {} as legacy quarks. Check whether they are available in the magnet index, \
                 and switch to them via `magnet add <name>`: magnet packages have versions which magnet can resolve, \
                 so the project stays compatible over time. Run `magnet install` to install the dependencies.",
                names.join(", ")
            );
        }
        Ok(())
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

/// A dependency of the `.quark` file as legacy quark, with its URL and refspec. Without a URL
/// (`name@refspec`), the git table only pins it and the URL comes from the quarks directory.
fn legacy_dependency(dependency: &QuarkDependency) -> MagnetManifestV1DependenciesValue {
    let mut git = MagnetManifestV1DependenciesValueObjectGit {
        url: dependency.url.as_ref().map(|url| url.to_string()),
        branch: None,
        ref_: None,
        tag: None,
    };
    // read like SuperCollider does (`Quark.validateRefspec`), which knows no branches; `HEAD`
    // is the default branch, which needs no pin
    match dependency.refspec.as_deref().map(parse_refspec) {
        Some(GitReference::Tag(tag)) => git.tag = Some(tag),
        Some(GitReference::Ref(sha)) => git.ref_ = Some(sha),
        Some(GitReference::Branch(branch)) => git.branch = Some(branch),
        Some(GitReference::DefaultBranch) | None => {}
    }
    let pinned = git.tag.is_some() || git.ref_.is_some() || git.branch.is_some();
    let git = (git.url.is_some() || pinned).then_some(git);
    MagnetManifestV1DependenciesValue::Object {
        git,
        ignore_quark_deps: None,
        index: None,
        legacy_quark: Some(true),
        path: None,
        version: None,
    }
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
    fn test_legacy_dependency() {
        let dependency = |name: &str, url: Option<&str>, refspec: Option<&str>| QuarkDependency {
            name: name.to_string(),
            url: url.map(|url| url.parse().unwrap()),
            refspec: refspec.map(String::from),
        };
        // as a line of magnet.toml
        let toml = |dependency: &QuarkDependency| {
            let mut table = toml_edit::Table::new();
            table.insert("x", legacy_dependency(dependency).to_toml_edit_item());
            table
                .to_string()
                .trim_start_matches("x = ")
                .trim_end()
                .to_string()
        };
        assert_eq!(
            toml(&dependency("Bjorklund", None, None)),
            "{ legacy-quark = true }"
        );
        // pinned by name, the URL comes from the quarks directory
        assert_eq!(
            toml(&dependency("cruciallib", None, Some("tags/4.1.4"))),
            "{ legacy-quark = true, git = { tag = \"4.1.4\" } }"
        );
        assert_eq!(
            toml(&dependency("cruciallib", None, Some("HEAD"))),
            "{ legacy-quark = true }"
        );
        let url = Some("https://github.com/user/foo.git");
        let sha = "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string();
        assert_eq!(
            toml(&dependency("foo", url, Some("tags/1.0"))),
            "{ legacy-quark = true, git = { tag = \"1.0\", url = \"https://github.com/user/foo.git\" } }"
        );
        assert_eq!(
            toml(&dependency("foo", url, Some(&sha))),
            format!("{{ legacy-quark = true, git = {{ ref = \"{sha}\", url = \"https://github.com/user/foo.git\" }} }}")
        );
        // like `Quark.validateRefspec`: anything else is a tag, also without `tags/`
        assert_eq!(
            toml(&dependency("foo", url, Some("1.0"))),
            "{ legacy-quark = true, git = { tag = \"1.0\", url = \"https://github.com/user/foo.git\" } }"
        );
        assert_eq!(
            toml(&dependency("foo", url, Some("HEAD"))),
            "{ legacy-quark = true, git = { url = \"https://github.com/user/foo.git\" } }"
        );
    }
}
