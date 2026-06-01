use crate::error::StorageError;
use llm_memory_core::id::SharedMemoryId;
use llm_memory_core::time::now_ms;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, Serialize, Deserialize)]
pub struct SharedMemory {
    pub id: String,
    pub name: String,
    /// 対応する vegapunk schema 名。shared scope のリクエストはこの schema に向く。
    pub vegapunk_schema: String,
    pub created_at: i64,
}

pub async fn create(
    pool: &SqlitePool,
    id: &SharedMemoryId,
    name: &str,
    vegapunk_schema: &str,
) -> Result<SharedMemory, StorageError> {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO shared_memories (id, name, vegapunk_schema, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(id.as_str())
    .bind(name)
    .bind(vegapunk_schema)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(SharedMemory {
        id: id.as_str().to_string(),
        name: name.into(),
        vegapunk_schema: vegapunk_schema.into(),
        created_at: now,
    })
}

pub async fn list_all(pool: &SqlitePool) -> Result<Vec<SharedMemory>, StorageError> {
    Ok(sqlx::query_as::<_, SharedMemory>(
        "SELECT id, name, vegapunk_schema, created_at FROM shared_memories ORDER BY id",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn find_by_id(pool: &SqlitePool, id: &str) -> Result<Option<SharedMemory>, StorageError> {
    Ok(sqlx::query_as::<_, SharedMemory>(
        "SELECT id, name, vegapunk_schema, created_at FROM shared_memories WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}
