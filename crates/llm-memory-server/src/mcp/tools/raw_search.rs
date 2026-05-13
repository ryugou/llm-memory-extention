use anyhow::{anyhow, Result};
use llm_memory_core::scope::Scope;
use llm_memory_storage::search::{self, SearchQuery};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args {
    query: String,
    scope: Option<String>,
    limit: Option<i64>,
}

pub async fn call(state: AppState, user: AuthenticatedUser, args: Value) -> Result<Value> {
    let a: Args = serde_json::from_value(args)?;
    let scope = match a.scope.as_deref() {
        None | Some("all") => None,
        Some("personal") => Some(Scope::Personal),
        Some("shared") => Some(Scope::Shared),
        Some(s) => return Err(anyhow!("invalid scope: {s}")),
    };
    let limit = a.limit.unwrap_or(20).clamp(1, 100);
    let mut results = Vec::new();
    if matches!(scope, None | Some(Scope::Personal)) {
        let mut hits = search::raws(
            &state.pool,
            SearchQuery {
                query: &a.query,
                scope: Some(Scope::Personal),
                owner_id: Some(&user.user_id),
                limit,
            },
        )
        .await?;
        results.append(&mut hits);
    }
    if matches!(scope, None | Some(Scope::Shared)) {
        let mut hits = search::raws(
            &state.pool,
            SearchQuery {
                query: &a.query,
                scope: Some(Scope::Shared),
                owner_id: None,
                limit,
            },
        )
        .await?;
        results.append(&mut hits);
    }
    results.truncate(limit as usize);
    Ok(json!({ "results": results }))
}
