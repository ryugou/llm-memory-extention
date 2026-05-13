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
    let cursor = a.cursor.unwrap_or_else(|| "0".into());
    let cursor_i: i64 = cursor.parse().map_err(|_| anyhow!("invalid cursor"))?;

    let raws = sqlx::query_as::<_, llm_memory_storage::raws::Raw>(
        "SELECT id, scope, owner_id, title, content, source, tags, created_by, created_at
         FROM raws WHERE scope='personal' AND owner_id = ? AND created_at > ?
         ORDER BY created_at ASC, id ASC LIMIT ?",
    )
    .bind(&user.user_id)
    .bind(cursor_i)
    .bind(PAGE_LIMIT as i64 + 1)
    .fetch_all(&state.pool)
    .await?;

    let next_cursor = if raws.len() > PAGE_LIMIT {
        Some(raws[PAGE_LIMIT - 1].created_at.to_string())
    } else {
        None
    };
    let page: Vec<_> = raws.into_iter().take(PAGE_LIMIT).collect();

    // wikis + schema は最初の page (cursor=0) でのみ返す
    let (wikis_value, schema) = if cursor_i == 0 {
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
}
