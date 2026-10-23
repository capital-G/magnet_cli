use anyhow::bail;
use pubgrub::{
    DefaultStringReporter, Dependencies, DependencyConstraints, DependencyProvider, PubGrubError,
    Ranges, Reporter, SelectedDependencies, SemanticVersion,
};
use semver::VersionReq;
use std::fmt::Display;
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
) -> DependencyConstraints<String, Ranges<SemanticVersion>> {
    dependencies
        .iter()
        // normalized, so differently spelled references resolve to the same package
        .map(|d| {
            (
                MagnetManifestV1::normalize_name(&d.name),
                d.constraint.to_ranges(),
            )
        })
        .collect()
}

/// pubgrub requires the error of a [DependencyProvider] to implement [std::error::Error],
/// which [anyhow::Error] does not.
#[derive(Debug)]
pub struct IndexError(anyhow::Error);

impl Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for IndexError {}

impl From<anyhow::Error> for IndexError {
    fn from(value: anyhow::Error) -> Self {
        Self(value)
    }
}

impl DependencyProvider for GitClient {
    type P = String;
    type V = SemanticVersion;
    type VS = Ranges<Self::V>;
    type M = String;

    type Priority = u32;

    type Err = IndexError;

    fn get_dependencies(
        &self,
        package: &Self::P,
        version: &Self::V,
    ) -> Result<pubgrub::Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        match self.get_release(package, version)? {
            Some(release) => match release.version_dependencies() {
                Ok(dependencies) => Ok(Dependencies::Available(dependency_list_to_hash_map(
                    &dependencies,
                ))),
                // pubgrub appends this message to "dependencies of .. are unavailable"
                Err(err) => Ok(Dependencies::Unavailable(format!("({err})"))),
            },
            None => Ok(Dependencies::Unavailable(format!(
                "({package} {version} is missing in the index)"
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
    ) -> Result<Option<Self::V>, Self::Err> {
        // an unknown package has no versions, which pubgrub reports as such
        let releases = self.get_releases(package)?.unwrap_or_default();
        Ok(releases
            .iter()
            .filter(|release| !release.yanked && range.contains(&release.version))
            .map(|release| release.version)
            .max())
    }
}

/// Name of the meta package which holds the dependencies of the project.
pub const ROOT_PACKAGE: &str = "root";

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

    type Err = IndexError;

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
        if package == ROOT_PACKAGE {
            return Ok(Some(SemanticVersion::zero()));
        }
        self.git_provider.choose_version(package, range)
    }

    fn get_dependencies(
        &self,
        package: &Self::P,
        version: &Self::V,
    ) -> Result<Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        if package == ROOT_PACKAGE {
            Ok(Dependencies::Available(dependency_list_to_hash_map(
                &self.dependencies,
            )))
        } else {
            self.git_provider.get_dependencies(package, version)
        }
    }
}

/// Resolves the dependencies of the root package and turns a failure into a readable error
pub fn resolve_dependencies(
    provider: &RootProvider,
) -> anyhow::Result<SelectedDependencies<RootProvider>> {
    match pubgrub::resolve(provider, ROOT_PACKAGE.to_string(), SemanticVersion::zero()) {
        Ok(selected) => Ok(selected),
        // no collapse_no_versions(): a cached index may lack versions, so
        // "there is no available version" is valuable information
        Err(PubGrubError::NoSolution(derivation_tree)) => {
            bail!(
                "Could not resolve the dependencies:\n{}",
                DefaultStringReporter::report(&derivation_tree)
            )
        }
        Err(PubGrubError::ErrorRetrievingDependencies {
            package,
            version,
            source,
        }) => bail!("Could not obtain the dependencies of {package} {version}: {source}"),
        Err(PubGrubError::ErrorChoosingVersion { package, source }) => {
            bail!("Could not choose a version of {package}: {source}")
        }
        Err(PubGrubError::ErrorInShouldCancel(source)) => {
            bail!("Dependency resolution was cancelled: {source}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn resolve_fixture(dependencies: &[(&str, &str)]) -> anyhow::Result<Vec<(String, String)>> {
        let provider = RootProvider {
            git_provider: GitClient::open_local(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/index"),
            )?,
            dependencies: dependencies
                .iter()
                .map(|(name, constraint)| GitDependency {
                    name: name.to_string(),
                    constraint: constraint.parse().unwrap(),
                })
                .collect(),
        };
        let mut selected: Vec<(String, String)> = resolve_dependencies(&provider)?
            .into_iter()
            .filter(|(name, _)| name != ROOT_PACKAGE)
            .map(|(name, version)| (name, version.to_string()))
            .collect();
        selected.sort();
        Ok(selected)
    }

    fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
        expected
            .iter()
            .map(|(name, version)| (name.to_string(), version.to_string()))
            .collect()
    }

    #[test]
    fn test_resolve_transitive() {
        assert_eq!(
            resolve_fixture(&[("alpha", "^1.0.0")]).unwrap(),
            pairs(&[
                ("alpha", "1.1.0"),
                ("beta", "1.2.0"),
                ("ddw-common", "0.1.0")
            ])
        );
    }

    #[test]
    fn test_resolve_skips_yanked() {
        // alpha 2.0.0 is yanked
        let selected = resolve_fixture(&[("alpha", "*")]).unwrap();
        assert!(selected.contains(&("alpha".to_string(), "1.1.0".to_string())));
    }

    #[test]
    fn test_resolve_unknown_package() {
        let err = resolve_fixture(&[("unknown", "*")])
            .unwrap_err()
            .to_string();
        println!("{err}");
        assert!(err.contains("unknown"));
    }

    #[test]
    fn test_resolve_conflict() {
        // alpha 1.1 requires beta ^1, delta 0.1 requires beta <1
        let err = resolve_fixture(&[("alpha", "^1.1.0"), ("delta", "=0.1.0")])
            .unwrap_err()
            .to_string();
        println!("{err}");
        assert!(err.contains("beta"));
    }

    #[test]
    fn test_resolve_legacy_quark_dependency() {
        let err = resolve_fixture(&[("delta", "=0.2.0")])
            .unwrap_err()
            .to_string();
        println!("{err}");
        assert!(err.contains("legacy quark"));
    }

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
