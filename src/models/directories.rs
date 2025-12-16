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

impl ProjectDirectory {
    pub const fn directory_name(&self) -> &'static str {
        match self {
            ProjectDirectory::Classes => "classes",
            ProjectDirectory::Dist => "dist",
            ProjectDirectory::Docs => "docs",
            ProjectDirectory::Tests => "tests",
            ProjectDirectory::External => "external",
        }
    }
}
