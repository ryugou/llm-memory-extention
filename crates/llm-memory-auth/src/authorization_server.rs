//! OAuth 2.1 Authorization Server scaffold (RFC 8414 metadata, RFC 7591 DCR).
//!
//! Handler bodies are placeholders — Task 29 fills in the real logic.
//! The `AsState` struct holds what AS handlers need at runtime.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;

use crate::google::GoogleClient;
use crate::jwt::JwtKeys;

/// State shared with the AS endpoints. Server crate constructs and injects this.
#[derive(Clone)]
pub struct AsState {
    pub pool: SqlitePool,
    pub jwt_keys: JwtKeys,
    pub google: Arc<GoogleClient>,
    pub public_url: String,
    pub trusted_proxy_count: usize,
}

/// Build the AS router. Caller merges this into the main application router.
pub fn router() -> Router<AsState> {
    Router::new()
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/callback/google", get(callback_google))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
}

// ---- handler stubs ----

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

async fn register(
    State(_state): State<AsState>,
    Json(_body): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Task 29 implements DCR validation + persistence
    (StatusCode::NOT_IMPLEMENTED, "register: pending Task 29")
}

#[derive(Deserialize)]
struct AuthorizeParams {
    #[allow(dead_code)] client_id: Option<String>,
    #[allow(dead_code)] redirect_uri: Option<String>,
    #[allow(dead_code)] state: Option<String>,
    #[allow(dead_code)] code_challenge: Option<String>,
    #[allow(dead_code)] code_challenge_method: Option<String>,
}

async fn authorize(
    State(_state): State<AsState>,
    Query(_p): Query<AuthorizeParams>,
) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "authorize: pending Task 29")
}

#[derive(Deserialize)]
struct CallbackParams {
    #[allow(dead_code)] code: Option<String>,
    #[allow(dead_code)] state: Option<String>,
}

async fn callback_google(
    State(_state): State<AsState>,
    Query(_p): Query<CallbackParams>,
) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "callback: pending Task 29")
}

async fn token(
    State(_state): State<AsState>,
    Json(_body): Json<serde_json::Value>,
) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "token: pending Task 29")
}

async fn revoke(
    State(_state): State<AsState>,
    Json(_body): Json<serde_json::Value>,
) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "revoke: pending Task 29")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;
    use std::collections::HashMap;

    fn test_state() -> AsState {
        AsState {
            pool: futures::executor::block_on(async {
                llm_memory_storage::pool::init_pool("sqlite::memory:").await.unwrap()
            }),
            jwt_keys: JwtKeys {
                current_kid: "v1".into(),
                keys: {
                    let mut m = HashMap::new();
                    m.insert("v1".into(), vec![0u8; 32]);
                    m
                },
            },
            google: Arc::new(GoogleClient::new(crate::google::GoogleConfig {
                client_id: "test".into(), client_secret: "s".into(),
                redirect_uri: "https://example.com/cb".into(),
            })),
            public_url: "https://memory.example.com".into(),
            trusted_proxy_count: 1,
        }
    }

    #[tokio::test]
    async fn metadata_endpoint_returns_json() {
        let app = router().with_state(test_state());
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
        assert!(v["authorization_endpoint"]
            .as_str()
            .unwrap()
            .contains("/oauth/authorize"));
    }

    #[tokio::test]
    async fn other_endpoints_return_not_implemented() {
        let app = router().with_state(test_state());
        let res = app
            .oneshot(
                Request::post("/oauth/token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
    }
}
