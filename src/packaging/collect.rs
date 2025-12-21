use std::path::PathBuf;

use git2::Repository;
use walkdir::WalkDir;

use crate::models::package::BundleType;

pub trait CollectFiles {
    fn get_package_files(&self, bundle_type: BundleType) -> anyhow::Result<Vec<PathBuf>>;
}

impl CollectFiles for PathBuf {
    fn get_package_files(&self, _bundle_type: BundleType) -> anyhow::Result<Vec<PathBuf>> {
        let entries = WalkDir::new(self)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_file())
            .map(|entry| entry.path().to_path_buf())
            .collect::<Vec<PathBuf>>();

        Ok(entries)
    }
}

impl CollectFiles for Repository {
    fn get_package_files(&self, bundle_type: BundleType) -> anyhow::Result<Vec<PathBuf>> {
        let repo_base_path = self
            .path()
            .parent()
            .ok_or(anyhow::anyhow!("Failed to access parent of .git repo"))?;

        repo_base_path.to_path_buf().get_package_files(bundle_type)
    }
}

pub trait PackageFileFilter {
    fn filter_files(&self, paths: Vec<PathBuf>, bundle_type: BundleType) -> Vec<PathBuf>;
}

impl PackageFileFilter for Repository {
    fn filter_files(&self, paths: Vec<PathBuf>, _bundle_type: BundleType) -> Vec<PathBuf> {
        paths
            .iter()
            .filter(|path| !self.is_path_ignored(path).unwrap_or(true))
            .map(|path| path.to_path_buf())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_package_source_repo() -> anyhow::Result<()> {
        let repo = Repository::open(".")?;

        let mut entries = repo.get_package_files(BundleType::SourceBundle)?;
        entries = repo.filter_files(entries, BundleType::SourceBundle);

        assert!(entries.iter().any(|p| p.ends_with("magnet_cli/Cargo.toml")));

        Ok(())
    }
}
