use anyhow::Result;
use llm_memory_core::scope::Scope;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args {
    content: String,
}

pub async fn call(state: AppState, user: AuthenticatedUser, args: Value) -> Result<Value> {
    let a: Args = serde_json::from_value(args)?;
    llm_memory_storage::schemas::upsert(&state.pool, Scope::Personal, &user.user_id, &a.content)
        .await?;
    Ok(json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::build_state_for_tests;
    use crate::config::ServerConfig;

    async fn state() -> AppState {
        build_state_for_tests(ServerConfig {
            database_url: "sqlite::memory:".into(),
            bind_addr: "x".into(),
            public_url: "x".into(),
            google_client_id: "x".into(),
            google_client_secret: "x".into(),
            vertex_project: "test-project".into(),
            vertex_location: "us-central1".into(),
            model_extract: "h".into(),
            model_synth: "s".into(),
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
    async fn upserts_schema() {
        let s = state().await;
        call(s.clone(), u(), json!({ "content": "v1" }))
            .await
            .unwrap();
        let got = llm_memory_storage::schemas::get(&s.pool, Scope::Personal, "u1")
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some("v1"));
    }
}
