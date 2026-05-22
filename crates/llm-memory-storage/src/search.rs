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

/// FTS5 MATCH 式に literal string を渡すための quoting。
/// 空白区切りの各 token を `"..."` (FTS5 phrase) で個別に wrap し、空白 join する。
/// FTS5 は空白区切りの phrase 列を implicit AND として扱うため、
/// `foo bar` は `foo` と `bar` 両方を含む doc に hit する (term-AND 意味を維持)。
/// token 内部の `-`, `*`, `OR`, `AND`, `NEAR` などは phrase の中で literal 扱いになる。
/// 内部 `"` は `""` で escape する。
/// 空 / whitespace-only 入力は空 phrase (`""`) を返し、no rows で安全に終わる。
fn fts5_escape(s: &str) -> String {
    let tokens: Vec<String> = s
        .split_whitespace()
        .map(|tok| {
            let escaped = tok.replace('"', "\"\"");
            format!("\"{escaped}\"")
        })
        .collect();
    if tokens.is_empty() {
        return "\"\"".into();
    }
    tokens.join(" ")
}

pub async fn raws(pool: &SqlitePool, q: SearchQuery<'_>) -> Result<Vec<Raw>, StorageError> {
    // 1..=100 にクランプ。負値や巨大値による DoS/誤動作を防ぐ。
    let limit = q.limit.clamp(1, 100);
    let mut sql = String::from(
        "SELECT r.id, r.scope, r.owner_id, r.title, r.content, r.source, r.tags, r.created_by, r.created_at
         FROM raws_fts JOIN raws r ON r.rowid = raws_fts.rowid
         WHERE raws_fts MATCH ?",
    );
    let mut binds: Vec<String> = vec![fts5_escape(q.query)];
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
    async fn query_with_hyphen_does_not_error() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "team frontend retro",
                content: "react",
                source: "m",
                tags_json: None,
                created_by: Some("u1"),
            },
        )
        .await
        .unwrap();
        // hyphen を含む query。escape 無しだと FTS5 が "team NOT frontend" として解釈し
        // SQL error または unrelated hit になる。escape 後は phrase として無 hit で
        // 返るのが正しい (title/content に "team-frontend" という連結文字列は無いため)。
        let res = raws(
            &pool,
            SearchQuery {
                query: "team-frontend",
                scope: Some(Scope::Personal),
                owner_id: Some("u1"),
                limit: 10,
            },
        )
        .await;
        assert!(
            res.is_ok(),
            "FTS5 must accept hyphenated query after escape"
        );
    }

    #[tokio::test]
    async fn query_with_double_quote_does_not_error() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "alpha quote",
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
                query: r#"foo " bar"#,
                scope: Some(Scope::Personal),
                owner_id: Some("u1"),
                limit: 10,
            },
        )
        .await;
        assert!(
            res.is_ok(),
            "FTS5 must accept query with inner double-quote"
        );
    }

    #[tokio::test]
    async fn empty_query_returns_no_rows_without_error() {
        // 空文字 query は escape 後 `""` (empty phrase) になる。
        // SQLite FTS5 では empty phrase は no match を返すだけで SQL error にならない。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "x",
                content: "y",
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
                query: "",
                scope: Some(Scope::Personal),
                owner_id: Some("u1"),
                limit: 10,
            },
        )
        .await
        .unwrap();
        assert_eq!(res.len(), 0, "empty query must produce no rows, not error");
    }

    #[tokio::test]
    async fn multi_word_query_uses_and_of_terms_semantics() {
        // raw_search の term-AND 意味を維持する回帰: `foo bar` で `foo baz bar` を
        // 含む doc が hit する (token ごとに phrase 化 + 空白 join で FTS5 implicit AND)。
        // 1 つの phrase で wrap すると adjacency 必須になり hit しなくなる。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "doc with foo baz bar",
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
                query: "foo bar",
                scope: Some(Scope::Personal),
                owner_id: Some("u1"),
                limit: 10,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            res.len(),
            1,
            "multi-word query must AND terms, not require adjacency"
        );
    }

    #[tokio::test]
    async fn query_with_operator_keyword_is_literal() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "rules and exceptions",
                content: "x",
                source: "m",
                tags_json: None,
                created_by: Some("u1"),
            },
        )
        .await
        .unwrap();
        // 'AND' を演算子としてではなく literal phrase として扱う
        let res = raws(
            &pool,
            SearchQuery {
                query: "rules AND exceptions",
                scope: Some(Scope::Personal),
                owner_id: Some("u1"),
                limit: 10,
            },
        )
        .await;
        assert!(res.is_ok());
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
