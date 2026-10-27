use std::{
    fs, io,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context};
use base64::Engine;
use rand::{distr::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tracing::{debug, instrument, trace, warn};
use url::Url;

use crate::{
    config::constants::{CREDENTIALS_DIR, MAGNET_TOKEN_ENV, OAUTH_APP_PUBLIC_KEY},
    git_client::client::GitClient,
    http_client::basic::{AuthToken, MagnetClient, NoAuth, OAuthToken},
};

/// Token auth reply from the magnet server
#[derive(Debug, Serialize, Deserialize)]
pub struct TokenResponse {
    access_token: String,
    expires_in: u64,
    token_type: String,
    scope: String,
    refresh_token: String,
}

/// Where the login for an API URL is stored within `home`, so logins to e.g. a production and a
/// testing server don't overwrite each other
pub fn credentials_path(home: &Path, api_url: &Url) -> PathBuf {
    home.join(CREDENTIALS_DIR)
        .join(format!("{}.json", GitClient::cache_dir_name(api_url)))
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

    fn update(&mut self, response: TokenResponse) {
        self.access_token = response.access_token;
        self.token_type = response.token_type;
        self.scope = response.scope;
        self.refresh_token = response.refresh_token;
        self.expires_in = Self::calculate_expires_in(response.expires_in);
    }

    fn calculate_expires_in(expires_in: u64) -> u64 {
        now() + expires_in
    }

    /// The stored login, `None` if there is none or it can't be read
    #[instrument]
    pub fn load(path: &Path) -> Option<Self> {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                debug!("Did not find a stored login");
                return None;
            }
            Err(err) => {
                warn!("Could not read the stored login {path:?}: {err}");
                return None;
            }
        };
        match serde_json::from_str(&content) {
            Ok(token) => Some(token),
            Err(err) => {
                warn!("Could not parse the stored login {path:?}: {err}");
                None
            }
        }
    }

    /// Stores the token as JSON, readable only by the user
    #[instrument(skip(self))]
    pub fn store(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Could not create directory {parent:?}"))?;
        }
        write_private(path, serde_json::to_string_pretty(&self)?.as_bytes())
            .with_context(|| format!("Could not store the login in {path:?}"))?;
        trace!("Stored OAuth token");
        Ok(())
    }

    pub fn is_expired(&self) -> bool {
        now() >= self.expires_in
    }

    /// Deletes the stored login. Returns `false` if there was none.
    #[instrument]
    pub fn delete(path: &Path) -> anyhow::Result<bool> {
        match fs::remove_file(path) {
            Ok(_) => {
                debug!("Deleted auth token");
                Ok(true)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err).with_context(|| format!("Could not delete {path:?}")),
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Could not obtain epoch time")
        .as_secs()
}

#[cfg(unix)]
fn write_private(path: &Path, content: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // the mode only applies to new files
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(content)
}

#[cfg(not(unix))]
fn write_private(path: &Path, content: &[u8]) -> io::Result<()> {
    fs::write(path, content)
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
        let verifier = random_string(64);

        let digest = Sha256::digest(verifier.as_bytes());

        let challenge = base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(digest);

        Self {
            challenge,
            verifier,
        }
    }
}

impl MagnetClient<NoAuth> {
    /// An authenticated client: `env_token` (from `MAGNET_TOKEN`) takes precedence over the
    /// login stored at `path`, which gets refreshed if it is expired
    #[instrument(skip(self, env_token))]
    pub async fn authenticate(
        &self,
        env_token: Option<String>,
        path: &Path,
    ) -> anyhow::Result<MagnetClient<AuthToken>> {
        if let Some(token) = env_token {
            debug!("Use {MAGNET_TOKEN_ENV}");
            return Ok(self.with_state(AuthToken::Env(token)));
        }
        let token = OAuthToken::load(path).with_context(|| {
            format!(
                "Not logged in to {}, run `magnet login` or set {MAGNET_TOKEN_ENV}",
                self.base_url
            )
        })?;
        let expired = token.is_expired();
        let mut client = self.with_state(AuthToken::Login {
            token,
            path: path.to_path_buf(),
        });
        if expired {
            client.refresh_token().await.with_context(|| {
                format!(
                    "The login to {} expired, run `magnet login` again",
                    self.base_url
                )
            })?;
        }
        Ok(client)
    }

    /// Logs in via the browser and stores the token at `path`: the browser gets redirected
    /// to a listener of this process on a random local port (RFC 8252), so nothing has to
    /// be copied. The URL is also printed, in case no browser opens.
    #[instrument(skip(self))]
    pub async fn login(&self, path: &Path) -> anyhow::Result<MagnetClient<AuthToken>> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("Could not listen on a local port for the login")?;
        let redirect_uri: Url = format!(
            "http://127.0.0.1:{}{LOOPBACK_CALLBACK_PATH}",
            listener.local_addr()?.port()
        )
        .parse()?;
        let pkce = PkceCodeChallenge::new();
        let state = random_string(32);

        let authorize_url = self.get_oauth_url(&pkce, &redirect_uri, Some(&state));
        println!(
            "Log in to {} in the browser, it opens by itself. Otherwise open:",
            self.base_url
        );
        println!("{authorize_url}");
        open_browser(&authorize_url);

        let code = tokio::time::timeout(LOGIN_TIMEOUT, receive_code(&listener, &state))
            .await
            .context("The login timed out, run `magnet login` again")??;
        self.finish_login(&code, pkce, &redirect_uri, path).await
    }

    /// Logs in by pasting the code which the server shows after the login, for machines
    /// without a browser (`magnet login --no-browser`)
    #[instrument(skip(self))]
    pub async fn login_pasting_code(&self, path: &Path) -> anyhow::Result<MagnetClient<AuthToken>> {
        let pkce = PkceCodeChallenge::new();
        let redirect_uri = self.display_token_uri();

        let authorize_url = self.get_oauth_url(&pkce, &redirect_uri, None);
        println!("Open this URL, log in and paste the code shown afterwards:");
        println!("{authorize_url}");

        let mut code = String::new();
        io::stdin()
            .read_line(&mut code)
            .context("Could not read the code")?;

        let code = code.trim();
        if code.is_empty() {
            bail!("No code entered, the login was cancelled");
        }
        self.finish_login(code, pkce, &redirect_uri, path).await
    }

    /// Exchanges the code for a token and stores it at `path`
    async fn finish_login(
        &self,
        code: &str,
        pkce: PkceCodeChallenge,
        redirect_uri: &Url,
        path: &Path,
    ) -> anyhow::Result<MagnetClient<AuthToken>> {
        let token_response = self.obtain_token(code, &pkce, redirect_uri).await?;
        let token = OAuthToken::new(token_response, pkce);
        token.store(path)?;
        Ok(self.with_state(AuthToken::Login {
            token,
            path: path.to_path_buf(),
        }))
    }

    fn oauth_authorize_url(&self) -> Url {
        self.base_url.join("/o/authorize/").unwrap()
    }

    /// The server's page which shows the code to paste
    fn display_token_uri(&self) -> Url {
        self.base_url.join("/users/display-token").unwrap()
    }

    fn get_oauth_url(
        &self,
        pkce: &PkceCodeChallenge,
        redirect_uri: &Url,
        state: Option<&str>,
    ) -> Url {
        let mut authorize_url = self.oauth_authorize_url();
        {
            let mut query = authorize_url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("code_challenge", &pkce.challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("client_id", OAUTH_APP_PUBLIC_KEY)
                .append_pair("redirect_uri", redirect_uri.as_str());
            if let Some(state) = state {
                query.append_pair("state", state);
            }
        }
        authorize_url
    }

    /// Obtains the OAuthToken for the code of the login, with the same `redirect_uri`
    async fn obtain_token(
        &self,
        code: &str,
        pkce: &PkceCodeChallenge,
        redirect_uri: &Url,
    ) -> anyhow::Result<TokenResponse> {
        let response = self
            .client
            .post(oauth_token_url(&self.base_url))
            .form(&[
                ("client_id", OAUTH_APP_PUBLIC_KEY),
                ("code", code),
                ("code_verifier", &pkce.verifier),
                ("redirect_uri", redirect_uri.as_str()),
                ("grant_type", "authorization_code"),
            ])
            .send()
            .await
            .context("Request to obtain OAuth token from server failed")?;
        parse_token_response(response).await
    }
}

/// The path of the loopback redirect URI, registered at the server as
/// `http://127.0.0.1/callback` (any port, RFC 8252)
const LOOPBACK_CALLBACK_PATH: &str = "/callback";

/// How long `magnet login` waits for the browser
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

fn random_string(length: usize) -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

/// Opens `url` in the default browser; failing silently, since the URL is printed anyway
fn open_browser(url: &Url) {
    let mut command = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut command = std::process::Command::new("cmd");
        // `start` takes the first quoted argument as the window title
        command.args(["/C", "start", ""]);
        command
    } else {
        std::process::Command::new("xdg-open")
    };
    command
        .arg(url.as_str())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Err(err) = command.spawn() {
        debug!("Could not open the browser: {err}");
    }
}

/// Waits for the browser's redirect to the login's callback and returns its code. Other
/// requests (e.g. `/favicon.ico`) get a 404; a wrong `state` or an `error` from the server
/// ends the login.
async fn receive_code(listener: &TcpListener, state: &str) -> anyhow::Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let Some(target) = read_request_target(&mut stream).await? else {
            continue;
        };
        let url = Url::parse("http://127.0.0.1")?.join(&target)?;
        if url.path() != LOOPBACK_CALLBACK_PATH {
            respond(&mut stream, "404 Not Found", "Not found").await;
            continue;
        }
        let query: std::collections::HashMap<String, String> =
            url.query_pairs().into_owned().collect();
        if query.get("state").map(String::as_str) != Some(state) {
            respond(
                &mut stream,
                "400 Bad Request",
                "This login doesn't belong to the running `magnet login`.",
            )
            .await;
            bail!("The browser returned a login which wasn't started by this `magnet login`");
        }
        if let Some(error) = query.get("error") {
            respond(
                &mut stream,
                "200 OK",
                "The login was cancelled, you can close this tab.",
            )
            .await;
            bail!("The server refused the login: {error}");
        }
        let Some(code) = query.get("code") else {
            respond(&mut stream, "400 Bad Request", "The redirect has no code.").await;
            bail!("The server redirected without a code");
        };
        respond(
            &mut stream,
            "200 OK",
            "Logged in to magnet, you can close this tab.",
        )
        .await;
        return Ok(code.clone());
    }
}

/// The target of an HTTP request (`/callback?code=…`), `None` if it isn't a GET request
async fn read_request_target(stream: &mut TcpStream) -> anyhow::Result<Option<String>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    // the request line is all that's needed, a browser sends the headers right after it
    while !buffer.windows(2).any(|pair| pair == b"\r\n") && buffer.len() < 16 * 1024 {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let request = String::from_utf8_lossy(&buffer);
    let mut parts = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    Ok(match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) => Some(target.to_string()),
        _ => None,
    })
}

async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>magnet</title></head>\
         <body style=\"font-family: sans-serif; margin: 3em\"><p>{message}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if let Err(err) = stream.write_all(response.as_bytes()).await {
        debug!("Could not answer the browser: {err}");
    }
    let _ = stream.shutdown().await;
}

fn oauth_token_url(base_url: &Url) -> Url {
    base_url.join("/o/token/").unwrap()
}

async fn parse_token_response(response: reqwest::Response) -> anyhow::Result<TokenResponse> {
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        bail!("The server refused the token request ({status}): {text}");
    }
    serde_json::from_str(&text).context("Invalid OAuth token response from server")
}

impl MagnetClient<AuthToken> {
    /// Trades the refresh token for a new access token and stores it, see
    /// https://www.oauth.com/oauth2-servers/making-authenticated-requests/refreshing-an-access-token/
    #[instrument(skip(self))]
    pub async fn refresh_token(&mut self) -> anyhow::Result<()> {
        let AuthToken::Login { token, path } = &mut self.state else {
            bail!("The server rejected {MAGNET_TOKEN_ENV}, it may be expired or revoked");
        };
        let response = self
            .client
            .post(oauth_token_url(&self.base_url))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &token.refresh_token),
                ("client_id", OAUTH_APP_PUBLIC_KEY),
            ])
            .send()
            .await
            .context("Could not refresh the login")?;
        token.update(parse_token_response(response).await?);
        token.store(path)?;
        trace!("Refreshed token");
        Ok(())
    }

    /// Revokes the login on the server and deletes the stored token, see
    /// https://django-oauth-toolkit.readthedocs.io/en/latest/tutorial/tutorial_04.html.
    /// Revoking the refresh token also revokes its access token.
    #[instrument(skip(self))]
    pub async fn logout(self) -> anyhow::Result<MagnetClient<NoAuth>> {
        let AuthToken::Login { token, path } = &self.state else {
            bail!("Logged in via {MAGNET_TOKEN_ENV}, unset it to log out");
        };
        let revoked = self
            .client
            .post(self.base_url.join("/o/revoke_token/")?)
            .form(&[
                ("token", token.refresh_token.as_str()),
                ("token_type_hint", "refresh_token"),
                ("client_id", OAUTH_APP_PUBLIC_KEY),
            ])
            .send()
            .await
            .and_then(|response| response.error_for_status());
        // the local login goes away in any case, an unrevoked token expires on its own
        OAuthToken::delete(path)?;
        if let Err(err) = revoked {
            warn!("Could not revoke the login on the server: {err}");
        }
        Ok(self.with_state(NoAuth))
    }
}

#[cfg(test)]
mod tests {
    use wiremock::{
        matchers::{body_string_contains, header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::http_client::endpoints::ServerInfo;

    fn stored_token(expires_in: u64) -> OAuthToken {
        OAuthToken {
            access_token: "stored-access".to_string(),
            token_type: "Bearer".to_string(),
            expires_in,
            scope: "read write".to_string(),
            refresh_token: "stored-refresh".to_string(),
            pkce_verifier: "verifier".to_string(),
        }
    }

    /// Starts `receive_code` on a local port, returns its URL and the running task
    async fn callback_listener(
        state: &'static str,
    ) -> (Url, tokio::task::JoinHandle<anyhow::Result<String>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port())
            .parse()
            .unwrap();
        let task = tokio::spawn(async move { receive_code(&listener, state).await });
        (url, task)
    }

    #[tokio::test]
    async fn test_receive_code() {
        let (url, task) = callback_listener("s1").await;
        // e.g. the browser asking for a favicon first
        let response = reqwest::get(url.join("/favicon.ico").unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        let response = reqwest::get(url.join("/callback?code=abc&state=s1").unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.text().await.unwrap().contains("Logged in"));
        assert_eq!(task.await.unwrap().unwrap(), "abc");
    }

    #[tokio::test]
    async fn test_receive_code_refuses() {
        // a redirect of another login
        let (url, task) = callback_listener("s1").await;
        let response = reqwest::get(url.join("/callback?code=abc&state=other").unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        let err = task.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("wasn't started by this"), "{err}");

        // the server's error, e.g. a denied login
        let (url, task) = callback_listener("s1").await;
        reqwest::get(url.join("/callback?error=access_denied&state=s1").unwrap())
            .await
            .unwrap();
        let err = task.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("access_denied"), "{err}");
    }

    #[test]
    fn test_oauth_url() {
        let client =
            MagnetClient::<NoAuth>::new("https://magnet.example.org/".parse::<Url>().unwrap());
        let pkce = PkceCodeChallenge::new();
        let redirect: Url = "http://127.0.0.1:53123/callback".parse().unwrap();
        let url = client.get_oauth_url(&pkce, &redirect, Some("s1"));
        let query: std::collections::HashMap<String, String> =
            url.query_pairs().into_owned().collect();
        assert_eq!(url.path(), "/o/authorize/");
        assert_eq!(query["redirect_uri"], redirect.as_str());
        assert_eq!(query["state"], "s1");
        assert_eq!(query["code_challenge"], pkce.challenge);
        // the paste flow has no state
        let url = client.get_oauth_url(&pkce, &client.display_token_uri(), None);
        assert!(!url.query().unwrap().contains("state="));
    }

    #[tokio::test]
    async fn test_token_request_uses_the_redirect_uri() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/o/token/"))
            .and(body_string_contains(
                "redirect_uri=http%3A%2F%2F127.0.0.1%3A53123%2Fcallback",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "a", "expires_in": 3600, "token_type": "Bearer",
                "scope": "read publish", "refresh_token": "r"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = MagnetClient::<NoAuth>::new(server.uri().parse::<Url>().unwrap());
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("login.json");
        client
            .finish_login(
                "code",
                PkceCodeChallenge::new(),
                &"http://127.0.0.1:53123/callback".parse().unwrap(),
                &credentials,
            )
            .await
            .unwrap();
        assert_eq!(OAuthToken::load(&credentials).unwrap().access_token, "a");
    }

    #[test]
    fn test_credentials_path_per_api_url() {
        let home = Path::new("/home/magnet");
        let production = credentials_path(home, &"https://magnet.example/".parse().unwrap());
        let testing = credentials_path(home, &"https://testing.magnet.example/".parse().unwrap());
        assert_ne!(production, testing);
        assert!(production.starts_with(home.join(CREDENTIALS_DIR)));
        assert!(production
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("magnet.example-"));
        assert_eq!(
            production,
            credentials_path(home, &"https://magnet.example/".parse().unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_store_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CREDENTIALS_DIR).join("token.json");
        let token = stored_token(now() + 60);
        token.store(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(OAuthToken::load(&path), Some(token));
    }

    #[tokio::test]
    async fn test_env_token_takes_precedence() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/version/"))
            .and(header("authorization", "Bearer ci-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"version": "0.1.0"}"#))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        stored_token(now() + 60).store(&path).unwrap();

        let client = MagnetClient::new(server.uri().parse::<Url>().unwrap())
            .authenticate(Some("ci-token".to_string()), &path)
            .await
            .unwrap();
        assert_eq!(client.state, AuthToken::Env("ci-token".to_string()));
        client.call_inner(ServerInfo).await.unwrap();
    }

    #[tokio::test]
    async fn test_stored_login_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        let client = MagnetClient::new("http://127.0.0.1:9/".parse::<Url>().unwrap());

        let err = client.authenticate(None, &path).await.err().unwrap();
        assert!(err.to_string().contains("magnet login"), "{err}");

        stored_token(now() + 60).store(&path).unwrap();
        let authenticated = client.authenticate(None, &path).await.unwrap();
        assert_eq!(authenticated.state.access_token(), "stored-access");
    }

    #[tokio::test]
    async fn test_expired_login_gets_refreshed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/o/token/"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=stored-refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "new-access",
                "expires_in": 7200,
                "token_type": "Bearer",
                "scope": "read write",
                "refresh_token": "new-refresh",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        stored_token(now() - 1).store(&path).unwrap();

        let client = MagnetClient::new(server.uri().parse::<Url>().unwrap())
            .authenticate(None, &path)
            .await
            .unwrap();
        assert_eq!(client.state.access_token(), "new-access");

        let stored = OAuthToken::load(&path).unwrap();
        assert_eq!(stored.refresh_token, "new-refresh");
        assert!(!stored.is_expired());
    }

    #[tokio::test]
    async fn test_failed_refresh_asks_for_login() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/o/token/"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string(r#"{"error": "invalid_grant"}"#),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        stored_token(now() - 1).store(&path).unwrap();

        let err = MagnetClient::new(server.uri().parse::<Url>().unwrap())
            .authenticate(None, &path)
            .await
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("magnet login"), "{err:#}");
        assert!(format!("{err:#}").contains("invalid_grant"), "{err:#}");
    }

    #[tokio::test]
    async fn test_logout_revokes_and_deletes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/o/revoke_token/"))
            .and(body_string_contains("token=stored-refresh"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        stored_token(now() + 60).store(&path).unwrap();

        let client = MagnetClient::new(server.uri().parse::<Url>().unwrap());
        client
            .authenticate(None, &path)
            .await
            .unwrap()
            .logout()
            .await
            .unwrap();
        assert!(!path.exists());

        let env = client
            .authenticate(Some("ci-token".to_string()), &path)
            .await
            .unwrap();
        assert!(env.logout().await.is_err());
    }
}
