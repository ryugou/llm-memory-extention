use std::env;

#[derive(Clone)]
pub struct ServerConfig {
    pub database_url: String,
    pub bind_addr: String,
    pub public_url: String,
    pub google_client_id: String,
    pub google_client_secret: String,
    // LLM (Vertex AI Gemini)
    pub vertex_project: String,
    pub vertex_location: String,
    pub model_extract: String,
    pub model_synth: String,
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
            google_client_id: env::var("GOOGLE_OAUTH_CLIENT_ID")
                .map_err(|_| anyhow::anyhow!("GOOGLE_OAUTH_CLIENT_ID not set"))?,
            google_client_secret: env::var("GOOGLE_OAUTH_CLIENT_SECRET")
                .map_err(|_| anyhow::anyhow!("GOOGLE_OAUTH_CLIENT_SECRET not set"))?,
            vertex_project: env::var("VERTEX_PROJECT")
                .map_err(|_| anyhow::anyhow!("VERTEX_PROJECT not set"))?,
            vertex_location: env::var("VERTEX_LOCATION").unwrap_or_else(|_| "us-central1".into()),
            model_extract: env::var("MODEL_EXTRACT").unwrap_or_else(|_| "gemini-2.5-flash".into()),
            model_synth: env::var("MODEL_SYNTH").unwrap_or_else(|_| "gemini-2.5-pro".into()),
            trusted_proxy_count: env::var("TRUSTED_PROXY_COUNT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1),
        })
    }
}
