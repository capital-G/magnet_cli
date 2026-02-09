use crate::models::project::Project;

#[derive(Debug, Clone)]
pub enum ProjectDirectory {
    /// sclang source code in .sc format
    Classes,
    /// Holds compiled package data of the project
    Dist,
    /// help files for project
    Docs,
    /// dependencies of the project.
    External,
    /// Test classes and test scd files.
    /// .scd files will be assumed to be unit tests for automated testing purposes
    Tests,
}

impl From<ProjectDirectory> for String {
    fn from(value: ProjectDirectory) -> Self {
        match value {
            ProjectDirectory::Classes => "classes",
            ProjectDirectory::Dist => "dist",
            ProjectDirectory::Docs => "docs",
            ProjectDirectory::Tests => "tests",
            ProjectDirectory::External => "external",
        }
        .to_string()
    }
}
