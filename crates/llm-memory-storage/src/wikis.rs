use llm_memory_core::scope::Scope;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::error::StorageError;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, Serialize, Deserialize)]
pub struct Wiki {
    pub scope: String,
    pub owner_id: String,
    pub concept: String,
    pub content: String,
    pub source_refs: String,        // JSON array of raw ids
    pub last_rebuilt_at: i64,
}

pub async fn upsert(
    pool: &SqlitePool,
    scope: Scope,
    owner_id: &str,
    concept: &str,
    content: &str,
    source_refs_json: &str,
    last_rebuilt_at: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO wikis (scope, owner_id, concept, content, source_refs, last_rebuilt_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (scope, owner_id, concept) DO UPDATE SET
           content = excluded.content,
           source_refs = excluded.source_refs,
           last_rebuilt_at = excluded.last_rebuilt_at",
    )
    .bind(scope.as_str()).bind(owner_id).bind(concept).bind(content)
    .bind(source_refs_json).bind(last_rebuilt_at)
    .execute(pool).await?;
    Ok(())
}

pub async fn get(pool: &SqlitePool, scope: Scope, owner_id: &str, concept: &str) -> Result<Option<Wiki>, StorageError> {
    Ok(sqlx::query_as::<_, Wiki>(
        "SELECT scope, owner_id, concept, content, source_refs, last_rebuilt_at
         FROM wikis WHERE scope = ? AND owner_id = ? AND concept = ?",
    ).bind(scope.as_str()).bind(owner_id).bind(concept).fetch_optional(pool).await?)
}

pub async fn list_concepts(pool: &SqlitePool, scope: Scope, owner_id: &str) -> Result<Vec<String>, StorageError> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT concept FROM wikis WHERE scope = ? AND owner_id = ? ORDER BY concept",
    ).bind(scope.as_str()).bind(owner_id).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(c,)| c).collect())
}

pub async fn max_last_rebuilt_at(pool: &SqlitePool, scope: Scope, owner_id: &str) -> Result<i64, StorageError> {
    let (v,): (Option<i64>,) = sqlx::query_as(
        "SELECT MAX(last_rebuilt_at) FROM wikis WHERE scope = ? AND owner_id = ?",
    ).bind(scope.as_str()).bind(owner_id).fetch_one(pool).await?;
    Ok(v.unwrap_or(0))
}

pub async fn count_concepts(pool: &SqlitePool, scope: Scope, owner_id: &str) -> Result<i64, StorageError> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM wikis WHERE scope = ? AND owner_id = ?",
    ).bind(scope.as_str()).bind(owner_id).fetch_one(pool).await?;
    Ok(n)
}

pub async fn list_for_owner(pool: &SqlitePool, scope: Scope, owner_id: &str) -> Result<Vec<Wiki>, StorageError> {
    Ok(sqlx::query_as::<_, Wiki>(
        "SELECT scope, owner_id, concept, content, source_refs, last_rebuilt_at
         FROM wikis WHERE scope = ? AND owner_id = ? ORDER BY concept",
    ).bind(scope.as_str()).bind(owner_id).fetch_all(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn upsert_replaces_existing() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "concept-a", "v1", "[]", 100).await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "concept-a", "v2", "[]", 200).await.unwrap();
        let w = get(&pool, Scope::Personal, "u1", "concept-a").await.unwrap().unwrap();
        assert_eq!(w.content, "v2");
        assert_eq!(w.last_rebuilt_at, 200);
    }

    #[tokio::test]
    async fn max_last_rebuilt_at_returns_zero_when_empty() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let v = max_last_rebuilt_at(&pool, Scope::Personal, "u1").await.unwrap();
        assert_eq!(v, 0);
    }

    #[tokio::test]
    async fn list_concepts_alphabetical() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "zeta", "x", "[]", 1).await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "alpha", "x", "[]", 1).await.unwrap();
        assert_eq!(list_concepts(&pool, Scope::Personal, "u1").await.unwrap(), vec!["alpha", "zeta"]);
    }
}
