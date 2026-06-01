//! Vegapunk gRPC client wrapper.
//!
//! tonic で proto から生成した client を、固定 Bearer + base URL の
//! 接続設定込みで使いやすくする層。MCP server (vegapunk-memory-server)
//! からは `vegapunk_client::connect(endpoint, bearer_token)` 経由で
//! `GraphRagClient` を得る。`GraphRagClient` は内部で
//! `BearerAuthInterceptor` を持ち、全リクエストに
//! `authorization: Bearer <token>` を自動付与する。

// tonic_build で生成された code は proto のコメントから doc 化される。proto 側で
// 整形しきれないため clippy::doc_overindented_list_items などが発火するが、
// 生成 code に手を入れる経路がないので module 単位で抑止する。
#[allow(
    clippy::doc_overindented_list_items,
    clippy::doc_lazy_continuation,
    clippy::tabs_in_doc_comments,
    clippy::all
)]
pub mod graphrag {
    // tonic_build で生成された code を取り込む。proto の package は
    // graphrag.proto 側で `package graphrag;` 宣言されている前提。
    tonic::include_proto!("graphrag");
}

use std::time::Duration;
use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};

use graphrag::graph_rag_engine_client::GraphRagEngineClient;

/// 固定 Bearer token を全リクエストに自動付与する Interceptor 付き client。
pub type GraphRagClient = GraphRagEngineClient<InterceptedService<Channel, BearerAuthInterceptor>>;

/// 各 gRPC リクエストに `authorization: Bearer <token>` を付与する interceptor。
#[derive(Clone)]
pub struct BearerAuthInterceptor {
    token: MetadataValue<tonic::metadata::Ascii>,
}

impl BearerAuthInterceptor {
    pub fn new(bearer_token: &str) -> Result<Self, BearerTokenError> {
        let raw = format!("Bearer {bearer_token}");
        let token = raw.parse().map_err(|_| BearerTokenError::InvalidAscii)?;
        Ok(Self { token })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BearerTokenError {
    #[error("bearer token contains invalid ASCII")]
    InvalidAscii,
}

impl tonic::service::Interceptor for BearerAuthInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert("authorization", self.token.clone());
        Ok(request)
    }
}

/// vegapunk gRPC endpoint と Bearer token を受け取って Client を組み立てる。
///
/// - `endpoint`: 例 `http://vegapunk.local:6840` (dev) / `https://<cloud>` (prod)
/// - `bearer_token`: vegapunk config (`server.auth.token`) と一致する token
///
/// `connect_timeout` / `tcp_keepalive` で接続安定性の最低限を確保する。
pub async fn connect(endpoint: &str, bearer_token: &str) -> Result<GraphRagClient, ConnectError> {
    let channel = Endpoint::from_shared(endpoint.to_string())
        .map_err(|e| ConnectError::InvalidEndpoint(e.to_string()))?
        .connect_timeout(Duration::from_secs(5))
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .connect()
        .await
        .map_err(|e| ConnectError::Connect(e.to_string()))?;

    let interceptor = BearerAuthInterceptor::new(bearer_token)
        .map_err(|e| ConnectError::InvalidToken(e.to_string()))?;

    Ok(GraphRagEngineClient::with_interceptor(channel, interceptor))
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("connect failed: {0}")]
    Connect(String),
    #[error("invalid bearer token: {0}")]
    InvalidToken(String),
}
