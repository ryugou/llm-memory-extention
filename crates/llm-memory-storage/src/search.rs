use crate::error::StorageError;
use crate::raws::Raw;
use llm_memory_core::scope::Scope;
use sqlx::SqlitePool;

pub struct SearchQuery<'a> {
    pub query: &'a str,
    pub scope: Option<Scope>,
    pub owner_id: Option<&'a str>,
    pub limit: i64,
}

pub async fn raws(pool: &SqlitePool, q: SearchQuery<'_>) -> Result<Vec<Raw>, StorageError> {
    // 1..=100 にクランプ。負値や巨大値による DoS/誤動作を防ぐ。
    let limit = q.limit.clamp(1, 100);
    let mut sql = String::from(
        "SELECT r.id, r.scope, r.owner_id, r.title, r.content, r.source, r.tags, r.created_by, r.created_at
         FROM raws_fts JOIN raws r ON r.rowid = raws_fts.rowid
         WHERE raws_fts MATCH ?",
    );
    let mut binds: Vec<String> = vec![q.query.into()];
    if let Some(s) = q.scope {
        sql.push_str(" AND r.scope = ?");
        binds.push(s.as_str().into());
    }
    if let Some(o) = q.owner_id {
        sql.push_str(" AND r.owner_id = ?");
        binds.push(o.into());
    }
    sql.push_str(" ORDER BY bm25(raws_fts) ASC LIMIT ?");

    let mut query = sqlx::query_as::<_, Raw>(&sql);
    for b in &binds {
        query = query.bind(b);
    }
    query = query.bind(limit);
    Ok(query.fetch_all(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;
    use crate::raws::{NewRaw, insert};

    #[tokio::test]
    async fn search_finds_inserted() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "Vegapunk overview",
                content: "GraphRAG knowledge engine",
                source: "manual",
                tags_json: None,
                created_by: Some("u1"),
            },
        )
        .await
        .unwrap();
        let res = raws(
            &pool,
            SearchQuery {
                query: "vegapunk",
                scope: Some(Scope::Personal),
                owner_id: Some("u1"),
                limit: 10,
            },
        )
        .await
        .unwrap();
        assert_eq!(res.len(), 1);
    }

    #[tokio::test]
    async fn negative_limit_is_clamped_to_one() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "Alpha",
                content: "x",
                source: "m",
                tags_json: None,
                created_by: Some("u1"),
            },
        )
        .await
        .unwrap();
        let res = raws(
            &pool,
            SearchQuery {
                query: "alpha",
                scope: Some(Scope::Personal),
                owner_id: Some("u1"),
                limit: -100,
            },
        )
        .await
        .unwrap();
        assert!(res.len() <= 1, "negative limit must clamp to 1");
    }

    #[tokio::test]
    async fn huge_limit_is_clamped_to_one_hundred() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        for i in 0..150 {
            insert(
                &pool,
                NewRaw {
                    scope: Scope::Personal,
                    owner_id: "u1",
                    title: &format!("alpha-{i}"),
                    content: "x",
                    source: "m",
                    tags_json: None,
                    created_by: Some("u1"),
                },
            )
            .await
            .unwrap();
        }
        let res = raws(
            &pool,
            SearchQuery {
                query: "alpha",
                scope: Some(Scope::Personal),
                owner_id: Some("u1"),
                limit: 9999,
            },
        )
        .await
        .unwrap();
        assert!(res.len() <= 100, "huge limit must clamp to 100");
    }

    #[tokio::test]
    async fn search_respects_owner_filter() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "Alpha",
                content: "x",
                source: "m",
                tags_json: None,
                created_by: Some("u1"),
            },
        )
        .await
        .unwrap();
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u2",
                title: "Alpha",
                content: "x",
                source: "m",
                tags_json: None,
                created_by: Some("u2"),
            },
        )
        .await
        .unwrap();
        let res = raws(
            &pool,
            SearchQuery {
                query: "alpha",
                scope: Some(Scope::Personal),
                owner_id: Some("u1"),
                limit: 10,
            },
        )
        .await
        .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].owner_id, "u1");
    }
}
