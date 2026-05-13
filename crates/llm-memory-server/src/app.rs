use std::sync::Arc;

use axum::{routing::get, Router};
use sqlx::SqlitePool;

use llm_memory_auth::jwt::JwtKeys;
use llm_memory_coordinator::coordinator::Coordinator;
use llm_memory_coordinator::state::StateMap;
use llm_memory_coordinator::worker::WorkerDeps;
use llm_memory_llm::client_http::AnthropicHttp;

use crate::config::ServerConfig;
use crate::metrics::Metrics;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub coordinator: Coordinator<AnthropicHttp>,
    pub jwt_keys: JwtKeys,
    pub cfg: Arc<ServerConfig>,
    pub rate_limiter: Arc<crate::rate_limit::RateLimiter>,
    pub metrics: Arc<Metrics>,
}

pub async fn build_state(cfg: ServerConfig) -> anyhow::Result<AppState> {
    let pool = llm_memory_storage::pool::init_pool(&cfg.database_url).await?;
    let llm = Arc::new(AnthropicHttp::new(cfg.anthropic_api_key.clone()));
    let deps = Arc::new(WorkerDeps {
        pool: pool.clone(),
        state: StateMap::new(),
        llm,
        model_haiku: cfg.model_haiku.clone(),
        model_sonnet: cfg.model_sonnet.clone(),
    });
    let coordinator = Coordinator::new(deps);
    let jwt_keys = JwtKeys::from_env();
    Ok(AppState {
        pool,
        coordinator,
        jwt_keys,
        cfg: Arc::new(cfg),
        rate_limiter: Arc::new(crate::rate_limit::RateLimiter::new()),
        metrics: Arc::new(Metrics::new()),
    })
}

pub fn build_router(state: AppState) -> Router {
    // AS state (separate from AppState — has its own session storage).
    let google = std::sync::Arc::new(llm_memory_auth::google::GoogleClient::new(
        llm_memory_auth::google::GoogleConfig {
            client_id: state.cfg.google_client_id.clone(),
            client_secret: state.cfg.google_client_secret.clone(),
            redirect_uri: format!("{}/oauth/callback/google", state.cfg.public_url),
        },
    ));
    let as_state = llm_memory_auth::authorization_server::AsState::new(
        state.pool.clone(),
        state.jwt_keys.clone(),
        google,
        state.cfg.public_url.clone(),
        state.cfg.trusted_proxy_count,
    );
    let as_router = llm_memory_auth::authorization_server::router().with_state(as_state);

    // /mcp requires auth; /healthz does not.
    let mcp_router = Router::new()
        .route("/mcp", axum::routing::post(crate::mcp::transport::handle))
        .route_layer(axum::middleware::from_fn_with_state(
            state.jwt_keys.clone(),
            llm_memory_auth::middleware::require_auth,
        ))
        .with_state(state.clone());

    Router::new()
        .merge(as_router)
        .merge(mcp_router)
        .route("/healthz", get(healthz))
        .route("/metrics", axum::routing::get(crate::metrics::handler))
        .with_state(state)
}

async fn healthz() -> &'static str { "ok" }

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn test_state() -> AppState {
        let cfg = ServerConfig {
            database_url: "sqlite::memory:".into(),
            bind_addr: "0.0.0.0:8080".into(),
            public_url: "https://test.example.com".into(),
            anthropic_api_key: "test".into(),
            google_client_id: "id".into(),
            google_client_secret: "s".into(),
            model_haiku: "h".into(),
            model_sonnet: "s".into(),
            trusted_proxy_count: 1,
        };
        build_state(cfg).await.unwrap()
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let state = test_state().await;
        let router = build_router(state);
        let res = router
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(res.status(), 200);
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"ok");
    }
}
