use std::{cell::OnceCell, collections::BTreeMap};

use anyhow::Context;
use tracing::warn;
use url::Url;

use crate::git_client::source::{GitCache, GitReference};
use crate::models::project_manifest::MagnetManifestV1;

/// The repository of the legacy quarks directory, the default of `Quarks.directoryUrl`
pub const DEFAULT_QUARKS_DIRECTORY_URL: &str = "https://github.com/supercollider-quarks/quarks.git";
/// Replaces the repository of the legacy quarks directory, e.g. for tests or a mirror
pub const MAGNET_QUARKS_DIRECTORY_ENV: &str = "MAGNET_QUARKS_DIRECTORY_URL";
/// The file within the directory repository which lists the legacy quarks
pub const DIRECTORY_FILE: &str = "directory.txt";

/// `$MAGNET_QUARKS_DIRECTORY_URL`, otherwise the official quarks directory
pub fn quarks_directory_url() -> anyhow::Result<Url> {
    let url = std::env::var(MAGNET_QUARKS_DIRECTORY_ENV)
        .unwrap_or_else(|_| DEFAULT_QUARKS_DIRECTORY_URL.to_string());
    url.parse()
        .with_context(|| format!("{url} ({MAGNET_QUARKS_DIRECTORY_ENV}) is not a valid URL"))
}

/// The repository of a legacy quark, and what to check out of it
#[derive(Debug, Clone, PartialEq)]
pub struct DirectoryEntry {
    pub url: Url,
    pub reference: GitReference,
}

/// Parses the lines `name=url` or `name=url@refspec` of `directory.txt`, keyed by the
/// normalized name. Lines which don't fit are skipped with a warning.
pub fn parse_directory(content: &str) -> BTreeMap<String, DirectoryEntry> {
    let mut entries = BTreeMap::new();
    for line in content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Some((name, entry)) = line.split_once('=') else {
            warn!("Skipping the line {line:?} of {DIRECTORY_FILE}, it has no `=`");
            continue;
        };
        // like `Quark.fromDirectoryEntry`, the refspec starts at the first `@`
        let (url, refspec) = match entry.split_once('@') {
            Some((url, refspec)) => (url, Some(refspec)),
            None => (entry, None),
        };
        let Ok(url) = url.parse::<Url>() else {
            warn!("Skipping the line {line:?} of {DIRECTORY_FILE}, {url} is not a valid URL");
            continue;
        };
        entries.insert(
            MagnetManifestV1::normalize_name(name),
            DirectoryEntry {
                url,
                reference: refspec.map_or(GitReference::DefaultBranch, parse_refspec),
            },
        );
    }
    entries
}

/// A refspec of `directory.txt` like `Quark.validateRefspec`: `HEAD`, a commit SHA, or a tag
/// with or without `tags/`
fn parse_refspec(refspec: &str) -> GitReference {
    if refspec == "HEAD" {
        return GitReference::DefaultBranch;
    }
    if refspec.len() == 40 && refspec.chars().all(|c| c.is_ascii_alphanumeric()) {
        return GitReference::Ref(refspec.to_string());
    }
    let tag = refspec.strip_prefix("tags/").unwrap_or(refspec);
    GitReference::Tag(tag.to_string())
}

/// The legacy quarks directory, which is fetched once, on the first lookup
pub struct QuarksDirectory<'a> {
    url: Url,
    cache: &'a GitCache,
    entries: OnceCell<BTreeMap<String, DirectoryEntry>>,
}

impl<'a> QuarksDirectory<'a> {
    pub fn new(url: Url, cache: &'a GitCache) -> Self {
        Self {
            url,
            cache,
            entries: OnceCell::new(),
        }
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    /// The entry of the legacy quark `name` (in any spelling), if the directory has one
    pub fn lookup(&self, name: &str) -> anyhow::Result<Option<DirectoryEntry>> {
        if self.entries.get().is_none() {
            let sha = self
                .cache
                .resolve(&self.url, &GitReference::DefaultBranch)
                .with_context(|| format!("Could not fetch the quarks directory {}", self.url))?;
            let content = self
                .cache
                .read_file(&self.url, &sha, DIRECTORY_FILE)?
                .with_context(|| {
                    format!("The quarks directory {} has no {DIRECTORY_FILE}", self.url)
                })?;
            let _ = self.entries.set(parse_directory(&content));
        }
        Ok(self
            .entries
            .get()
            .and_then(|entries| entries.get(&MagnetManifestV1::normalize_name(name)))
            .cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_client::source::tests::commit_files;

    #[test]
    fn test_parse_directory() {
        let entries = parse_directory(
            "3Dj=https://github.com/supercollider-quarks/3Dj\n\
             BBCut=https://github.com/snappizz/BBCut@tags/v2.2\n\
             Pinned=https://x.org/pinned@0123456789abcdef0123456789abcdef01234567\n\
             Head=https://x.org/head@HEAD\n\
             Bare=https://x.org/bare@1.0\n\
             \n\
             broken line\n",
        );
        let entry = |name: &str| entries[name].clone();
        assert_eq!(
            entry("3dj"),
            DirectoryEntry {
                url: "https://github.com/supercollider-quarks/3Dj"
                    .parse()
                    .unwrap(),
                reference: GitReference::DefaultBranch,
            }
        );
        assert_eq!(
            entry("bbcut").reference,
            GitReference::Tag("v2.2".to_string())
        );
        assert_eq!(
            entry("pinned").reference,
            GitReference::Ref("0123456789abcdef0123456789abcdef01234567".to_string())
        );
        assert_eq!(entry("head").reference, GitReference::DefaultBranch);
        assert_eq!(
            entry("bare").reference,
            GitReference::Tag("1.0".to_string())
        );
        assert_eq!(entries.len(), 5);
    }

    #[test]
    fn test_lookup_by_normalized_name() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("quarks");
        commit_files(
            &repo,
            &[(
                DIRECTORY_FILE,
                "ddwCommon=https://github.com/jamshark70/ddwCommon\n",
            )],
        );
        let cache = GitCache::new(dir.path().join("cache"));
        let directory = QuarksDirectory::new(Url::from_directory_path(&repo).unwrap(), &cache);
        assert_eq!(
            directory.lookup("DDWCommon").unwrap().unwrap().url.as_str(),
            "https://github.com/jamshark70/ddwCommon"
        );
        assert_eq!(directory.lookup("unknown").unwrap(), None);
    }
}
