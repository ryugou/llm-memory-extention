//! MCP tool handler 本体 (vegapunk gRPC への fan-out)。
//!
//! 各 handler は次の責務を持つ:
//! 1. MCP `tools/call` の `arguments` JSON を proto request struct に詰め替える。
//! 2. `schema` フィールド (= vegapunk tenant key) は必ず `AuthenticatedUser.vegapunk_schema`
//!    から注入し、client が `arguments.schema` で別 tenant を指定しても上書きしない
//!    (cross-tenant guard).
//! 3. vegapunk gRPC を呼び、`tonic::Status` を MCP の `isError: true` content に変換する。
//! 4. 正常応答は MCP `tools/call` の content 形 (`type: "text"` + `structuredContent`) に整形して返す。
//!
//! 単体テストは request 構築 (`build_*`) を pure function として狙う — 実 gRPC を mock せず、
//! 「schema 注入」「引数マッピング」「missing required の検出」だけを保証する。
//! 実際の gRPC 往復は handler 全体の integration test (= 別 PR で fake server を立てて) で見る。

use serde_json::{Value, json};

use vegapunk_client::graphrag::{GetSchemaRequest, SearchRequest};
use vegapunk_memory_auth::middleware::AuthenticatedUser;

use crate::state::AppState;

/// `tools/list` advertised range for `search.limit` (`top_k`). Keep in sync
/// with `tool_descriptor("search").inputSchema.properties.limit` in
/// `crate::mcp::tools::tool_descriptor`.
const SEARCH_LIMIT_MIN: i64 = 1;
const SEARCH_LIMIT_MAX: i64 = 100;

/// `search` argument を `SearchRequest` に詰める。`schema` は `user_schema` で
/// 強制上書きするので、`arguments` に schema 指定があっても無視する。
pub(super) fn build_search_request(
    user_schema: &str,
    args: &Value,
) -> Result<SearchRequest, String> {
    let text = args
        .get("query")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing required argument: 'query'".to_string())?
        .to_string();
    // `tools/list` advertises mode default = "hybrid", but vegapunk's
    // SearchRequest.mode defaults to "local" if left empty. Inject "hybrid"
    // explicitly so the advertised default matches actual behavior.
    let mode = Some(
        args.get("mode")
            .and_then(Value::as_str)
            .unwrap_or("hybrid")
            .to_string(),
    );
    // `tools/list` advertises `limit: integer, minimum 1, maximum 100`. JSON
    // Schema validation lives client-side, so the wrapper has to enforce the
    // contract too — naïve `as i32` would silently wrap negatives / huge
    // numbers and send a nonsense `top_k` to vegapunk.
    let top_k = match args.get("limit") {
        None => None,
        Some(Value::Null) => None,
        Some(v) => {
            let n = v
                .as_i64()
                .ok_or_else(|| "'limit' must be an integer".to_string())?;
            if !(SEARCH_LIMIT_MIN..=SEARCH_LIMIT_MAX).contains(&n) {
                return Err(format!(
                    "'limit' must be between {SEARCH_LIMIT_MIN} and {SEARCH_LIMIT_MAX}, got {n}"
                ));
            }
            // bounds-checked above, so the i32 cast is safe.
            Some(n as i32)
        }
    };
    Ok(SearchRequest {
        text,
        filter: None,
        depth: None,
        top_k,
        format: None,
        mode,
        schema: user_schema.to_string(),
        offset: None,
        limit: None,
        structural_weight: None,
    })
}

/// `get_schema` request を組む。MCP 側 inputSchema は no-arg なので、user の
/// vegapunk_schema をそのまま `name` に入れる。arguments は (将来 fields が
/// 増えるかもしれないので signature だけ受け取り) 現状は使わない。
pub(super) fn build_get_schema_request(user_schema: &str) -> GetSchemaRequest {
    GetSchemaRequest {
        name: user_schema.to_string(),
    }
}

/// `tools/call` 用の正常 content を組む。MCP spec: `content` は text/image/...
/// の配列、`structuredContent` で JSON object を別途返せる (clients/IDE がパースする)。
pub(super) fn success_content(structured: Value) -> Value {
    let text = serde_json::to_string(&structured)
        .unwrap_or_else(|_| "<failed to serialize structured response>".into());
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": false,
    })
}

/// `tonic::Status` を MCP tool error content に詰める。code は `gRPC <code>` の
/// プレフィクス付きで text に残す (= client が retry 判断に使える)。
pub(super) fn tonic_error_content(method: &str, status: tonic::Status) -> Value {
    let code = status.code();
    let msg = status.message();
    let body = format!("vegapunk {method} failed: gRPC {code:?}: {msg}");
    tracing::warn!(method = method, code = ?code, message = msg, "vegapunk gRPC error");
    json!({
        "content": [{ "type": "text", "text": body }],
        "isError": true,
    })
}

/// `arguments` 不備など handler 内で生じた client 起因エラーを tool error にする。
pub(super) fn invalid_args_content(method: &str, reason: &str) -> Value {
    let body = format!("invalid arguments for '{method}': {reason}");
    json!({
        "content": [{ "type": "text", "text": body }],
        "isError": true,
    })
}

pub(super) async fn search(state: &AppState, user: &AuthenticatedUser, args: Value) -> Value {
    let request = match build_search_request(&user.vegapunk_schema, &args) {
        Ok(r) => r,
        Err(e) => return invalid_args_content("search", &e),
    };
    let mut client = state.vegapunk.clone();
    match client.search(request).await {
        Err(status) => tonic_error_content("Search", status),
        Ok(resp) => {
            let resp = resp.into_inner();
            let results: Vec<Value> = resp
                .results
                .iter()
                .map(|item| {
                    json!({
                        "type": item.r#type,
                        "id": item.id,
                        "text": item.text,
                        "score": item.score,
                        "person": item.person,
                        "timestamp": item.timestamp,
                        "summary": item.summary,
                        "channel": item.channel,
                        "decided_at": item.decided_at,
                        "rationales": item.rationales,
                    })
                })
                .collect();
            success_content(json!({
                "search_id": resp.search_id,
                "total_count": resp.total_count,
                "results": results,
            }))
        }
    }
}

pub(super) async fn get_schema(state: &AppState, user: &AuthenticatedUser) -> Value {
    let request = build_get_schema_request(&user.vegapunk_schema);
    let mut client = state.vegapunk.clone();
    match client.get_schema(request).await {
        Err(status) => tonic_error_content("GetSchema", status),
        Ok(resp) => {
            let resp = resp.into_inner();
            success_content(json!({
                "name": resp.name,
                "schema_yaml": resp.schema_yaml,
                "version": resp.version,
                "description": resp.description,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_request_uses_user_schema_and_ignores_args_schema() {
        // cross-tenant guard: arguments.schema は信用しない。
        let args = json!({"query": "hello", "schema": "evil-tenant"});
        let req = build_search_request("alice-tenant", &args).unwrap();
        assert_eq!(req.schema, "alice-tenant");
        assert_eq!(req.text, "hello");
        // mode は省略時 tools/list の default ("hybrid") と一致させる。
        assert_eq!(req.mode.as_deref(), Some("hybrid"));
        assert_eq!(req.top_k, None);
    }

    #[test]
    fn search_request_maps_mode_and_limit() {
        let args = json!({"query": "q", "mode": "local", "limit": 25});
        let req = build_search_request("t", &args).unwrap();
        assert_eq!(req.mode.as_deref(), Some("local"));
        assert_eq!(req.top_k, Some(25));
    }

    #[test]
    fn search_request_defaults_mode_to_hybrid_when_omitted() {
        // tools/list が advertise する default ("hybrid") と、handler が
        // vegapunk に投げる実値が一致することを保証する。proto の default は
        // "local" なので、wrapper が明示的に "hybrid" を埋める必要がある。
        let req = build_search_request("t", &json!({"query": "q"})).unwrap();
        assert_eq!(req.mode.as_deref(), Some("hybrid"));
    }

    #[test]
    fn search_request_rejects_negative_limit() {
        let err = build_search_request("t", &json!({"query":"q","limit":-1})).unwrap_err();
        assert!(err.contains("'limit'"), "got: {err}");
    }

    #[test]
    fn search_request_rejects_limit_above_max() {
        let err = build_search_request("t", &json!({"query":"q","limit":101})).unwrap_err();
        assert!(err.contains("100"), "got: {err}");
    }

    #[test]
    fn search_request_rejects_huge_limit_no_silent_wrap() {
        // 2^40 を `as i32` で wrap させない (= 元値が範囲外なら range error)。
        let err = build_search_request("t", &json!({"query":"q","limit":1_099_511_627_776_i64}))
            .unwrap_err();
        assert!(err.contains("'limit'"), "got: {err}");
    }

    #[test]
    fn search_request_rejects_non_integer_limit() {
        let err = build_search_request("t", &json!({"query":"q","limit":"ten"})).unwrap_err();
        assert!(err.contains("integer"), "got: {err}");
    }

    #[test]
    fn search_request_accepts_null_limit_as_omitted() {
        // JSON null は省略と同じ扱い: top_k は None。
        let req = build_search_request("t", &json!({"query":"q","limit":null})).unwrap();
        assert_eq!(req.top_k, None);
    }

    #[test]
    fn search_request_accepts_boundary_limit() {
        let req_min = build_search_request("t", &json!({"query":"q","limit":1})).unwrap();
        assert_eq!(req_min.top_k, Some(1));
        let req_max = build_search_request("t", &json!({"query":"q","limit":100})).unwrap();
        assert_eq!(req_max.top_k, Some(100));
    }

    #[test]
    fn search_request_rejects_missing_query() {
        let err = build_search_request("t", &json!({})).unwrap_err();
        assert!(err.contains("query"), "error should mention query: {err}");
    }

    #[test]
    fn search_request_rejects_empty_query() {
        // 空文字を許すと vegapunk 側で意味の無い query が走ってリソースを食う。
        let err = build_search_request("t", &json!({"query": ""})).unwrap_err();
        assert!(err.contains("query"));
    }

    #[test]
    fn get_schema_request_always_uses_user_schema() {
        let req = build_get_schema_request("bob-tenant");
        assert_eq!(req.name, "bob-tenant");
    }

    #[test]
    fn success_content_has_text_and_structured() {
        let v = success_content(json!({"hello": "world"}));
        assert_eq!(v["isError"], false);
        assert_eq!(v["structuredContent"]["hello"], "world");
        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("hello"));
    }

    #[test]
    fn tonic_error_content_marks_iserror_true_and_includes_code() {
        let status = tonic::Status::permission_denied("no access");
        let v = tonic_error_content("Search", status);
        assert_eq!(v["isError"], true);
        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Search"));
        assert!(text.contains("PermissionDenied"), "got: {text}");
        assert!(text.contains("no access"));
    }

    #[test]
    fn invalid_args_content_marks_iserror_true() {
        let v = invalid_args_content("search", "missing 'query'");
        assert_eq!(v["isError"], true);
        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("search"));
        assert!(text.contains("missing 'query'"));
    }
}
