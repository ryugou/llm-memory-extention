use std::sync::Arc;

use llm_memory_core::scope::OwnerKey;
use llm_memory_core::time::now_ms;
use llm_memory_llm::client::AnthropicClient;
use llm_memory_llm::haiku::HaikuExtractor;
use llm_memory_llm::sonnet::{SonnetSynthesizer, SynthInput};
use llm_memory_storage::{raws, wikis};
use sqlx::SqlitePool;
use tracing::{error, info, warn};

use crate::error::CoordinatorError;
use crate::input_builder;
use crate::state::{RebuildMode, StateMap};

pub const MAX_ITERATIONS: usize = 10;
pub const CONCEPT_LIMIT_PER_OWNER: i64 = 200;
pub const CONCEPT_CONCURRENCY: usize = 4;

pub struct WorkerDeps<C: AnthropicClient + 'static> {
    pub pool: SqlitePool,
    pub state: StateMap,
    pub llm: Arc<C>,
    pub model_haiku: String,
    pub model_sonnet: String,
}

/// Spawn a rebuild worker. Uses an outer tokio task to capture panics via JoinHandle::await
/// (§7.3 of the spec). Always restores state.running = false on completion, error, or panic.
pub fn spawn_worker<C: AnthropicClient + 'static>(
    deps: Arc<WorkerDeps<C>>,
    key: OwnerKey,
    initial_mode: RebuildMode,
) {
    let deps_outer = deps.clone();
    let key_outer = key.clone();

    tokio::spawn(async move {
        // Spawn the actual work as a child task so we can await its JoinHandle and catch panics.
        let inner = tokio::spawn({
            let deps = deps_outer.clone();
            let key = key_outer.clone();
            async move { run_worker(deps, key, initial_mode).await }
        });

        match inner.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!(owner = ?key_outer, error = ?e, "rebuild worker returned error"),
            Err(join_err) if join_err.is_panic() => {
                error!(owner = ?key_outer, ?join_err, "rebuild worker panicked");
            }
            Err(e) => error!(owner = ?key_outer, error = ?e, "rebuild worker join error"),
        }

        // Always release the slot. force_idle is idempotent.
        deps_outer.state.force_idle(&key_outer).await;
    });
}

/// Outer loop: run a session, then check manual_pending to decide whether to continue.
/// state.running is managed by the StateMap, not by us directly (except via force_idle in the outer wrapper).
async fn run_worker<C: AnthropicClient + 'static>(
    deps: Arc<WorkerDeps<C>>,
    key: OwnerKey,
    initial_mode: RebuildMode,
) -> Result<(), CoordinatorError> {
    let mut next_mode: Option<RebuildMode> = Some(initial_mode);
    while let Some(mode) = next_mode.take() {
        run_session(&deps, &key, mode).await?;
        next_mode = deps.state.mark_idle_or_continue(&key).await;
    }
    Ok(())
}

/// One session = up to MAX_ITERATIONS drain iterations.
/// First iteration uses `starting_mode`; subsequent iterations are Append (drain).
pub(crate) async fn run_session<C: AnthropicClient>(
    deps: &WorkerDeps<C>,
    key: &OwnerKey,
    starting_mode: RebuildMode,
) -> Result<(), CoordinatorError> {
    let mut mode = starting_mode;
    for iteration in 1..=MAX_ITERATIONS {
        let started_at = now_ms();
        let watermark = wikis::max_last_rebuilt_at(&deps.pool, key.scope, &key.owner_id).await?;
        let new_raws = raws::list_since(&deps.pool, key.scope, &key.owner_id, watermark, started_at).await?;
        let existing_concepts = wikis::list_concepts(&deps.pool, key.scope, &key.owner_id).await?;

        let affected: Vec<String> = match &mode {
            RebuildMode::Append => {
                if new_raws.is_empty() {
                    info!(owner = ?key, "drain complete (no new raws)");
                    return Ok(());
                }
                let extractor = HaikuExtractor { client: deps.llm.as_ref(), model: deps.model_haiku.clone() };
                let titles_contents: Vec<(&str, &str)> = new_raws.iter().map(|r| (r.title.as_str(), r.content.as_str())).collect();
                let extracted = extractor.extract(&titles_contents, &existing_concepts).await?;
                let mut set: std::collections::BTreeSet<String> = extracted.affected_existing.into_iter().collect();
                let current_count = wikis::count_concepts(&deps.pool, key.scope, &key.owner_id).await?;
                if current_count < CONCEPT_LIMIT_PER_OWNER {
                    for c in extracted.new_concepts { set.insert(c); }
                } else {
                    warn!(owner = ?key, current_count, "concept limit reached, ignoring new_concepts");
                }
                set.into_iter().collect()
            }
            RebuildMode::Manual { concept: Some(c) } => vec![c.clone()],
            RebuildMode::Manual { concept: None } => existing_concepts.clone(),
        };

        if affected.is_empty() {
            info!(owner = ?key, "no affected concepts, ending session");
            return Ok(());
        }

        synthesize_concepts(deps, key, &affected, &new_raws, started_at).await?;

        // Subsequent iterations are pure Append (drain).
        mode = RebuildMode::Append;

        if iteration == MAX_ITERATIONS {
            warn!(owner = ?key, "drain loop hit MAX_ITERATIONS, deferring remainder");
            return Ok(());
        }
    }
    Ok(())
}

async fn synthesize_concepts<C: AnthropicClient>(
    deps: &WorkerDeps<C>,
    key: &OwnerKey,
    affected: &[String],
    new_raws: &[raws::Raw],
    started_at: i64,
) -> Result<(), CoordinatorError> {
    use futures::stream::{self, StreamExt};
    let key_for_stream = key.clone();
    let new_raws_owned = new_raws.to_vec();
    stream::iter(affected.iter().cloned())
        .for_each_concurrent(CONCEPT_CONCURRENCY, |concept| {
            let key = key_for_stream.clone();
            let new_raws = new_raws_owned.clone();
            async move {
                if let Err(e) = synthesize_one(deps, &key, &concept, &new_raws, started_at).await {
                    error!(owner = ?key, %concept, error = ?e, "synthesize_one failed");
                }
            }
        })
        .await;
    Ok(())
}

async fn synthesize_one<C: AnthropicClient>(
    deps: &WorkerDeps<C>,
    key: &OwnerKey,
    concept: &str,
    new_raws: &[raws::Raw],
    started_at: i64,
) -> Result<(), CoordinatorError> {
    let existing_wiki = wikis::get(&deps.pool, key.scope, &key.owner_id, concept).await?;
    let existing_refs: Vec<String> = existing_wiki.as_ref()
        .and_then(|w| serde_json::from_str(&w.source_refs).ok())
        .unwrap_or_default();
    let inputs = input_builder::build(&deps.pool, key.scope, &key.owner_id, concept, &existing_refs, new_raws).await?;

    let synth = SonnetSynthesizer { client: deps.llm.as_ref(), model: deps.model_sonnet.clone() };
    let raws_tuple: Vec<(String, String, String)> = inputs.iter().map(|r| (r.id.clone(), r.title.clone(), r.content.clone())).collect();
    let result = synth.synthesize(SynthInput {
        concept,
        existing_wiki: existing_wiki.as_ref().map(|w| w.content.as_str()),
        raws: &raws_tuple,
    }).await?;

    let valid_ids: std::collections::HashSet<&String> = inputs.iter().map(|r| &r.id).collect();
    let filtered_refs: Vec<String> = result.source_refs.into_iter().filter(|id| valid_ids.contains(id)).collect();
    let refs_json = serde_json::to_string(&filtered_refs).map_err(|e| CoordinatorError::Storage(llm_memory_storage::error::StorageError::Json(e)))?;

    wikis::upsert(&deps.pool, key.scope, &key.owner_id, concept, &result.content, &refs_json, started_at).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_memory_core::scope::Scope;
    use llm_memory_storage::pool::init_pool;
    use llm_memory_storage::raws::{insert, NewRaw};
    use llm_memory_llm::mock::MockClient;

    async fn deps(pool: SqlitePool, mock: Arc<MockClient>) -> Arc<WorkerDeps<MockClient>> {
        Arc::new(WorkerDeps {
            pool, state: StateMap::new(), llm: mock,
            model_haiku: "haiku".into(), model_sonnet: "sonnet".into(),
        })
    }

    #[tokio::test]
    async fn append_mode_creates_wiki() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let mock = Arc::new(MockClient::new());
        mock.push_text(r#"{"affected_existing":[],"new_concepts":["vegapunk"]}"#).await;
        mock.push_text(r##"{"content":"# Vegapunk","source_refs":[]}"##).await;
        insert(&pool, NewRaw {
            scope: Scope::Personal, owner_id: "u1", title: "v1", content: "graphrag",
            source: "m", tags_json: None, created_by: Some("u1"),
        }).await.unwrap();

        let deps = deps(pool.clone(), mock).await;
        run_session(&deps, &OwnerKey::personal("u1"), RebuildMode::Append).await.unwrap();

        let w = wikis::get(&pool, Scope::Personal, "u1", "vegapunk").await.unwrap();
        assert!(w.is_some());
    }

    #[tokio::test]
    async fn manual_full_rebuilds_all_existing_concepts() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        wikis::upsert(&pool, Scope::Personal, "u1", "alpha", "old-a", "[]", 100).await.unwrap();
        wikis::upsert(&pool, Scope::Personal, "u1", "beta", "old-b", "[]", 100).await.unwrap();
        let mock = Arc::new(MockClient::new());
        // Manual{None}: no Haiku, two Sonnet calls. Order in BTreeSet is alpha then beta.
        mock.push_text(r#"{"content":"new-a","source_refs":[]}"#).await;
        mock.push_text(r#"{"content":"new-b","source_refs":[]}"#).await;

        let deps = deps(pool.clone(), mock).await;
        run_session(&deps, &OwnerKey::personal("u1"), RebuildMode::Manual { concept: None }).await.unwrap();

        let a = wikis::get(&pool, Scope::Personal, "u1", "alpha").await.unwrap().unwrap();
        let b = wikis::get(&pool, Scope::Personal, "u1", "beta").await.unwrap().unwrap();
        assert_ne!(a.content, "old-a");
        assert_ne!(b.content, "old-b");
    }

    #[tokio::test]
    async fn manual_single_concept_skips_haiku() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        wikis::upsert(&pool, Scope::Personal, "u1", "alpha", "old", "[]", 100).await.unwrap();
        let mock = Arc::new(MockClient::new());
        mock.push_text(r#"{"content":"new","source_refs":[]}"#).await;

        let deps = deps(pool.clone(), mock.clone()).await;
        run_session(&deps, &OwnerKey::personal("u1"), RebuildMode::Manual { concept: Some("alpha".into()) }).await.unwrap();

        let cap = mock.captured().await;
        assert_eq!(cap.len(), 1);
        assert_eq!(cap[0].model, "sonnet");
    }

    #[tokio::test]
    async fn worker_recovers_from_inner_error() {
        // mock に何も push しない → 最初の complete() で LlmError::Api を返す。
        // spawn_worker は inner error でも state を idle に戻すはず。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let mock = Arc::new(MockClient::new());
        let deps_arc = deps(pool.clone(), mock).await;
        insert(&pool, NewRaw {
            scope: Scope::Personal, owner_id: "u1", title: "x", content: "y",
            source: "m", tags_json: None, created_by: Some("u1"),
        }).await.unwrap();

        // Set running = true to simulate try_start having claimed it
        let key = OwnerKey::personal("u1");
        deps_arc.state.try_start(&key, RebuildMode::Append).await;

        spawn_worker(deps_arc.clone(), key.clone(), RebuildMode::Append);

        // Wait for state to be released
        for _ in 0..50 {
            if !deps_arc.state.is_running(&key).await { return; }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("state still running after worker should have ended");
    }

    struct PanicClient;

    #[async_trait::async_trait]
    impl llm_memory_llm::client::AnthropicClient for PanicClient {
        async fn complete(
            &self,
            _req: llm_memory_llm::client::CompleteRequest,
        ) -> Result<llm_memory_llm::client::CompleteResponse, llm_memory_llm::error::LlmError> {
            panic!("intentional panic for test");
        }
    }

    #[tokio::test]
    async fn worker_recovers_from_real_panic() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let llm = Arc::new(PanicClient);
        let deps_arc = Arc::new(WorkerDeps {
            pool: pool.clone(), state: StateMap::new(), llm,
            model_haiku: "haiku".into(), model_sonnet: "sonnet".into(),
        });
        insert(&pool, NewRaw {
            scope: Scope::Personal, owner_id: "u-panic", title: "x", content: "y",
            source: "m", tags_json: None, created_by: Some("u-panic"),
        }).await.unwrap();

        let key = OwnerKey::personal("u-panic");
        deps_arc.state.try_start(&key, RebuildMode::Append).await;
        spawn_worker(deps_arc.clone(), key.clone(), RebuildMode::Append);

        // Wait up to ~5s for state to be released
        for _ in 0..50 {
            if !deps_arc.state.is_running(&key).await { return; }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("state still running after PanicClient should have panicked the worker");
    }
}
