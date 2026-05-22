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
