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
    /// 対応する vegapunk schema 名。admin が事前に設定する。
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

/// 新規 user 行を作成する admin provisioning ヘルパ。
///
/// vegapunk_schema は OAuth 段階では確定しない (= ユーザ自身は自分の schema を
/// 知らない) ため、本サーバの認証経路では自動 INSERT しない。admin が
/// 事前に vegapunk Console / CLI で schema を作成 → ここで対応 user 行を
/// 投入 → 以降 OAuth 経路は既存行を SELECT して使う、という運用。
///
/// テストフィクスチャ用途と admin スクリプト用途を兼ねるため `pub` のまま
/// 残しているが、サーバ実装内の OAuth flow からは呼ばないこと。
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
