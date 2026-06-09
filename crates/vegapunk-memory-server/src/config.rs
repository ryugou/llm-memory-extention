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
//! - `BIND_ADDR` (default `0.0.0.0:8081`)
//! - `TRUSTED_PROXY_COUNT` (default `1`)
//! - `VEGAPUNK_SHARED_SCHEMA_NAME` (default `sivira-shared`).
//!   全 user で共有する schema 名。OAuth callback ごとに idempotent に
//!   `ensure_schema` される。値は trim される。
//! - `VEGAPUNK_DEFAULT_SCHEMA_TEMPLATE` (default `discussion`).
//!   個人 schema を新規 provision するときに vegapunk へ渡す
//!   `SchemaTemplate` の名前。値は trim される。`ListSchemaTemplates` で
//!   利用可能な template を確認しておくこと。
//!
//! 別経路で読まれる env (本ファイルでは扱わない):
//! - `JWT_SIGNING_KEY_<kid>` (HS256 base64 32+ bytes、`jwt::JwtKeys::from_env()`)

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
    /// 全 user で共有する vegapunk schema の名前 (例: `sivira-shared`)。
    /// 初回 user の OAuth callback で idempotent に作成される。
    pub shared_schema_name: String,
    /// 個人 schema を新規作成するときに使う vegapunk template name
    /// (例: `discussion`、`review`)。vegapunk の ListSchemaTemplates の
    /// いずれかに一致する必要がある。
    pub default_schema_template: String,
    /// Gemini API キー (LLM canonicalize 用)。`None` なら LLM
    /// canonicalize は無効化されて PR #21 word-boundary scan 結果がそのまま
    /// vegapunk に流れる。
    pub gemini_api_key: Option<String>,
    /// LLM canonicalize で使う Gemini model 名。default は最新 Flash 系。
    pub gemini_model: String,
    /// Gemini API 呼び出しの per-request timeout (秒)。LLM 応答が遅い時に
    /// wrapper の deadline 全体を食い潰さないよう、各 LLM 呼び出しは
    /// この秒数で打ち切る。
    pub gemini_timeout_secs: u64,
    /// Gemini API のエンドポイント base URL。テストでは wiremock の URL を
    /// 注入する。production では `https://generativelanguage.googleapis.com/v1beta`
    /// (= default)。末尾 `/` は持たない形に正規化される。
    pub gemini_endpoint_base: String,
}

impl ServerConfig {
    pub fn from_env() -> Result<Self> {
        // `PUBLIC_URL` は末尾 `/` を取り除いて正規化する (詳細は
        // `normalize_public_url` を参照)。
        let public_url = normalize_public_url(env_required("PUBLIC_URL")?);
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
            // 値自体も trim して持つ: env で先頭末尾に空白が紛れていても
            // そのまま vegapunk へ渡さない (= "  sivira-shared  " のような
            // 値が schema 名として保存されると vegapunk 側で別 schema 扱いに
            // なって idempotent ensure_schema が壊れる)。
            shared_schema_name: std::env::var("VEGAPUNK_SHARED_SCHEMA_NAME")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "sivira-shared".to_string()),
            default_schema_template: std::env::var("VEGAPUNK_DEFAULT_SCHEMA_TEMPLATE")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "discussion".to_string()),
            gemini_api_key: std::env::var("GEMINI_API_KEY")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            gemini_model: std::env::var("GEMINI_MODEL")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "gemini-3.5-flash".to_string()),
            gemini_timeout_secs: std::env::var("GEMINI_TIMEOUT_SECS")
                .ok()
                // Copilot review #26 round 2: 他の string env (e.g.
                // `VEGAPUNK_SHARED_SCHEMA_NAME`) と同じく先頭末尾 whitespace を
                // 取ってから parse する。テンプレートで `" 30"` のように渡る
                // ケースで silent fallback してしまうのを防ぐ。
                .map(|s| s.trim().to_string())
                .and_then(|s| s.parse().ok())
                .filter(|&n: &u64| n > 0)
                .unwrap_or(30),
            gemini_endpoint_base: std::env::var("GEMINI_ENDPOINT_BASE")
                .ok()
                .map(|s| s.trim().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string()),
        })
    }
}

/// `PUBLIC_URL` の正規化: 末尾 `/` を取り除く。
///
/// 残したまま OAuth の redirect URI や issuer 文字列に `format!` で連結すると
/// `https://host//oauth/callback/google` の double-slash になり、Google OAuth
/// client 側に登録された redirect URI とマッチしないなどの紛らわしい failure
/// を生む。`AsState::new` 側でも同じ正規化が行われるが、`ServerConfig` 自体の
/// 値も Google redirect URI 構築に使われる (`app::build_router` 内) ため
/// `from_env` の入口で一元的に正規化しておく。
fn normalize_public_url(raw: String) -> String {
    raw.trim_end_matches('/').to_string()
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
    use super::normalize_public_url;

    /// `from_env` が経由する `normalize_public_url` を直接呼んで、
    /// trailing slash が削られることを保証する (= 関数本体を消すと test が
    /// fail することで回帰検知できる)。
    #[test]
    fn normalize_public_url_strips_single_trailing_slash() {
        assert_eq!(
            normalize_public_url("https://vegapunk-host.example.com/".into()),
            "https://vegapunk-host.example.com"
        );
    }

    #[test]
    fn normalize_public_url_strips_multiple_trailing_slashes() {
        assert_eq!(
            normalize_public_url("https://vegapunk-host.example.com///".into()),
            "https://vegapunk-host.example.com"
        );
    }

    #[test]
    fn normalize_public_url_passthrough_when_no_trailing_slash() {
        assert_eq!(
            normalize_public_url("https://vegapunk-host.example.com".into()),
            "https://vegapunk-host.example.com"
        );
    }
}
