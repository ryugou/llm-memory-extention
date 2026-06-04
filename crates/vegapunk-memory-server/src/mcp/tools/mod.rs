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

// `query_nodes` tool 契約。schema と runtime validation を一致させる。
pub(super) const QUERY_NODES_LIMIT_MIN: i64 = 1;
pub(super) const QUERY_NODES_LIMIT_MAX: i64 = 1000;
pub(super) const QUERY_NODES_LIMIT_DEFAULT: i32 = 50;
pub(super) const QUERY_NODES_VALID_SORT_ORDERS: &[&str] = &["asc", "desc"];
pub(super) const QUERY_NODES_SORT_ORDER_DEFAULT: &str = "desc";

// `AttributeFilter.op` の許容セット (query_nodes / stats 共通)。
pub(super) const ATTRIBUTE_FILTER_VALID_OPS: &[&str] = &["eq", "gt", "gte", "lt", "lte"];

// `get_traceable_chain.max_depth` 契約 (proto: default 5, max 10)。
pub(super) const TRACEABLE_CHAIN_MAX_DEPTH_MIN: i64 = 1;
pub(super) const TRACEABLE_CHAIN_MAX_DEPTH_MAX: i64 = 10;
pub(super) const TRACEABLE_CHAIN_MAX_DEPTH_DEFAULT: i32 = 5;

/// vegapunk wrapper として公開する tool 名の集合。
/// 「公開しない vegapunk RPC」(= UpsertNodes / Reingest / Rebuild / Migrate /
/// PurgeRawMessages / SetMaintenanceMode 等の admin) は意図的に外す。
// NOTE: `feedback` と `get_job_status` は意図的に外している。proto 上、
// `FeedbackRequest` / `GetJobStatusRequest` には schema フィールドが無く、
// 識別子 (`search_id` / `msg_id`) だけで vegapunk に投げる API。wrapper 側で
// 「その識別子が caller の tenant のものか」を確認する仕組み (ownership
// tracking テーブル) が無い状態で advertise すると、他 tenant の id を
// 推測 / 漏洩経由で知った caller が cross-tenant でアクセスできてしまう
// (Codex P1/P2)。ownership tracking を別 PR で入れてから再 advertise する。
const TOOL_NAMES: &[&str] = &[
    "search",
    "ingest",
    "ingest_raw",
    "query_nodes",
    "get_schema",
    "list_schemas",
    "stats",
    "get_traceable_chain",
    // 低レベル決定論的 upsert。LLM 抽出に頼らず client が stable id で
    // entity を挿入/更新できる経路 (= ingest_raw が抱える重複問題の代替)。
    "upsert_nodes",
    "upsert_edges",
    "upsert_vectors",
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
            "description": "Ingest structured messages into the knowledge graph (vegapunk Ingest RPC). Each message is treated as one provenance unit (e.g. a Slack post or commit). For long-form text without per-utterance structure, prefer `ingest_raw`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "messages": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "description": "Optional client-supplied msg id. Server generates one if omitted."
                                },
                                "text": {
                                    "type": "string",
                                    "minLength": 1,
                                    "pattern": "\\S",
                                    "description": "Message body. Must contain at least one non-whitespace character."
                                },
                                "metadata": {
                                    "type": "object",
                                    "properties": {
                                        "source_type": {"type": "string", "minLength": 1, "pattern": "\\S"},
                                        "author": {"type": "string", "minLength": 1, "pattern": "\\S"},
                                        "author_id": {"type": "string"},
                                        "channel": {"type": "string", "minLength": 1, "pattern": "\\S"},
                                        "channel_id": {"type": "string"},
                                        "thread_id": {"type": "string"},
                                        "timestamp": {
                                            "type": "string",
                                            "minLength": 1,
                                            "pattern": "\\S",
                                            "description": "RFC3339 timestamp, e.g. 2026-06-02T10:00:00+09:00."
                                        }
                                    },
                                    "required": ["source_type", "author", "channel", "timestamp"]
                                }
                            },
                            "required": ["text", "metadata"]
                        }
                    }
                },
                "required": ["messages"]
            }
        }),
        "ingest_raw" => json!({
            "name": "ingest_raw",
            "description": "Ingest a single block of raw text into the knowledge graph; vegapunk chunks it and returns one msg_id per chunk (vegapunk IngestRaw RPC).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "minLength": 1,
                        "pattern": "\\S",
                        "description": "Raw text. Must contain at least one non-whitespace character."
                    },
                    "metadata": {
                        "type": "object",
                        "properties": {
                            "source_type": {"type": "string", "minLength": 1, "pattern": "\\S"},
                            "author": {"type": "string"},
                            "channel": {"type": "string"},
                            "timestamp": {
                                "type": "string",
                                "minLength": 1,
                                "pattern": "\\S",
                                "description": "RFC3339 timestamp. Server uses current time if omitted; whitespace-only is treated as omitted."
                            }
                        },
                        "required": ["source_type"]
                    }
                },
                "required": ["text", "metadata"]
            }
        }),
        "query_nodes" => json!({
            "name": "query_nodes",
            "description": "List nodes by type with optional attribute filters (vegapunk QueryNodes RPC). Filters are AND-combined.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_type": {
                        "type": "string",
                        "minLength": 1,
                        "pattern": "\\S",
                        "description": "Node type to list (e.g. \"Message\", \"Person\", \"Decision\")."
                    },
                    "filters": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "key": {"type": "string", "minLength": 1, "pattern": "\\S"},
                                "op": {"type": "string", "enum": ATTRIBUTE_FILTER_VALID_OPS},
                                "value": {"type": "string"}
                            },
                            "required": ["key", "op", "value"]
                        }
                    },
                    "sort_by": {"type": "string", "minLength": 1, "pattern": "\\S"},
                    "sort_order": {
                        "type": "string",
                        "enum": QUERY_NODES_VALID_SORT_ORDERS,
                        "default": QUERY_NODES_SORT_ORDER_DEFAULT,
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": QUERY_NODES_LIMIT_MIN,
                        "maximum": QUERY_NODES_LIMIT_MAX,
                        "default": QUERY_NODES_LIMIT_DEFAULT,
                    },
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
            "description": "List schemas visible to the authenticated user (vegapunk ListSchemas RPC, filtered server-side to the user's tenant — other tenants' schemas are never returned).",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        "stats" => json!({
            "name": "stats",
            "description": "Get knowledge graph statistics for the authenticated user's schema (vegapunk GetStats RPC). Optional node_type and filters narrow node_count to a subset.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_type": {"type": "string", "minLength": 1, "pattern": "\\S"},
                    "filters": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "key": {"type": "string", "minLength": 1, "pattern": "\\S"},
                                "op": {"type": "string", "enum": ATTRIBUTE_FILTER_VALID_OPS},
                                "value": {"type": "string"}
                            },
                            "required": ["key", "op", "value"]
                        }
                    }
                }
            }
        }),
        "get_traceable_chain" => json!({
            "name": "get_traceable_chain",
            "description": "Walk the provenance chain from a node back to its source within the authenticated user's schema (vegapunk GetTraceableChain RPC).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_id": {
                        "type": "string",
                        "minLength": 1,
                        "pattern": "\\S"
                    },
                    "max_depth": {
                        "type": "integer",
                        "minimum": TRACEABLE_CHAIN_MAX_DEPTH_MIN,
                        "maximum": TRACEABLE_CHAIN_MAX_DEPTH_MAX,
                        "default": TRACEABLE_CHAIN_MAX_DEPTH_DEFAULT,
                    }
                },
                "required": ["node_id"]
            }
        }),
        "upsert_nodes" => json!({
            "name": "upsert_nodes",
            "description": "Upsert nodes by deterministic id (vegapunk UpsertNodes RPC). Unlike `ingest`/`ingest_raw` (which rely on vegapunk's async LLM extraction and can split the same entity into multiple nodes on rapid-fire ingest), this tool lets the caller assign stable ids — re-upserting the same id updates the existing node instead of creating a duplicate. ID convention: first call `list_schemas` to obtain the authenticated user's personal schema name (the entry with `name` starting with `user-`), then build each id as `{personal_schema_name}:{local-id}` (e.g. `user-<sub>:proj-vegapunk`). The server rejects any id that does not start with the caller's personal schema prefix, and writes to the shared schema are not allowed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nodes": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 256,
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "minLength": 1,
                                    "pattern": "\\S",
                                    "description": "Stable node id formatted as `{personal_schema_name}:{local-id}`. Re-upserting the same id updates the existing node. Use `list_schemas` to discover the personal schema name; do not guess it."
                                },
                                "type": {
                                    "type": "string",
                                    "minLength": 1,
                                    "pattern": "\\S",
                                    "description": "Node type (e.g. 'Project', 'Person', 'Topic')."
                                },
                                "attributes": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "key": {"type": "string", "minLength": 1, "pattern": "\\S"},
                                            "value": {"type": "string"}
                                        },
                                        "required": ["key", "value"]
                                    },
                                    "description": "List of {key, value} attribute pairs. Both must be strings (proto constraint)."
                                }
                            },
                            "required": ["id", "type"]
                        }
                    }
                },
                "required": ["nodes"]
            }
        }),
        "upsert_edges" => json!({
            "name": "upsert_edges",
            "description": "Upsert edges between previously-known nodes (vegapunk UpsertEdges RPC). Both `from_id` and `to_id` must start with the authenticated user's personal schema prefix (use `list_schemas` to discover it; same convention as `upsert_nodes`). Re-upserting the same (from_id, to_id, type) triple updates the existing edge in place.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "edges": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 256,
                        "items": {
                            "type": "object",
                            "properties": {
                                "from_id": {
                                    "type": "string",
                                    "minLength": 1,
                                    "pattern": "\\S",
                                    "description": "Source node id formatted as `{personal_schema_name}:{local-id}`. Use `list_schemas` to discover the personal schema name."
                                },
                                "to_id": {
                                    "type": "string",
                                    "minLength": 1,
                                    "pattern": "\\S",
                                    "description": "Target node id formatted as `{personal_schema_name}:{local-id}`."
                                },
                                "type": {
                                    "type": "string",
                                    "minLength": 1,
                                    "pattern": "\\S",
                                    "description": "Edge type (e.g. 'related_to', 'authored_by')."
                                },
                                "attributes": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "key": {"type": "string", "minLength": 1, "pattern": "\\S"},
                                            "value": {"type": "string"}
                                        },
                                        "required": ["key", "value"]
                                    }
                                }
                            },
                            "required": ["from_id", "to_id", "type"]
                        }
                    }
                },
                "required": ["edges"]
            }
        }),
        "upsert_vectors" => json!({
            "name": "upsert_vectors",
            "description": "Upsert embedding vectors keyed by node id (vegapunk UpsertVectors RPC). Use vegapunk's `embed` tool (or any embedder aligned to vegapunk's dimension) to produce the float array. `id` must start with the authenticated user's personal schema prefix (use `list_schemas` to discover it; same convention as `upsert_nodes`). All vector elements must be finite (NaN / ±Inf and values outside f32 range are rejected).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "vectors": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 256,
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "minLength": 1,
                                    "pattern": "\\S",
                                    "description": "Vector id (typically the node id it embeds), formatted as `{personal_schema_name}:{local-id}`."
                                },
                                "vector": {
                                    "type": "array",
                                    "minItems": 1,
                                    "maxItems": 8192,
                                    "items": {"type": "number"},
                                    "description": "Embedding float array. Must match vegapunk's configured embedder dimension. All elements must be finite numbers."
                                },
                                "metadata": {
                                    "type": "object",
                                    "additionalProperties": {"type": "string"},
                                    "description": "Optional string→string metadata map (proto constraint)."
                                }
                            },
                            "required": ["id", "vector"]
                        }
                    }
                },
                "required": ["vectors"]
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
        "ingest" => handlers::ingest(&state, &user, args).await,
        "ingest_raw" => handlers::ingest_raw(&state, &user, args).await,
        "query_nodes" => handlers::query_nodes(&state, &user, args).await,
        "list_schemas" => handlers::list_schemas(&state, &user).await,
        "stats" => handlers::stats(&state, &user, args).await,
        "get_traceable_chain" => handlers::get_traceable_chain(&state, &user, args).await,
        "upsert_nodes" => handlers::upsert_nodes(&state, &user, args).await,
        "upsert_edges" => handlers::upsert_edges(&state, &user, args).await,
        "upsert_vectors" => handlers::upsert_vectors(&state, &user, args).await,
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
    async fn call_routes_query_nodes_into_handler_and_surfaces_validation_error() {
        let state = test_state().await;
        let params = json!({ "name": "query_nodes", "arguments": {} });
        let resp = call(state, test_user(), Some(json!(1)), params).await;
        let v = serde_json::to_value(resp).unwrap();
        assert!(v["error"].is_null(), "expected tool error, got: {v}");
        assert_eq!(v["result"]["isError"], true);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("query_nodes"), "got: {text}");
        assert!(text.contains("node_type"), "got: {text}");
    }

    #[tokio::test]
    async fn call_routes_get_traceable_chain_into_handler_and_surfaces_validation_error() {
        let state = test_state().await;
        let params = json!({ "name": "get_traceable_chain", "arguments": {} });
        let resp = call(state, test_user(), Some(json!(1)), params).await;
        let v = serde_json::to_value(resp).unwrap();
        assert!(v["error"].is_null(), "expected tool error, got: {v}");
        assert_eq!(v["result"]["isError"], true);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("get_traceable_chain"), "got: {text}");
        assert!(text.contains("node_id"), "got: {text}");
    }

    #[tokio::test]
    async fn call_routes_ingest_into_handler_and_surfaces_validation_error() {
        // gRPC は実呼び出ししないが、handler 経由で arguments 検証エラーが
        // tool error として返ることを確認 (= dispatcher が ingest に届いた証拠)。
        let state = test_state().await;
        let params = json!({ "name": "ingest", "arguments": {} });
        let resp = call(state, test_user(), Some(json!(1)), params).await;
        let v = serde_json::to_value(resp).unwrap();
        assert!(v["error"].is_null(), "expected tool error, got: {v}");
        assert_eq!(v["result"]["isError"], true);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("ingest"), "got: {text}");
        assert!(text.contains("messages"), "got: {text}");
    }

    #[tokio::test]
    async fn call_routes_ingest_raw_into_handler_and_surfaces_validation_error() {
        let state = test_state().await;
        let params = json!({ "name": "ingest_raw", "arguments": {} });
        let resp = call(state, test_user(), Some(json!(1)), params).await;
        let v = serde_json::to_value(resp).unwrap();
        assert!(v["error"].is_null(), "expected tool error, got: {v}");
        assert_eq!(v["result"]["isError"], true);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("ingest_raw"), "got: {text}");
        assert!(text.contains("text"), "got: {text}");
    }

    #[test]
    fn feedback_and_get_job_status_are_not_advertised() {
        // proto: FeedbackRequest / GetJobStatusRequest には schema が無く、
        // wrapper 側で「その識別子は caller の tenant か」を確認する仕組み
        // (ownership tracking) が未実装。それまで tools/list に出すと
        // cross-tenant の rating 改竄 / job 状態漏洩を許してしまうので、
        // TOOL_NAMES に含めないことを test で pin する。
        assert!(!TOOL_NAMES.contains(&"feedback"));
        assert!(!TOOL_NAMES.contains(&"get_job_status"));
    }
}
