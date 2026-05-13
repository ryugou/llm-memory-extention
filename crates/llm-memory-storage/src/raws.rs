use llm_memory_core::id::new_ulid;
use llm_memory_core::scope::Scope;
use llm_memory_core::time::now_ms;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::error::StorageError;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, Serialize, Deserialize)]
pub struct Raw {
    pub id: String,
    pub scope: String,
    pub owner_id: String,
    pub title: String,
    pub content: String,
    pub source: String,
    pub tags: Option<String>,
    pub created_by: Option<String>,
    pub created_at: i64,
}

pub struct NewRaw<'a> {
    pub scope: Scope,
    pub owner_id: &'a str,
    pub title: &'a str,
    pub content: &'a str,
    pub source: &'a str,
    pub tags_json: Option<&'a str>,
    pub created_by: Option<&'a str>,
}

pub async fn insert(pool: &SqlitePool, r: NewRaw<'_>) -> Result<Raw, StorageError> {
    let id = new_ulid();
    let now = now_ms();
    sqlx::query(
        "INSERT INTO raws (id, scope, owner_id, title, content, source, tags, created_by, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id).bind(r.scope.as_str()).bind(r.owner_id).bind(r.title).bind(r.content)
    .bind(r.source).bind(r.tags_json).bind(r.created_by).bind(now)
    .execute(pool).await?;
    Ok(Raw {
        id,
        scope: r.scope.as_str().into(),
        owner_id: r.owner_id.into(),
        title: r.title.into(),
        content: r.content.into(),
        source: r.source.into(),
        tags: r.tags_json.map(Into::into),
        created_by: r.created_by.map(Into::into),
        created_at: now,
    })
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Raw>, StorageError> {
    Ok(sqlx::query_as::<_, Raw>("SELECT * FROM raws WHERE id = ?").bind(id).fetch_optional(pool).await?)
}

pub async fn list_since(
    pool: &SqlitePool,
    scope: Scope,
    owner_id: &str,
    since_exclusive: i64,
    until_inclusive: i64,
) -> Result<Vec<Raw>, StorageError> {
    Ok(sqlx::query_as::<_, Raw>(
        "SELECT * FROM raws WHERE scope = ? AND owner_id = ? AND created_at > ? AND created_at <= ?
         ORDER BY created_at ASC, id ASC",
    )
    .bind(scope.as_str()).bind(owner_id).bind(since_exclusive).bind(until_inclusive)
    .fetch_all(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn insert_and_get() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let r = insert(&pool, NewRaw {
            scope: Scope::Personal,
            owner_id: "u1",
            title: "t",
            content: "c",
            source: "manual",
            tags_json: Some(r#"["a","b"]"#),
            created_by: Some("u1"),
        }).await.unwrap();
        let got = get(&pool, &r.id).await.unwrap().unwrap();
        assert_eq!(got.title, "t");
        assert_eq!(got.tags.as_deref(), Some(r#"["a","b"]"#));
    }

    #[tokio::test]
    async fn list_since_filters_by_range() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        for _ in 0..5 {
            insert(&pool, NewRaw {
                scope: Scope::Personal, owner_id: "u1", title: "t", content: "c",
                source: "manual", tags_json: None, created_by: Some("u1"),
            }).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        let until = now_ms();
        let all = list_since(&pool, Scope::Personal, "u1", 0, until).await.unwrap();
        assert_eq!(all.len(), 5);
        let none = list_since(&pool, Scope::Personal, "u1", until, until).await.unwrap();
        assert_eq!(none.len(), 0);
    }
}
