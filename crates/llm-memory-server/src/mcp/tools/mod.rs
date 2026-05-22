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

fn tool_meta(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema
    })
}

/// Returns `true` iff `name` is a tool this server can dispatch. Kept in sync
/// with the match arm in [`call`] and the schema list in [`list`].
fn is_known_tool(name: &str) -> bool {
    matches!(
        name,
        "raw_append"
            | "raw_read"
            | "raw_search"
            | "wiki_read"
            | "wiki_list"
            | "wiki_rebuild"
            | "schema_read"
            | "schema_update"
            | "export"
    )
}

/// `tools/list` — enumerate tools with JSON-Schema `inputSchema`.
pub async fn list(id: Option<Value>) -> Json<JsonRpcResponse> {
    let tools = json!([
        tool_meta(
            "raw_append",
            "Append a personal raw",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "content": {"type": "string"},
                    "source": {"type": "string"},
                    "tags": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["title", "content", "source"]
            })
        ),
        tool_meta(
            "raw_read",
            "Read a single raw",
            json!({
                "type": "object",
                "properties": { "id": {"type": "string"} },
                "required": ["id"]
            })
        ),
        tool_meta(
            "raw_search",
            "Search raws via FTS5",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "scope": {"type": "string", "enum": ["all", "personal", "shared"]},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100}
                },
                "required": ["query"]
            })
        ),
        tool_meta(
            "wiki_read",
            "Read concept wiki across personal+shared",
            json!({
                "type": "object",
                "properties": {
                    "concept": {"type": "string"},
                    "scope": {"type": "string", "enum": ["all", "personal", "shared"]}
                },
                "required": ["concept"]
            })
        ),
        tool_meta(
            "wiki_list",
            "List concepts",
            json!({
                "type": "object",
                "properties": {
                    "scope": {"type": "string", "enum": ["all", "personal", "shared"]}
                }
            })
        ),
        tool_meta(
            "wiki_rebuild",
            "Manually trigger rebuild",
            json!({
                "type": "object",
                "properties": {
                    "concept": {"type": "string"}
                }
            })
        ),
        tool_meta(
            "schema_read",
            "Read schema",
            json!({
                "type": "object",
                "properties": {
                    "scope": {"type": "string", "enum": ["personal", "shared"]},
                    "shared_memory_id": {"type": "string"}
                },
                "required": ["scope"],
                // `shared_memory_id` is required iff scope == "shared".
                "oneOf": [
                    {
                        "properties": { "scope": { "const": "personal" } }
                    },
                    {
                        "properties": { "scope": { "const": "shared" } },
                        "required": ["shared_memory_id"]
                    }
                ]
            })
        ),
        tool_meta(
            "schema_update",
            "Update personal schema",
            json!({
                "type": "object",
                "properties": {
                    "content": {"type": "string"}
                },
                "required": ["content"]
            })
        ),
        tool_meta(
            "export",
            "Export personal data",
            json!({
                "type": "object",
                "properties": {
                    "cursor": {"type": "string"}
                }
            })
        ),
    ]);
    Json(JsonRpcResponse::success(id, json!({ "tools": tools })))
}

/// Wrap a tool's success value into the MCP `CallToolResult` shape.
fn call_tool_result_ok(value: &Value) -> Value {
    let text = serde_json::to_string(value).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "tool result serialization failed; emitting null");
        "null".into()
    });
    json!({
        "content": [
            { "type": "text", "text": text }
        ],
        "isError": false
    })
}

/// Wrap a tool failure into the MCP `CallToolResult` shape. The message is
/// intentionally stable/generic; details are written to logs only.
fn call_tool_result_err(message: impl Into<String>) -> Value {
    json!({
        "content": [
            { "type": "text", "text": message.into() }
        ],
        "isError": true
    })
}

/// `tools/call` — invoke a tool by name. Returns a `CallToolResult` for both
/// success and tool-level failure. Returns a JSON-RPC error only for
/// protocol-level problems (unknown tool name).
pub async fn call(
    state: AppState,
    user: AuthenticatedUser,
    id: Option<Value>,
    params: Value,
) -> Json<JsonRpcResponse> {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    // Reject unknown tool names at the protocol layer *before* consuming a
    // rate-limit token. Otherwise a throttled bucket would mask the real
    // error and return `rate_limited` for a nonexistent tool.
    if !is_known_tool(name) {
        return Json(JsonRpcResponse::error(
            id,
            -32602,
            format!("unknown tool: {name}"),
        ));
    }

    let tier = crate::rate_limit::tier_of(name);
    if !state.rate_limiter.check(&user.user_id, tier) {
        // Rate-limit failures are surfaced as tool-level errors so MCP clients
        // can render them inline rather than treating them as protocol faults.
        return Json(JsonRpcResponse::success(
            id,
            call_tool_result_err(format!("rate_limited: {} tier", tier.name)),
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
        // Unreachable: `is_known_tool` above gates this match.
        _ => unreachable!("unknown tool slipped past is_known_tool guard: {name}"),
    };
    let call_result = match result {
        Ok(v) => call_tool_result_ok(&v),
        Err(e) => {
            tracing::warn!(tool = %name, error = %e, "tool call failed");
            // Surface a stable, generic message; keep details in the log.
            call_tool_result_err(format!("tool '{name}' failed"))
        }
    };
    Json(JsonRpcResponse::success(id, call_result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::build_state_for_tests;
    use crate::config::ServerConfig;

    async fn state() -> AppState {
        let cfg = ServerConfig {
            database_url: "sqlite::memory:".into(),
            bind_addr: "0.0.0.0:8080".into(),
            public_url: "https://test.example.com".into(),
            google_client_id: "x".into(),
            google_client_secret: "x".into(),
            vertex_project: "test-project".into(),
            vertex_location: "us-central1".into(),
            model_extract: "h".into(),
            model_synth: "s".into(),
            trusted_proxy_count: 1,
        };
        build_state_for_tests(cfg).await.unwrap()
    }

    fn user() -> AuthenticatedUser {
        AuthenticatedUser {
            user_id: "u1".into(),
            client_id: "c".into(),
        }
    }

    #[tokio::test]
    async fn list_emits_input_schema_per_tool() {
        let r = list(Some(Value::from(1))).await;
        let body = serde_json::to_value(&r.0).unwrap();
        let tools = body["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 9);
        for t in tools {
            assert!(t["name"].is_string());
            assert!(t["description"].is_string());
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[tokio::test]
    async fn unknown_tool_returns_jsonrpc_error() {
        let s = state().await;
        let r = call(s, user(), None, json!({"name": "nope", "arguments": {}})).await;
        let body = serde_json::to_value(&r.0).unwrap();
        assert!(body["error"].is_object());
        assert_eq!(body["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn unknown_tool_takes_priority_over_rate_limit() {
        // Drain the read-tier bucket for u1, then call an unknown tool. The
        // dispatcher must return `-32602 unknown tool`, NOT a tool-level
        // `rate_limited` envelope, because rate-limiting an imaginary tool
        // hides the real (and stable) contract violation from the client.
        let s = state().await;
        // Drain the read-tier bucket.
        for _ in 0..crate::rate_limit::tier_of("raw_read").per_minute {
            s.rate_limiter
                .check(&user().user_id, crate::rate_limit::tier_of("raw_read"));
        }
        let r = call(s, user(), None, json!({"name": "nope", "arguments": {}})).await;
        let body = serde_json::to_value(&r.0).unwrap();
        assert_eq!(body["error"]["code"], -32602);
        assert!(body["result"].is_null());
    }

    #[tokio::test]
    async fn tool_error_is_wrapped_in_calltoolresult() {
        let s = state().await;
        // raw_append with empty title/content → tool error inside CallToolResult.
        let r = call(
            s,
            user(),
            None,
            json!({"name": "raw_append", "arguments": {"title": "", "content": "", "source": "m"}}),
        )
        .await;
        let body = serde_json::to_value(&r.0).unwrap();
        assert!(body["error"].is_null());
        assert_eq!(body["result"]["isError"], true);
        let text = body["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        assert!(text.contains("failed"));
    }

    #[tokio::test]
    async fn wiki_read_invalid_scope_is_tool_error() {
        // 未知 scope (e.g., "unknown_scope") は silent empty ではなく isError=true で返す。
        let s = state().await;
        let r = call(
            s,
            user(),
            None,
            json!({"name": "wiki_read", "arguments": {"concept": "foo", "scope": "unknown_scope"}}),
        )
        .await;
        let body = serde_json::to_value(&r.0).unwrap();
        assert!(body["error"].is_null());
        assert_eq!(body["result"]["isError"], true);
    }

    #[tokio::test]
    async fn wiki_list_invalid_scope_is_tool_error() {
        let s = state().await;
        let r = call(
            s,
            user(),
            None,
            json!({"name": "wiki_list", "arguments": {"scope": "unknown_scope"}}),
        )
        .await;
        let body = serde_json::to_value(&r.0).unwrap();
        assert!(body["error"].is_null());
        assert_eq!(body["result"]["isError"], true);
    }

    #[tokio::test]
    async fn tool_success_is_wrapped_in_calltoolresult() {
        let s = state().await;
        let r = call(
            s,
            user(),
            None,
            json!({"name": "raw_append", "arguments": {"title": "t", "content": "c", "source": "manual"}}),
        )
        .await;
        let body = serde_json::to_value(&r.0).unwrap();
        assert_eq!(body["result"]["isError"], false);
        let text = body["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        // The inner text is the JSON-serialized tool output (raw_id etc.).
        let inner: Value = serde_json::from_str(text).expect("inner JSON");
        assert!(inner["raw_id"].is_string());
    }
}
