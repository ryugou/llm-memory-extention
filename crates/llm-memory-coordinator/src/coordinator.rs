use std::sync::Arc;

use llm_memory_core::scope::OwnerKey;
use llm_memory_llm::client::AnthropicClient;

use crate::state::{RebuildMode, StartOutcome};
use crate::worker::{spawn_worker, WorkerDeps};

#[derive(Clone)]
pub struct Coordinator<C: AnthropicClient + 'static> {
    deps: Arc<WorkerDeps<C>>,
}

impl<C: AnthropicClient + 'static> Coordinator<C> {
    pub fn new(deps: Arc<WorkerDeps<C>>) -> Self { Self { deps } }

    /// Append-triggered notification. Returns true if a new rebuild worker was spawned,
    /// false if a worker was already running (lazy drain).
    pub async fn notify_append(&self, user_id: &str) -> bool {
        let key = OwnerKey::personal(user_id);
        match self.deps.state.try_start(&key, RebuildMode::Append).await {
            StartOutcome::Started(mode) => {
                spawn_worker(self.deps.clone(), key, mode);
                true
            }
            _ => false,
        }
    }

    /// Manual rebuild request. If `concept` is None, all concepts are rebuilt.
    /// Returns Started when a new worker is spawned, Pending when an existing worker
    /// is running (the request is queued via manual_pending).
    pub async fn request_manual(&self, user_id: &str, concept: Option<String>) -> ManualOutcome {
        let key = OwnerKey::personal(user_id);
        let mode = RebuildMode::Manual { concept };
        match self.deps.state.try_start(&key, mode).await {
            StartOutcome::Started(m) => {
                spawn_worker(self.deps.clone(), key, m);
                ManualOutcome::Started
            }
            StartOutcome::Pending => ManualOutcome::Pending,
            // AlreadyRunning is only returned for Append; Manual always goes to Pending when running.
            StartOutcome::AlreadyRunning => ManualOutcome::Pending,
        }
    }

    /// For tests / introspection
    pub fn deps(&self) -> &Arc<WorkerDeps<C>> { &self.deps }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ManualOutcome {
    Started,
    Pending,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateMap;
    use llm_memory_llm::mock::MockClient;
    use llm_memory_storage::pool::init_pool;

    async fn make_coord() -> Coordinator<MockClient> {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let mock = Arc::new(MockClient::new());
        let deps = Arc::new(WorkerDeps {
            pool, state: StateMap::new(), llm: mock,
            model_haiku: "haiku".into(), model_sonnet: "sonnet".into(),
        });
        Coordinator::new(deps)
    }

    #[tokio::test]
    async fn notify_append_returns_true_first_time() {
        let c = make_coord().await;
        // First append: should start (returns true)
        // (Worker may fail with LlmError because mock has no queued responses,
        // but that doesn't affect the notify_append return value.)
        let started = c.notify_append("u1").await;
        assert!(started);
    }

    #[tokio::test]
    async fn request_manual_returns_started_when_idle() {
        let c = make_coord().await;
        let r = c.request_manual("u1", Some("alpha".into())).await;
        assert_eq!(r, ManualOutcome::Started);
    }
}
