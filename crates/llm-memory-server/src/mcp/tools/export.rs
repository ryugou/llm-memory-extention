use anyhow::{Result, anyhow};
use llm_memory_core::scope::Scope;
use llm_memory_core::time::now_ms;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

const PAGE_LIMIT: usize = 5000;

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
            let (at, id) = s
                .split_once(':')
                .ok_or_else(|| anyhow!("invalid cursor format"))?;
            let at: i64 = at
                .parse()
                .map_err(|_| anyhow!("invalid cursor created_at"))?;
            (at, id.to_string())
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

    let next_cursor = if raws.len() > PAGE_LIMIT {
        let last = &raws[PAGE_LIMIT - 1];
        Some(format!("{}:{}", last.created_at, last.id))
    } else {
        None
    };
    let page: Vec<_> = raws.into_iter().take(PAGE_LIMIT).collect();

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
    use crate::app::build_state;
    use crate::config::ServerConfig;
    use llm_memory_storage::raws::{NewRaw, insert};

    async fn state() -> AppState {
        build_state(ServerConfig {
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
