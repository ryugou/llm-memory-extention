use sqlx::SqlitePool;
use crate::error::StorageError;

pub async fn create_refresh(
    pool: &SqlitePool,
    token: &str,
    user_id: &str,
    client_id: &str,
    expires_at: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO tokens (refresh_token, user_id, client_id, expires_at) VALUES (?, ?, ?, ?)",
    )
    .bind(token).bind(user_id).bind(client_id).bind(expires_at)
    .execute(pool).await?;
    Ok(())
}

pub async fn validate_refresh(pool: &SqlitePool, token: &str, now: i64) -> Result<Option<(String, String)>, StorageError> {
    let row: Option<(String, String, i64, Option<i64>)> = sqlx::query_as(
        "SELECT user_id, client_id, expires_at, revoked_at FROM tokens WHERE refresh_token = ?",
    ).bind(token).fetch_optional(pool).await?;
    Ok(row.and_then(|(u, c, exp, rev)| {
        if rev.is_some() || exp <= now { None } else { Some((u, c)) }
    }))
}

pub async fn revoke(pool: &SqlitePool, token: &str, now: i64) -> Result<(), StorageError> {
    sqlx::query("UPDATE tokens SET revoked_at = ? WHERE refresh_token = ?")
        .bind(now).bind(token).execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth_clients;
    use crate::users;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn create_and_validate() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let user = users::upsert(&pool, "01HJUSER0000000000000000A", "google", "sub", None).await.unwrap();
        let client = oauth_clients::register(&pool, "[]", "[]", "none", None).await.unwrap();
        create_refresh(&pool, "tok-1", &user.id, &client.id, 2_000_000_000_000).await.unwrap();
        let v = validate_refresh(&pool, "tok-1", 1_700_000_000_000).await.unwrap();
        assert_eq!(v, Some((user.id.clone(), client.id.clone())));
        revoke(&pool, "tok-1", 1_800_000_000_000).await.unwrap();
        assert_eq!(validate_refresh(&pool, "tok-1", 1_800_500_000_000).await.unwrap(), None);
    }

    #[tokio::test]
    async fn validate_returns_none_after_expiry() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let user = users::upsert(&pool, "01HJUSER0000000000000000B", "google", "sub-b", None).await.unwrap();
        let client = oauth_clients::register(&pool, "[]", "[]", "none", None).await.unwrap();
        create_refresh(&pool, "tok-exp", &user.id, &client.id, 1_000_000_000_000).await.unwrap();
        // now > expires_at
        assert_eq!(validate_refresh(&pool, "tok-exp", 1_500_000_000_000).await.unwrap(), None);
    }

    #[tokio::test]
    async fn validate_at_exact_expiry_is_invalid() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let user = users::upsert(&pool, "01HJUSER0000000000000000C", "google", "sub-c", None).await.unwrap();
        let client = oauth_clients::register(&pool, "[]", "[]", "none", None).await.unwrap();
        create_refresh(&pool, "tok-boundary", &user.id, &client.id, 1_500_000_000_000).await.unwrap();
        // now == expires_at
        assert_eq!(validate_refresh(&pool, "tok-boundary", 1_500_000_000_000).await.unwrap(), None);
    }
}
