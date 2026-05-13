use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("anthropic api error (status {status}): {message}")]
    Api { status: u16, message: String },
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
