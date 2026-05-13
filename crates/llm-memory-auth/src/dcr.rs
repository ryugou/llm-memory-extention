use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::AuthError;

#[derive(Debug, Deserialize)]
pub struct DcrRequest {
    pub redirect_uris: Vec<String>,
    #[serde(default = "default_grant_types")]
    pub grant_types: Vec<String>,
    #[serde(default)]
    pub token_endpoint_auth_method: Option<String>,
    pub client_name: Option<String>,
}

fn default_grant_types() -> Vec<String> {
    vec!["authorization_code".into(), "refresh_token".into()]
}

#[derive(Debug, Serialize)]
pub struct DcrResponse {
    pub client_id: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub token_endpoint_auth_method: String,
    pub client_name: Option<String>,
}

pub const MAX_REDIRECT_URIS: usize = 5;
pub const ALLOWED_GRANT_TYPES: &[&str] = &["authorization_code", "refresh_token"];
pub const ALLOWED_AUTH_METHODS: &[&str] = &["none"];

/// Validate a DCR request and return a populated response (without client_id).
/// Caller must fill `client_id` with a freshly generated value (ULID).
pub fn validate(req: &DcrRequest) -> Result<DcrResponse, AuthError> {
    if req.redirect_uris.is_empty() {
        return Err(AuthError::OAuth("redirect_uris required".into()));
    }
    if req.redirect_uris.len() > MAX_REDIRECT_URIS {
        return Err(AuthError::OAuth(format!(
            "redirect_uris exceeds max {MAX_REDIRECT_URIS}"
        )));
    }
    for u in &req.redirect_uris {
        let parsed =
            Url::parse(u).map_err(|_| AuthError::OAuth(format!("invalid redirect_uri: {u}")))?;
        if parsed.scheme() != "https" {
            return Err(AuthError::OAuth(format!("redirect_uri must be https: {u}")));
        }
    }
    for g in &req.grant_types {
        if !ALLOWED_GRANT_TYPES.contains(&g.as_str()) {
            return Err(AuthError::OAuth(format!("grant_type not allowed: {g}")));
        }
    }
    let method = req
        .token_endpoint_auth_method
        .clone()
        .unwrap_or_else(|| "none".into());
    if !ALLOWED_AUTH_METHODS.contains(&method.as_str()) {
        return Err(AuthError::OAuth(format!(
            "auth method not allowed: {method}"
        )));
    }
    Ok(DcrResponse {
        client_id: String::new(),
        redirect_uris: req.redirect_uris.clone(),
        grant_types: req.grant_types.clone(),
        token_endpoint_auth_method: method,
        client_name: req.client_name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> DcrRequest {
        DcrRequest {
            redirect_uris: vec!["https://example.com/cb".into()],
            grant_types: default_grant_types(),
            token_endpoint_auth_method: None,
            client_name: None,
        }
    }

    #[test]
    fn accepts_https_redirect() {
        assert!(validate(&base()).is_ok());
    }

    #[test]
    fn rejects_http_redirect() {
        let mut r = base();
        r.redirect_uris = vec!["http://example.com/cb".into()];
        assert!(validate(&r).is_err());
    }

    #[test]
    fn rejects_unknown_grant_type() {
        let mut r = base();
        r.grant_types = vec!["implicit".into()];
        assert!(validate(&r).is_err());
    }

    #[test]
    fn rejects_too_many_redirects() {
        let mut r = base();
        r.redirect_uris = vec!["https://x/cb".into(); 6];
        assert!(validate(&r).is_err());
    }

    #[test]
    fn rejects_empty_redirects() {
        let mut r = base();
        r.redirect_uris = vec![];
        assert!(validate(&r).is_err());
    }

    #[test]
    fn defaults_auth_method_to_none() {
        let resp = validate(&base()).unwrap();
        assert_eq!(resp.token_endpoint_auth_method, "none");
    }

    #[test]
    fn rejects_client_secret_basic() {
        // /oauth/token 側で client_secret_basic を検証していないため、当面 advertise しない。
        let mut r = base();
        r.token_endpoint_auth_method = Some("client_secret_basic".into());
        assert!(validate(&r).is_err());
    }

    #[test]
    fn rejects_unknown_auth_method() {
        let mut r = base();
        r.token_endpoint_auth_method = Some("client_secret_jwt".into());
        assert!(validate(&r).is_err());
    }
}
