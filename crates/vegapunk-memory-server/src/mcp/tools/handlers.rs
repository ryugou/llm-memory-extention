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
    AttributeFilter, GetSchemaRequest, GetStatsRequest, GetTraceableChainRequest, IngestMessage,
    IngestRawMetadata, IngestRawRequest, IngestRequest, ListSchemasRequest, MessageMetadata,
    QueryNodesRequest, SchemaListItem, SearchRequest, SearchResultItem,
};
use vegapunk_memory_auth::middleware::AuthenticatedUser;

use crate::state::AppState;

use super::{
    ATTRIBUTE_FILTER_VALID_OPS, QUERY_NODES_LIMIT_DEFAULT, QUERY_NODES_LIMIT_MAX,
    QUERY_NODES_LIMIT_MIN, QUERY_NODES_SORT_ORDER_DEFAULT, QUERY_NODES_VALID_SORT_ORDERS,
    SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX, SEARCH_LIMIT_MIN, SEARCH_MODE_DEFAULT,
    SEARCH_VALID_MODES, TRACEABLE_CHAIN_MAX_DEPTH_DEFAULT, TRACEABLE_CHAIN_MAX_DEPTH_MAX,
    TRACEABLE_CHAIN_MAX_DEPTH_MIN,
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

/// vegapunk が定義する schema YAML のうち、wrapper が「同名重複」の
/// 検出対象にする node_type 群。Project / Person / Specification / Topic は
/// 「概念として 1 つしか存在しないべき」固有名詞性の高い node。Decision /
/// Message / Thread のような事実イベント系は除外する (= 同名でも別 entity)。
const DEDUP_SCAN_NODE_TYPES: &[&str] = &["Project", "Person", "Specification", "Topic"];

/// 1 schema あたり最大 fetch する entity 件数。多いと query_nodes が重く
/// なるが、4 種 × 2 schema = 8 query なので合計上限は 8000 件。これを超える
/// 規模になったら別 PR で paging / 増分 fetch を入れる。
const DEDUP_FETCH_LIMIT_PER_TYPE: i32 = 1000;

/// 表記揺れ判定のための正規化キー。Case-insensitive + 前後空白除去のみ。
/// Unicode NFKC (全角半角統一) は次フェーズ。
fn normalize_entity_key(s: &str) -> String {
    s.trim().to_lowercase()
}

/// `is_word_char` 風の判定。alphanumeric + underscore に CJK (= 連続した
/// 日本語名で word boundary を区切りたくないため) を含める粗い定義。
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `text` に `needle` (= 正規化済キー) が **word boundary 境界** で含まれて
/// いるかを case-insensitive で判定する。`Vegapunk` で「Vegapunk Inc.」は
/// 検出するが「Vegapunker」は検出しない。
///
/// 制約: `text.to_lowercase()` の byte length が元と異なる場合 (例: Turkish
/// dotted I `İ` (2 bytes) → `i\u{0307}` (3 bytes)) は byte offset が元 text
/// にマップできないため conservative に `false` (= no match) を返す。
/// non-ASCII の真の case 比較は NFC normalize 込みの別フェーズで扱う。
fn word_boundary_contains_normalized(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let text_lc = text.to_lowercase();
    if text_lc.len() != text.len() {
        return false;
    }
    let mut start = 0usize;
    while let Some(rel) = text_lc[start..].find(needle) {
        let abs = start + rel;
        let end = abs + needle.len();
        let before_ok = match text_lc[..abs].chars().next_back() {
            None => true,
            Some(c) => !is_word_char(c),
        };
        let after_ok = match text_lc[end..].chars().next() {
            None => true,
            Some(c) => !is_word_char(c),
        };
        if before_ok && after_ok {
            return true;
        }
        // 1 byte 進めるのではなく、ASCII safe な char-boundary 上で進める
        start = abs
            + text_lc[abs..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
    }
    false
}

/// `text` の中に `needle_normalized` (= 正規化済キー) が word boundary で
/// 出現する箇所を、`canonical` で **case-preserving に置換** した文字列を返す。
/// 出現がなければ `None`。複数出現は全置換。
///
/// 制約: `word_boundary_contains_normalized` と同じく、`to_lowercase()` で
/// byte length が変わるケースでは元 text に offset を反映できないため no-op
/// (= `None`) を返す。NFC 正規化込みの真の対応は別フェーズ。
fn replace_word_case_insensitive(
    text: &str,
    needle_normalized: &str,
    canonical: &str,
) -> Option<String> {
    if needle_normalized.is_empty() {
        return None;
    }
    let text_lc = text.to_lowercase();
    if text_lc.len() != text.len() {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut found_any = false;
    let mut search_from = 0usize;
    while let Some(rel) = text_lc[search_from..].find(needle_normalized) {
        let abs = search_from + rel;
        let end = abs + needle_normalized.len();
        let before_ok = match text_lc[..abs].chars().next_back() {
            None => true,
            Some(c) => !is_word_char(c),
        };
        let after_ok = match text_lc[end..].chars().next() {
            None => true,
            Some(c) => !is_word_char(c),
        };
        if before_ok && after_ok {
            out.push_str(&text[cursor..abs]);
            out.push_str(canonical);
            cursor = end;
            found_any = true;
            search_from = end;
        } else {
            search_from = abs
                + text_lc[abs..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(1);
        }
    }
    if found_any {
        out.push_str(&text[cursor..]);
        Some(out)
    } else {
        None
    }
}

/// vegapunk gRPC `query_nodes` を直接叩いて、指定 schema / node_type の
/// `attributes.name` 一覧を取り出す。失敗時は warn + 空 Vec を返す
/// (= dedup pre-check は best-effort で、失敗しても ingest を止めない)。
async fn fetch_entity_names(state: &AppState, schema: &str, node_type: &str) -> Vec<String> {
    let req = QueryNodesRequest {
        schema: schema.to_string(),
        node_type: node_type.to_string(),
        filters: vec![],
        sort_by: None,
        sort_order: None,
        limit: Some(DEDUP_FETCH_LIMIT_PER_TYPE),
        offset: Some(0),
        traverse: None,
    };
    let mut client = state.vegapunk.clone();
    match client.query_nodes(req).await {
        Ok(resp) => resp
            .into_inner()
            .nodes
            .into_iter()
            .filter_map(|n| n.attributes.get("name").cloned())
            .filter(|n| !n.trim().is_empty())
            .collect(),
        Err(status) => {
            tracing::warn!(
                schema = %schema,
                node_type = %node_type,
                code = ?status.code(),
                message = %status.message(),
                "dedup pre-check: query_nodes failed (continuing without it)",
            );
            Vec::new()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EntityRef {
    pub name: String,
    pub node_type: String,
    pub schema: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum IngestPreCheck {
    /// shared (= 共有 schema) に同名 entity が既存するので ingest を抑制する。
    BlockedByShared { hit: EntityRef },
    /// personal schema に同名 entity が既存する。`new_text` は表記揺れを
    /// canonical name に統一した rewrite 後 text。同名一致した entity のリスト
    /// は `rewrites` に。
    Rewritten {
        new_text: String,
        rewrites: Vec<EntityRef>,
    },
    /// 重複なし。そのまま ingest する。
    Proceed,
}

/// `collect_dedup_catalogue` の戻り値: 1 リクエストで一度だけ fetch すれば
/// 良い entity 一覧。`ingest` の batch 内では同一 catalogue を全 message で
/// 共有して N×fetch を避ける。
#[derive(Debug, Default, Clone)]
pub(super) struct DedupCatalogue {
    pub shared: Vec<EntityRef>,
    pub personal: Vec<EntityRef>,
}

/// dedup pre-check 用の entity 一覧を **1 リクエスト 1 回だけ** 取得する。
/// `DEDUP_SCAN_NODE_TYPES` × {shared, personal} の組み合わせ (= 上限 8) を
/// query_nodes で集める。fetch 失敗 (個別 node_type / schema 単位) は
/// `fetch_entity_names` の中で warn + 空 Vec に落とすので、pre-check 全体を
/// 止めずに済む。
pub(super) async fn collect_dedup_catalogue(
    state: &AppState,
    user: &AuthenticatedUser,
) -> DedupCatalogue {
    let shared_schema = state.cfg.shared_schema_name.as_str();
    let mut shared: Vec<EntityRef> = Vec::new();
    let mut personal: Vec<EntityRef> = Vec::new();
    for node_type in DEDUP_SCAN_NODE_TYPES {
        for name in fetch_entity_names(state, shared_schema, node_type).await {
            shared.push(EntityRef {
                name,
                node_type: (*node_type).to_string(),
                schema: shared_schema.to_string(),
            });
        }
        for name in fetch_entity_names(state, &user.vegapunk_schema, node_type).await {
            personal.push(EntityRef {
                name,
                node_type: (*node_type).to_string(),
                schema: user.vegapunk_schema.clone(),
            });
        }
    }
    DedupCatalogue { shared, personal }
}

/// `text` に対し catalogue を当てて pre-check 結果を返す **pure な scan**。
/// I/O 無しなので per-message ループで何度呼んでも safe。
///
/// 判定順:
/// 1. shared にヒット → 抑制 (`BlockedByShared`)。複数ヒットでも 1 件返す。
/// 2. shared 無し / personal にヒット → 表記揺れと判断、canonical name に rewrite。
/// 3. いずれも該当無し → `Proceed`。
pub(super) fn scan_text_with_catalogue(catalogue: &DedupCatalogue, text: &str) -> IngestPreCheck {
    for ent in &catalogue.shared {
        let key = normalize_entity_key(&ent.name);
        if word_boundary_contains_normalized(text, &key) {
            return IngestPreCheck::BlockedByShared { hit: ent.clone() };
        }
    }

    let mut new_text = text.to_string();
    let mut rewrites = Vec::new();
    for ent in &catalogue.personal {
        let key = normalize_entity_key(&ent.name);
        if let Some(updated) = replace_word_case_insensitive(&new_text, &key, &ent.name) {
            if updated != new_text {
                rewrites.push(ent.clone());
                new_text = updated;
            }
        }
    }

    if rewrites.is_empty() {
        IngestPreCheck::Proceed
    } else {
        IngestPreCheck::Rewritten { new_text, rewrites }
    }
}

/// `IngestPreCheck::BlockedByShared` を MCP tool error content に変換する。
/// `method` は debug 用に "ingest" / "ingest_raw" / "ingest (messages[i])" 等
/// を渡す (= 抑制対象が確定するためどの handler / どの message が原因か残す)。
pub(super) fn shared_dedup_block_content(method: &str, hit: &EntityRef) -> Value {
    let body = format!(
        "{method} blocked: '{}' ({}) already exists in schema '{}'. \
         Reference the existing entity instead of re-ingesting it; this avoids \
         cross-schema duplication.",
        hit.name, hit.node_type, hit.schema
    );
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
    let personal_req = match build_search_request(&user.vegapunk_schema, &args) {
        Ok(r) => r,
        Err(e) => return invalid_args_content("search", &e),
    };
    let mut shared_req = personal_req.clone();
    shared_req.schema = state.cfg.shared_schema_name.clone();
    // limit を merge 後 truncate に使うため、shared に投げる前に控える。
    let merged_limit = personal_req.top_k;

    // own personal + shared の 2 schema を並行で叩いて merge する。
    // cross-tenant guard: wrapper が呼ぶ schema は user.vegapunk_schema と
    // state.cfg.shared_schema_name の 2 つだけで、client 由来の値は使わない。
    let mut client_p = state.vegapunk.clone();
    let mut client_s = state.vegapunk.clone();
    let (personal, shared) =
        tokio::join!(client_p.search(personal_req), client_s.search(shared_req));
    let personal = match personal {
        Ok(r) => r.into_inner(),
        Err(s) => return tonic_error_content("Search (personal)", s),
    };
    // shared は best-effort: 初回ユーザ deployment では未作成 (= NotFound) の
    // ことがあり、その場合 personal の検索結果まで巻き込んで全体エラーに
    // するべきではない。warn だけ残して空の結果として扱う。
    let shared = match shared {
        Ok(r) => Some(r.into_inner()),
        Err(s) => {
            tracing::warn!(
                schema = %state.cfg.shared_schema_name,
                code = ?s.code(),
                "shared Search failed (continuing with personal only)",
            );
            None
        }
    };

    // results を merge。両 schema は別 graph で node id 衝突は無いはずだが、
    // safety net として (type, id) で dedup する。score 降順で sort 後、
    // client が要求した limit (= top_k) で truncate して fan-out で 2 倍に
    // なるのを防ぐ。
    let mut seen = std::collections::HashSet::new();
    let mut all_results: Vec<(f32, Value)> = Vec::new();
    let empty_results = Vec::new();
    let shared_results = shared
        .as_ref()
        .map(|s| &s.results)
        .unwrap_or(&empty_results);
    for item in personal.results.iter().chain(shared_results.iter()) {
        let v = search_result_item_json(item);
        let key = format!("{}:{}", v["type"], v["id"]);
        if seen.insert(key) {
            let score = item.score.unwrap_or(f32::NEG_INFINITY);
            all_results.push((score, v));
        }
    }
    // 降順 sort (= NaN は最下位扱い)。total_cmp は IEEE-754 全順序で確定。
    all_results.sort_by(|a, b| b.0.total_cmp(&a.0));
    if let Some(limit) = merged_limit {
        let limit = limit.max(0) as usize;
        all_results.truncate(limit);
    }
    let merged_results: Vec<Value> = all_results.into_iter().map(|(_, v)| v).collect();

    let empty_sp = Vec::new();
    let shared_sp = shared
        .as_ref()
        .map(|s| &s.similar_patterns)
        .unwrap_or(&empty_sp);
    let similar_patterns: Vec<Value> = personal
        .similar_patterns
        .iter()
        .chain(shared_sp.iter())
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
        "search_ids": {
            "personal": personal.search_id,
            "shared": shared.as_ref().map(|s| s.search_id.clone()),
        },
        "total_count": personal.total_count + shared.as_ref().map(|s| s.total_count).unwrap_or(0),
        "results": merged_results,
        "similar_patterns": similar_patterns,
    }))
}

pub(super) async fn get_schema(state: &AppState, user: &AuthenticatedUser) -> Value {
    let personal_req = build_get_schema_request(&user.vegapunk_schema);
    let shared_req = GetSchemaRequest {
        name: state.cfg.shared_schema_name.clone(),
    };

    let mut client_p = state.vegapunk.clone();
    let mut client_s = state.vegapunk.clone();
    let (personal, shared) = tokio::join!(
        client_p.get_schema(personal_req),
        client_s.get_schema(shared_req),
    );
    // personal は必須、shared は best-effort (= 初回ユーザが居ない時点で shared
    // schema が未作成のままだと NotFound になる、その場合は null を返して
    // クライアントに「shared は無いよ」と伝える)。
    let personal = match personal {
        Ok(r) => r.into_inner(),
        Err(s) => return tonic_error_content("GetSchema (personal)", s),
    };
    let shared_json = match shared {
        Ok(r) => {
            let r = r.into_inner();
            Some(json!({
                "name": r.name,
                "schema_yaml": r.schema_yaml,
                "version": r.version,
                "description": r.description,
            }))
        }
        Err(s) => {
            tracing::warn!(
                schema = %state.cfg.shared_schema_name,
                code = ?s.code(),
                "shared schema GetSchema failed (continuing with personal-only)",
            );
            None
        }
    };

    success_content(json!({
        "personal": {
            "name": personal.name,
            "schema_yaml": personal.schema_yaml,
            "version": personal.version,
            "description": personal.description,
        },
        "shared": shared_json,
    }))
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
    let mut request = match build_ingest_request(&user.vegapunk_schema, &args) {
        Ok(r) => r,
        Err(e) => return invalid_args_content("ingest", &e),
    };
    // dedup pre-check: entity 一覧の fetch を **batch 全体で 1 回**
    // (= 8 query_nodes 上限) に抑え、scan は pure 関数で per-message に
    // 適用する。N messages × 8 fetch の問題を回避する。
    let catalogue = collect_dedup_catalogue(state, user).await;
    for (i, msg) in request.messages.iter_mut().enumerate() {
        match scan_text_with_catalogue(&catalogue, &msg.text) {
            IngestPreCheck::BlockedByShared { hit } => {
                return shared_dedup_block_content(&format!("ingest (messages[{i}])"), &hit);
            }
            IngestPreCheck::Rewritten { new_text, rewrites } => {
                tracing::info!(
                    message_index = i,
                    rewrites = ?rewrites,
                    "ingest text normalized to existing canonical names",
                );
                msg.text = new_text;
            }
            IngestPreCheck::Proceed => {}
        }
    }
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
    let mut request = match build_ingest_raw_request(&user.vegapunk_schema, &args) {
        Ok(r) => r,
        Err(e) => return invalid_args_content("ingest_raw", &e),
    };
    // dedup pre-check。shared 既存は抑制、personal 表記揺れは canonical 化。
    // catalogue 取得は ingest_raw 1 件あたり 1 回。
    let catalogue = collect_dedup_catalogue(state, user).await;
    match scan_text_with_catalogue(&catalogue, &request.text) {
        IngestPreCheck::BlockedByShared { hit } => {
            return shared_dedup_block_content("ingest_raw", &hit);
        }
        IngestPreCheck::Rewritten { new_text, rewrites } => {
            tracing::info!(
                rewrites = ?rewrites,
                "ingest_raw text normalized to existing canonical names",
            );
            request.text = new_text;
        }
        IngestPreCheck::Proceed => {}
    }
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
    let personal_req = match build_query_nodes_request(&user.vegapunk_schema, &args) {
        Ok(r) => r,
        Err(e) => return invalid_args_content("query_nodes", &e),
    };
    let mut shared_req = personal_req.clone();
    shared_req.schema = state.cfg.shared_schema_name.clone();
    // merge 後 truncate に使うため、limit を控える。
    let merged_limit = personal_req.limit;

    let mut client_p = state.vegapunk.clone();
    let mut client_s = state.vegapunk.clone();
    let (personal, shared) = tokio::join!(
        client_p.query_nodes(personal_req),
        client_s.query_nodes(shared_req),
    );
    let personal = match personal {
        Ok(r) => r.into_inner(),
        Err(s) => return tonic_error_content("QueryNodes (personal)", s),
    };
    // shared best-effort: 未作成・空でも personal の結果は返す。
    let shared = match shared {
        Ok(r) => Some(r.into_inner()),
        Err(s) => {
            tracing::warn!(
                schema = %state.cfg.shared_schema_name,
                code = ?s.code(),
                "shared QueryNodes failed (continuing with personal only)",
            );
            None
        }
    };
    // node_id は schema を prefix に持つ ULID なので両 schema 跨ぎで衝突
    // しないはず。safety net として dedup を入れる (先勝ち = personal 優先)。
    // fan-out で limit が 2 倍になるのを防ぐため、merge 後 limit で truncate。
    let mut seen = std::collections::HashSet::new();
    let mut nodes: Vec<Value> = Vec::new();
    let empty_nodes = Vec::new();
    let shared_nodes = shared.as_ref().map(|s| &s.nodes).unwrap_or(&empty_nodes);
    for n in personal.nodes.iter().chain(shared_nodes.iter()) {
        if seen.insert(n.node_id.clone()) {
            nodes.push(json!({
                "node_id": n.node_id,
                "node_type": n.node_type,
                "attributes": n.attributes,
            }));
        }
    }
    if let Some(limit) = merged_limit {
        let limit = limit.max(0) as usize;
        nodes.truncate(limit);
    }
    success_content(json!({
        "nodes": nodes,
        "total_count": personal.total_count + shared.as_ref().map(|s| s.total_count).unwrap_or(0),
    }))
}

/// vegapunk の `ListSchemas` は全 schema を返す admin RPC。wrapper では呼び出し
/// user 視点で意味のある **own personal + shared** の 2 件だけを通す
/// (= 他テナントの schema 名 / yaml は絶対に出さない)。filter 実装は
/// security-critical なので pure な [`filter_schemas_for_user_and_shared`]
/// に切り出して unit test で pin する。
pub(super) async fn list_schemas(state: &AppState, user: &AuthenticatedUser) -> Value {
    let mut client = state.vegapunk.clone();
    match client.list_schemas(ListSchemasRequest {}).await {
        Err(status) => tonic_error_content("ListSchemas", status),
        Ok(resp) => {
            let resp = resp.into_inner();
            let schemas = filter_schemas_for_user_and_shared(
                &resp.schemas,
                &user.vegapunk_schema,
                &state.cfg.shared_schema_name,
            );
            success_content(json!({ "schemas": schemas }))
        }
    }
}

/// `ListSchemas` レスポンスから、own personal と shared の 2 件だけを通す。
/// それ以外は全 drop。filter は exact match (= 部分一致は不可) で、別 tenant
/// の `user-` prefix schema や偶然似た名前の schema が leak しないようにする。
fn filter_schemas_for_user_and_shared(
    schemas: &[SchemaListItem],
    user_schema: &str,
    shared_schema: &str,
) -> Vec<Value> {
    schemas
        .iter()
        .filter(|s| s.name == user_schema || s.name == shared_schema)
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
    let personal_req = match build_get_stats_request(&user.vegapunk_schema, &args) {
        Ok(r) => r,
        Err(e) => return invalid_args_content("stats", &e),
    };
    let mut shared_req = personal_req.clone();
    shared_req.schema = Some(state.cfg.shared_schema_name.clone());

    let mut client_p = state.vegapunk.clone();
    let mut client_s = state.vegapunk.clone();
    let (personal, shared) = tokio::join!(
        client_p.get_stats(personal_req),
        client_s.get_stats(shared_req),
    );
    let personal = match personal {
        Ok(r) => r.into_inner(),
        Err(s) => return tonic_error_content("GetStats (personal)", s),
    };
    // shared が無い deployment (= 初回 user 来訪前) を考慮して best-effort。
    let shared_json = match shared {
        Ok(r) => {
            let r = r.into_inner();
            json!({
                "node_count": r.node_count,
                "edge_count": r.edge_count,
                "vector_count": r.vector_count,
                "community_count": r.community_count,
            })
        }
        Err(s) => {
            tracing::warn!(
                schema = %state.cfg.shared_schema_name,
                code = ?s.code(),
                "shared GetStats failed (returning null for shared)",
            );
            Value::Null
        }
    };
    success_content(json!({
        "personal": {
            "node_count": personal.node_count,
            "edge_count": personal.edge_count,
            "vector_count": personal.vector_count,
            "community_count": personal.community_count,
        },
        "shared": shared_json,
    }))
}

// `feedback` / `get_job_status` handlers are intentionally absent in this PR.
// Their proto requests (FeedbackRequest / GetJobStatusRequest) carry no schema
// field, and the wrapper has no ownership-tracking table yet to verify the
// caller actually owns the supplied `search_id` / `msg_id`. Without that, a
// caller who learns another tenant's identifier could rate / inspect it.
// They will land in a follow-up PR once we record (search_id, msg_id) →
// user_id mapping during search / ingest.

/// `get_traceable_chain` argument を `GetTraceableChainRequest` に詰める。
/// `schema` は wrapper が user.vegapunk_schema を強制注入 (cross-tenant guard)。
/// proto では `max_depth` default 5, max 10。tools/list と整合する範囲で
/// runtime guard する。
pub(super) fn build_get_traceable_chain_request(
    user_schema: &str,
    args: &Value,
) -> Result<GetTraceableChainRequest, String> {
    let args = require_object_args(args)?;
    let node_id = require_str_field(args, "node_id", "arguments")?;
    let max_depth = optional_bounded_i32(
        args,
        "max_depth",
        TRACEABLE_CHAIN_MAX_DEPTH_MIN,
        TRACEABLE_CHAIN_MAX_DEPTH_MAX,
        Some(TRACEABLE_CHAIN_MAX_DEPTH_DEFAULT),
        "arguments",
    )?;
    Ok(GetTraceableChainRequest {
        node_id,
        schema: Some(user_schema.to_string()),
        max_depth,
    })
}

pub(super) async fn get_traceable_chain(
    state: &AppState,
    user: &AuthenticatedUser,
    args: Value,
) -> Value {
    // node_id がどちらの schema にあるかは呼び出し側からは分からないので、
    // 両 schema で並行に試行する。links が返った方を優先採用し、**両方 Err
    // のときだけ tonic error にして返す** (= 片方 Err は warn + 残った方を
    // 使う、両方 Ok で両方 links 空でも personal 側の空 chain を success と
    // して返す)。
    let personal_req = match build_get_traceable_chain_request(&user.vegapunk_schema, &args) {
        Ok(r) => r,
        Err(e) => return invalid_args_content("get_traceable_chain", &e),
    };
    let mut shared_req = personal_req.clone();
    shared_req.schema = Some(state.cfg.shared_schema_name.clone());

    let mut client_p = state.vegapunk.clone();
    let mut client_s = state.vegapunk.clone();
    let (personal, shared) = tokio::join!(
        client_p.get_traceable_chain(personal_req),
        client_s.get_traceable_chain(shared_req),
    );

    // どちらか / 両方の Ok から links を取り、空でない方を優先採用する。
    // 両方 Err のときだけ tonic_error_content。片方 Err は warn + 残る方を使う。
    let (found_in, resp) = match (personal, shared) {
        (Ok(p), Ok(s)) => {
            let p = p.into_inner();
            let s = s.into_inner();
            if !p.links.is_empty() {
                ("personal", p)
            } else if !s.links.is_empty() {
                ("shared", s)
            } else {
                ("personal", p)
            }
        }
        (Ok(p), Err(s_err)) => {
            tracing::warn!(
                error = %s_err,
                "shared get_traceable_chain failed (continuing with personal)",
            );
            ("personal", p.into_inner())
        }
        (Err(p_err), Ok(s)) => {
            tracing::warn!(
                error = %p_err,
                "personal get_traceable_chain failed (continuing with shared)",
            );
            ("shared", s.into_inner())
        }
        (Err(p_err), Err(_)) => {
            return tonic_error_content("GetTraceableChain", p_err);
        }
    };

    let links: Vec<Value> = resp
        .links
        .iter()
        .map(|l| {
            json!({
                "node_id": l.node_id,
                "node_type": l.node_type,
                "display_text": l.display_text,
                "edge_type": l.edge_type,
                "depth": l.depth,
                "timestamp": l.timestamp,
            })
        })
        .collect();
    success_content(json!({
        "found_in": found_in,
        "links": links,
    }))
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

    // ── ingest pre-check helpers (dedup normalization) ─────────────────

    #[test]
    fn normalize_entity_key_lowercases_and_trims() {
        assert_eq!(normalize_entity_key("  Vegapunk  "), "vegapunk");
        assert_eq!(normalize_entity_key("SIVIRA"), "sivira");
        assert_eq!(normalize_entity_key("\tfoo BAR\n"), "foo bar");
    }

    #[test]
    fn word_boundary_contains_matches_isolated_words() {
        assert!(word_boundary_contains_normalized(
            "hello vegapunk world",
            "vegapunk"
        ));
        assert!(word_boundary_contains_normalized(
            "Vegapunk launched",
            "vegapunk"
        ));
        assert!(word_boundary_contains_normalized(
            "VEGAPUNK ROCKS",
            "vegapunk"
        ));
    }

    #[test]
    fn word_boundary_contains_rejects_substring_inside_word() {
        // "Vegapunker" inside should NOT match "vegapunk" alone.
        assert!(!word_boundary_contains_normalized(
            "Vegapunker rocks",
            "vegapunk"
        ));
        assert!(!word_boundary_contains_normalized("XVegapunkX", "vegapunk"));
    }

    #[test]
    fn word_boundary_contains_handles_punctuation_boundaries() {
        // 句読点や括弧は word boundary として扱う。
        assert!(word_boundary_contains_normalized("Vegapunk.", "vegapunk"));
        assert!(word_boundary_contains_normalized("(Vegapunk)", "vegapunk"));
        assert!(word_boundary_contains_normalized(
            "see: vegapunk!",
            "vegapunk"
        ));
    }

    #[test]
    fn word_boundary_contains_returns_false_for_empty_needle() {
        assert!(!word_boundary_contains_normalized("anything", ""));
    }

    #[test]
    fn replace_word_case_insensitive_rewrites_to_canonical() {
        let out = replace_word_case_insensitive("we use Vegapunk daily", "vegapunk", "Vegapunk");
        assert_eq!(out.as_deref(), Some("we use Vegapunk daily"));
        let out = replace_word_case_insensitive("we use VEGAPUNK daily", "vegapunk", "Vegapunk");
        assert_eq!(out.as_deref(), Some("we use Vegapunk daily"));
        let out = replace_word_case_insensitive("we use vegapunk daily", "vegapunk", "Vegapunk");
        assert_eq!(out.as_deref(), Some("we use Vegapunk daily"));
    }

    #[test]
    fn replace_word_case_insensitive_replaces_all_occurrences() {
        let out = replace_word_case_insensitive("vegapunk vs VEGAPUNK", "vegapunk", "Vegapunk");
        assert_eq!(out.as_deref(), Some("Vegapunk vs Vegapunk"));
    }

    #[test]
    fn replace_word_case_insensitive_skips_substring_within_word() {
        // "Vegapunker" inside "Vegapunkers" should NOT be touched.
        let out = replace_word_case_insensitive("Vegapunker is a fan", "vegapunk", "Vegapunk");
        assert!(out.is_none());
    }

    #[test]
    fn replace_word_case_insensitive_returns_none_when_no_match() {
        let out = replace_word_case_insensitive("hello world", "vegapunk", "Vegapunk");
        assert!(out.is_none());
    }

    #[test]
    fn shared_dedup_block_content_marks_iserror_and_names_entity() {
        let v = shared_dedup_block_content(
            "ingest_raw",
            &EntityRef {
                name: "Vegapunk".into(),
                node_type: "Project".into(),
                schema: "sivira-shared".into(),
            },
        );
        assert_eq!(v["isError"], true);
        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("ingest_raw"));
        assert!(text.contains("Vegapunk"));
        assert!(text.contains("Project"));
        assert!(text.contains("sivira-shared"));
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
    fn filter_schemas_returns_user_and_shared_only() {
        let schemas = vec![
            schema_item("alice-tenant"),
            schema_item("bob-tenant"),
            schema_item("eve-tenant"),
            schema_item("sivira-shared"),
        ];
        let out = filter_schemas_for_user_and_shared(&schemas, "bob-tenant", "sivira-shared");
        let names: Vec<&str> = out.iter().map(|v| v["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"bob-tenant"));
        assert!(names.contains(&"sivira-shared"));
        assert_eq!(out.len(), 2, "expected own+shared only, got: {names:?}");
    }

    #[test]
    fn filter_schemas_drops_other_tenants_completely() {
        // alice / eve は出力に絶対に含まれない (cross-tenant guard の核)。
        let schemas = vec![
            schema_item("alice-tenant"),
            schema_item("eve-tenant"),
            schema_item("sivira-shared"),
        ];
        let out = filter_schemas_for_user_and_shared(&schemas, "bob-tenant", "sivira-shared");
        // bob-tenant 自身は schemas に居ないので、出力は shared のみ。
        let names: Vec<&str> = out.iter().map(|v| v["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["sivira-shared"], "got: {names:?}");
    }

    #[test]
    fn filter_schemas_uses_exact_match_not_substring() {
        // "bob-tenant" を要求した時に "bob-tenant-v2" のような prefix-match が
        // 入らないこと (substring leak の典型パターンを潰す)。shared 名も
        // exact match で判定される。
        let schemas = vec![
            schema_item("bob-tenant"),
            schema_item("bob-tenant-v2"),
            schema_item("bob"),
            schema_item("sivira-shared"),
            schema_item("sivira-shared-v2"),
        ];
        let out = filter_schemas_for_user_and_shared(&schemas, "bob-tenant", "sivira-shared");
        let names: Vec<&str> = out.iter().map(|v| v["name"].as_str().unwrap()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["bob-tenant", "sivira-shared"]);
    }

    #[test]
    fn filter_schemas_returns_empty_when_user_and_shared_both_absent() {
        let schemas = vec![schema_item("alice-tenant")];
        let out = filter_schemas_for_user_and_shared(&schemas, "bob-tenant", "sivira-shared");
        assert!(out.is_empty());
    }

    // ── get_traceable_chain ────────────────────────────────────────────

    #[test]
    fn get_traceable_chain_request_uses_user_schema_and_ignores_args_schema() {
        let req = build_get_traceable_chain_request(
            "alice-tenant",
            &json!({"node_id": "n1", "schema": "evil"}),
        )
        .unwrap();
        assert_eq!(req.schema.as_deref(), Some("alice-tenant"));
        assert_eq!(req.node_id, "n1");
        // 省略時は advertised default が入る (proto の default 5 と一致)。
        assert_eq!(req.max_depth, Some(TRACEABLE_CHAIN_MAX_DEPTH_DEFAULT));
    }

    #[test]
    fn get_traceable_chain_request_rejects_missing_node_id() {
        let err = build_get_traceable_chain_request("t", &json!({})).unwrap_err();
        assert!(err.contains("node_id"), "got: {err}");
    }

    #[test]
    fn get_traceable_chain_request_rejects_max_depth_out_of_range() {
        let err = build_get_traceable_chain_request("t", &json!({"node_id": "n", "max_depth": 0}))
            .unwrap_err();
        assert!(err.contains("max_depth"), "got: {err}");
        let err = build_get_traceable_chain_request("t", &json!({"node_id": "n", "max_depth": 11}))
            .unwrap_err();
        assert!(err.contains("max_depth"), "got: {err}");
    }

    #[test]
    fn get_traceable_chain_request_accepts_max_depth_in_range() {
        let req = build_get_traceable_chain_request("t", &json!({"node_id": "n", "max_depth": 7}))
            .unwrap();
        assert_eq!(req.max_depth, Some(7));
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
