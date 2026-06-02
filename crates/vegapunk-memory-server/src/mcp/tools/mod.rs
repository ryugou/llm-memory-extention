//! MCP tool 一覧と dispatcher。
//!
//! `list()` は MCP client (Claude.ai 等) が `tools/list` で取得する inputSchema を返す。
//! `call()` は `tools/call` を受けて個別 handler に dispatch する — 各 handler は
//! `handlers` 子モジュールで実装する。未実装の tool は `tools/list` には載るが
//! `call()` 経由で叩かれた場合 `isError: true` の "not yet implemented" content を返す
//! (= client が capability discover した後で早めに気付ける)。

use serde_json::{Value, json};

use crate::mcp::transport::JsonRpcResponse;
use crate::state::AppState;
use vegapunk_memory_auth::middleware::AuthenticatedUser;

mod handlers;

// `search` tool の入出力契約。`tool_descriptor("search")` の inputSchema と
// `handlers::build_search_request` の runtime validation が同じ値を見るために
// ここを single source of truth にする。
pub(super) const SEARCH_LIMIT_MIN: i64 = 1;
pub(super) const SEARCH_LIMIT_MAX: i64 = 100;
pub(super) const SEARCH_LIMIT_DEFAULT: i32 = 10;
pub(super) const SEARCH_VALID_MODES: &[&str] = &["local", "global", "hybrid"];
pub(super) const SEARCH_MODE_DEFAULT: &str = "hybrid";

/// vegapunk wrapper として公開する tool 名の集合。
/// 「公開しない vegapunk RPC」(= UpsertNodes / Reingest / Rebuild / Migrate /
/// PurgeRawMessages / SetMaintenanceMode 等の admin) は意図的に外す。
const TOOL_NAMES: &[&str] = &[
    "search",
    "ingest",
    "ingest_raw",
    "query_nodes",
    "get_schema",
    "list_schemas",
    "stats",
    "feedback",
    "get_job_status",
    "get_traceable_chain",
];

/// `tools/list` の戻り値を組み立てる。
///
/// inputSchema は MCP 仕様 (https://spec.modelcontextprotocol.io/) に従い
/// JSON Schema (`$schema` は省略、`type: "object"` + `properties` で書く)。
/// `schema` 引数はサーバ側 (= wrapper) が認証 user の vegapunk_schema を
/// 強制注入するため、client からは受け取らない (= properties に出さない)。
pub fn list(id: Option<Value>) -> JsonRpcResponse {
    let tools: Vec<Value> = TOOL_NAMES
        .iter()
        .map(|name| tool_descriptor(name))
        .collect();
    JsonRpcResponse::success(id, json!({ "tools": tools }))
}

fn tool_descriptor(name: &str) -> Value {
    match name {
        "search" => json!({
            "name": "search",
            "description": "Search the knowledge graph for relevant information (vegapunk Search RPC, local/global/hybrid mode).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    // query must contain at least one non-whitespace character;
                    // server-side `build_search_request` trims and rejects
                    // whitespace-only input. minLength alone would still allow
                    // " ", so the pattern guard is what enforces non-empty.
                    "query": {
                        "type": "string",
                        "minLength": 1,
                        "pattern": "\\S",
                        "description": "Search query. Must contain at least one non-whitespace character."
                    },
                    "mode": {
                        "type": "string",
                        "enum": SEARCH_VALID_MODES,
                        "default": SEARCH_MODE_DEFAULT,
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": SEARCH_LIMIT_MIN,
                        "maximum": SEARCH_LIMIT_MAX,
                        "default": SEARCH_LIMIT_DEFAULT,
                    }
                },
                "required": ["query"]
            }
        }),
        "ingest" => json!({
            "name": "ingest",
            "description": "Ingest structured messages into the knowledge graph (vegapunk Ingest RPC).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "messages": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string"},
                                "body": {"type": "string"},
                                "tags": {"type": "array", "items": {"type": "string"}}
                            },
                            "required": ["body"]
                        }
                    }
                },
                "required": ["messages"]
            }
        }),
        "ingest_raw" => json!({
            "name": "ingest_raw",
            "description": "Ingest raw text into the knowledge graph with automatic chunking (vegapunk IngestRaw RPC).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {"type": "string"},
                    "title": {"type": "string"},
                    "tags": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["text"]
            }
        }),
        "query_nodes" => json!({
            "name": "query_nodes",
            "description": "List nodes by type with attribute filters (vegapunk QueryNodes RPC).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_type": {"type": "string"},
                    "filters": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "key": {"type": "string"},
                                "op": {"type": "string", "enum": ["eq", "gt", "gte", "lt", "lte"]},
                                "value": {"type": "string"}
                            },
                            "required": ["key", "op", "value"]
                        }
                    },
                    "sort_by": {"type": "string"},
                    "sort_order": {"type": "string", "enum": ["asc", "desc"], "default": "desc"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 50},
                    "offset": {"type": "integer", "minimum": 0, "default": 0}
                },
                "required": ["node_type"]
            }
        }),
        "get_schema" => json!({
            "name": "get_schema",
            "description": "Get the active schema definition for the authenticated user (vegapunk GetSchema RPC). The schema name is injected server-side from the user's tenant, so this tool takes no arguments.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        "list_schemas" => json!({
            "name": "list_schemas",
            "description": "List all available schemas (vegapunk ListSchemas RPC).",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        "stats" => json!({
            "name": "stats",
            "description": "Get knowledge graph statistics (vegapunk GetStats RPC).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_type": {"type": "string"}
                }
            }
        }),
        "feedback" => json!({
            "name": "feedback",
            "description": "Submit feedback (1-5 rating) for a previous search result (vegapunk Feedback RPC).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "search_id": {"type": "string"},
                    "rating": {"type": "integer", "minimum": 1, "maximum": 5},
                    "note": {"type": "string"}
                },
                "required": ["search_id", "rating"]
            }
        }),
        "get_job_status" => json!({
            "name": "get_job_status",
            "description": "Get the status of an ingest job by msg_id or job_id (vegapunk GetJobStatus RPC). At least one of msg_id / job_id is required.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "msg_id": {"type": "string"},
                    "job_id": {"type": "string"}
                },
                "anyOf": [
                    {"required": ["msg_id"]},
                    {"required": ["job_id"]}
                ]
            }
        }),
        "get_traceable_chain" => json!({
            "name": "get_traceable_chain",
            "description": "Get a traceable chain of provenance from a node back to its source (vegapunk GetTraceableChain RPC).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_id": {"type": "string"}
                },
                "required": ["node_id"]
            }
        }),
        _ => json!({
            "name": name,
            "description": "(unknown tool)",
            "inputSchema": { "type": "object", "properties": {} }
        }),
    }
}

/// `tools/call` dispatcher。
///
/// JSON-RPC 上は常に `success` で応答し、tool 失敗は MCP の `isError: true` content
/// として返す (MCP spec: tool execution failure ≠ protocol error)。
/// dispatch できない場合 (= name 欠落 / 未知 tool 名) のみ JSON-RPC `-32602` を返す。
pub async fn call(
    state: AppState,
    user: AuthenticatedUser,
    id: Option<Value>,
    params: Value,
) -> JsonRpcResponse {
    // MCP `tools/call` params must be a JSON object. Calling `.get("name")` on
    // an array / string / null would silently return `None` and surface as a
    // misleading "missing 'name'" error.
    if !params.is_object() {
        return JsonRpcResponse::error(id, -32602, "Invalid params: must be a JSON object");
    }
    let name = match params.get("name") {
        None | Some(Value::Null) => {
            return JsonRpcResponse::error(id, -32602, "Invalid params: missing 'name'");
        }
        Some(v) => match v.as_str() {
            None => {
                return JsonRpcResponse::error(
                    id,
                    -32602,
                    "Invalid params: 'name' must be a string",
                );
            }
            Some("") => {
                return JsonRpcResponse::error(
                    id,
                    -32602,
                    "Invalid params: 'name' must not be empty",
                );
            }
            Some(s) => s.to_string(),
        },
    };
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let content = match name.as_str() {
        "search" => handlers::search(&state, &user, args).await,
        "get_schema" => handlers::get_schema(&state, &user).await,
        other if TOOL_NAMES.contains(&other) => not_implemented_content(other),
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                format!("Invalid params: unknown tool '{name}'"),
            );
        }
    };
    JsonRpcResponse::success(id, content)
}

fn not_implemented_content(name: &str) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": format!("tool '{name}' is registered but not yet implemented"),
        }],
        "isError": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_descriptor_has_inputschema_for_each_tool() {
        let resp = list(Some(json!(1)));
        let v = serde_json::to_value(resp).unwrap();
        let tools = v["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), TOOL_NAMES.len());
        for t in tools {
            assert!(t["name"].is_string());
            assert!(t["description"].is_string());
            assert!(t["inputSchema"].is_object());
        }
    }

    #[test]
    fn list_does_not_advertise_schema_argument() {
        // wrapper が user.vegapunk_schema を強制注入するため、tools/list の
        // inputSchema.properties に "schema" を露出しないことを保証する。
        let resp = list(Some(json!(1)));
        let v = serde_json::to_value(resp).unwrap();
        for t in v["result"]["tools"].as_array().unwrap() {
            let properties = &t["inputSchema"]["properties"];
            assert!(
                properties.get("schema").is_none(),
                "tool {:?} must not expose 'schema' to clients: {:?}",
                t["name"],
                properties
            );
        }
    }

    use crate::test_support::{test_state, test_user};

    #[tokio::test]
    async fn call_missing_name_returns_minus_32602() {
        let state = test_state().await;
        let params = json!({ "arguments": {} });
        let resp = call(state, test_user(), Some(json!(1)), params).await;
        let v = serde_json::to_value(resp).unwrap();
        assert_eq!(v["error"]["code"], -32602);
        assert!(
            v["error"]["message"].as_str().unwrap().contains("missing"),
            "got: {}",
            v["error"]["message"]
        );
    }

    #[tokio::test]
    async fn call_non_string_name_returns_type_error() {
        // `{ "name": 1 }` を「missing 'name'」と返すと client が debug できない。
        // 型エラーを区別する。
        let state = test_state().await;
        let params = json!({ "name": 1, "arguments": {} });
        let resp = call(state, test_user(), Some(json!(1)), params).await;
        let v = serde_json::to_value(resp).unwrap();
        assert_eq!(v["error"]["code"], -32602);
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains("'name'") && msg.contains("string"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn call_non_object_params_returns_type_error() {
        // `params` が array / string / null だと `.get("name")` が None になり
        // 「missing 'name'」と返ってしまうので、ここで弾く。
        for bad in [json!(["search"]), json!("search"), json!(null), json!(42)] {
            let state = test_state().await;
            let resp = call(state, test_user(), Some(json!(1)), bad.clone()).await;
            let v = serde_json::to_value(resp).unwrap();
            assert_eq!(v["error"]["code"], -32602, "input {bad:?} should be -32602");
            let msg = v["error"]["message"].as_str().unwrap();
            assert!(
                msg.contains("JSON object"),
                "input {bad:?} expected shape error; got: {msg}"
            );
        }
    }

    #[tokio::test]
    async fn call_empty_string_name_returns_error() {
        let state = test_state().await;
        let params = json!({ "name": "", "arguments": {} });
        let resp = call(state, test_user(), Some(json!(1)), params).await;
        let v = serde_json::to_value(resp).unwrap();
        assert_eq!(v["error"]["code"], -32602);
        assert!(
            v["error"]["message"].as_str().unwrap().contains("empty"),
            "got: {}",
            v["error"]["message"]
        );
    }

    #[tokio::test]
    async fn call_unknown_tool_returns_minus_32602() {
        let state = test_state().await;
        let params = json!({ "name": "no_such_tool", "arguments": {} });
        let resp = call(state, test_user(), Some(json!(1)), params).await;
        let v = serde_json::to_value(resp).unwrap();
        assert_eq!(v["error"]["code"], -32602);
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no_such_tool")
        );
    }

    #[tokio::test]
    async fn call_registered_but_unimplemented_tool_returns_iserror_content() {
        // tools/list には載っているが本 PR ではまだ実装が無い tool は、
        // JSON-RPC では success、tool 側で `isError: true` を返す。
        // 「ingest」を例にとる (PR #16 で実装予定)。
        let state = test_state().await;
        let params = json!({ "name": "ingest", "arguments": {} });
        let resp = call(state, test_user(), Some(json!(1)), params).await;
        let v = serde_json::to_value(resp).unwrap();
        assert!(
            v["error"].is_null(),
            "should be a tool error, not JSON-RPC error: {v}"
        );
        assert_eq!(v["result"]["isError"], true);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("ingest") && text.contains("not yet implemented"));
    }

    #[test]
    fn get_job_status_requires_at_least_one_identifier() {
        // msg_id / job_id どちらか必須を anyOf で表現していることを保証。
        // 「{} で通る」抜け穴を防ぐ。
        let v = tool_descriptor("get_job_status");
        let any_of = v["inputSchema"]["anyOf"].as_array().unwrap();
        let required_sets: Vec<Vec<String>> = any_of
            .iter()
            .map(|branch| {
                branch["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_str().unwrap().to_string())
                    .collect()
            })
            .collect();
        assert!(required_sets.contains(&vec!["msg_id".to_string()]));
        assert!(required_sets.contains(&vec!["job_id".to_string()]));
    }
}
