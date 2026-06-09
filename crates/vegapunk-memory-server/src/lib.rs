//! vegapunk-memory-server: vegapunk gRPC を backend とする MCP wrapper。
//!
//! 役割:
//! - Claude.ai (もしくは他 MCP client) から OAuth 経由で接続を受ける
//!   (`vegapunk_memory_auth` 経由の Authorization Server を mount)
//! - JWT を verify して user を特定、user.vegapunk_schema を取得 (`require_auth`)
//! - MCP tool call (`tools/list` / `tools/call`) を vegapunk gRPC に forward
//! - request の `schema` 引数は wrapper が user の schema に強制注入 (cross-tenant 防止)
//! - LLM (Gemini Flash) で typo / 異字体を canonical name に canonicalize して
//!   ingest 経路に流す (PR #21 word-boundary scan の後段、PR #26 で追加)
//!
//! モジュール構成:
//! - `app`: axum router 構築 + AppState 組み立て (OAuth router + protected /mcp)
//! - `config`: 起動時 env var → `ServerConfig` 変換
//! - `mcp::transport`: MCP Streamable HTTP transport handler (`POST /mcp`)
//! - `mcp::tools`: tools/list と tools/call 個別 handler (search / ingest /
//!   ingest_raw / query_nodes / list_schemas / stats / get_traceable_chain /
//!   feedback / get_job_status / get_schema)
//! - `canonicalize`: Gemini Flash 呼び出し + prompt 構築
//! - `schema_provisioner`: 初回 user の vegapunk schema 自動作成
//! - `ingest_serializer`: 同一 schema への ingest を直列化する mutex queue
//!
//! 詳細仕様 doc は `docs/superpowers/specs/` 配下を参照。

pub mod app;
pub mod canonicalize;
pub mod config;
pub mod ingest_serializer;
pub mod mcp;
pub mod schema_provisioner;
pub mod state;

#[cfg(test)]
mod test_support;
