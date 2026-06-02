use crate::error::StorageError;
use llm_memory_core::time::now_ms;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub provider: String,
    pub subject: String,
    pub email: Option<String>,
    /// 対応する vegapunk schema 名。初回 OAuth callback で
    /// `user-{google_subject}` 形式 (現状 Google 以外の provider は無いので
    /// provider prefix は付けない) に自動生成される (`find_or_provision`)。
    /// 将来 Google 以外の provider を追加する場合は `user-{provider}-{subject}`
    /// 形に拡張して衝突を防ぐこと。
    /// 一度作られたら user 自身が動かす API は無く、middleware が tool 呼び
    /// 出し時の cross-tenant guard としてこの値を強制注入する。
    pub vegapunk_schema: String,
    pub created_at: i64,
}

/// `provider + subject` で user を引く (= Google OAuth flow 後に存在確認 + 取得)。
pub async fn find_by_provider_subject(
    pool: &SqlitePool,
    provider: &str,
    subject: &str,
) -> Result<Option<User>, StorageError> {
    Ok(sqlx::query_as::<_, User>(
        "SELECT id, provider, subject, email, vegapunk_schema, created_at
         FROM users WHERE provider = ? AND subject = ?",
    )
    .bind(provider)
    .bind(subject)
    .fetch_optional(pool)
    .await?)
}

pub async fn find_by_id(pool: &SqlitePool, id: &str) -> Result<Option<User>, StorageError> {
    Ok(sqlx::query_as::<_, User>(
        "SELECT id, provider, subject, email, vegapunk_schema, created_at FROM users WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// 新規 user 行を作成する低レベル INSERT。テストフィクスチャと
/// `find_or_provision` の内部で使う。OAuth 経路からは `find_or_provision`
/// を呼ぶこと (= 既存行があれば SELECT、無ければ INSERT) — `insert` を
/// 直接呼ぶと既存行の race で UNIQUE 違反になる。
pub async fn insert(
    pool: &SqlitePool,
    id: &str,
    provider: &str,
    subject: &str,
    email: Option<&str>,
    vegapunk_schema: &str,
) -> Result<User, StorageError> {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO users (id, provider, subject, email, vegapunk_schema, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(provider)
    .bind(subject)
    .bind(email)
    .bind(vegapunk_schema)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(User {
        id: id.into(),
        provider: provider.into(),
        subject: subject.into(),
        email: email.map(Into::into),
        vegapunk_schema: vegapunk_schema.into(),
        created_at: now,
    })
}

/// `provider + subject` で既存行を探し、無ければ与えられた id / schema で
/// 自動 INSERT する。OAuth callback で呼び、初回接続時の自動 provisioning
/// と既存ユーザの再ログインを同じ経路で扱う。
///
/// `id` / `vegapunk_schema` は **新規時のみ** 使われ、既存行は値が違っても
/// 上書きしない (= schema の名前変更は別 API。ここで上書きすると wrapper の
/// 強制注入 schema が user 操作で書き換わる事故になる)。`email` は新規時の
/// み記録。
///
/// 同じ未登録 user が 2 つの OAuth callback でほぼ同時に来た場合の race を
/// 吸収するため、`SELECT` → 無ければ `INSERT ... ON CONFLICT(provider, subject)
/// DO NOTHING` → もう一度 `SELECT` の 3 段。race の loser でも UNIQUE 違反で
/// 落ちず、相手の INSERT した行を返す。
pub async fn find_or_provision(
    pool: &SqlitePool,
    new_id: &str,
    provider: &str,
    subject: &str,
    email: Option<&str>,
    new_vegapunk_schema: &str,
) -> Result<User, StorageError> {
    if let Some(existing) = find_by_provider_subject(pool, provider, subject).await? {
        return Ok(existing);
    }
    let now = now_ms();
    sqlx::query(
        "INSERT INTO users (id, provider, subject, email, vegapunk_schema, created_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(provider, subject) DO NOTHING",
    )
    .bind(new_id)
    .bind(provider)
    .bind(subject)
    .bind(email)
    .bind(new_vegapunk_schema)
    .bind(now)
    .execute(pool)
    .await?;
    // race の loser 含めて確実に取れる: INSERT が conflict した場合は対応する
    // 行が既にコミットされている (SQLite は default で serializable に近い挙動)。
    find_by_provider_subject(pool, provider, subject)
        .await?
        .ok_or(StorageError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn insert_then_lookup_by_provider_subject() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(
            &pool,
            "u1",
            "google",
            "sub1",
            Some("x@example.com"),
            "schema-1",
        )
        .await
        .unwrap();
        let found = find_by_provider_subject(&pool, "google", "sub1")
            .await
            .unwrap()
            .expect("user should exist");
        assert_eq!(found.id, "u1");
        assert_eq!(found.email.as_deref(), Some("x@example.com"));
        assert_eq!(found.vegapunk_schema, "schema-1");
    }

    #[tokio::test]
    async fn find_by_id_returns_inserted_user_with_null_email() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(&pool, "u2", "google", "sub2", None, "schema-2")
            .await
            .unwrap();
        let found = find_by_id(&pool, "u2").await.unwrap().expect("exists");
        assert_eq!(found.id, "u2");
        assert!(found.email.is_none());
    }

    #[tokio::test]
    async fn find_by_id_returns_none_for_missing_user() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        assert!(find_by_id(&pool, "nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn find_or_provision_creates_on_first_call_and_returns_same_row_on_second() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let first = find_or_provision(
            &pool,
            "id-1",
            "google",
            "1234567890",
            Some("x@example.com"),
            "user-1234567890",
        )
        .await
        .unwrap();
        assert_eq!(first.id, "id-1");
        assert_eq!(first.vegapunk_schema, "user-1234567890");

        // 2nd call — provider/subject 同じだが id/schema を別値で渡しても、
        // 既存行をそのまま返す (上書きしない).
        let second = find_or_provision(
            &pool,
            "id-2-different",
            "google",
            "1234567890",
            Some("other@example.com"),
            "user-spoofed",
        )
        .await
        .unwrap();
        assert_eq!(second.id, "id-1", "must keep original id");
        assert_eq!(
            second.vegapunk_schema, "user-1234567890",
            "must keep original schema (cross-tenant guard)"
        );
        assert_eq!(second.email.as_deref(), Some("x@example.com"));
    }

    #[tokio::test]
    async fn find_or_provision_keys_on_provider_plus_subject() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        find_or_provision(&pool, "id-a", "google", "sub", None, "user-google-sub")
            .await
            .unwrap();
        // 異なる provider なら同じ subject でも別 user 行を作る.
        let other = find_or_provision(&pool, "id-b", "github", "sub", None, "user-github-sub")
            .await
            .unwrap();
        assert_eq!(other.id, "id-b");
        assert_eq!(other.vegapunk_schema, "user-github-sub");
    }

    #[tokio::test]
    async fn find_or_provision_is_concurrency_safe() {
        // 同じ未登録 (provider, subject) を 2 つの task が同時に provision しに
        // 来ても、両方 Ok を返す。負け側は UNIQUE 違反で落ちず、勝ち側が入れた
        // 同じ行を観測する (cross-tenant guard と同じ「先勝ち」仕様)。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let (a, b) = tokio::join!(
            tokio::spawn(async move {
                find_or_provision(&pool_a, "id-a", "google", "race-sub", None, "user-race-a").await
            }),
            tokio::spawn(async move {
                find_or_provision(&pool_b, "id-b", "google", "race-sub", None, "user-race-b").await
            }),
        );
        let a = a.unwrap().expect("task a ok");
        let b = b.unwrap().expect("task b ok");
        assert_eq!(a.id, b.id, "both tasks must observe the same row id");
        assert_eq!(
            a.vegapunk_schema, b.vegapunk_schema,
            "both tasks must observe the same schema"
        );
        // Whoever won, only their (id, schema) survives.
        assert!(matches!(a.id.as_str(), "id-a" | "id-b"));
    }
}
