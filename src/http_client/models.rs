use serde::{Deserialize, Serialize};

use crate::git_client::models::GitArtifact;

#[derive(Debug, Serialize, Deserialize)]
pub struct MagnetServerVersion {
    pub version: String,
}

/// State of a version on the server, see DESIGN.md "Publishing a package"
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionState {
    /// only visible to the owners, not part of the index
    Draft,
    /// part of the index, artifacts can only be added
    Final,
}

/// A version of a package as returned by the publish endpoints
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublishedVersion {
    pub name: String,
    pub version: String,
    pub state: VersionState,
    #[serde(default)]
    pub artifacts: Vec<GitArtifact>,
}

/// Error body of the server
#[derive(Debug, Deserialize)]
pub struct ApiError {
    pub detail: String,
}
