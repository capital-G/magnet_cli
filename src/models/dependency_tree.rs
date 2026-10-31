//! `magnet tree`: the locked dependencies below the project, via the `dependencies` of each
//! locked package, so it needs neither the index nor the network

use std::collections::HashSet;

use crate::config::constants::MAGNET_LOCK_FILE;
use crate::models::lock_file::{LockDependency, LockFile, LockSource};

/// `project` with its `dependencies` (normalized names) and theirs below it. A package whose
/// dependencies were already shown is marked with `(*)`, which also ends cycles.
pub fn render(project: &str, dependencies: &[String], lock_file: &LockFile) -> String {
    let mut tree = Tree {
        lock_file,
        shown: HashSet::new(),
        repeated: false,
        lines: vec![project.to_string()],
    };
    tree.children(dependencies, "");

    if tree.repeated {
        tree.lines
            .push("(*) its dependencies are shown above".to_string());
    }
    let reachable = lock_file.reachable(dependencies);
    let unreachable: Vec<String> = lock_file
        .dependencies
        .iter()
        .filter(|(name, _)| !reachable.contains(*name))
        .map(|(name, dependency)| format!("  {name} {}", label(dependency)))
        .collect();
    if !unreachable.is_empty() {
        tree.lines.push(format!(
            "Locked, but not required by the project ({MAGNET_LOCK_FILE} is out of date, run `magnet install`):"
        ));
        tree.lines.extend(unreachable);
    }
    tree.lines.join("\n")
}

struct Tree<'a> {
    lock_file: &'a LockFile,
    /// packages whose dependencies were shown
    shown: HashSet<String>,
    repeated: bool,
    lines: Vec<String>,
}

impl Tree<'_> {
    fn children(&mut self, names: &[String], prefix: &str) {
        for (index, name) in names.iter().enumerate() {
            let (branch, indent) = match index + 1 == names.len() {
                true => ("└── ", "    "),
                false => ("├── ", "│   "),
            };
            let Some(dependency) = self.lock_file.dependencies.get(name) else {
                self.lines.push(format!(
                    "{prefix}{branch}{name} (not in {MAGNET_LOCK_FILE}, run `magnet install`)"
                ));
                continue;
            };
            let line = format!("{prefix}{branch}{name} {}", label(dependency));
            if dependency.dependencies.is_empty() {
                self.lines.push(line);
            } else if !self.shown.insert(name.clone()) {
                self.lines.push(format!("{line} (*)"));
                self.repeated = true;
            } else {
                self.lines.push(line);
                self.children(&dependency.dependencies, &format!("{prefix}{indent}"));
            }
        }
    }
}

/// e.g. `1.2.0`, `0.0.0 (git 4b825dc)` or `0.3.0 (../baz)`
fn label(dependency: &LockDependency) -> String {
    match &dependency.source {
        LockSource::Path(path) => format!("{} ({})", dependency.version, path.display()),
        _ => dependency.label(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use pubgrub::SemanticVersion;

    use super::*;
    use crate::git_client::source::GitReference;
    use crate::models::package::ArtifactKind;

    fn locked(version: &str, source: LockSource, dependencies: &[&str]) -> LockDependency {
        LockDependency {
            version: version.parse::<SemanticVersion>().unwrap(),
            kinds: vec![ArtifactKind::Quark],
            source,
            dependencies: dependencies.iter().map(|name| name.to_string()).collect(),
            checksums: BTreeMap::new(),
        }
    }

    fn lock_file(dependencies: Vec<(&str, LockDependency)>) -> LockFile {
        LockFile {
            lock_version: 1,
            index: "https://github.com/supercollider/magnet-index.git"
                .parse()
                .unwrap(),
            index_http: None,
            dependencies: dependencies
                .into_iter()
                .map(|(name, dependency)| (name.to_string(), dependency))
                .collect(),
        }
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn test_render() {
        let git = LockSource::Git {
            url: "https://github.com/jamshark70/ddwCommon.git"
                .parse()
                .unwrap(),
            reference: GitReference::DefaultBranch,
            sha: "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string(),
        };
        let lock_file = lock_file(vec![
            ("alpha", locked("1.1.0", LockSource::Index, &["beta"])),
            ("beta", locked("1.2.0", LockSource::Index, &["ddw-common"])),
            ("ddw-common", locked("0.0.0", git, &[])),
            ("gamma", locked("0.3.0", LockSource::Index, &["alpha"])),
            (
                "local",
                locked(
                    "0.3.0",
                    LockSource::Path(PathBuf::from("../local")),
                    &["beta"],
                ),
            ),
        ]);
        assert_eq!(
            render(
                "p 0.1.0",
                &names(&["gamma", "local", "missing"]),
                &lock_file
            ),
            "p 0.1.0\n\
             ├── gamma 0.3.0\n\
             │   └── alpha 1.1.0\n\
             │       └── beta 1.2.0\n\
             │           └── ddw-common 0.0.0 (git 4b825dc)\n\
             ├── local 0.3.0 (../local)\n\
             │   └── beta 1.2.0 (*)\n\
             └── missing (not in magnet.lock, run `magnet install`)\n\
             (*) its dependencies are shown above"
        );
    }

    #[test]
    fn test_render_cycle_and_unreachable() {
        let lock_file = lock_file(vec![
            ("alpha", locked("1.0.0", LockSource::Index, &["beta"])),
            ("beta", locked("1.0.0", LockSource::Index, &["alpha"])),
            ("old", locked("2.0.0", LockSource::Index, &[])),
        ]);
        assert_eq!(
            render("p 0.1.0", &names(&["alpha"]), &lock_file),
            concat!(
                "p 0.1.0\n",
                "└── alpha 1.0.0\n",
                "    └── beta 1.0.0\n",
                "        └── alpha 1.0.0 (*)\n",
                "(*) its dependencies are shown above\n",
                "Locked, but not required by the project (magnet.lock is out of date, run `magnet install`):\n",
                "  old 2.0.0",
            )
        );
    }
}
