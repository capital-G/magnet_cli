//! Readable warnings for the server's class check (C7.2): naming conflicts with other packages
//! and `.sc` files the server couldn't parse. sclang has one flat namespace, so a class defined
//! by two packages doesn't compile, and of two extensions adding the same method one silently
//! replaces the other.

use crate::http_client::classes::{ClassCheck, ConflictingMethod, PackageConflict};

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
