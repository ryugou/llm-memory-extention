//! vegapunk-memory-server entry point.
//!
//! 起動時:
//! 1. env から config を組み立て (config::ServerConfig::from_env)
//! 2. SQLite pool を init + migration
//! 3. vegapunk gRPC client を connect
//! 4. axum router を立てて bind
//!
//! 本 PR では config / state 雛形のみで bind 経路は未実装 (= 後続 PR で実装)。

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    let _config = vegapunk_memory_server::config::ServerConfig::from_env()?;
    tracing::info!("vegapunk-memory-server starting (skeleton, http transport not yet wired)");

    // TODO: pool init / vegapunk connect / axum bind は後続 PR で。
    Ok(())
}
