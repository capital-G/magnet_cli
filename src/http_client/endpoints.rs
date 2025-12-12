use serde::{de::DeserializeOwned, Serialize};

use crate::http_client::{
    basic::{NoAuth, OAuthToken},
    models::{MagnetServerVersion, Project, ProjectDetail as ApiProjectDetail},
};

pub trait NoAuthNecessary {}
pub trait AuthNecessary: NoAuthNecessary {}

impl NoAuthNecessary for NoAuth {}
impl NoAuthNecessary for OAuthToken {}
impl AuthNecessary for OAuthToken {}

pub trait Endpoint {
    type Response: DeserializeOwned + Send + 'static;
    type Body: Serialize + Send + 'static;
    type Auth: ?Sized;

    fn path(&self) -> String;
    fn method(&self) -> reqwest::Method;
    fn body(&self) -> Option<&Self::Body> {
        None
    }
}

pub struct ProjectEndpoint;

impl Endpoint for ProjectEndpoint {
    type Response = Vec<Project>;
    type Body = ();
    type Auth = dyn NoAuthNecessary;

    fn path(&self) -> String {
        "/api/v1/projects".to_string()
    }

    fn method(&self) -> reqwest::Method {
        reqwest::Method::GET
    }
}

pub struct ProjectDetail {
    pub name: String,
}

impl Endpoint for ProjectDetail {
    type Response = ApiProjectDetail;

    type Body = ();

    type Auth = dyn NoAuthNecessary;

    fn path(&self) -> String {
        format!("/api/project/{}", self.name)
    }

    fn method(&self) -> reqwest::Method {
        reqwest::Method::GET
    }
}

pub struct ServerInfo;

impl Endpoint for ServerInfo {
    type Response = MagnetServerVersion;
    type Body = ();

    type Auth = dyn NoAuthNecessary;

    fn path(&self) -> String {
        "/api/version".to_string()
    }

    fn method(&self) -> reqwest::Method {
        reqwest::Method::GET
    }
}
