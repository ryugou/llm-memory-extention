//! 起動時設定。env var から読み込む。
//!
//! 必須:
//! - `DATABASE_URL`            (例: sqlite:///data/vegapunk-memory.sqlite)
//! - `BIND_ADDR`               (例: 0.0.0.0:8081)
//! - `PUBLIC_URL`              (例: https://vegapunk-136-110-78-245.nip.io)
//! - `GOOGLE_OAUTH_CLIENT_ID`  (Secret Manager 経由で注入)
//! - `GOOGLE_OAUTH_CLIENT_SECRET`
//! - `VEGAPUNK_GRPC_ENDPOINT`  (例: http://vegapunk.local:6840)
//! - `VEGAPUNK_BEARER_TOKEN`   (vegapunk server.auth.token と一致)
//! - `JWT_SIGNING_KEY_<kid>`   (少なくとも 1 つ、HS256 base64 32+ bytes)
//!
//! オプション:
//! - `TRUSTED_PROXY_COUNT`     (default 1)

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub database_url: String,
    pub bind_addr: String,
    pub public_url: String,
    pub google_client_id: String,
    pub google_client_secret: String,
    pub vegapunk_grpc_endpoint: String,
    pub vegapunk_bearer_token: String,
    pub trusted_proxy_count: usize,
}

impl ServerConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            database_url: env_required("DATABASE_URL")?,
            bind_addr: std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".to_string()),
            public_url: env_required("PUBLIC_URL")?,
            google_client_id: env_required("GOOGLE_OAUTH_CLIENT_ID")?,
            google_client_secret: env_required("GOOGLE_OAUTH_CLIENT_SECRET")?,
            vegapunk_grpc_endpoint: env_required("VEGAPUNK_GRPC_ENDPOINT")?,
            vegapunk_bearer_token: env_required("VEGAPUNK_BEARER_TOKEN")?,
            trusted_proxy_count: std::env::var("TRUSTED_PROXY_COUNT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1),
        })
    }
}

fn env_required(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("missing required env var: {key}"))
}
