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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn create_and_list_all() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let id = SharedMemoryId::parse("team-x").unwrap();
        create(&pool, &id, "Team X", "shared-schema-team-x")
            .await
            .unwrap();
        let list = list_all(&pool).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "team-x");
        assert_eq!(list[0].name, "Team X");
        assert_eq!(list[0].vegapunk_schema, "shared-schema-team-x");
    }

    #[tokio::test]
    async fn find_by_id_returns_created_shared_memory() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let id = SharedMemoryId::parse("team-y").unwrap();
        create(&pool, &id, "Team Y", "shared-schema-y")
            .await
            .unwrap();
        let found = find_by_id(&pool, "team-y").await.unwrap().expect("exists");
        assert_eq!(found.id, "team-y");
        assert_eq!(found.vegapunk_schema, "shared-schema-y");
    }

    #[tokio::test]
    async fn find_by_id_returns_none_for_missing_id() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        assert!(find_by_id(&pool, "nonexistent").await.unwrap().is_none());
    }
}
