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
        self.comparators
            .iter()
            .fold(Ranges::full(), |ranges, comparator| {
                ranges.intersection(&comparator_to_ranges(comparator))
            })
    }
}

/// Follows the semantics of the semver crate (and therefore Cargo), see
/// https://docs.rs/semver/latest/semver/enum.Op.html.
/// Pre-releases are not supported and get ignored.
fn comparator_to_ranges(comparator: &semver::Comparator) -> Ranges<SemanticVersion> {
    let number = |n: u64| -> u32 { n.try_into().unwrap_or(u32::MAX) };
    let major = number(comparator.major);
    let minor = comparator.minor.map(number);
    let patch = comparator.patch.map(number);

    // a partial version stands for all versions it covers, e.g. `1.2` for `1.2.x`,
    // so it spans from `lower` (1.2.0) to the first version after it, `next` (1.3.0)
    let lower = SemanticVersion::new(major, minor.unwrap_or(0), patch.unwrap_or(0));
    let next = match (minor, patch) {
        (None, _) => SemanticVersion::new(major + 1, 0, 0),
        (Some(minor), None) => SemanticVersion::new(major, minor + 1, 0),
        (Some(minor), Some(patch)) => SemanticVersion::new(major, minor, patch + 1),
    };
    let up_to = |upper: SemanticVersion| Ranges::between(lower, upper);

    match comparator.op {
        semver::Op::Exact | semver::Op::Wildcard => up_to(next),
        semver::Op::Greater => Ranges::higher_than(next),
        semver::Op::GreaterEq => Ranges::higher_than(lower),
        semver::Op::Less => Ranges::strictly_lower_than(lower),
        semver::Op::LessEq => Ranges::strictly_lower_than(next),
        // ~1.2.3 := >=1.2.3, <1.3.0 and ~1 := >=1.0.0, <2.0.0
        semver::Op::Tilde => match minor {
            Some(minor) => up_to(SemanticVersion::new(major, minor + 1, 0)),
            None => up_to(SemanticVersion::new(major + 1, 0, 0)),
        },
        // the left-most non-zero component must not change, e.g.
        // ^1.2.3 := <2.0.0, ^0.2.3 := <0.3.0, ^0.0.3 := =0.0.3
        semver::Op::Caret => match (major, minor) {
            (0, Some(0)) => up_to(next),
            (0, Some(minor)) => up_to(SemanticVersion::new(0, minor + 1, 0)),
            _ => up_to(SemanticVersion::new(major + 1, 0, 0)),
        },
        _ => {
            error!("Unknown comparator {comparator}");
            Ranges::full()
        }
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
pub struct RootProvider<'a> {
    pub git_provider: &'a GitClient,
    pub dependencies: Vec<GitDependency>,
}

impl DependencyProvider for RootProvider<'_> {
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
pub fn resolve_dependencies<'a>(
    provider: &RootProvider<'a>,
) -> anyhow::Result<SelectedDependencies<RootProvider<'a>>> {
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
        let index = GitClient::open_local(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/index"),
        )?;
        let provider = RootProvider {
            git_provider: &index,
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

    /// the semver crate is the reference implementation of the version requirements
    #[test]
    fn test_ranges_match_semver() {
        let requirements = [
            "1.2.3",
            "1.2",
            "1",
            "0.2.3",
            "0.2",
            "0.0.3",
            "0.0",
            "0",
            "=1.2.3",
            "=1.2",
            "=1",
            ">1.2.3",
            ">1.2",
            ">1",
            ">=1.2.3",
            ">=1.2",
            "<1.2.3",
            "<1.2",
            "<1",
            "<=1.2.3",
            "<=1.2",
            "<=1",
            "~1.2.3",
            "~1.2",
            "~1",
            "~0.2.3",
            "^1.2.3",
            "^1.2",
            "^1",
            "^0.2.3",
            "^0.2",
            "^0.0.3",
            "^0.0",
            "^0",
            "1.*",
            "1.2.*",
            "*",
            ">=1.2, <1.5",
            "^1.2, <1.4",
        ];
        for requirement in requirements {
            let req = VersionReq::parse(requirement).unwrap();
            let ranges = req.to_ranges();
            for major in 0..3 {
                for minor in 0..4 {
                    for patch in 0..5 {
                        assert_eq!(
                            ranges.contains(&SemanticVersion::new(major, minor, patch)),
                            req.matches(&semver::Version::new(
                                major.into(),
                                minor.into(),
                                patch.into()
                            )),
                            "{requirement} vs {major}.{minor}.{patch}"
                        );
                    }
                }
            }
        }
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
