use crate::id::SharedMemoryId;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Personal,
    Shared,
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Personal => f.write_str("personal"),
            Scope::Shared => f.write_str("shared"),
        }
    }
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Personal => "personal",
            Scope::Shared => "shared",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OwnerKey {
    pub scope: Scope,
    pub owner_id: String,
}

impl OwnerKey {
    pub fn personal(user_id: impl Into<String>) -> Self {
        Self {
            scope: Scope::Personal,
            owner_id: user_id.into(),
        }
    }
    pub fn shared(shared_memory_id: SharedMemoryId) -> Self {
        Self {
            scope: Scope::Shared,
            owner_id: shared_memory_id.as_str().to_string(),
        }
    }
    pub fn owner_id(&self) -> &str {
        &self.owner_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_display() {
        assert_eq!(Scope::Personal.to_string(), "personal");
        assert_eq!(Scope::Shared.to_string(), "shared");
    }

    #[test]
    fn owner_key_equality() {
        let a = OwnerKey::personal("u1");
        let b = OwnerKey::personal("u1");
        assert_eq!(a, b);
    }

    #[test]
    fn owner_key_shared_from_validated_id() {
        use crate::id::SharedMemoryId;
        let sm = SharedMemoryId::parse("company-wide").unwrap();
        let k = OwnerKey::shared(sm);
        assert_eq!(k.scope, Scope::Shared);
        assert_eq!(k.owner_id(), "company-wide");
    }
}
