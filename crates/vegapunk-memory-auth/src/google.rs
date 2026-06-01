use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
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
        // Google userinfo / token endpoint への TCP hang から保護する total timeout。
        // timeout 無しの `Client::new()` だと down host への TLS handshake で
        // 数十秒 await し続けることがあり、OAuth callback 経路で UX を悪化させる。
        // 10s は Google の SLO に対して十分余裕がある現実的な上限。
        //
        // `redirect(Policy::none())` は SSRF 防止のため。oauth2 crate の default
        // `async_http_client` も同じ判断で no-redirect を強制している。
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client build with timeout");
        Self { inner, http }
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
    pub async fn exchange_code(
        &self,
        code: String,
        verifier: PkceCodeVerifier,
    ) -> Result<String, AuthError> {
        // `oauth2::reqwest::async_http_client` を使うと library 内部で毎回
        // reqwest::Client::new() (= timeout 無し) を構築するため、token endpoint
        // への TCP hang から保護できない。自前 closure で `self.http`
        // (timeout=10s, redirect=none) を経由させて userinfo と同じ制約を適用。
        let http = self.http.clone();
        let token = self
            .inner
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(verifier)
            .request_async(move |request| {
                let http = http.clone();
                async move {
                    // oauth2 4.x は古い `http` crate (0.x) の Method/HeaderMap を
                    // 使うのに対し reqwest 0.12 は `http` 1.x を使うため、bytes
                    // を経由して型変換する。
                    let method = reqwest::Method::from_bytes(request.method.as_str().as_bytes())
                        .expect("oauth2 always passes valid HTTP method bytes");
                    let mut builder =
                        http.request(method, request.url.as_str()).body(request.body);
                    for (name, value) in &request.headers {
                        builder = builder.header(name.as_str(), value.as_bytes());
                    }
                    let req = builder.build()?;
                    let resp = http.execute(req).await?;
                    let status =
                        oauth2::http::StatusCode::from_u16(resp.status().as_u16())
                            .expect("reqwest StatusCode is always valid http 0.x StatusCode");
                    let mut oauth_headers = oauth2::http::HeaderMap::new();
                    for (name, value) in resp.headers() {
                        if let (Ok(n), Ok(v)) = (
                            oauth2::http::HeaderName::from_bytes(name.as_str().as_bytes()),
                            oauth2::http::HeaderValue::from_bytes(value.as_bytes()),
                        ) {
                            oauth_headers.insert(n, v);
                        }
                    }
                    let body = resp.bytes().await?.to_vec();
                    Ok::<oauth2::HttpResponse, reqwest::Error>(oauth2::HttpResponse {
                        status_code: status,
                        headers: oauth_headers,
                        body,
                    })
                }
            })
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
        assert!(
            s.contains("code_challenge="),
            "PKCE challenge should be in URL: {s}"
        );
        assert!(s.contains("code_challenge_method=S256"));
        assert!(s.contains("client_id=test-client-id"));
        assert!(s.contains("redirect_uri="));
    }

    #[test]
    fn authorize_url_requests_openid_email_scopes() {
        let c = GoogleClient::new(GoogleConfig {
            client_id: "id".into(),
            client_secret: "s".into(),
            redirect_uri: "https://example.com/cb".into(),
        });
        let (url, _, _) = c.authorize_url();
        let s = url.to_string();
        assert!(s.contains("scope=openid"));
        assert!(s.contains("email"));
    }
}
