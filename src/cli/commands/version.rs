use std::{env, path::PathBuf};

use anyhow::{bail, ensure, Context};
use clap::Parser;
use magnet::{
    config::constants::MAGNET_PROJECT_FILE, models::project_manifest::ProjectManifest,
    packaging::version_tag::version_tag_prefix,
};
use semver::Version;
use tracing::warn;

use crate::cli::{CliContext, MagnetSubCommand};

/// Prints the version of the current magnet project, or sets it (C11.7)
#[derive(Parser, Debug)]
pub struct VersionArgs {
    /// Path of the project, the current directory by default
    #[arg(long)]
    path: Option<PathBuf>,

    /// `major`, `minor` or `patch` to bump the version, or a version like 1.3.0 to set it.
    /// Without it, the version is only printed
    bump: Option<String>,
}

impl MagnetSubCommand for VersionArgs {
    async fn run(self, _cli_context: &CliContext) -> anyhow::Result<()> {
        let root = match self.path {
            Some(path) => path,
            None => env::current_dir()?,
        };
        let mut manifest = ProjectManifest::open(root.join(MAGNET_PROJECT_FILE))?;
        let current = manifest.get_manifest().package.version.to_string();
        let Some(bump) = self.bump else {
            // only the version, for scripts
            println!("{current}");
            return Ok(());
        };

        let new = new_version(&current, &bump)?;
        if new == current {
            println!("The version is already {current}");
            return Ok(());
        }
        let parse = |version: &str| Version::parse(version).ok();
        if let (Some(old), Some(parsed)) = (parse(&current), parse(&new)) {
            if parsed < old {
                warn!("{new} is lower than the current version {current}");
            }
        }
        manifest.set_version(&new)?;
        println!("{current} → {new}");
        if let Some(prefix) = version_tag_prefix(&root) {
            println!("Once it is committed, tag it with `git tag {prefix}{new}`");
        }
        Ok(())
    }
}

/// The version `bump` (`major`, `minor`, `patch` or a version) makes of `current`. Bumping
/// follows semver: `major` of `0.3.1` is `1.0.0`, and a pre-release needs an explicit version.
fn new_version(current: &str, bump: &str) -> anyhow::Result<String> {
    let increment = match bump {
        "major" | "minor" | "patch" => bump,
        _ => {
            ensure!(
                Version::parse(bump).is_ok(),
                "{bump} is neither `major`, `minor` nor `patch`, nor a version like 1.3.0"
            );
            return Ok(bump.to_string());
        }
    };
    let mut version = Version::parse(&current)
        .with_context(|| format!("The version {current} of {MAGNET_PROJECT_FILE} is invalid"))?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        bail!("{current} is a pre-release, so it can't be bumped: give the new version, e.g. `magnet version {}.{}.{}`", version.major, version.minor, version.patch);
    }
    match increment {
        "major" => {
            version.major += 1;
            version.minor = 0;
            version.patch = 0;
        }
        "minor" => {
            version.minor += 1;
            version.patch = 0;
        }
        _ => version.patch += 1,
    }
    Ok(version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_version() {
        for (current, bump, new) in [
            ("1.2.3", "patch", "1.2.4"),
            ("1.2.3", "minor", "1.3.0"),
            ("1.2.3", "major", "2.0.0"),
            ("0.3.1", "major", "1.0.0"),
            ("0.3.1", "minor", "0.4.0"),
            ("1.2.3", "1.5.0", "1.5.0"),
            ("1.2.3", "1.0.0", "1.0.0"),
            ("1.0.0-beta.1", "1.0.0", "1.0.0"),
        ] {
            assert_eq!(new_version(current, bump).unwrap(), new, "{current} {bump}");
        }
        for (current, bump, error) in [
            ("1.2.3", "huge", "neither `major`, `minor` nor `patch`"),
            ("1.2.3", "1.3", "neither `major`"),
            ("1.0.0-beta.1", "patch", "is a pre-release"),
        ] {
            let err = new_version(current, bump).unwrap_err().to_string();
            assert!(err.contains(error), "{current} {bump}: {err}");
        }
    }
}
