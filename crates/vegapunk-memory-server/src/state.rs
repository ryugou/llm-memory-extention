//! 共有 state。axum handler から `State<AppState>` で取り回す。
//!
//! - `pool`: vegapunk-memory-storage の SQLite (users / oauth_clients / tokens
//!   / shared_memories)。データ本体 (raws / wikis) は vegapunk gRPC が持つ。
//! - `vegapunk`: vegapunk gRPC client (Bearer 自動付与 interceptor 込み)。
//! - `jwt_keys`: 自前発行 JWT の signing/verify 鍵。
//! - `cfg`: 起動時 env から組み立てた immutable な設定 (`PUBLIC_URL` 等)。
//!
//! MCP tool handler は次 PR で追加するので、本 PR では axum router の root
//! (= healthz / OAuth router) と auth middleware の wiring が中心。

use std::sync::Arc;

use sqlx::SqlitePool;
use vegapunk_client::GraphRagClient;
use vegapunk_memory_auth::jwt::JwtKeys;

use crate::config::ServerConfig;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub vegapunk: GraphRagClient,
    pub jwt_keys: JwtKeys,
    pub cfg: Arc<ServerConfig>,
}
