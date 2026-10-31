//! Explains why the dependencies can't be resolved (C6.12): the facts of pubgrub's derivation
//! tree, one per line from the project down the chain of dependencies, instead of pubgrub's
//! default report with its `root 0.0.0` and `0.1.0 | 0.1.1`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Bound;

use pubgrub::{DerivationTree, External, Ranges, SemanticVersion};

use crate::error::HintedError;
use crate::models::project_manifest::MagnetManifestV1;
use crate::resolver::online_provider::{
    lowest_sclang_version, RootProvider, ROOT_PACKAGE, SCLANG_PACKAGE,
};

type Tree = DerivationTree<String, Ranges<SemanticVersion>, String>;
type Fact = External<String, Ranges<SemanticVersion>, String>;

/// The facts of `tree` which together make resolving impossible, with hints where one is known
pub fn explain(tree: &Tree, provider: &RootProvider) -> HintedError {
    let mut facts = vec![];
    collect_facts(tree, &mut HashSet::new(), &mut facts);
    let depths = depths(&facts);
    // stable, so facts about the same package keep pubgrub's order
    facts.sort_by_key(|fact| depths.get(subject(fact)).copied().unwrap_or(usize::MAX));

    let mut lines: Vec<String> = vec![];
    for line in facts
        .iter()
        .filter(|fact| !is_gap(fact, &facts, provider))
        .filter_map(|fact| describe(fact, provider))
    {
        if !lines.contains(&line) {
            lines.push(line);
        }
    }
    HintedError {
        message: format!(
            "Could not resolve the dependencies:\n{}",
            lines
                .iter()
                .map(|line| format!("  {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        hints: hints(&facts),
    }
}

/// The external incompatibilities of `tree`, a shared subtree only once
fn collect_facts<'t>(tree: &'t Tree, shared: &mut HashSet<usize>, facts: &mut Vec<&'t Fact>) {
    match tree {
        DerivationTree::External(External::NotRoot(..)) => {}
        DerivationTree::External(fact) => facts.push(fact),
        DerivationTree::Derived(derived) => {
            if let Some(id) = derived.shared_id {
                if !shared.insert(id) {
                    return;
                }
            }
            collect_facts(&derived.cause1, shared, facts);
            collect_facts(&derived.cause2, shared, facts);
        }
    }
}

/// How many dependency steps each package of `facts` is away from the project
fn depths<'t>(facts: &[&'t Fact]) -> HashMap<&'t str, usize> {
    let mut depths = HashMap::from([(ROOT_PACKAGE, 0)]);
    let mut queue = VecDeque::from([ROOT_PACKAGE]);
    while let Some(package) = queue.pop_front() {
        let depth = depths[package];
        for fact in facts {
            if let External::FromDependencyOf(from, _, dependency, _) = fact {
                if from == package && !depths.contains_key(dependency.as_str()) {
                    depths.insert(dependency, depth + 1);
                    queue.push_back(dependency);
                }
            }
        }
    }
    depths
}

/// `true` for pubgrub's note that a package has no versions around one it explained by another
/// fact, e.g. "no version >1.1.0, <2.0.0" next to "alpha 1.1.0 depends on …", which
/// `collapse_no_versions` would drop. Unlike that, a missing package or a requirement no version
/// in the index meets is kept.
fn is_gap(fact: &Fact, facts: &[&Fact], provider: &RootProvider) -> bool {
    let External::NoVersions(package, range) = fact else {
        return false;
    };
    if package == SCLANG_PACKAGE || provider.sources.contains_key(package) {
        return false;
    }
    let releases = releases(package, provider);
    !releases.is_empty()
        && !releases.iter().any(|(version, _)| range.contains(version))
        && facts.iter().any(|other| {
            matches!(other, External::FromDependencyOf(..) | External::Custom(..))
                && subject(other) == package
        })
}

fn subject(fact: &Fact) -> &str {
    match fact {
        External::NotRoot(package, _)
        | External::NoVersions(package, _)
        | External::FromDependencyOf(package, _, _, _)
        | External::Custom(package, _, _) => package,
    }
}

fn describe(fact: &Fact, provider: &RootProvider) -> Option<String> {
    Some(match fact {
        External::NotRoot(..) => return None,
        External::FromDependencyOf(package, _, dependency, range) if package == ROOT_PACKAGE => {
            if dependency == SCLANG_PACKAGE {
                format!(
                    "The project supports SuperCollider {} (`sclang` in magnet.toml).",
                    provider.sclang.as_ref()?
                )
            } else {
                // the requirement as written in the manifest, e.g. `^0.1` instead of `>=0.1.0, <0.2.0`
                let requirement = provider
                    .dependencies
                    .iter()
                    .find(|d| MagnetManifestV1::normalize_name(&d.name) == *dependency)
                    .map(|d| d.constraint.to_string())
                    .unwrap_or_else(|| format_ranges(range));
                match requirement.as_str() {
                    "*" => format!("The project depends on {dependency}."),
                    _ => format!("The project depends on {dependency} {requirement}."),
                }
            }
        }
        External::FromDependencyOf(package, versions, dependency, range) => {
            let (versions, plural) = describe_versions(package, versions, provider);
            let verb = match (dependency == SCLANG_PACKAGE, plural) {
                (true, false) => "needs",
                (true, true) => "need",
                (false, false) => "depends on",
                (false, true) => "depend on",
            };
            match range == &Ranges::full() {
                true => format!("{versions} {verb} {dependency}."),
                false => format!("{versions} {verb} {dependency} {}.", format_ranges(range)),
            }
        }
        External::NoVersions(package, _) if package == SCLANG_PACKAGE => {
            // the virtual package's only version is the lowest one the project allows
            let sclang = provider.sclang.as_ref()?;
            format!(
                "The project supports SuperCollider {sclang} (`sclang` in magnet.toml), so its dependencies must support SuperCollider {}.",
                lowest_sclang_version(sclang)
            )
        }
        External::NoVersions(package, range) => describe_no_versions(package, range, provider),
        External::Custom(_, _, reason) => format!("{reason}."),
    })
}

/// e.g. `foo 1.0.0`, `foo 1.0.0 and 1.1.0` or `foo 1.0.0 to 1.4.2`, and whether that's
/// several versions
fn describe_versions(
    package: &str,
    range: &Ranges<SemanticVersion>,
    provider: &RootProvider,
) -> (String, bool) {
    if let Some(source) = provider.sources.get(package) {
        return (format!("{package} {}", source.version), false);
    }
    let preferred = provider.preferred.get(package);
    let versions: Vec<SemanticVersion> = releases(package, provider)
        .into_iter()
        // a yanked version only counts if it's locked, otherwise it isn't chosen
        .filter(|(version, yanked)| !yanked || Some(version) == preferred)
        .map(|(version, _)| version)
        .filter(|version| range.contains(version))
        .collect();
    match versions.len() {
        0 => (format!("{package} {}", format_ranges(range)), false),
        count => (
            format!("{package} {}", format_versions(&versions)),
            count > 1,
        ),
    }
}

fn describe_no_versions(
    package: &str,
    range: &Ranges<SemanticVersion>,
    provider: &RootProvider,
) -> String {
    if let Some(source) = provider.sources.get(package) {
        return format!(
            "{package} {} is required, but {package} from {} has version {}.",
            format_ranges(range),
            source.source,
            source.version
        );
    }
    let releases = releases(package, provider);
    if releases.is_empty() {
        return format!("{package} is not in the index.");
    }
    let available: Vec<SemanticVersion> = releases
        .iter()
        .filter(|(_, yanked)| !yanked)
        .map(|(version, _)| *version)
        .collect();
    if available.is_empty() {
        return format!("Every version of {package} is yanked.");
    }
    let yanked = match releases
        .iter()
        .any(|(version, yanked)| *yanked && range.contains(version))
    {
        true => " which isn't yanked",
        false => "",
    };
    format!(
        "{package} has no version {}{yanked} (available: {}).",
        format_ranges(range),
        format_versions(&available)
    )
}

/// The versions of `package` in the index and whether they're yanked, sorted. Empty if it
/// isn't in the index or the index can't be read, which the resolver already reported then.
fn releases(package: &str, provider: &RootProvider) -> Vec<(SemanticVersion, bool)> {
    let mut releases: Vec<(SemanticVersion, bool)> = provider
        .git_provider
        .get_releases(package)
        .ok()
        .flatten()
        .unwrap_or_default()
        .iter()
        .map(|release| (release.version, release.yanked))
        .collect();
    releases.sort();
    releases
}

/// `1.0.0`, `1.0.0 and 1.1.0`, `1.0.0, 1.1.0 and 1.2.0` or `1.0.0 to 1.4.2`
fn format_versions(versions: &[SemanticVersion]) -> String {
    match versions {
        [] => String::new(),
        [version] => version.to_string(),
        [first, .., last] if versions.len() > 3 => format!("{first} to {last}"),
        [init @ .., last] => format!(
            "{} and {last}",
            init.iter()
                .map(|version| version.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// pubgrub's notation, with `or` instead of `|` between several segments
fn format_ranges(range: &Ranges<SemanticVersion>) -> String {
    range.to_string().replace(" | ", " or ")
}

fn hints(facts: &[&Fact]) -> Vec<String> {
    let mut hints = vec![];
    let sclang_too_low = facts
        .iter()
        .any(|fact| matches!(fact, External::NoVersions(package, _) if package == SCLANG_PACKAGE));
    // the highest SuperCollider version a dependency in the conflict needs
    let needed = facts
        .iter()
        .filter_map(|fact| match fact {
            External::FromDependencyOf(package, _, dependency, range)
                if package != ROOT_PACKAGE && dependency == SCLANG_PACKAGE =>
            {
                match range.bounding_range() {
                    Some((Bound::Included(lower), _)) => Some(*lower),
                    Some((Bound::Excluded(lower), _)) => Some(lower.bump_patch()),
                    _ => None,
                }
            }
            _ => None,
        })
        .max();
    if let (true, Some(needed)) = (sclang_too_low, needed) {
        let (major, minor, patch) = needed.into();
        let version = match patch {
            0 => format!("{major}.{minor}"),
            _ => needed.to_string(),
        };
        hints.push(format!(
            "raise `sclang` in magnet.toml to \">={version}\" if SuperCollider support below version {version} is not required"
        ));
    }
    hints
}
