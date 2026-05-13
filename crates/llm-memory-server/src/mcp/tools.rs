use axum::Json;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::mcp::transport::JsonRpcResponse;
use llm_memory_auth::middleware::AuthenticatedUser;

pub async fn list(id: Option<Value>) -> Json<JsonRpcResponse> {
    let tools = json!([
        { "name": "raw_append", "description": "Append a personal raw" },
        { "name": "raw_read", "description": "Read a single raw" },
        { "name": "raw_search", "description": "Search raws via FTS5" },
        { "name": "wiki_read", "description": "Read concept wiki across personal+shared" },
        { "name": "wiki_list", "description": "List concepts" },
        { "name": "wiki_rebuild", "description": "Manually trigger rebuild" },
        { "name": "schema_read", "description": "Read schema" },
        { "name": "schema_update", "description": "Update personal schema" },
        { "name": "export", "description": "Export personal data" }
    ]);
    Json(JsonRpcResponse::success(id, json!({ "tools": tools })))
}

/// Dispatch a tool call. Real handlers come in Tasks 26-28.
pub async fn call(
    _state: AppState,
    _user: AuthenticatedUser,
    id: Option<Value>,
    _params: Value,
) -> Json<JsonRpcResponse> {
    Json(JsonRpcResponse::error(id, -32601, "tool dispatch pending Tasks 26-28"))
}
