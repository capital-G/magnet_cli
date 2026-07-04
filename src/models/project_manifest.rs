use anyhow::Context;
use pubgrub::SemanticVersion;
use regex::Regex;
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

impl TryFrom<MagnetManifestV1Version> for SemanticVersion {
    type Error = <SemanticVersion as FromStr>::Err;

    fn try_from(value: MagnetManifestV1Version) -> Result<Self, Self::Error> {
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

    pub fn add_dependency(
        &self,
        dependency: MagnetManifestV1DependenciesItem,
    ) -> anyhow::Result<()> {
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
        requirement: MagnetManifestV1DependenciesItem,
    ) -> anyhow::Result<String> {
        let mut doc: DocumentMut = contents.parse()?;

        let dependencies = doc
            .entry("dependencies")
            .or_insert_with(|| toml_edit::Item::ArrayOfTables(Default::default()))
            .as_array_of_tables_mut()
            .context("dependencies is not an array of tables")?;

        let existing_index = dependencies
            .iter_mut()
            .position(|d| d.get("name").and_then(|x| x.as_str()) == Some(&requirement.name));

        if let Some(index) = existing_index {
            let version_string = toml_edit::value(requirement.version.unwrap().to_string());
            let dependency = dependencies.iter_mut().nth(index).unwrap();
            dependency.insert("requirement", version_string);
        } else {
            dependencies.push(serialize_toml_via_serde(&requirement)?);
        }

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
        let version = MagnetManifestV1Version::from_str(&version.to_string())?;

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
            dependencies: vec![],
            entrypoints: vec![],
            magnet: 1.0,
            metadata: Some(metadata),
            min_sclang_version,
            name,
            version,
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
        "magnet = 1

name = \"test-project\"
version = \"0.1.0\"

[metadata]

[[dependency]]

"
        .to_string()
    }

    fn sample_manifest_with_dependency() -> String {
        format!(
            "{}
[[dependencies]]
name = \"foo\"
requirement = \"1.2.0\"
",
            sample_manifest()
        )
    }

    #[test]
    fn test_new_template_valid_toml() {
        let parsed: MagnetManifestV1 = toml::from_str(&sample_manifest()).unwrap();
        assert_eq!(parsed.name, "test-project");
        assert_eq!(parsed.version.to_string(), "0.1.0");
        assert_eq!(parsed.dependencies.len(), 0);
        assert_eq!(parsed.artifacts.len(), 0);
    }

    #[test]
    fn test_dependency() {
        let manifest = MagnetManifestV1::from_string(&sample_manifest_with_dependency()).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].name, "foo");
        // @todo version is currently not parsed
        // let version = new_manifest.dependencies[0].version.clone().unwrap();
        // assert_eq!(version, "1.2.0");
    }

    #[test]
    fn add_dependency_existing_dependencies() {
        let raw_manifest = sample_manifest_with_dependency();
        let modified_raw_manifest = ProjectManifest::add_dependency_preserving(
            &raw_manifest,
            MagnetManifestV1DependenciesItem {
                name: "bar".to_string(),
                version: Some("2.0.0".to_string()),
                git: None,
                index: None,
                legacy_quark: None,
                path: None,
            },
        )
        .unwrap();

        let modified_manifest = MagnetManifestV1::from_string(&modified_raw_manifest).unwrap();

        assert_eq!(modified_manifest.dependencies.len(), 2);
        assert_eq!(modified_manifest.dependencies[0].name, "foo");
        assert_eq!(modified_manifest.dependencies[1].name, "bar");
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
}
