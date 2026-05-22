# Phase 1 Follow-up: Functional Bugs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** issue #2 の "機能バグ (優先)" セクションの 5 件を 1 PR で潰す: FTS5 escape, JSON-RPC parse envelope, invalid scope reject, Haiku concept post-validation, wiki_rebuild concept validation。

**Architecture:** 共通化のため `llm-memory-core` に concept validator を追加し、coordinator/worker と server/wiki_rebuild の双方から流用する。FTS5 escape は storage 内部に閉じた helper として置く。JSON-RPC envelope は transport 層で Bytes 受け + 手動 parse に変更。invalid scope reject は既存 raw_search / schema_read の anyhow!(...) パターンに合わせる。

**Tech Stack:** Rust 1.88, axum 0.7, sqlx 0.8 (SQLite + FTS5), serde_json, anyhow, thiserror, regex 1。

**Branch:** `feat/phase1-followup-functional-bugs` (main から切る)

**Related Issue:** https://github.com/ryugou/llm-memory-extention/issues/2

---

## Task 1: feat ブランチを切る

**Files:** なし (git のみ)

- [ ] **Step 1: ブランチを作成**

Run:
```bash
git checkout -b feat/phase1-followup-functional-bugs
git status
```
Expected: `On branch feat/phase1-followup-functional-bugs ... nothing to commit, working tree clean`

---

## Task 2: `llm-memory-core::concept` モジュール (concept name validator)

**Files:**
- Create: `crates/llm-memory-core/src/concept.rs`
- Modify: `crates/llm-memory-core/src/lib.rs`

**Why:** spec / prompt 上の concept 名規約 (2–64 chars, lowercase alphanumeric + hyphen) を 1 箇所で表現し、worker / wiki_rebuild から共有する。LLM 出力もクライアント入力も「trust boundary 外」なので必ず通す。

- [ ] **Step 1: 失敗テストを書く**

Create `crates/llm-memory-core/src/concept.rs`:
```rust
//! Concept name validation. Concept names live in URLs (wiki_read concept param),
//! in MCP tool args (wiki_rebuild concept), and in LLM-generated output (Haiku
//! affected_existing / new_concepts). LLM output is not a trust boundary, so the
//! same validator gates every entry point.

/// Returns true iff `s` is a valid concept name:
/// - length 2–64
/// - first char `[a-z0-9]`
/// - remaining chars `[a-z0-9-]`
///
/// Matches the format declared in `EXTRACT_CONCEPTS_SYSTEM` prompt and the
/// `^[a-z0-9][a-z0-9-]{1,63}$` regex shape used elsewhere for short identifiers.
pub fn is_valid(s: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"^[a-z0-9][a-z0-9-]{1,63}$").unwrap());
    re.is_match(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_typical_concepts() {
        assert!(is_valid("vegapunk"));
        assert!(is_valid("team-frontend"));
        assert!(is_valid("rust-2024"));
        assert!(is_valid("a1")); // min length 2
    }

    #[test]
    fn rejects_too_short() {
        assert!(!is_valid(""));
        assert!(!is_valid("a"));
    }

    #[test]
    fn rejects_too_long() {
        let s = "a".repeat(65);
        assert!(!is_valid(&s));
    }

    #[test]
    fn accepts_max_length() {
        let s = "a".repeat(64);
        assert!(is_valid(&s));
    }

    #[test]
    fn rejects_uppercase() {
        assert!(!is_valid("Vegapunk"));
        assert!(!is_valid("API"));
    }

    #[test]
    fn rejects_whitespace_and_specials() {
        assert!(!is_valid("with space"));
        assert!(!is_valid("dot.case"));
        assert!(!is_valid("snake_case"));
        assert!(!is_valid("slash/x"));
        assert!(!is_valid("quote\"x"));
    }

    #[test]
    fn rejects_leading_hyphen() {
        assert!(!is_valid("-leading"));
    }

    #[test]
    fn accepts_trailing_hyphen() {
        // SharedMemoryId と同じ regex shape を採用: 末尾 hyphen は許容。
        // (snake_case ではなく kebab-case にしている既存運用との一貫性)
        assert!(is_valid("trailing-"));
    }
}
```

- [ ] **Step 2: lib.rs に module を登録**

Modify `crates/llm-memory-core/src/lib.rs`:
```rust
pub mod concept;
pub mod error;
pub mod id;
pub mod scope;
pub mod time;
```

- [ ] **Step 3: regex crate を core に追加** (id.rs で既に使っているが Cargo.toml の dep を確認)

Run:
```bash
grep -n "regex" crates/llm-memory-core/Cargo.toml
```
Expected: `regex = ...` が既に存在 (id.rs で使っているため)。無ければ `regex = { workspace = true }` を追加。

- [ ] **Step 4: テスト実行 (失敗を確認)**

Run:
```bash
cargo test -p llm-memory-core --lib concept::
```
Expected: 全 8 テスト PASS (実装が同ファイルにあるため即 pass する。失敗確認は省略 — 単一 pure function かつ regex 1 つだけなので red-green-refactor の red は overkill)

- [ ] **Step 5: Commit**

```bash
git add crates/llm-memory-core/src/concept.rs crates/llm-memory-core/src/lib.rs
git commit -m "feat(core): add concept name validator (2-64 lowercase + hyphen)"
```

---

## Task 3: FTS5 MATCH 式の escape

**Files:**
- Modify: `crates/llm-memory-storage/src/search.rs:13-38`
- Modify: `crates/llm-memory-coordinator/src/input_builder.rs:14-77` (引数経由で渡るだけなので呼び出し側変更は不要。だが concept が `team-frontend` のように hyphen を含むケースの統合テストを追加)

**Why:** FTS5 MATCH は `-`, `"`, `OR`, `*` などを演算子として解釈する。`team-frontend` を素で渡すと「team AND NOT frontend」と誤解釈、SQL error、または無関係 hit。各バインド前に double-quote で wrap し、内部 `"` を `""` に escape する標準的な FTS5 phrase quoting を適用する。

- [ ] **Step 1: 失敗テストを書く (search.rs)**

Add to `crates/llm-memory-storage/src/search.rs` (既存 `tests` モジュール末尾に追加):
```rust
    #[tokio::test]
    async fn query_with_hyphen_does_not_blow_up_or_match_unrelated() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "team frontend retro",
                content: "react",
                source: "m",
                tags_json: None,
                created_by: Some("u1"),
            },
        )
        .await
        .unwrap();
        // hyphen を含む query。escape 無しだと FTS5 が "team NOT frontend" として解釈し
        // SQL error または hit 0。escape 後は phrase として 1 件 hit する。
        let res = raws(
            &pool,
            SearchQuery {
                query: "team-frontend",
                scope: Some(Scope::Personal),
                owner_id: Some("u1"),
                limit: 10,
            },
        )
        .await;
        assert!(res.is_ok(), "FTS5 must accept hyphenated query after escape");
    }

    #[tokio::test]
    async fn query_with_double_quote_does_not_break_sql() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "alpha quote",
                content: "x",
                source: "m",
                tags_json: None,
                created_by: Some("u1"),
            },
        )
        .await
        .unwrap();
        let res = raws(
            &pool,
            SearchQuery {
                query: r#"foo " bar"#, // 内部 " が含まれる
                scope: Some(Scope::Personal),
                owner_id: Some("u1"),
                limit: 10,
            },
        )
        .await;
        assert!(res.is_ok(), "FTS5 must accept query with inner double-quote");
    }

    #[tokio::test]
    async fn query_with_operator_keyword_is_literal() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "rules and exceptions",
                content: "x",
                source: "m",
                tags_json: None,
                created_by: Some("u1"),
            },
        )
        .await
        .unwrap();
        // 'AND' を演算子としてではなく literal phrase として扱う
        let res = raws(
            &pool,
            SearchQuery {
                query: "rules AND exceptions",
                scope: Some(Scope::Personal),
                owner_id: Some("u1"),
                limit: 10,
            },
        )
        .await;
        assert!(res.is_ok());
    }
```

- [ ] **Step 2: テスト実行 (失敗を確認)**

Run:
```bash
cargo test -p llm-memory-storage --lib search::tests::query_with_hyphen_does_not_blow_up_or_match_unrelated
```
Expected: FAIL (FTS5 syntax error or 0 rows on hyphen).

- [ ] **Step 3: fts5_escape helper を追加して呼び出し側で適用**

Modify `crates/llm-memory-storage/src/search.rs`:
```rust
use crate::error::StorageError;
use crate::raws::Raw;
use llm_memory_core::scope::Scope;
use sqlx::SqlitePool;

pub struct SearchQuery<'a> {
    pub query: &'a str,
    pub scope: Option<Scope>,
    pub owner_id: Option<&'a str>,
    pub limit: i64,
}

/// FTS5 MATCH 式に literal string を渡すための quoting。
/// SQLite FTS5 のクエリ言語では `-`, `*`, `OR`, `"` などが演算子になるため、
/// double-quote で囲んだ phrase に変換する。内部 `"` は `""` で escape する。
/// 結果は単純な phrase query (フレーズ一致) になる。
fn fts5_escape(s: &str) -> String {
    let escaped = s.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

pub async fn raws(pool: &SqlitePool, q: SearchQuery<'_>) -> Result<Vec<Raw>, StorageError> {
    // 1..=100 にクランプ。負値や巨大値による DoS/誤動作を防ぐ。
    let limit = q.limit.clamp(1, 100);
    let mut sql = String::from(
        "SELECT r.id, r.scope, r.owner_id, r.title, r.content, r.source, r.tags, r.created_by, r.created_at
         FROM raws_fts JOIN raws r ON r.rowid = raws_fts.rowid
         WHERE raws_fts MATCH ?",
    );
    let mut binds: Vec<String> = vec![fts5_escape(q.query)];
    if let Some(s) = q.scope {
        sql.push_str(" AND r.scope = ?");
        binds.push(s.as_str().into());
    }
    if let Some(o) = q.owner_id {
        sql.push_str(" AND r.owner_id = ?");
        binds.push(o.into());
    }
    sql.push_str(" ORDER BY bm25(raws_fts) ASC LIMIT ?");

    let mut query = sqlx::query_as::<_, Raw>(&sql);
    for b in &binds {
        query = query.bind(b);
    }
    query = query.bind(limit);
    Ok(query.fetch_all(pool).await?)
}
```

- [ ] **Step 4: テスト実行 (pass を確認)**

Run:
```bash
cargo test -p llm-memory-storage --lib search::
```
Expected: 全 7 テスト PASS。

- [ ] **Step 5: coordinator input_builder の integration テストを追加**

Add to `crates/llm-memory-coordinator/src/input_builder.rs` (既存 `tests` モジュール末尾):
```rust
    #[tokio::test]
    async fn build_with_hyphenated_concept_succeeds() {
        // FTS5 escape の回帰: concept = "team-frontend" でも SQL error が出ず空 hit で
        // pass する (search.rs 側で fts5_escape を通すため)。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let out = build(&pool, Scope::Personal, "u1", "team-frontend", &[], &[])
            .await;
        assert!(out.is_ok(), "hyphenated concept must not crash FTS5 search");
    }
```

- [ ] **Step 6: テスト実行**

Run:
```bash
cargo test -p llm-memory-coordinator --lib input_builder::
```
Expected: 全 4 テスト PASS。

- [ ] **Step 7: Commit**

```bash
git add crates/llm-memory-storage/src/search.rs crates/llm-memory-coordinator/src/input_builder.rs
git commit -m "fix(storage,coordinator): escape FTS5 MATCH input as phrase query"
```

---

## Task 4: JSON-RPC malformed body → `-32700 Parse error` envelope

**Files:**
- Modify: `crates/llm-memory-server/src/mcp/transport.rs:112-180`

**Why:** 現状 `Json<Value>` extractor が malformed JSON を HTTP 400 で reject する。MCP / JSON-RPC 2.0 §5.1 では `-32700 Parse error` JSON envelope を `id: null` で返すのが正しい挙動。`Bytes` 受けに変更して transport 層で手動 parse し、envelope を返す。

- [ ] **Step 1: 失敗テストを書く**

Add to `crates/llm-memory-server/src/mcp/transport.rs` の `tests` モジュール末尾:
```rust
    #[tokio::test]
    async fn malformed_json_body_returns_parse_error_envelope() {
        // JSON-RPC 2.0 §5.1: 完全に壊れた JSON は HTTP 200 で `-32700 Parse error`
        // envelope を `id: null` で返さなければならない (HTTP 400 で reject しない)。
        let state = test_state().await;
        // 不正な JSON 本文を直接送る
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from("{not json"))
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
        let v: Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["error"]["code"], -32700);
        assert!(v["id"].is_null());
        assert_eq!(v["jsonrpc"], "2.0");
    }
```

- [ ] **Step 2: テスト実行 (失敗を確認)**

Run:
```bash
cargo test -p llm-memory-server --lib mcp::transport::tests::malformed_json_body_returns_parse_error_envelope
```
Expected: FAIL (現状 axum が HTTP 400 を返す)。

- [ ] **Step 3: handle 関数のシグネチャを Bytes 受けに変更**

Modify `crates/llm-memory-server/src/mcp/transport.rs` の `handle` 関数 (line 123-180):
```rust
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
///
/// 本文の JSON parse 失敗は JSON-RPC 2.0 §5.1 に従い HTTP 200 で
/// `-32700 Parse error` envelope (`id: null`) を返す。axum の `Json<Value>`
/// だと HTTP 400 で reject されるため、`Bytes` 受けで手動 parse する。
pub async fn handle(
    State(state): State<AppState>,
    axum::Extension(user): axum::Extension<AuthenticatedUser>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let body: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            let err = JsonRpcResponse::error(None, -32700, format!("Parse error: {e}"));
            return Json(err).into_response();
        }
    };

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
```

注意: `use axum::Json;` は既に冒頭で import 済み。`use axum::body::Bytes` か `axum::body::Bytes` を直接 path で書くか — どちらでも良いが既存スタイルに合わせて `axum::body::Bytes` を path で使う。`use` 行の調整不要。

- [ ] **Step 4: テスト実行 (pass を確認)**

Run:
```bash
cargo test -p llm-memory-server --lib mcp::transport::
```
Expected: 全 13 テスト PASS (既存 12 + 新規 1)。

- [ ] **Step 5: Commit**

```bash
git add crates/llm-memory-server/src/mcp/transport.rs
git commit -m "fix(mcp): return -32700 envelope for malformed JSON body"
```

---

## Task 5: `wiki_read` / `wiki_list` の invalid scope を reject

**Files:**
- Modify: `crates/llm-memory-server/src/mcp/tools/wiki_read.rs:16-41`
- Modify: `crates/llm-memory-server/src/mcp/tools/wiki_list.rs:15-35`

**Why:** 現状 `matches!(mode, "all"|"personal")` で未知 scope を silently 空配列にしている。`raw_search.rs:23` / `schema_read.rs:25` と同じ `Err(anyhow!("invalid scope: {s}"))` パターンに揃える。typo の早期検知のため。

- [ ] **Step 1: 失敗テストを書く**

mcp/tools/mod.rs 末尾の `tests` モジュールに追加する形を取りたいが、既存パターンを確認するため:

Run:
```bash
grep -n "invalid_scope\|invalid scope" crates/llm-memory-server/src/mcp/tools/ -r
```

`wiki_read.rs` / `wiki_list.rs` には現状テストが無いため、`mcp/tools/mod.rs` の `tests` モジュールに統合テストとして書く。`raw_append` の既存テストパターン (line 313 付近) を参考にする。

まずは下記 unit テストを `crates/llm-memory-server/src/mcp/tools/mod.rs` 末尾の `tests` モジュールに追加:
```rust
    #[tokio::test]
    async fn wiki_read_invalid_scope_is_tool_error() {
        // 未知 scope (e.g., "all_users") は silent empty ではなく isError=true で返す。
        let state = state().await;
        let body = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"wiki_read","arguments":{"concept":"foo","scope":"unknown_scope"}}
        });
        let (status, v) = invoke(state, body).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["result"]["isError"], true);
    }

    #[tokio::test]
    async fn wiki_list_invalid_scope_is_tool_error() {
        let state = state().await;
        let body = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"wiki_list","arguments":{"scope":"unknown_scope"}}
        });
        let (status, v) = invoke(state, body).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["result"]["isError"], true);
    }
```

注意: `state` / `invoke` ヘルパは既存テストで定義済み (mod.rs line 249 以降)。`invoke` は transport.rs の `tests` 内で定義されているがそちらの中だけ。`mod.rs::tests` ではどう呼んでいるかを確認:

Run:
```bash
sed -n '243,360p' crates/llm-memory-server/src/mcp/tools/mod.rs
```

そこで使われているヘルパ ( `state()`, `invoke()` ) を使い回す。

- [ ] **Step 2: テスト実行 (失敗を確認)**

Run:
```bash
cargo test -p llm-memory-server --lib mcp::tools::tests::wiki_read_invalid_scope_is_tool_error mcp::tools::tests::wiki_list_invalid_scope_is_tool_error
```
Expected: FAIL (現状 isError=false で空配列が返る)。

- [ ] **Step 3: wiki_read.rs の scope ハンドリングを書き換え**

Replace `crates/llm-memory-server/src/mcp/tools/wiki_read.rs`:
```rust
use anyhow::{Result, anyhow};
use llm_memory_core::scope::Scope;
use llm_memory_storage::{shared_memories, wikis};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args {
    concept: String,
    scope: Option<String>,
}

pub async fn call(state: AppState, user: AuthenticatedUser, args: Value) -> Result<Value> {
    let a: Args = serde_json::from_value(args)?;
    // raw_search / schema_read と挙動を揃える: 未知 scope は silent empty ではなく
    // tool error として reject (typo の早期検知)。
    let mode = match a.scope.as_deref() {
        None | Some("all") => "all",
        Some("personal") => "personal",
        Some("shared") => "shared",
        Some(s) => return Err(anyhow!("invalid scope: {s}")),
    };
    let personal = if matches!(mode, "all" | "personal") {
        wikis::get(&state.pool, Scope::Personal, &user.user_id, &a.concept).await?
    } else {
        None
    };
    let shared = if matches!(mode, "all" | "shared") {
        let sms = shared_memories::list_all(&state.pool).await?;
        let mut out = Vec::new();
        for sm in sms {
            if let Some(w) = wikis::get(&state.pool, Scope::Shared, &sm.id, &a.concept).await? {
                out.push(w);
            }
        }
        out
    } else {
        vec![]
    };
    Ok(json!({
        "concept": a.concept,
        "personal": personal,
        "shared": shared,
    }))
}
```

- [ ] **Step 4: wiki_list.rs の scope ハンドリングを書き換え**

Replace `crates/llm-memory-server/src/mcp/tools/wiki_list.rs`:
```rust
use anyhow::{Result, anyhow};
use llm_memory_core::scope::Scope;
use llm_memory_storage::{shared_memories, wikis};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args {
    scope: Option<String>,
}

pub async fn call(state: AppState, user: AuthenticatedUser, args: Value) -> Result<Value> {
    let a: Args = serde_json::from_value(args)?;
    let mode = match a.scope.as_deref() {
        None | Some("all") => "all",
        Some("personal") => "personal",
        Some("shared") => "shared",
        Some(s) => return Err(anyhow!("invalid scope: {s}")),
    };
    let personal = if matches!(mode, "all" | "personal") {
        wikis::list_concepts(&state.pool, Scope::Personal, &user.user_id).await?
    } else {
        vec![]
    };
    let shared = if matches!(mode, "all" | "shared") {
        let sms = shared_memories::list_all(&state.pool).await?;
        let mut out = Vec::new();
        for sm in sms {
            let concepts = wikis::list_concepts(&state.pool, Scope::Shared, &sm.id).await?;
            out.push(json!({ "shared_memory_id": sm.id, "concepts": concepts }));
        }
        out
    } else {
        vec![]
    };
    Ok(json!({ "personal": personal, "shared": shared }))
}
```

- [ ] **Step 5: テスト実行**

Run:
```bash
cargo test -p llm-memory-server --lib mcp::tools::
```
Expected: 全 PASS。

- [ ] **Step 6: Commit**

```bash
git add crates/llm-memory-server/src/mcp/tools/wiki_read.rs crates/llm-memory-server/src/mcp/tools/wiki_list.rs crates/llm-memory-server/src/mcp/tools/mod.rs
git commit -m "fix(mcp): reject invalid scope in wiki_read/wiki_list"
```

---

## Task 6: Haiku 抽出 concept の post-validation

**Files:**
- Modify: `crates/llm-memory-coordinator/Cargo.toml` (llm-memory-core への dep は既にあるはず — 確認)
- Modify: `crates/llm-memory-coordinator/src/worker.rs:254-285`

**Why:** Haiku が返した concept 名は LLM 出力なので trust boundary 外。`existing_concepts` フィルタは既にあるが、フォーマット validation はない。invalid concept (uppercase, 空文字, 65 文字超, etc.) が wikis テーブルに upsert されると後段 (wiki_read URL/MCP arg) で再露出するため、worker 入口で reject する。

- [ ] **Step 1: 失敗テストを書く**

Worker のテストは既存 `crates/llm-memory-coordinator/src/worker.rs` の `#[cfg(test)] mod tests` (整合 / 統合) を見て追加場所を決める:

Run:
```bash
grep -n "mod tests\|#\[tokio::test\]" crates/llm-memory-coordinator/src/worker.rs | head -10
```

worker.rs のテストは MockClient を queue する形で書かれているはず。1 テスト追加:

`crates/llm-memory-coordinator/src/worker.rs` の `tests` モジュール末尾に:
```rust
    #[tokio::test]
    async fn haiku_returns_invalid_concept_names_filtered_out() {
        // Haiku が `INVALID` (大文字) や空文字を new_concepts に返してきても、
        // wikis に upsert される前に core::concept::is_valid で reject される。
        // 正常 concept は通る。
        use crate::state::StateMap;
        use llm_memory_llm::mock::MockClient;
        let pool = init_pool("sqlite::memory:").await.unwrap();
        // raws を 1 件投入 (Append session が走る最低条件)
        llm_memory_storage::raws::insert(
            &pool,
            llm_memory_storage::raws::NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "t",
                content: "c",
                source: "m",
                tags_json: None,
                created_by: Some("u1"),
            },
        )
        .await
        .unwrap();
        let mock = MockClient::new();
        // 1 回目: extract で valid + invalid 混在
        mock.queue_extract(serde_json::json!({
            "affected_existing": [],
            "new_concepts": ["valid-concept", "INVALID", "x"]  // x は 1 char で reject
        }));
        // synth 呼び出し用 (valid-concept 1 件分)
        mock.queue_synth(serde_json::json!({
            "content": "wiki content",
            "source_refs": []
        }));
        let deps = std::sync::Arc::new(WorkerDeps {
            pool: pool.clone(),
            state: StateMap::new(),
            llm: std::sync::Arc::new(mock),
            model_extract: "h".into(),
            model_synth: "s".into(),
            metrics: std::sync::Arc::new(crate::metrics::NoopMetricsSink),
        });
        let key = OwnerKey::personal("u1");
        // run_worker を直接走らせて結果を確認 (spawn だと panic safe 経路に行き観測しにくい)
        let _ = run_worker(deps.clone(), key.clone(), RebuildMode::Append).await;
        // valid-concept のみ wikis に存在し INVALID と x は無い
        let concepts = wikis::list_concepts(&pool, Scope::Personal, "u1").await.unwrap();
        assert!(concepts.contains(&"valid-concept".to_string()));
        assert!(!concepts.iter().any(|c| c == "INVALID"));
        assert!(!concepts.iter().any(|c| c == "x"));
    }
```

注意: `MockClient` の queue API 名は実装を要確認 (`queue_extract` / `queue_synth` 等):

Run:
```bash
grep -n "queue_\|pub fn" crates/llm-memory-llm/src/mock.rs
```
ここで判明した API 名にテストを合わせる。

- [ ] **Step 2: テスト実行 (失敗を確認)**

Run:
```bash
cargo test -p llm-memory-coordinator --lib worker::tests::haiku_returns_invalid_concept_names_filtered_out
```
Expected: FAIL (INVALID / x が wikis に upsert される)。

- [ ] **Step 3: worker.rs の Append モード分岐に validation を追加**

Modify `crates/llm-memory-coordinator/src/worker.rs` の line 254-285 付近 (Append モード分岐内):
```rust
                // 安全対策: Haiku が `affected_existing` に existing でない concept を入れて
                // 返してきても、それは無視する (そうしないと set 経由で Sonnet に投げられ
                // CONCEPT_LIMIT_PER_OWNER を bypass して wiki が新規作成されてしまう)。
                let existing_set: std::collections::HashSet<&String> =
                    existing_concepts.iter().collect();
                // LLM 出力は trust boundary 外: concept 名規約 (2-64 lowercase + hyphen)
                // を満たさないものは drop。drop 件数は warn ログで観測。
                let dropped_existing = extracted
                    .affected_existing
                    .iter()
                    .filter(|c| !llm_memory_core::concept::is_valid(c))
                    .count();
                if dropped_existing > 0 {
                    warn!(owner = ?key, dropped_existing, "Haiku returned invalid affected_existing names; dropped");
                }
                let mut set: std::collections::BTreeSet<String> = extracted
                    .affected_existing
                    .into_iter()
                    .filter(|c| llm_memory_core::concept::is_valid(c))
                    .filter(|c| existing_set.contains(c))
                    .collect();
                let current_count =
                    wikis::count_concepts(&deps.pool, key.scope, &key.owner_id).await?;
                // 残り枠だけ追加。current_count=199, new=100 でも 200 までで止める。
                let remaining = (CONCEPT_LIMIT_PER_OWNER - current_count).max(0) as usize;
                // LLM 出力は trust boundary 外: 形式 invalid な new_concepts は drop。
                let new_concepts_validated: Vec<String> = extracted
                    .new_concepts
                    .into_iter()
                    .filter(|c| {
                        if llm_memory_core::concept::is_valid(c) {
                            true
                        } else {
                            warn!(owner = ?key, concept = %c, "Haiku returned invalid new_concept name; dropped");
                            false
                        }
                    })
                    .collect();
                let new_total = new_concepts_validated.len();
                if remaining == 0 && new_total > 0 {
                    warn!(owner = ?key, current_count, "concept limit reached, ignoring new_concepts");
                } else if new_total > remaining {
                    warn!(
                        owner = ?key,
                        current_count,
                        new_total,
                        remaining,
                        "concept limit approached, truncated new_concepts"
                    );
                }
                for c in new_concepts_validated.into_iter().take(remaining) {
                    // 既存 concept と衝突する場合は set に入れるだけで新規 count を消費しない。
                    set.insert(c);
                }
                set.into_iter().collect()
```

- [ ] **Step 4: テスト実行 (pass を確認)**

Run:
```bash
cargo test -p llm-memory-coordinator --lib worker::
```
Expected: 全 PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/llm-memory-coordinator/src/worker.rs
git commit -m "fix(coordinator): post-validate Haiku concept names before upsert"
```

---

## Task 7: `wiki_rebuild` の manual concept を validate

**Files:**
- Modify: `crates/llm-memory-server/src/mcp/tools/wiki_rebuild.rs:15-27`

**Why:** クライアントが渡す `concept` を validate せず `coordinator.request_manual` に渡している。形式違反 (大文字 / 空文字 / 巨大) は coordinator / worker に進む前に reject。

- [ ] **Step 1: 失敗テストを書く**

Add to `crates/llm-memory-server/src/mcp/tools/mod.rs` の `tests` 末尾:
```rust
    #[tokio::test]
    async fn wiki_rebuild_invalid_concept_is_tool_error() {
        let state = state().await;
        let body = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"wiki_rebuild","arguments":{"concept":"INVALID UPPER"}}
        });
        let (status, v) = invoke(state, body).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["result"]["isError"], true);
    }

    #[tokio::test]
    async fn wiki_rebuild_omitted_concept_is_accepted() {
        // concept 省略 (= 全 concept 再合成) は引き続き有効
        let state = state().await;
        let body = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"wiki_rebuild","arguments":{}}
        });
        let (status, v) = invoke(state, body).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["result"]["isError"], false);
    }
```

- [ ] **Step 2: テスト実行 (失敗を確認)**

Run:
```bash
cargo test -p llm-memory-server --lib mcp::tools::tests::wiki_rebuild_invalid_concept_is_tool_error
```
Expected: FAIL (現状 isError=false でそのまま queue される)。

- [ ] **Step 3: wiki_rebuild.rs に validation を追加**

Replace `crates/llm-memory-server/src/mcp/tools/wiki_rebuild.rs`:
```rust
use anyhow::{Result, anyhow};
use llm_memory_coordinator::coordinator::ManualOutcome;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args {
    #[serde(default)]
    concept: Option<String>,
}

pub async fn call(state: AppState, user: AuthenticatedUser, args: Value) -> Result<Value> {
    let a: Args = serde_json::from_value(args)?;
    // クライアント入力は trust boundary 外: concept 名規約 (2-64 lowercase + hyphen)
    // を満たさないものは queue 投入前に reject。worker 側 (Haiku 出力) と同じ規約。
    if let Some(c) = a.concept.as_deref() {
        if !llm_memory_core::concept::is_valid(c) {
            return Err(anyhow!("invalid concept: {c}"));
        }
    }
    let r = state
        .coordinator
        .request_manual(&user.user_id, a.concept)
        .await;
    Ok(json!({
        "status": match r {
            ManualOutcome::Started => "started",
            ManualOutcome::Pending => "pending",
        }
    }))
}
```

- [ ] **Step 4: テスト実行**

Run:
```bash
cargo test -p llm-memory-server --lib mcp::tools::
```
Expected: 全 PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/llm-memory-server/src/mcp/tools/wiki_rebuild.rs crates/llm-memory-server/src/mcp/tools/mod.rs
git commit -m "fix(mcp): validate concept name in wiki_rebuild before queue"
```

---

## Task 8: 完了確認 + code-review + PR 作成

**Files:** なし (確認 + Github)

- [ ] **Step 1: 全 crate test + clippy + fmt**

Run:
```bash
cargo fmt --all --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: 全部 PASS / 0 error / 0 warning。

- [ ] **Step 2: simplify スキルを実行**

Skill ツールで `simplify` を invoke。差分を見直して dead code / 不要なネスト / 重複を除去。

- [ ] **Step 3: code-review スキルを実行**

Skill ツールで `code-review` を invoke。フローに従って全レビューラウンドを完走。

- [ ] **Step 4: issue #2 の対応項目を marked done する形式で記載**

PR description 内で issue #2 から今回対応した 5 項目を引用し `Closes #2 の機能バグセクションのみ` を明示。残 (Observability / Performance / Maintainability / 他) は別 PR で。

- [ ] **Step 5: PR 作成**

Run:
```bash
git push -u origin feat/phase1-followup-functional-bugs
gh pr create --title "fix: Phase 1 follow-up — functional bugs (FTS5 escape / parse envelope / scope reject / concept validation)" --body "$(cat <<'EOF'
## Summary
issue #2 の "機能バグ (優先)" 5 件を 1 PR で対応。

- FTS5 MATCH の入力を phrase quoting で escape (`team-frontend` などの hyphen / 演算子衝突を回避)
- malformed JSON 本文に対して JSON-RPC `-32700 Parse error` envelope を返す (axum 400 ではない)
- `wiki_read` / `wiki_list` の未知 scope を silent empty ではなく tool error で reject
- Haiku 抽出 concept 名を `llm-memory-core::concept::is_valid` で post-validation (LLM 出力は trust boundary 外)
- `wiki_rebuild` の concept 引数を同じ validator で pre-validation

## Refs
- Closes (partial) #2 — 機能バグ (優先) セクションのみ。Observability / Performance / Maintainability / Deployment はフォローアップ。

## Test plan
- [ ] `cargo test --workspace`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo fmt --all --check`
EOF
)"
```

- [ ] **Step 6: 残項目を issue #2 に追記コメント**

`gh issue comment 2 -b "feat/phase1-followup-functional-bugs (PR #N) で機能バグ 5 件を対応。残: Observability 1, Performance 3, Maintainability 3, Claim 整合性 1, Deployment 6, 細部 1 (合計 15 件) → 次 PR で。"`

---

## Self-Review

**1. Spec coverage:** issue #2 の機能バグセクション 5 件と Task 2-7 が 1:1 対応 — OK。
- FTS5 escape → Task 3
- JSON-RPC parse envelope → Task 4
- invalid scope silent empty → Task 5
- Haiku concept post-validation → Task 6
- wiki_rebuild concept validation → Task 7
- 共通 validator → Task 2

**2. Placeholder scan:** 各 Step に actual code / actual commands を記載。Task 6 の MockClient API 名のみ "要確認" として残しているが、これは事前に確認するためのコマンドを記載済み (placeholder ではない探索ステップ)。

**3. Type consistency:** `llm_memory_core::concept::is_valid` を Task 2 で定義し、Task 6 / Task 7 で使う。worker.rs の use 形式は path 直書きを採用 (既存ファイルに `use llm_memory_core::...` が既にあるが import が膨らみ過ぎないようインライン path 使用)。
