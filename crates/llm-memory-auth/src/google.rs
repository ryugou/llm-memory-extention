use oauth2::basic::BasicClient;
use oauth2::{AuthUrl, ClientId, ClientSecret, RedirectUrl, TokenUrl, AuthorizationCode, CsrfToken, PkceCodeChallenge, PkceCodeVerifier, Scope, TokenResponse};
use reqwest::Client;
use serde::Deserialize;

use crate::error::AuthError;

pub struct GoogleConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

pub struct GoogleClient {
    inner: BasicClient,
    http: Client,
}

#[derive(Debug, Deserialize)]
pub struct GoogleUserInfo {
    pub sub: String,
    pub email: Option<String>,
}

impl GoogleClient {
    pub fn new(cfg: GoogleConfig) -> Self {
        let inner = BasicClient::new(
            ClientId::new(cfg.client_id),
            Some(ClientSecret::new(cfg.client_secret)),
            AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".into()).expect("AuthUrl"),
            Some(TokenUrl::new("https://oauth2.googleapis.com/token".into()).expect("TokenUrl")),
        )
        .set_redirect_uri(RedirectUrl::new(cfg.redirect_uri).expect("RedirectUrl"));
        Self {
            inner,
            http: Client::new(),
        }
    }

    /// Returns (authorize_url, csrf_token, pkce_verifier).
    /// Caller must persist csrf_token and pkce_verifier for the callback step.
    pub fn authorize_url(&self) -> (url::Url, CsrfToken, PkceCodeVerifier) {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, csrf) = self
            .inner
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("openid".into()))
            .add_scope(Scope::new("email".into()))
            .set_pkce_challenge(challenge)
            .url();
        (url, csrf, verifier)
    }

    /// Exchange authorization code for access token.
    pub async fn exchange_code(&self, code: String, verifier: PkceCodeVerifier) -> Result<String, AuthError> {
        let token = self
            .inner
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(verifier)
            .request_async(oauth2::reqwest::async_http_client)
            .await
            .map_err(|e| AuthError::OAuth(e.to_string()))?;
        Ok(token.access_token().secret().clone())
    }

    /// Fetch OIDC userinfo with the access token.
    pub async fn userinfo(&self, access_token: &str) -> Result<GoogleUserInfo, AuthError> {
        let info = self
            .http
            .get("https://openidconnect.googleapis.com/v1/userinfo")
            .bearer_auth(access_token)
            .send()
            .await?
            .error_for_status()?
            .json::<GoogleUserInfo>()
            .await?;
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_has_pkce_challenge() {
        let c = GoogleClient::new(GoogleConfig {
            client_id: "test-client-id".into(),
            client_secret: "test-secret".into(),
            redirect_uri: "https://example.com/cb".into(),
        });
        let (url, _csrf, _verifier) = c.authorize_url();
        let s = url.to_string();
        assert!(s.contains("code_challenge="), "PKCE challenge should be in URL: {s}");
        assert!(s.contains("code_challenge_method=S256"));
        assert!(s.contains("client_id=test-client-id"));
        assert!(s.contains("redirect_uri="));
    }

    #[test]
    fn authorize_url_requests_openid_email_scopes() {
        let c = GoogleClient::new(GoogleConfig {
            client_id: "id".into(), client_secret: "s".into(),
            redirect_uri: "https://example.com/cb".into(),
        });
        let (url, _, _) = c.authorize_url();
        let s = url.to_string();
        assert!(s.contains("scope=openid"));
        assert!(s.contains("email"));
    }
}
