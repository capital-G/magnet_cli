use anyhow::Context;
use git2::Repository;
use std::{fs::read_to_string, path::PathBuf};

use tracing::{info, instrument, trace};
use url::Url;

use crate::git_client::models::GitRelease;
use crate::models::project_manifest::MagnetManifestV1;

pub struct GitClient {
    url: Url,
    local_destination: PathBuf,
    repo: Repository,
}

impl GitClient {
    pub fn new(url: Url, local_destination: PathBuf) -> anyhow::Result<Self> {
        match local_destination.exists() {
            true => {
                let repo = Repository::open(&local_destination)?;
                // @todo check if repo matches the designated URL - otherwise delete it and clone
                Ok(Self {
                    url,
                    local_destination,
                    repo,
                })
            }
            false => Self::clone(url, local_destination),
        }
    }

    pub fn clone(url: Url, local_destination: PathBuf) -> anyhow::Result<Self> {
        info!("Cloning index repo {url:?} to {local_destination:?}");
        let repo = Repository::clone(url.as_str(), &local_destination)?;
        Ok(Self {
            url,
            local_destination,
            repo,
        })
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn update(&self) -> anyhow::Result<()> {
        self.hard_reset_to_remote()
    }

    fn fetch_origin(&self) -> anyhow::Result<()> {
        let _ = self
            .repo
            .find_remote("origin")?
            .fetch(&["main"], None, None);
        Ok(())
    }

    fn hard_reset_to_remote(&self) -> anyhow::Result<()> {
        let _ = self.fetch_origin();

        let oid = self.repo.refname_to_id("refs/remotes/origin/main")?;
        let object = self.repo.find_object(oid, None)?;
        self.repo.reset(&object, git2::ResetType::Hard, None)?;
        Ok(())
    }

    #[instrument(skip(self), fields(release_path=tracing::field::Empty))]
    pub fn get_releases(&self, name: &String) -> anyhow::Result<Option<Vec<GitRelease>>> {
        let name = MagnetManifestV1::normalize_name(name);
        let release_path = self
            .local_destination
            .join(
                name.get(0..1)
                    .with_context(|| "A dependency name must have at least two characters")?,
            )
            .join(&name);

        tracing::Span::current().record("release_path", release_path.to_str());

        match release_path.is_file() {
            true => {
                let releases = read_to_string(&release_path)
                    .with_context(|| format!("Could not open git index file {release_path:?}"))?
                    .lines()
                    .filter_map(|line| match serde_json::from_str(line) {
                        Ok(release) => Some(release),
                        Err(err) => {
                            trace!(
                                "Could not parse entry in git index file {release_path:?}: {err}"
                            );
                            None
                        }
                    })
                    .collect();
                Ok(Some(releases))
            }
            false => {
                trace!("Could not find a matching file in the git index");
                Ok(None)
            }
        }
    }

    // fn resolve_dependencies(self, dependencies: Vec<GitDependency>) {
    //     todo!()
    // }
}
