use std::{env, path::PathBuf};

use clap::{builder::BoolishValueParser, Parser};
use magnet::{
    http_client::models::VersionState,
    models::project::Project,
    packaging::{
        publish::{check_publishable, publish, publish_archives},
        version_tag::{require_version_tag, version_tag_warning},
    },
};
use tracing::warn;

use crate::cli::{CliContext, MagnetSubCommand};

/// Publishes packages of the current magnet project on the magnet server
#[derive(Parser, Debug)]
pub struct PublishArgs {
    /// Path of the project, the current directory by default
    #[arg(long)]
    path: Option<PathBuf>,

    /// Stop before uploading unless HEAD is tagged with the version of magnet.toml
    /// (`v1.2.0` or `1.2.0`), e.g. in a CI which publishes on a pushed tag
    #[arg(long, env = "MAGNET_REQUIRE_TAG", value_parser = BoolishValueParser::new())]
    require_tag: bool,

    /// Packages to publish, by default all of the current version in `dist/`
    files: Vec<PathBuf>,
}

impl MagnetSubCommand for PublishArgs {
    async fn run(self, cli_context: &CliContext) -> anyhow::Result<()> {
        let project_path = match self.path {
            Some(path) => path,
            None => env::current_dir()?,
        };
        let project = Project::open(&project_path)?;
        let manifest = project.manifest();
        check_publishable(manifest)?;
        let archives = publish_archives(project.root(), manifest, &self.files)?;
        let version = manifest.package.version.to_string();
        if self.require_tag {
            require_version_tag(project.root(), &version)?;
        // otherwise only a warning: CI may publish from a branch, and some don't tag (C11.6)
        } else if let Some(warning) = version_tag_warning(project.root(), &version) {
            warn!("{warning}");
        }

        let client = cli_context.authenticated_client().await?;
        let published = publish(&client, manifest, &archives).await?;
        match published.state {
            VersionState::Final => println!(
                "Published {} {} on {}",
                published.name, published.version, client.base_url
            ),
            VersionState::Draft => println!(
                "Uploaded to the draft of {} {} on {}",
                published.name, published.version, client.base_url
            ),
        }
        Ok(())
    }
}
