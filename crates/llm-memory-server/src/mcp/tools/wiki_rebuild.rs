use anyhow::{Result, anyhow};
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
    // クライアント入力は trust boundary 外: concept 名規約 (2-64 lowercase + hyphen)
    // を満たさないものは queue 投入前に reject。worker 側 (Haiku 出力) と同じ規約。
    if let Some(c) = a.concept.as_deref() {
        if !llm_memory_core::concept::is_valid(c) {
            return Err(anyhow!("invalid concept: {c}"));
        }
    }
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
