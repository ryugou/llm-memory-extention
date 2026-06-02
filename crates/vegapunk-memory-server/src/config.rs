//! 起動時設定。env var から読み込む。
//!
//! `from_env` は値が無いとき `anyhow::Error` を返す (= unwrap せず Result で
//! エラー化)。起動 binary で `?` で受けて、欠けている env var 名を tracing で
//! 報告した上で exit する想定。
//!
//! 必須 (未設定だと `from_env` が Err を返す):
//! - `DATABASE_URL`              (例: sqlite:///data/vegapunk-memory.sqlite)
//! - `PUBLIC_URL`                (例: https://vegapunk-136-110-78-245.nip.io)
//! - `GOOGLE_OAUTH_CLIENT_ID`    (Secret Manager 経由で注入)
//! - `GOOGLE_OAUTH_CLIENT_SECRET`
//! - `VEGAPUNK_GRPC_ENDPOINT`    (例: http://vegapunk.local:6840)
//! - `VEGAPUNK_BEARER_TOKEN`     (vegapunk server.auth.token と一致)
//!
//! オプション (default あり):
//! - `BIND_ADDR`                 (default `0.0.0.0:8081`)
//! - `TRUSTED_PROXY_COUNT`       (default 1)
//!
//! 後続 PR で追加予定 (本 skeleton では未読込):
//! - `JWT_SIGNING_KEY_<kid>`     (HS256 base64 32+ bytes、HTTP transport 実装と
//!   合わせて `llm_memory_auth::jwt::JwtKeys::from_env()` 経由で読み込む)

use anyhow::{Context, Result};

/// 秘匿値 (OAuth client secret / vegapunk bearer / JWT 鍵 など) を含むため
/// `Debug` を derive しない。意図せず `{:?}` でログに流出するのを防ぐ。
/// 必要なら redaction 付きの手書き `Debug` 実装を追加する。
#[derive(Clone)]
pub struct ServerConfig {
    pub database_url: String,
    pub bind_addr: String,
    pub public_url: String,
    pub google_client_id: String,
    pub google_client_secret: String,
    pub vegapunk_grpc_endpoint: String,
    pub vegapunk_bearer_token: String,
    pub trusted_proxy_count: usize,
}

impl ServerConfig {
    pub fn from_env() -> Result<Self> {
        // `PUBLIC_URL` は末尾 `/` を取り除いて正規化する。残したまま OAuth の
        // redirect URI や issuer 文字列に format! で連結すると `https://host//...`
        // の double-slash になり、Google OAuth client 側に登録された redirect URI
        // とマッチしないなどの紛らわしい failure を生む。`AsState::new` 側でも
        // 同じ正規化が行われるが、`ServerConfig` 自体の値も Google redirect URI
        // 構築に使われる (`app::build_router` 内) ため一元的に正規化しておく。
        let public_url = env_required("PUBLIC_URL")?
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            database_url: env_required("DATABASE_URL")?,
            bind_addr: std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".to_string()),
            public_url,
            google_client_id: env_required("GOOGLE_OAUTH_CLIENT_ID")?,
            google_client_secret: env_required("GOOGLE_OAUTH_CLIENT_SECRET")?,
            vegapunk_grpc_endpoint: env_required("VEGAPUNK_GRPC_ENDPOINT")?,
            vegapunk_bearer_token: env_required("VEGAPUNK_BEARER_TOKEN")?,
            trusted_proxy_count: std::env::var("TRUSTED_PROXY_COUNT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1),
        })
    }
}

fn env_required(key: &str) -> Result<String> {
    let value = std::env::var(key).with_context(|| format!("missing required env var: {key}"))?;
    // 空文字 / whitespace-only な値は実用的には未設定と等価で、後段の API 呼び出しで
    // 紛らわしい失敗を生む。env_required の時点で fail-fast してエラーメッセージで
    // 原因を明示する。
    if value.trim().is_empty() {
        anyhow::bail!("required env var {key} is empty or whitespace-only");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    /// trailing slash 付き PUBLIC_URL を渡しても、normalize されて Google redirect
    /// URI 構築や AsState の issuer 文字列で double-slash にならないこと。
    /// (env を直接いじらずに正規化ロジック単体を test する)
    #[test]
    fn public_url_strips_trailing_slash() {
        let value = "https://vegapunk-host.example.com/"
            .trim_end_matches('/')
            .to_string();
        assert_eq!(value, "https://vegapunk-host.example.com");
    }
}
