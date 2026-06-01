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
///
/// `Debug` は `MetadataValue` 経由で token の中身まで露出するため、手書きで
/// redaction する。`Clone` は tonic の Interceptor trait で複製される経路があるため必要。
#[derive(Clone)]
pub struct BearerAuthInterceptor {
    token: MetadataValue<tonic::metadata::Ascii>,
}

impl std::fmt::Debug for BearerAuthInterceptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BearerAuthInterceptor")
            .field("token", &"<redacted>")
            .finish()
    }
}

impl BearerAuthInterceptor {
    pub fn new(bearer_token: &str) -> Result<Self, BearerTokenError> {
        // 空文字列 / whitespace-only を許すと `authorization: Bearer ` (= 空値) を
        // 送ってしまい、vegapunk 側で「auth header はあるがフォーマット不正」の
        // 紛らわしい失敗を引き起こす。fail-fast で起動時に弾く。
        if bearer_token.trim().is_empty() {
            return Err(BearerTokenError::Empty);
        }
        let raw = format!("Bearer {bearer_token}");
        // tonic の MetadataValue<Ascii> ≈ http::HeaderValue で、RFC 7230 の
        // field-vchar (VCHAR + obs-text 0x80-0xFF) は通すが、CR/LF/NUL 等の
        // 制御文字は弾かれる。UTF-8 multibyte は obs-text として通る点に注意。
        let token = raw
            .parse()
            .map_err(|_| BearerTokenError::InvalidHeaderChars)?;
        Ok(Self { token })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BearerTokenError {
    #[error("bearer token is empty or whitespace-only")]
    Empty,
    #[error("bearer token contains characters that are not valid in an HTTP header value")]
    InvalidHeaderChars,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_bearer_token() {
        let err = BearerAuthInterceptor::new("").unwrap_err();
        assert!(matches!(err, BearerTokenError::Empty), "got {err:?}");
    }

    #[test]
    fn rejects_whitespace_only_bearer_token() {
        let err = BearerAuthInterceptor::new("   \t\n").unwrap_err();
        assert!(matches!(err, BearerTokenError::Empty), "got {err:?}");
    }

    #[test]
    fn rejects_token_with_control_chars() {
        // CR/LF は header value に含められないので fail-fast する
        let err = BearerAuthInterceptor::new("bad\r\ntoken").unwrap_err();
        assert!(
            matches!(err, BearerTokenError::InvalidHeaderChars),
            "got {err:?}"
        );
    }

    #[test]
    fn accepts_valid_bearer_token() {
        BearerAuthInterceptor::new("abc123").expect("valid token");
    }
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
