//! Whether the commit which gets published is tagged with its version (C11.6)

use std::path::Path;

use anyhow::{bail, Context};
use git2::{Oid, Repository};
use tracing::debug;

/// A warning if the project is in a git repository which tags its versions (`v1.2.0` or
/// `1.2.0`), but its HEAD has no tag of `version`, or that tag points to another commit,
/// e.g. because the wrong checkout gets published. `None` without git, without version tags or
/// if HEAD has the tag, so projects which don't tag their versions never get a warning.
/// Problems reading the repository only get logged, since this check must never stop
/// publishing.
pub fn version_tag_warning(root: &Path, version: &str) -> Option<String> {
    match tag_state(root, version) {
        // only for repositories which tag their versions, so it's no noise for others
        Ok(
            state @ (TagState::Untagged {
                tags_versions: true,
                ..
            }
            | TagState::Elsewhere { .. }),
        ) => Some(state.message(version, "although the repository tags its versions")),
        Ok(_) => None,
        Err(err) => {
            debug!("Could not check the version tag: {err:#}");
            None
        }
    }
}

/// Fails unless HEAD carries the tag of `version` (`v1.2.0` or `1.2.0`), for
/// `magnet publish --require-tag` (C12.5), e.g. in a CI which publishes on a pushed tag
pub fn require_version_tag(root: &Path, version: &str) -> anyhow::Result<()> {
    let state = tag_state(root, version).context("Could not check the version tag")?;
    match state {
        TagState::Tagged => Ok(()),
        TagState::NoRepository => bail!(
            "--require-tag needs a git repository whose HEAD is tagged with v{version}, but {root:?} isn't in one"
        ),
        state => bail!(state.message(version, "which --require-tag requires")),
    }
}

/// How HEAD relates to the tags of a version
enum TagState {
    /// not in a git repository, or one without commits
    NoRepository,
    /// HEAD has the tag of the version
    Tagged,
    /// no tag of the version, `tags_versions` if the repository has other version tags
    Untagged { head: String, tags_versions: bool },
    /// the tag of the version is on another commit
    Elsewhere {
        tag: String,
        commit: String,
        head: String,
    },
}

impl TagState {
    /// The problem, with `why` it matters after the missing tag
    fn message(&self, version: &str, why: &str) -> String {
        match self {
            TagState::Untagged { head, .. } => format!(
                "HEAD ({head}) has no tag of version {version}, {why}, e.g. via `git tag v{version}`"
            ),
            TagState::Elsewhere { tag, commit, head } => format!(
                "The tag {tag} of version {version} points to the commit {commit}, but HEAD is {head}: is this the checkout to publish?"
            ),
            TagState::NoRepository | TagState::Tagged => String::new(),
        }
    }
}

/// The prefix of the version tags (`"v"` if any of them has it, else `""`) if the project is
/// in a git repository which tags its versions, e.g. for the hint to tag a new version
/// (`magnet version`, C11.7)
pub fn version_tag_prefix(root: &Path) -> Option<&'static str> {
    let tags = Repository::discover(root)
        .ok()
        .and_then(|repo| version_tags(&repo).ok())?;
    match tags.is_empty() {
        true => None,
        false if tags.iter().any(|(name, _)| name.starts_with('v')) => Some("v"),
        false => Some(""),
    }
}

/// The tags of `repo` which are versions, with the commit each points to
fn version_tags(repo: &Repository) -> anyhow::Result<Vec<(String, Oid)>> {
    let mut version_tags = vec![];
    for name in repo.tag_names(None)?.iter().flatten() {
        if !is_version_tag(name) {
            continue;
        }
        // annotated tags point to a tag object, which points to the commit
        let commit = repo
            .revparse_single(&format!("refs/tags/{name}"))?
            .peel_to_commit()?
            .id();
        version_tags.push((name.to_string(), commit));
    }
    Ok(version_tags)
}

fn tag_state(root: &Path, version: &str) -> anyhow::Result<TagState> {
    let Ok(repo) = Repository::discover(root) else {
        return Ok(TagState::NoRepository);
    };
    let Ok(head) = repo.head().and_then(|head| head.peel_to_commit()) else {
        return Ok(TagState::NoRepository);
    };
    let version_tags = version_tags(&repo)?;
    let own: Vec<&(String, Oid)> = version_tags
        .iter()
        .filter(|(name, _)| name.strip_prefix('v').unwrap_or(name) == version)
        .collect();
    if own.iter().any(|(_, commit)| *commit == head.id()) {
        return Ok(TagState::Tagged);
    }
    let head_id = short(&repo, head.id());
    Ok(match own.first() {
        Some((tag, commit)) => TagState::Elsewhere {
            tag: tag.clone(),
            commit: short(&repo, *commit),
            head: head_id,
        },
        None => TagState::Untagged {
            head: head_id,
            tags_versions: !version_tags.is_empty(),
        },
    })
}

/// `v1.2.0` or `1.2.0`
fn is_version_tag(name: &str) -> bool {
    semver::Version::parse(name.strip_prefix('v').unwrap_or(name)).is_ok()
}

/// The abbreviated hash of `commit`, like git shows it
fn short(repo: &Repository, commit: Oid) -> String {
    repo.find_object(commit, None)
        .and_then(|object| object.short_id())
        .ok()
        .and_then(|id| id.as_str().map(String::from))
        .unwrap_or_else(|| commit.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use git2::Signature;

    use super::*;

    /// a repository with one commit per entry of `commits`, returning their ids
    fn repo(dir: &Path, commits: usize) -> (Repository, Vec<Oid>) {
        let repo = Repository::init(dir).unwrap();
        let signature = Signature::now("t", "t@t").unwrap();
        let mut ids: Vec<Oid> = vec![];
        for index in 0..commits {
            fs::write(dir.join("magnet.toml"), index.to_string()).unwrap();
            let mut git_index = repo.index().unwrap();
            git_index.add_path(Path::new("magnet.toml")).unwrap();
            let tree = repo.find_tree(git_index.write_tree().unwrap()).unwrap();
            let parents: Vec<git2::Commit> = ids
                .last()
                .map(|id| repo.find_commit(*id).unwrap())
                .into_iter()
                .collect();
            let parents: Vec<&git2::Commit> = parents.iter().collect();
            let id = repo
                .commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    "commit",
                    &tree,
                    &parents,
                )
                .unwrap();
            ids.push(id);
        }
        (repo, ids)
    }

    fn lightweight_tag(repo: &Repository, name: &str, commit: Oid) {
        let object = repo.find_object(commit, None).unwrap();
        repo.tag_lightweight(name, &object, false).unwrap();
    }

    #[test]
    fn test_no_warning_without_git_or_version_tags() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(version_tag_warning(dir.path(), "1.0.0"), None);

        let (repo, ids) = repo(dir.path(), 1);
        assert_eq!(version_tag_warning(dir.path(), "1.0.0"), None);
        // tags which aren't versions don't count
        lightweight_tag(&repo, "release", ids[0]);
        lightweight_tag(&repo, "foo-v0.9.0", ids[0]);
        assert_eq!(version_tag_warning(dir.path(), "1.0.0"), None);
    }

    #[test]
    fn test_version_tag_prefix() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(version_tag_prefix(dir.path()), None);
        let (repo, ids) = repo(dir.path(), 1);
        lightweight_tag(&repo, "release", ids[0]);
        assert_eq!(version_tag_prefix(dir.path()), None);
        lightweight_tag(&repo, "0.1.0", ids[0]);
        assert_eq!(version_tag_prefix(dir.path()), Some(""));
        lightweight_tag(&repo, "v0.2.0", ids[0]);
        assert_eq!(version_tag_prefix(dir.path()), Some("v"));
    }

    #[test]
    fn test_tagged_head() {
        for tag in ["v1.0.0", "1.0.0"] {
            let dir = tempfile::tempdir().unwrap();
            let (repo, ids) = repo(dir.path(), 2);
            lightweight_tag(&repo, "v0.9.0", ids[0]);
            lightweight_tag(&repo, tag, ids[1]);
            assert_eq!(version_tag_warning(dir.path(), "1.0.0"), None, "{tag}");
        }
        // an annotated tag, also from a folder of the repository
        let dir = tempfile::tempdir().unwrap();
        let (repo, ids) = repo(dir.path(), 1);
        let commit = repo.find_object(ids[0], None).unwrap();
        let signature = Signature::now("t", "t@t").unwrap();
        repo.tag("v1.0.0", &commit, &signature, "release", false)
            .unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        assert_eq!(version_tag_warning(&dir.path().join("sub"), "1.0.0"), None);
    }

    #[test]
    fn test_untagged_head() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, ids) = repo(dir.path(), 2);
        lightweight_tag(&repo, "v0.9.0", ids[0]);
        let warning = version_tag_warning(dir.path(), "1.0.0").unwrap();
        assert!(
            warning.starts_with("HEAD (") && warning.contains("has no tag of version 1.0.0"),
            "{warning}"
        );
        assert!(warning.contains("`git tag v1.0.0`"), "{warning}");
    }

    #[test]
    fn test_require_version_tag() {
        let dir = tempfile::tempdir().unwrap();
        let err = require_version_tag(dir.path(), "1.0.0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs a git repository"), "{err}");

        // also without any version tags, unlike the warning
        let (repo, ids) = repo(dir.path(), 2);
        assert_eq!(version_tag_warning(dir.path(), "1.0.0"), None);
        let err = require_version_tag(dir.path(), "1.0.0")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("has no tag of version 1.0.0, which --require-tag requires"),
            "{err}"
        );

        lightweight_tag(&repo, "v1.0.0", ids[0]);
        let err = require_version_tag(dir.path(), "1.0.0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("points to the commit"), "{err}");

        lightweight_tag(&repo, "1.0.0", ids[1]);
        require_version_tag(dir.path(), "1.0.0").unwrap();
    }

    #[test]
    fn test_tag_of_the_version_on_another_commit() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, ids) = repo(dir.path(), 2);
        lightweight_tag(&repo, "v1.0.0", ids[0]);
        let warning = version_tag_warning(dir.path(), "1.0.0").unwrap();
        let first = &ids[0].to_string()[..7];
        let head = &ids[1].to_string()[..7];
        assert!(
            warning.contains(&format!(
                "The tag v1.0.0 of version 1.0.0 points to the commit {first}"
            )) && warning.contains(&format!("but HEAD is {head}")),
            "{warning}"
        );
    }
}
