use anyhow::Context;
use clap::{command, Parser, Subcommand};
use magnet::{git_client::models::GitDependency, resolver::online_provider::RootProvider};
use pubgrub::{resolve, DefaultStringReporter, PubGrubError, Reporter, SemanticVersion};
use tracing::info;

use crate::cli::{CliContext, MagnetSubCommand};

#[derive(Parser, Debug)]
#[command(name = "index", about = "Package index")]
pub struct IndexArgs {
    #[command(subcommand)]
    command: IndexCommand,
}

#[derive(Debug, Parser)]
pub struct SearchArgs {
    name: String,
}

#[derive(Debug, Parser)]
pub struct ResolveArgs {
    name: String,
    version: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum IndexCommand {
    /// Update remote index
    Update,
    /// Search for a package
    Search(SearchArgs),
    Resolve(ResolveArgs),
    Foobar,
}

impl MagnetSubCommand for IndexArgs {
    async fn run(self, cli_context: &CliContext) -> anyhow::Result<()> {
        let git_client = cli_context.git_client()?;
        match self.command {
            IndexCommand::Update => match git_client.update() {
                Ok(_) => {
                    info!("Successfully updated index")
                }
                Err(_) => {
                    info!("Could not update")
                }
            },
            IndexCommand::Search(args) => match git_client.get_releases(&args.name).unwrap() {
                Some(releases) => {
                    println!("{releases:?}");
                }
                None => {
                    println!("Found no releases")
                }
            },
            IndexCommand::Resolve(args) => {
                let search_version = match args.version {
                    Some(v) => v
                        .parse::<SemanticVersion>()
                        .with_context(|| format!("Invalid semantic version format: {v}"))?,
                    None => SemanticVersion::zero(),
                };
                match resolve(&git_client, args.name, search_version) {
                    Ok(packages) => println!("{packages:?}"),
                    Err(PubGrubError::NoSolution(mut derivation_tree)) => {
                        derivation_tree.collapse_no_versions();
                        eprintln!("{}", DefaultStringReporter::report(&derivation_tree));
                    }
                    Err(err) => panic!("{:?}", err),
                }
            }
            IndexCommand::Foobar => {
                let root_provider = RootProvider {
                    git_provider: &git_client,
                    dependencies: vec![GitDependency {
                        name: "jitlib".to_string(),
                        constraint: "<2.0.0".to_string().parse()?,
                    }],
                    preferred: Default::default(),
                    sclang: None,
                };
                match resolve(&root_provider, "root".to_string(), SemanticVersion::zero()) {
                    Ok(dependencies) => {
                        println!("{:?}", dependencies)
                    }
                    Err(err) => {
                        println!("Error: {}", err)
                    }
                }
            }
        }
        Ok(())
    }
}
