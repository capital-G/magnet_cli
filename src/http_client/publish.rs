//! The publish endpoints of the magnet server: draft → upload → finalise,
//! see DESIGN.md "Publishing a package" and the endpoint contract in implementation_plan.md

use reqwest::{RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use tracing::{debug, instrument};
use url::Url;

use crate::{
    config::constants::MAGNET_TOKEN_ENV,
    http_client::{
        basic::{AuthToken, MagnetClient},
        models::{ApiError, PublishedVersion},
    },
    models::package::ArtifactTags,
};

/// Header which carries the sha256 of an uploaded artifact, to guard the transfer
pub const SHA256_HEADER: &str = "X-Magnet-Sha256";

/// Result of uploading an artifact
#[derive(Debug, Clone, PartialEq)]
pub enum Upload {
    Created,
    /// the server already had this artifact with the same checksum
    AlreadyPresent,
}

impl MagnetClient<AuthToken> {
    fn versions_url(&self, name: &str) -> anyhow::Result<Url> {
        Ok(self
            .base_url
            .join(&format!("/api/v1/packages/{name}/versions/"))?)
    }

    fn version_url(&self, name: &str, version: &str) -> anyhow::Result<Url> {
        Ok(self.versions_url(name)?.join(&format!("{version}/"))?)
    }

    async fn send(&self, request: RequestBuilder) -> anyhow::Result<Response> {
        Ok(request
            .bearer_auth(self.state.access_token())
            .send()
            .await?)
    }

    /// Creates a draft of the version, or returns the existing draft or final version
    #[instrument(skip(self))]
    pub async fn create_draft(
        &self,
        name: &str,
        version: &str,
    ) -> anyhow::Result<PublishedVersion> {
        let response = self
            .send(
                self.client
                    .post(self.versions_url(name)?)
                    .json(&serde_json::json!({ "version": version })),
            )
            .await?;
        self.parse(response).await
    }

    /// Uploads an artifact with its sha256, which the server compares with its own
    #[instrument(skip(self, content))]
    pub async fn upload_artifact(
        &self,
        name: &str,
        version: &str,
        tags: &ArtifactTags,
        content: Vec<u8>,
        sha256: &str,
    ) -> anyhow::Result<Upload> {
        let url = self
            .version_url(name, version)?
            .join(&format!("artifacts/{}/{}/", tags.abi, tags.platform))?;
        // the whole body is sent with its Content-Length, which a WSGI server needs
        let response = self
            .send(
                self.client
                    .put(url)
                    .header(reqwest::header::CONTENT_TYPE, "application/gzip")
                    .header(SHA256_HEADER, sha256)
                    .body(content),
            )
            .await?;
        match response.status() {
            StatusCode::CREATED => Ok(Upload::Created),
            StatusCode::OK => Ok(Upload::AlreadyPresent),
            _ => Err(self.error(response).await),
        }
    }

    /// Finalises a draft, after which the version is part of the index
    #[instrument(skip(self))]
    pub async fn finalise(&self, name: &str, version: &str) -> anyhow::Result<PublishedVersion> {
        let url = self.version_url(name, version)?.join("finalise/")?;
        let response = self.send(self.client.post(url)).await?;
        self.parse(response).await
    }

    async fn parse<T: DeserializeOwned>(&self, response: Response) -> anyhow::Result<T> {
        if !response.status().is_success() {
            return Err(self.error(response).await);
        }
        Ok(response.json().await?)
    }

    /// The server's `detail` message, which explains why a request was refused
    async fn error(&self, response: Response) -> anyhow::Error {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        debug!(%status, text, "Request refused");
        // e.g. an HTML error page of a proxy says nothing beyond the status
        let detail = match serde_json::from_str::<ApiError>(&text) {
            Ok(error) => format!(": {}", error.detail),
            Err(_) if !text.is_empty() && !text.trim_start().starts_with('<') => {
                format!(": {}", text.chars().take(200).collect::<String>())
            }
            Err(_) => String::new(),
        };
        match status {
            StatusCode::UNAUTHORIZED => match self.state {
                AuthToken::Env(_) => {
                    anyhow::anyhow!("The server rejected {MAGNET_TOKEN_ENV}{detail}")
                }
                AuthToken::Login { .. } => {
                    anyhow::anyhow!(
                        "The server rejected the login, run `magnet login` again: {detail}"
                    )
                }
            },
            _ => anyhow::anyhow!("The server refused the request ({status}){detail}"),
        }
    }
}
