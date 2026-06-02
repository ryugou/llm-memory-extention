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

use serde_json::{Map, Value, json};

use vegapunk_client::graphrag::{
    GetSchemaRequest, IngestMessage, IngestRawMetadata, IngestRawRequest, IngestRequest,
    MessageMetadata, SearchRequest, SearchResultItem,
};
use vegapunk_memory_auth::middleware::AuthenticatedUser;

use crate::state::AppState;

use super::{
    SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX, SEARCH_LIMIT_MIN, SEARCH_MODE_DEFAULT,
    SEARCH_VALID_MODES,
};

/// MCP `tools/call.arguments` を共通の作法で object に降ろす。
/// `arguments` は spec 上 object のみ、string/array/null/number は早く弾く。
fn require_object_args(args: &Value) -> Result<&Map<String, Value>, String> {
    args.as_object()
        .ok_or_else(|| "'arguments' must be a JSON object".to_string())
}

/// 必須の string field を取り出す: 存在しない / null / 非 string / 空白のみは
/// すべて error にする。空白を許すと vegapunk の required string が "" で
/// 通って何の意味も無い row が入る。
fn require_str_field(obj: &Map<String, Value>, field: &str, owner: &str) -> Result<String, String> {
    let raw = match obj.get(field) {
        None | Some(Value::Null) => {
            return Err(format!("missing required '{owner}.{field}'"));
        }
        Some(v) => v
            .as_str()
            .ok_or_else(|| format!("'{owner}.{field}' must be a string"))?,
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("'{owner}.{field}' must not be empty"));
    }
    Ok(trimmed.to_string())
}

/// 任意の string field: null / missing は None、非 string は error。
fn optional_str_field(
    obj: &Map<String, Value>,
    field: &str,
    owner: &str,
) -> Result<Option<String>, String> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_str() {
            Some(s) => Ok(Some(s.to_string())),
            None => Err(format!("'{owner}.{field}' must be a string")),
        },
    }
}

/// `search` argument を `SearchRequest` に詰める。`schema` は `user_schema` で
/// 強制上書きするので、`arguments` に schema 指定があっても無視する。
pub(super) fn build_search_request(
    user_schema: &str,
    args: &Value,
) -> Result<SearchRequest, String> {
    // `arguments` は MCP spec で object と決まっている。client が string/array/null
    // を渡してきた場合に「missing required argument」と言うのは誤導になるので
    // 早めに shape を弾く。
    let args = args
        .as_object()
        .ok_or_else(|| "'arguments' must be a JSON object".to_string())?;
    let text = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing or empty required argument: 'query'".to_string())?
        .to_string();
    // `tools/list` advertises mode as enum {local, global, hybrid} with default
    // "hybrid"; vegapunk's SearchRequest.mode defaults to "local". The wrapper
    // can't rely on client-side JSON Schema validation, so enforce the enum
    // here and inject "hybrid" when omitted to match the advertised default.
    let mode = {
        let raw = match args.get("mode") {
            None | Some(Value::Null) => SEARCH_MODE_DEFAULT,
            Some(v) => v
                .as_str()
                .ok_or_else(|| "'mode' must be a string".to_string())?,
        };
        if !SEARCH_VALID_MODES.contains(&raw) {
            return Err(format!(
                "'mode' must be one of {SEARCH_VALID_MODES:?}, got {raw:?}"
            ));
        }
        Some(raw.to_string())
    };
    // `tools/list` advertises `limit: integer, minimum 1, maximum 100, default 10`.
    // Inject the advertised default when omitted (vegapunk's SearchRequest
    // would pick its own default otherwise, which we'd then be lying about),
    // and bounds-check before casting so naïve `as i32` can't silently wrap
    // negatives / huge numbers.
    let top_k = match args.get("limit") {
        None | Some(Value::Null) => Some(SEARCH_LIMIT_DEFAULT),
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
/// vegapunk_schema をそのまま `name` に入れる。tool 側で arguments が来ても
/// handler が無視する設計なので、ここでは引数を受け取らない。
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

/// `tonic::Status` を MCP tool error content に詰める。
///
/// gRPC code は client が retry 判定に使うので常に露出するが、`Internal` /
/// `Unknown` / `DataLoss` / `Unimplemented` の message は backend 内部の trace
/// やパスを含むことがあり、wrapper の public surface に流すと情報漏洩リスクが
/// ある。そういう code では message を落として code だけを返し、full message
/// は tracing に残してオペレータが追えるようにする。
/// client-actionable な code (InvalidArgument / NotFound / Unauthenticated 等)
/// は message も含める — 何を直せばいいか伝わらないと意味が無いため。
pub(super) fn tonic_error_content(method: &str, status: tonic::Status) -> Value {
    let code = status.code();
    let raw_msg = status.message();
    tracing::warn!(method = method, code = ?code, message = raw_msg, "vegapunk gRPC error");
    let body = if message_is_client_safe(code) {
        format!("vegapunk {method} failed: gRPC {code:?}: {raw_msg}")
    } else {
        format!("vegapunk {method} failed: gRPC {code:?} (details suppressed)")
    };
    json!({
        "content": [{ "type": "text", "text": body }],
        "isError": true,
    })
}

fn message_is_client_safe(code: tonic::Code) -> bool {
    use tonic::Code::*;
    matches!(
        code,
        Cancelled
            | InvalidArgument
            | DeadlineExceeded
            | NotFound
            | AlreadyExists
            | PermissionDenied
            | ResourceExhausted
            | FailedPrecondition
            | Aborted
            | OutOfRange
            | Unavailable
            | Unauthenticated
    )
}

/// `arguments` 不備など handler 内で生じた client 起因エラーを tool error にする。
pub(super) fn invalid_args_content(method: &str, reason: &str) -> Value {
    let body = format!("invalid arguments for '{method}': {reason}");
    json!({
        "content": [{ "type": "text", "text": body }],
        "isError": true,
    })
}

fn search_result_item_json(item: &SearchResultItem) -> Value {
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
            let results: Vec<Value> = resp.results.iter().map(search_result_item_json).collect();
            // Preserve Phase 3 cross-project similar_patterns from the
            // backend (proto field 4) — drop nothing the server returns,
            // even if today's clients ignore it.
            let similar_patterns: Vec<Value> = resp
                .similar_patterns
                .iter()
                .map(|sp| {
                    let nodes: Vec<Value> = sp.nodes.iter().map(search_result_item_json).collect();
                    json!({
                        "source_project": sp.source_project,
                        "structural_similarity": sp.structural_similarity,
                        "nodes": nodes,
                    })
                })
                .collect();
            success_content(json!({
                "search_id": resp.search_id,
                "total_count": resp.total_count,
                "results": results,
                "similar_patterns": similar_patterns,
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

/// `ingest` argument を `IngestRequest` に詰める。`schema` は `user_schema` で
/// 強制上書きする (cross-tenant guard)。messages.metadata の必須 field は
/// `tools/list` の inputSchema と同じ集合 (source_type / author / channel /
/// timestamp) を runtime でも guard する — client 側 JSON Schema 検証は無保証。
pub(super) fn build_ingest_request(
    user_schema: &str,
    args: &Value,
) -> Result<IngestRequest, String> {
    let args = require_object_args(args)?;
    let messages_value = args
        .get("messages")
        .ok_or_else(|| "missing required argument: 'messages'".to_string())?;
    let messages_array = messages_value
        .as_array()
        .ok_or_else(|| "'messages' must be an array".to_string())?;
    if messages_array.is_empty() {
        return Err("'messages' must contain at least one item".to_string());
    }
    let messages = messages_array
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let owner = format!("messages[{i}]");
            let m = item
                .as_object()
                .ok_or_else(|| format!("'{owner}' must be an object"))?;
            let id = optional_str_field(m, "id", &owner)?;
            let text = require_str_field(m, "text", &owner)?;
            let meta_owner = format!("{owner}.metadata");
            let metadata_value = m
                .get("metadata")
                .ok_or_else(|| format!("missing required '{meta_owner}'"))?;
            let metadata_obj = metadata_value
                .as_object()
                .ok_or_else(|| format!("'{meta_owner}' must be an object"))?;
            let metadata = MessageMetadata {
                source_type: require_str_field(metadata_obj, "source_type", &meta_owner)?,
                author: require_str_field(metadata_obj, "author", &meta_owner)?,
                author_id: optional_str_field(metadata_obj, "author_id", &meta_owner)?,
                channel: require_str_field(metadata_obj, "channel", &meta_owner)?,
                channel_id: optional_str_field(metadata_obj, "channel_id", &meta_owner)?,
                thread_id: optional_str_field(metadata_obj, "thread_id", &meta_owner)?,
                timestamp: require_str_field(metadata_obj, "timestamp", &meta_owner)?,
            };
            Ok::<_, String>(IngestMessage {
                id,
                text,
                metadata: Some(metadata),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(IngestRequest {
        messages,
        schema: user_schema.to_string(),
    })
}

/// `ingest_raw` argument を `IngestRawRequest` に詰める。`schema` は
/// `user_schema` で強制上書き。`metadata.source_type` だけ必須、author / channel /
/// timestamp は optional。proto 側で timestamp 省略時は server time が使われる。
pub(super) fn build_ingest_raw_request(
    user_schema: &str,
    args: &Value,
) -> Result<IngestRawRequest, String> {
    let args = require_object_args(args)?;
    let text = require_str_field(args, "text", "arguments")?;
    let metadata_value = args
        .get("metadata")
        .ok_or_else(|| "missing required argument: 'metadata'".to_string())?;
    let metadata_obj = metadata_value
        .as_object()
        .ok_or_else(|| "'metadata' must be an object".to_string())?;
    let metadata = IngestRawMetadata {
        source_type: require_str_field(metadata_obj, "source_type", "metadata")?,
        author: optional_str_field(metadata_obj, "author", "metadata")?,
        channel: optional_str_field(metadata_obj, "channel", "metadata")?,
        timestamp: optional_str_field(metadata_obj, "timestamp", "metadata")?,
    };
    Ok(IngestRawRequest {
        text,
        metadata: Some(metadata),
        schema: user_schema.to_string(),
    })
}

pub(super) async fn ingest(state: &AppState, user: &AuthenticatedUser, args: Value) -> Value {
    let request = match build_ingest_request(&user.vegapunk_schema, &args) {
        Ok(r) => r,
        Err(e) => return invalid_args_content("ingest", &e),
    };
    let mut client = state.vegapunk.clone();
    match client.ingest(request).await {
        Err(status) => tonic_error_content("Ingest", status),
        Ok(resp) => {
            let resp = resp.into_inner();
            success_content(json!({
                "ingested_count": resp.ingested_count,
                "job_id": resp.job_id,
            }))
        }
    }
}

pub(super) async fn ingest_raw(state: &AppState, user: &AuthenticatedUser, args: Value) -> Value {
    let request = match build_ingest_raw_request(&user.vegapunk_schema, &args) {
        Ok(r) => r,
        Err(e) => return invalid_args_content("ingest_raw", &e),
    };
    let mut client = state.vegapunk.clone();
    match client.ingest_raw(request).await {
        Err(status) => tonic_error_content("IngestRaw", status),
        Ok(resp) => {
            let resp = resp.into_inner();
            success_content(json!({
                "chunk_count": resp.chunk_count,
                "msg_ids": resp.msg_ids,
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
        // mode は省略時 tools/list の default ("hybrid")、limit は default 10。
        assert_eq!(req.mode.as_deref(), Some("hybrid"));
        assert_eq!(req.top_k, Some(SEARCH_LIMIT_DEFAULT));
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
    fn search_request_treats_null_limit_as_omitted_and_applies_default() {
        // JSON null は省略と同じ扱いになり、advertised default 10 が入る。
        let req = build_search_request("t", &json!({"query":"q","limit":null})).unwrap();
        assert_eq!(req.top_k, Some(SEARCH_LIMIT_DEFAULT));
    }

    #[test]
    fn search_request_defaults_limit_to_advertised_default() {
        // limit を渡さなかった場合は tools/list の default (10) に揃える
        // (vegapunk 側のバックエンド default に流されない)。
        let req = build_search_request("t", &json!({"query":"q"})).unwrap();
        assert_eq!(req.top_k, Some(SEARCH_LIMIT_DEFAULT));
    }

    #[test]
    fn search_request_rejects_unknown_mode() {
        let err = build_search_request("t", &json!({"query":"q","mode":"banana"})).unwrap_err();
        assert!(err.contains("'mode'"), "got: {err}");
        assert!(err.contains("banana"), "got: {err}");
    }

    #[test]
    fn search_request_rejects_empty_mode() {
        let err = build_search_request("t", &json!({"query":"q","mode":""})).unwrap_err();
        assert!(err.contains("'mode'"), "got: {err}");
    }

    #[test]
    fn search_request_rejects_non_string_mode() {
        let err = build_search_request("t", &json!({"query":"q","mode": 42})).unwrap_err();
        assert!(
            err.contains("'mode'") && err.contains("string"),
            "got: {err}"
        );
    }

    #[test]
    fn search_request_accepts_all_advertised_modes() {
        for m in SEARCH_VALID_MODES {
            let req = build_search_request("t", &json!({"query":"q","mode": m})).unwrap();
            assert_eq!(req.mode.as_deref(), Some(*m));
        }
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
    fn search_request_rejects_whitespace_only_query() {
        // " \t\n" 等の空白だけのクエリも実質的に空。trim してから判定する。
        let err = build_search_request("t", &json!({"query": "   \t\n"})).unwrap_err();
        assert!(err.contains("query"), "got: {err}");
    }

    #[test]
    fn search_request_trims_surrounding_whitespace_in_query() {
        let req = build_search_request("t", &json!({"query": "  hello world  "})).unwrap();
        assert_eq!(req.text, "hello world");
    }

    #[test]
    fn search_request_rejects_non_object_arguments() {
        // MCP spec: tools/call の arguments は object。string/array/null を渡された
        // ら "missing required argument" ではなく shape error として返す。
        for bad in [json!("oops"), json!([1, 2]), json!(null), json!(42)] {
            let err = build_search_request("t", &bad).unwrap_err();
            assert!(
                err.contains("'arguments'") && err.contains("object"),
                "input {bad:?} should be rejected as wrong-shape; got: {err}"
            );
        }
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
        // PermissionDenied は client-safe code なので message も含まれる。
        assert!(text.contains("no access"), "got: {text}");
    }

    #[test]
    fn tonic_error_content_redacts_internal_message() {
        // Internal は backend trace / file path / stack を含む可能性があるので
        // message は public surface に流さず、code だけ返す。
        let status = tonic::Status::internal(
            "SQL: SELECT * FROM secrets WHERE token='...' caused panic at vegapunk/src/db.rs:42",
        );
        let v = tonic_error_content("Search", status);
        assert_eq!(v["isError"], true);
        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Internal"), "got: {text}");
        assert!(
            text.contains("details suppressed"),
            "internal message must be suppressed: {text}"
        );
        assert!(
            !text.contains("SELECT") && !text.contains("vegapunk/src/db.rs"),
            "internal details leaked into client error: {text}"
        );
    }

    #[test]
    fn tonic_error_content_redacts_unknown_message() {
        let status =
            tonic::Status::unknown("backend panicked: thread 'tokio-runtime' panicked at ...");
        let v = tonic_error_content("Search", status);
        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Unknown"));
        assert!(text.contains("details suppressed"));
        assert!(!text.contains("panicked"), "got: {text}");
    }

    #[test]
    fn tonic_error_content_keeps_invalid_argument_message() {
        // InvalidArgument は何を直せばいいか伝える必要があるので message を保持。
        let status = tonic::Status::invalid_argument("schema 'foo' not found");
        let v = tonic_error_content("GetSchema", status);
        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("InvalidArgument"));
        assert!(text.contains("schema 'foo' not found"), "got: {text}");
    }

    #[test]
    fn invalid_args_content_marks_iserror_true() {
        let v = invalid_args_content("search", "missing 'query'");
        assert_eq!(v["isError"], true);
        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("search"));
        assert!(text.contains("missing 'query'"));
    }

    // ── ingest ──────────────────────────────────────────────────────────

    fn good_message() -> Value {
        json!({
            "text": "hello",
            "metadata": {
                "source_type": "slack",
                "author": "ryugo",
                "channel": "#general",
                "timestamp": "2026-06-02T10:00:00+09:00",
            }
        })
    }

    #[test]
    fn ingest_request_uses_user_schema_and_ignores_args_schema() {
        let args = json!({ "messages": [good_message()], "schema": "evil" });
        let req = build_ingest_request("alice-tenant", &args).unwrap();
        assert_eq!(req.schema, "alice-tenant");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].text, "hello");
        let md = req.messages[0].metadata.as_ref().unwrap();
        assert_eq!(md.source_type, "slack");
        assert_eq!(md.author, "ryugo");
        assert_eq!(md.channel, "#general");
        assert_eq!(md.timestamp, "2026-06-02T10:00:00+09:00");
    }

    #[test]
    fn ingest_request_rejects_non_object_arguments() {
        for bad in [json!("oops"), json!([1, 2]), json!(null), json!(42)] {
            let err = build_ingest_request("t", &bad).unwrap_err();
            assert!(err.contains("'arguments'"), "input {bad:?}: {err}");
        }
    }

    #[test]
    fn ingest_request_rejects_missing_messages() {
        let err = build_ingest_request("t", &json!({})).unwrap_err();
        assert!(err.contains("'messages'"), "got: {err}");
    }

    #[test]
    fn ingest_request_rejects_non_array_messages() {
        let err = build_ingest_request("t", &json!({ "messages": "nope" })).unwrap_err();
        assert!(err.contains("array"), "got: {err}");
    }

    #[test]
    fn ingest_request_rejects_empty_messages() {
        // 空配列で gRPC を叩いても無意味、early reject。
        let err = build_ingest_request("t", &json!({ "messages": [] })).unwrap_err();
        assert!(err.contains("at least one"), "got: {err}");
    }

    #[test]
    fn ingest_request_rejects_non_object_message_item() {
        let err = build_ingest_request("t", &json!({ "messages": ["nope"] })).unwrap_err();
        assert!(err.contains("messages[0]"), "got: {err}");
        assert!(err.contains("object"), "got: {err}");
    }

    #[test]
    fn ingest_request_rejects_empty_text() {
        let mut bad = good_message();
        bad["text"] = json!("   ");
        let err = build_ingest_request("t", &json!({ "messages": [bad] })).unwrap_err();
        assert!(err.contains("messages[0].text"), "got: {err}");
    }

    #[test]
    fn ingest_request_rejects_missing_metadata() {
        let bad = json!({ "text": "hello" });
        let err = build_ingest_request("t", &json!({ "messages": [bad] })).unwrap_err();
        assert!(err.contains("metadata"), "got: {err}");
    }

    #[test]
    fn ingest_request_requires_each_metadata_field() {
        // source_type / author / channel / timestamp — proto では全て required。
        for field in ["source_type", "author", "channel", "timestamp"] {
            let mut msg = good_message();
            msg["metadata"].as_object_mut().unwrap().remove(field);
            let err = build_ingest_request("t", &json!({ "messages": [msg] })).unwrap_err();
            assert!(
                err.contains(field) && err.contains("metadata"),
                "removing {field} should report it; got: {err}"
            );
        }
    }

    #[test]
    fn ingest_request_accepts_optional_id_and_metadata_fields() {
        let mut msg = good_message();
        msg["id"] = json!("client-supplied-id");
        msg["metadata"]["author_id"] = json!("U123");
        msg["metadata"]["channel_id"] = json!("C123");
        msg["metadata"]["thread_id"] = json!("1.2");
        let req = build_ingest_request("t", &json!({ "messages": [msg] })).unwrap();
        let m = &req.messages[0];
        assert_eq!(m.id.as_deref(), Some("client-supplied-id"));
        let md = m.metadata.as_ref().unwrap();
        assert_eq!(md.author_id.as_deref(), Some("U123"));
        assert_eq!(md.channel_id.as_deref(), Some("C123"));
        assert_eq!(md.thread_id.as_deref(), Some("1.2"));
    }

    #[test]
    fn ingest_request_reports_index_for_bad_item_in_batch() {
        // 1st item OK, 2nd item missing metadata → エラーは index 1 を示す。
        let mut bad = good_message();
        bad.as_object_mut().unwrap().remove("metadata");
        let err =
            build_ingest_request("t", &json!({ "messages": [good_message(), bad] })).unwrap_err();
        assert!(err.contains("messages[1]"), "got: {err}");
    }

    // ── ingest_raw ──────────────────────────────────────────────────────

    fn good_raw_args() -> Value {
        json!({
            "text": "hello world",
            "metadata": {
                "source_type": "wiki",
            }
        })
    }

    #[test]
    fn ingest_raw_request_uses_user_schema_and_ignores_args_schema() {
        let mut args = good_raw_args();
        args["schema"] = json!("evil");
        let req = build_ingest_raw_request("alice-tenant", &args).unwrap();
        assert_eq!(req.schema, "alice-tenant");
        assert_eq!(req.text, "hello world");
        let md = req.metadata.as_ref().unwrap();
        assert_eq!(md.source_type, "wiki");
        assert!(md.author.is_none());
        assert!(md.channel.is_none());
        assert!(md.timestamp.is_none());
    }

    #[test]
    fn ingest_raw_request_rejects_empty_text() {
        let mut args = good_raw_args();
        args["text"] = json!("   ");
        let err = build_ingest_raw_request("t", &args).unwrap_err();
        assert!(err.contains("'arguments.text'"), "got: {err}");
    }

    #[test]
    fn ingest_raw_request_rejects_missing_metadata() {
        let err = build_ingest_raw_request("t", &json!({ "text": "x" })).unwrap_err();
        assert!(err.contains("'metadata'"), "got: {err}");
    }

    #[test]
    fn ingest_raw_request_requires_source_type() {
        let err =
            build_ingest_raw_request("t", &json!({ "text": "x", "metadata": {} })).unwrap_err();
        assert!(err.contains("metadata.source_type"), "got: {err}");
    }

    #[test]
    fn ingest_raw_request_accepts_full_metadata() {
        let req = build_ingest_raw_request(
            "t",
            &json!({
                "text": "hello",
                "metadata": {
                    "source_type": "wiki",
                    "author": "ryugo",
                    "channel": "kb",
                    "timestamp": "2026-06-02T10:00:00+09:00",
                }
            }),
        )
        .unwrap();
        let md = req.metadata.unwrap();
        assert_eq!(md.author.as_deref(), Some("ryugo"));
        assert_eq!(md.channel.as_deref(), Some("kb"));
        assert_eq!(md.timestamp.as_deref(), Some("2026-06-02T10:00:00+09:00"));
    }
}
