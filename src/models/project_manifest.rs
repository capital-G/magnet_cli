use anyhow::Context;
use pubgrub::SemanticVersion;
use regex::Regex;
use std::collections::HashMap;
use std::default::Default;
use std::io::Read;
use std::path::PathBuf;
use std::{
    fs::File,
    io::{BufWriter, Write},
    str::FromStr,
};
use toml_edit::DocumentMut;
use tracing::{error, instrument, trace, warn};

use crate::models::package::ArtifactKind;
use crate::target::current_platform;

include!(concat!(env!("OUT_DIR"), "/manifest_models.rs"));

pub struct NamedDependency {
    pub name: String,
    pub dependency: MagnetManifestV1DependenciesValue,
}

impl MagnetManifestV1DependenciesValue {
    pub fn to_toml_edit_item(&self) -> toml_edit::Item {
        match self {
            MagnetManifestV1DependenciesValue::String(version) => {
                toml_edit::value(version.as_str())
            }
            _ => {
                let toml_str = toml::to_string(self).unwrap();
                let doc: DocumentMut = toml_str.parse().unwrap();
                let mut inline = toml_edit::InlineTable::new();
                for (key, item) in doc.as_table().iter() {
                    // nested tables (e.g. `git`) become nested inline tables
                    if let Ok(v) = item.clone().into_value() {
                        inline.insert(key, v);
                    }
                }
                // the values keep the spacing of the parsed document otherwise
                inline.fmt();
                toml_edit::Item::Value(toml_edit::Value::InlineTable(inline))
            }
        }
    }
}

impl TryFrom<MagnetManifestV1PackageVersion> for SemanticVersion {
    type Error = <SemanticVersion as FromStr>::Err;

    fn try_from(value: MagnetManifestV1PackageVersion) -> Result<Self, Self::Error> {
        SemanticVersion::from_str(&value)
    }
}

/// A helper function which uses serde serialization for toml_edit
fn serialize_toml_via_serde<T: serde::Serialize>(value: &T) -> anyhow::Result<toml_edit::Table> {
    let fragment = toml::to_string(value)?;
    let doc: DocumentMut = fragment.parse()?;
    Ok(doc.into_table())
}

/// The `[metadata]` table of a new project, in the order an author reads it, with the
/// authors as an inline array (serde would write them as `[[metadata.authors]]` tables)
fn serialize_metadata(metadata: &MagnetManifestV1Metadata) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    let strings = |values: &[String]| values.iter().collect::<toml_edit::Array>();
    for (key, value) in [
        ("summary", &metadata.summary),
        ("description", &metadata.description),
    ] {
        if let Some(value) = value {
            table[key] = toml_edit::value(value.as_str());
        }
    }
    if !metadata.authors.is_empty() {
        let authors: toml_edit::Array = metadata
            .authors
            .iter()
            .map(|author| {
                let mut inline = toml_edit::InlineTable::new();
                inline.insert("name", author.name.as_str().into());
                if let Some(email) = &author.email {
                    inline.insert("email", email.as_str().into());
                }
                inline
            })
            .collect();
        table["authors"] = toml_edit::value(authors);
    }
    for (key, value) in [
        ("license", &metadata.license),
        ("repository", &metadata.repository),
        ("readme", &metadata.readme),
    ] {
        if let Some(value) = value {
            table[key] = toml_edit::value(value.as_str());
        }
    }
    if !metadata.tags.is_empty() {
        table["tags"] = toml_edit::value(strings(&metadata.tags));
    }
    table
}

/// Where the files of a new project are, which decides its `[artifact]` template
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProjectLayout {
    /// the folders `magnet init` creates (`classes/`, `HelpSource/`)
    Magnet,
    /// an imported legacy quark, whose files can be anywhere (always a quark)
    LegacyQuark,
}

/// The `[artifact]` tables of a new project, as a commented starting point for the author,
/// see DESIGN.md "Packaging". The quark matches the folders of a new project (`classes/`,
/// `HelpSource/`), or selects everything for a legacy quark, like the legacy Quark system
/// installs it. A new project also gets a commented `[artifact.lepton]` with the binaries of
/// `platform` (if known), where `build/` is a guess for the build folder, so it only needs to
/// be uncommented once the project builds server plugins.
fn artifact_template(platform: Option<Platform>, layout: ProjectLayout) -> String {
    let mut template = String::from("\n[artifact.quark]\n");
    template.push_str(match layout {
        ProjectLayout::Magnet => {
            "# Only the files selected here get packaged into the quark (the sclang classes),\n\
             # so add every other file it needs, e.g. \"sounds/*.wav\" or \"examples/*.scd\"\n\
             paths = [\n    \"classes/**/*.sc\",\n    \"HelpSource/**\",\n]\n"
        }
        ProjectLayout::LegacyQuark => {
            "# Every file gets packaged, like the legacy Quark system installs it (hidden files,\n\
             # dist/ and magnet_packages/ never are). Narrow it down, e.g. to \"Classes/**\",\n\
             # \"HelpSource/**\" and \"*.quark\", or skip files via `exclude` in [artifact]\n\
             paths = [\"**\"]\n"
        }
    });
    if layout == ProjectLayout::LegacyQuark {
        return template;
    }
    // the plugins and the shared libraries they need, which get loaded next to them
    let (platform, libraries) = match platform {
        Some(platform @ (Platform::MacosX8664 | Platform::MacosArm64)) => (platform, "dylib"),
        Some(platform @ (Platform::WindowsX8664 | Platform::WindowsArm64)) => (platform, "dll"),
        Some(platform) => (platform, "so"),
        None => (Platform::LinuxX8664, "so"),
    };
    template.push_str(&format!(
        "\n# Uncomment this once the project builds server plugins (a lepton): the binaries per\n\
         # platform tag, i.e. the plugins and the shared libraries they need, which get packaged\n\
         # into plugins/<platform>/<abi>/. Their sources (e.g. src/ and CMakeLists.txt) go into\n\
         # the source artifact, which takes the files tracked by git.\n\
         # [artifact.lepton.platform-paths]\n\
         # {platform} = [\"build/*.{{scx,{libraries}}}\"]\n"
    ));
    template
}

pub struct ProjectManifest {
    path: PathBuf,
    manifest: MagnetManifestV1,
}

impl ProjectManifest {
    #[instrument(fields(path))]
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let mut content = String::new();
        File::open(&path)
            .context(format!("Could not open magnet project file {:?}", &path))?
            .read_to_string(&mut content)?;

        Ok(Self {
            path,
            manifest: MagnetManifestV1::from_string(&content)?,
        })
    }

    pub fn write_new(
        path: PathBuf,
        manifest: &MagnetManifestV1,
        layout: ProjectLayout,
    ) -> anyhow::Result<Self> {
        let mut content = Self::serialize_manifest(manifest)?;
        if manifest.artifact.is_none() {
            let platform = match current_platform() {
                Ok(platform) => Some(platform),
                Err(err) => {
                    warn!(
                        "{err:#}, so the commented [artifact.lepton] names linux_x86_64 as example"
                    );
                    None
                }
            };
            content.push_str(&artifact_template(platform, layout));
        }
        let file = File::create(&path).context("Could not create project file")?;
        let mut writer = BufWriter::new(file);
        writer.write(content.as_bytes())?;
        writer.flush()?;
        trace!("Init project file");
        Self::open(path)
    }

    /// A helper function to create a new magnet file w/ a specific layout to start with.
    /// Do not use this to edit an existing manifest file as this will overwrite existing comments
    /// and layout.
    fn serialize_manifest(manifest: &MagnetManifestV1) -> anyhow::Result<String> {
        let mut doc = DocumentMut::new();

        doc["magnet-version"] = toml_edit::value(manifest.magnet_version);

        let mut package = toml_edit::Table::new();
        package["name"] = toml_edit::value(manifest.package.name.as_str());
        package["version"] = toml_edit::value(manifest.package.version.to_string());
        if let Some(sclang) = &manifest.package.sclang {
            package["sclang"] = toml_edit::value(sclang.as_str());
        }
        package.decor_mut().set_prefix("\n");
        doc["package"] = toml_edit::Item::Table(package);

        let mut metadata = match &manifest.metadata {
            Some(meta) => serialize_metadata(meta),
            None => toml_edit::Table::new(),
        };
        metadata.decor_mut().set_prefix("\n");
        doc["metadata"] = toml_edit::Item::Table(metadata);

        let mut deps = toml_edit::Table::new();
        // sorted, since the order of the map is random
        let mut dependencies: Vec<_> = manifest.dependencies.iter().collect();
        dependencies.sort_by_key(|(name, _)| name.to_lowercase());
        for (name, value) in dependencies {
            deps.insert(name, value.to_toml_edit_item());
        }
        deps.decor_mut().set_prefix("\n");
        doc["dependencies"] = toml_edit::Item::Table(deps);

        if let Some(artifact) = &manifest.artifact {
            let mut table = serialize_toml_via_serde(artifact)?;
            // only a header for `exclude`, the kinds have their own tables
            table.set_implicit(true);
            table.decor_mut().set_prefix("\n");
            for (_, item) in table.iter_mut() {
                if let Some(kind) = item.as_table_mut() {
                    kind.decor_mut().set_prefix("\n");
                }
            }
            doc["artifact"] = toml_edit::Item::Table(table);
        }

        Ok(doc.to_string())
    }

    pub fn add_dependency(&self, dependency: NamedDependency) -> anyhow::Result<()> {
        let updated = Self::add_dependency_preserving(
            &std::fs::read_to_string(&self.path).context("Could not read file")?,
            dependency,
        )?;
        self.write(&updated)?;
        Ok(())
    }

    /// Removes the dependency with exactly this key, preserving the rest of the file
    pub fn remove_dependency(&self, name: &str) -> anyhow::Result<()> {
        let updated = Self::remove_dependency_preserving(
            &std::fs::read_to_string(&self.path).context("Could not read file")?,
            name,
        )?;
        self.write(&updated)?;
        Ok(())
    }

    pub fn get_manifest(&self) -> &MagnetManifestV1 {
        &self.manifest
    }

    /// transforms the manifest in a non-destructive way, i.e.
    /// comments, positioning and formatting are preserved.
    fn add_dependency_preserving(
        contents: &str,
        requirement: NamedDependency,
    ) -> anyhow::Result<String> {
        let mut doc: DocumentMut = contents.parse()?;

        let dependencies = doc
            .entry("dependencies")
            .or_insert_with(|| toml_edit::Item::Table(Default::default()))
            .as_table_like_mut()
            .context("dependencies is not an array of tables")?;

        dependencies.insert(
            &requirement.name,
            requirement.dependency.to_toml_edit_item(),
        );

        Ok(doc.to_string())
    }

    /// the counterpart of [Self::add_dependency_preserving]
    fn remove_dependency_preserving(contents: &str, name: &str) -> anyhow::Result<String> {
        let mut doc: DocumentMut = contents.parse()?;
        doc.get_mut("dependencies")
            .and_then(|dependencies| dependencies.as_table_like_mut())
            .and_then(|dependencies| dependencies.remove(name))
            .with_context(|| format!("{name} is not a dependency in the manifest"))?;
        Ok(doc.to_string())
    }

    fn write(&self, contents: &String) -> anyhow::Result<()> {
        std::fs::write(&self.path, contents).context("Could not write project file")?;
        Ok(())
    }
}

impl MagnetManifestV1 {
    pub fn new(
        name: String,
        version: SemanticVersion,
        sclang: Option<String>,
        metadata: MagnetManifestV1Metadata,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            Self::is_valid_name(&name),
            format!("{} is not a valid project name!", name)
        );

        let version = MagnetManifestV1PackageVersion::from_str(&version.to_string())?;

        Ok(MagnetManifestV1 {
            artifact: None,
            dependencies: HashMap::new(),
            entrypoint: vec![],
            magnet_version: 1,
            metadata: Some(metadata),
            overrides: HashMap::new(),
            package: MagnetManifestV1Package {
                name,
                sclang,
                version,
            },
        })
    }

    /// The kinds of artifacts the version contains, i.e. whose `[artifact]` tables it has.
    /// The source isn't one of them, every version has one.
    pub fn artifact_kinds(&self) -> Vec<ArtifactKind> {
        let artifact = self.artifact.clone().unwrap_or_default();
        let mut kinds = vec![];
        if artifact.quark.is_some() {
            kinds.push(ArtifactKind::Quark);
        }
        if artifact.lepton.is_some() {
            kinds.push(ArtifactKind::Lepton);
        }
        kinds
    }

    /// The globs of `[artifact.quark]`, empty if the manifest has none
    pub fn quark_paths(&self) -> &[String] {
        self.artifact
            .as_ref()
            .and_then(|artifact| artifact.quark.as_ref())
            .map(|quark| quark.paths.as_slice())
            .unwrap_or_default()
    }

    /// The `platform-paths` of `[artifact.lepton]`, if the manifest has it
    pub fn lepton_platform_paths(&self) -> Option<&HashMap<Platform, Vec<String>>> {
        self.artifact
            .as_ref()
            .and_then(|artifact| artifact.lepton.as_ref())
            .map(|lepton| &lepton.platform_paths)
    }

    /// `[artifact.source]`, whose defaults take the files tracked by git
    pub fn source(&self) -> MagnetManifestV1ArtifactSource {
        self.artifact
            .as_ref()
            .and_then(|artifact| artifact.source.clone())
            .unwrap_or_default()
    }

    /// The globs of `exclude` in `[artifact]`
    pub fn exclude(&self) -> &[String] {
        self.artifact
            .as_ref()
            .map(|artifact| artifact.exclude.as_slice())
            .unwrap_or_default()
    }

    /// Letters, digits, `_` and `-`, starting with a letter or digit (e.g. the legacy quark
    /// `3Dj`). No dots, which would read like file extensions in `dl` file names and folders.
    pub fn is_valid_name(name: &str) -> bool {
        let re = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]*$").unwrap();
        re.is_match(name)
    }

    /// The canonical form of a package name, used for index lookups and lock entries,
    /// i.e. `DDW_Common` and `ddw-common` refer to the same package.
    /// This is in place in order to avoid typo squatting.
    pub fn normalize_name(name: &str) -> String {
        name.to_lowercase().replace('_', "-")
    }

    pub(crate) fn from_string(content: &str) -> anyhow::Result<Self> {
        let value = toml::from_str(content);
        match value {
            Ok(project) => Ok(project),
            Err(err) => {
                error!(?err, "Could not parse project file");
                match manifest_hint(content) {
                    Some(hint) => Err(anyhow::Error::from(err).context(hint)),
                    None => Err(err.into()),
                }
            }
        }
    }
}

/// The artifact kinds this version of magnet knows, see DESIGN.md "Package kinds"
const ARTIFACT_KINDS: [&str; 3] = ["source", "quark", "lepton"];

/// Explains why a manifest which doesn't parse was probably written for another version of
/// magnet: a newer one knows more artifact kinds, an older one declared a single kind.
fn manifest_hint(content: &str) -> Option<String> {
    let table: toml::Table = content.parse().ok()?;
    let mut hints = vec![];
    if table
        .get("package")
        .and_then(|package| package.get("kind"))
        .is_some()
    {
        hints.push(
            "[package] has no `kind` anymore, the [artifact.quark] and [artifact.lepton] tables \
             declare what a version contains"
                .to_string(),
        );
    }
    if let Some(artifact) = table
        .get("artifact")
        .and_then(|artifact| artifact.as_table())
    {
        let old: Vec<&str> = ["global-paths", "platform-paths", "source-paths"]
            .into_iter()
            .filter(|key| artifact.contains_key(*key))
            .collect();
        if !old.is_empty() {
            hints.push(format!(
                "{} in [artifact] moved: `global-paths` to `paths` of [artifact.quark], \
                 `platform-paths` to [artifact.lepton] and `source-paths` to `paths` of \
                 [artifact.source] (with `from = \"paths\"`)",
                old.join(", ")
            ));
        }
        let unknown: Vec<String> = artifact
            .iter()
            .filter(|(key, value)| value.is_table() && !ARTIFACT_KINDS.contains(&key.as_str()))
            .map(|(key, _)| format!("[artifact.{key}]"))
            .collect();
        if !unknown.is_empty() {
            hints.push(format!(
                "This version of magnet doesn't know {}, maybe the project needs a newer magnet",
                unknown.join(", ")
            ));
        }
    }
    match hints.is_empty() {
        true => None,
        false => Some(hints.join("\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> String {
        "magnet-version = 1

[package]
name = \"test-project\"
version = \"0.1.0\"

[metadata]

"
        .to_string()
    }

    fn sample_manifest_with_dependency() -> String {
        format!(
            "{}
[dependencies]
foo = \"1.2.0\"
baz = {}
",
            sample_manifest(),
            "{ version = \"1.2.0\", legacy-quark = true }",
        )
    }

    #[test]
    fn test_new_template_valid_toml() {
        let parsed: MagnetManifestV1 = toml::from_str(&sample_manifest()).unwrap();
        assert_eq!(parsed.package.name, "test-project");
        assert_eq!(parsed.package.version.to_string(), "0.1.0");
        assert_eq!(parsed.dependencies.len(), 0);
        assert!(parsed.artifact.is_none());
    }

    #[test]
    fn test_dependency() {
        let manifest = MagnetManifestV1::from_string(&sample_manifest_with_dependency()).unwrap();
        assert_eq!(manifest.dependencies.len(), 2);
        match manifest.dependencies.get("foo").unwrap() {
            MagnetManifestV1DependenciesValue::String(semver) => {
                assert_eq!(semver.to_string(), "1.2.0");
            }
            MagnetManifestV1DependenciesValue::Object { .. } => {
                todo!()
            }
        }

        match manifest.dependencies.get("baz").unwrap() {
            MagnetManifestV1DependenciesValue::String(_) => {
                todo!();
            }
            MagnetManifestV1DependenciesValue::Object {
                legacy_quark,
                version,
                ..
            } => {
                assert_eq!(version.as_ref().unwrap().to_string(), "1.2.0");
                assert_eq!(legacy_quark.unwrap(), true);
            }
        }
    }

    #[test]
    fn add_dependency_existing_dependencies() {
        let raw_manifest = sample_manifest_with_dependency();
        let modified_raw_manifest = ProjectManifest::add_dependency_preserving(
            &raw_manifest,
            NamedDependency {
                name: "bar".to_string(),
                dependency: MagnetManifestV1DependenciesValue::Object {
                    version: Some("2.0.0".to_string()),
                    git: None,
                    ignore_quark_deps: None,
                    index: None,
                    legacy_quark: Some(true),
                    path: None,
                },
            },
        )
        .unwrap();

        let modified_manifest = MagnetManifestV1::from_string(&modified_raw_manifest).unwrap();

        assert_eq!(modified_manifest.dependencies.len(), 3);
        assert!(modified_manifest.dependencies.contains_key("foo"));
        assert!(modified_manifest.dependencies.contains_key("bar"));
        assert!(modified_manifest.dependencies.contains_key("baz"));
        // @todo version is currently not parsed
    }

    #[test]
    fn remove_dependency_keeps_comments() {
        let raw_manifest = format!(
            "{}\n[dependencies]\n# about foo\nfoo = \"1.2.0\"\n# about bar\nbar = \"^2\"\nbaz = {{ version = \"1.0\" }}\n",
            sample_manifest()
        );
        let modified = ProjectManifest::remove_dependency_preserving(&raw_manifest, "bar").unwrap();
        assert!(
            modified.contains("# about foo\nfoo = \"1.2.0\"\nbaz = { version = \"1.0\" }\n"),
            "{modified}"
        );
        // the comment of the removed entry goes with it
        assert!(!modified.contains("about bar"), "{modified}");

        assert!(ProjectManifest::remove_dependency_preserving(&raw_manifest, "missing").is_err());
        assert!(ProjectManifest::remove_dependency_preserving(&sample_manifest(), "foo").is_err());
    }

    #[test]
    fn test_project_name() {
        for name in [
            "foobar",
            "hello42",
            "hello-foo",
            "hello_bar",
            "42hello",
            "3Dj",
        ] {
            assert!(MagnetManifestV1::is_valid_name(name))
        }

        for name in ["foo@bar", "_hello", "-hello", "Faust.quark", ""] {
            assert!(!MagnetManifestV1::is_valid_name(name))
        }
    }

    #[test]
    fn test_normalize_name() {
        let normalized = MagnetManifestV1::normalize_name("ddwCommon");
        assert_eq!(normalized, "ddwcommon");
        assert_eq!(normalized, MagnetManifestV1::normalize_name("ddwcommon"));

        let normalized = MagnetManifestV1::normalize_name("ddw_common");
        assert_eq!(normalized, "ddw-common");
        assert_eq!(normalized, MagnetManifestV1::normalize_name("DDW-common"));
        assert_eq!(normalized, MagnetManifestV1::normalize_name("DDW_Common"));
    }

    #[test]
    fn test_dependency_string_to_toml_edit_item() {
        let item =
            MagnetManifestV1DependenciesValue::String("0.2.0".to_string()).to_toml_edit_item();
        assert_eq!(item.as_str(), Some("0.2.0"));
    }

    #[test]
    fn test_dependency_object_to_toml_edit_item() {
        let dep = MagnetManifestV1DependenciesValue::Object {
            git: Some(MagnetManifestV1DependenciesValueObjectGit {
                url: Some("http://my.git".to_string()),
                branch: Some("main".to_string()),
                ref_: None,
                tag: None,
            }),
            ignore_quark_deps: None,
            index: None,
            legacy_quark: None,
            path: None,
            version: None,
        };
        let item = dep.to_toml_edit_item();
        let table = item.as_inline_table().unwrap();
        let git = table.get("git").and_then(|v| v.as_inline_table()).unwrap();
        assert_eq!(
            git.get("url").and_then(|v| v.as_str()),
            Some("http://my.git")
        );
        assert_eq!(git.get("branch").and_then(|v| v.as_str()), Some("main"));
        assert!(table.get("version").is_none());
    }

    #[test]
    fn test_serialize_manifest() {
        let manifest = MagnetManifestV1 {
            magnet_version: 1,
            overrides: HashMap::new(),
            package: MagnetManifestV1Package {
                name: "bar".to_string(),
                version: MagnetManifestV1PackageVersion::from_str("0.1.0").unwrap(),
                sclang: None,
            },
            metadata: Some(MagnetManifestV1Metadata {
                authors: vec![],
                description: None,
                license: None,
                readme: None,
                repository: None,
                summary: None,
                tags: vec![],
            }),
            dependencies: HashMap::new(),
            artifact: Some(MagnetManifestV1Artifact {
                quark: Some(MagnetManifestV1ArtifactQuark {
                    paths: vec!["classes/**/*.sc".to_string()],
                }),
                ..Default::default()
            }),
            entrypoint: vec![],
        };

        let content = ProjectManifest::serialize_manifest(&manifest).unwrap();

        assert_eq!(
            content,
            "magnet-version = 1\n\
             \n\
             [package]\n\
             name = \"bar\"\n\
             version = \"0.1.0\"\n\
             \n\
             [metadata]\n\
             \n\
             [dependencies]\n\
             \n\
             [artifact.quark]\n\
             paths = [\"classes/**/*.sc\"]\n"
        );
    }

    #[test]
    fn test_serialize_metadata() {
        let metadata = MagnetManifestV1Metadata {
            authors: vec![
                MagnetManifestV1MetadataAuthorsItem {
                    name: "Jane Doe".to_string(),
                    email: Some("jane@example.org".to_string()),
                },
                MagnetManifestV1MetadataAuthorsItem {
                    name: "John".to_string(),
                    email: None,
                },
            ],
            description: Some("Some longer text".to_string()),
            license: Some("GPL-3.0-or-later".to_string()),
            readme: None,
            repository: Some("https://github.com/jane/foo".to_string()),
            summary: Some("Does foo".to_string()),
            tags: vec!["synth".to_string(), "fx".to_string()],
        };
        let manifest = MagnetManifestV1::new(
            "foo".to_string(),
            SemanticVersion::new(0, 1, 0),
            None,
            metadata,
        )
        .unwrap();
        let content = ProjectManifest::serialize_manifest(&manifest).unwrap();
        assert!(
            content.contains(
                "[metadata]\n\
                 summary = \"Does foo\"\n\
                 description = \"Some longer text\"\n\
                 authors = [{ name = \"Jane Doe\", email = \"jane@example.org\" }, { name = \"John\" }]\n\
                 license = \"GPL-3.0-or-later\"\n\
                 repository = \"https://github.com/jane/foo\"\n\
                 tags = [\"synth\", \"fx\"]\n"
            ),
            "{content}"
        );

        let parsed = MagnetManifestV1::from_string(&content)
            .unwrap()
            .metadata
            .unwrap();
        assert_eq!(parsed.authors.len(), 2);
        assert_eq!(parsed.authors[0].email.as_deref(), Some("jane@example.org"));
        assert_eq!(parsed.tags, ["synth", "fx"]);
    }

    fn template_content(platform: Option<Platform>, layout: ProjectLayout) -> String {
        let manifest = MagnetManifestV1::new(
            "foo".to_string(),
            SemanticVersion::new(0, 1, 0),
            None,
            Default::default(),
        )
        .unwrap();
        ProjectManifest::serialize_manifest(&manifest).unwrap()
            + &artifact_template(platform, layout)
    }

    /// The template with its `[artifact.lepton]` uncommented, as the author would do it
    fn uncommented_lepton(content: &str) -> String {
        let mut lines: Vec<String> = content.lines().map(String::from).collect();
        let start = lines
            .iter()
            .position(|line| line == "# [artifact.lepton.platform-paths]")
            .expect("no commented [artifact.lepton] in the template");
        for line in &mut lines[start..start + 2] {
            *line = line.strip_prefix("# ").unwrap().to_string();
        }
        lines.join("\n") + "\n"
    }

    #[test]
    fn test_quark_template() {
        let manifest =
            MagnetManifestV1::from_string(&template_content(None, ProjectLayout::Magnet)).unwrap();
        assert_eq!(manifest.quark_paths(), ["classes/**/*.sc", "HelpSource/**"]);
        assert!(manifest.lepton_platform_paths().is_none());
        assert!(manifest.artifact.unwrap().source.is_none());
    }

    #[test]
    fn test_legacy_quark_template() {
        let content = template_content(Some(Platform::MacosArm64), ProjectLayout::LegacyQuark);
        assert!(!content.contains("lepton"), "{content}");
        let manifest = MagnetManifestV1::from_string(&content).unwrap();
        assert_eq!(manifest.quark_paths(), ["**"]);
        assert_eq!(manifest.artifact_kinds(), [ArtifactKind::Quark]);
    }

    #[test]
    fn test_serialize_sorts_dependencies() {
        let mut manifest = MagnetManifestV1::new(
            "foo".to_string(),
            SemanticVersion::new(0, 1, 0),
            None,
            Default::default(),
        )
        .unwrap();
        for name in ["redUniverse", "MathLib", "bjorklund"] {
            manifest.dependencies.insert(
                name.to_string(),
                MagnetManifestV1DependenciesValue::String("*".to_string()),
            );
        }
        let content = ProjectManifest::serialize_manifest(&manifest).unwrap();
        assert!(
            content.contains(
                "[dependencies]\nbjorklund = \"*\"\nMathLib = \"*\"\nredUniverse = \"*\"\n"
            ),
            "{content}"
        );
    }

    #[test]
    fn test_lepton_template() {
        for (platform, binaries) in [
            (Some(Platform::MacosArm64), "build/*.{scx,dylib}"),
            (Some(Platform::LinuxArmv7), "build/*.{scx,so}"),
            (Some(Platform::WindowsX8664), "build/*.{scx,dll}"),
            // an unknown platform gets an example, which the author adapts
            (None, "build/*.{scx,so}"),
        ] {
            let content = template_content(platform, ProjectLayout::Magnet);
            let manifest = MagnetManifestV1::from_string(&uncommented_lepton(&content)).unwrap();
            assert_eq!(
                manifest.artifact_kinds(),
                [ArtifactKind::Quark, ArtifactKind::Lepton]
            );
            assert_eq!(manifest.quark_paths(), ["classes/**/*.sc", "HelpSource/**"]);
            assert_eq!(
                manifest.lepton_platform_paths().unwrap(),
                &HashMap::from([(
                    platform.unwrap_or(Platform::LinuxX8664),
                    vec![binaries.to_string()]
                )])
            );
        }
    }

    #[test]
    fn test_artifact_kinds() {
        let manifest = |artifact: &str| {
            MagnetManifestV1::from_string(&format!(
                "magnet-version = 1\n[package]\nname = \"foo\"\nversion = \"1.0.0\"\n{artifact}"
            ))
            .unwrap()
        };
        assert!(manifest("").artifact_kinds().is_empty());
        assert_eq!(
            manifest("[artifact.quark]\npaths = [\"*.sc\"]\n").artifact_kinds(),
            [ArtifactKind::Quark]
        );
        // a lepton without classes, e.g. one which only ships binaries
        let lepton = manifest("[artifact.lepton.platform-paths]\nmacos_arm64 = [\"*.scx\"]\n");
        assert_eq!(lepton.artifact_kinds(), [ArtifactKind::Lepton]);
        assert!(lepton.quark_paths().is_empty());
        assert_eq!(
            lepton.source().from,
            MagnetManifestV1ArtifactSourceFrom::Git
        );
    }

    /// Manifests of other magnet versions explain what changed
    #[test]
    fn test_manifest_hints() {
        let error = |rest: &str| {
            let content = format!(
                "magnet-version = 1\n[package]\nname = \"foo\"\nversion = \"1.0.0\"\n{rest}"
            );
            format!("{:#}", MagnetManifestV1::from_string(&content).unwrap_err())
        };
        assert!(error("kind = \"quark\"\n").contains("[package] has no `kind` anymore"));
        // an old manifest gets all hints at once
        let old = error("kind = \"quark\"\n[artifact]\nglobal-paths = [\"*.sc\"]\n");
        assert!(
            old.contains("no `kind` anymore") && old.contains("global-paths in [artifact] moved"),
            "{old}"
        );
        let moved = error("[artifact]\nglobal-paths = [\"*.sc\"]\nsource-paths = [\"src/**\"]\n");
        assert!(
            moved.contains("global-paths, source-paths in [artifact] moved"),
            "{moved}"
        );
        let unknown = error("[artifact.gluon]\npaths = [\"*.scx\"]\n");
        assert!(
            unknown
                .contains("doesn't know [artifact.gluon], maybe the project needs a newer magnet"),
            "{unknown}"
        );
        // other errors stay as they are
        assert!(!error("[artifact.quark]\n").contains("magnet"));
    }

    /// The manifest example in DESIGN.md must always be valid against the schema.
    #[test]
    fn test_design_doc_example() {
        // fix: git may check out DESIGN.md with CRLF line endings on Windows
        let design = include_str!("../../DESIGN.md").replace("\r\n", "\n");
        let example = design
            .split("\n### Example\n")
            .nth(1)
            .and_then(|section| section.split("```toml\n").nth(1))
            .and_then(|block| block.split("\n```").next())
            .expect("Could not find manifest example in DESIGN.md");

        let manifest = MagnetManifestV1::from_string(example).unwrap();
        assert_eq!(manifest.package.name, "magnet-test");
    }
}
