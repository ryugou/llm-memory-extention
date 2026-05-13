use anyhow::Result;
use axum::Json;
use serde_json::{Value, json};

use crate::app::AppState;
use crate::mcp::transport::JsonRpcResponse;
use llm_memory_auth::middleware::AuthenticatedUser;

pub mod export;
pub mod raw_append;
pub mod raw_read;
pub mod raw_search;
pub mod schema_read;
pub mod schema_update;
pub mod wiki_list;
pub mod wiki_read;
pub mod wiki_rebuild;

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

pub async fn call(
    state: AppState,
    user: AuthenticatedUser,
    id: Option<Value>,
    params: Value,
) -> Json<JsonRpcResponse> {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let tier = crate::rate_limit::tier_of(name);
    if !state.rate_limiter.check(&user.user_id, tier) {
        return Json(JsonRpcResponse::error(
            id,
            -32000,
            format!("rate_limited: {} tier", tier.name),
        ));
    }
    let result: Result<Value> = match name {
        "raw_append" => raw_append::call(state, user, args).await,
        "raw_read" => raw_read::call(state, user, args).await,
        "raw_search" => raw_search::call(state, user, args).await,
        "wiki_read" => wiki_read::call(state, user, args).await,
        "wiki_list" => wiki_list::call(state, user, args).await,
        "wiki_rebuild" => wiki_rebuild::call(state, user, args).await,
        "schema_read" => schema_read::call(state, user, args).await,
        "schema_update" => schema_update::call(state, user, args).await,
        "export" => export::call(state, user, args).await,
        _ => {
            return Json(JsonRpcResponse::error(
                id,
                -32602,
                format!("unknown tool: {name}"),
            ));
        }
    };
    match result {
        Ok(v) => Json(JsonRpcResponse::success(id, v)),
        Err(e) => Json(JsonRpcResponse::error(id, -32603, e.to_string())),
    }
}
