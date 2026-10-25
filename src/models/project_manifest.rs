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
use tracing::{error, instrument, trace};

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

    pub fn write_new(path: PathBuf, manifest: &MagnetManifestV1) -> anyhow::Result<Self> {
        let content = Self::serialize_manifest(manifest)?;
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
        package["kind"] = toml_edit::value(manifest.package.kind.to_string());
        if let Some(sclang) = &manifest.package.sclang {
            package["sclang"] = toml_edit::value(sclang.as_str());
        }
        package.decor_mut().set_prefix("\n");
        doc["package"] = toml_edit::Item::Table(package);

        let mut metadata = match &manifest.metadata {
            Some(meta) => serialize_toml_via_serde(meta)?,
            None => toml_edit::Table::new(),
        };
        metadata.decor_mut().set_prefix("\n");
        doc["metadata"] = toml_edit::Item::Table(metadata);

        let mut deps = toml_edit::Table::new();
        for (name, value) in &manifest.dependencies {
            deps.insert(name, value.to_toml_edit_item());
        }
        deps.decor_mut().set_prefix("\n");
        doc["dependencies"] = toml_edit::Item::Table(deps);

        if let Some(artifact) = &manifest.artifact {
            let mut table = serialize_toml_via_serde(artifact)?;
            table.decor_mut().set_prefix("\n");
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
        description: Option<String>,
        license: Option<String>,
        version: SemanticVersion,
        sclang: Option<String>,
        project_kind: MagnetManifestV1PackageKind,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            Self::is_valid_name(&name),
            format!("{} is not a valid project name!", name)
        );

        let version = MagnetManifestV1PackageVersion::from_str(&version.to_string())?;

        let metadata = MagnetManifestV1Metadata {
            authors: vec![],
            description,
            license,
            readme: None,
            repository: None,
            tags: vec![],
        };

        Ok(MagnetManifestV1 {
            artifact: None,
            dependencies: HashMap::new(),
            entrypoint: vec![],
            magnet_version: 1,
            metadata: Some(metadata),
            package: MagnetManifestV1Package {
                kind: project_kind,
                name,
                sclang,
                version,
            },
        })
    }

    pub fn is_valid_name(name: &str) -> bool {
        let re = Regex::new(r"^[A-Za-z][A-Za-z0-9_-]*$").unwrap();
        re.is_match(name)
    }

    /// The canonical form of a package name, used for index lookups and lock entries,
    /// i.e. `DDW_Common` and `ddw-common` refer to the same package.
    /// This is in place in order to avoid typo squatting.
    pub fn normalize_name(name: &str) -> String {
        name.to_lowercase().replace('_', "-")
    }

    fn from_string(content: &str) -> anyhow::Result<Self> {
        let value = toml::from_str(content);
        match value {
            Ok(project) => Ok(project),
            Err(err) => {
                error!(?err, "Could not parse project file");
                Err(err.into())
            }
        }
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
kind = \"quark\"

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
        for name in ["foobar", "hello42", "hello-foo", "hello_bar"] {
            assert!(MagnetManifestV1::is_valid_name(name))
        }

        for name in ["foo@bar", "42hello", "_hello"] {
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
                url: "http://my.git".to_string(),
                branch: Some("main".to_string()),
                ref_: None,
                tag: None,
            }),
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
            package: MagnetManifestV1Package {
                name: "bar".to_string(),
                version: MagnetManifestV1PackageVersion::from_str("0.1.0").unwrap(),
                kind: MagnetManifestV1PackageKind::Quark,
                sclang: None,
            },
            metadata: Some(MagnetManifestV1Metadata {
                authors: vec![],
                description: None,
                license: None,
                readme: None,
                repository: None,
                tags: vec![],
            }),
            dependencies: HashMap::new(),
            artifact: Some(MagnetManifestV1Artifact {
                global_paths: vec!["classes/**/*.sc".to_string()],
                platform_paths: None,
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
             kind = \"quark\"\n\
             \n\
             [metadata]\n\
             \n\
             [dependencies]\n\
             \n\
             [artifact]\n\
             global-paths = [\"classes/**/*.sc\"]\n"
        );
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
