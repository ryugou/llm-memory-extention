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

    #[tokio::test]
    async fn get_returns_none_when_missing() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let w = get(&pool, Scope::Personal, "u1", "missing").await.unwrap();
        assert!(w.is_none());
    }

    #[tokio::test]
    async fn count_concepts_reflects_inserts() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        assert_eq!(count_concepts(&pool, Scope::Personal, "u1").await.unwrap(), 0);
        upsert(&pool, Scope::Personal, "u1", "c1", "x", "[]", 1).await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "c2", "x", "[]", 1).await.unwrap();
        assert_eq!(count_concepts(&pool, Scope::Personal, "u1").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn list_for_owner_returns_full_wikis() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "alpha", "content-a", r#"["raw1"]"#, 100).await.unwrap();
        let all = list_for_owner(&pool, Scope::Personal, "u1").await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].content, "content-a");
        assert_eq!(all[0].source_refs, r#"["raw1"]"#);
    }

    #[tokio::test]
    async fn owner_isolation_in_personal_scope() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "shared-name", "u1-content", "[]", 1).await.unwrap();
        upsert(&pool, Scope::Personal, "u2", "shared-name", "u2-content", "[]", 1).await.unwrap();
        let u1 = get(&pool, Scope::Personal, "u1", "shared-name").await.unwrap().unwrap();
        let u2 = get(&pool, Scope::Personal, "u2", "shared-name").await.unwrap().unwrap();
        assert_eq!(u1.content, "u1-content");
        assert_eq!(u2.content, "u2-content");
        assert_eq!(list_concepts(&pool, Scope::Personal, "u1").await.unwrap(), vec!["shared-name"]);
        assert_eq!(count_concepts(&pool, Scope::Personal, "u1").await.unwrap(), 1);
    }
}
