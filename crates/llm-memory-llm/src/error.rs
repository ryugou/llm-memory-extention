use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("llm api error (status {status}): {message}")]
    Api { status: u16, message: String },
    #[error("parse error: {0}")]
    Parse(String),
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
