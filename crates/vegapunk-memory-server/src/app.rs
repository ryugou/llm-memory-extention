//! HTTP transport の組み立て。
//!
//! `build_state` で pool 初期化 / vegapunk gRPC 接続 / JWT 鍵をまとめ、
//! `build_router` で OAuth router + protected route + healthz をマウントする。
//! MCP `/mcp` tool dispatch は次 PR で追加する想定で、本 PR では skeleton
//! のみ (= bind 経路と認証経路の wiring まで)。

use std::sync::Arc;

use anyhow::Result;
use axum::{
    Router,
    routing::{get, post},
};

use vegapunk_memory_auth::jwt::JwtKeys;
use vegapunk_memory_auth::middleware::{AuthState, require_auth};

use crate::config::ServerConfig;
use crate::mcp;
use crate::state::AppState;

/// 起動時 state を組み立てる。
/// - DB pool: `vegapunk_memory_storage::pool::init_pool` で WAL + migrations
/// - vegapunk gRPC: `VEGAPUNK_GRPC_ENDPOINT` + `VEGAPUNK_BEARER_TOKEN`
/// - JWT 鍵は caller (`main`) から渡す (= startup 失敗の局所化)
pub async fn build_state(cfg: ServerConfig, jwt_keys: JwtKeys) -> Result<AppState> {
    let pool = vegapunk_memory_storage::pool::init_pool(&cfg.database_url).await?;
    let vegapunk =
        vegapunk_client::connect(&cfg.vegapunk_grpc_endpoint, &cfg.vegapunk_bearer_token)
            .await
            .map_err(|e| anyhow::anyhow!("vegapunk gRPC connect failed: {e}"))?;
    Ok(AppState {
        pool,
        vegapunk,
        jwt_keys,
        cfg: Arc::new(cfg),
    })
}

/// HTTP router を組み立てる。
///
/// 公開エンドポイント:
/// - `GET /healthz`: liveness
/// - OAuth Authorization Server (vegapunk_memory_auth::authorization_server):
///   `/.well-known/oauth-authorization-server`, `/oauth/register`,
///   `/oauth/authorize`, `/oauth/callback/google`, `/oauth/token`,
///   `/oauth/revoke`
///
/// 認証必須エンドポイント (`route_layer(require_auth)` 配下):
/// - `POST /mcp`: MCP Streamable HTTP transport
pub fn build_router(state: AppState) -> Router {
    // OAuth Authorization Server は独自の AsState を持つ別 router として組む。
    let google = Arc::new(vegapunk_memory_auth::google::GoogleClient::new(
        vegapunk_memory_auth::google::GoogleConfig {
            client_id: state.cfg.google_client_id.clone(),
            client_secret: state.cfg.google_client_secret.clone(),
            redirect_uri: format!("{}/oauth/callback/google", state.cfg.public_url),
        },
    ));
    let as_state = vegapunk_memory_auth::authorization_server::AsState::new(
        state.pool.clone(),
        state.jwt_keys.clone(),
        google,
        state.cfg.public_url.clone(),
        state.cfg.trusted_proxy_count,
    );
    let as_router = vegapunk_memory_auth::authorization_server::router().with_state(as_state);

    let auth_state = AuthState::new(state.jwt_keys.clone(), state.pool.clone());
    let protected_router = Router::new()
        .route("/mcp", post(mcp::transport::handle))
        .route_layer(axum::middleware::from_fn_with_state(
            auth_state,
            require_auth,
        ))
        .with_state(state.clone());

    Router::new()
        .merge(as_router)
        .merge(protected_router)
        .route("/healthz", get(healthz))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    use vegapunk_client::BearerAuthInterceptor;
    use vegapunk_client::graphrag::graph_rag_engine_client::GraphRagEngineClient;

    /// HTTP transport の routing 確認用に AppState を組み立てる。
    /// 実 vegapunk への接続は使わず、無効な channel に Bearer interceptor を
    /// 被せた client を仕込む (= route まで届けば良く、handler が gRPC を
    /// 呼ばない限り問題ない)。
    async fn test_state() -> AppState {
        let pool = vegapunk_memory_storage::pool::init_pool("sqlite::memory:")
            .await
            .unwrap();
        let endpoint = tonic::transport::Endpoint::from_static("http://127.0.0.1:0");
        let channel = endpoint.connect_lazy();
        let interceptor = BearerAuthInterceptor::new("dummy").unwrap();
        let vegapunk = GraphRagEngineClient::with_interceptor(channel, interceptor);
        let cfg = ServerConfig {
            database_url: "sqlite::memory:".into(),
            bind_addr: "0.0.0.0:8081".into(),
            public_url: "https://test.example.com".into(),
            google_client_id: "id".into(),
            google_client_secret: "s".into(),
            vegapunk_grpc_endpoint: "http://127.0.0.1:0".into(),
            vegapunk_bearer_token: "dummy".into(),
            trusted_proxy_count: 1,
        };
        AppState {
            pool,
            vegapunk,
            jwt_keys: JwtKeys::for_tests(),
            cfg: Arc::new(cfg),
        }
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let state = test_state().await;
        let router = build_router(state);
        let res = router
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn mcp_endpoint_requires_auth() {
        // 認証ヘッダ無しで /mcp を叩いたら 401。 protected_router が
        // require_auth middleware 配下に置かれていることの保証。
        let state = test_state().await;
        let router = build_router(state);
        let res = router
            .oneshot(
                Request::post("/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 401);
    }

    #[tokio::test]
    async fn oauth_metadata_is_mounted() {
        // .well-known/oauth-authorization-server は OAuth Authorization Server
        // router がマウントされていれば 200 を返す (= as_router が wiring
        // された証拠)。
        let state = test_state().await;
        let router = build_router(state);
        let res = router
            .oneshot(
                Request::get("/.well-known/oauth-authorization-server")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }
}
