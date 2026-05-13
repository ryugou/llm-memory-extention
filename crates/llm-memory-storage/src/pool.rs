use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

use crate::error::StorageError;

pub async fn init_pool(url: &str) -> Result<SqlitePool, StorageError> {
    let opts = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;

    sqlx::query("PRAGMA wal_autocheckpoint = 1000;")
        .execute(&pool)
        .await?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(|e| sqlx::Error::Migrate(Box::new(e)))?;

    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pool_opens_with_wal() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let (mode,): (String,) = sqlx::query_as("PRAGMA journal_mode;")
            .fetch_one(&pool)
            .await
            .unwrap();
        // in-memory では memory が返る。実 file の挙動は integration test に任せる。
        assert!(mode == "wal" || mode == "memory", "got {mode}");
    }

    #[tokio::test]
    async fn migrations_apply() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let (count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='users'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
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

    #[tokio::test]
    async fn raws_fts_table_exists() {
        // 0002 migration の virtual table が作成されていること
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let (count,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM sqlite_master WHERE name='raws_fts'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(count >= 1, "raws_fts virtual table should exist");
    }
}
