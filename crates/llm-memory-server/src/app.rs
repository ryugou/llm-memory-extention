use std::sync::Arc;

use axum::{Router, routing::get};
use sqlx::SqlitePool;

use llm_memory_auth::jwt::JwtKeys;
use llm_memory_coordinator::coordinator::Coordinator;
use llm_memory_coordinator::state::StateMap;
use llm_memory_coordinator::worker::WorkerDeps;
use llm_memory_llm::client::LlmClient;
use llm_memory_llm::client_http::VertexAi;
use llm_memory_llm::mock::MockClient;

use crate::config::ServerConfig;
use crate::metrics::Metrics;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub coordinator: Coordinator,
    pub jwt_keys: JwtKeys,
    pub cfg: Arc<ServerConfig>,
    pub rate_limiter: Arc<crate::rate_limit::RateLimiter>,
    pub metrics: Arc<Metrics>,
}

/// Build the shared app state.
///
/// JWT signing keys are passed in explicitly (rather than loaded inside) so
/// callers can fail fast on configuration errors (`main`) or substitute a
/// deterministic test key (`build_state_for_tests`).
pub async fn build_state(cfg: ServerConfig, jwt_keys: JwtKeys) -> anyhow::Result<AppState> {
    let llm: Arc<dyn LlmClient> = Arc::new(
        VertexAi::new(cfg.vertex_project.clone(), cfg.vertex_location.clone())
            .await
            .map_err(|e| anyhow::anyhow!("VertexAi init failed: {e}"))?,
    );
    build_state_with_llm(cfg, jwt_keys, llm).await
}

/// Shared builder used by both production (`build_state`) and tests
/// (`build_state_for_tests`). The LLM client is injected so that tests can use
/// `MockClient` without needing ADC credentials.
async fn build_state_with_llm(
    cfg: ServerConfig,
    jwt_keys: JwtKeys,
    llm: Arc<dyn LlmClient>,
) -> anyhow::Result<AppState> {
    let pool = llm_memory_storage::pool::init_pool(&cfg.database_url).await?;
    // Metrics は worker と /metrics ハンドラの両方で同じインスタンスを共有する。
    let metrics = Arc::new(Metrics::new());
    let deps = Arc::new(WorkerDeps {
        pool: pool.clone(),
        state: StateMap::new(),
        llm,
        model_extract: cfg.model_extract.clone(),
        model_synth: cfg.model_synth.clone(),
        metrics: metrics.clone() as Arc<dyn llm_memory_coordinator::metrics::MetricsSink>,
    });
    let coordinator = Coordinator::new(deps);
    Ok(AppState {
        pool,
        coordinator,
        jwt_keys,
        cfg: Arc::new(cfg),
        rate_limiter: Arc::new(crate::rate_limit::RateLimiter::new()),
        metrics,
    })
}

/// Convenience wrapper for tests: injects deterministic `JwtKeys` + `MockClient`.
/// 単体テスト用 (ADC 不要、外部 API 呼ばない)。
#[doc(hidden)]
pub async fn build_state_for_tests(cfg: ServerConfig) -> anyhow::Result<AppState> {
    let llm: Arc<dyn LlmClient> = Arc::new(MockClient::new());
    build_state_with_llm(cfg, JwtKeys::for_tests(), llm).await
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

    // /mcp and /v1/account require auth; /healthz does not.
    // AuthState は JWT 鍵に加えて users 表参照用の pool を持つ
    // (削除済み user の token を弾くため毎リクエストで users::find_by_id を引く)。
    let auth_state =
        llm_memory_auth::middleware::AuthState::new(state.jwt_keys.clone(), state.pool.clone());
    let protected_router = Router::new()
        .route("/mcp", axum::routing::post(crate::mcp::transport::handle))
        .route(
            "/v1/account",
            axum::routing::delete(crate::account::delete_me),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            auth_state,
            llm_memory_auth::middleware::require_auth,
        ))
        .with_state(state.clone());

    Router::new()
        .merge(as_router)
        .merge(protected_router)
        .route("/healthz", get(healthz))
        .route("/metrics", axum::routing::get(crate::metrics::handler))
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

    async fn test_state() -> AppState {
        let cfg = ServerConfig {
            database_url: "sqlite::memory:".into(),
            bind_addr: "0.0.0.0:8080".into(),
            public_url: "https://test.example.com".into(),
            google_client_id: "id".into(),
            google_client_secret: "s".into(),
            vertex_project: "test-project".into(),
            vertex_location: "us-central1".into(),
            model_extract: "h".into(),
            model_synth: "s".into(),
            trusted_proxy_count: 1,
        };
        build_state_for_tests(cfg).await.unwrap()
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
}
