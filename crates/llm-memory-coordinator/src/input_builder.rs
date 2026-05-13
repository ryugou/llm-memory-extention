use std::collections::HashSet;
use llm_memory_core::scope::Scope;
use llm_memory_storage::raws::Raw;
use llm_memory_storage::search::{self, SearchQuery};
use sqlx::SqlitePool;

use crate::error::CoordinatorError;

pub const INPUT_LIMIT: usize = 50;

/// Build the set of input raws for a Sonnet wiki synthesis call.
/// Order of preference: existing wiki source_refs, then new raws, then FTS top-k.
/// Capped at INPUT_LIMIT (50). Deduplicates by raw id.
pub async fn build(
    pool: &SqlitePool,
    scope: Scope,
    owner_id: &str,
    concept: &str,
    existing_source_refs: &[String],
    new_raws: &[Raw],
) -> Result<Vec<Raw>, CoordinatorError> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Raw> = Vec::new();

    // 1. 既存 source_refs を読み込む
    for id in existing_source_refs {
        if seen.contains(id) { continue; }
        if let Some(r) = llm_memory_storage::raws::get(pool, id).await? {
            seen.insert(r.id.clone());
            out.push(r);
            if out.len() >= INPUT_LIMIT { return Ok(out); }
        }
    }

    // 2. 新規 raws を追加
    for r in new_raws {
        if seen.contains(&r.id) { continue; }
        seen.insert(r.id.clone());
        out.push(r.clone());
        if out.len() >= INPUT_LIMIT { return Ok(out); }
    }

    // 3. FTS top-k で補完
    let remaining = INPUT_LIMIT.saturating_sub(out.len());
    if remaining > 0 {
        let hits = search::raws(pool, SearchQuery {
            query: concept,
            scope: Some(scope),
            owner_id: Some(owner_id),
            limit: (remaining as i64) * 2, // 重複見越して多めに
        }).await?;
        for h in hits {
            if seen.contains(&h.id) { continue; }
            seen.insert(h.id.clone());
            out.push(h);
            if out.len() >= INPUT_LIMIT { break; }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_memory_storage::pool::init_pool;
    use llm_memory_storage::raws::{insert, NewRaw};

    #[tokio::test]
    async fn limit_is_enforced() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        for i in 0..60 {
            insert(&pool, NewRaw {
                scope: Scope::Personal, owner_id: "u1",
                title: &format!("vegapunk-{i}"), content: "graphrag knowledge",
                source: "m", tags_json: None, created_by: Some("u1"),
            }).await.unwrap();
        }
        let out = build(&pool, Scope::Personal, "u1", "vegapunk", &[], &[]).await.unwrap();
        assert!(out.len() <= INPUT_LIMIT);
    }

    #[tokio::test]
    async fn existing_source_refs_come_first() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let r = insert(&pool, NewRaw {
            scope: Scope::Personal, owner_id: "u1", title: "old", content: "alpha",
            source: "m", tags_json: None, created_by: Some("u1"),
        }).await.unwrap();
        let out = build(&pool, Scope::Personal, "u1", "alpha", &[r.id.clone()], &[]).await.unwrap();
        assert_eq!(out[0].id, r.id);
    }

    #[tokio::test]
    async fn dedupes_across_sources() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let r = insert(&pool, NewRaw {
            scope: Scope::Personal, owner_id: "u1", title: "alpha thing", content: "alpha content",
            source: "m", tags_json: None, created_by: Some("u1"),
        }).await.unwrap();
        // r.id を existing_source_refs と new_raws の両方に入れる + FTS でも当たるはず
        let out = build(&pool, Scope::Personal, "u1", "alpha", &[r.id.clone()], &[r.clone()]).await.unwrap();
        let ids: Vec<_> = out.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec![r.id.as_str()], "same raw should appear only once");
    }
}
