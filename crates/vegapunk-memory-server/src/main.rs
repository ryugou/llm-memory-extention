//! vegapunk-memory-server entry point.
//!
//! 起動順序:
//! 1. tracing-subscriber 初期化 (JSON ログ + env filter)
//! 2. env から `ServerConfig` を組み立て (`from_env` が空文字も拒否)
//! 3. `JwtKeys::from_env()` で署名鍵を読み込み (kid が空でないことも検証)
//! 4. `build_state` で pool init + vegapunk gRPC connect を済ませる
//! 5. `build_router` で OAuth / healthz を mount
//! 6. axum でバインドして listen
//!
//! MCP `/mcp` 経路は次 PR で `protected_router` 配下に足す。

use anyhow::Result;
use tracing_subscriber::EnvFilter;
use vegapunk_memory_auth::jwt::JwtKeys;
use vegapunk_memory_server::{app, config};

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
    // Fail-fast: missing / weak JWT signing keys は startup error にする。
    // 後段の OAuth 経路で MissingKid に化けて user 視点で「unauthorized」と
    // 見える事故を防ぐ。
    let jwt_keys = JwtKeys::from_env()?;
    let state = app::build_state(cfg, jwt_keys).await?;
    let router = app::build_router(state);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "vegapunk-memory-server starting");
    axum::serve(listener, router).await?;
    Ok(())
}
