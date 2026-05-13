//! OAuth 2.1 Authorization Server (RFC 8414 metadata, RFC 7591 DCR).
//!
//! Flow:
//! 1. Client calls /oauth/register (DCR) to get a client_id.
//! 2. Client redirects user to /oauth/authorize with their PKCE challenge.
//! 3. We redirect to Google. User authenticates.
//! 4. Google returns to /oauth/callback/google with code+state.
//! 5. We exchange Google code → userinfo → upsert user → issue an auth code.
//! 6. We redirect back to the client's redirect_uri with our auth code.
//! 7. Client calls /oauth/token with the auth code + PKCE verifier.
//! 8. We verify PKCE and issue (access_token, refresh_token).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use axum::{
    Form, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Redirect},
    routing::{get, post},
};
use base64::Engine;
use llm_memory_core::id::new_ulid;
use llm_memory_core::time::now_ms;
use oauth2::PkceCodeVerifier;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tracing::warn;

use crate::dcr;
use crate::google::GoogleClient;
use crate::jwt::{self, JwtKeys};

const AUTH_CODE_TTL_SECS: i64 = 60;
const ACCESS_TOKEN_TTL_SECS: i64 = 3600;
const REFRESH_TOKEN_TTL_SECS: i64 = 30 * 24 * 3600;

/// Pending /authorize session: tracks the client's PKCE challenge while we
/// bounce through Google.
#[derive(Clone, Debug)]
struct PendingAuth {
    client_id: String,
    redirect_uri: String,
    client_state: Option<String>,
    code_challenge: String,
    code_challenge_method: String,
    #[allow(dead_code)]
    google_csrf: String,
    google_verifier_secret: String,
    expires_at_ms: i64,
}

/// Authorization code: short-lived, one-time use.
#[derive(Clone, Debug)]
struct AuthCode {
    user_id: String,
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    #[allow(dead_code)]
    code_challenge_method: String,
    expires_at_ms: i64,
}

#[derive(Clone, Default)]
struct InMemorySessions {
    pending: Arc<Mutex<HashMap<String, PendingAuth>>>,
    codes: Arc<Mutex<HashMap<String, AuthCode>>>,
}

impl InMemorySessions {
    fn put_pending(&self, key: String, p: PendingAuth) {
        self.pending.lock().unwrap().insert(key, p);
    }
    fn take_pending(&self, key: &str) -> Option<PendingAuth> {
        let mut m = self.pending.lock().unwrap();
        let now = now_ms();
        m.retain(|_, p| p.expires_at_ms > now);
        m.remove(key)
    }
    fn put_code(&self, key: String, c: AuthCode) {
        self.codes.lock().unwrap().insert(key, c);
    }
    fn take_code(&self, key: &str) -> Option<AuthCode> {
        let mut m = self.codes.lock().unwrap();
        let now = now_ms();
        m.retain(|_, c| c.expires_at_ms > now);
        m.remove(key)
    }
}

#[derive(Clone)]
pub struct AsState {
    pub pool: SqlitePool,
    pub jwt_keys: JwtKeys,
    pub google: Arc<GoogleClient>,
    pub public_url: String,
    pub trusted_proxy_count: usize,
    sessions: InMemorySessions,
}

impl AsState {
    pub fn new(
        pool: SqlitePool,
        jwt_keys: JwtKeys,
        google: Arc<GoogleClient>,
        public_url: String,
        trusted_proxy_count: usize,
    ) -> Self {
        Self {
            pool,
            jwt_keys,
            google,
            public_url,
            trusted_proxy_count,
            sessions: InMemorySessions::default(),
        }
    }
}

pub fn router() -> Router<AsState> {
    Router::new()
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/callback/google", get(callback_google))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
}

async fn metadata(State(state): State<AsState>) -> impl IntoResponse {
    Json(json!({
        "issuer": state.public_url,
        "authorization_endpoint": format!("{}/oauth/authorize", state.public_url),
        "token_endpoint": format!("{}/oauth/token", state.public_url),
        "registration_endpoint": format!("{}/oauth/register", state.public_url),
        "revocation_endpoint": format!("{}/oauth/revoke", state.public_url),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none", "client_secret_basic"]
    }))
}

async fn register(State(state): State<AsState>, Json(body): Json<dcr::DcrRequest>) -> Response {
    let mut resp = match dcr::validate(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "invalid_client_metadata",
                    "error_description": e.to_string()
                })),
            )
                .into_response();
        }
    };
    let redirect_uris_json = serde_json::to_string(&resp.redirect_uris).unwrap();
    let grant_types_json = serde_json::to_string(&resp.grant_types).unwrap();
    let client = match llm_memory_storage::oauth_clients::register(
        &state.pool,
        &redirect_uris_json,
        &grant_types_json,
        &resp.token_endpoint_auth_method,
        resp.client_name.as_deref(),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => return server_error(&e.to_string()),
    };
    resp.client_id = client.id;
    (
        StatusCode::CREATED,
        Json(serde_json::to_value(resp).unwrap()),
    )
        .into_response()
}

#[derive(Deserialize)]
struct AuthorizeParams {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    code_challenge_method: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    scope: Option<String>,
}

async fn authorize(State(state): State<AsState>, Query(p): Query<AuthorizeParams>) -> Response {
    if p.response_type != "code" {
        return bad_request(
            "unsupported_response_type",
            "only response_type=code supported",
        );
    }
    if p.code_challenge_method != "S256" {
        return bad_request("invalid_request", "code_challenge_method must be S256");
    }
    // Verify client exists + redirect_uri is registered.
    let client = match llm_memory_storage::oauth_clients::get(&state.pool, &p.client_id).await {
        Ok(Some(c)) => c,
        _ => return bad_request("invalid_client", "unknown client_id"),
    };
    let allowed: Vec<String> = serde_json::from_str(&client.redirect_uris).unwrap_or_default();
    if !allowed.contains(&p.redirect_uri) {
        return bad_request("invalid_request", "redirect_uri not registered");
    }

    // Touch the client's last-seen timestamp.
    let _ = llm_memory_storage::oauth_clients::touch_last_seen(&state.pool, &p.client_id).await;

    // Build the Google authorize URL.
    let (g_url, csrf, verifier) = state.google.authorize_url();
    let pending_key = csrf.secret().clone();
    let pending = PendingAuth {
        client_id: p.client_id.clone(),
        redirect_uri: p.redirect_uri.clone(),
        client_state: p.state.clone(),
        code_challenge: p.code_challenge.clone(),
        code_challenge_method: p.code_challenge_method.clone(),
        google_csrf: csrf.secret().clone(),
        google_verifier_secret: verifier.secret().clone(),
        expires_at_ms: now_ms() + 10 * 60 * 1000,
    };
    state.sessions.put_pending(pending_key, pending);
    Redirect::to(g_url.as_ref()).into_response()
}

#[derive(Deserialize)]
struct CallbackParams {
    code: String,
    state: String,
}

async fn callback_google(
    State(state): State<AsState>,
    Query(p): Query<CallbackParams>,
) -> Response {
    let pending = match state.sessions.take_pending(&p.state) {
        Some(x) => x,
        None => return bad_request("invalid_state", "no matching session"),
    };
    let verifier = PkceCodeVerifier::new(pending.google_verifier_secret.clone());
    let access_token = match state.google.exchange_code(p.code, verifier).await {
        Ok(t) => t,
        Err(e) => {
            warn!(?e, "google code exchange failed");
            return bad_request("google_exchange_failed", &e.to_string());
        }
    };
    let info = match state.google.userinfo(&access_token).await {
        Ok(i) => i,
        Err(e) => return bad_request("google_userinfo_failed", &e.to_string()),
    };
    let user_id = new_ulid();
    let user = match llm_memory_storage::users::upsert(
        &state.pool,
        &user_id,
        "google",
        &info.sub,
        info.email.as_deref(),
    )
    .await
    {
        Ok(u) => u,
        Err(e) => return server_error(&e.to_string()),
    };
    // Issue our auth code.
    let code = new_ulid();
    state.sessions.put_code(
        code.clone(),
        AuthCode {
            user_id: user.id,
            client_id: pending.client_id,
            redirect_uri: pending.redirect_uri.clone(),
            code_challenge: pending.code_challenge,
            code_challenge_method: pending.code_challenge_method,
            expires_at_ms: now_ms() + AUTH_CODE_TTL_SECS * 1000,
        },
    );

    // Redirect to client's redirect_uri with code+state.
    let mut url = pending.redirect_uri.clone();
    let sep = if url.contains('?') { '&' } else { '?' };
    url.push_str(&format!("{sep}code={code}"));
    if let Some(client_state) = pending.client_state {
        url.push_str(&format!("&state={}", urlencoding(&client_state)));
    }
    Redirect::to(&url).into_response()
}

fn urlencoding(s: &str) -> String {
    // tiny utility — only need to encode states which are typically alphanumeric
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect()
}

#[derive(Deserialize)]
struct TokenForm {
    grant_type: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Serialize)]
struct TokenResponse {
    access_token: String,
    token_type: &'static str,
    expires_in: i64,
    refresh_token: String,
}

async fn token(State(state): State<AsState>, Form(body): Form<TokenForm>) -> Response {
    match body.grant_type.as_str() {
        "authorization_code" => grant_auth_code(state, body).await,
        "refresh_token" => grant_refresh(state, body).await,
        _ => bad_request(
            "unsupported_grant_type",
            "only authorization_code or refresh_token",
        ),
    }
}

async fn grant_auth_code(state: AsState, body: TokenForm) -> Response {
    let code = match body.code {
        Some(c) => c,
        None => return bad_request("invalid_request", "code required"),
    };
    let verifier = match body.code_verifier {
        Some(v) => v,
        None => return bad_request("invalid_request", "code_verifier required (PKCE)"),
    };
    let entry = match state.sessions.take_code(&code) {
        Some(e) => e,
        None => return bad_request("invalid_grant", "code unknown or expired"),
    };
    // Verify PKCE
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    if computed != entry.code_challenge {
        return bad_request("invalid_grant", "PKCE verifier mismatch");
    }
    // Verify client and redirect_uri
    if let Some(req_client) = body.client_id {
        if req_client != entry.client_id {
            return bad_request("invalid_grant", "client_id mismatch");
        }
    }
    if let Some(req_uri) = body.redirect_uri {
        if req_uri != entry.redirect_uri {
            return bad_request("invalid_grant", "redirect_uri mismatch");
        }
    }
    issue_tokens(state, &entry.user_id, &entry.client_id).await
}

async fn grant_refresh(state: AsState, body: TokenForm) -> Response {
    let tok = match body.refresh_token {
        Some(t) => t,
        None => return bad_request("invalid_request", "refresh_token required"),
    };
    let now = now_ms();
    let (user_id, client_id) =
        match llm_memory_storage::tokens::validate_refresh(&state.pool, &tok, now).await {
            Ok(Some(x)) => x,
            Ok(None) => return bad_request("invalid_grant", "refresh_token unknown or expired"),
            Err(e) => return server_error(&e.to_string()),
        };
    issue_tokens(state, &user_id, &client_id).await
}

async fn issue_tokens(state: AsState, user_id: &str, client_id: &str) -> Response {
    let access = match jwt::issue(&state.jwt_keys, user_id, client_id, ACCESS_TOKEN_TTL_SECS) {
        Ok(t) => t,
        Err(e) => return server_error(&e.to_string()),
    };
    let refresh = new_ulid();
    let expires_at = now_ms() + REFRESH_TOKEN_TTL_SECS * 1000;
    if let Err(e) = llm_memory_storage::tokens::create_refresh(
        &state.pool,
        &refresh,
        user_id,
        client_id,
        expires_at,
    )
    .await
    {
        return server_error(&e.to_string());
    }
    Json(TokenResponse {
        access_token: access,
        token_type: "Bearer",
        expires_in: ACCESS_TOKEN_TTL_SECS,
        refresh_token: refresh,
    })
    .into_response()
}

#[derive(Deserialize)]
struct RevokeForm {
    token: String,
    #[serde(default)]
    #[allow(dead_code)]
    token_type_hint: Option<String>,
}

async fn revoke(State(state): State<AsState>, Form(body): Form<RevokeForm>) -> impl IntoResponse {
    let _ = body.token_type_hint; // ヒントは無視（refresh_token のみ revoke）
    let _ = llm_memory_storage::tokens::revoke(&state.pool, &body.token, now_ms()).await;
    StatusCode::OK
}

// ---- helpers ----

type Response = axum::response::Response;

fn bad_request(error: &str, description: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":error,"error_description":description})),
    )
        .into_response()
}
fn server_error(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error":"server_error","error_description":message})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use std::collections::HashMap;
    use tower::ServiceExt;

    async fn test_state() -> AsState {
        let pool = llm_memory_storage::pool::init_pool("sqlite::memory:")
            .await
            .unwrap();
        let mut m = HashMap::new();
        m.insert("v1".into(), b"01234567890123456789012345678901".to_vec());
        let keys = JwtKeys {
            current_kid: "v1".into(),
            keys: m,
        };
        let g = Arc::new(GoogleClient::new(crate::google::GoogleConfig {
            client_id: "g".into(),
            client_secret: "s".into(),
            redirect_uri: "https://memory.example.com/oauth/callback/google".into(),
        }));
        AsState::new(pool, keys, g, "https://memory.example.com".into(), 1)
    }

    #[tokio::test]
    async fn metadata_returns_expected_fields() {
        let app = router().with_state(test_state().await);
        let res = app
            .oneshot(
                Request::get("/.well-known/oauth-authorization-server")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["issuer"], "https://memory.example.com");
        assert_eq!(v["grant_types_supported"][0], "authorization_code");
    }

    #[tokio::test]
    async fn register_persists_a_client() {
        let s = test_state().await;
        let app = router().with_state(s.clone());
        let req = Request::post("/oauth/register")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "redirect_uris": ["https://claude.ai/cb"],
                    "grant_types": ["authorization_code","refresh_token"],
                    "token_endpoint_auth_method": "none",
                    "client_name": "Claude Test"
                }))
                .unwrap(),
            ))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["client_id"].as_str().unwrap().len() == 26); // ULID
        assert_eq!(v["redirect_uris"][0], "https://claude.ai/cb");
    }

    #[tokio::test]
    async fn register_rejects_http_redirect() {
        let s = test_state().await;
        let app = router().with_state(s);
        let req = Request::post("/oauth/register")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "redirect_uris": ["http://insecure/cb"]
                }))
                .unwrap(),
            ))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn authorize_unknown_client_returns_400() {
        let s = test_state().await;
        let app = router().with_state(s);
        let q = "response_type=code&client_id=unknown&redirect_uri=https%3A%2F%2Fc%2Fcb&code_challenge=x&code_challenge_method=S256";
        let req = Request::get(format!("/oauth/authorize?{q}"))
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn token_unsupported_grant_returns_400() {
        let s = test_state().await;
        let app = router().with_state(s);
        let req = Request::post("/oauth/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("grant_type=implicit"))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
}
