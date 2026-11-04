//! Readable warnings for the server's class check (C7.2): naming conflicts with other packages
//! and `.sc` files the server couldn't parse. sclang has one flat namespace, so a class defined
//! by two packages doesn't compile, and of two extensions adding the same method one silently
//! replaces the other.

use std::collections::BTreeMap;

use crate::http_client::classes::{CheckedClass, ClassCheck, ConflictingMethod, PackageConflict};

/// A method as SCDoc writes it: `Class:-name` (instance) or `Class:*name` (class method)
pub fn method_label(class_name: &str, name: &str, class_method: bool) -> String {
    let sign = if class_method { '*' } else { '-' };
    format!("{class_name}:{sign}{name}")
}

/// One line per other package and kind of clash, e.g. `beta-lib 1.0.0 also defines class Foo,
/// so sclang can't compile both packages together`
pub fn conflict_warnings(conflicts: &[PackageConflict]) -> Vec<String> {
    let mut warnings = vec![];
    for conflict in conflicts {
        let package = format!("{} {}", conflict.package, conflict.version);
        match conflict.classes.as_slice() {
            [] => {}
            [class] => warnings.push(format!(
                "{package} also defines class {class}, so sclang can't compile both packages together"
            )),
            classes => warnings.push(format!(
                "{package} also defines classes {}, so sclang can't compile both packages together",
                classes.join(", ")
            )),
        }
        if !conflict.methods.is_empty() {
            let methods: Vec<String> = conflict
                .methods
                .iter()
                .map(
                    |ConflictingMethod {
                         class_name,
                         name,
                         class_method,
                     }| method_label(class_name, name, *class_method),
                )
                .collect();
            warnings.push(format!(
                "{package} also adds {}, so with both packages installed one replaces the other",
                methods.join(", ")
            ));
        }
    }
    warnings
}

/// `n class`/`n classes`
fn count(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

/// The classes of a check per file, sorted by path: `Foo.sc: 2 classes, 1 extension`. With
/// `list`, every class follows on its own line (`Foo : Object (line 3): *new, -play`,
/// `+ String (line 20): -shout`), an extension's methods written like SCDoc.
pub fn class_summary(classes: &[CheckedClass], list: bool) -> Vec<String> {
    let mut files: BTreeMap<&str, Vec<&CheckedClass>> = BTreeMap::new();
    for class in classes {
        files.entry(&class.path).or_default().push(class);
    }
    let mut lines = vec![];
    for (path, classes) in files {
        let extensions = classes.iter().filter(|class| class.extension).count();
        let mut counts = vec![];
        if classes.len() > extensions {
            counts.push(count(classes.len() - extensions, "class", "classes"));
        }
        if extensions > 0 {
            counts.push(count(extensions, "extension", "extensions"));
        }
        lines.push(format!("{path}: {}", counts.join(", ")));
        if !list {
            continue;
        }
        for class in classes {
            let mut line = match (class.extension, class.superclass.as_str()) {
                (true, _) => format!("  + {}", class.name),
                (false, "") => format!("  {}", class.name),
                (false, superclass) => format!("  {} : {superclass}", class.name),
            };
            line.push_str(&format!(" (line {})", class.line));
            if !class.methods.is_empty() {
                let methods: Vec<String> = class
                    .methods
                    .iter()
                    .map(|method| {
                        format!(
                            "{}{}",
                            if method.class_method { '*' } else { '-' },
                            method.name
                        )
                    })
                    .collect();
                line.push_str(&format!(": {}", methods.join(", ")));
            }
            lines.push(line);
        }
    }
    lines
}

/// The conflicts of a check, then its files the server couldn't parse
pub fn check_warnings(check: &ClassCheck) -> Vec<String> {
    let mut warnings = conflict_warnings(&check.conflicts);
    for error in &check.errors {
        warnings.push(format!(
            "The server could not parse {} (its classes aren't checked): {}",
            error.path, error.message
        ));
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_client::classes::FileError;

    fn conflict(
        package: &str,
        classes: &[&str],
        methods: &[(&str, &str, bool)],
    ) -> PackageConflict {
        PackageConflict {
            package: package.to_string(),
            version: "1.0.0".to_string(),
            classes: classes.iter().map(|c| c.to_string()).collect(),
            methods: methods
                .iter()
                .map(|(class_name, name, class_method)| ConflictingMethod {
                    class_name: class_name.to_string(),
                    name: name.to_string(),
                    class_method: *class_method,
                })
                .collect(),
        }
    }

    fn class(
        name: &str,
        superclass: &str,
        extension: bool,
        path: &str,
        methods: &[(&str, bool)],
    ) -> CheckedClass {
        CheckedClass {
            name: name.to_string(),
            superclass: superclass.to_string(),
            extension,
            path: path.to_string(),
            line: 3,
            platform: String::new(),
            methods: methods
                .iter()
                .map(
                    |(name, class_method)| crate::http_client::classes::CheckedMethod {
                        name: name.to_string(),
                        class_method: *class_method,
                    },
                )
                .collect(),
        }
    }

    #[test]
    fn test_class_summary() {
        let classes = [
            class(
                "Foo",
                "Object",
                false,
                "Foo.sc",
                &[("new", true), ("play", false)],
            ),
            class("String", "", true, "Foo.sc", &[("shout", false)]),
            class("Bar", "", false, "Bar.sc", &[]),
            class("Baz", "Bar", false, "Bar.sc", &[]),
        ];
        assert_eq!(
            class_summary(&classes, false),
            ["Bar.sc: 2 classes", "Foo.sc: 1 class, 1 extension"]
        );
        assert_eq!(
            class_summary(&classes, true),
            [
                "Bar.sc: 2 classes",
                "  Bar (line 3)",
                "  Baz : Bar (line 3)",
                "Foo.sc: 1 class, 1 extension",
                "  Foo : Object (line 3): *new, -play",
                "  + String (line 3): -shout",
            ]
        );
    }

    #[test]
    fn test_check_warnings() {
        let check = ClassCheck {
            classes: vec![],
            errors: vec![FileError {
                path: "Broken.sc".to_string(),
                message: "unexpected '}'".to_string(),
            }],
            conflicts: vec![
                conflict("beta-lib", &["Foo"], &[]),
                conflict(
                    "gamma",
                    &["Bar", "Baz"],
                    &[("String", "shout", false), ("Object", "make", true)],
                ),
            ],
        };
        assert_eq!(
            check_warnings(&check),
            [
                "beta-lib 1.0.0 also defines class Foo, so sclang can't compile both packages together",
                "gamma 1.0.0 also defines classes Bar, Baz, so sclang can't compile both packages together",
                "gamma 1.0.0 also adds String:-shout, Object:*make, so with both packages installed one replaces the other",
                "The server could not parse Broken.sc (its classes aren't checked): unexpected '}'",
            ]
        );
    }
}
