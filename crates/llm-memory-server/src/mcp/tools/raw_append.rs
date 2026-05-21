use anyhow::{Result, anyhow};
use llm_memory_core::scope::Scope;
use llm_memory_storage::raws::{NewRaw, insert};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

const MAX_CONTENT_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
struct Args {
    title: String,
    content: String,
    source: String,
    tags: Option<Vec<String>>,
}

pub async fn call(state: AppState, user: AuthenticatedUser, args: Value) -> Result<Value> {
    let a: Args = serde_json::from_value(args)?;
    if a.title.is_empty() || a.content.is_empty() {
        return Err(anyhow!("title and content required"));
    }
    if a.content.len() > MAX_CONTENT_BYTES {
        return Err(anyhow!("content exceeds 1 MB"));
    }
    let tags_json = a.tags.as_ref().map(|t| serde_json::to_string(t).unwrap());
    let r = insert(
        &state.pool,
        NewRaw {
            scope: Scope::Personal,
            owner_id: &user.user_id,
            title: &a.title,
            content: &a.content,
            source: &a.source,
            tags_json: tags_json.as_deref(),
            created_by: Some(&user.user_id),
        },
    )
    .await?;
    let started = state.coordinator.notify_append(&user.user_id).await;
    Ok(json!({ "raw_id": r.id, "rebuild_started": started }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::build_state_for_tests;
    use crate::config::ServerConfig;

    async fn state() -> AppState {
        let cfg = ServerConfig {
            database_url: "sqlite::memory:".into(),
            bind_addr: "0.0.0.0:8080".into(),
            public_url: "https://test.example.com".into(),
            google_client_id: "id".into(),
            google_client_secret: "s".into(),
            vertex_project: "test-project".into(),
            vertex_location: "us-central1".into(),
            model_extract: "h".into(),
            model_synth: "s".into(),
            trusted_proxy_count: 1,
        };
        build_state_for_tests(cfg).await.unwrap()
    }

    fn user() -> AuthenticatedUser {
        AuthenticatedUser {
            user_id: "u1".into(),
            client_id: "c1".into(),
        }
    }

    #[tokio::test]
    async fn rejects_empty_content() {
        let s = state().await;
        let args = json!({ "title": "t", "content": "", "source": "m" });
        let r = call(s, user(), args).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn rejects_oversized_content() {
        let s = state().await;
        let huge = "a".repeat(MAX_CONTENT_BYTES + 1);
        let args = json!({ "title": "t", "content": huge, "source": "m" });
        let r = call(s, user(), args).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn inserts_and_notifies() {
        let s = state().await;
        let args = json!({ "title": "t", "content": "c", "source": "manual", "tags": ["alpha"] });
        let r = call(s, user(), args).await.unwrap();
        assert!(r["raw_id"].is_string());
        // rebuild_started can be true or false (worker may have already fired and finished)
        assert!(r["rebuild_started"].is_boolean());
    }
}
