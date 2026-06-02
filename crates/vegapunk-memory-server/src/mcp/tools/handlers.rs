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
    AttributeFilter, GetSchemaRequest, GetStatsRequest, IngestMessage, IngestRawMetadata,
    IngestRawRequest, IngestRequest, ListSchemasRequest, MessageMetadata, QueryNodesRequest,
    SchemaListItem, SearchRequest, SearchResultItem,
};
use vegapunk_memory_auth::middleware::AuthenticatedUser;

use crate::state::AppState;

use super::{
    ATTRIBUTE_FILTER_VALID_OPS, QUERY_NODES_LIMIT_DEFAULT, QUERY_NODES_LIMIT_MAX,
    QUERY_NODES_LIMIT_MIN, QUERY_NODES_SORT_ORDER_DEFAULT, QUERY_NODES_VALID_SORT_ORDERS,
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
/// 空白だけの値 (`""` や `"   "`) は「省略」と同じ扱いにする — 例えば
/// `ingest_raw.metadata.timestamp` の advertised default は「省略時 server time」
/// だが、`""` を Some として通すと server time fallback が走らず空文字が
/// proto に乗ってしまう。trim して empty なら None に下す。
fn optional_str_field(
    obj: &Map<String, Value>,
    field: &str,
    owner: &str,
) -> Result<Option<String>, String> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_str() {
            Some(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_string()))
                }
            }
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

/// 単一の AttributeFilter (key / op / value) を build する。op は enum guard。
fn build_attribute_filter(item: &Value, owner: &str) -> Result<AttributeFilter, String> {
    let m = item
        .as_object()
        .ok_or_else(|| format!("'{owner}' must be an object"))?;
    let key = require_str_field(m, "key", owner)?;
    let op = require_str_field(m, "op", owner)?;
    if !ATTRIBUTE_FILTER_VALID_OPS.contains(&op.as_str()) {
        return Err(format!(
            "'{owner}.op' must be one of {ATTRIBUTE_FILTER_VALID_OPS:?}, got {op:?}"
        ));
    }
    // value は require_str_field を使わず、空文字も明示的に許容する
    // (eq "" のような検索が想定される)。
    let value = match m.get("value") {
        None | Some(Value::Null) => {
            return Err(format!("missing required '{owner}.value'"));
        }
        Some(v) => v
            .as_str()
            .ok_or_else(|| format!("'{owner}.value' must be a string"))?
            .to_string(),
    };
    Ok(AttributeFilter { key, op, value })
}

/// `arguments.filters` (optional JSON array) を `Vec<AttributeFilter>` に変換。
/// `filters` が無い / null なら空配列を返す。query_nodes / stats で共通。
fn build_attribute_filters_from(
    obj: &Map<String, Value>,
    owner: &str,
) -> Result<Vec<AttributeFilter>, String> {
    let owner_field = format!("{owner}.filters");
    match obj.get("filters") {
        None | Some(Value::Null) => Ok(vec![]),
        Some(v) => {
            let arr = v
                .as_array()
                .ok_or_else(|| format!("'{owner_field}' must be an array"))?;
            arr.iter()
                .enumerate()
                .map(|(i, f)| build_attribute_filter(f, &format!("{owner_field}[{i}]")))
                .collect()
        }
    }
}

/// optional integer field を i32 にまで降ろす。null/missing は default を返す
/// (default が None ならそのまま None)。範囲外は err。
fn optional_bounded_i32(
    obj: &Map<String, Value>,
    field: &str,
    min: i64,
    max: i64,
    default: Option<i32>,
    owner: &str,
) -> Result<Option<i32>, String> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => {
            let n = v
                .as_i64()
                .ok_or_else(|| format!("'{owner}.{field}' must be an integer"))?;
            if !(min..=max).contains(&n) {
                return Err(format!(
                    "'{owner}.{field}' must be between {min} and {max}, got {n}"
                ));
            }
            Ok(Some(n as i32))
        }
    }
}

/// `query_nodes` argument を `QueryNodesRequest` に詰める。`schema` は user の
/// vegapunk_schema で強制注入する。
pub(super) fn build_query_nodes_request(
    user_schema: &str,
    args: &Value,
) -> Result<QueryNodesRequest, String> {
    let args = require_object_args(args)?;
    let node_type = require_str_field(args, "node_type", "arguments")?;
    let filters = build_attribute_filters_from(args, "arguments")?;
    let sort_by = optional_str_field(args, "sort_by", "arguments")?;
    let sort_order = {
        let raw = match args.get("sort_order") {
            None | Some(Value::Null) => QUERY_NODES_SORT_ORDER_DEFAULT,
            Some(v) => v
                .as_str()
                .ok_or_else(|| "'arguments.sort_order' must be a string".to_string())?,
        };
        if !QUERY_NODES_VALID_SORT_ORDERS.contains(&raw) {
            return Err(format!(
                "'arguments.sort_order' must be one of {QUERY_NODES_VALID_SORT_ORDERS:?}, got {raw:?}"
            ));
        }
        Some(raw.to_string())
    };
    let limit = optional_bounded_i32(
        args,
        "limit",
        QUERY_NODES_LIMIT_MIN,
        QUERY_NODES_LIMIT_MAX,
        Some(QUERY_NODES_LIMIT_DEFAULT),
        "arguments",
    )?;
    // offset は 0..=i32::MAX で受ける。default 0。
    let offset = optional_bounded_i32(args, "offset", 0, i32::MAX as i64, Some(0), "arguments")?;
    Ok(QueryNodesRequest {
        schema: user_schema.to_string(),
        node_type,
        filters,
        sort_by,
        sort_order,
        limit,
        offset,
        traverse: None,
    })
}

/// `stats` argument を `GetStatsRequest` に詰める。proto の `schema` は optional
/// で「empty = cross-schema total (admin Dashboard default)」だが、wrapper では
/// user.vegapunk_schema を必ずセットして tenant 越境を遮断する。
pub(super) fn build_get_stats_request(
    user_schema: &str,
    args: &Value,
) -> Result<GetStatsRequest, String> {
    let args = require_object_args(args)?;
    let node_type = optional_str_field(args, "node_type", "arguments")?;
    let filters = build_attribute_filters_from(args, "arguments")?;
    Ok(GetStatsRequest {
        schema: Some(user_schema.to_string()),
        node_type,
        filters,
    })
}

pub(super) async fn query_nodes(state: &AppState, user: &AuthenticatedUser, args: Value) -> Value {
    let request = match build_query_nodes_request(&user.vegapunk_schema, &args) {
        Ok(r) => r,
        Err(e) => return invalid_args_content("query_nodes", &e),
    };
    let mut client = state.vegapunk.clone();
    match client.query_nodes(request).await {
        Err(status) => tonic_error_content("QueryNodes", status),
        Ok(resp) => {
            let resp = resp.into_inner();
            let nodes: Vec<Value> = resp
                .nodes
                .iter()
                .map(|n| {
                    json!({
                        "node_id": n.node_id,
                        "node_type": n.node_type,
                        "attributes": n.attributes,
                    })
                })
                .collect();
            success_content(json!({
                "nodes": nodes,
                "total_count": resp.total_count,
            }))
        }
    }
}

/// vegapunk の `ListSchemas` は全 schema を返す admin-ish RPC だが、wrapper
/// 単位では user 1 人につき 1 schema (= `user.vegapunk_schema`) しか紐付かない
/// 設計なので、結果を user の schema 名に厳格に filter する。これがないと、
/// 他 tenant の schema 名と yaml が caller に丸見えになる (情報漏洩)。
///
/// filter 実装は cross-tenant 防止の要なので、pure な
/// [`filter_schemas_for_user`] に切り出して unit test で pin する。
pub(super) async fn list_schemas(state: &AppState, user: &AuthenticatedUser) -> Value {
    let mut client = state.vegapunk.clone();
    match client.list_schemas(ListSchemasRequest {}).await {
        Err(status) => tonic_error_content("ListSchemas", status),
        Ok(resp) => {
            let resp = resp.into_inner();
            let schemas = filter_schemas_for_user(&resp.schemas, &user.vegapunk_schema);
            success_content(json!({ "schemas": schemas }))
        }
    }
}

/// `ListSchemas` レスポンスから、要求 user の vegapunk_schema に **完全一致**
/// する entry のみを抜き出して JSON に整形する。`list_schemas` ハンドラの
/// security-critical な部分なので、handler 自体を gRPC 込みで mock しなくても
/// この pure function だけで cross-tenant filter の挙動を試験できる。
fn filter_schemas_for_user(schemas: &[SchemaListItem], user_schema: &str) -> Vec<Value> {
    schemas
        .iter()
        .filter(|s| s.name == user_schema)
        .map(|s| {
            json!({
                "name": s.name,
                "version": s.version,
                "description": s.description,
                "schema_yaml": s.schema_yaml,
            })
        })
        .collect()
}

pub(super) async fn stats(state: &AppState, user: &AuthenticatedUser, args: Value) -> Value {
    let request = match build_get_stats_request(&user.vegapunk_schema, &args) {
        Ok(r) => r,
        Err(e) => return invalid_args_content("stats", &e),
    };
    let mut client = state.vegapunk.clone();
    match client.get_stats(request).await {
        Err(status) => tonic_error_content("GetStats", status),
        Ok(resp) => {
            let resp = resp.into_inner();
            success_content(json!({
                "node_count": resp.node_count,
                "edge_count": resp.edge_count,
                "vector_count": resp.vector_count,
                "community_count": resp.community_count,
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
    fn ingest_request_treats_empty_optional_metadata_as_omitted() {
        // ingest 側の optional metadata field (author_id, channel_id, thread_id)
        // も同じく空白は None として落とす。
        let mut msg = good_message();
        msg["metadata"]["author_id"] = json!("");
        msg["metadata"]["channel_id"] = json!("   ");
        msg["metadata"]["thread_id"] = json!("\t\n");
        let req = build_ingest_request("t", &json!({ "messages": [msg] })).unwrap();
        let md = req.messages[0].metadata.as_ref().unwrap();
        assert!(md.author_id.is_none());
        assert!(md.channel_id.is_none());
        assert!(md.thread_id.is_none());
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
    fn ingest_raw_request_treats_empty_optional_fields_as_omitted() {
        // `""` や `"   "` を `Some("")` のまま乗せると、proto 側で
        // "timestamp omitted → server time" の fallback が効かない。
        // optional_str_field が trim + empty→None に下すことを ingest_raw
        // のフル経路でも確認する。
        let req = build_ingest_raw_request(
            "t",
            &json!({
                "text": "hello",
                "metadata": {
                    "source_type": "wiki",
                    "author": "  ",
                    "channel": "",
                    "timestamp": " \t ",
                }
            }),
        )
        .unwrap();
        let md = req.metadata.unwrap();
        assert!(md.author.is_none(), "got: {:?}", md.author);
        assert!(md.channel.is_none(), "got: {:?}", md.channel);
        assert!(md.timestamp.is_none(), "got: {:?}", md.timestamp);
    }

    // ── query_nodes ─────────────────────────────────────────────────────

    #[test]
    fn query_nodes_request_uses_user_schema_and_ignores_args_schema() {
        let args = json!({ "node_type": "Message", "schema": "evil" });
        let req = build_query_nodes_request("alice-tenant", &args).unwrap();
        assert_eq!(req.schema, "alice-tenant");
        assert_eq!(req.node_type, "Message");
        // defaults
        assert_eq!(req.limit, Some(QUERY_NODES_LIMIT_DEFAULT));
        assert_eq!(req.offset, Some(0));
        assert_eq!(
            req.sort_order.as_deref(),
            Some(QUERY_NODES_SORT_ORDER_DEFAULT)
        );
        assert!(req.filters.is_empty());
        assert!(req.sort_by.is_none());
        assert!(req.traverse.is_none());
    }

    #[test]
    fn query_nodes_request_rejects_missing_node_type() {
        let err = build_query_nodes_request("t", &json!({})).unwrap_err();
        assert!(err.contains("node_type"), "got: {err}");
    }

    #[test]
    fn query_nodes_request_maps_filters() {
        let req = build_query_nodes_request(
            "t",
            &json!({
                "node_type": "Message",
                "filters": [
                    {"key": "channel_id", "op": "eq", "value": "C123"},
                    {"key": "timestamp", "op": "gte", "value": "2026-01-01T00:00:00Z"}
                ]
            }),
        )
        .unwrap();
        assert_eq!(req.filters.len(), 2);
        assert_eq!(req.filters[0].key, "channel_id");
        assert_eq!(req.filters[0].op, "eq");
        assert_eq!(req.filters[0].value, "C123");
        assert_eq!(req.filters[1].op, "gte");
    }

    #[test]
    fn query_nodes_request_rejects_unknown_filter_op() {
        let err = build_query_nodes_request(
            "t",
            &json!({
                "node_type": "Message",
                "filters": [{"key": "x", "op": "contains", "value": "y"}]
            }),
        )
        .unwrap_err();
        assert!(err.contains("filters[0].op"), "got: {err}");
        assert!(err.contains("contains"), "got: {err}");
    }

    #[test]
    fn query_nodes_request_rejects_non_array_filters() {
        let err = build_query_nodes_request("t", &json!({ "node_type": "M", "filters": "nope" }))
            .unwrap_err();
        assert!(
            err.contains("filters") && err.contains("array"),
            "got: {err}"
        );
    }

    #[test]
    fn query_nodes_request_filter_value_accepts_empty_string() {
        // `value` は eq "" のような検索用に空文字を許す (key / op は不可)。
        let req = build_query_nodes_request(
            "t",
            &json!({
                "node_type": "M",
                "filters": [{"key": "k", "op": "eq", "value": ""}]
            }),
        )
        .unwrap();
        assert_eq!(req.filters[0].value, "");
    }

    #[test]
    fn query_nodes_request_rejects_unknown_sort_order() {
        let err =
            build_query_nodes_request("t", &json!({ "node_type": "M", "sort_order": "rand" }))
                .unwrap_err();
        // 他フィールドと同じ `'arguments.<field>'` 形式で報告する。
        assert!(err.contains("'arguments.sort_order'"), "got: {err}");
    }

    #[test]
    fn query_nodes_request_rejects_non_string_sort_order_with_fully_qualified_path() {
        let err = build_query_nodes_request("t", &json!({ "node_type": "M", "sort_order": 1 }))
            .unwrap_err();
        assert!(err.contains("'arguments.sort_order'"), "got: {err}");
        assert!(err.contains("string"), "got: {err}");
    }

    #[test]
    fn query_nodes_request_rejects_limit_out_of_range() {
        let err =
            build_query_nodes_request("t", &json!({ "node_type": "M", "limit": 0 })).unwrap_err();
        assert!(err.contains("limit"), "got: {err}");

        let err = build_query_nodes_request("t", &json!({ "node_type": "M", "limit": 1001 }))
            .unwrap_err();
        assert!(err.contains("limit"), "got: {err}");
    }

    #[test]
    fn query_nodes_request_rejects_negative_offset() {
        let err =
            build_query_nodes_request("t", &json!({ "node_type": "M", "offset": -1 })).unwrap_err();
        assert!(err.contains("offset"), "got: {err}");
    }

    #[test]
    fn query_nodes_request_accepts_full_args() {
        let req = build_query_nodes_request(
            "t",
            &json!({
                "node_type": "Message",
                "filters": [{"key": "k", "op": "lt", "value": "v"}],
                "sort_by": "timestamp",
                "sort_order": "asc",
                "limit": 25,
                "offset": 5
            }),
        )
        .unwrap();
        assert_eq!(req.sort_by.as_deref(), Some("timestamp"));
        assert_eq!(req.sort_order.as_deref(), Some("asc"));
        assert_eq!(req.limit, Some(25));
        assert_eq!(req.offset, Some(5));
    }

    // ── stats ──────────────────────────────────────────────────────────

    #[test]
    fn stats_request_uses_user_schema_and_ignores_args_schema() {
        let req = build_get_stats_request("alice", &json!({ "schema": "evil" })).unwrap();
        assert_eq!(req.schema.as_deref(), Some("alice"));
        assert!(req.node_type.is_none());
        assert!(req.filters.is_empty());
    }

    #[test]
    fn stats_request_always_sets_schema_even_when_args_empty() {
        // proto では schema は optional だが、wrapper は cross-tenant 防止のため
        // 必ず user.vegapunk_schema をセットする ("admin Dashboard default" 経路
        // に漏らさない)。
        let req = build_get_stats_request("alice", &json!({})).unwrap();
        assert_eq!(req.schema.as_deref(), Some("alice"));
    }

    #[test]
    fn stats_request_accepts_filters_and_node_type() {
        let req = build_get_stats_request(
            "t",
            &json!({
                "node_type": "Message",
                "filters": [{"key": "channel", "op": "eq", "value": "C1"}]
            }),
        )
        .unwrap();
        assert_eq!(req.node_type.as_deref(), Some("Message"));
        assert_eq!(req.filters.len(), 1);
    }

    // ── list_schemas filter (cross-tenant guard) ────────────────────────

    fn schema_item(name: &str) -> SchemaListItem {
        SchemaListItem {
            name: name.to_string(),
            version: 1,
            description: format!("schema for {name}"),
            schema_yaml: format!("name: {name}\n"),
        }
    }

    #[test]
    fn filter_schemas_returns_only_matching_user_schema() {
        let schemas = vec![
            schema_item("alice-tenant"),
            schema_item("bob-tenant"),
            schema_item("eve-tenant"),
        ];
        let out = filter_schemas_for_user(&schemas, "bob-tenant");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["name"], "bob-tenant");
        assert_eq!(out[0]["schema_yaml"], "name: bob-tenant\n");
    }

    #[test]
    fn filter_schemas_drops_other_tenants_completely() {
        // alice / eve は出力に絶対に含まれない (cross-tenant guard の核)。
        let schemas = vec![schema_item("alice-tenant"), schema_item("eve-tenant")];
        let out = filter_schemas_for_user(&schemas, "bob-tenant");
        assert!(out.is_empty(), "got: {out:?}");
    }

    #[test]
    fn filter_schemas_uses_exact_match_not_substring() {
        // "bob-tenant" を要求した時に "bob-tenant-v2" のような prefix-match が
        // 入らないこと (substring leak の典型パターンを潰す)。
        let schemas = vec![
            schema_item("bob-tenant"),
            schema_item("bob-tenant-v2"),
            schema_item("bob"),
        ];
        let out = filter_schemas_for_user(&schemas, "bob-tenant");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["name"], "bob-tenant");
    }

    #[test]
    fn filter_schemas_returns_empty_when_user_has_no_match() {
        let schemas = vec![schema_item("alice-tenant")];
        let out = filter_schemas_for_user(&schemas, "bob-tenant");
        assert!(out.is_empty());
    }

    #[test]
    fn stats_request_rejects_unknown_filter_op() {
        let err = build_get_stats_request(
            "t",
            &json!({ "filters": [{"key": "k", "op": "like", "value": "v"}] }),
        )
        .unwrap_err();
        assert!(err.contains("filters[0].op"), "got: {err}");
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
