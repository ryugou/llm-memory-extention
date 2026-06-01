//! vegapunk-memory-server 専用 storage 層。
//!
//! データ本体 (wiki / raw 相当) は vegapunk gRPC 側に保持するため、
//! ここでは認証・ユーザメタ情報 (users / oauth_clients / tokens /
//! shared_memories) のみを SQLite に格納する。
//!
//! migration: `migrations/0001_initial.sql`

pub mod error;
pub mod oauth_clients;
pub mod pool;
pub mod shared_memories;
pub mod tokens;
pub mod users;

pub use error::StorageError;
