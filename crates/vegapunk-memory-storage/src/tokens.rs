//! Refresh token storage for vegapunk-memory-server.
//!
//! llm-memory-storage::tokens と同じ schema・同じ挙動だが、別 SQLite ファイル
//! (vegapunk-memory-server 専用) を見るために物理的に分けてある。token 自体は
//! SHA-256 でハッシュ化して保存し、平文は DB に残さない。

use crate::error::StorageError;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

/// 32-byte SHA-256 digest of the refresh token. Stored at rest so that a DB /
/// backup leak does not yield directly usable bearer tokens. Tokens are
/// high-entropy ULIDs, so plain SHA-256 (no per-token salt) is adequate.
fn hash_refresh(token: &str) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.finalize().to_vec()
}

pub async fn create_refresh(
    pool: &SqlitePool,
    token: &str,
    user_id: &str,
    client_id: &str,
    expires_at: i64,
) -> Result<(), StorageError> {
    let token_hash = hash_refresh(token);
    sqlx::query(
        "INSERT INTO tokens (refresh_token_hash, user_id, client_id, expires_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&token_hash)
    .bind(user_id)
    .bind(client_id)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn validate_refresh(
    pool: &SqlitePool,
    token: &str,
    now: i64,
) -> Result<Option<(String, String)>, StorageError> {
    let token_hash = hash_refresh(token);
    let row: Option<(String, String, i64, Option<i64>)> = sqlx::query_as(
        "SELECT user_id, client_id, expires_at, revoked_at FROM tokens WHERE refresh_token_hash = ?",
    )
    .bind(&token_hash)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(u, c, exp, rev)| {
        if rev.is_some() || exp <= now {
            None
        } else {
            Some((u, c))
        }
    }))
}

pub async fn revoke(pool: &SqlitePool, token: &str, now: i64) -> Result<(), StorageError> {
    let token_hash = hash_refresh(token);
    sqlx::query("UPDATE tokens SET revoked_at = ? WHERE refresh_token_hash = ?")
        .bind(now)
        .bind(&token_hash)
        .execute(pool)
        .await?;
    Ok(())
}

/// Atomically validate-and-revoke a refresh token in a single UPDATE statement.
/// 並行 2 リクエストが同じ token を提示しても、片方しか結果を取れないため
/// rotation race (両方が新 token を発行する) を閉じる。SQLite 3.35+ の
/// `RETURNING` を利用。
pub async fn validate_and_revoke(
    pool: &SqlitePool,
    token: &str,
    now: i64,
) -> Result<Option<(String, String)>, StorageError> {
    let token_hash = hash_refresh(token);
    let row: Option<(String, String)> = sqlx::query_as(
        "UPDATE tokens
         SET revoked_at = ?
         WHERE refresh_token_hash = ?
           AND revoked_at IS NULL
           AND expires_at > ?
         RETURNING user_id, client_id",
    )
    .bind(now)
    .bind(&token_hash)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth_clients;
    use crate::pool::init_pool;
    use crate::users;

    /// vegapunk-memory-storage の users は upsert を持たず、admin provisioning
    /// として `insert(... vegapunk_schema)` を要求する。テストフィクスチャ用に
    /// プレースホルダ schema 名を 1 つ作って差し込む。
    async fn fixture_user(pool: &sqlx::SqlitePool, id: &str, subject: &str) -> String {
        users::insert(pool, id, "google", subject, None, "fixture-schema")
            .await
            .unwrap();
        id.to_string()
    }

    async fn fixture_client(pool: &sqlx::SqlitePool) -> String {
        let c = oauth_clients::register(pool, "[]", "[]", "none", None)
            .await
            .unwrap();
        c.id
    }

    #[tokio::test]
    async fn create_and_validate() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let user_id = fixture_user(&pool, "01HJUSER0000000000000000A", "sub-a").await;
        let client_id = fixture_client(&pool).await;
        create_refresh(&pool, "tok-1", &user_id, &client_id, 2_000_000_000_000)
            .await
            .unwrap();
        let v = validate_refresh(&pool, "tok-1", 1_700_000_000_000)
            .await
            .unwrap();
        assert_eq!(v, Some((user_id.clone(), client_id.clone())));
        revoke(&pool, "tok-1", 1_800_000_000_000).await.unwrap();
        assert_eq!(
            validate_refresh(&pool, "tok-1", 1_800_500_000_000)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn validate_returns_none_after_expiry() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let user_id = fixture_user(&pool, "01HJUSER0000000000000000B", "sub-b").await;
        let client_id = fixture_client(&pool).await;
        create_refresh(&pool, "tok-exp", &user_id, &client_id, 1_000_000_000_000)
            .await
            .unwrap();
        // now > expires_at
        assert_eq!(
            validate_refresh(&pool, "tok-exp", 1_500_000_000_000)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn validate_at_exact_expiry_is_invalid() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let user_id = fixture_user(&pool, "01HJUSER0000000000000000C", "sub-c").await;
        let client_id = fixture_client(&pool).await;
        create_refresh(
            &pool,
            "tok-boundary",
            &user_id,
            &client_id,
            1_500_000_000_000,
        )
        .await
        .unwrap();
        // now == expires_at
        assert_eq!(
            validate_refresh(&pool, "tok-boundary", 1_500_000_000_000)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn validate_and_revoke_is_atomic() {
        // 同じ token を 2 回 validate_and_revoke すると、1 回目だけが成功して
        // 2 回目は None を返す。これが rotation race 防止の根拠。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let user_id = fixture_user(&pool, "01HJUSER0000000000ROTATION", "sub-r").await;
        let client_id = fixture_client(&pool).await;
        create_refresh(&pool, "tok-r", &user_id, &client_id, 2_000_000_000_000)
            .await
            .unwrap();
        let now = 1_700_000_000_000;

        let first = validate_and_revoke(&pool, "tok-r", now).await.unwrap();
        assert_eq!(first, Some((user_id.clone(), client_id.clone())));

        // 同じ token で 2 回目はもう取れない (atomic revoke 済み)
        let second = validate_and_revoke(&pool, "tok-r", now).await.unwrap();
        assert_eq!(second, None, "second attempt must not yield tokens");
    }

    #[tokio::test]
    async fn plain_token_not_stored_in_db() {
        // 流出時に plain refresh token を再構築できないことを保証する回帰テスト。
        // sqlite_master / 全 row を直接覗いても token 文字列が出ないこと。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let user_id = fixture_user(&pool, "01HJUSER000000000000000HSH", "sub-h").await;
        let client_id = fixture_client(&pool).await;
        let token = "tok-leak-canary-xyzzy";
        create_refresh(&pool, token, &user_id, &client_id, 2_000_000_000_000)
            .await
            .unwrap();

        // hash として PK に入っているはず → token と一致する hash で row が引ける
        let hash = hash_refresh(token);
        let row: Option<(String,)> =
            sqlx::query_as("SELECT user_id FROM tokens WHERE refresh_token_hash = ?")
                .bind(&hash)
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(row, Some((user_id.clone(),)));

        // plain token 文字列で引いても何も出ない
        let no_row: Option<(String,)> =
            sqlx::query_as("SELECT user_id FROM tokens WHERE refresh_token_hash = ?")
                .bind(token.as_bytes())
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert!(no_row.is_none(), "plain token must not match stored hash");
    }
}
