use std::sync::Arc;

use llm_memory_core::scope::OwnerKey;
use llm_memory_core::time::now_ms;
use llm_memory_llm::client::LlmClient;
use llm_memory_llm::haiku::HaikuExtractor;
use llm_memory_llm::sonnet::{SonnetSynthesizer, SynthInput};
use llm_memory_storage::{raws, wikis};
use sqlx::SqlitePool;
use tracing::{error, info, warn};

use crate::error::CoordinatorError;
use crate::input_builder;
use crate::metrics::MetricsSink;
use crate::state::{RebuildMode, StateMap};

pub const MAX_ITERATIONS: usize = 10;
pub const CONCEPT_LIMIT_PER_OWNER: i64 = 200;
pub const CONCEPT_CONCURRENCY: usize = 4;

pub struct WorkerDeps {
    pub pool: SqlitePool,
    pub state: StateMap,
    pub llm: Arc<dyn LlmClient>,
    pub model_extract: String,
    pub model_synth: String,
    /// 外部メトリクス層への薄い抽象。テストでは `NoopMetricsSink` を渡す。
    pub metrics: Arc<dyn MetricsSink>,
}

/// Spawn a rebuild worker. Uses an outer tokio task to capture panics via JoinHandle::await
/// (§7.3 of the spec).
///
/// 重要: `force_idle` は inner が panic / Err で異常終了したときのみ呼ぶ。
/// 正常終了 (Ok(Ok(()))) では `run_worker` 内の `mark_idle_or_continue` で既に
/// running フラグが下りており、その直後に新規 append が来て新 worker が起動した場合、
/// ここで force_idle を呼ぶと「新 worker の running フラグまで巻き戻して clear」してしまい
/// owner ごとに複数 worker が並走する race が発生する。
pub fn spawn_worker(deps: Arc<WorkerDeps>, key: OwnerKey, initial_mode: RebuildMode) {
    let deps_outer = deps.clone();
    let key_outer = key.clone();
    deps_outer.metrics.rebuild_in_flight_inc();

    tokio::spawn(async move {
        // Spawn the actual work as a child task so we can await its JoinHandle and catch panics.
        let inner = tokio::spawn({
            let deps = deps_outer.clone();
            let key = key_outer.clone();
            async move { run_worker(deps, key, initial_mode).await }
        });

        let join_result = inner.await;
        deps_outer.metrics.rebuild_in_flight_dec();

        match join_result {
            Ok(Ok(())) => {
                // 正常終了: run_worker 内で mark_idle_or_continue 済み。
                // ここで force_idle を呼ぶと後続 worker の状態を破壊するので何もしない。
            }
            Ok(Err(e)) => {
                // 一過性 (LLM / DB) エラー: state は信頼できる。running フラグだけ
                // 下ろして `manual_pending` / `append_missed` を温存し、次の
                // notify / manual rebuild トリガで再 spawn される worker が
                // 続きを引き継げるようにする。
                error!(owner = ?key_outer, error = ?e, "rebuild worker returned error");
                deps_outer.metrics.inc_rebuild_failed();
                deps_outer
                    .state
                    .release_running_preserve_pending(&key_outer)
                    .await;
            }
            Err(join_err) if join_err.is_panic() => {
                // panic: state が信用できないので全クリア (force_idle)。
                error!(owner = ?key_outer, ?join_err, "rebuild worker panicked");
                deps_outer.metrics.inc_rebuild_failed();
                deps_outer.state.force_idle(&key_outer).await;
            }
            Err(e) => {
                // join error (cancellation 等): state が信用できないので全クリア。
                error!(owner = ?key_outer, error = ?e, "rebuild worker join error");
                deps_outer.metrics.inc_rebuild_failed();
                deps_outer.state.force_idle(&key_outer).await;
            }
        }
    });
}

/// Outer loop: run a session, then check manual_pending to decide whether to continue.
/// state.running is managed by the StateMap, not by us directly (except via force_idle in the outer wrapper).
///
/// drain cap (MAX_ITERATIONS 到達) で `SessionOutcome::DrainCapped` が返ってきた場合は、
/// `mark_idle_or_continue` を経由せずに即座に新規 Append session を起動して残 raw を捌く。
/// これにより「次の append 通知が来るまで残 raw が放置される」問題を防ぐ。
///
/// Race 対策: worker 実行中に届いた `try_start(Append)` は state map 内で `append_missed=true`
/// を立て、`mark_idle_or_continue` がそのフラグを mutex 内で原子的に拾い上げて
/// `Some(Append)` を返す。これにより running=true の窓で届いた append 通知が取りこぼされず、
/// MAX_CONSECUTIVE_CAPS 到達時の解放直前の race も含めてカバーされる。
///
/// Accepted risk: 連続 cap が `MAX_CONSECUTIVE_CAPS` を超えた場合、その時点で append_missed も
/// manual_pending も無ければ worker を解放する。極端な負荷で raw が積まれ続けたまま誰も
/// notify_append を呼ばない異常系では残 raw が次の append/manual rebuild まで放置される
/// 可能性があるが、これは spec § 7.3 の安全停止要件と整合。運用側はメトリクス
/// `rebuild_drain_capped` のアラート設定と manual rebuild 経路でフォローする。
async fn run_worker(
    deps: Arc<WorkerDeps>,
    key: OwnerKey,
    initial_mode: RebuildMode,
) -> Result<(), CoordinatorError> {
    let mut next_mode: Option<RebuildMode> = Some(initial_mode);
    let mut consecutive_caps: usize = 0;
    // 残 raw を保ったまま mark_idle_or_continue に進むと、`manual_pending` =
    // `Manual{Some(c)}` が pending している場合に c だけが last_rebuilt_at を
    // started_at に進めて owner 全体の watermark を Advance してしまい、
    // c 以外の concept 向けの残 raw が永久に list_since 範囲外になる
    // (= per-owner MAX watermark architecture の制約)。
    // 対策として、cap streak が limit に達した直後に残 raw が観測されたら、
    // pending に譲る前に 1 回だけ Append を強制して全 concept を Haiku で
    // 走査させる。1 回までに制限することで worker の寿命を有界に保つ。
    let mut remainder_drain_used = false;
    while let Some(mode) = next_mode.take() {
        let outcome = run_session(&deps, &key, mode).await?;
        match outcome {
            SessionOutcome::Done => {
                consecutive_caps = 0;
                next_mode = deps.state.mark_idle_or_continue(&key).await;
            }
            SessionOutcome::DrainCapped => {
                consecutive_caps += 1;
                if consecutive_caps >= MAX_CONSECUTIVE_CAPS {
                    // 残 raw 数を取得 (アラート + drain-before-yield 判定)。
                    let watermark =
                        wikis::max_last_rebuilt_at(&deps.pool, key.scope, &key.owner_id).await?;
                    let lookup_result =
                        raws::list_since(&deps.pool, key.scope, &key.owner_id, watermark, now_ms())
                            .await;
                    let remaining_count: Option<usize> = match &lookup_result {
                        Ok(remaining) => {
                            error!(
                                owner = ?key,
                                consecutive_caps,
                                remaining_raws = remaining.len(),
                                "drain cap streak exceeded MAX_CONSECUTIVE_CAPS; \
                                 remaining raws will be picked up by `append_missed` race-recovery \
                                 or next append/manual rebuild. \
                                 Operator action: check `rebuild_drain_capped` metric and consider manual rebuild."
                            );
                            Some(remaining.len())
                        }
                        Err(e) => {
                            error!(
                                owner = ?key,
                                consecutive_caps,
                                error = ?e,
                                "drain cap streak exceeded MAX_CONSECUTIVE_CAPS; \
                                 remaining_raws lookup failed. \
                                 Operator action: check `rebuild_drain_capped` metric and consider manual rebuild."
                            );
                            None
                        }
                    };

                    // lookup 成功で `> 0` か、lookup 失敗 (None) なら fail-safe で
                    // 残 raw 有り扱い。後者は manual_pending に譲って watermark を進めて
                    // 取りこぼすリスクを回避するための保守的挙動。
                    let remainder_present = remaining_count.map(|n| n > 0).unwrap_or(true);
                    if remainder_present && !remainder_drain_used {
                        // 残 raw がある状態で pending Manual{Some(c)} に譲ると
                        // c の synthesize で watermark が進んで他 concept 向け raw を
                        // 取りこぼすので、Append を 1 回だけ強制して全 concept を
                        // Haiku 経由で走査させる。worker 寿命を有界化するため
                        // remainder_drain_used で 1 回に制限。
                        remainder_drain_used = true;
                        consecutive_caps = 0;
                        warn!(
                            owner = ?key,
                            lookup_failed = remaining_count.is_none(),
                            "forcing extra Append session to drain remainder before yielding to pending"
                        );
                        next_mode = Some(RebuildMode::Append);
                    } else {
                        // 残 raw なし、または既に追加 Append を試行済み → 通常通り
                        // mark_idle_or_continue で append_missed / manual_pending を消費。
                        next_mode = deps.state.mark_idle_or_continue(&key).await;
                    }
                } else {
                    // mark_idle_or_continue を経由しないので running=true は維持される。
                    // 既存 manual_pending は次 Done セッションで評価される。
                    next_mode = Some(RebuildMode::Append);
                }
            }
        }
    }
    Ok(())
}

/// run_session の終了種別。drain cap 時は呼び元 (run_worker) が即座に
/// 次 session を Append で起こすことで残 raw を漏れなく処理させる。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionOutcome {
    Done,
    DrainCapped,
}

/// drain cap 後の連続継続セッションの上限。raw が無限に積まれ続ける異常系で
/// worker が永久に走り続けるのを防ぐ安全策。
pub const MAX_CONSECUTIVE_CAPS: usize = 3;

/// One session = up to MAX_ITERATIONS drain iterations.
/// First iteration uses `starting_mode`; subsequent iterations are Append (drain).
///
/// 戻り値:
/// - `SessionOutcome::Done` — drain 完了 or affected 空 or budget reject 等で session 終了
/// - `SessionOutcome::DrainCapped` — MAX_ITERATIONS に到達 (呼び元が即座に Append で再起動する)
pub(crate) async fn run_session(
    deps: &WorkerDeps,
    key: &OwnerKey,
    starting_mode: RebuildMode,
) -> Result<SessionOutcome, CoordinatorError> {
    let mut mode = starting_mode;
    for iteration in 1..=MAX_ITERATIONS {
        let started_at = now_ms();
        let watermark = wikis::max_last_rebuilt_at(&deps.pool, key.scope, &key.owner_id).await?;
        let new_raws =
            raws::list_since(&deps.pool, key.scope, &key.owner_id, watermark, started_at).await?;
        let existing_concepts = wikis::list_concepts(&deps.pool, key.scope, &key.owner_id).await?;

        let affected: Vec<String> = match &mode {
            RebuildMode::Append => {
                if new_raws.is_empty() {
                    info!(owner = ?key, "drain complete (no new raws)");
                    deps.metrics.observe_drain_iterations(iteration as u64);
                    return Ok(SessionOutcome::Done);
                }
                let extractor = HaikuExtractor {
                    client: deps.llm.as_ref(),
                    model: deps.model_extract.clone(),
                };
                let titles_contents: Vec<(&str, &str)> = new_raws
                    .iter()
                    .map(|r| (r.title.as_str(), r.content.as_str()))
                    .collect();
                let extracted = extractor
                    .extract(&titles_contents, &existing_concepts)
                    .await
                    .map_err(|e| {
                        // LLM provider の HTTP/quota error は session 全体を落とす
                        // 経路でも `llm_api_error_total` に計上する。
                        if matches!(e, llm_memory_llm::error::LlmError::Api { .. }) {
                            deps.metrics.inc_llm_api_error();
                        }
                        e
                    })?;
                // 安全対策: Haiku が `affected_existing` に existing でない concept を入れて
                // 返してきても、それは無視する (そうしないと set 経由で Sonnet に投げられ
                // CONCEPT_LIMIT_PER_OWNER を bypass して wiki が新規作成されてしまう)。
                // 加えて LLM 出力は trust boundary 外なので、2-64 chars / lowercase+hyphen の
                // concept 名規約を満たさないものは upsert 前に drop し warn で観測する。
                let existing_set: std::collections::HashSet<&String> =
                    existing_concepts.iter().collect();
                let mut set: std::collections::BTreeSet<String> = extracted
                    .affected_existing
                    .into_iter()
                    .filter(|c| {
                        if !llm_memory_core::concept::is_valid(c) {
                            warn!(owner = ?key, concept = %c, "drop invalid affected_existing");
                            return false;
                        }
                        true
                    })
                    .filter(|c| existing_set.contains(c))
                    .collect();
                let current_count =
                    wikis::count_concepts(&deps.pool, key.scope, &key.owner_id).await?;
                // 残り枠だけ追加。current_count=199, new=100 でも 200 までで止める。
                let remaining = (CONCEPT_LIMIT_PER_OWNER - current_count).max(0) as usize;
                // budget の判定は validation 後の件数で行う。raw 件数で warn すると
                // Haiku が invalid 大量返答した時に「truncated」と誤って警告してしまう。
                let valid_new: Vec<String> = extracted
                    .new_concepts
                    .into_iter()
                    .filter(|c| {
                        if !llm_memory_core::concept::is_valid(c) {
                            warn!(owner = ?key, concept = %c, "drop invalid new_concept");
                            return false;
                        }
                        true
                    })
                    .collect();
                let new_total = valid_new.len();
                if remaining == 0 && new_total > 0 {
                    warn!(owner = ?key, current_count, "concept limit reached, ignoring new_concepts");
                } else if new_total > remaining {
                    warn!(
                        owner = ?key,
                        current_count,
                        new_total,
                        remaining,
                        "concept limit approached, truncated new_concepts"
                    );
                }
                // 既存 concept と衝突する場合は set に入れるだけで新規 count を消費しない。
                for c in valid_new.into_iter().take(remaining) {
                    set.insert(c);
                }
                set.into_iter().collect()
            }
            RebuildMode::Manual { concept: Some(c) } => {
                // 既存 concept の rebuild は無条件 OK。新規 concept を Manual で
                // 作る場合だけ CONCEPT_LIMIT_PER_OWNER をチェックして budget bypass を防ぐ。
                let is_existing = existing_concepts.iter().any(|x| x == c);
                if !is_existing {
                    let current_count =
                        wikis::count_concepts(&deps.pool, key.scope, &key.owner_id).await?;
                    if current_count >= CONCEPT_LIMIT_PER_OWNER {
                        warn!(
                            owner = ?key,
                            concept = %c,
                            current_count,
                            "manual rebuild of new concept rejected: concept limit reached"
                        );
                        // budget reject: この concept は諦めるが、同 owner の未処理 raw を
                        // 取りこぼさないよう Append drain に遷移して継続する。
                        mode = RebuildMode::Append;
                        continue;
                    }
                }
                vec![c.clone()]
            }
            RebuildMode::Manual { concept: None } => existing_concepts.clone(),
        };

        if affected.is_empty() {
            info!(owner = ?key, "no affected concepts, ending session");
            deps.metrics.observe_drain_iterations(iteration as u64);
            return Ok(SessionOutcome::Done);
        }

        synthesize_concepts(deps, key, &affected, &new_raws, started_at).await?;

        // Subsequent iterations are pure Append (drain).
        mode = RebuildMode::Append;

        if iteration == MAX_ITERATIONS {
            warn!(
                owner = ?key,
                "drain loop hit MAX_ITERATIONS, deferring remainder to next Append session"
            );
            deps.metrics.inc_rebuild_drain_capped();
            deps.metrics.observe_drain_iterations(iteration as u64);
            // run_worker が SessionOutcome::DrainCapped を見て即座に新規 Append session を
            // 起動する。Manual{None} を pending に積まないので、残 raw に潜む新規 concept を
            // Haiku で抽出する経路が維持される & 既存の manual_pending (user intent) も
            // 破壊しない。
            return Ok(SessionOutcome::DrainCapped);
        }
    }
    Ok(SessionOutcome::Done)
}

async fn synthesize_concepts(
    deps: &WorkerDeps,
    key: &OwnerKey,
    affected: &[String],
    new_raws: &[raws::Raw],
    started_at: i64,
) -> Result<(), CoordinatorError> {
    use futures::stream::{self, StreamExt};
    let key_for_stream = key.clone();
    // concept ごとに new_raws を clone すると 50 raws × 100 concept = 5000 clone と
    // メモリ過多。Arc 共有して各タスクは参照だけ持つ。
    let new_raws_arc: Arc<Vec<raws::Raw>> = Arc::new(new_raws.to_vec());
    stream::iter(affected.iter().cloned())
        .for_each_concurrent(CONCEPT_CONCURRENCY, |concept| {
            let key = key_for_stream.clone();
            let new_raws_ref = new_raws_arc.clone();
            async move {
                if let Err(e) =
                    synthesize_one(deps, &key, &concept, &new_raws_ref, started_at).await
                {
                    error!(owner = ?key, %concept, error = ?e, "synthesize_one failed");
                    deps.metrics.inc_concept_rebuild_failed();
                    // LLM provider の HTTP / quota error (LlmError::Api) のみを
                    // 専用カウンタに記録。Parse / Reqwest / Json は API 障害では
                    // ないので除外 (`inc_concept_rebuild_failed` だけにする)。
                    if matches!(
                        e,
                        CoordinatorError::Llm(llm_memory_llm::error::LlmError::Api { .. })
                    ) {
                        deps.metrics.inc_llm_api_error();
                    }
                }
            }
        })
        .await;
    Ok(())
}

async fn synthesize_one(
    deps: &WorkerDeps,
    key: &OwnerKey,
    concept: &str,
    new_raws: &[raws::Raw],
    started_at: i64,
) -> Result<(), CoordinatorError> {
    let existing_wiki = wikis::get(&deps.pool, key.scope, &key.owner_id, concept).await?;
    let existing_refs: Vec<String> = existing_wiki
        .as_ref()
        .and_then(|w| serde_json::from_str(&w.source_refs).ok())
        .unwrap_or_default();
    let inputs = input_builder::build(
        &deps.pool,
        key.scope,
        &key.owner_id,
        concept,
        &existing_refs,
        new_raws,
    )
    .await?;

    let synth = SonnetSynthesizer {
        client: deps.llm.as_ref(),
        model: deps.model_synth.clone(),
    };
    let raws_tuple: Vec<(String, String, String)> = inputs
        .iter()
        .map(|r| (r.id.clone(), r.title.clone(), r.content.clone()))
        .collect();
    let result = synth
        .synthesize(SynthInput {
            concept,
            existing_wiki: existing_wiki.as_ref().map(|w| w.content.as_str()),
            raws: &raws_tuple,
        })
        .await?;

    let valid_ids: std::collections::HashSet<&String> = inputs.iter().map(|r| &r.id).collect();
    let filtered_refs: Vec<String> = result
        .source_refs
        .into_iter()
        .filter(|id| valid_ids.contains(id))
        .collect();
    let refs_json = serde_json::to_string(&filtered_refs)
        .map_err(|e| CoordinatorError::Storage(llm_memory_storage::error::StorageError::Json(e)))?;

    wikis::upsert(
        &deps.pool,
        key.scope,
        &key.owner_id,
        concept,
        &result.content,
        &refs_json,
        started_at,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MetricsSink, NoopMetricsSink};
    use llm_memory_core::scope::Scope;
    use llm_memory_llm::mock::MockClient;
    use llm_memory_storage::pool::init_pool;
    use llm_memory_storage::raws::{NewRaw, insert};

    async fn deps(pool: SqlitePool, mock: Arc<MockClient>) -> Arc<WorkerDeps> {
        Arc::new(WorkerDeps {
            pool,
            state: StateMap::new(),
            llm: mock as Arc<dyn LlmClient>,
            model_extract: "haiku".into(),
            model_synth: "sonnet".into(),
            metrics: Arc::new(NoopMetricsSink) as Arc<dyn MetricsSink>,
        })
    }

    #[tokio::test]
    async fn append_drops_haiku_invalid_concept_names() {
        // Haiku が "INVALID" (大文字) や "x" (1 char) を new_concepts に返してきても、
        // wikis に upsert される前に core::concept::is_valid で reject される。
        // 正常 concept ("valid-concept") のみ通る。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let mock = Arc::new(MockClient::new());
        mock.push_text(
            r#"{"affected_existing":[],"new_concepts":["valid-concept","INVALID","x"]}"#,
        )
        .await;
        // synth は valid-concept 1 件分のみ要求される
        mock.push_text(r#"{"content":"wiki","source_refs":[]}"#)
            .await;
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "t",
                content: "c",
                source: "m",
                tags_json: None,
                created_by: Some("u1"),
            },
        )
        .await
        .unwrap();

        let deps = deps(pool.clone(), mock).await;
        run_session(&deps, &OwnerKey::personal("u1"), RebuildMode::Append)
            .await
            .unwrap();

        let concepts = wikis::list_concepts(&pool, Scope::Personal, "u1")
            .await
            .unwrap();
        assert!(concepts.contains(&"valid-concept".to_string()));
        assert!(
            !concepts.iter().any(|c| c == "INVALID"),
            "uppercase concept must be dropped"
        );
        assert!(
            !concepts.iter().any(|c| c == "x"),
            "1-char concept must be dropped"
        );
    }

    #[tokio::test]
    async fn append_drops_haiku_invalid_affected_existing_names() {
        // Haiku が affected_existing に "INVALID" のような形式違反を返してきても drop。
        // 既存 set に存在しない (existing_set filter で先に落ちる) 場合と独立に
        // is_valid filter が効くことを確認。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        // 既存 concept として大文字を持つ wiki は本来 upsert されないが、
        // 過去の bug で残った wiki が affected_existing に乗っても upsert は走らない
        // (is_valid で drop) ことを直接は検証できないので、ここでは
        // 「invalid 形式の affected_existing は無視され、Sonnet 呼び出しが発生しない」
        // ことを mock の captured() で検証する。
        let mock = Arc::new(MockClient::new());
        mock.push_text(r#"{"affected_existing":["BadName"],"new_concepts":[]}"#)
            .await;
        // Sonnet 呼び出しを期待しない (= push_text しない)。
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "t",
                content: "c",
                source: "m",
                tags_json: None,
                created_by: Some("u1"),
            },
        )
        .await
        .unwrap();
        let deps = deps(pool.clone(), mock.clone()).await;
        run_session(&deps, &OwnerKey::personal("u1"), RebuildMode::Append)
            .await
            .unwrap();
        // mock の captured 数は 1 (Haiku のみ; Sonnet は呼ばれていない)
        let captured = mock.captured().await;
        assert_eq!(
            captured.len(),
            1,
            "Sonnet must NOT be called when no valid concept survives validation"
        );
    }

    #[tokio::test]
    async fn append_mode_creates_wiki() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let mock = Arc::new(MockClient::new());
        mock.push_text(r#"{"affected_existing":[],"new_concepts":["vegapunk"]}"#)
            .await;
        mock.push_text(r##"{"content":"# Vegapunk","source_refs":[]}"##)
            .await;
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "v1",
                content: "graphrag",
                source: "m",
                tags_json: None,
                created_by: Some("u1"),
            },
        )
        .await
        .unwrap();

        let deps = deps(pool.clone(), mock).await;
        run_session(&deps, &OwnerKey::personal("u1"), RebuildMode::Append)
            .await
            .unwrap();

        let w = wikis::get(&pool, Scope::Personal, "u1", "vegapunk")
            .await
            .unwrap();
        assert!(w.is_some());
    }

    #[tokio::test]
    async fn manual_full_rebuilds_all_existing_concepts() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        wikis::upsert(&pool, Scope::Personal, "u1", "alpha", "old-a", "[]", 100)
            .await
            .unwrap();
        wikis::upsert(&pool, Scope::Personal, "u1", "beta", "old-b", "[]", 100)
            .await
            .unwrap();
        let mock = Arc::new(MockClient::new());
        // Manual{None}: no Haiku, two Sonnet calls. Order in BTreeSet is alpha then beta.
        mock.push_text(r#"{"content":"new-a","source_refs":[]}"#)
            .await;
        mock.push_text(r#"{"content":"new-b","source_refs":[]}"#)
            .await;

        let deps = deps(pool.clone(), mock).await;
        run_session(
            &deps,
            &OwnerKey::personal("u1"),
            RebuildMode::Manual { concept: None },
        )
        .await
        .unwrap();

        let a = wikis::get(&pool, Scope::Personal, "u1", "alpha")
            .await
            .unwrap()
            .unwrap();
        let b = wikis::get(&pool, Scope::Personal, "u1", "beta")
            .await
            .unwrap()
            .unwrap();
        assert_ne!(a.content, "old-a");
        assert_ne!(b.content, "old-b");
    }

    #[tokio::test]
    async fn manual_single_concept_skips_haiku() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        wikis::upsert(&pool, Scope::Personal, "u1", "alpha", "old", "[]", 100)
            .await
            .unwrap();
        let mock = Arc::new(MockClient::new());
        mock.push_text(r#"{"content":"new","source_refs":[]}"#)
            .await;

        let deps = deps(pool.clone(), mock.clone()).await;
        run_session(
            &deps,
            &OwnerKey::personal("u1"),
            RebuildMode::Manual {
                concept: Some("alpha".into()),
            },
        )
        .await
        .unwrap();

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

        // Set running = true to simulate try_start having claimed it
        let key = OwnerKey::personal("u1");
        deps_arc.state.try_start(&key, RebuildMode::Append).await;

        spawn_worker(deps_arc.clone(), key.clone(), RebuildMode::Append);

        // Wait for state to be released
        for _ in 0..50 {
            if !deps_arc.state.is_running(&key).await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("state still running after worker should have ended");
    }

    struct PanicClient;

    #[async_trait::async_trait]
    impl llm_memory_llm::client::LlmClient for PanicClient {
        async fn complete(
            &self,
            _req: llm_memory_llm::client::CompleteRequest,
        ) -> Result<llm_memory_llm::client::CompleteResponse, llm_memory_llm::error::LlmError>
        {
            panic!("intentional panic for test");
        }
    }

    #[tokio::test]
    async fn append_clamps_new_concepts_to_remaining_budget() {
        // 既存 CONCEPT_LIMIT_PER_OWNER - 1 個 (= 199) の wiki を投入して残り枠を 1 にする。
        // Haiku が 5 個の new_concept を返しても、Sonnet 呼び出しは 1 件分しか
        // 発生せず、最終的に CONCEPT_LIMIT_PER_OWNER を超えないこと。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        for i in 0..(CONCEPT_LIMIT_PER_OWNER - 1) {
            wikis::upsert(&pool, Scope::Personal, "u1", &format!("c{i}"), "x", "[]", 1)
                .await
                .unwrap();
        }
        let mock = Arc::new(MockClient::new());
        // Haiku: 5 個の new_concept (clamp 対象)
        mock.push_text(
            r#"{"affected_existing":[],"new_concepts":["new1","new2","new3","new4","new5"]}"#,
        )
        .await;
        // Sonnet: 残り枠 1 つだけ呼ばれる
        mock.push_text(r#"{"content":"x","source_refs":[]}"#).await;

        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "t",
                content: "c",
                source: "m",
                tags_json: None,
                created_by: Some("u1"),
            },
        )
        .await
        .unwrap();

        let deps_arc = deps(pool.clone(), mock.clone()).await;
        run_session(&deps_arc, &OwnerKey::personal("u1"), RebuildMode::Append)
            .await
            .unwrap();

        let count = wikis::count_concepts(&pool, Scope::Personal, "u1")
            .await
            .unwrap();
        assert_eq!(
            count, CONCEPT_LIMIT_PER_OWNER,
            "must not exceed CONCEPT_LIMIT_PER_OWNER"
        );
        // 呼ばれた LLM は Haiku 1 + Sonnet 1 のみ (clamp により Sonnet が 1 件)
        let cap = mock.captured().await;
        assert_eq!(cap.len(), 2, "Haiku + 1 Sonnet only");
    }

    #[tokio::test]
    async fn worker_recovers_from_real_panic() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let llm = Arc::new(PanicClient);
        let deps_arc = Arc::new(WorkerDeps {
            pool: pool.clone(),
            state: StateMap::new(),
            llm,
            model_extract: "haiku".into(),
            model_synth: "sonnet".into(),
            metrics: Arc::new(NoopMetricsSink) as Arc<dyn MetricsSink>,
        });
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u-panic",
                title: "x",
                content: "y",
                source: "m",
                tags_json: None,
                created_by: Some("u-panic"),
            },
        )
        .await
        .unwrap();

        let key = OwnerKey::personal("u-panic");
        deps_arc.state.try_start(&key, RebuildMode::Append).await;
        spawn_worker(deps_arc.clone(), key.clone(), RebuildMode::Append);

        // Wait up to ~5s for state to be released
        for _ in 0..50 {
            if !deps_arc.state.is_running(&key).await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("state still running after PanicClient should have panicked the worker");
    }

    #[tokio::test]
    async fn append_ignores_haiku_unknown_affected_existing() {
        // Haiku が `affected_existing` に existing でない concept 名を返しても、
        // それが synthesize 対象に混入しないこと (= Sonnet 呼び出しが発生しないこと)。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        // existing concept は無し (`existing_concepts` 空)
        let mock = Arc::new(MockClient::new());
        // Haiku: ありもしない "ghost" を affected_existing に入れる + new_concepts は空
        mock.push_text(r#"{"affected_existing":["ghost"],"new_concepts":[]}"#)
            .await;
        // Sonnet 用の応答は push しない。もし呼ばれたら mock がエラーを返してテストで気付ける。

        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u-ghost",
                title: "t",
                content: "c",
                source: "m",
                tags_json: None,
                created_by: Some("u-ghost"),
            },
        )
        .await
        .unwrap();

        let deps_arc = deps(pool.clone(), mock.clone()).await;
        run_session(
            &deps_arc,
            &OwnerKey::personal("u-ghost"),
            RebuildMode::Append,
        )
        .await
        .expect("Sonnet should never be called; session should end gracefully");

        // "ghost" 用 wiki は作られない
        let w = wikis::get(&pool, Scope::Personal, "u-ghost", "ghost")
            .await
            .unwrap();
        assert!(w.is_none(), "ghost concept must not be synthesized");

        // mock は Haiku の 1 回だけ呼ばれた
        let cap = mock.captured().await;
        assert_eq!(cap.len(), 1, "only Haiku should have been called");
        assert_eq!(cap[0].model, "haiku");
    }

    #[tokio::test]
    async fn manual_single_new_concept_rejected_when_limit_reached() {
        // CONCEPT_LIMIT_PER_OWNER 個まで埋めた状態で Manual{Some("brand-new")} を
        // 呼んでも、新規 concept は budget で弾かれて Sonnet が呼ばれないこと。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        for i in 0..CONCEPT_LIMIT_PER_OWNER {
            wikis::upsert(&pool, Scope::Personal, "u1", &format!("c{i}"), "x", "[]", 1)
                .await
                .unwrap();
        }
        let mock = Arc::new(MockClient::new());
        // Sonnet 応答を push しない: もし呼ばれたら LlmError で worker が Err になり気付ける。

        let deps_arc = deps(pool.clone(), mock.clone()).await;
        run_session(
            &deps_arc,
            &OwnerKey::personal("u1"),
            RebuildMode::Manual {
                concept: Some("brand-new".into()),
            },
        )
        .await
        .expect("should return Ok without invoking Sonnet when budget exhausted");

        // 新規 concept は作成されていない
        let w = wikis::get(&pool, Scope::Personal, "u1", "brand-new")
            .await
            .unwrap();
        assert!(w.is_none(), "brand-new must not be synthesized");

        // 総 concept 数は変わらず
        let count = wikis::count_concepts(&pool, Scope::Personal, "u1")
            .await
            .unwrap();
        assert_eq!(count, CONCEPT_LIMIT_PER_OWNER);

        // LLM は一度も呼ばれない (Manual{Some} は Haiku をスキップする)
        let cap = mock.captured().await;
        assert!(cap.is_empty(), "no LLM call expected");
    }

    #[tokio::test]
    async fn manual_single_new_concept_reject_falls_through_to_append_drain() {
        // Manual{Some(brand-new)} が budget で reject されたあと、同 session 内で
        // Append drain に遷移して未処理 raw を Haiku → Sonnet で処理すること。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        for i in 0..CONCEPT_LIMIT_PER_OWNER {
            wikis::upsert(&pool, Scope::Personal, "u1", &format!("c{i}"), "x", "[]", 1)
                .await
                .unwrap();
        }
        // 未処理 raw を 1 件投入
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u1",
                title: "t",
                content: "c",
                source: "m",
                tags_json: None,
                created_by: Some("u1"),
            },
        )
        .await
        .unwrap();

        let mock = Arc::new(MockClient::new());
        // Append drain 1 回目: Haiku → existing c0 を更新
        mock.push_text(r#"{"affected_existing":["c0"],"new_concepts":[]}"#)
            .await;
        // Sonnet: c0 を更新
        mock.push_text(r#"{"content":"updated","source_refs":[]}"#)
            .await;

        let deps_arc = deps(pool.clone(), mock.clone()).await;
        let outcome = run_session(
            &deps_arc,
            &OwnerKey::personal("u1"),
            RebuildMode::Manual {
                concept: Some("brand-new".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome, SessionOutcome::Done);

        // brand-new は作成されない
        let w = wikis::get(&pool, Scope::Personal, "u1", "brand-new")
            .await
            .unwrap();
        assert!(w.is_none());
        // c0 は drain で更新された
        let c0 = wikis::get(&pool, Scope::Personal, "u1", "c0")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(c0.content, "updated");
        // Haiku + Sonnet が 1 回ずつ呼ばれている
        assert_eq!(mock.captured().await.len(), 2);
    }

    #[tokio::test]
    async fn manual_single_existing_concept_proceeds_at_limit() {
        // CONCEPT_LIMIT_PER_OWNER 個埋まっていても、既存 concept の rebuild は通る。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        for i in 0..CONCEPT_LIMIT_PER_OWNER {
            wikis::upsert(
                &pool,
                Scope::Personal,
                "u1",
                &format!("c{i}"),
                "old",
                "[]",
                1,
            )
            .await
            .unwrap();
        }
        let mock = Arc::new(MockClient::new());
        mock.push_text(r#"{"content":"new","source_refs":[]}"#)
            .await;

        let deps_arc = deps(pool.clone(), mock.clone()).await;
        run_session(
            &deps_arc,
            &OwnerKey::personal("u1"),
            RebuildMode::Manual {
                concept: Some("c0".into()),
            },
        )
        .await
        .unwrap();

        // 既存 concept "c0" は更新されている
        let w = wikis::get(&pool, Scope::Personal, "u1", "c0")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(w.content, "new");
    }

    #[tokio::test]
    async fn append_missed_recovered_even_at_consecutive_caps_boundary() {
        // 安全停止分岐 (MAX_CONSECUTIVE_CAPS 到達 → mark_idle_or_continue) でも、
        // 直前に届いた append 通知が append_missed として記録されていれば、
        // mark_idle_or_continue が mutex 内で原子的に拾って Some(Append) を返すこと。
        // run_worker を直接呼ばず、state.rs の API で境界を直接検証する。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let mock = Arc::new(MockClient::new());
        let deps_arc = deps(pool.clone(), mock).await;
        let key = OwnerKey::personal("u-boundary");

        // worker A running 中の状態を作る
        deps_arc.state.try_start(&key, RebuildMode::Append).await;
        // remaining_raws lookup している瞬間に append 通知が来た想定
        let r = deps_arc.state.try_start(&key, RebuildMode::Append).await;
        assert_eq!(r, crate::state::StartOutcome::AlreadyRunning);

        // この時点で run_worker の DrainCapped 安全停止分岐が
        // mark_idle_or_continue を呼ぶのと同じシーケンスを直接踏む
        let cont = deps_arc.state.mark_idle_or_continue(&key).await;
        assert_eq!(
            cont,
            Some(RebuildMode::Append),
            "append_missed must be recovered atomically even at MAX_CONSECUTIVE_CAPS boundary"
        );
        // running は維持され (Some を返したので)、append が取りこぼされていない
        assert!(deps_arc.state.is_running(&key).await);
    }

    #[tokio::test]
    async fn append_missed_is_recovered_after_session_done() {
        // worker A 実行中に届いた append 通知 (= AlreadyRunning) は append_missed として
        // 記録され、session 終了時の mark_idle_or_continue で Some(Append) として
        // 取り出されて drain が継続すること。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let mock = Arc::new(MockClient::new());
        // session 1: 空 extraction → Done
        mock.push_text(r#"{"affected_existing":[],"new_concepts":[]}"#)
            .await;
        // session 2 (append_missed 経由で起動): 空 extraction → Done
        mock.push_text(r#"{"affected_existing":[],"new_concepts":[]}"#)
            .await;

        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u-missed",
                title: "t",
                content: "c",
                source: "m",
                tags_json: None,
                created_by: Some("u-missed"),
            },
        )
        .await
        .unwrap();

        let deps_arc = deps(pool.clone(), mock.clone()).await;
        let key = OwnerKey::personal("u-missed");
        // worker A claim
        deps_arc.state.try_start(&key, RebuildMode::Append).await;
        // running 中に append 通知が落ちる → append_missed = true
        let r = deps_arc.state.try_start(&key, RebuildMode::Append).await;
        assert_eq!(r, crate::state::StartOutcome::AlreadyRunning);

        // run_worker を起動 (run_worker 内の while ループが mark_idle_or_continue で
        // append_missed を拾って session 2 を実行する)
        spawn_worker(deps_arc.clone(), key.clone(), RebuildMode::Append);

        // 両 session 完了で running=false に戻る
        let mut ended = false;
        for _ in 0..100 {
            if !deps_arc.state.is_running(&key).await {
                ended = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            ended,
            "worker should release running after both sessions complete"
        );

        // Haiku が 2 回呼ばれたはず (session 1 と append_missed-recovery session 2)
        let cap = mock.captured().await;
        assert_eq!(cap.len(), 2, "two haiku calls expected (one per session)");
    }

    #[tokio::test]
    async fn append_session_returns_done_when_no_raws() {
        // SessionOutcome::Done を返す基本パス: new_raws 空 → drain complete。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let mock = Arc::new(MockClient::new());
        let deps_arc = deps(pool.clone(), mock).await;
        let outcome = run_session(&deps_arc, &OwnerKey::personal("u1"), RebuildMode::Append)
            .await
            .unwrap();
        assert_eq!(outcome, SessionOutcome::Done);
    }

    #[tokio::test]
    async fn manual_full_session_returns_done() {
        // Manual{None} で 0 existing concept のパス → affected 空で Done を返すこと。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let mock = Arc::new(MockClient::new());
        let deps_arc = deps(pool.clone(), mock).await;
        let outcome = run_session(
            &deps_arc,
            &OwnerKey::personal("u1"),
            RebuildMode::Manual { concept: None },
        )
        .await
        .unwrap();
        assert_eq!(outcome, SessionOutcome::Done);
    }

    #[tokio::test]
    async fn normal_exit_does_not_clobber_subsequent_workers_state() {
        // 1) worker A 起動 → 正常終了 (Haiku が空応答 → drain complete)
        // 2) outer wrapper の await が終わった後も、後続の worker B の state を
        //    破壊しないこと (= 正常終了パスで force_idle が呼ばれていないこと) を検証。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let mock = Arc::new(MockClient::new());
        // worker A 用: 空 extraction → drain complete
        mock.push_text(r#"{"affected_existing":[],"new_concepts":[]}"#)
            .await;

        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u-race",
                title: "x",
                content: "y",
                source: "m",
                tags_json: None,
                created_by: Some("u-race"),
            },
        )
        .await
        .unwrap();

        let deps_arc = deps(pool.clone(), mock).await;
        let key = OwnerKey::personal("u-race");
        deps_arc.state.try_start(&key, RebuildMode::Append).await;
        spawn_worker(deps_arc.clone(), key.clone(), RebuildMode::Append);

        // A が正常終了して running=false に戻ること
        let mut ended = false;
        for _ in 0..50 {
            if !deps_arc.state.is_running(&key).await {
                ended = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(ended, "worker A should release running flag normally");

        // 別経路で worker B 相当の状態を作る (try_start で running=true)
        let started = deps_arc
            .state
            .try_start(&key, RebuildMode::Manual { concept: None })
            .await;
        assert!(matches!(started, crate::state::StartOutcome::Started(_)));
        assert!(deps_arc.state.is_running(&key).await, "B should be running");

        // 少し待って、A の outer wrapper が遅延 force_idle を呼んでいないことを確認。
        // (正常終了パスで force_idle を消したので、ここで running が落ちないはず)
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            deps_arc.state.is_running(&key).await,
            "A's outer wrapper must not clobber B's running flag"
        );
    }

    #[tokio::test]
    async fn worker_inner_error_preserves_pending() {
        // 一過性 (LLM) エラーで worker が Err(_) を返した場合、
        // 並行して届いていた manual_pending / append_missed が消えずに
        // 次の worker spawn まで温存されること (release_running_preserve_pending 経由)。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let mock = Arc::new(MockClient::new());
        // mock 応答を push しない → 最初の Haiku で LlmError::Api
        let deps_arc = deps(pool.clone(), mock).await;
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u-err",
                title: "x",
                content: "y",
                source: "m",
                tags_json: None,
                created_by: Some("u-err"),
            },
        )
        .await
        .unwrap();

        let key = OwnerKey::personal("u-err");
        // worker 起動準備: running=true、その上で manual_pending を投入
        deps_arc.state.try_start(&key, RebuildMode::Append).await;
        deps_arc
            .state
            .try_start(
                &key,
                RebuildMode::Manual {
                    concept: Some("alpha".into()),
                },
            )
            .await;
        // append 通知が落とされた状態も再現
        deps_arc.state.try_start(&key, RebuildMode::Append).await;

        spawn_worker(deps_arc.clone(), key.clone(), RebuildMode::Append);

        // worker が Err で終わって running=false になるまで待つ
        let mut ended = false;
        for _ in 0..50 {
            if !deps_arc.state.is_running(&key).await {
                ended = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(ended, "worker should release running after inner error");

        // append_missed と manual_pending が残っているはず (Round 3 priority で
        // append_missed 先取り)
        let cont = deps_arc.state.mark_idle_or_continue(&key).await;
        assert_eq!(
            cont,
            Some(RebuildMode::Append),
            "append_missed must survive recoverable error and consume first"
        );
        let cont2 = deps_arc.state.mark_idle_or_continue(&key).await;
        assert_eq!(
            cont2,
            Some(RebuildMode::Manual {
                concept: Some("alpha".into())
            }),
            "manual_pending must survive recoverable error"
        );
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingMetrics {
        concept_failed: AtomicUsize,
        llm_api_failed: AtomicUsize,
    }
    impl MetricsSink for CountingMetrics {
        fn inc_rebuild_failed(&self) {}
        fn inc_concept_rebuild_failed(&self) {
            self.concept_failed.fetch_add(1, Ordering::Relaxed);
        }
        fn inc_rebuild_drain_capped(&self) {}
        fn observe_drain_iterations(&self, _: u64) {}
        fn rebuild_in_flight_inc(&self) {}
        fn rebuild_in_flight_dec(&self) {}
        fn inc_llm_api_error(&self) {
            self.llm_api_failed.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn counting_metrics() -> Arc<CountingMetrics> {
        Arc::new(CountingMetrics {
            concept_failed: AtomicUsize::new(0),
            llm_api_failed: AtomicUsize::new(0),
        })
    }

    #[tokio::test]
    async fn concept_failure_increments_metric() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        wikis::upsert(&pool, Scope::Personal, "u1", "alpha", "old", "[]", 1)
            .await
            .unwrap();
        let mock = Arc::new(MockClient::new());
        // Manual{Some("alpha")} は Haiku 抽出をスキップして直接 Sonnet を呼ぶ。
        // Sonnet 側の呼び出しを Api エラーで失敗させる。
        mock.push_error("synthesize failed").await;

        let metrics = counting_metrics();
        let deps_arc = Arc::new(WorkerDeps {
            pool: pool.clone(),
            state: StateMap::new(),
            llm: mock,
            model_extract: "h".into(),
            model_synth: "s".into(),
            metrics: metrics.clone() as Arc<dyn MetricsSink>,
        });
        run_session(
            &deps_arc,
            &OwnerKey::personal("u1"),
            RebuildMode::Manual {
                concept: Some("alpha".into()),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            metrics.concept_failed.load(Ordering::Relaxed),
            1,
            "concept failure counter must increment"
        );
        assert_eq!(
            metrics.llm_api_failed.load(Ordering::Relaxed),
            1,
            "LlmError::Api on synthesize must also count llm_api_error"
        );
    }

    #[tokio::test]
    async fn extract_llm_api_error_increments_llm_api_metric() {
        // Haiku 抽出 (extract) で LlmError::Api が起きた場合、session 全体が
        // 早期 Err になるが、`inc_llm_api_error` は呼ばれる必要がある (worker.rs
        // 244 の map_err 経路)。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(
            &pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: "u-extract-fail",
                title: "t",
                content: "c",
                source: "m",
                tags_json: None,
                created_by: Some("u-extract-fail"),
            },
        )
        .await
        .unwrap();

        let mock = Arc::new(MockClient::new());
        // mock の push_error は LlmError::Api を返す
        mock.push_error("extract failed").await;

        let metrics = counting_metrics();
        let deps_arc = Arc::new(WorkerDeps {
            pool: pool.clone(),
            state: StateMap::new(),
            llm: mock,
            model_extract: "h".into(),
            model_synth: "s".into(),
            metrics: metrics.clone() as Arc<dyn MetricsSink>,
        });
        let result = run_session(
            &deps_arc,
            &OwnerKey::personal("u-extract-fail"),
            RebuildMode::Append,
        )
        .await;
        assert!(result.is_err(), "extract failure should propagate as Err");
        assert_eq!(
            metrics.llm_api_failed.load(Ordering::Relaxed),
            1,
            "LlmError::Api on extract must count llm_api_error"
        );
    }
}
