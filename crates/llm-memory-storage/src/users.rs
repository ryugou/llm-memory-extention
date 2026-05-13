use llm_memory_core::time::now_ms;
use sqlx::SqlitePool;

use crate::error::StorageError;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct User {
    pub id: String,
    pub provider: String,
    pub subject: String,
    pub email: Option<String>,
    pub created_at: i64,
}

pub async fn upsert(
    pool: &SqlitePool,
    id: &str,
    provider: &str,
    subject: &str,
    email: Option<&str>,
) -> Result<User, StorageError> {
    let now = now_ms();
    // 既存 (provider, subject) があれば email のみ更新、無ければ新規作成
    sqlx::query(
        "INSERT INTO users (id, provider, subject, email, created_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(provider, subject) DO UPDATE SET email = excluded.email",
    )
    .bind(id).bind(provider).bind(subject).bind(email).bind(now)
    .execute(pool).await?;

    let user: User = sqlx::query_as(
        "SELECT id, provider, subject, email, created_at FROM users WHERE provider = ? AND subject = ?"
    ).bind(provider).bind(subject).fetch_one(pool).await?;
    Ok(user)
}

pub async fn find_by_id(pool: &SqlitePool, id: &str) -> Result<Option<User>, StorageError> {
    let row = sqlx::query_as::<_, User>(
        "SELECT id, provider, subject, email, created_at FROM users WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn delete_cascade(pool: &SqlitePool, user_id: &str) -> Result<(), StorageError> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM raws WHERE scope='personal' AND owner_id = ?").bind(user_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM wikis WHERE scope='personal' AND owner_id = ?").bind(user_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM schemas WHERE scope='personal' AND owner_id = ?").bind(user_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM tokens WHERE user_id = ?").bind(user_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM users WHERE id = ?").bind(user_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn upsert_creates_user() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let u = upsert(&pool, "01HJTESTUSER0000000000000A", "google", "sub-1", Some("a@example.com")).await.unwrap();
        assert_eq!(u.provider, "google");
        assert_eq!(u.subject, "sub-1");
        assert_eq!(u.email.as_deref(), Some("a@example.com"));
    }

    #[tokio::test]
    async fn upsert_is_idempotent_on_provider_subject() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let u1 = upsert(&pool, "01HJ1", "google", "sub-x", Some("old@example.com")).await.unwrap();
        let u2 = upsert(&pool, "01HJ2", "google", "sub-x", Some("new@example.com")).await.unwrap();
        assert_eq!(u1.id, u2.id, "same provider+subject should map to same user");
        assert_eq!(u2.email.as_deref(), Some("new@example.com"));
    }

    #[tokio::test]
    async fn delete_cascade_removes_user_and_personal_data() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let u = upsert(&pool, "01HJDEL0000000000000000000", "google", "sub-del", None).await.unwrap();
        delete_cascade(&pool, &u.id).await.unwrap();
        assert!(find_by_id(&pool, &u.id).await.unwrap().is_none());
    }
}
