use anyhow::{anyhow, Result};
use serde_json::Value;
use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

pub async fn call(_state: AppState, _user: AuthenticatedUser, _args: Value) -> Result<Value> {
    Err(anyhow!("not implemented"))
}
