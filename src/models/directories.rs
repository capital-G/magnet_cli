#[derive(Debug, Clone)]
pub enum ProjectDirectory {
    /// sclang source code in .sc format
    Classes,
    /// Holds compiled package data of the project
    Dist,
    /// help files of the project, SCDoc only finds them in a folder with this name
    HelpSource,
    /// dependencies of the project, installed by magnet. Named after magnet, so it doesn't
    /// clash with e.g. an `external/` folder holding the C++ dependencies of a lepton.
    Packages,
    /// Test classes and test scd files.
    /// .scd files will be assumed to be unit tests for automated testing purposes
    Tests,
}

impl From<ProjectDirectory> for String {
    fn from(value: ProjectDirectory) -> Self {
        match value {
            ProjectDirectory::Classes => "classes",
            ProjectDirectory::Dist => "dist",
            ProjectDirectory::HelpSource => "HelpSource",
            ProjectDirectory::Tests => "tests",
            ProjectDirectory::Packages => "magnet_packages",
        }
        .to_string()
    }
}
