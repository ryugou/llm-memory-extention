use axum::{Json, extract::State, response::IntoResponse};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
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

/// MCP /mcp endpoint. Dispatches based on `method` field.
pub async fn handle(
    State(state): State<AppState>,
    axum::Extension(user): axum::Extension<AuthenticatedUser>,
    Json(req): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    match req.method.as_str() {
        "tools/list" => crate::mcp::tools::list(req.id).await,
        "tools/call" => crate::mcp::tools::call(state, user, req.id, req.params).await,
        _ => Json(JsonRpcResponse::error(req.id, -32601, "Method not found")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_rpc_success_serializes() {
        let r = JsonRpcResponse::success(Some(Value::from(1)), serde_json::json!({"ok": true}));
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
}
