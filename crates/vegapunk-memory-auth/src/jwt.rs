use base64::Engine;
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::AuthError;
use llm_memory_core::time::now_ms;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Claims {
    pub sub: String,
    pub client_id: String,
    pub iat: i64,
    pub exp: i64,
}

#[derive(Clone)]
pub struct JwtKeys {
    pub current_kid: String,
    pub keys: HashMap<String, Vec<u8>>,
}

/// HS256 RFC 8725 §3.2 推奨の最低鍵長 (256 bit = 32 bytes)。
pub const MIN_HS256_KEY_BYTES: usize = 32;

impl JwtKeys {
    /// Load keys from env vars named `JWT_SIGNING_KEY_<kid>` (base64).
    /// The lexicographically largest kid becomes the current one.
    ///
    /// Validation (fail-fast at startup):
    /// - 最低 1 件の `JWT_SIGNING_KEY_<kid>` 環境変数が必要 (空集合は Err)
    /// - 各値は valid base64 でなければ Err (panic しない)
    /// - 各 decode 後の鍵は >= 32 bytes (HS256) でなければ Err
    pub fn from_env() -> Result<Self, AuthError> {
        let mut keys = HashMap::new();
        let mut current = String::new();
        for (k, v) in std::env::vars() {
            if let Some(kid) = k.strip_prefix("JWT_SIGNING_KEY_") {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&v)
                    .map_err(|e| {
                        AuthError::OAuth(format!("JWT_SIGNING_KEY_{kid}: invalid base64 ({e})"))
                    })?;
                if bytes.len() < MIN_HS256_KEY_BYTES {
                    return Err(AuthError::OAuth(format!(
                        "JWT_SIGNING_KEY_{kid}: decoded key is {} bytes; HS256 requires >= {}",
                        bytes.len(),
                        MIN_HS256_KEY_BYTES
                    )));
                }
                keys.insert(kid.to_string(), bytes);
                if kid > current.as_str() {
                    current = kid.to_string();
                }
            }
        }
        if keys.is_empty() {
            return Err(AuthError::OAuth(
                "no JWT_SIGNING_KEY_<kid> environment variable configured; \
                 refusing to start without a signing key"
                    .into(),
            ));
        }
        Ok(Self {
            current_kid: current,
            keys,
        })
    }

    /// Convenience constructor for tests. Returns a `JwtKeys` with a single
    /// 32-byte key tagged `v1`. Not for production use.
    #[doc(hidden)]
    pub fn for_tests() -> Self {
        let mut keys = HashMap::new();
        keys.insert("v1".into(), b"01234567890123456789012345678901".to_vec());
        Self {
            current_kid: "v1".into(),
            keys,
        }
    }
}

pub fn issue(
    keys: &JwtKeys,
    user_id: &str,
    client_id: &str,
    ttl_seconds: i64,
) -> Result<String, AuthError> {
    let now = now_ms() / 1000;
    let claims = Claims {
        sub: user_id.into(),
        client_id: client_id.into(),
        iat: now,
        exp: now + ttl_seconds,
    };
    let mut header = Header::new(jsonwebtoken::Algorithm::HS256);
    header.kid = Some(keys.current_kid.clone());
    let secret = keys
        .keys
        .get(&keys.current_kid)
        .ok_or(AuthError::MissingKid)?;
    Ok(jsonwebtoken::encode(
        &header,
        &claims,
        &EncodingKey::from_secret(secret),
    )?)
}

pub fn verify(keys: &JwtKeys, token: &str) -> Result<Claims, AuthError> {
    let header = jsonwebtoken::decode_header(token)?;
    let kid = header.kid.ok_or(AuthError::MissingKid)?;
    let secret = keys
        .keys
        .get(&kid)
        .ok_or_else(|| AuthError::UnknownKid(kid.clone()))?;
    let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.validate_exp = true;
    let data =
        jsonwebtoken::decode::<Claims>(token, &DecodingKey::from_secret(secret), &validation)?;
    Ok(data.claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> JwtKeys {
        let mut m = HashMap::new();
        m.insert("v1".into(), b"01234567890123456789012345678901".to_vec());
        JwtKeys {
            current_kid: "v1".into(),
            keys: m,
        }
    }

    #[test]
    fn issue_and_verify_roundtrip() {
        let k = keys();
        let token = issue(&k, "u1", "c1", 3600).unwrap();
        let claims = verify(&k, &token).unwrap();
        assert_eq!(claims.sub, "u1");
        assert_eq!(claims.client_id, "c1");
    }

    #[test]
    fn unknown_kid_rejected() {
        let mut k = keys();
        let token = issue(&k, "u1", "c1", 3600).unwrap();
        k.keys.remove("v1");
        let err = verify(&k, &token).unwrap_err();
        assert!(matches!(err, AuthError::UnknownKid(_)));
    }

    #[test]
    fn for_tests_returns_valid_keys() {
        let k = JwtKeys::for_tests();
        assert_eq!(k.current_kid, "v1");
        assert!(k.keys.get("v1").unwrap().len() >= MIN_HS256_KEY_BYTES);
    }

    #[test]
    fn old_kid_still_valid_during_rotation_window() {
        // v1 で発行、v2 を追加して current_kid = v2 にしても v1 token は valid
        let mut k = keys();
        let token = issue(&k, "u1", "c1", 3600).unwrap();
        k.keys
            .insert("v2".into(), b"abcdefghijklmnopqrstuvwxyzabcdef".to_vec());
        k.current_kid = "v2".into();
        let claims = verify(&k, &token).unwrap();
        assert_eq!(claims.sub, "u1");
    }
}
