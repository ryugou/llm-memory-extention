use std::env;

#[derive(Clone)]
pub struct ServerConfig {
    pub database_url: String,
    pub bind_addr: String,
    pub public_url: String,
    pub anthropic_api_key: String,
    pub google_client_id: String,
    pub google_client_secret: String,
    pub model_haiku: String,
    pub model_sonnet: String,
    pub trusted_proxy_count: usize,
}

impl ServerConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            database_url: env::var("DATABASE_URL")
                .map_err(|_| anyhow::anyhow!("DATABASE_URL not set"))?,
            bind_addr: env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            public_url: env::var("PUBLIC_URL")
                .map_err(|_| anyhow::anyhow!("PUBLIC_URL not set"))?,
            anthropic_api_key: env::var("ANTHROPIC_API_KEY")
                .map_err(|_| anyhow::anyhow!("ANTHROPIC_API_KEY not set"))?,
            google_client_id: env::var("GOOGLE_OAUTH_CLIENT_ID")
                .map_err(|_| anyhow::anyhow!("GOOGLE_OAUTH_CLIENT_ID not set"))?,
            google_client_secret: env::var("GOOGLE_OAUTH_CLIENT_SECRET")
                .map_err(|_| anyhow::anyhow!("GOOGLE_OAUTH_CLIENT_SECRET not set"))?,
            model_haiku: env::var("MODEL_HAIKU")
                .unwrap_or_else(|_| "claude-haiku-4-5-20251001".into()),
            model_sonnet: env::var("MODEL_SONNET").unwrap_or_else(|_| "claude-sonnet-4-6".into()),
            trusted_proxy_count: env::var("TRUSTED_PROXY_COUNT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1),
        })
    }
}
