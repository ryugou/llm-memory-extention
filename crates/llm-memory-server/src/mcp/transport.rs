use axum::{Json, extract::State, response::IntoResponse};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

/// MCP protocol version this server speaks (see `https://spec.modelcontextprotocol.io/`).
pub(crate) const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
pub(crate) const SERVER_NAME: &str = "llm-memory-server";
pub(crate) const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
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

/// One of three outcomes for a single JSON-RPC entry inside a request body.
enum Outcome {
    /// A normal request that produced a JSON-RPC response envelope.
    Response(JsonRpcResponse),
    /// A notification that MUST NOT produce any envelope (HTTP 202 only).
    Notification,
    /// The entry failed to parse as a JSON-RPC request; emit a `-32600` error
    /// with `id: null` (per JSON-RPC 2.0).
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

/// MCP `/mcp` endpoint. Dispatches based on `method` field.
///
/// Supported methods:
/// - `initialize` — handshake; returns `protocolVersion`, `capabilities`, `serverInfo`.
/// - `ping` — empty `{}` result.
/// - `notifications/*` — no response; whole body returns HTTP 202.
/// - `tools/list` — enumerate tools with `inputSchema`.
/// - `tools/call` — invoke a tool; returns `CallToolResult { content, isError }`.
///
/// Accepts both single JSON-RPC requests and JSON-RPC 2.0 batch arrays
/// (MCP 2025-03-26 Streamable HTTP MUST).
pub async fn handle(
    State(state): State<AppState>,
    axum::Extension(user): axum::Extension<AuthenticatedUser>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    // Single object → process as one entry; array → process each entry, collect.
    let is_batch = body.is_array();
    let entries: Vec<Value> = if is_batch {
        body.as_array().cloned().unwrap_or_default()
    } else {
        vec![body]
    };

    // Per JSON-RPC 2.0 §6: an empty batch is itself a `-32600 Invalid Request`.
    if is_batch && entries.is_empty() {
        let err = JsonRpcResponse::error(None, -32600, "Invalid Request: empty batch");
        return Json(err).into_response();
    }

    let mut outcomes = Vec::with_capacity(entries.len());
    for entry in entries {
        let outcome = match serde_json::from_value::<JsonRpcRequest>(entry) {
            Ok(req) => dispatch_one(state.clone(), user.clone(), req).await,
            Err(e) => Outcome::ParseError(format!("Invalid Request: {e}")),
        };
        outcomes.push(outcome);
    }

    let responses: Vec<Value> = outcomes
        .into_iter()
        .filter_map(Outcome::into_response_value)
        .collect();

    // All-notifications input → 202 Accepted with no body (MCP transport MUST).
    if responses.is_empty() {
        return axum::http::StatusCode::ACCEPTED.into_response();
    }

    if is_batch {
        Json(Value::Array(responses)).into_response()
    } else {
        // Single request: unwrap the single response object.
        Json(responses.into_iter().next().unwrap_or(json!(null))).into_response()
    }
}

/// Dispatch a single (already-parsed) JSON-RPC request and produce an `Outcome`.
async fn dispatch_one(state: AppState, user: AuthenticatedUser, req: JsonRpcRequest) -> Outcome {
    let id = req.id.clone();

    // JSON-RPC notifications (no `id`) MUST NOT receive a response envelope.
    if id.is_none() {
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
        "tools/list" => crate::mcp::tools::list(id).await.0,
        "tools/call" => crate::mcp::tools::call(state, user, id, req.params).await.0,
        _ => JsonRpcResponse::error(id, -32601, "Method not found"),
    };
    Outcome::Response(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::build_state;
    use crate::config::ServerConfig;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn test_state() -> AppState {
        let cfg = ServerConfig {
            database_url: "sqlite::memory:".into(),
            bind_addr: "0.0.0.0:8080".into(),
            public_url: "https://test.example.com".into(),
            anthropic_api_key: "x".into(),
            google_client_id: "x".into(),
            google_client_secret: "x".into(),
            model_haiku: "h".into(),
            model_sonnet: "s".into(),
            trusted_proxy_count: 1,
        };
        build_state(cfg).await.unwrap()
    }

    fn user() -> AuthenticatedUser {
        AuthenticatedUser {
            user_id: "u1".into(),
            client_id: "c".into(),
        }
    }

    async fn invoke(state: AppState, body: Value) -> (axum::http::StatusCode, Value) {
        // Build a minimal router that injects the `Extension<AuthenticatedUser>`
        // that the real middleware would supply, then call `handle` via POST.
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let app = axum::Router::new()
            .route("/mcp", axum::routing::post(handle))
            .layer(axum::middleware::from_fn(inject_user))
            .with_state(state);
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let body_bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body_val: Value = if body_bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body_bytes).unwrap_or(Value::Null)
        };
        (status, body_val)
    }

    async fn inject_user(
        mut req: axum::http::Request<axum::body::Body>,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        req.extensions_mut().insert(user());
        next.run(req).await
    }

    #[test]
    fn json_rpc_success_serializes() {
        let r = JsonRpcResponse::success(Some(Value::from(1)), json!({"ok": true}));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"jsonrpc\":\"2.0\""));
        assert!(s.contains("\"result\""));
        assert!(!s.contains("\"error\""));
    }

    #[test]
    fn json_rpc_error_serializes() {
        let r = JsonRpcResponse::error(None, -32601, "Method not found");
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"error\""));
        assert!(s.contains("Method not found"));
        assert!(!s.contains("\"result\""));
    }

    #[tokio::test]
    async fn initialize_returns_protocol_version() {
        let state = test_state().await;
        let body = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
        let (status, v) = invoke(state, body).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(v["result"]["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(v["result"]["capabilities"]["tools"]["listChanged"], false);
    }

    #[tokio::test]
    async fn ping_returns_empty_result() {
        let state = test_state().await;
        let body = json!({"jsonrpc":"2.0","id":7,"method":"ping"});
        let (status, v) = invoke(state, body).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["id"], 7);
        assert_eq!(v["result"], json!({}));
    }

    #[tokio::test]
    async fn notification_returns_202_with_no_body() {
        let state = test_state().await;
        let body = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        let (status, v) = invoke(state, body).await;
        assert_eq!(status, axum::http::StatusCode::ACCEPTED);
        assert!(v.is_null());
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let state = test_state().await;
        let body = json!({"jsonrpc":"2.0","id":2,"method":"does/not/exist"});
        let (status, v) = invoke(state, body).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn batch_with_only_notifications_returns_202() {
        let state = test_state().await;
        let body = json!([
            {"jsonrpc":"2.0","method":"notifications/initialized"},
            {"jsonrpc":"2.0","method":"notifications/cancelled","params":{"id":1}}
        ]);
        let (status, v) = invoke(state, body).await;
        assert_eq!(status, axum::http::StatusCode::ACCEPTED);
        assert!(v.is_null());
    }

    #[tokio::test]
    async fn batch_mixes_responses_and_notifications() {
        let state = test_state().await;
        let body = json!([
            {"jsonrpc":"2.0","id":1,"method":"ping"},
            {"jsonrpc":"2.0","method":"notifications/initialized"},
            {"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}
        ]);
        let (status, v) = invoke(state, body).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let arr = v.as_array().expect("batch response array");
        assert_eq!(arr.len(), 2, "notification omitted from batch response");
        let ids: Vec<&Value> = arr.iter().map(|r| &r["id"]).collect();
        assert!(ids.contains(&&json!(1)));
        assert!(ids.contains(&&json!(2)));
    }

    #[tokio::test]
    async fn empty_batch_is_invalid_request() {
        let state = test_state().await;
        let body = json!([]);
        let (_status, v) = invoke(state, body).await;
        assert_eq!(v["error"]["code"], -32600);
    }
}
