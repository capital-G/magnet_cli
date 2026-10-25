use anyhow::bail;
use pubgrub::{
    DefaultStringReporter, Dependencies, DependencyConstraints, DependencyProvider, PubGrubError,
    Ranges, Reporter, SelectedDependencies, SemanticVersion,
};
use semver::VersionReq;
use std::{collections::HashMap, fmt::Display, ops::Bound};
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

/// The versions of SuperCollider a dependency's `sclang` range allows while resolving: its upper
/// bounds are ignored, since defensive caps would block the resolution (DESIGN.md
/// "SuperCollider version"), e.g. `>=3.13, <3.14` and `^3.13` both become `>=3.13.0`.
pub fn sclang_lower_bound(requirement: &VersionReq) -> Ranges<SemanticVersion> {
    match requirement.to_ranges().bounding_range() {
        Some((Bound::Included(lower), _)) => Ranges::higher_than(*lower),
        Some((Bound::Excluded(lower), _)) => Ranges::strictly_higher_than(*lower),
        Some((Bound::Unbounded, _)) => Ranges::full(),
        None => Ranges::empty(),
    }
}

/// The lowest version of SuperCollider a project's `sclang` range allows, for which the
/// dependencies get resolved, so the lock file holds for the whole range
fn lowest_sclang_version(requirement: &VersionReq) -> SemanticVersion {
    match requirement.to_ranges().bounding_range() {
        Some((Bound::Included(lower), _)) => *lower,
        Some((Bound::Excluded(lower), _)) => lower.bump_patch(),
        _ => SemanticVersion::zero(),
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

/// Name of the virtual package for the version of SuperCollider, like `Python` in uv.
/// Its only version is the lowest one the project allows. Upper case, so it can't clash with
/// the normalized name of a package.
pub const SCLANG_PACKAGE: &str = "SuperCollider";

/// The RootProvider acts wraps the GitProvider sucht that the meta package `root` can
/// be injected to the package index.
/// The goal is to resolve the meta-package root which contains all the dependencies
/// we need for the project.
/// The root package always uses semver 0.0.0!
// Maybe use this pattern instead? https://pubgrub-rs-guide.pages.dev/pubgrub_crate/caching ?
pub struct RootProvider<'a> {
    pub git_provider: &'a GitClient,
    pub dependencies: Vec<GitDependency>,
    /// versions which are chosen if they satisfy the requirements, e.g. those of the lock
    /// file, so a resolve only changes what it must. Keyed by the normalized name.
    pub preferred: HashMap<String, SemanticVersion>,
    /// the `sclang` range of the project. Without one, the `sclang` ranges of the
    /// dependencies are ignored (DESIGN.md "SuperCollider version").
    pub sclang: Option<VersionReq>,
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
        if package == SCLANG_PACKAGE {
            return Ok(self
                .sclang
                .as_ref()
                .map(lowest_sclang_version)
                .filter(|version| range.contains(version)));
        }
        // a preferred version may also be yanked, as yanking must not break existing lock files
        if let Some(version) = self.preferred.get(package) {
            if range.contains(version) && self.git_provider.get_release(package, version)?.is_some()
            {
                return Ok(Some(*version));
            }
        }
        self.git_provider.choose_version(package, range)
    }

    fn get_dependencies(
        &self,
        package: &Self::P,
        version: &Self::V,
    ) -> Result<Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        if package == ROOT_PACKAGE {
            let mut dependencies = dependency_list_to_hash_map(&self.dependencies);
            if let Some(sclang) = &self.sclang {
                dependencies.insert(SCLANG_PACKAGE.to_string(), sclang.to_ranges());
            }
            return Ok(Dependencies::Available(dependencies));
        }
        if package == SCLANG_PACKAGE {
            return Ok(Dependencies::Available(DependencyConstraints::default()));
        }
        let dependencies = self.git_provider.get_dependencies(package, version)?;
        match (dependencies, &self.sclang) {
            (Dependencies::Available(mut dependencies), Some(_)) => {
                let release_sclang = self
                    .git_provider
                    .get_release(package, version)?
                    .and_then(|release| release.sclang);
                if let Some(requirement) = release_sclang {
                    dependencies
                        .insert(SCLANG_PACKAGE.to_string(), sclang_lower_bound(&requirement));
                }
                Ok(Dependencies::Available(dependencies))
            }
            (dependencies, _) => Ok(dependencies),
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
        resolve_fixture_preferred(dependencies, &[])
    }

    fn resolve_fixture_preferred(
        dependencies: &[(&str, &str)],
        preferred: &[(&str, &str)],
    ) -> anyhow::Result<Vec<(String, String)>> {
        resolve_fixture_with(dependencies, preferred, None)
    }

    fn resolve_fixture_with(
        dependencies: &[(&str, &str)],
        preferred: &[(&str, &str)],
        sclang: Option<&str>,
    ) -> anyhow::Result<Vec<(String, String)>> {
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
            preferred: preferred
                .iter()
                .map(|(name, version)| (name.to_string(), version.parse().unwrap()))
                .collect(),
            sclang: sclang.map(|sclang| sclang.parse().unwrap()),
        };
        let mut selected: Vec<(String, String)> = resolve_dependencies(&provider)?
            .into_iter()
            .filter(|(name, _)| name != ROOT_PACKAGE && name != SCLANG_PACKAGE)
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
    fn test_resolve_prefers_locked_versions() {
        // beta 1.0.0 is kept, although alpha 1.1.0 would also allow beta 1.2.0
        assert_eq!(
            resolve_fixture_preferred(&[("alpha", "^1.0.0")], &[("beta", "1.0.0")]).unwrap(),
            pairs(&[("alpha", "1.1.0"), ("beta", "1.0.0")])
        );
    }

    #[test]
    fn test_resolve_preferred_yanked_version() {
        // alpha 2.0.0 is yanked, but already locked
        let selected = resolve_fixture_preferred(&[("alpha", "*")], &[("alpha", "2.0.0")]).unwrap();
        assert!(selected.contains(&("alpha".to_string(), "2.0.0".to_string())));
    }

    #[test]
    fn test_resolve_ignores_unsuitable_preferred_versions() {
        // out of range, and missing in the index
        assert_eq!(
            resolve_fixture_preferred(
                &[("alpha", "^1.1.0"), ("ddw-common", "*")],
                &[("alpha", "1.0.0"), ("ddw-common", "0.9.0")]
            )
            .unwrap(),
            pairs(&[
                ("alpha", "1.1.0"),
                ("beta", "1.2.0"),
                ("ddw-common", "0.1.0")
            ])
        );
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

    // epsilon 1.0.0 needs sclang >=3.10, 1.1.0 >=3.13, <3.14 and 1.2.0 >=3.15

    #[test]
    fn test_resolve_falls_back_to_older_sclang_versions() {
        // 1.2.0 needs a newer sclang, 1.1.0's upper bound is ignored
        assert_eq!(
            resolve_fixture_with(&[("epsilon", "*")], &[], Some(">=3.14")).unwrap(),
            pairs(&[("epsilon", "1.1.0")])
        );
        assert_eq!(
            resolve_fixture_with(&[("epsilon", "*")], &[], Some(">=3.10, <3.13")).unwrap(),
            pairs(&[("epsilon", "1.0.0")])
        );
        assert_eq!(
            resolve_fixture_with(&[("epsilon", "*")], &[], Some(">=3.15")).unwrap(),
            pairs(&[("epsilon", "1.2.0")])
        );
    }

    #[test]
    fn test_resolve_without_sclang_range() {
        assert_eq!(
            resolve_fixture_with(&[("epsilon", "*")], &[], None).unwrap(),
            pairs(&[("epsilon", "1.2.0")])
        );
    }

    #[test]
    fn test_resolve_sclang_conflict() {
        let err = resolve_fixture_with(&[("epsilon", "^1.2")], &[], Some(">=3.14"))
            .unwrap_err()
            .to_string();
        println!("{err}");
        assert!(err.contains(SCLANG_PACKAGE), "{err}");
    }

    #[test]
    fn test_resolve_sclang_ignores_preferred_unsupported_version() {
        assert_eq!(
            resolve_fixture_with(&[("epsilon", "*")], &[("epsilon", "1.2.0")], Some(">=3.13"))
                .unwrap(),
            pairs(&[("epsilon", "1.1.0")])
        );
    }

    #[test]
    fn test_sclang_lower_bound() {
        for (requirement, lower) in [
            (">=3.13", "3.13.0"),
            (">=3.13, <3.14", "3.13.0"),
            ("^3.13", "3.13.0"),
            ("~3.13.2", "3.13.2"),
            (">3.13", "3.14.0"),
            ("=3.14.1", "3.14.1"),
        ] {
            let requirement: VersionReq = requirement.parse().unwrap();
            let lower: SemanticVersion = lower.parse().unwrap();
            assert_eq!(
                sclang_lower_bound(&requirement),
                Ranges::higher_than(lower),
                "{requirement}"
            );
            assert_eq!(lowest_sclang_version(&requirement), lower, "{requirement}");
        }
        assert_eq!(
            sclang_lower_bound(&"<3.15".parse().unwrap()),
            Ranges::full()
        );
        assert_eq!(
            lowest_sclang_version(&"<3.15".parse().unwrap()),
            SemanticVersion::zero()
        );
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
