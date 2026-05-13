use llm_memory_core::scope::Scope;
use llm_memory_core::time::now_ms;
use sqlx::SqlitePool;
use crate::error::StorageError;

pub async fn upsert(pool: &SqlitePool, scope: Scope, owner_id: &str, content: &str) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO schemas (scope, owner_id, content, updated_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT (scope, owner_id) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at",
    )
    .bind(scope.as_str()).bind(owner_id).bind(content).bind(now_ms())
    .execute(pool).await?;
    Ok(())
}

pub async fn get(pool: &SqlitePool, scope: Scope, owner_id: &str) -> Result<Option<String>, StorageError> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT content FROM schemas WHERE scope = ? AND owner_id = ?",
    ).bind(scope.as_str()).bind(owner_id).fetch_optional(pool).await?;
    Ok(row.map(|(c,)| c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn upsert_replaces() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "v1").await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "v2").await.unwrap();
        assert_eq!(get(&pool, Scope::Personal, "u1").await.unwrap().as_deref(), Some("v2"));
    }

    #[tokio::test]
    async fn get_returns_none_when_missing() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        assert!(get(&pool, Scope::Personal, "u1").await.unwrap().is_none());
    }
}
