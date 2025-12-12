#[derive(Debug, Clone)]
pub enum ProjectDirectory {
    /// Large non-code asset files. These will usually not be stored in git
    Assets,
    /// Example files in .scd format
    Examples,
    /// sclang source code in .sc format
    Classes,
    /// Holds compiled package data of the project
    Dist,
    /// help files for project
    HelpSource,
    /// All platform-specific code, e.g. server plugins or executables.
    /// This gets managed by the Magnet
    Platform,
    /// Test classes and test scd files.
    /// .scd files will be assumed to be unit tests for automated testing purposes
    Tests,
}

impl ProjectDirectory {
    pub const fn directory_name(&self) -> &'static str {
        match self {
            ProjectDirectory::Assets => "assets",
            ProjectDirectory::Classes => "classes",
            ProjectDirectory::Dist => "dist",
            ProjectDirectory::HelpSource => "help",
            ProjectDirectory::Platform => "platform",
            ProjectDirectory::Tests => "tests",
            ProjectDirectory::Examples => "examples",
        }
    }
}
