use anyhow::Context;
use pubgrub::SemanticVersion;
use regex::Regex;
use std::io::Read;
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
    str::FromStr,
};
use tracing::{error, instrument, trace};

include!(concat!(env!("OUT_DIR"), "/manifest_models.rs"));

impl TryFrom<MagnetManifestV1Version> for SemanticVersion {
    type Error = <SemanticVersion as FromStr>::Err;

    fn try_from(value: MagnetManifestV1Version) -> Result<Self, Self::Error> {
        SemanticVersion::from_str(&value)
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

    #[instrument(fields(path))]
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut content = String::new();
        File::open(path)
            .context(format!("Could not open magnet project file {:?}", path))?
            .read_to_string(&mut content)?;

        Self::from_string(&content)
    }

    fn from_string(content: &str) -> anyhow::Result<Self> {
        let value = serde_saphyr::from_str(content);
        match value {
            Ok(project) => Ok(project),
            Err(err) => {
                error!(?err, "Could not parse project file");
                Err(err.into())
            }
        }
    }

    #[instrument(skip(self))]
    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        let file = File::create(path).context("Could not create project file")?;
        let mut writer = BufWriter::new(file);
        serde_saphyr::to_io_writer(&mut writer, self).context("Could not write project file")?;
        writer.flush()?;
        trace!("Init project file");
        Ok(())
    }

    /// creates a new file
    pub fn write_new(&self, path: &Path) -> anyhow::Result<()> {
        let file = File::create(path).context("Could not create project file")?;
        let mut writer = BufWriter::new(file);
        writer.write(self.new_template().as_bytes())?;
        writer.flush()?;
        trace!("Init project file");
        Ok(())
    }

    /// This scaffolds a new magnet template, which allows to force a certain formatting
    fn new_template(&self) -> String {
        let content = format!(
            "magnet: 1

name: {name}
version: \"{version}\"

dependencies: []

artifacts: []

metadata:

",
            name = self.name.to_string(),
            version = self.version.to_string(),
        );
        content
    }

    /// insert a new entry to a top level section (e.g. dependencies)
    /// this is done via string manipulation to not delete any comments and keep
    /// changes on the file to a minimum.
    ///
    /// `new_entry` should end with a new line.
    /// If `section_name` could not be found it will return `None`.
    fn insert_before_next_section(
        contents: &str,
        section_name: &str,
        new_entry: &str,
    ) -> Option<String> {
        let lines: Vec<&str> = contents.lines().collect();
        let section_header = format!("{}:", section_name);

        // search for start of section
        let Some(section_start) = lines.iter().position(|l| l.starts_with(&section_header)) else {
            return None;
        };

        let mut last_content_line = section_start;
        for i in (section_start + 1)..lines.len() {
            let line = lines[i];
            if line.trim().is_empty() {
                // skip blank lines as they do not end a section
                continue;
            }
            if !line.starts_with(' ') {
                // non-indented && non-blank = next top level key
                break;
            }
            last_content_line += 1;
        }

        // rebuild by appending the new_entry
        let mut result = String::with_capacity(contents.len() + new_entry.len());
        for (i, line) in lines.iter().enumerate() {
            result.push_str(line);
            result.push('\n');
            if i == last_content_line {
                result.push_str(new_entry)
            }
        }
        Some(result)
    }

    /// adds a dependency to the manifest YAML. Returns the new string if successful.
    /// This is done via string patching b/c we would otherwise delete comments
    /// when we parse and write the YAML file back.
    pub fn add_dependency(
        yaml_content: &str,
        name: &str,
        requirement: &str,
    ) -> anyhow::Result<String> {
        // validate current manifest
        let manifest: MagnetManifestV1 = serde_saphyr::from_str(&yaml_content)?;

        let new_entry = format!("  - name: {name}\n    requirement: \"{requirement}\"\n");

        let output = if manifest.dependencies.is_empty() {
            yaml_content.replace("dependencies: []", &format!("dependencies:\n{new_entry}"))
        } else {
            if let Some(text) =
                Self::insert_before_next_section(&yaml_content, "dependencies", &new_entry)
            {
                text
            } else {
                anyhow::bail!("Could not find section \"dependencies\" in magnet.yaml")
            }
        };

        // re-validate after insertion
        let _: MagnetManifestV1 = serde_saphyr::from_str(&output)?;

        Ok(output)
    }

    /// Inserts a new dependency to the manifest file.
    pub fn add_dependency_to_file(
        path: &Path,
        name: &str,
        requirement: &str,
    ) -> anyhow::Result<()> {
        let mut content = std::fs::read_to_string(path)?;
        let modified = Self::add_dependency(&content, name, requirement)?;
        std::fs::write(path, modified)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> MagnetManifestV1 {
        MagnetManifestV1::new(
            "test-project".to_string(),
            Some("A test project".to_string()),
            Some("GPL-3.0".to_string()),
            SemanticVersion::new(0, 1, 0),
            None,
            MagnetManifestV1ArtifactsItemKind::Quark,
        )
        .unwrap()
    }

    #[test]
    fn test_new_template_valid_yaml() {
        let manifest = sample_manifest();
        let yaml_string = manifest.new_template();
        let parsed: MagnetManifestV1 = serde_saphyr::from_str(yaml_string.as_str()).unwrap();
        assert_eq!(parsed.name, "test-project");
        assert_eq!(parsed.version.to_string(), "0.1.0");
        assert_eq!(parsed.dependencies.len(), 0);
        assert_eq!(parsed.artifacts.len(), 0);
    }

    #[test]
    fn test_insert_before_next_section_no_section() {
        let text = MagnetManifestV1::insert_before_next_section(
            "name: foo\n",
            "hello",
            "nothing_new: foo",
        );
        assert!(text.is_none());
    }

    #[test]
    fn test_insert_before_next_section_proper() {
        let text =
            MagnetManifestV1::insert_before_next_section("name:\n  -foo\n", "name", "  -bar\n");
        assert!(text.is_some());
        let new_text = text.unwrap();
        assert_eq!(new_text, "name:\n  -foo\n  -bar\n");
    }

    #[test]
    fn add_dependency_empty_dependencies() {
        let manifest = sample_manifest();
        let empty_dependencies_yaml = manifest.new_template();
        let new_yaml =
            MagnetManifestV1::add_dependency(&empty_dependencies_yaml, "foo", "1.2.0").unwrap();
        assert!(new_yaml.contains("dependencies:\n  - name: foo\n    requirement: \"1.2.0\"\n"));
        let new_manifest = MagnetManifestV1::from_string(&new_yaml).unwrap();

        assert_eq!(new_manifest.dependencies.len(), 1);
        assert_eq!(new_manifest.dependencies[0].name, "foo");
        // @todo version is currently not parsed
        // let version = new_manifest.dependencies[0].version.clone().unwrap();
        // assert_eq!(version, "1.2.0");
    }

    #[test]
    fn add_dependency_existing_dependencies() {
        let manifest = sample_manifest();
        let empty_dependencies_yaml = manifest.new_template();
        // this is covered by add_dependency_empty_dependencies
        let new_yaml =
            MagnetManifestV1::add_dependency(&empty_dependencies_yaml, "foo", "1.2.0").unwrap();
        let new_manifest = MagnetManifestV1::from_string(&new_yaml).unwrap();
        let newest_yaml = MagnetManifestV1::add_dependency(&new_yaml, "bar", "2.2.0").unwrap();
        assert!(newest_yaml.contains("dependencies:\n  - name: foo\n    requirement: \"1.2.0\"\n  - name: bar\n    requirement: \"2.2.0\"\n"));

        let newest_manifest = MagnetManifestV1::from_string(&newest_yaml).unwrap();
        assert_eq!(newest_manifest.dependencies.len(), 2);
        assert_eq!(newest_manifest.dependencies[0].name, "foo");
        assert_eq!(newest_manifest.dependencies[1].name, "bar");
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
