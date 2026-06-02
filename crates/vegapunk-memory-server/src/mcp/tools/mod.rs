//! MCP tool 一覧と dispatcher。
//!
//! `list()` で返す inputSchema は MCP client (Claude.ai 等) が tools/list で
//! 取得し、tool call 時の引数 validation に使う。各 handler の実装は次 PR で
//! 順次追加する想定で、本 PR ではすべて `not_implemented` を返す薄い dispatcher。

use serde_json::{Value, json};

use crate::mcp::transport::JsonRpcResponse;
use crate::state::AppState;
use vegapunk_memory_auth::middleware::AuthenticatedUser;

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
                    "query": {"type": "string"},
                    "mode": {"type": "string", "enum": ["local", "global", "hybrid"], "default": "hybrid"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 10}
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

/// `tools/call` dispatcher。本 PR では各 handler 未実装のため、すべて
/// `not_implemented` の MCP tool error (= JSON-RPC 自体は success、
/// `isError: true` の content を返す) を返す。
pub async fn call(
    _state: AppState,
    _user: AuthenticatedUser,
    id: Option<Value>,
    params: Value,
) -> JsonRpcResponse {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return JsonRpcResponse::error(id, -32602, "Invalid params: missing 'name'");
        }
    };
    if !TOOL_NAMES.contains(&name.as_str()) {
        return JsonRpcResponse::error(
            id,
            -32602,
            format!("Invalid params: unknown tool '{name}'"),
        );
    }
    JsonRpcResponse::success(
        id,
        json!({
            "content": [{
                "type": "text",
                "text": format!("tool '{name}' is registered but not yet implemented in this PR")
            }],
            "isError": true
        }),
    )
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
