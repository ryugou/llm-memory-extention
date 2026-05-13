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
    /// - If idle: marks running=true and returns Started(mode).
    /// - If running and mode is Append: returns AlreadyRunning (lazy drain).
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
                RebuildMode::Append => StartOutcome::AlreadyRunning,
            }
        } else {
            entry.running = true;
            StartOutcome::Started(mode)
        }
    }

    /// Called by worker after a session ends. Returns Some(next_mode) if there's
    /// a manual_pending and the worker should continue with it (running remains true).
    /// Returns None if the worker should fully release the slot (running becomes false).
    pub async fn mark_idle_or_continue(&self, key: &OwnerKey) -> Option<RebuildMode> {
        let mut map = self.inner.lock().await;
        let entry = map.entry(key.clone()).or_default();
        if let Some(m) = entry.manual_pending.take() {
            Some(m)
        } else {
            entry.running = false;
            None
        }
    }

    /// Force-release the slot (used by panic recovery in spawn wrapper).
    pub async fn force_idle(&self, key: &OwnerKey) {
        let mut map = self.inner.lock().await;
        if let Some(entry) = map.get_mut(key) {
            entry.running = false;
            entry.manual_pending = None;
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
}
