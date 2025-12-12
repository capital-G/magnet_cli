use tracing::trace;
use url::Url;

use crate::http_client::basic::{MagnetClient, OAuthToken};

pub trait TokenRefresh {
    fn with_auto_refresh<T, F, Fut>(
        &mut self,
        request_fn: F,
    ) -> impl std::future::Future<Output = anyhow::Result<T>>
    where
        F: Fn(&reqwest::Client, &Url, &str) -> Fut + Send,
        Fut: std::future::Future<Output = anyhow::Result<T>> + Send,
        T: Send;
}

pub fn is_auth_error(err: &anyhow::Error) -> bool {
    if let Some(http_err) = err.downcast_ref::<reqwest::Error>() {
        if let Some(status) = http_err.status() {
            return status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN;
        }
    }
    false
}

impl TokenRefresh for MagnetClient<OAuthToken> {
    async fn with_auto_refresh<T, F, Fut>(&mut self, request_fn: F) -> anyhow::Result<T>
    where
        F: Fn(&reqwest::Client, &Url, &str) -> Fut + Send,
        Fut: std::future::Future<Output = anyhow::Result<T>> + Send,
        T: Send,
    {
        let token = &self.state.access_token;

        match request_fn(&self.client, &self.base_url, token).await {
            Ok(res) => Ok(res),
            Err(err) if is_auth_error(&err) => {
                trace!("Token got invalid - try to obtain a new one");

                self.refresh_token().await?;
                let new_token = &self.state.access_token;
                request_fn(&self.client, &self.base_url, new_token).await
            }
            Err(err) => Err(err),
        }
    }
}
