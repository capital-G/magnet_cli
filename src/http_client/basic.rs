use anyhow::bail;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::error;
use url::Url;

use crate::http_client::endpoints::{AuthNecessary, Endpoint, NoAuthNecessary};

/// OAuth token from the magnet server combined with a pkce
/// challenge b/c we do not trust the client and therefore can
/// not share the app secret with the client. See
/// https://auth0.com/docs/get-started/authentication-and-authorization-flow/authorization-code-flow-with-pkce
#[derive(Debug, Serialize, Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub scope: String,
    pub refresh_token: String,
    pub pkce_verifier: String,
}

/// Generic struct indicating an Client in an non-authorized state
pub struct NoAuth;

pub struct MagnetClient<State> {
    // @todo how to make this non-public? this creates problem
    // when turning an NoAuth client into an AuthClient
    pub client: Client,
    pub base_url: Url,
    pub state: State,
}

impl<State> MagnetClient<State> {
    // #[instrument(skip(self, endpoint), fields(endpoint = endpoint.path()))]
    async fn call_inner<E>(&self, endpoint: E) -> anyhow::Result<E::Response>
    where
        E: Endpoint + Send + Sync,
    {
        let url = self.base_url.join(&endpoint.path())?;

        match self.client.request(endpoint.method(), url).send().await {
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

impl MagnetClient<OAuthToken> {
    pub async fn call<E>(&self, endpoint: E) -> anyhow::Result<E::Response>
    where
        E: Endpoint<Auth = dyn AuthNecessary> + Send + Sync,
    {
        self.call_inner(endpoint).await
    }
}
