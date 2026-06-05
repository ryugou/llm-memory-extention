//! MCP `/mcp` endpoint (Streamable HTTP transport, MCP 2025-03-26)。
//!
//! JSON-RPC 2.0 を受けて method ベースで dispatch する:
//! - `initialize`: handshake (protocolVersion / capabilities / serverInfo)
//! - `ping`: 空 result
//! - `notifications/*`: response を返さず HTTP 202
//! - `tools/list`: tool 一覧 (vegapunk RPC subset の input schema)
//! - `tools/call`: 個別 tool handler 呼び出し (本 PR では未実装スタブ)
//!
//! 本ハンドラに到達する時点で `route_layer(require_auth)` 経由で JWT
//! が verify 済み、`AuthenticatedUser` が `Extension` に乗っている。
//! tool handler は user の `vegapunk_schema` を強制注入して cross-tenant
//! を防ぐ (= 次 PR で実装)。

use axum::{Json, body::Bytes, extract::State, response::IntoResponse};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::state::AppState;
use vegapunk_memory_auth::middleware::AuthenticatedUser;

pub(crate) const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
pub(crate) const SERVER_NAME: &str = "vegapunk-memory-server";
pub(crate) const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    /// JSON-RPC 2.0 §4: 「id 省略 → notification、`id: null` → null id を持つ
    /// 通常 request」を区別するため、serde default だと両者が None に潰される
    /// のを `deserialize_with` で field が現れたら Some にラップ。
    #[serde(default, deserialize_with = "deserialize_optional_id")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

fn deserialize_optional_id<'de, D>(d: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Value::deserialize(d)?))
}

impl JsonRpcRequest {
    fn jsonrpc_version_is_valid(&self) -> bool {
        self.jsonrpc == "2.0"
    }
    fn id_type_is_valid(&self) -> bool {
        match &self.id {
            None => true,
            Some(v) => v.is_string() || v.is_number() || v.is_null(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

impl JsonRpcResponse {
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn error(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

enum Outcome {
    Response(JsonRpcResponse),
    Notification,
    ParseError(String),
}

impl Outcome {
    fn into_response_value(self) -> Option<Value> {
        match self {
            Outcome::Response(r) => Some(serde_json::to_value(r).unwrap_or(Value::Null)),
            Outcome::Notification => None,
            Outcome::ParseError(msg) => Some(
                serde_json::to_value(JsonRpcResponse::error(None, -32600, msg))
                    .unwrap_or(Value::Null),
            ),
        }
    }
}

/// MCP `/mcp` endpoint. Single object / batch (JSON-RPC 2.0 §6) どちらも受ける。
///
/// JSON 全体の parse 失敗は §5.1 に従い HTTP 200 + `-32700 Parse error`
/// (`id: null`)。axum の Json extractor だと HTTP 400 で reject されるので
/// `Bytes` で受けて手動 parse する。
pub async fn handle(
    State(state): State<AppState>,
    axum::Extension(user): axum::Extension<AuthenticatedUser>,
    body: Bytes,
) -> axum::response::Response {
    let body: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "JSON-RPC body parse failed");
            let err = JsonRpcResponse::error(None, -32700, "Parse error");
            return Json(err).into_response();
        }
    };

    let is_batch = body.is_array();
    let entries: Vec<Value> = if is_batch {
        body.as_array().cloned().unwrap_or_default()
    } else {
        vec![body]
    };

    if is_batch && entries.is_empty() {
        let err = JsonRpcResponse::error(None, -32600, "Invalid Request: empty batch");
        return Json(err).into_response();
    }

    let mut outcomes = Vec::with_capacity(entries.len());
    for entry in entries {
        let outcome = match serde_json::from_value::<JsonRpcRequest>(entry) {
            Ok(req) => {
                if !req.jsonrpc_version_is_valid() {
                    Outcome::ParseError(format!(
                        "Invalid Request: jsonrpc must be \"2.0\", got {:?}",
                        req.jsonrpc
                    ))
                } else if !req.id_type_is_valid() {
                    Outcome::ParseError(
                        "Invalid Request: id must be string, number, or null".into(),
                    )
                } else {
                    dispatch_one(state.clone(), user.clone(), req).await
                }
            }
            Err(e) => Outcome::ParseError(format!("Invalid Request: {e}")),
        };
        outcomes.push(outcome);
    }

    let responses: Vec<Value> = outcomes
        .into_iter()
        .filter_map(Outcome::into_response_value)
        .collect();

    if responses.is_empty() {
        return axum::http::StatusCode::ACCEPTED.into_response();
    }

    if is_batch {
        Json(Value::Array(responses)).into_response()
    } else {
        Json(responses.into_iter().next().unwrap_or(json!(null))).into_response()
    }
}

async fn dispatch_one(state: AppState, user: AuthenticatedUser, req: JsonRpcRequest) -> Outcome {
    let id = req.id.clone();
    let is_notification = id.is_none();

    // MCP lifecycle notifications (`notifications/initialized`,
    // `notifications/cancelled`, ...) are fire-and-forget per JSON-RPC §4.1 /
    // MCP spec: log only, never produce a response. We don't currently act on
    // any of them (cancellation will land with the tool handlers in the next
    // PR), so this is intentionally an ack-only path.
    //
    // If a client mistakenly sends an `id` with a `notifications/*` method,
    // it isn't a real notification: JSON-RPC says any request *with* an id
    // MUST receive a response, and silently suppressing one would hang the
    // client. Reject those as `-32600 Invalid Request`.
    if req.method.starts_with("notifications/") {
        if !is_notification {
            return Outcome::Response(JsonRpcResponse::error(
                id,
                -32600,
                format!(
                    "Invalid Request: '{}' must be sent as a Notification (no id)",
                    req.method
                ),
            ));
        }
        tracing::debug!(method = %req.method, "MCP notification received (ack only)");
        return Outcome::Notification;
    }

    let response = match req.method.as_str() {
        "initialize" => JsonRpcResponse::success(
            id,
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
            }),
        ),
        "ping" => JsonRpcResponse::success(id, json!({})),
        "tools/list" => crate::mcp::tools::list(id),
        "tools/call" => crate::mcp::tools::call(state, user, id, req.params).await,
        _ => JsonRpcResponse::error(id, -32601, "Method not found"),
    };

    // JSON-RPC §4.1: a request with no `id` is a Notification — the method
    // still ran above, but we MUST NOT return a Response object.
    if is_notification {
        Outcome::Notification
    } else {
        Outcome::Response(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use crate::test_support::{test_state, test_user};
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn invoke(state: AppState, body: Value) -> (axum::http::StatusCode, Value) {
        let app = Router::new()
            .route("/mcp", axum::routing::post(handle))
            .layer(axum::Extension(test_user()))
            .with_state(state);
        let req = Request::post("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, v)
    }

    #[tokio::test]
    async fn parse_error_returns_jsonrpc_envelope_with_null_id() {
        let app = Router::new()
            .route("/mcp", axum::routing::post(handle))
            .layer(axum::Extension(test_user()))
            .with_state(test_state().await);
        let req = Request::post("/mcp")
            .header("content-type", "application/json")
            .body(Body::from("not json"))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), 200);
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], -32700);
        assert_eq!(v["error"]["message"], "Parse error");
        assert_eq!(v["id"], Value::Null);
    }

    #[tokio::test]
    async fn initialize_returns_protocol_version() {
        let body = json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
        let (status, v) = invoke(test_state().await, body).await;
        assert_eq!(status, 200);
        assert_eq!(v["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(v["result"]["serverInfo"]["name"], SERVER_NAME);
    }

    #[tokio::test]
    async fn tools_list_returns_known_tool_names() {
        let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        let (status, v) = invoke(test_state().await, body).await;
        assert_eq!(status, 200);
        let tools = v["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // `feedback` / `get_job_status` are intentionally NOT advertised
        // until ownership tracking lands (see `tools::mod` TOOL_NAMES note).
        for required in [
            "search",
            "ingest",
            "ingest_raw",
            "query_nodes",
            "get_schema",
            "list_schemas",
            "stats",
            "get_traceable_chain",
        ] {
            assert!(names.contains(&required), "missing tool: {required}");
        }
        for must_not in ["feedback", "get_job_status"] {
            assert!(
                !names.contains(&must_not),
                "tool '{must_not}' must not be advertised yet (cross-tenant guard not implemented)"
            );
        }
    }

    #[tokio::test]
    async fn unknown_method_returns_minus_32601() {
        let body = json!({"jsonrpc":"2.0","id":1,"method":"foo/bar"});
        let (_status, v) = invoke(test_state().await, body).await;
        assert_eq!(v["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn notification_returns_202_with_no_body() {
        // id 省略 = notification → response 無し、HTTP 202
        let body = json!({"jsonrpc":"2.0","method":"initialize"});
        let (status, v) = invoke(test_state().await, body).await;
        assert_eq!(status, axum::http::StatusCode::ACCEPTED);
        assert!(v.is_null());
    }

    #[tokio::test]
    async fn mcp_notifications_namespace_is_accepted_without_response() {
        // MCP lifecycle messages (`notifications/initialized`,
        // `notifications/cancelled`) come in via `/mcp` as JSON-RPC
        // notifications — server must ack with 202 and produce no response.
        for method in ["notifications/initialized", "notifications/cancelled"] {
            let body = json!({"jsonrpc":"2.0","method":method});
            let (status, v) = invoke(test_state().await, body).await;
            assert_eq!(
                status,
                axum::http::StatusCode::ACCEPTED,
                "method {method} should return 202"
            );
            assert!(v.is_null());
        }
    }

    #[tokio::test]
    async fn notifications_namespace_with_id_returns_invalid_request() {
        // notifications/* に id が付いて来たら spec 違反 — JSON-RPC は
        // id 付き request に必ず response を返す義務があるので、ack-only
        // パスでなく `-32600 Invalid Request` を返してクライアントが
        // hang しないようにする。
        let body = json!({"jsonrpc":"2.0","id":42,"method":"notifications/initialized"});
        let (status, v) = invoke(test_state().await, body).await;
        assert_eq!(status, 200);
        assert_eq!(v["error"]["code"], -32600);
        assert_eq!(v["id"], 42);
    }

    #[tokio::test]
    async fn batch_with_only_notifications_returns_202() {
        // Batch entirely of notifications → no responses → 202.
        let body = json!([
            {"jsonrpc":"2.0","method":"notifications/initialized"},
            {"jsonrpc":"2.0","method":"ping"}
        ]);
        let (status, v) = invoke(test_state().await, body).await;
        assert_eq!(status, axum::http::StatusCode::ACCEPTED);
        assert!(v.is_null());
    }
}
