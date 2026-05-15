use llm_memory_core::scope::OwnerKey;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildMode {
    Append,
    Manual { concept: Option<String> },
}

#[derive(Debug, Default)]
pub struct RebuildState {
    pub running: bool,
    pub manual_pending: Option<RebuildMode>,
    /// running=true の最中に届いた `try_start(Append)` が `AlreadyRunning` で
    /// 取りこぼされたことを示すフラグ。worker は `mark_idle_or_continue` で
    /// このフラグを消費し、未処理 raw を取りこぼさず次セッションへ繋ぐ。
    pub append_missed: bool,
}

#[derive(Clone, Default)]
pub struct StateMap {
    inner: Arc<Mutex<HashMap<OwnerKey, RebuildState>>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StartOutcome {
    Started(RebuildMode),
    AlreadyRunning,
    Pending,
}

impl StateMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to claim a worker slot for the given owner.
    /// - If idle: 通常は running=true 化して Started(mode) を返す。
    ///   ただし `append_missed=true` で incoming が Manual の場合、Manual を
    ///   `manual_pending` に積んで Append を Started する。
    ///   これは recoverable error 後の経路 (worker が Ok(Err(_)) で終了 →
    ///   release_running_preserve_pending で running=false かつ
    ///   `append_missed=true`、`manual_pending=Some(...)` を保持した状態で
    ///   request_manual(Some(c)) が呼ばれる) で必要。append_missed を無視して
    ///   Manual を直走させると global watermark が advance して他 concept の raw
    ///   を取りこぼすため。
    /// - If running and mode is Append: sets `append_missed=true` and returns AlreadyRunning
    ///   (worker は session 終了時にこのフラグを見て drain 継続を判断する → race window で
    ///   届いた append 通知も取りこぼさない)。
    /// - If running and mode is Manual: stores in manual_pending (merge semantics) and returns Pending.
    pub async fn try_start(&self, key: &OwnerKey, mode: RebuildMode) -> StartOutcome {
        let mut map = self.inner.lock().await;
        let entry = map.entry(key.clone()).or_default();
        if entry.running {
            match &mode {
                RebuildMode::Manual { concept } => {
                    let incoming = RebuildMode::Manual {
                        concept: concept.clone(),
                    };
                    entry.manual_pending =
                        Some(merge_pending(entry.manual_pending.take(), incoming));
                    StartOutcome::Pending
                }
                RebuildMode::Append => {
                    entry.append_missed = true;
                    StartOutcome::AlreadyRunning
                }
            }
        } else {
            // idle 再開: append_missed が立っている + incoming が Manual の場合、
            // Manual を pending に積んで Append を先に走らせる (raw 取りこぼし防止)。
            let starting_mode = if entry.append_missed && matches!(mode, RebuildMode::Manual { .. })
            {
                entry.append_missed = false;
                entry.manual_pending = Some(merge_pending(entry.manual_pending.take(), mode));
                RebuildMode::Append
            } else {
                // incoming が Append なら append_missed は冗長 (どっちにせよ Append)。
                // クリアして同じ Append を Started で返す。
                if entry.append_missed && matches!(mode, RebuildMode::Append) {
                    entry.append_missed = false;
                }
                mode
            };
            entry.running = true;
            StartOutcome::Started(starting_mode)
        }
    }

    /// Called by worker after a session ends.
    /// 優先順位 (append_missed 優先):
    /// 1. `append_missed` が true なら Append で継続 (running=true 維持)
    /// 2. `manual_pending` があれば取り出して継続 (running=true 維持)
    /// 3. どちらも無ければ running=false にして worker 解放
    ///
    /// `append_missed` を `manual_pending` より先に消費する理由:
    /// `Manual{Some(c)}` セッションは c の `last_rebuilt_at` を session 開始時刻に
    /// 進めるため、`wikis::max_last_rebuilt_at` (owner 全体 MAX) が advance する。
    /// すると次の Append が `raws::list_since(watermark, ...)` で旧 watermark〜
    /// 新 watermark 間の raw を範囲外として落とし、他 concept (c2, c3, ...) に
    /// その raw が反映されない (= raw 取りこぼし)。append_missed を先に消化して
    /// 全 concept を Haiku 経由で走査することでこの window を閉じる。
    /// Manual rebuild の到着は遅延するが、user は再実行できる (再度 wiki_rebuild
    /// を呼べば次の mark_idle_or_continue で取り出される)。
    pub async fn mark_idle_or_continue(&self, key: &OwnerKey) -> Option<RebuildMode> {
        let mut map = self.inner.lock().await;
        let entry = map.entry(key.clone()).or_default();
        if entry.append_missed {
            entry.append_missed = false;
            Some(RebuildMode::Append)
        } else if let Some(m) = entry.manual_pending.take() {
            Some(m)
        } else {
            entry.running = false;
            None
        }
    }

    /// Force-release the slot (used by panic recovery in spawn wrapper).
    /// append_missed もクリアする (panic 後の状態は信用できないため zero-out)。
    pub async fn force_idle(&self, key: &OwnerKey) {
        let mut map = self.inner.lock().await;
        if let Some(entry) = map.get_mut(key) {
            entry.running = false;
            entry.manual_pending = None;
            entry.append_missed = false;
        }
    }

    /// Release the running slot but preserve `manual_pending` / `append_missed`
    /// (used for recoverable errors returned by run_worker — LLM / DB transient
    /// failures shouldn't drop user-issued manual rebuilds or notify_append).
    pub async fn release_running_preserve_pending(&self, key: &OwnerKey) {
        let mut map = self.inner.lock().await;
        if let Some(entry) = map.get_mut(key) {
            entry.running = false;
        }
    }

    pub async fn is_running(&self, key: &OwnerKey) -> bool {
        let map = self.inner.lock().await;
        map.get(key).map(|s| s.running).unwrap_or(false)
    }
}

/// Merge an existing pending entry with an incoming Manual request.
/// `None` (full rebuild) is the strongest, so any combination involving None resolves to None.
fn merge_pending(existing: Option<RebuildMode>, incoming: RebuildMode) -> RebuildMode {
    match (existing, incoming) {
        (Some(RebuildMode::Manual { concept: None }), _) => RebuildMode::Manual { concept: None },
        (_, RebuildMode::Manual { concept: None }) => RebuildMode::Manual { concept: None },
        (_, m) => m,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> OwnerKey {
        OwnerKey::personal("u1")
    }

    #[tokio::test]
    async fn append_starts_when_idle() {
        let s = StateMap::new();
        let r = s.try_start(&key(), RebuildMode::Append).await;
        assert_eq!(r, StartOutcome::Started(RebuildMode::Append));
        assert!(s.is_running(&key()).await);
    }

    #[tokio::test]
    async fn append_skips_when_running() {
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        let r = s.try_start(&key(), RebuildMode::Append).await;
        assert_eq!(r, StartOutcome::AlreadyRunning);
    }

    #[tokio::test]
    async fn manual_pending_when_running() {
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        let r = s
            .try_start(
                &key(),
                RebuildMode::Manual {
                    concept: Some("c".into()),
                },
            )
            .await;
        assert_eq!(r, StartOutcome::Pending);
        let cont = s.mark_idle_or_continue(&key()).await;
        assert_eq!(
            cont,
            Some(RebuildMode::Manual {
                concept: Some("c".into())
            })
        );
        // After continuation, running stays true
        assert!(s.is_running(&key()).await);
    }

    #[tokio::test]
    async fn manual_none_overrides_some_in_pending() {
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        s.try_start(
            &key(),
            RebuildMode::Manual {
                concept: Some("c".into()),
            },
        )
        .await;
        s.try_start(&key(), RebuildMode::Manual { concept: None })
            .await;
        let cont = s.mark_idle_or_continue(&key()).await;
        assert_eq!(cont, Some(RebuildMode::Manual { concept: None }));
    }

    #[tokio::test]
    async fn mark_idle_when_no_pending() {
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        assert_eq!(s.mark_idle_or_continue(&key()).await, None);
        assert!(!s.is_running(&key()).await);
    }

    #[tokio::test]
    async fn force_idle_releases_slot() {
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        s.force_idle(&key()).await;
        assert!(!s.is_running(&key()).await);
    }

    #[tokio::test]
    async fn append_missed_continues_session_after_dropped_notify() {
        // running 中の try_start(Append) は AlreadyRunning を返すが、
        // append_missed フラグを立てる。次の mark_idle_or_continue は
        // それを拾って Some(Append) を返し、worker を継続させる。
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        // 並行 append 通知が AlreadyRunning で落とされた状態を模す
        let r = s.try_start(&key(), RebuildMode::Append).await;
        assert_eq!(r, StartOutcome::AlreadyRunning);

        // session 終了時、append_missed が拾われて Append で継続
        let cont = s.mark_idle_or_continue(&key()).await;
        assert_eq!(cont, Some(RebuildMode::Append));
        assert!(s.is_running(&key()).await);
        // 二度目の mark_idle_or_continue では append_missed は既に消費済みで None
        let cont2 = s.mark_idle_or_continue(&key()).await;
        assert_eq!(cont2, None);
        assert!(!s.is_running(&key()).await);
    }

    #[tokio::test]
    async fn append_missed_takes_priority_over_manual_pending() {
        // manual_pending と append_missed の両方が立っている場合、
        // append_missed を優先する (Manual{Some(c)} が global watermark を
        // advance して他 concept の raw を取りこぼすのを防ぐため)。
        // Manual 側は遅延するが次の mark_idle_or_continue で取り出される。
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        // append 通知が落とされる
        s.try_start(&key(), RebuildMode::Append).await;
        // manual rebuild 要求も追加
        s.try_start(
            &key(),
            RebuildMode::Manual {
                concept: Some("c".into()),
            },
        )
        .await;

        let cont = s.mark_idle_or_continue(&key()).await;
        assert_eq!(
            cont,
            Some(RebuildMode::Append),
            "append_missed must drain before Manual{{Some(c)}} runs to prevent raw loss"
        );
        // Append session 後に manual_pending が取り出される
        let cont2 = s.mark_idle_or_continue(&key()).await;
        assert_eq!(
            cont2,
            Some(RebuildMode::Manual {
                concept: Some("c".into())
            }),
            "manual_pending survives the prepended Append session"
        );
        // 三度目は何も残らず running 解放
        let cont3 = s.mark_idle_or_continue(&key()).await;
        assert_eq!(cont3, None);
        assert!(!s.is_running(&key()).await);
    }

    #[tokio::test]
    async fn release_running_preserve_pending_keeps_manual_and_missed() {
        // 一過性エラー回復用 API: running フラグだけ下げ、pending は温存する。
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        // running 中に manual_pending と append_missed の両方を立てる
        s.try_start(
            &key(),
            RebuildMode::Manual {
                concept: Some("c".into()),
            },
        )
        .await;
        s.try_start(&key(), RebuildMode::Append).await;

        s.release_running_preserve_pending(&key()).await;
        assert!(!s.is_running(&key()).await, "running must be released");

        // 次の worker spawn が pending を引き継げる。
        // Round 4 priority: idle Append start は append_missed を冗長としてクリア、
        // manual_pending は次の mark_idle_or_continue で取り出される。
        let started = s.try_start(&key(), RebuildMode::Append).await;
        assert_eq!(started, StartOutcome::Started(RebuildMode::Append));
        let cont = s.mark_idle_or_continue(&key()).await;
        assert_eq!(
            cont,
            Some(RebuildMode::Manual {
                concept: Some("c".into())
            }),
            "manual_pending must survive a recoverable error"
        );
    }

    #[tokio::test]
    async fn idle_manual_start_yields_to_pending_append_missed() {
        // recoverable error 後の再開シナリオ:
        // running=false + append_missed=true + manual_pending=Some(...) で、
        // request_manual が Manual を idle start しようとしたとき、
        // append_missed を優先消化して Append を Started で返す。
        // Manual は manual_pending に積まれて次の mark_idle_or_continue で取り出される。
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        // running 中に append 通知が来て append_missed が立つ
        s.try_start(&key(), RebuildMode::Append).await;
        // running 中に manual rebuild が来て pending に積まれる
        s.try_start(
            &key(),
            RebuildMode::Manual {
                concept: Some("c-prev".into()),
            },
        )
        .await;
        // recoverable error を模擬: running だけ下ろす
        s.release_running_preserve_pending(&key()).await;
        assert!(!s.is_running(&key()).await);

        // 後発の request_manual: idle 再開ルートで Append が先に走るべき
        let outcome = s
            .try_start(
                &key(),
                RebuildMode::Manual {
                    concept: Some("c-new".into()),
                },
            )
            .await;
        assert_eq!(
            outcome,
            StartOutcome::Started(RebuildMode::Append),
            "idle restart with append_missed must drain first, not Manual"
        );
        assert!(s.is_running(&key()).await);

        // Append session 完了後、manual_pending を消費 (c-prev と c-new の merge 結果)
        let cont = s.mark_idle_or_continue(&key()).await;
        match cont {
            Some(RebuildMode::Manual { concept }) => {
                // merge_pending の挙動: 同じ Some(c) どうしは incoming で上書き
                // c-prev → c-new (incoming) で上書きされている
                assert_eq!(concept.as_deref(), Some("c-new"));
            }
            _ => panic!("expected Manual{{Some(_)}} pending, got {cont:?}"),
        }
    }

    #[tokio::test]
    async fn idle_append_start_clears_append_missed() {
        // idle で append_missed=true + incoming Append なら冗長フラグをクリアして
        // Append を直 Started する (manual_pending には積まない)。
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        s.try_start(&key(), RebuildMode::Append).await; // miss を立てる
        s.release_running_preserve_pending(&key()).await;

        let outcome = s.try_start(&key(), RebuildMode::Append).await;
        assert_eq!(outcome, StartOutcome::Started(RebuildMode::Append));
        // append_missed は冗長としてクリア
        let cont = s.mark_idle_or_continue(&key()).await;
        assert_eq!(
            cont, None,
            "append_missed must be cleared on idle Append start"
        );
    }

    #[tokio::test]
    async fn force_idle_clears_append_missed() {
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        s.try_start(&key(), RebuildMode::Append).await; // miss を立てる
        s.force_idle(&key()).await;
        // force_idle 後は append_missed もクリア → 次の mark_idle_or_continue は None
        // (但し running=false の状態なので mark_idle_or_continue を呼ぶ前提が崩れる。
        //  ここでは新しい try_start から始まる正常パスを検証)
        let started = s.try_start(&key(), RebuildMode::Append).await;
        assert_eq!(started, StartOutcome::Started(RebuildMode::Append));
        let cont = s.mark_idle_or_continue(&key()).await;
        assert_eq!(cont, None, "force_idle should clear append_missed");
    }
}
