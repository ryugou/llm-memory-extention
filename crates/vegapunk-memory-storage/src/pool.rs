use crate::error::StorageError;
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;
use std::str::FromStr;

/// SQLite pool を初期化し、migration を適用する。
/// - `database_url`: `sqlite:///path/to/file.sqlite` or `sqlite::memory:`
pub async fn init_pool(database_url: &str) -> Result<SqlitePool, StorageError> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePool::connect_with(opts).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}
