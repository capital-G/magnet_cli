use pubgrub::{Dependencies, DependencyConstraints, DependencyProvider, Ranges, SemanticVersion};
use semver::VersionReq;
use std::convert::Infallible;
use tracing::error;

use crate::git_client::{client::GitClient, models::GitDependency};
use crate::models::project_manifest::MagnetManifestV1;

pub trait ToRanges {
    fn to_ranges(&self) -> Ranges<SemanticVersion>;
}

impl ToRanges for VersionReq {
    fn to_ranges(&self) -> Ranges<SemanticVersion> {
        let mut boundaries = Ranges::<SemanticVersion>::full();

        for comperator in &self.comparators {
            let semantic_version = SemanticVersion::new(
                comperator.major.try_into().unwrap_or(0),
                comperator.minor.unwrap_or(0).try_into().unwrap_or(0),
                comperator.patch.unwrap_or(0).try_into().unwrap_or(0),
            );

            let bound = match comperator.op {
                semver::Op::Exact => Ranges::singleton(semantic_version),
                semver::Op::Greater => Ranges::strictly_higher_than(semantic_version),
                semver::Op::GreaterEq => Ranges::higher_than(semantic_version),
                semver::Op::Less => Ranges::strictly_lower_than(semantic_version),
                semver::Op::LessEq => Ranges::lower_than(semantic_version),
                semver::Op::Caret => {
                    // ^1.2.3 is equal to >= 1.2.3, <2.0.0
                    let upper =
                        SemanticVersion::new((comperator.major + 1).try_into().unwrap_or(0), 0, 0);
                    Ranges::higher_than(semantic_version)
                        .intersection(&Ranges::strictly_lower_than(upper))
                }
                semver::Op::Tilde => {
                    // ~1.2.3 is equal to >= 1.2.3, <1.3.0
                    let upper = SemanticVersion::new(
                        comperator.major.try_into().unwrap_or(0),
                        comperator.minor.unwrap_or(0).try_into().unwrap_or(0) + 1,
                        0,
                    );
                    Ranges::higher_than(semantic_version)
                        .intersection(&Ranges::strictly_lower_than(upper))
                }
                semver::Op::Wildcard => Ranges::full(),
                _ => {
                    error!("Unknown comperator");
                    Ranges::full()
                }
            };

            boundaries = boundaries.intersection(&bound);
        }
        boundaries
    }
}

fn dependency_list_to_hash_map(
    dependencies: &[GitDependency],
) -> anyhow::Result<DependencyConstraints<String, Ranges<SemanticVersion>>> {
    let map = dependencies
        .iter()
        // normalized, so differently spelled references resolve to the same package
        .map(|d| {
            (
                MagnetManifestV1::normalize_name(&d.name),
                d.constraint.to_ranges(),
            )
        })
        .collect();
    Ok(map)
}

impl DependencyProvider for GitClient {
    type P = String;
    type V = SemanticVersion;
    type VS = Ranges<Self::V>;
    type M = String;

    type Priority = u32;

    type Err = Infallible;

    fn get_dependencies(
        &self,
        package: &Self::P,
        version: &Self::V,
    ) -> anyhow::Result<pubgrub::Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        let releases = self.get_releases(package).expect("Something").unwrap();
        match releases
            .into_iter()
            .find(|release| release.version == *version)
        {
            Some(release) => {
                let dependencies = dependency_list_to_hash_map(&release.dependencies).unwrap();

                Ok(Dependencies::Available(dependencies))
            }
            None => Ok(Dependencies::Unavailable(format!(
                "No dependency for {package}"
            ))),
        }
    }

    fn prioritize(
        &self,
        _package: &Self::P,
        _range: &Self::VS,
        package_conflicts_counts: &pubgrub::PackageResolutionStatistics,
    ) -> Self::Priority {
        // plainly use the conflict count for now?
        package_conflicts_counts.conflict_count()
    }

    #[inline]
    fn choose_version(
        &self,
        package: &Self::P,
        range: &Self::VS,
    ) -> anyhow::Result<Option<Self::V>, Self::Err> {
        match self
            .get_releases(package)
            .expect("Could not obtain release file")
        {
            Some(mut releases) => {
                releases.sort_by(|a, b| a.version.cmp(&b.version));
                // sort is ascending, but we want descending
                releases.reverse();

                match releases
                    .iter()
                    .find(|release| range.contains(&release.version))
                {
                    Some(release) => Ok(Some(release.version)),
                    None => Ok(None),
                }
            }
            None => Ok(None),
        }
    }
}

/// The RootProvider acts wraps the GitProvider sucht that the meta package `root` can
/// be injected to the package index.
/// The goal is to resolve the meta-package root which contains all the dependencies
/// we need for the project.
/// The root package always uses semver 0.0.0!
// Maybe use this pattern instead? https://pubgrub-rs-guide.pages.dev/pubgrub_crate/caching ?
pub struct RootProvider {
    pub git_provider: GitClient,
    pub dependencies: Vec<GitDependency>,
}

impl DependencyProvider for RootProvider {
    type P = String;
    type V = SemanticVersion;
    type VS = Ranges<Self::V>;
    type M = String;

    type Priority = u32;

    type Err = Infallible;

    fn prioritize(
        &self,
        package: &Self::P,
        range: &Self::VS,
        package_conflicts_counts: &pubgrub::PackageResolutionStatistics,
    ) -> Self::Priority {
        self.git_provider
            .prioritize(package, range, package_conflicts_counts)
    }

    fn choose_version(
        &self,
        package: &Self::P,
        range: &Self::VS,
    ) -> Result<Option<Self::V>, Self::Err> {
        if package == "root" {
            return Ok(Some(SemanticVersion::zero()));
        }
        self.git_provider.choose_version(package, range)
    }

    fn get_dependencies(
        &self,
        package: &Self::P,
        version: &Self::V,
    ) -> Result<Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        if package == "root" {
            Ok(Dependencies::Available(
                dependency_list_to_hash_map(&self.dependencies).unwrap(),
            ))
        } else {
            self.git_provider.get_dependencies(package, version)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact() {
        let req = VersionReq::parse("=1.0.0").unwrap().to_ranges();
        assert!(req.contains(&SemanticVersion::one()));
        assert!(!req.contains(&SemanticVersion::zero()));
        assert!(!req.contains(&SemanticVersion::new(1, 1, 0)));
        assert!(!req.contains(&SemanticVersion::new(1, 0, 4)));
        assert!(!req.contains(&SemanticVersion::new(2, 0, 0)));
    }

    #[test]
    fn test_greater() {
        let req = VersionReq::parse(">1.0.0").unwrap().to_ranges();
        assert!(!req.contains(&SemanticVersion::one()));
        assert!(!req.contains(&SemanticVersion::zero()));
        assert!(req.contains(&SemanticVersion::new(1, 1, 0)));
        assert!(req.contains(&SemanticVersion::new(1, 0, 4)));
        assert!(req.contains(&SemanticVersion::new(2, 0, 0)));
    }

    #[test]
    fn test_greater_equal() {
        let req = VersionReq::parse(">=1.0.0").unwrap().to_ranges();
        assert!(req.contains(&SemanticVersion::one()));
        assert!(!req.contains(&SemanticVersion::zero()));
        assert!(req.contains(&SemanticVersion::new(1, 1, 0)));
        assert!(req.contains(&SemanticVersion::new(1, 0, 4)));
        assert!(req.contains(&SemanticVersion::new(2, 0, 0)));
    }

    #[test]
    fn test_less() {
        let req = VersionReq::parse("<1.1.0").unwrap().to_ranges();
        assert!(req.contains(&SemanticVersion::one()));
        assert!(req.contains(&SemanticVersion::zero()));
        assert!(!req.contains(&SemanticVersion::new(1, 1, 0)));
        assert!(req.contains(&SemanticVersion::new(1, 0, 4)));
        assert!(!req.contains(&SemanticVersion::new(2, 0, 0)));
    }

    #[test]
    fn test_less_equal() {
        let req = VersionReq::parse("<=1.1.0").unwrap().to_ranges();
        assert!(req.contains(&SemanticVersion::one()));
        assert!(req.contains(&SemanticVersion::zero()));
        assert!(req.contains(&SemanticVersion::new(1, 1, 0)));
        assert!(req.contains(&SemanticVersion::new(1, 0, 4)));
        assert!(!req.contains(&SemanticVersion::new(2, 0, 0)));
    }

    #[test]
    fn test_caret() {
        let req = VersionReq::parse("^1.0.0").unwrap().to_ranges();
        assert!(req.contains(&SemanticVersion::one()));
        assert!(!req.contains(&SemanticVersion::zero()));
        assert!(req.contains(&SemanticVersion::new(1, 2, 0)));
        assert!(!req.contains(&SemanticVersion::new(0, 2, 0)));
        assert!(!req.contains(&SemanticVersion::new(2, 0, 0)));
    }

    #[test]
    fn test_tilde() {
        let req = VersionReq::parse("~1.0.0").unwrap().to_ranges();
        assert!(req.contains(&SemanticVersion::one()));
        assert!(!req.contains(&SemanticVersion::zero()));
        assert!(!req.contains(&SemanticVersion::new(1, 1, 0)));
        assert!(req.contains(&SemanticVersion::new(1, 0, 4)));
        assert!(!req.contains(&SemanticVersion::new(2, 0, 0)));
    }

    #[test]
    fn test_wildcard() {
        let req = VersionReq::parse("*").unwrap().to_ranges();
        assert!(req.contains(&SemanticVersion::one()));
        assert!(req.contains(&SemanticVersion::zero()));
        assert!(req.contains(&SemanticVersion::new(1, 1, 0)));
        assert!(req.contains(&SemanticVersion::new(1, 0, 4)));
        assert!(req.contains(&SemanticVersion::new(2, 0, 0)));
    }
}
