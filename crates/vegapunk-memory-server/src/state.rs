//! 共有 state。axum handler から `State<AppState>` で取り回す想定。
//!
//! 本 PR では構造体定義のみ。実際の構築 (pool init / vegapunk connect / auth wiring)
//! は後続 PR で main から組み立てる。

use sqlx::SqlitePool;
use vegapunk_client::GraphRagClient;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub vegapunk: GraphRagClient,
}
