use std::path::PathBuf;

use anyhow::bail;
use reqwest::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use tracing::error;
use url::Url;

use crate::http_client::endpoints::{AuthNecessary, Endpoint, NoAuthNecessary};

/// OAuth token from the magnet server combined with a pkce
/// challenge b/c we do not trust the client and therefore can
/// not share the app secret with the client. See
/// https://auth0.com/docs/get-started/authentication-and-authorization-flow/authorization-code-flow-with-pkce
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OAuthToken {
    pub access_token: String,
    pub token_type: String,
    /// unix time at which the access token expires
    pub expires_in: u64,
    pub scope: String,
    pub refresh_token: String,
    pub pkce_verifier: String,
}

/// The token an authenticated client sends as `Authorization: Bearer`
#[derive(Debug, Clone, PartialEq)]
pub enum AuthToken {
    /// from `magnet login`, stored per API URL and refreshed when it expires
    Login { token: OAuthToken, path: PathBuf },
    /// from `MAGNET_TOKEN`, e.g. a long-lived CI token, which is never stored
    Env(String),
}

impl AuthToken {
    pub fn access_token(&self) -> &str {
        match self {
            AuthToken::Login { token, .. } => &token.access_token,
            AuthToken::Env(token) => token,
        }
    }
}

/// Generic struct indicating an Client in an non-authorized state
pub struct NoAuth;

/// Adds the credentials of a client state to a request
pub trait ApplyAuth {
    fn apply(&self, request: RequestBuilder) -> RequestBuilder;
}

impl ApplyAuth for NoAuth {
    fn apply(&self, request: RequestBuilder) -> RequestBuilder {
        request
    }
}

impl ApplyAuth for AuthToken {
    fn apply(&self, request: RequestBuilder) -> RequestBuilder {
        request.bearer_auth(self.access_token())
    }
}

pub struct MagnetClient<State> {
    // @todo how to make this non-public? this creates problem
    // when turning an NoAuth client into an AuthClient
    pub client: Client,
    pub base_url: Url,
    pub state: State,
}

impl<State> MagnetClient<State> {
    /// The same server, but with another state
    pub fn with_state<S>(&self, state: S) -> MagnetClient<S> {
        MagnetClient {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            state,
        }
    }
}

impl<State: ApplyAuth> MagnetClient<State> {
    // #[instrument(skip(self, endpoint), fields(endpoint = endpoint.path()))]
    pub(crate) async fn call_inner<E>(&self, endpoint: E) -> anyhow::Result<E::Response>
    where
        E: Endpoint + Send + Sync,
    {
        let url = self.base_url.join(&endpoint.path())?;
        let request = self
            .state
            .apply(self.client.request(endpoint.method(), url));

        match request.send().await {
            Ok(response) => {
                if !response.status().is_success() {
                    error!("Invalid request to server");
                    bail!("Invalid request")
                }

                match response.json::<E::Response>().await {
                    Ok(result) => Ok(result),
                    Err(err) => {
                        error!(?err, "Invalid response from server");
                        Err(err.into())
                    }
                }
            }
            Err(err) => {
                error!(?err, "Could not connect to server");
                Err(err.into())
            }
        }
    }
}

impl MagnetClient<NoAuth> {
    pub fn new(base_url: impl Into<Url>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into(),
            state: NoAuth,
        }
    }

    pub async fn call<E>(&self, endpoint: E) -> anyhow::Result<E::Response>
    where
        E: Endpoint<Auth = dyn NoAuthNecessary> + Send + Sync,
    {
        self.call_inner(endpoint).await
    }
}

impl MagnetClient<AuthToken> {
    pub async fn call<E>(&self, endpoint: E) -> anyhow::Result<E::Response>
    where
        E: Endpoint<Auth = dyn AuthNecessary> + Send + Sync,
    {
        self.call_inner(endpoint).await
    }
}
