use llm_memory_core::id::SharedMemoryId;
use llm_memory_core::time::now_ms;
use sqlx::SqlitePool;

use crate::error::StorageError;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SharedMemory {
    pub id: String,
    pub name: String,
    pub created_at: i64,
}

pub async fn create(
    pool: &SqlitePool,
    id: &SharedMemoryId,
    name: &str,
) -> Result<SharedMemory, StorageError> {
    let now = now_ms();
    sqlx::query("INSERT INTO shared_memories (id, name, created_at) VALUES (?, ?, ?)")
        .bind(id.as_str())
        .bind(name)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(SharedMemory {
        id: id.as_str().to_string(),
        name: name.into(),
        created_at: now,
    })
}

pub async fn list_all(pool: &SqlitePool) -> Result<Vec<SharedMemory>, StorageError> {
    Ok(sqlx::query_as::<_, SharedMemory>(
        "SELECT id, name, created_at FROM shared_memories ORDER BY id",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn exists(pool: &SqlitePool, id: &str) -> Result<bool, StorageError> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM shared_memories WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn create_and_list() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let id = SharedMemoryId::parse("company-wide").unwrap();
        create(&pool, &id, "Company Wide").await.unwrap();
        let list = list_all(&pool).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "company-wide");
    }

    #[tokio::test]
    async fn exists_returns_correctly() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let id = SharedMemoryId::parse("team-x").unwrap();
        create(&pool, &id, "Team X").await.unwrap();
        assert!(exists(&pool, "team-x").await.unwrap());
        assert!(!exists(&pool, "team-y").await.unwrap());
    }
}
