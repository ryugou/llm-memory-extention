use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RawId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SharedMemoryId(pub String);

#[derive(Debug, Error)]
pub enum IdError {
    #[error("invalid shared memory id: {0}")]
    InvalidSharedMemoryId(String),
}

pub fn new_ulid() -> String {
    ulid::Ulid::new().to_string()
}

impl SharedMemoryId {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"^[a-z0-9][a-z0-9-]{0,63}$").unwrap());
        if re.is_match(s) {
            Ok(Self(s.to_string()))
        } else {
            Err(IdError::InvalidSharedMemoryId(s.to_string()))
        }
    }
}

impl fmt::Display for UserId { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) } }
impl fmt::Display for RawId { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) } }
impl fmt::Display for SharedMemoryId { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) } }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulid_is_26_chars_and_sortable() {
        let a = new_ulid();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_ulid();
        assert_eq!(a.len(), 26);
        assert_eq!(b.len(), 26);
        assert!(a < b, "ULID should be time-ordered: {a} >= {b}");
    }

    #[test]
    fn shared_memory_id_accepts_valid() {
        assert!(SharedMemoryId::parse("company-wide").is_ok());
        assert!(SharedMemoryId::parse("a").is_ok());
        assert!(SharedMemoryId::parse("team-frontend-2026").is_ok());
    }

    #[test]
    fn shared_memory_id_rejects_invalid() {
        assert!(SharedMemoryId::parse("-leading-hyphen").is_err());
        assert!(SharedMemoryId::parse("UPPER").is_err());
        assert!(SharedMemoryId::parse("with space").is_err());
        assert!(SharedMemoryId::parse("").is_err());
        let too_long = "a".repeat(65);
        assert!(SharedMemoryId::parse(&too_long).is_err());
    }
}
