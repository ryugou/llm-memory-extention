use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("invalid token")]
    InvalidToken,
    #[error("missing kid")]
    MissingKid,
    #[error("unknown kid: {0}")]
    UnknownKid(String),
    #[error(transparent)]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error("oauth error: {0}")]
    OAuth(String),
}
