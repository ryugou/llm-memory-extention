use anyhow::Result;
use llm_memory_core::scope::Scope;
use llm_memory_storage::{shared_memories, wikis};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use crate::mcp::tools::parse_scope_arg;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args {
    concept: String,
    scope: Option<String>,
}

pub async fn call(state: AppState, user: AuthenticatedUser, args: Value) -> Result<Value> {
    let a: Args = serde_json::from_value(args)?;
    let scope = parse_scope_arg(a.scope.as_deref())?;
    let personal = if matches!(scope, None | Some(Scope::Personal)) {
        wikis::get(&state.pool, Scope::Personal, &user.user_id, &a.concept).await?
    } else {
        None
    };
    let shared = if matches!(scope, None | Some(Scope::Shared)) {
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
