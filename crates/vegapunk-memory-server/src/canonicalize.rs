//! LLM (Gemini Flash) ベースの text canonicalization。
//!
//! PR #21 の word-boundary scan は **大文字小文字差・所有格** など句読点
//! 境界の variants を canonical name に rewrite する。しかし `vegpunk`
//! (= typo) や `Vegapunkとは` (= 助詞付き異字体) のような **編集距離あり**
//! のケースは exact substring 比較で取りこぼす。本モジュールは
//! `gemini-3.5-flash` に catalogue (= 既存 entity 名一覧) と text を渡して、
//! 「typo / 異字体 / 同義語のうち、catalogue 内の canonical に高い確信度で
//! 一致するものだけ」を canonical name に書き換える pipeline を提供する。
//!
//! 設計方針:
//! - **stochastic な層**: LLM 呼び出しの失敗は静かに fallback (= warn ログ +
//!   元 text を返す) して ingest 全体を落とさない。冪等な高信頼レイヤは
//!   依然 PR #21 word-boundary scan で、本モジュールはその後段の追加
//!   canonicalization に位置づける。
//! - **API key 未設定なら無効化**: `AppState.canonicalizer = None` の場合
//!   handler は本モジュールを呼ばず PR #21 結果をそのまま使う。
//!
//! Gemini API 仕様参照:
//! - https://ai.google.dev/gemini-api/docs/text-generation
//! - エンドポイント: POST https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent
//! - 認証: header `x-goog-api-key: {api_key}`

use std::time::Duration;

use reqwest::Client;
use thiserror::Error;
use tracing::warn;

/// LLM canonicalize の上限。これより長い text は LLM 呼び出し対象外
/// (= そのまま返す)。LLM の context window を超えるリクエストで料金や
/// レイテンシが爆発するのを防ぐ防御線。
const MAX_TEXT_BYTES: usize = 32 * 1024;

/// catalogue 件数の上限。これより多いと prompt が肥大化するため、
/// 切り詰めて先頭分のみ LLM に渡す。
const MAX_CATALOGUE_ENTRIES: usize = 200;

#[derive(Debug, Error)]
pub enum CanonicalizeError {
    #[error("http transport error: {0}")]
    Http(String),
    #[error("gemini api error: status={status} body={body}")]
    Api { status: u16, body: String },
    #[error("response parse error: {0}")]
    Parse(String),
}

/// Gemini API client。`AppState` から `Arc<GeminiCanonicalizer>` で共有。
#[derive(Debug, Clone)]
pub struct GeminiCanonicalizer {
    client: Client,
    api_key: String,
    model: String,
}

impl GeminiCanonicalizer {
    /// `timeout` は 1 リクエストの上限。Gemini Flash の通常応答は 1〜3 秒
    /// だが、混雑時は数十秒かかることがある。caller (= handler) が更に
    /// 上位の deadline を持つ前提で、ここでは 30 秒を default にする。
    pub fn new(
        api_key: String,
        model: String,
        timeout: Duration,
    ) -> Result<Self, CanonicalizeError> {
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| CanonicalizeError::Http(format!("build client: {e}")))?;
        Ok(Self {
            client,
            api_key,
            model,
        })
    }

    /// `catalogue_names` を canonical 候補として `text` を rewrite する。
    /// 失敗時は元 text を返さず Err を bubble する (= 呼び出し側で warn +
    /// fallback)。
    ///
    /// 空 catalogue / 空 text / 上限超過 text は **API 呼び出し無しで** 元 text
    /// をそのまま返す (= LLM 呼び出しのコスト 0 で no-op)。
    pub async fn canonicalize(
        &self,
        catalogue_names: &[String],
        text: &str,
    ) -> Result<String, CanonicalizeError> {
        if catalogue_names.is_empty() {
            return Ok(text.to_string());
        }
        if text.trim().is_empty() {
            return Ok(text.to_string());
        }
        if text.len() > MAX_TEXT_BYTES {
            warn!(
                text_bytes = text.len(),
                limit_bytes = MAX_TEXT_BYTES,
                "canonicalize: text exceeds size limit; skipping LLM call"
            );
            return Ok(text.to_string());
        }

        let prompt = build_prompt(catalogue_names, text);
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            self.model
        );
        let body = serde_json::json!({
            "contents": [{
                "role": "user",
                "parts": [{"text": prompt}]
            }],
            "generationConfig": {
                // 決定論的に動かしたい (= 同じ入力 → 同じ rewrite)。
                "temperature": 0.0,
                // 出力は text とほぼ同じか少し短くなる前提。catalogue + text の
                // 合計 byte 数より小さく setしておく。安全マージンで 32k tokens
                // (= Gemini Flash の MaxOutputTokens 上限内)。
                "maxOutputTokens": 32768,
            }
        });

        let resp = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| CanonicalizeError::Http(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_else(|_| "<no body>".into());
            return Err(CanonicalizeError::Api {
                status: status.as_u16(),
                // body から API key が漏れるのを避けるため最初 200 文字に切る
                body: body.chars().take(200).collect(),
            });
        }
        let payload: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| CanonicalizeError::Parse(format!("json decode: {e}")))?;
        extract_text(&payload)
    }
}

/// Gemini レスポンス JSON から rewritten text を取り出す。
/// candidates[0].content.parts[*].text を順番に concat する (= Gemini は
/// 通常 1 part に収まるが multi-part も仕様上ありうる)。
fn extract_text(payload: &serde_json::Value) -> Result<String, CanonicalizeError> {
    let parts = payload
        .pointer("/candidates/0/content/parts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CanonicalizeError::Parse("missing candidates[0].content.parts".into()))?;
    let mut out = String::new();
    for p in parts {
        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
            out.push_str(t);
        }
    }
    if out.is_empty() {
        return Err(CanonicalizeError::Parse(
            "candidates[0].content.parts had no text".into(),
        ));
    }
    Ok(out)
}

/// LLM 用 prompt 構築。catalogue は `MAX_CATALOGUE_ENTRIES` で切り詰める。
/// 重複名は事前に dedup する責任は caller 側 (= 通常 catalogue は wrapper
/// 内 `collect_dedup_catalogue` から取るので既に並列で揃っている)。
fn build_prompt(catalogue_names: &[String], text: &str) -> String {
    let trimmed: Vec<&str> = catalogue_names
        .iter()
        .take(MAX_CATALOGUE_ENTRIES)
        .map(String::as_str)
        .collect();
    let entities = trimmed
        .iter()
        .map(|n| format!("- {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are a text canonicalization helper for a knowledge graph ingest pipeline.\n\
\n\
You will be given:\n\
1. A list of canonical entity names that already exist in the graph.\n\
2. A piece of text that is about to be ingested.\n\
\n\
Your task is to rewrite the text so that any clear reference to an existing canonical entity uses the canonical name verbatim. This includes:\n\
- Typos (e.g. 'vegpunk' -> 'Vegapunk')\n\
- Possessives and particles (e.g. 'Vegapunk's', 'Vegapunkの')\n\
- Abbreviations and alternate spellings that obviously refer to the canonical entity\n\
\n\
Strict rules:\n\
- Only rewrite when you are highly confident (>90%) that the reference matches a listed entity.\n\
- Do NOT add or remove information, summarize, or rephrase non-entity content.\n\
- Preserve all other words, punctuation, line breaks, and formatting verbatim.\n\
- If you are unsure, leave the word unchanged.\n\
- Output ONLY the rewritten text. No commentary, no markdown fences, no preamble.\n\
\n\
Canonical entities:\n{entities}\n\
\n\
Text:\n{text}\n\
\n\
Rewritten text:"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_catalogue_returns_text_unchanged() {
        // tokio runtime 不要の path (= 早期 return)。
        let rt = tokio::runtime::Runtime::new().unwrap();
        let canon = GeminiCanonicalizer::new(
            "test-key".into(),
            "gemini-3.5-flash".into(),
            Duration::from_secs(30),
        )
        .unwrap();
        let out = rt.block_on(canon.canonicalize(&[], "hello world")).unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn empty_text_returns_unchanged() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let canon = GeminiCanonicalizer::new(
            "test-key".into(),
            "gemini-3.5-flash".into(),
            Duration::from_secs(30),
        )
        .unwrap();
        let names = vec!["Vegapunk".to_string()];
        let out = rt.block_on(canon.canonicalize(&names, "")).unwrap();
        assert_eq!(out, "");
        let out = rt.block_on(canon.canonicalize(&names, "   ")).unwrap();
        assert_eq!(out, "   ");
    }

    #[test]
    fn oversize_text_returns_unchanged_without_api_call() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let canon = GeminiCanonicalizer::new(
            "test-key".into(),
            "gemini-3.5-flash".into(),
            Duration::from_secs(30),
        )
        .unwrap();
        let big_text = "x".repeat(MAX_TEXT_BYTES + 1);
        let names = vec!["Vegapunk".to_string()];
        // API key は dummy だが呼び出し前に早期 return するので Ok を返す。
        let out = rt.block_on(canon.canonicalize(&names, &big_text)).unwrap();
        assert_eq!(out, big_text);
    }

    #[test]
    fn build_prompt_includes_catalogue_and_text() {
        let names = vec!["Vegapunk".into(), "GraphRAG Engine".into()];
        let p = build_prompt(&names, "We use vegpunk for graphs.");
        assert!(p.contains("- Vegapunk"));
        assert!(p.contains("- GraphRAG Engine"));
        assert!(p.contains("We use vegpunk for graphs."));
        // strict rule の指示文も含まれていること。
        assert!(p.contains("rewrite when you are highly confident"));
    }

    #[test]
    fn build_prompt_truncates_long_catalogue() {
        let names: Vec<String> = (0..(MAX_CATALOGUE_ENTRIES + 50))
            .map(|i| format!("Entity{i}"))
            .collect();
        let p = build_prompt(&names, "text");
        // 切り詰め境界の前のものは入っている。
        let last_idx = MAX_CATALOGUE_ENTRIES - 1;
        assert!(p.contains(&format!("- Entity{last_idx}")));
        // 切り詰め後のものは入っていない。
        assert!(!p.contains(&format!("- Entity{MAX_CATALOGUE_ENTRIES}")));
    }

    #[test]
    fn extract_text_concatenates_multi_parts() {
        let payload = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "hello "},
                        {"text": "world"}
                    ]
                }
            }]
        });
        assert_eq!(extract_text(&payload).unwrap(), "hello world");
    }

    #[test]
    fn extract_text_fails_on_missing_parts() {
        let payload = serde_json::json!({"candidates": [{"content": {}}]});
        let err = extract_text(&payload).unwrap_err();
        assert!(matches!(err, CanonicalizeError::Parse(_)));
    }

    #[test]
    fn extract_text_fails_on_empty_text() {
        let payload = serde_json::json!({
            "candidates": [{"content": {"parts": [{"text": ""}]}}]
        });
        let err = extract_text(&payload).unwrap_err();
        assert!(matches!(err, CanonicalizeError::Parse(_)));
    }
}
