use anyhow::Result;
use llm_memory_coordinator::coordinator::ManualOutcome;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args {
    #[serde(default)]
    concept: Option<String>,
}

pub async fn call(state: AppState, user: AuthenticatedUser, args: Value) -> Result<Value> {
    let a: Args = serde_json::from_value(args)?;
    let r = state
        .coordinator
        .request_manual(&user.user_id, a.concept)
        .await;
    Ok(json!({
        "status": match r {
            ManualOutcome::Started => "started",
            ManualOutcome::Pending => "pending",
        }
    }))
}
