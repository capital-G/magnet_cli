use std::{
    fmt::Debug,
    fs::{self, File},
    io::{self, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Ok};
use base64::Engine;
use rand::{distr::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, error, instrument, trace};
use url::Url;

use crate::{
    config::constants::{magnet_home, OAUTH_APP_PUBLIC_KEY, OAUTH_TOKEN_FILE_NAME},
    http_client::basic::{MagnetClient, NoAuth, OAuthToken},
};

struct IntrospectResponse {
    active: bool,
}

/// Token auth reply from the magnet server
#[derive(Debug, Serialize, Deserialize)]
pub struct TokenResponse {
    access_token: String,
    expires_in: u64,
    token_type: String,
    scope: String,
    refresh_token: String,
}

impl OAuthToken {
    #[instrument(skip(response, pkce))]
    fn new(response: TokenResponse, pkce: PkceCodeChallenge) -> Self {
        OAuthToken {
            access_token: response.access_token,
            token_type: response.token_type,
            expires_in: Self::calculate_expires_in(response.expires_in),
            scope: response.scope,
            refresh_token: response.refresh_token,
            pkce_verifier: pkce.verifier,
        }
    }

    #[instrument(skip(self, response))]
    pub fn refresh_token(&mut self, response: TokenResponse) {
        self.access_token = response.access_token;
        self.token_type = response.token_type;
        self.scope = response.scope;
        self.refresh_token = response.refresh_token;
        self.expires_in = Self::calculate_expires_in(response.expires_in);
    }

    fn calculate_expires_in(expires_in: u64) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Could not obtain epoch time");

        now.as_secs() + expires_in
    }

    /// Gets default path of local token
    pub fn local_token_path() -> anyhow::Result<PathBuf> {
        Ok(magnet_home()?.join(OAUTH_TOKEN_FILE_NAME))
    }

    #[instrument]
    fn get_local_token(token_path: &Path) -> Option<Self> {
        match File::open(token_path) {
            std::result::Result::Ok(mut file) => {
                let mut buffer = String::new();
                let _ = file.read_to_string(&mut buffer);
                match serde_json::from_str::<OAuthToken>(&buffer) {
                    std::result::Result::Ok(token) => {
                        debug!("Use local token file");
                        Some(token)
                    }
                    Err(e) => {
                        error!(?e, "Failed to parse local access token");
                        None
                    }
                }
            }
            _ => {
                debug!("Did not find local token file");
                None
            }
        }
    }

    /// stores the token as json under the given path
    #[instrument(skip(self))]
    fn store_token(&self, token_path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = token_path.parent() {
            if !parent.exists() {
                trace!("Creating directory to store oauth token");
                fs::create_dir_all(parent)?;
            }
        }

        if token_path.exists() {
            debug!("Token already exists - will be overwritten");
        }

        let json = serde_json::to_string_pretty(&self)?;

        let mut file = fs::File::create(token_path)?;
        file.write_all(json.as_bytes())?;

        trace!("Stored OAuth token");

        Ok(())
    }

    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Could not obtain epoch time");
        now.as_secs() >= self.expires_in
    }

    /// Deletes token at given path.
    /// Returns true if successful, false if file could not been found.
    #[instrument]
    pub fn delete_token(token_path: &Path) -> anyhow::Result<bool> {
        match fs::remove_file(token_path) {
            std::result::Result::Ok(_) => {
                debug!("Deleted auth token");
                Ok(true)
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                debug!("Could not find an existing auth token file");
                Ok(false)
            }
            Err(err) => {
                error!(?err, "Failed to delete auth token file");
                Err(err.into())
            }
        }
    }
}

/// PKCE is needed as additional challenge b/c we don't want
/// to embedd the client secret into the CLI - see
/// https://auth0.com/docs/get-started/authentication-and-authorization-flow/authorization-code-flow-with-pkce
#[derive(Debug, Serialize, Deserialize)]
struct PkceCodeChallenge {
    pub challenge: String,
    pub verifier: String,
}

impl PkceCodeChallenge {
    pub fn new() -> Self {
        let verifier: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(64)
            .map(char::from)
            .collect();

        let digest = Sha256::digest(verifier.as_bytes());

        let challenge = base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(digest);

        Self {
            challenge,
            verifier,
        }
    }
}

impl<Unauth> MagnetClient<Unauth> {
    /// Authenticates the client by consuming it and returning and authenticated client
    /// or returns an err
    #[instrument(skip(self))]
    pub async fn authenticate(self, token_path: &Path) -> anyhow::Result<MagnetClient<OAuthToken>> {
        let token = match OAuthToken::get_local_token(token_path) {
            Some(token) => match token.is_expired() {
                true => {
                    bail!("Oauth token expired")
                }
                false => token,
            },
            None => {
                let token = self.obtain_new_token().await?;
                token.store_token(&OAuthToken::local_token_path()?)?;
                token
            }
        };

        Ok(MagnetClient {
            client: self.client,
            base_url: self.base_url,
            state: token,
        })
    }

    /// Requires manual intervention from the user by clicking on the URL
    #[instrument(skip(self))]
    async fn obtain_new_token(&self) -> anyhow::Result<OAuthToken> {
        let pkce = PkceCodeChallenge::new();

        let authorize_url = self.get_oauth_url(&pkce);
        println!("Go to: {}", authorize_url.as_str());

        let mut code = String::new();
        io::stdin().read_line(&mut code).unwrap();
        code = code.replace("\n", "");

        let token_response = self.obtain_token(&code, &pkce).await?;

        Ok(OAuthToken::new(token_response, pkce))
    }

    fn oauth_authorize_url(&self) -> Url {
        self.base_url.join("/o/authorize/").unwrap()
    }

    fn oauth_redirect_uri(&self) -> Url {
        self.base_url.join("/users/display-token").unwrap()
    }

    fn get_oauth_url(&self, pkce: &PkceCodeChallenge) -> Url {
        let mut authorize_url = self.oauth_authorize_url();
        authorize_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("client_id", OAUTH_APP_PUBLIC_KEY)
            .append_pair("redirect_uri", self.oauth_redirect_uri().as_str())
            .finish();

        authorize_url
    }

    fn oauth_token_url(&self) -> Url {
        self.base_url.join("/o/token/").unwrap()
    }

    /// Obtains the OAuthToken after user inserted the challenge
    /// from the website.
    async fn obtain_token(
        &self,
        code: &str,
        pkce: &PkceCodeChallenge,
    ) -> anyhow::Result<TokenResponse> {
        let token_url = self.oauth_token_url();
        let token_request = self
            .client
            .post(token_url.as_str())
            .form(&[
                ("client_id", OAUTH_APP_PUBLIC_KEY),
                ("code", code),
                ("code_verifier", &pkce.verifier),
                ("redirect_uri", self.oauth_redirect_uri().as_str()),
                ("grant_type", "authorization_code"),
            ])
            .send()
            .await?;

        let text = token_request
            .text()
            .await
            .context("Request to obtain OAuth token from server failed")?;
        let token_response: TokenResponse =
            serde_json::from_str(&text).context("Invalid OAuth token response from server")?;

        Ok(token_response)
    }
}

impl MagnetClient<OAuthToken> {
    /// See https://auth0.com/docs/secure/tokens/refresh-tokens/revoke-refresh-tokens
    pub async fn revoke_token(self) -> MagnetClient<NoAuth> {
        todo!("Not implemented yet!");
    }

    /// Trades an existing token for a new one, see
    /// https://www.oauth.com/oauth2-servers/making-authenticated-requests/refreshing-an-access-token/
    #[instrument(skip(self))]
    pub async fn refresh_token(&mut self) -> anyhow::Result<()> {
        let url = self.oauth_token_url();
        let request = self
            .client
            .post(url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &self.state.refresh_token),
                ("client_id", OAUTH_APP_PUBLIC_KEY),
            ])
            .send()
            .await;

        match request {
            reqwest::Result::Ok(response) => {
                let token_response_parsing =
                    serde_json::from_str::<TokenResponse>(&response.text().await?);

                match token_response_parsing {
                    std::result::Result::Ok(token_response) => {
                        trace!("Successfully refreshed token");
                        self.state.refresh_token(token_response);
                        self.state.store_token(&OAuthToken::local_token_path()?)?;
                        anyhow::Ok(())
                    }
                    Err(err) => {
                        error!(
                            ?err,
                            "Unexpected JSON response from server while parsing refresh token"
                        );
                        Err(err.into())
                    }
                }
            }
            Err(err) => {
                error!(?err, "Failed to refresh token");
                Err(err.into())
            }
        }
    }
}
