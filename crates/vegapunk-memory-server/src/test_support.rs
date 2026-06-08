//! Shared test fixtures (only compiled under `cfg(test)`).
//!
//! `test_state()` と `test_user()` を crate 内の test 群で 1 箇所に集約する。
//! `ServerConfig` のフィールドが増えても、ここを 1 行直せば全 test が追随する
//! ので、複数ファイルで `Field: missing` を踏むことが無くなる。
//!
//! 実 vegapunk gRPC には繋がない: `connect_lazy` で valid な channel を作っておき、
//! Bearer interceptor を被せて `GraphRagClient` を組み立てる。handler が gRPC を
//! 叩かないテスト (= routing / dispatcher / pure validation) はこれで十分。

use std::sync::Arc;

use vegapunk_client::BearerAuthInterceptor;
use vegapunk_client::graphrag::graph_rag_engine_client::GraphRagEngineClient;
use vegapunk_memory_auth::jwt::JwtKeys;
use vegapunk_memory_auth::middleware::AuthenticatedUser;

use crate::config::ServerConfig;
use crate::state::AppState;

pub(crate) async fn test_state() -> AppState {
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
        shared_schema_name: "sivira-shared".into(),
        default_schema_template: "discussion".into(),
        gemini_api_key: None,
        gemini_model: "gemini-3.5-flash".into(),
        gemini_timeout_secs: 30,
    };
    AppState {
        pool,
        vegapunk,
        jwt_keys: JwtKeys::for_tests(),
        cfg: Arc::new(cfg),
        ingest_serializer: Arc::new(crate::ingest_serializer::IngestSerializer::new()),
        canonicalizer: None,
    }
}

pub(crate) fn test_user() -> AuthenticatedUser {
    AuthenticatedUser {
        user_id: "u1".into(),
        client_id: "c".into(),
        vegapunk_schema: "u1-schema".into(),
    }
}
