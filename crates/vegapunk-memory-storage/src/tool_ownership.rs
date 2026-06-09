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

/// 単一の (kind, foreign_id) → user_id を記録する。既存行 (= `INSERT OR
/// IGNORE` で無視された行) が **この user_id 所有か** を Bool で返す
/// (Copilot review #27 round 2)。これで caller は `ownership_recorded=true/
/// false` を正確に expose できる:
/// - `Ok(true)` : 今回新規挿入された、または既存行が同じ user_id 所有
///   だった (= verify が後で true を返す状態)
/// - `Ok(false)` : 既存行が **別 user_id 所有** だった (= verify が後で
///   false を返す状態、attacker による id 横取り試行や vegapunk 側衝突など)
///
/// search_id は vegapunk が UUID 風に発行するため通常は衝突しないが、
/// 万一の衝突や攻撃者の試行で「ingest_raw 成功なのに get_job_status が
/// 403」という不可逆な状態に黙って陥らないようにするため、確認 step を
/// 残す。
pub async fn record(
    pool: &SqlitePool,
    kind: OwnershipKind,
    foreign_id: &str,
    user_id: &str,
) -> Result<bool, StorageError> {
    let result = sqlx::query(
        "INSERT OR IGNORE INTO tool_ownership (kind, foreign_id, user_id, created_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(kind.as_str())
    .bind(foreign_id)
    .bind(user_id)
    .bind(now_ms())
    .execute(pool)
    .await?;
    if result.rows_affected() == 1 {
        // 新規挿入 → この user_id 所有。
        return Ok(true);
    }
    // IGNORE された (= 既存行あり)。同じ user_id 所有か念のため確認する。
    verify(pool, kind, foreign_id, user_id).await
}

/// 複数 foreign_id をまとめて記録する。1 ingest_raw あたり数十件の
/// msg_id を 1 transaction (= 1 commit) で INSERT する用途。
/// 各 id ごとに `execute` を発行するため SQLite との round trip は
/// 件数分だが、atomic 性は transaction で担保される。
///
/// Copilot review #27 round 2: `record` と同じく、全件が **この user_id
/// 所有** なら `true`、1 件でも別 user_id 所有が混じれば `false` を
/// 返す。`false` の時 caller (= `ingest_raw`) は `ownership_recorded:
/// false` を expose して client に retry 判断を任せる。
pub async fn record_many(
    pool: &SqlitePool,
    kind: OwnershipKind,
    foreign_ids: &[String],
    user_id: &str,
) -> Result<bool, StorageError> {
    if foreign_ids.is_empty() {
        return Ok(true);
    }
    let mut tx = pool.begin().await?;
    let now = now_ms();
    // INSERT 段は IGNORE 経由でまとめて回し、commit 後に「全件 user_id
    // 所有」かを 1 query で確認する (= per-row verify を transaction 内で
    // やると read-after-write の整合性は OK だが query 数が多い。commit
    // 後の bulk verify で十分)。
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

    // 既存行衝突で別 user 所有のままになった行が無いかを確認。1 件でも
    // 別 user_id 所有が混じれば false。
    for id in foreign_ids {
        if !verify(pool, kind, id, user_id).await? {
            return Ok(false);
        }
    }
    Ok(true)
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
        // 同じ (kind, foreign_id) を 2 度 record しても `INSERT OR IGNORE`
        // で既存 user_id が保護される。
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
