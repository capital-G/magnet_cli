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
    path::Path,
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
                    if let Some(v) = item.as_value() {
                        inline.insert(key, v.clone());
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

    pub fn new(path: PathBuf, name: &String, version: &SemanticVersion) -> anyhow::Result<Self> {
        let file = File::create(&path).context("Could not create project file")?;
        let mut writer = BufWriter::new(file);
        writer.write(MagnetManifestV1::new_template(&name, &version).as_bytes())?;
        writer.flush()?;
        trace!("Init project file");
        Self::open(path)
    }

    pub fn write_new(path: PathBuf, manifest: &MagnetManifestV1) -> anyhow::Result<Self> {
        let content = toml::to_string(manifest)?;
        let file = File::create(&path).context("Could not create project file")?;
        let mut writer = BufWriter::new(file);
        writer.write(content.as_bytes())?;
        writer.flush()?;
        trace!("Init project file");
        Self::open(path)
    }

    pub fn add_dependency(&self, dependency: NamedDependency) -> anyhow::Result<()> {
        let updated = Self::add_dependency_preserving(
            &std::fs::read_to_string(&self.path).context("Could not read file")?,
            dependency,
        )?;
        self.write(&updated)?;
        Ok(())
    }

    pub fn get_manifest(&self) -> &MagnetManifestV1 {
        &self.manifest
    }

    // #[instrument(skip(self))]
    // pub fn write(&self) -> anyhow::Result<()> {
    //     std::fs::write(
    //         &self.path,
    //         toml::to_string_pretty(&self.manifest).context("Could not serialize project file")?
    //     ).context("Could not write project file")?;
    //     trace!("Init project file");
    //     Ok(())
    // }

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
        min_sclang_version: Option<SemanticVersion>,
        project_kind: MagnetManifestV1ArtifactsItemKind,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            Self::is_valid_name(&name),
            format!("{} is not a valid project name!", name)
        );

        let min_sclang_version = match min_sclang_version {
            Some(version) => Some(MagnetManifestV1MinSclangVersion::from_str(
                &version.to_string(),
            )?),
            None => None,
        };
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
            artifacts: vec![MagnetManifestV1ArtifactsItem {
                global_paths: vec![],
                kind: project_kind,
                platform_paths: None,
            }],
            dependencies: HashMap::new(),
            entrypoints: vec![],
            magnet_version: 1,
            metadata: Some(metadata),
            min_sclang_version,
            package: MagnetManifestV1Package { name, version },
        })
    }

    pub fn is_valid_name(name: &str) -> bool {
        let re = Regex::new(r"^[A-Za-z][A-Za-z0-9_-]*$").unwrap();
        re.is_match(name)
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

    /// This scaffolds a new magnet template, which allows to force a certain formatting
    fn new_template(name: &String, version: &SemanticVersion) -> String {
        let content = format!(
            "magnet = 1

name = \"{name}\"
version = \"{version}\"

[metadata]

",
            name = name,
            version = version.to_string(),
        );
        content
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
",
            sample_manifest()
        )
    }

    #[test]
    fn test_new_template_valid_toml() {
        let parsed: MagnetManifestV1 = toml::from_str(&sample_manifest()).unwrap();
        assert_eq!(parsed.package.name, "test-project");
        assert_eq!(parsed.package.version.to_string(), "0.1.0");
        assert_eq!(parsed.dependencies.len(), 0);
        assert_eq!(parsed.artifacts.len(), 0);
    }

    #[test]
    fn test_dependency() {
        let manifest = MagnetManifestV1::from_string(&sample_manifest_with_dependency()).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies.keys().last().unwrap(), "foo");
        match manifest.dependencies.get("foo").unwrap() {
            MagnetManifestV1DependenciesValue::String(semver) => {
                assert_eq!(semver.to_string(), "1.2.0");
            }
            MagnetManifestV1DependenciesValue::Object { .. } => {
                todo!()
            }
        }

        // @todo version is currently not parsed
    }

    #[test]
    fn add_dependency_existing_dependencies() {
        let raw_manifest = sample_manifest_with_dependency();
        let modified_raw_manifest = ProjectManifest::add_dependency_preserving(
            &raw_manifest,
            NamedDependency {
                name: "bar".to_string(),
                dependency: MagnetManifestV1DependenciesValue::Object {
                    branch: None,
                    version: Some("2.0.0".to_string()),
                    git: None,
                    index: None,
                    legacy_quark: Some(true),
                    path: None,
                    ref_: None,
                    tag: None,
                },
            },
        )
        .unwrap();

        let modified_manifest = MagnetManifestV1::from_string(&modified_raw_manifest).unwrap();

        assert_eq!(modified_manifest.dependencies.len(), 2);
        assert!(modified_manifest.dependencies.contains_key("foo"));
        assert!(modified_manifest.dependencies.contains_key("bar"));
        // @todo version is currently not parsed
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
    fn test_dependency_string_to_toml_edit_item() {
        let item =
            MagnetManifestV1DependenciesValue::String("0.2.0".to_string()).to_toml_edit_item();
        assert_eq!(item.as_str(), Some("0.2.0"));
    }

    #[test]
    fn test_dependency_object_to_toml_edit_item() {
        let dep = MagnetManifestV1DependenciesValue::Object {
            git: Some("http://my.git".to_string()),
            branch: Some("main".to_string()),
            index: None,
            legacy_quark: None,
            path: None,
            ref_: None,
            tag: None,
            version: None,
        };
        let item = dep.to_toml_edit_item();
        let table = item.as_inline_table().unwrap();
        assert_eq!(
            table.get("git").and_then(|v| v.as_str()),
            Some("http://my.git")
        );
        assert_eq!(table.get("branch").and_then(|v| v.as_str()), Some("main"));
        assert!(table.get("version").is_none());
    }
}
