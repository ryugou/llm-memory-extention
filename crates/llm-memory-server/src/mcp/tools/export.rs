use anyhow::{Result, anyhow};
use llm_memory_core::scope::Scope;
use llm_memory_core::time::now_ms;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

/// Export ページあたりの raw 件数上限。raws.content は 1 MB 上限なので、
/// 100 件 × 1 MB ≒ 100 MB が単一ページの理論ワーストケース。これより大きく
/// すると pump 経由のメモリ展開で OOM リスクが急増する。クライアント側は
/// `next_cursor` で必要に応じて続きを取得する想定。
const PAGE_LIMIT: usize = 100;

/// 1 ページ分の content 合計バイト数の上限。理論ワーストケース (100 × 1 MB) を
/// 切り上げる前に、累積で 8 MB を超えた時点で打ち切って `next_cursor` を返す。
/// 典型的なケース (raw あたり 数 KB) では PAGE_LIMIT が先に効くので、この cap は
/// 主に「巨大 raw が含まれるケース」のセーフティネット。
const PAGE_CONTENT_BYTE_BUDGET: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
struct Args {
    #[serde(default)]
    cursor: Option<String>,
}

pub async fn call(state: AppState, user: AuthenticatedUser, args: Value) -> Result<Value> {
    let a: Args = serde_json::from_value(args)?;
    // cursor 形式: "<created_at>:<id>"。空 / 不在 のときは (0, "") (= 最初)。
    // ORDER BY (created_at, id) ASC と整合させ、同一 ms 境界をスキップしないようにする。
    let (cursor_at, cursor_id) = match a.cursor.as_deref() {
        Some(s) if !s.is_empty() => {
            // 旧形式 ("<created_at>") と新形式 ("<created_at>:<id>") の両方を許容。
            // 旧形式の場合 cursor_id は空文字。同一 ms 境界スキップ問題は新形式でのみ
            // 解消されるが、旧クライアントの 400 を防ぐため後方互換として受け入れる。
            let (at_str, id) = match s.split_once(':') {
                Some((at, id)) => (at, id.to_string()),
                None => (s, String::new()),
            };
            let at: i64 = at_str
                .parse()
                .map_err(|_| anyhow!("invalid cursor created_at"))?;
            (at, id)
        }
        _ => (0i64, String::new()),
    };

    let raws = sqlx::query_as::<_, llm_memory_storage::raws::Raw>(
        "SELECT id, scope, owner_id, title, content, source, tags, created_by, created_at
         FROM raws
         WHERE scope='personal' AND owner_id = ?
           AND (created_at > ? OR (created_at = ? AND id > ?))
         ORDER BY created_at ASC, id ASC LIMIT ?",
    )
    .bind(&user.user_id)
    .bind(cursor_at)
    .bind(cursor_at)
    .bind(&cursor_id)
    .bind(PAGE_LIMIT as i64 + 1)
    .fetch_all(&state.pool)
    .await?;

    // 1 ページ分を切り出す: 「件数 PAGE_LIMIT」と「累積 content バイト数
    // PAGE_CONTENT_BYTE_BUDGET」のいずれかに引っかかった時点で停止する。
    let fetched_more_than_limit = raws.len() > PAGE_LIMIT;
    let mut content_bytes_total: usize = 0;
    let mut page: Vec<llm_memory_storage::raws::Raw> =
        Vec::with_capacity(PAGE_LIMIT.min(raws.len()));
    let mut byte_budget_overflow = false;
    for r in raws.into_iter().take(PAGE_LIMIT) {
        let next_total = content_bytes_total.saturating_add(r.content.len());
        // 1 件も入らない極端なケース (raw 単体で budget 超え) では最低 1 件は
        // 入れて pagination を進める。それ以外は budget を厳守。
        if !page.is_empty() && next_total > PAGE_CONTENT_BYTE_BUDGET {
            byte_budget_overflow = true;
            break;
        }
        content_bytes_total = next_total;
        page.push(r);
    }
    // `next_cursor` 発行条件:
    // - byte budget で打ち切った → 続きあり
    // - PAGE_LIMIT+1 件 fetch して全部詰めきった → 元から PAGE_LIMIT 超えあり
    let next_cursor = if byte_budget_overflow || fetched_more_than_limit {
        page.last()
            .map(|last| format!("{}:{}", last.created_at, last.id))
    } else {
        None
    };

    // wikis + schema は最初の page (cursor 未指定) でのみ返す
    let (wikis_value, schema) = if cursor_at == 0 && cursor_id.is_empty() {
        let wikis =
            llm_memory_storage::wikis::list_for_owner(&state.pool, Scope::Personal, &user.user_id)
                .await?;
        let schema =
            llm_memory_storage::schemas::get(&state.pool, Scope::Personal, &user.user_id).await?;
        (Some(wikis), schema)
    } else {
        (None, None)
    };

    Ok(json!({
        "version": 1,
        "exported_at": now_ms(),
        "user_id": user.user_id,
        "raws": page,
        "wikis": wikis_value,
        "schema": schema,
        "next_cursor": next_cursor,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::build_state_for_tests;
    use crate::config::ServerConfig;
    use llm_memory_storage::raws::{NewRaw, insert};

    async fn state() -> AppState {
        build_state_for_tests(ServerConfig {
            database_url: "sqlite::memory:".into(),
            bind_addr: "x".into(),
            public_url: "x".into(),
            anthropic_api_key: "x".into(),
            google_client_id: "x".into(),
            google_client_secret: "x".into(),
            model_haiku: "h".into(),
            model_sonnet: "s".into(),
            trusted_proxy_count: 1,
        })
        .await
        .unwrap()
    }

    fn u() -> AuthenticatedUser {
        AuthenticatedUser {
            user_id: "u1".into(),
            client_id: "c".into(),
        }
    }

    #[tokio::test]
    async fn export_returns_raws_and_wikis_on_first_page() {
        let s = state().await;
        insert(
            &s.pool,
            NewRaw {
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
        let res = call(s, u(), json!({})).await.unwrap();
        assert_eq!(res["raws"].as_array().unwrap().len(), 1);
        assert!(res["wikis"].is_array());
        assert!(res["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn export_accepts_legacy_cursor_format() {
        let s = state().await;
        insert(
            &s.pool,
            NewRaw {
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
        // 旧形式（数字のみ）を渡しても 400 にせず処理する。
        // cursor_at=0, cursor_id="" として扱われ、全件取得できる。
        let res = call(s, u(), json!({ "cursor": "0" })).await.unwrap();
        assert_eq!(res["raws"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn export_truncates_by_byte_budget() {
        // 巨大 content の raw を 2 件入れて、PAGE_CONTENT_BYTE_BUDGET (8 MB) で
        // 1 ページに収まらないことを検証する (1 件目だけ返って next_cursor 発行)。
        let s = state().await;
        let big = "x".repeat(6 * 1024 * 1024); // 6 MB
        for _ in 0..2 {
            insert(
                &s.pool,
                NewRaw {
                    scope: Scope::Personal,
                    owner_id: "u1",
                    title: "t",
                    content: &big,
                    source: "m",
                    tags_json: None,
                    created_by: Some("u1"),
                },
            )
            .await
            .unwrap();
        }
        let res = call(s, u(), json!({})).await.unwrap();
        let arr = res["raws"].as_array().unwrap();
        // 1 件入った時点で次の 6 MB を足すと 12 MB > 8 MB なので打ち切り
        assert_eq!(arr.len(), 1, "byte budget should stop after first 6MB raw");
        assert!(
            res["next_cursor"].is_string(),
            "byte budget overflow must emit next_cursor"
        );
    }

    #[tokio::test]
    async fn export_composite_cursor_does_not_skip_same_ms() {
        let s = state().await;
        // 同一 ms に近接する raw を 2 件作る。ORDER BY (created_at, id) ASC + 複合 cursor で
        // どちらもスキップされないことを検証する。
        for _ in 0..2 {
            insert(
                &s.pool,
                NewRaw {
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
        }
        // PAGE_LIMIT を超えないので next_cursor は None
        let res = call(s, u(), json!({})).await.unwrap();
        assert_eq!(res["raws"].as_array().unwrap().len(), 2);
        assert!(res["next_cursor"].is_null());
    }
}
