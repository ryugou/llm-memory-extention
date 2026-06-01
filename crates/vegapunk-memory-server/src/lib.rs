//! vegapunk-memory-server: vegapunk gRPC を backend とする MCP wrapper。
//!
//! 役割:
//! - Claude.ai (もしくは他 MCP client) から OAuth 経由で接続を受ける
//! - JWT を verify して user を特定、user.vegapunk_schema を取得
//! - MCP tool call を vegapunk gRPC に forward
//! - request の `schema` 引数は wrapper が user の schema に強制注入 (cross-tenant 防止)
//!
//! 設計詳細は repo 内 `docs/superpowers/specs/vegapunk-memory-server.md` 参照。

pub mod config;
pub mod state;
