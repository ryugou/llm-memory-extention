use llm_memory_core::id::new_ulid;
use llm_memory_core::time::now_ms;
use sqlx::SqlitePool;
use crate::error::StorageError;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OAuthClient {
    pub id: String,
    pub redirect_uris: String,         // JSON array
    pub grant_types: String,           // JSON array
    pub token_endpoint_auth_method: String,
    pub client_name: Option<String>,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

pub async fn register(
    pool: &SqlitePool,
    redirect_uris_json: &str,
    grant_types_json: &str,
    auth_method: &str,
    client_name: Option<&str>,
) -> Result<OAuthClient, StorageError> {
    let id = new_ulid();
    let now = now_ms();
    sqlx::query(
        "INSERT INTO oauth_clients (id, redirect_uris, grant_types, token_endpoint_auth_method, client_name, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id).bind(redirect_uris_json).bind(grant_types_json).bind(auth_method).bind(client_name).bind(now)
    .execute(pool).await?;
    Ok(OAuthClient {
        id, redirect_uris: redirect_uris_json.into(), grant_types: grant_types_json.into(),
        token_endpoint_auth_method: auth_method.into(), client_name: client_name.map(Into::into),
        created_at: now, last_seen_at: None, revoked_at: None,
    })
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<OAuthClient>, StorageError> {
    Ok(sqlx::query_as::<_, OAuthClient>(
        "SELECT id, redirect_uris, grant_types, token_endpoint_auth_method, client_name, created_at, last_seen_at, revoked_at
         FROM oauth_clients WHERE id = ?",
    ).bind(id).fetch_optional(pool).await?)
}

pub async fn touch_last_seen(pool: &SqlitePool, id: &str) -> Result<(), StorageError> {
    sqlx::query("UPDATE oauth_clients SET last_seen_at = ? WHERE id = ?")
        .bind(now_ms()).bind(id).execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn register_and_get() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let c = register(&pool, r#"["https://example.com/cb"]"#, r#"["authorization_code"]"#, "none", Some("Claude")).await.unwrap();
        let got = get(&pool, &c.id).await.unwrap().unwrap();
        assert_eq!(got.client_name.as_deref(), Some("Claude"));
    }

    #[tokio::test]
    async fn touch_last_seen_updates() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let c = register(&pool, "[]", "[]", "none", None).await.unwrap();
        assert!(c.last_seen_at.is_none());
        touch_last_seen(&pool, &c.id).await.unwrap();
        let updated = get(&pool, &c.id).await.unwrap().unwrap();
        assert!(updated.last_seen_at.is_some());
    }
}
