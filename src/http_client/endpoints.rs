use serde::{de::DeserializeOwned, Serialize};

use crate::http_client::{
    basic::{AuthToken, NoAuth},
    models::MagnetServerVersion,
};

pub trait NoAuthNecessary {}
pub trait AuthNecessary: NoAuthNecessary {}

impl NoAuthNecessary for NoAuth {}
impl NoAuthNecessary for AuthToken {}
impl AuthNecessary for AuthToken {}

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

pub struct ServerInfo;

impl Endpoint for ServerInfo {
    type Response = MagnetServerVersion;
    type Body = ();

    type Auth = dyn NoAuthNecessary;

    fn path(&self) -> String {
        "/api/version/".to_string()
    }

    fn method(&self) -> reqwest::Method {
        reqwest::Method::GET
    }
}
