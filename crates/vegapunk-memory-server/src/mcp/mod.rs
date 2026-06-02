//! MCP (Model Context Protocol) wrapper layer.
//!
//! Streamable HTTP transport を `/mcp` で公開し、JSON-RPC 2.0 経由で
//! vegapunk gRPC の subset を tool として expose する。
//! - `transport`: JSON-RPC envelope と method dispatch
//! - `tools`: tool 一覧 (`tools/list`) と handler dispatch (`tools/call`)
//!
//! 本 PR では transport の骨組み + `tools/list` のみ実装。各 tool handler
//! は次 PR で追加する (`tools/call` は当面 `not_implemented` を返す)。

pub mod tools;
pub mod transport;
