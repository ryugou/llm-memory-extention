use anyhow::{Result, anyhow};
use llm_memory_core::scope::Scope;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args {
    scope: String,
    shared_memory_id: Option<String>,
}

pub async fn call(state: AppState, user: AuthenticatedUser, args: Value) -> Result<Value> {
    let a: Args = serde_json::from_value(args)?;
    let (scope, owner_id) = match a.scope.as_str() {
        "personal" => (Scope::Personal, user.user_id.clone()),
        "shared" => (
            Scope::Shared,
            a.shared_memory_id
                .clone()
                .ok_or_else(|| anyhow!("shared_memory_id required"))?,
        ),
        s => return Err(anyhow!("invalid scope: {s}")),
    };
    let content = llm_memory_storage::schemas::get(&state.pool, scope, &owner_id).await?;
    Ok(json!({ "content": content }))
}
