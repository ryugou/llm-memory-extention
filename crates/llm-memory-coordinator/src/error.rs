use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoordinatorError {
    #[error(transparent)]
    Storage(#[from] llm_memory_storage::error::StorageError),
    #[error(transparent)]
    Llm(#[from] llm_memory_llm::error::LlmError),
    #[error("worker panicked")]
    WorkerPanic,
}
