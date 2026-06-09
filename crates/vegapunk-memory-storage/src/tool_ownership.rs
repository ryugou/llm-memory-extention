//! `tool_ownership` テーブルの低レベル API。
//!
//! vegapunk MCP の `feedback` / `get_job_status` は引数 (`search_id` /
//! `msg_id`) に schema を含まないため、PR #15 の cross-tenant guard
//! (= request の schema を user の schema に強制注入) が効かない経路。
//! 本モジュールは search / ingest_raw 経路で取得した id を user_id と
//! 紐付けて記録し、feedback / get_job_status で検証することで、他 tenant
//! の id を試行で叩けない state を保証する。
//!
//! migration: `migrations/0002_tool_ownership.sql`

use crate::error::StorageError;
use llm_memory_core::time::now_ms;
use sqlx::SqlitePool;

/// `kind` の取りうる値。`Search` = `SearchResponse.search_id`、`Msg` =
/// `IngestRawResponse.msg_ids[i]`。文字列値を直接渡すと typo で
/// silent fallback するので enum に閉じ込めて使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipKind {
    Search,
    Msg,
}

impl OwnershipKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OwnershipKind::Search => "search",
            OwnershipKind::Msg => "msg",
        }
    }
}

/// 単一の (kind, foreign_id) → user_id を記録する。既に行があれば
/// 何もしない (= idempotent、`INSERT OR IGNORE` で実装)。
///
/// search_id は vegapunk が UUID 風に発行するため衝突は起き得ないが、
/// 例えば同じ search を別 user が `search_id` を当てに来た時にここで
/// 別 user_id の上書きが起きると ownership が壊れる。`OR IGNORE` で
/// 既存値を保護する。
pub async fn record(
    pool: &SqlitePool,
    kind: OwnershipKind,
    foreign_id: &str,
    user_id: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT OR IGNORE INTO tool_ownership (kind, foreign_id, user_id, created_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(kind.as_str())
    .bind(foreign_id)
    .bind(user_id)
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

/// 複数 foreign_id をまとめて記録する。1 ingest_raw あたり数十件の
/// msg_id を 1 round trip で INSERT する用途。
pub async fn record_many(
    pool: &SqlitePool,
    kind: OwnershipKind,
    foreign_ids: &[String],
    user_id: &str,
) -> Result<(), StorageError> {
    if foreign_ids.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    let now = now_ms();
    for id in foreign_ids {
        sqlx::query(
            "INSERT OR IGNORE INTO tool_ownership (kind, foreign_id, user_id, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(kind.as_str())
        .bind(id)
        .bind(user_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// `user_id` が `(kind, foreign_id)` を所有しているか確認する。
/// - 行が存在し user_id が一致 → `Ok(true)`
/// - 行が存在するが別 user_id → `Ok(false)` (= 403)
/// - 行が存在しない → `Ok(false)` (= 不明な id、403 で安全側に倒す)
///
/// caller は false の時 403 PermissionDenied で返す想定。
pub async fn verify(
    pool: &SqlitePool,
    kind: OwnershipKind,
    foreign_id: &str,
    user_id: &str,
) -> Result<bool, StorageError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT user_id FROM tool_ownership WHERE kind = ? AND foreign_id = ?")
            .bind(kind.as_str())
            .bind(foreign_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.is_some_and(|(owner,)| owner == user_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;
    use crate::users::insert as insert_user;

    async fn setup() -> SqlitePool {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert_user(&pool, "u1", "google", "sub1", None, "u1-schema")
            .await
            .unwrap();
        insert_user(&pool, "u2", "google", "sub2", None, "u2-schema")
            .await
            .unwrap();
        pool
    }

    #[tokio::test]
    async fn record_then_verify_returns_true_for_owner() {
        let pool = setup().await;
        record(&pool, OwnershipKind::Search, "sid-1", "u1")
            .await
            .unwrap();
        assert!(
            verify(&pool, OwnershipKind::Search, "sid-1", "u1")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn verify_returns_false_for_other_user() {
        let pool = setup().await;
        record(&pool, OwnershipKind::Search, "sid-1", "u1")
            .await
            .unwrap();
        assert!(
            !verify(&pool, OwnershipKind::Search, "sid-1", "u2")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn verify_returns_false_for_unknown_id() {
        let pool = setup().await;
        // record 無し、unknown id を verify。
        assert!(
            !verify(&pool, OwnershipKind::Search, "unknown", "u1")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn record_is_idempotent() {
        // 同じ (kind, foreign_id) を 2 度 record しても OK_IGNORE で
        // 既存 user_id が保護される。
        let pool = setup().await;
        record(&pool, OwnershipKind::Search, "sid-1", "u1")
            .await
            .unwrap();
        record(&pool, OwnershipKind::Search, "sid-1", "u2")
            .await
            .unwrap();
        // 元の u1 が残っているはず。
        assert!(
            verify(&pool, OwnershipKind::Search, "sid-1", "u1")
                .await
                .unwrap()
        );
        assert!(
            !verify(&pool, OwnershipKind::Search, "sid-1", "u2")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn kind_is_separate_namespace() {
        // 同じ foreign_id でも kind が違えば別 entry。
        let pool = setup().await;
        record(&pool, OwnershipKind::Search, "abc", "u1")
            .await
            .unwrap();
        record(&pool, OwnershipKind::Msg, "abc", "u2")
            .await
            .unwrap();
        assert!(
            verify(&pool, OwnershipKind::Search, "abc", "u1")
                .await
                .unwrap()
        );
        assert!(
            verify(&pool, OwnershipKind::Msg, "abc", "u2")
                .await
                .unwrap()
        );
        assert!(
            !verify(&pool, OwnershipKind::Search, "abc", "u2")
                .await
                .unwrap()
        );
        assert!(
            !verify(&pool, OwnershipKind::Msg, "abc", "u1")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn record_many_batch_inserts() {
        let pool = setup().await;
        let ids = vec!["m1".to_string(), "m2".to_string(), "m3".to_string()];
        record_many(&pool, OwnershipKind::Msg, &ids, "u1")
            .await
            .unwrap();
        for id in &ids {
            assert!(verify(&pool, OwnershipKind::Msg, id, "u1").await.unwrap());
        }
    }

    #[tokio::test]
    async fn record_many_empty_is_noop() {
        let pool = setup().await;
        record_many(&pool, OwnershipKind::Msg, &[], "u1")
            .await
            .unwrap();
    }
}
