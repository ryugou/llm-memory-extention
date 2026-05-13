use anyhow::Result;
use llm_memory_server::{app, config};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    let cfg = config::ServerConfig::from_env()?;
    let bind = cfg.bind_addr.clone();
    let state = app::build_state(cfg).await?;
    let router = app::build_router(state);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "server starting");
    axum::serve(listener, router).await?;
    Ok(())
}
