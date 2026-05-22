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
