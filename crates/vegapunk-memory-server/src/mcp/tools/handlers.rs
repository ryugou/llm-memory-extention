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

use vegapunk_client::GraphRagClient;
use vegapunk_client::graphrag::{
    AttributeFilter, GetJobStatusRequest, GetSchemaRequest, GetStatsRequest,
    GetTraceableChainRequest, IngestMessage, IngestRawMetadata, IngestRawRequest, IngestRequest,
    ListJobsRequest, ListSchemasRequest, MessageMetadata, QueryNodesRequest, SchemaListItem,
    SearchRequest, SearchResultItem,
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

/// `text.to_lowercase()` の各 char が **元 char と 1 対 1 + byte 長一致** で
/// 対応しているかを確認する strict alignment guard。
///
/// 「`text_lc.len() == text.len()`」だけでは不十分: 例えば KELVIN SIGN
/// (U+212A, 3 bytes) は `k` (1 byte) に shrink し、Turkish dotted I
/// (U+0130, 2 bytes) は `i\u{0307}` (3 bytes) に expand する。両者が混在
/// すれば total byte 長が偶然一致しても per-char offset がずれ、
/// `text_lc` 上で得た byte offset で `text` を slice すると **char 境界外
/// で panic** する可能性がある (= UTF-8 char boundary 違反)。
///
/// この関数が `true` を返す場合に限り、`text_lc` 上の byte offset を
/// `text` に直接マップして slice しても安全と見做せる。それ以外
/// (= 1 char が複数 lowercase char にマップされる / 長さが変わる) は
/// conservative に scan / rewrite を no-op に倒す。
fn is_lowercase_byte_aligned(text: &str) -> bool {
    text.chars().all(|c| {
        let mut iter = c.to_lowercase();
        match (iter.next(), iter.next()) {
            (Some(lc), None) => lc.len_utf8() == c.len_utf8(),
            _ => false,
        }
    })
}

/// `is_word_char` 風の判定。alphanumeric + underscore に CJK (= 連続した
/// 日本語名で word boundary を区切りたくないため) を含める粗い定義。
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// 既に lowercased な `text_lc` 上で word boundary scan を行う。
/// catalogue scan のホットパスで `text.to_lowercase()` を per-entity に
/// allocate しないよう、scan 本体はこの関数に切り出して、1 回作った
/// `text_lc` を呼び出し側が使い回せるようにする。
///
/// **呼び出し側の責任**: `is_lowercase_byte_aligned(text)` が true な
/// `text` から作った `text_lc` を渡すこと。`text_lc.len() == text.len()`
/// だけでは不十分 — shrink + expand 打ち消し (KELVIN SIGN + Turkish
/// dotted I の混在等) で per-char offset がずれているケースを通してしまい、
/// `text_lc` 上で得た byte offset で `text` を slice すると char-boundary
/// 違反で panic し得るため。strict guard が false のときは呼び出し側で
/// no-match に倒すこと。non-ASCII の真の case 比較は NFC normalize 込み
/// の別フェーズで扱う。
fn word_boundary_contains_in_lowercased(text_lc: &str, needle: &str) -> bool {
    if needle.is_empty() {
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
/// 出現する箇所を、すべて `canonical` 文字列に置換した結果を返す。
/// マッチ箇所の元のケースは失われ、`canonical` のケースに統一される
/// (= case-preserving ではなく、case-normalizing な置換)。
/// 出現がなければ `None`。複数出現は全置換。
///
/// 制約: `word_boundary_contains_in_lowercased` と同じく、`to_lowercase()`
/// で byte length が変わるケースでは元 text に offset を反映できないため
/// no-op (= `None`) を返す。NFC 正規化込みの真の対応は別フェーズ。
/// `replace_word_case_insensitive_with_lc` の薄い stand-alone wrapper。
/// 現状は tests からのみ呼ばれる (= production の hot-path は直接
/// `_with_lc` 版を使い、`text_lc` を catalogue 全体で再利用するため)。
#[cfg(test)]
fn replace_word_case_insensitive(
    text: &str,
    needle_normalized: &str,
    canonical: &str,
) -> Option<String> {
    if !is_lowercase_byte_aligned(text) {
        return None;
    }
    let text_lc = text.to_lowercase();
    replace_word_case_insensitive_with_lc(text, &text_lc, needle_normalized, canonical)
}

/// `text` の中に `needle_normalized` (= 正規化済キー) が word boundary で
/// 出現する箇所を、すべて `canonical` 文字列に置換した結果を返す。
/// 既に作ってある `text_lc` を渡すことで、ホットパス (= catalogue を回す
/// per-entity ループ) で `text.to_lowercase()` を allocate しなくて済む。
///
/// **呼び出し側の責任**:
/// - `text_lc == text.to_lowercase()` であること。
/// - `is_lowercase_byte_aligned(text)` が true であること
///   (= `text_lc` の byte offset を `text` の slice に直接マップしても
///   char-boundary を踏まない前提)。
///
/// 戻り値は実際に書き換えが起きたときだけ `Some(updated)`。全 match が
/// canonical と一致するなら `None`。
fn replace_word_case_insensitive_with_lc(
    text: &str,
    text_lc: &str,
    needle_normalized: &str,
    canonical: &str,
) -> Option<String> {
    if needle_normalized.is_empty() {
        return None;
    }
    // lazy alloc: 「実際に書き換わる match (= 元 span が canonical と異なる)」を
    // 初めて見つけるまで `out` を allocate しない。全 match が既に canonical
    // と一致する text (例: 元から正しい表記の "Vegapunk" を含む文章) では、
    // String::with_capacity も実 copy も発生せず `None` を返せる。これにより
    // scan_text_with_catalogue 内で personal catalogue × messages を回す
    // ホットパスの per-entity alloc が消える。
    let mut out: Option<String> = None;
    let mut cursor = 0usize;
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
            let span = &text[abs..end];
            if span != canonical {
                // 初めての実書き換え match: ここで初めて alloc する。
                let buf = out.get_or_insert_with(|| String::with_capacity(text.len()));
                buf.push_str(&text[cursor..abs]);
                buf.push_str(canonical);
                cursor = end;
            } else if let Some(buf) = out.as_mut() {
                // 既に build 中なら、canonical と同一 match の区間も
                // そのまま out にコピーして cursor を進める。
                buf.push_str(&text[cursor..end]);
                cursor = end;
            }
            // out が未 alloc かつ span == canonical の場合は何もしない
            // (= cursor も進めない。後続で実書き換え match を見つけたら
            //  そこから先頭の prefix としてまとめて copy される)。
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
    if let Some(mut buf) = out {
        buf.push_str(&text[cursor..]);
        Some(buf)
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
    /// `normalize_entity_key(name)` の事前計算結果。catalogue 構築時に
    /// 一度だけ計算し、scan のホットパスで reuse する。shared catalogue
    /// 単独で最大 8000 entity 規模 × messages 件数の組み合わせで scan が
    /// 走るため、per-scan に `trim + to_lowercase` を再計算すると無駄な
    /// `String` allocate がホットパスを支配する。
    pub normalized_key: String,
}

impl EntityRef {
    fn new(name: String, node_type: String, schema: String) -> Self {
        let normalized_key = normalize_entity_key(&name);
        Self {
            name,
            node_type,
            schema,
            normalized_key,
        }
    }
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
    // 各 node_type について shared と personal の fetch は独立なので
    // `tokio::join!` で並列に走らせ、レイテンシを node_type 数比例から
    // (node_type 数 × 1 ラウンド) まで圧縮する。
    for node_type in DEDUP_SCAN_NODE_TYPES {
        let (shared_names, personal_names) = tokio::join!(
            fetch_entity_names(state, shared_schema, node_type),
            fetch_entity_names(state, &user.vegapunk_schema, node_type),
        );
        for name in shared_names {
            shared.push(EntityRef::new(
                name,
                (*node_type).to_string(),
                shared_schema.to_string(),
            ));
        }
        for name in personal_names {
            personal.push(EntityRef::new(
                name,
                (*node_type).to_string(),
                user.vegapunk_schema.clone(),
            ));
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
    // alignment guard は `is_lowercase_byte_aligned(text)` を使う。
    // (`text_lc.len() == text.len()` だけだと shrink + expand 打ち消しの
    // ケース — KELVIN SIGN U+212A → 'k' と Turkish dotted I U+0130 →
    // i\u{0307} が混在する text — を検出できず、`text_lc` 上の byte offset
    // で `text` を slice したときに char-boundary 違反で panic し得る。)
    //
    // guard が false な text は shared scan も personal rewrite (=
    // replace_word_case_insensitive の strict guard) も全部 no-op に倒れる
    // ので、結果は確実に `Proceed`。`to_lowercase()` (= 大きな String alloc)
    // と entity loop の両方を skip して early return する。
    if !is_lowercase_byte_aligned(text) {
        return IngestPreCheck::Proceed;
    }

    // shared scan は text を変更しないので `text_lc` を 1 回作って per-entity
    // で再利用する。catalogue.shared は最大 8000 entity × messages 件数まで
    // 膨らみ得るため、ここで per-entity に lowercase を allocate すると
    // ホットパスのコストが O(entity × text_len) になってしまう。
    let text_lc = text.to_lowercase();
    for ent in &catalogue.shared {
        // ent.normalized_key は catalogue 構築時に 1 回だけ作ってあるので
        // scan のホットパスでは `trim + to_lowercase` を回さず再利用する。
        if word_boundary_contains_in_lowercased(&text_lc, &ent.normalized_key) {
            return IngestPreCheck::BlockedByShared { hit: ent.clone() };
        }
    }

    // personal scan: text_lc を再利用して per-entity の `to_lowercase()`
    // を排除する。最大 4000 entity (= 4 node_type × 1000) × messages 件数
    // を 1 ingest で回す可能性があり、per-entity に text 全体を lowercase
    // すると O(entities × text_len) の alloc が発生する。
    //
    // 戦略:
    // - rewrite が起きるまでは `text` を借りたまま、`text_lc` も
    //   shared scan で作ったものをそのまま再利用する (= no-rewrite な
    //   一般ケースで `text.to_string()` の alloc を完全に省く)。
    // - rewrite が起きた瞬間に `new_text_owned: String` を持ち始める。
    //   以降の scan はこの owned 文字列に対して走る。
    // - rewrite 後の `new_text.to_lowercase()` を作り直すが、その前に
    //   `is_lowercase_byte_aligned` を再 check する。canonical 名が
    //   Turkish I 等 alignment を崩す char を含む可能性があり、崩れたら
    //   後続 scan は safety guard を再保証できないので break する。
    let mut new_text_owned: Option<String> = None;
    let mut new_text_lc: String = text_lc;
    let mut rewrites = Vec::new();
    for ent in &catalogue.personal {
        let current_text = new_text_owned.as_deref().unwrap_or(text);
        let updated = replace_word_case_insensitive_with_lc(
            current_text,
            &new_text_lc,
            &ent.normalized_key,
            &ent.name,
        );
        if let Some(updated) = updated {
            rewrites.push(ent.clone());
            let aligned = is_lowercase_byte_aligned(&updated);
            new_text_owned = Some(updated);
            if !aligned {
                break;
            }
            // unwrap: Some(updated) を直前で代入したので確実に Some。
            new_text_lc = new_text_owned.as_deref().unwrap().to_lowercase();
        }
    }

    match new_text_owned {
        Some(new_text) if !rewrites.is_empty() => IngestPreCheck::Rewritten { new_text, rewrites },
        // rewrite が無い (= alloc も発生していない) 一般ケース。
        _ => IngestPreCheck::Proceed,
    }
}

/// `IngestPreCheck::BlockedByShared` を MCP tool error content に変換する。
/// `method` は debug 用に "ingest" / "ingest_raw" / "ingest (messages[i])" 等
/// を渡す (= 抑制対象が確定するためどの handler / どの message が原因か残す)。
pub(super) fn shared_dedup_block_content(method: &str, hit: &EntityRef) -> Value {
    // 改行 + インデント混入を避けるため 1 行で組み立てる
    // (= "Reference the existing" の前に連続スペースが入らない)。
    let body = format!(
        "{method} blocked: '{}' ({}) already exists in schema '{}'. Reference the existing entity instead of re-ingesting it; this avoids cross-schema duplication.",
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
        // id が文字列で非空のときだけ dedup する。proto 的に `id` は optional
        // で、`null` / 空文字を含めて `"{type}:null"` という同一 key にまとめて
        // しまうと、id 無しの結果が 2 件目以降ごっそり落ちる事故になる。
        // id が無いものは dedup の対象外 (= 全部素通し) にする。
        // `type` も文字列として取り出してから key を組む。serde_json::Value の
        // Display 経由だと `"Project"` のように引用符付きの形になり、
        // (a) key の semantics が読みにくくなる、(b) Value の表現が
        // 将来変わると key が黙って変わる、という不安定さが残る。
        let type_str = v["type"].as_str().unwrap_or("");
        let should_keep = match v["id"].as_str() {
            Some(id) if !id.is_empty() => {
                let key = format!("{type_str}:{id}");
                seen.insert(key)
            }
            _ => true,
        };
        if should_keep {
            // None / NaN は両方 NEG_INFINITY に正規化して必ず最下位に落とす。
            // (total_cmp の全順序では負の NaN が NEG_INFINITY より下に来る
            //  ので、生 NaN を素通しすると top_k truncate が不安定になる。)
            let raw = item.score.unwrap_or(f32::NEG_INFINITY);
            let score = if raw.is_nan() { f32::NEG_INFINITY } else { raw };
            all_results.push((score, v));
        }
    }
    // 降順 sort。NaN は上の正規化で除いてあるので total_cmp の挙動は安全。
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

    // backward compat: 既存 client は top-level `search_id` (= personal の id)
    // を `feedback` tool に渡す前提なのでそのまま残す。新規 client は
    // `search_ids.{personal,shared}` で両方の id を取得できる。
    let personal_search_id = personal.search_id;
    let shared_search_id = shared.as_ref().map(|s| s.search_id.clone());
    success_content(json!({
        "search_id": personal_search_id.clone(),
        "search_ids": {
            "personal": personal_search_id,
            "shared": shared_search_id,
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

    // backward compat: 既存 client は top-level の name / schema_yaml /
    // version / description を読む前提で書かれているので、personal schema の
    // 値をそのまま top-level に残す。`shared` は新規 field として optional に
    // 併設する (shared schema 未作成時は null)。
    success_content(json!({
        "name": personal.name,
        "schema_yaml": personal.schema_yaml,
        "version": personal.version,
        "description": personal.description,
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

/// entity_extraction job が **catalogue を更新する形で正常終了** したと
/// 見做せる唯一の status。`failed` / `dead_letter` は terminal だが entity
/// は graph に書かれていないので、dedup catalogue 観点では「未更新」扱い。
const JOB_STATUS_OK: &str = "completed";
/// terminal status (= 以後 status が変わらない) として待機ループから外す
/// 値。`failed` / `dead_letter` も含むが、catalogue 観点では失敗扱いになる。
const TERMINAL_JOB_STATUSES: &[&str] = &[JOB_STATUS_OK, JOB_STATUS_FAILED, JOB_STATUS_DEAD_LETTER];
const JOB_STATUS_FAILED: &str = "failed";
const JOB_STATUS_DEAD_LETTER: &str = "dead_letter";

/// `await_*` の最大待ち時間 (秒)。vegapunk の entity_extraction は通常
/// 5〜30 秒で完了するが、LLM 側 rate limit / リトライで伸びることもある。
/// timeout に達しても本処理はエラーにせず、`await_status` を response に
/// 含めて client に状態を返す (= "ok" / "timeout" / "partial") ことで
/// transparent に伝える設計。
const JOB_AWAIT_TIMEOUT_SECS: u64 = 120;
const JOB_POLL_INTERVAL_MS: u64 = 500;

/// `since_ms` を ingest RPC 直前に wrapper 時計で取るが、vegapunk server 時計
/// との skew で job の `created_at < since_ms` になり取りこぼす可能性がある。
/// 5 秒 margin で巻き戻して取りこぼしを軽減 (= clock skew 5 秒以内なら拾う)。
const SINCE_MS_SKEW_MARGIN: i64 = 5_000;

/// `ListJobs` の 1 ページ最大件数 (proto 上の上限と整合)。
const LIST_JOBS_PAGE_SIZE: i32 = 500;

/// `await_msg_ids_complete` の 1 round で同時に投げる `GetJobStatus` の
/// 上限。chunk 数が多い `ingest_raw` (= 巨大 text) で task が爆発し
/// wrapper / vegapunk を圧迫しないよう **chunk 単位で batch spawn** して
/// 任意時点に存在する tokio task 数を `JOB_POLL_PARALLELISM` 以下に保つ
/// (Semaphore で permit 待ちさせる構造だと N task が常駐するので NG)。
const JOB_POLL_PARALLELISM: usize = 32;

/// 1 個の `GetJobStatus` RPC の per-call timeout。stuck な call が
/// `join_next()` を無限ブロックして全体 deadline を bypass しないよう、
/// 短めの 10 秒で deadline_exceeded を返して次の round に切り替える。
const GET_JOB_STATUS_RPC_TIMEOUT_SECS: u64 = 10;
/// 1 個の `ListJobs` RPC の per-call timeout。同上。
const LIST_JOBS_RPC_TIMEOUT_SECS: u64 = 15;

/// ingest / ingest_raw が wrapper 内で job polling した結果を、client に
/// machine-readable に返すための status enum 的 string。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AwaitStatus {
    /// 全 job が `completed` で終わった = catalogue が更新済み。
    Ok,
    /// 一部 / 全 job が `failed` or `dead_letter` で terminal。catalogue が
    /// 期待通り更新されていない可能性がある。
    Partial,
    /// timeout に達した = まだ pending が残っている。
    Timeout,
}

impl AwaitStatus {
    fn as_str(self) -> &'static str {
        match self {
            AwaitStatus::Ok => "ok",
            AwaitStatus::Partial => "partial",
            AwaitStatus::Timeout => "timeout",
        }
    }
}

/// `ListJobs` の戻り `JobInfo` 集合に対し、`{user_schema}:` prefix を持つ
/// msg_id のもののうち **status が terminal** なもののカウントを返す pure
/// helper。Codex review M1 / R3 / R6 をテスト可能にする。
///
/// 戻り値 `.0` = `completed` 数 (= catalogue 更新済み)
/// 戻り値 `.1` = `failed` + `dead_letter` 数 (= terminal だが catalogue 未更新)
fn count_terminal_jobs_for_schema(
    jobs: &[vegapunk_client::graphrag::JobInfo],
    schema_prefix: &str,
) -> (i32, i32) {
    let mut ok = 0i32;
    let mut failed = 0i32;
    for j in jobs {
        let in_schema = j
            .msg_id
            .as_deref()
            .map(|m| m.starts_with(schema_prefix))
            .unwrap_or(false);
        if !in_schema {
            continue;
        }
        match j.status.as_str() {
            JOB_STATUS_OK => ok += 1,
            JOB_STATUS_FAILED | JOB_STATUS_DEAD_LETTER => failed += 1,
            _ => {}
        }
    }
    (ok, failed)
}

/// `IngestRaw` で返ってきた `msg_ids` の entity_extraction が完了するまで
/// `GetJobStatus` で polling する。`msg_ids` の全件が **terminal** (=
/// `completed` / `failed` / `dead_letter`) になるか timeout に達するまでループ。
///
/// 戻り値 `AwaitStatus` は client への response に machine-readable に
/// 露出する (= warn だけだと「catalogue が更新済み」と誤認されるため)。
///   - `Ok`     : 全 msg_id が `completed` 終了
///   - `Partial`: 一部 / 全部が `failed` or `dead_letter` で terminal
///   - `Timeout`: deadline 経過、まだ pending あり
///
/// 各 round は `tokio::task::JoinSet` で **並列に** `GetJobStatus` を投げる
/// (= chunk 数が多い ingest_raw で逐次 polling だと 1 round が `N * RTT`
/// になり、deadline を polling だけで消費する Codex P1 を回避)。
async fn await_msg_ids_complete(client: &mut GraphRagClient, msg_ids: Vec<String>) -> AwaitStatus {
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    if msg_ids.is_empty() {
        return AwaitStatus::Ok;
    }
    let deadline = Instant::now() + Duration::from_secs(JOB_AWAIT_TIMEOUT_SECS);
    let mut pending: HashSet<String> = msg_ids.into_iter().collect();
    let mut saw_non_ok_terminal = false;
    while !pending.is_empty() && Instant::now() < deadline {
        let ids_snapshot: Vec<String> = pending.iter().cloned().collect();
        // 並列に GetJobStatus を投げる。Semaphore で permit 待ちさせる構造
        // だと N task が常に runtime に常駐 (= memory/scheduler overhead)
        // するため、**chunk 単位で batch spawn → join_all → 次 chunk** に
        // 切り替え (Copilot P1)。任意時点の同時 task 数は
        // `JOB_POLL_PARALLELISM` 以下に保たれる。
        // 各 RPC には per-call timeout を被せ、stuck な call が
        // `join_next()` を無限ブロックして全体 deadline を bypass しないよう
        // にする (Copilot P1)。
        let mut results: Vec<(String, Result<tonic::Response<_>, tonic::Status>)> =
            Vec::with_capacity(ids_snapshot.len());
        for chunk in ids_snapshot.chunks(JOB_POLL_PARALLELISM) {
            let mut set = tokio::task::JoinSet::new();
            for msg_id in chunk {
                let mut c = client.clone();
                let id = msg_id.clone();
                set.spawn(async move {
                    let fut = c.get_job_status(GetJobStatusRequest { msg_id: id.clone() });
                    let res = match tokio::time::timeout(
                        Duration::from_secs(GET_JOB_STATUS_RPC_TIMEOUT_SECS),
                        fut,
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(_) => Err(tonic::Status::deadline_exceeded(
                            "get_job_status rpc timed out",
                        )),
                    };
                    (id, res)
                });
            }
            while let Some(join) = set.join_next().await {
                match join {
                    Ok(pair) => results.push(pair),
                    Err(e) => tracing::warn!(error = %e, "join_next failed in ingest_raw await"),
                }
            }
        }
        for (msg_id, res) in results {
            match res {
                Ok(resp) => {
                    let s = resp.into_inner();
                    let status = s.overall_status.as_str();
                    if TERMINAL_JOB_STATUSES.contains(&status) {
                        if status != JOB_STATUS_OK {
                            saw_non_ok_terminal = true;
                            tracing::warn!(
                                msg_id = %msg_id,
                                status = status,
                                "ingest_raw entity_extraction terminal but not completed"
                            );
                        }
                        pending.remove(&msg_id);
                    }
                }
                Err(e) => {
                    // 一過性 gRPC エラーは pending に残し、次の round で retry。
                    // 何度も失敗するなら deadline に達して抜ける。
                    tracing::warn!(
                        msg_id = %msg_id,
                        code = ?e.code(),
                        "get_job_status failed during ingest_raw await; retrying"
                    );
                }
            }
        }
        if pending.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(JOB_POLL_INTERVAL_MS)).await;
    }
    if !pending.is_empty() {
        tracing::warn!(
            pending_count = pending.len(),
            timeout_secs = JOB_AWAIT_TIMEOUT_SECS,
            "ingest_raw entity_extraction did not complete within timeout"
        );
        AwaitStatus::Timeout
    } else if saw_non_ok_terminal {
        AwaitStatus::Partial
    } else {
        AwaitStatus::Ok
    }
}

/// `since_ms` 以降に作られた `entity_extraction` job を **全ページ** 取り、
/// 該当ページの `JobInfo` を返す pure-ish async helper。`ListJobs` 1 ページ
/// 上限 `LIST_JOBS_PAGE_SIZE` = 500 (proto 上の上限) で大 batch / 並列環境
/// では足りないので offset を進めて巡回する (Codex R3 対応)。
///
/// `list_jobs` の一過性エラーは bubble する (= caller が retry を判断)。
async fn list_extraction_jobs_paged(
    client: &mut GraphRagClient,
    since_ms: i64,
) -> Result<Vec<vegapunk_client::graphrag::JobInfo>, tonic::Status> {
    use std::collections::HashMap;
    use std::time::Duration;
    // `ListJobs` は `created_at DESC` で並ぶ。ページング中に新規 job が
    // 挿入されると page が後方にシフトして同じ job が複数 page に現れ得る。
    // dedup の際、**後から取得した entry で上書きする** ことで status の
    // 鮮度を確保する (例: page 0 では running, シフト後 page 1 で completed と
    // 観測 → 古い running を捨てて最新 completed を採用)。前者を保持すると
    // `await_extraction_jobs_for_schema` が完了判定を遅延して timeout し
    // 易くなる (Copilot R3 round 3)。
    let mut by_id: HashMap<String, vegapunk_client::graphrag::JobInfo> = HashMap::new();
    let mut offset: i32 = 0;
    loop {
        let req = ListJobsRequest {
            status: None,
            since_ms: Some(since_ms),
            until_ms: None,
            offset: Some(offset),
            limit: Some(LIST_JOBS_PAGE_SIZE),
            job_type: Some("entity_extraction".to_string()),
        };
        // per-RPC timeout を被せて、stuck な list_jobs が全体 deadline を
        // bypass しないようにする (Copilot P2)。
        let resp = match tokio::time::timeout(
            Duration::from_secs(LIST_JOBS_RPC_TIMEOUT_SECS),
            client.list_jobs(req),
        )
        .await
        {
            Ok(r) => r?,
            Err(_) => {
                return Err(tonic::Status::deadline_exceeded("list_jobs rpc timed out"));
            }
        };
        let page = resp.into_inner().jobs;
        let page_len = page.len() as i32;
        for j in page {
            // `insert` で既存 entry を上書き = 後で観測した status を採用。
            by_id.insert(j.job_id.clone(), j);
        }
        if page_len < LIST_JOBS_PAGE_SIZE {
            break;
        }
        offset += LIST_JOBS_PAGE_SIZE;
        // 無限ループ防御: vegapunk 上の total entries が極端に多い場合は
        // 10k 件で打ち切る (= 通常 batch では絶対に到達しない)。
        if offset >= 10_000 {
            tracing::warn!(
                offset,
                "list_extraction_jobs_paged hit safety cap; truncating"
            );
            break;
        }
    }
    Ok(by_id.into_values().collect())
}

/// `Ingest` (structured) は proto レスポンスに `msg_ids` を含まない
/// (`IngestResponse { ingested_count, job_id: Option<String> }`, `job_id` は
/// 将来用で常に None)。よって `since_ms` を ingest 直前にキャプチャしてから
/// `ListJobs(job_type=entity_extraction, since_ms=...)` を polling し、
/// `{user_schema}:` prefix の msg_id を持つ job のうち
/// `completed` 数が `expected_count` 以上になるまで待つ。
///
/// 制約 (Codex R1 / R2 を transparent に伝える):
/// - **clock skew**: `since_ms` は wrapper 時計、`JobInfo.created_at` は
///   vegapunk server 時計。`SINCE_MS_SKEW_MARGIN` (5s) 巻き戻して取りこぼし軽減。
/// - **並列混入**: 同 schema に対する別 client の並列 ingest で job 数が
///   膨らむ可能性。本実装は「自分の job だけ」を厳密に追跡する手段が無いため
///   (= proto に msg_ids も batch job_id も無い)、`completed` 件数が expected
///   に達した時点で return する **下限保証** モデル。早期完了の余地は
///   `AwaitStatus` の返り値で client に開示する。
async fn await_extraction_jobs_for_schema(
    client: &mut GraphRagClient,
    user_schema: &str,
    since_ms: i64,
    expected_count: i32,
) -> AwaitStatus {
    use std::time::{Duration, Instant};

    if expected_count <= 0 {
        return AwaitStatus::Ok;
    }
    let deadline = Instant::now() + Duration::from_secs(JOB_AWAIT_TIMEOUT_SECS);
    let schema_prefix = format!("{user_schema}:");
    let skewed_since_ms = since_ms.saturating_sub(SINCE_MS_SKEW_MARGIN);
    let mut last_failed = 0i32;
    while Instant::now() < deadline {
        match list_extraction_jobs_paged(client, skewed_since_ms).await {
            Ok(jobs) => {
                let (ok, failed) = count_terminal_jobs_for_schema(&jobs, &schema_prefix);
                last_failed = failed;
                if ok + failed >= expected_count {
                    if failed > 0 {
                        tracing::warn!(
                            schema = %user_schema,
                            failed,
                            ok,
                            expected_count,
                            "ingest entity_extraction reached expected count but had failed/dead_letter jobs"
                        );
                        return AwaitStatus::Partial;
                    }
                    return AwaitStatus::Ok;
                }
            }
            Err(e) => {
                tracing::warn!(
                    code = ?e.code(),
                    "list_jobs failed during ingest await; retrying"
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(JOB_POLL_INTERVAL_MS)).await;
    }
    tracing::warn!(
        schema = %user_schema,
        expected_count,
        timeout_secs = JOB_AWAIT_TIMEOUT_SECS,
        last_failed_seen = last_failed,
        "ingest entity_extraction did not reach expected_count within timeout"
    );
    AwaitStatus::Timeout
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
                // entity 名 (Person 等) を default 出力する INFO に載せると
                // 個人情報の漏えいリスクがあるので DEBUG に下げ、本数のみ出す。
                tracing::debug!(
                    message_index = i,
                    rewrite_count = rewrites.len(),
                    "ingest text normalized to existing canonical names",
                );
                msg.text = new_text;
            }
            IngestPreCheck::Proceed => {}
        }
    }
    let mut client = state.vegapunk.clone();
    // entity_extraction job を後で `ListJobs(since_ms=...)` で拾うため、
    // ingest を叩く **直前** に timestamp を取る (= ingest 後に取ると、jobs
    // の created_at よりも since_ms が後ろになって 0 件返る race を防ぐ)。
    let since_ms = llm_memory_core::time::now_ms();
    match client.ingest(request).await {
        Err(status) => tonic_error_content("Ingest", status),
        Ok(resp) => {
            let resp = resp.into_inner();
            // PR #21 の dedup pre-check は前回 ingest で作られた entity を
            // catalogue 経由で見るが、vegapunk の LLM 抽出は async で 5〜30s
            // かかる。client が連投すると catalogue が空振りして同名重複が
            // 生まれるため、本 ingest の extraction が落ち着くまで待つ。
            // 結果は `await_status` で client に返す (= "ok" / "partial" /
            //  "timeout")、catalogue が確実に更新されたかを呼び出し側が判断可能。
            let await_status = await_extraction_jobs_for_schema(
                &mut client,
                &user.vegapunk_schema,
                since_ms,
                resp.ingested_count,
            )
            .await;
            success_content(json!({
                "ingested_count": resp.ingested_count,
                "job_id": resp.job_id,
                "await_status": await_status.as_str(),
                // structured `ingest` は proto レスポンスに msg_ids も batch
                // job_id も含まないため、wrapper は `ListJobs(since_ms,
                // schema prefix)` から **下限保証** で待機している。同 schema
                // への並列 ingest が混ざると自分の job が pending のまま
                // early return する余地が残るため、それを client に明示する。
                // ingest_raw は msg_ids ベースで exact tracking している。
                "await_semantics": "lower-bound",
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
            // ingest と同じく entity 名は default log に載せず、本数のみ DEBUG。
            tracing::debug!(
                rewrite_count = rewrites.len(),
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
            // ingest_raw は IngestRawResponse.msg_ids を返すので、各 msg_id を
            // 直接 GetJobStatus で polling できる (= list_jobs ベースの ingest
            // 経路よりピンポイント)。詳細は `await_msg_ids_complete` 参照。
            // 結果は `await_status` で client に返す。
            let await_status = await_msg_ids_complete(&mut client, resp.msg_ids.clone()).await;
            success_content(json!({
                "chunk_count": resp.chunk_count,
                "msg_ids": resp.msg_ids,
                "await_status": await_status.as_str(),
                // ingest_raw は msg_ids 個別追跡で **正確に** 待機できる。
                // structured ingest との対比で client が判断できるよう明示。
                "await_semantics": "exact",
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
    // fan-out 用に request を組み直す。client が指定した `offset` / `limit` は
    // **union (personal + shared) に対する意味** として解釈し直し、各 schema
    // には `offset=0`, `limit=client_offset+client_limit` を投げる。これにより
    //   - client の `offset` が片方の集合だけにかかってしまう問題を回避
    //   - merge 後に正しく `[offset, offset+limit)` の range を slice できる
    // sort_by / sort_order は **per-schema 適用** で、cross-schema 全体 sort
    // は wrapper では再現しない (= proto の sort は backend 側の属性順序を
    // 使うため、クライアント側で再ソートすると属性の型推測が必要になる)。
    // personal を先・shared を後ろの安定 order で merge することで「自分の
    // データが優先表示」という直感的な挙動を担保する。
    let client_offset = personal_req.offset.unwrap_or(0).max(0);
    let client_limit = personal_req.limit;
    // fetch_limit = `offset + limit` を投げて merge 後に slice したいが、
    // `client_offset + client_limit` が proto/wrapper の上限 (= QUERY_NODES_LIMIT_MAX)
    // を超えると vegapunk が invalid_argument を返してしまう。client が validate
    // 範囲内の値を渡しても fan-out で上限超過になり得るので、ここで上限へ
    // clamp する。トレードオフ: `offset + limit > LIMIT_MAX` の領域では片方の
    // schema からの fetch 量が LIMIT_MAX に頭打ちになり、結果 page から欠落
    // しうる (= deep pagination は backend 仕様で制約される)。
    let fetch_limit = client_limit.map(|l| {
        client_offset
            .saturating_add(l.max(0))
            .min(QUERY_NODES_LIMIT_MAX as i32)
    });
    let mut personal_req = personal_req;
    personal_req.offset = Some(0);
    personal_req.limit = fetch_limit;
    let mut shared_req = personal_req.clone();
    shared_req.schema = state.cfg.shared_schema_name.clone();

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
    // union 全体に対して [client_offset, client_offset + client_limit) で slice。
    let start = (client_offset as usize).min(nodes.len());
    nodes.drain(..start);
    if let Some(limit) = client_limit {
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
///
/// 順序は **personal → shared** で固定する。`schemas` (= backend の
/// `ListSchemas` レスポンス) の列挙順は backend 実装依存で安定保証が無く、
/// 他の handler (search / query_nodes 等) では personal を先に merge する
/// 慣習があるので、ここも同じ並びにしてクライアント側で「`schemas[0]` が
/// 自分の personal schema」と仮定しても安全にしておく。
/// 万一 `user_schema == shared_schema` のときは 1 件しか返さない
/// (= cross-tenant guard の通常パスでは起き得ないが、念のため)。
fn filter_schemas_for_user_and_shared(
    schemas: &[SchemaListItem],
    user_schema: &str,
    shared_schema: &str,
) -> Vec<Value> {
    let to_json = |s: &SchemaListItem| {
        json!({
            "name": s.name,
            "version": s.version,
            "description": s.description,
            "schema_yaml": s.schema_yaml,
        })
    };
    let mut out = Vec::with_capacity(2);
    if let Some(p) = schemas.iter().find(|s| s.name == user_schema) {
        out.push(to_json(p));
    }
    if user_schema != shared_schema {
        if let Some(s) = schemas.iter().find(|s| s.name == shared_schema) {
            out.push(to_json(s));
        }
    }
    out
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
    // backward compat: 既存 client は top-level の counter を読む前提なので
    // personal の値を top-level に残し、`shared` は新規 field として併設する
    // (shared 取得失敗時は null)。
    success_content(json!({
        "node_count": personal.node_count,
        "edge_count": personal.edge_count,
        "vector_count": personal.vector_count,
        "community_count": personal.community_count,
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
    // 両 schema で並行に試行する。エラーの扱いは他 read handler と揃える:
    //
    // - **shared** 側の Err は **best-effort warn + 続行** (= 他 read と同じ
    //   方針)。shared は誰でも触れる共有 schema で、未作成 / Unavailable /
    //   PermissionDenied などの状態でも personal が answer を持ち得るので、
    //   片側で完結できる限り処理を止めない。
    // - **personal** 側の Err は **NotFound のみ非致命** で、それ以外
    //   (Unauthenticated / PermissionDenied / Unavailable など) は致命扱い。
    //   personal の本物の障害を shared が Ok だからと黙らせると、cross-tenant
    //   guard や authz が壊れていても気付けない。
    // - 両方 Err は personal の Err を tonic_error_content で返す。
    // - 両方 Ok で両方 links 空のときは personal 側の空 chain を success と
    //   して返す (= "no chain found" を呼び出し側に伝える)。
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
                code = ?s_err.code(),
                "shared get_traceable_chain failed (continuing with personal)",
            );
            ("personal", p.into_inner())
        }
        (Err(p_err), Ok(s)) => {
            if p_err.code() == tonic::Code::NotFound {
                tracing::warn!(
                    code = ?p_err.code(),
                    "personal get_traceable_chain NotFound (continuing with shared)",
                );
                ("shared", s.into_inner())
            } else {
                return tonic_error_content("GetTraceableChain (personal)", p_err);
            }
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

    // test helper: 旧 `word_boundary_contains_normalized` 相当の薄い wrapper。
    // production は lowercased text を再利用するホットパスで直接 helper を
    // 呼ぶ (= scan_text_with_catalogue 参照)、test はこの簡便版を使う。
    // alignment guard は production と同じ `is_lowercase_byte_aligned` を使い、
    // 挙動の差が出ないようにする。
    fn wb_contains_for_test(text: &str, needle: &str) -> bool {
        if needle.is_empty() {
            return false;
        }
        if !is_lowercase_byte_aligned(text) {
            return false;
        }
        let text_lc = text.to_lowercase();
        word_boundary_contains_in_lowercased(&text_lc, needle)
    }

    #[test]
    fn word_boundary_contains_matches_isolated_words() {
        assert!(wb_contains_for_test("hello vegapunk world", "vegapunk"));
        assert!(wb_contains_for_test("Vegapunk launched", "vegapunk"));
        assert!(wb_contains_for_test("VEGAPUNK ROCKS", "vegapunk"));
    }

    #[test]
    fn word_boundary_contains_rejects_substring_inside_word() {
        // "Vegapunker" inside should NOT match "vegapunk" alone.
        assert!(!wb_contains_for_test("Vegapunker rocks", "vegapunk"));
        assert!(!wb_contains_for_test("XVegapunkX", "vegapunk"));
    }

    #[test]
    fn word_boundary_contains_handles_punctuation_boundaries() {
        // 句読点や括弧は word boundary として扱う。
        assert!(wb_contains_for_test("Vegapunk.", "vegapunk"));
        assert!(wb_contains_for_test("(Vegapunk)", "vegapunk"));
        assert!(wb_contains_for_test("see: vegapunk!", "vegapunk"));
    }

    #[test]
    fn word_boundary_contains_returns_false_for_empty_needle() {
        assert!(!wb_contains_for_test("anything", ""));
    }

    #[test]
    fn replace_word_case_insensitive_returns_none_on_misaligned_lowercase() {
        // U+212A KELVIN SIGN (3 bytes) → 'k' (1 byte) = shrink -2
        // U+0130 LATIN CAPITAL LETTER I WITH DOT ABOVE (2 bytes) →
        //   "i\u{0307}" (3 bytes) = expand +1
        // 全体長は (3 + 2 + 2) → (1 + 3 + 3) = 7 で同じだが、per-char で
        // byte offset がずれているため `text_lc` の byte index を `text` に
        // 直接マップすると char-boundary 違反で panic する。
        // strict guard 経由で no-op (None) を返すこと。
        let text = "\u{212A}\u{0130}\u{0130}";
        assert_eq!(text.to_lowercase().len(), text.len());
        assert!(!is_lowercase_byte_aligned(text));
        let out = replace_word_case_insensitive(text, "kii", "Foo");
        assert!(
            out.is_none(),
            "strict guard must drop misaligned lowercase to avoid panic"
        );
        // wb_contains_for_test も同じ guard で false に倒す。
        assert!(!wb_contains_for_test(text, "kii"));
    }

    #[test]
    fn replace_word_case_insensitive_rewrites_to_canonical() {
        // text が既に canonical 表記なら None (= 実書き換え無し)。
        let out = replace_word_case_insensitive("we use Vegapunk daily", "vegapunk", "Vegapunk");
        assert_eq!(
            out, None,
            "no rewrite expected when match already equals canonical"
        );
        // 大文字 / 小文字違いは canonical へ書き換え。
        let out = replace_word_case_insensitive("we use VEGAPUNK daily", "vegapunk", "Vegapunk");
        assert_eq!(out.as_deref(), Some("we use Vegapunk daily"));
        let out = replace_word_case_insensitive("we use vegapunk daily", "vegapunk", "Vegapunk");
        assert_eq!(out.as_deref(), Some("we use Vegapunk daily"));
    }

    #[test]
    fn replace_word_case_insensitive_mixed_canonical_and_off_case() {
        // 1 文中で「既に canonical な match」と「書き換え対象 match」が
        // 混在するケース: alloc は遅延だが、書き換え後の出力は両方の match
        // を含む完全な文字列であること。
        let out =
            replace_word_case_insensitive("Vegapunk and VEGAPUNK rocks", "vegapunk", "Vegapunk");
        assert_eq!(out.as_deref(), Some("Vegapunk and Vegapunk rocks"));
        // 逆順 (= off-case が先、canonical が後) も同様に正しく結合される。
        let out =
            replace_word_case_insensitive("VEGAPUNK and Vegapunk rocks", "vegapunk", "Vegapunk");
        assert_eq!(out.as_deref(), Some("Vegapunk and Vegapunk rocks"));
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
            &EntityRef::new("Vegapunk".into(), "Project".into(), "sivira-shared".into()),
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

    // ── count_terminal_jobs_for_schema (= ingest sync-wait helper) ─────

    fn job_info(msg_id: Option<&str>, status: &str) -> vegapunk_client::graphrag::JobInfo {
        vegapunk_client::graphrag::JobInfo {
            job_id: "job-x".into(),
            job_type: "entity_extraction".into(),
            status: status.into(),
            error: None,
            created_at: 0,
            completed_at: None,
            msg_id: msg_id.map(|s| s.to_string()),
            retry_count: 0,
        }
    }

    #[test]
    fn count_terminal_filters_by_schema_prefix_and_counts_ok_vs_failed() {
        let jobs = vec![
            // 自分の schema、completed → ok
            job_info(Some("user-alice:gen1:msg-a"), "completed"),
            // 自分の schema、failed → failed
            job_info(Some("user-alice:gen1:msg-b"), "failed"),
            // 自分の schema、dead_letter → failed
            job_info(Some("user-alice:gen1:msg-c"), "dead_letter"),
            // 自分の schema、まだ pending → どちらにも入らない
            job_info(Some("user-alice:gen1:msg-d"), "pending"),
            // 自分の schema、running → 同上
            job_info(Some("user-alice:gen1:msg-e"), "running"),
            // 他テナント → 除外
            job_info(Some("user-bob:gen1:msg-x"), "completed"),
            // msg_id None → 除外
            job_info(None, "completed"),
        ];
        let (ok, failed) = count_terminal_jobs_for_schema(&jobs, "user-alice:");
        assert_eq!(ok, 1, "completed for user-alice");
        assert_eq!(failed, 2, "failed + dead_letter for user-alice");
    }

    #[test]
    fn count_terminal_rejects_prefix_shadow_user() {
        // user-alice が prefix の場合、user-alicemore は前方一致しない。
        // PR #22 の prefix-shadow と同じ落とし穴を回避できているか。
        let jobs = vec![
            job_info(Some("user-alice:gen1:msg-1"), "completed"),
            job_info(Some("user-alicemore:gen1:msg-2"), "completed"),
        ];
        let (ok, _) = count_terminal_jobs_for_schema(&jobs, "user-alice:");
        assert_eq!(ok, 1, "user-alicemore must NOT be counted");
    }

    #[test]
    fn count_terminal_treats_unknown_status_as_not_terminal() {
        // 仕様外の status は terminal 扱いしない (= 安全側に倒す)。
        let jobs = vec![
            job_info(Some("user-alice:gen1:msg-1"), "weird-future-status"),
            job_info(Some("user-alice:gen1:msg-2"), ""),
        ];
        let (ok, failed) = count_terminal_jobs_for_schema(&jobs, "user-alice:");
        assert_eq!(ok, 0);
        assert_eq!(failed, 0);
    }

    #[test]
    fn count_terminal_returns_zero_when_no_jobs() {
        let jobs: Vec<vegapunk_client::graphrag::JobInfo> = vec![];
        let (ok, failed) = count_terminal_jobs_for_schema(&jobs, "user-alice:");
        assert_eq!((ok, failed), (0, 0));
    }

    #[test]
    fn await_status_to_str_is_machine_readable() {
        assert_eq!(AwaitStatus::Ok.as_str(), "ok");
        assert_eq!(AwaitStatus::Partial.as_str(), "partial");
        assert_eq!(AwaitStatus::Timeout.as_str(), "timeout");
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
