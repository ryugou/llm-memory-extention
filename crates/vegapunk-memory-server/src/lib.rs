//! vegapunk-memory-server: vegapunk gRPC を backend とする MCP wrapper。
//!
//! 役割:
//! - Claude.ai (もしくは他 MCP client) から OAuth 経由で接続を受ける
//! - JWT を verify して user を特定、user.vegapunk_schema を取得
//! - MCP tool call を vegapunk gRPC に forward
//! - request の `schema` 引数は wrapper が user の schema に強制注入 (cross-tenant 防止)
//!
//! 詳細仕様 doc は別 PR で `docs/superpowers/specs/` 配下に起こす予定。
//! 本 PR は skeleton のみで、tool handler / HTTP transport は後続 PR で追加する。

pub mod app;
pub mod config;
pub mod mcp;
pub mod state;

#[cfg(test)]
mod test_support;
