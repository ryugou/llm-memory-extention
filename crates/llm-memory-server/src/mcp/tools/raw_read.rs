use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args {
    id: String,
}

pub async fn call(state: AppState, user: AuthenticatedUser, args: Value) -> Result<Value> {
    let a: Args = serde_json::from_value(args)?;
    let raw = llm_memory_storage::raws::get(&state.pool, &a.id)
        .await?
        .ok_or_else(|| anyhow!("not found"))?;
    // 認可: personal は自分のみ。shared は誰でも。
    if raw.scope == "personal" && raw.owner_id != user.user_id {
        return Err(anyhow!("not found"));
    }
    Ok(serde_json::to_value(raw)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::build_state;
    use crate::config::ServerConfig;
    use llm_memory_core::scope::Scope;
    use llm_memory_storage::raws::{insert, NewRaw};
    use serde_json::json;

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

    fn u(id: &str) -> AuthenticatedUser {
        AuthenticatedUser {
            user_id: id.into(),
            client_id: "c".into(),
        }
    }

    #[tokio::test]
    async fn reads_own_personal_raw() {
        let s = state().await;
        let r = insert(
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
        let res = call(s, u("u1"), json!({ "id": r.id })).await.unwrap();
        assert_eq!(res["title"], "t");
    }

    #[tokio::test]
    async fn others_personal_raw_is_404() {
        let s = state().await;
        let r = insert(
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
        let res = call(s, u("u2"), json!({ "id": r.id })).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn shared_raw_accessible_to_anyone() {
        let s = state().await;
        let r = insert(
            &s.pool,
            NewRaw {
                scope: Scope::Shared,
                owner_id: "company-wide",
                title: "t",
                content: "c",
                source: "m",
                tags_json: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let res = call(s, u("anyone"), json!({ "id": r.id })).await.unwrap();
        assert_eq!(res["title"], "t");
    }
}
