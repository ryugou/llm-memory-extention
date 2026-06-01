use crate::error::StorageError;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::time::Duration;

/// SQLite pool を初期化し、migration を適用する。
/// - `database_url`: `sqlite:///path/to/file.sqlite` or `sqlite::memory:`
///
/// 接続設定は `llm-memory-storage::pool` と同等にして、本番 SQLite で
/// `database is locked` が出にくいようにする:
/// - WAL ジャーナル + synchronous=Normal で書き込み性能と安全性のバランス
/// - busy_timeout 5s で短時間の競合を再試行で吸収
/// - max_connections 8 + wal_autocheckpoint 1000 ページで checkpoint
pub async fn init_pool(database_url: &str) -> Result<SqlitePool, StorageError> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;

    sqlx::query("PRAGMA wal_autocheckpoint = 1000;")
        .execute(&pool)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pool_opens_with_wal_and_migrations_apply() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        // in-memory では memory が返る。実 file の挙動は integration test に任せる。
        let (mode,): (String,) = sqlx::query_as("PRAGMA journal_mode;")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(mode == "wal" || mode == "memory", "got {mode}");

        // 0001_initial.sql が走っていること
        let (users_count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='users'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(users_count, 1);
    }

    #[tokio::test]
    async fn foreign_keys_are_enabled() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let (fk,): (i64,) = sqlx::query_as("PRAGMA foreign_keys;")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(fk, 1, "foreign_keys must be ON");
    }
}
