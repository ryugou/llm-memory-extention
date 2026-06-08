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

use crate::canonicalize::GeminiCanonicalizer;
use crate::config::ServerConfig;
use crate::ingest_serializer::IngestSerializer;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub vegapunk: GraphRagClient,
    pub jwt_keys: JwtKeys,
    pub cfg: Arc<ServerConfig>,
    /// 同一 schema に対する `ingest` / `ingest_raw` を直列化する mutex pool。
    /// PR #21 dedup catalogue + PR #24 sync-wait は **逐次 ingest 前提** で
    /// 設計されており、並列 ingest が race で互いの entity を見逃して dedup
    /// が空振りする問題がある。本 lock で同 schema 内の ingest を順番に並べる。
    pub ingest_serializer: Arc<IngestSerializer>,
    /// LLM (Gemini Flash) ベースの canonicalize クライアント。
    /// `GEMINI_API_KEY` が env で設定されていない環境では `None` で、
    /// その場合 ingest handler は PR #21 word-boundary scan 結果をそのまま
    /// vegapunk に流す (= LLM canonicalize は無効化)。
    pub canonicalizer: Option<Arc<GeminiCanonicalizer>>,
}
