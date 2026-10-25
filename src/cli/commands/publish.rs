use std::{env, path::PathBuf};

use clap::Parser;
use magnet::{
    http_client::models::VersionState,
    models::project::Project,
    packaging::publish::{check_publishable, publish, publish_archives},
};

use crate::cli::{CliContext, MagnetSubCommand};

/// Publishes packages of the current magnet project on the magnet server
#[derive(Parser, Debug)]
pub struct PublishArgs {
    /// Path of the project, the current directory by default
    #[arg(long)]
    path: Option<PathBuf>,

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
