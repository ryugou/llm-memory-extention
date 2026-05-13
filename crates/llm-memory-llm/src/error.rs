use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("anthropic api error: {0}")]
    Api(String),
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
