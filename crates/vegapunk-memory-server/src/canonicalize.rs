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
pub(crate) const MAX_CATALOGUE_ENTRIES: usize = 200;

/// LLM 出力が **入力 text の何倍まで** 許容されるか。これより長い rewrite
/// は prompt injection / model drift の疑いがあるので拒否する (= 元 text に
/// fallback)。1.5x で「typo を canonical に置換 (= 多少伸びる)」程度の余裕
/// は確保しつつ、`Ignore previous...` 系の暴走出力を検出する。
const MAX_OUTPUT_RATIO: f32 = 1.5;
/// 出力の最低絶対バイト数 (= 入力 text が極小の時の比率検査ノイズを防ぐ)。
const OUTPUT_ABSOLUTE_MIN_BYTES: usize = 256;

#[derive(Debug, Error)]
pub enum CanonicalizeError {
    #[error("http transport error: {0}")]
    Http(String),
    /// API が non-2xx を返した。**body は意図的に保持しない** ことに注意。
    /// prompt や catalogue 名が provider 側 error body に echo されて log に
    /// 出回るリスクを避けるため、status code のみ surface する。
    #[error("gemini api error: status={status}")]
    Api { status: u16 },
    #[error("response parse error: {0}")]
    Parse(String),
    /// LLM 出力が想定範囲を超えた (= 異常に長い / prompt injection 疑い)。
    /// 呼び出し側は元 text に fallback する想定。
    #[error("llm output rejected: {0}")]
    OutputRejected(String),
}

/// Gemini API client。`AppState` から `Arc<GeminiCanonicalizer>` で共有。
///
/// `Debug` は **手書きで実装** して `api_key` を `<redacted>` に置き換える。
/// derive(Debug) を残すと `{:?}` で出力した瞬間に API key が log に流出する
/// 危険があるため、struct field 直アクセス以外では key 値が表に出ないようにする。
#[derive(Clone)]
pub struct GeminiCanonicalizer {
    client: Client,
    api_key: String,
    model: String,
    /// API base URL (末尾 `/` 無し)。production は
    /// `https://generativelanguage.googleapis.com/v1beta`、テストでは
    /// wiremock のホスト URL を注入する。
    endpoint_base: String,
}

impl std::fmt::Debug for GeminiCanonicalizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiCanonicalizer")
            .field("client", &"<reqwest::Client>")
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("endpoint_base", &self.endpoint_base)
            .finish()
    }
}

impl GeminiCanonicalizer {
    /// `timeout` は 1 リクエストの上限。Gemini Flash の通常応答は 1〜3 秒
    /// だが、混雑時は数十秒かかることがある。caller (= handler) が更に
    /// 上位の deadline を持つ前提で、ここでは 30 秒を default にする。
    pub fn new(
        api_key: String,
        model: String,
        endpoint_base: String,
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
            endpoint_base: endpoint_base.trim_end_matches('/').to_string(),
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
            "{}/models/{}:generateContent",
            self.endpoint_base, self.model
        );
        // `validate_output_size` の cap から逆算した安全マージン込みの token 数。
        // どうせ受け取った後に拒否するので、provider 側で巨大生成させて cost /
        // latency を浪費しないよう、上限を request 時点で絞る (Codex round 2)。
        // 入力 byte ≈ 入力 token と仮定し (= 英数主体で 1 byte ≈ 1 char ≈ 0.25
        // token は誇張気味だが安全側)、output cap byte を 1 byte = 1 token と
        // 同等扱いに +256 余白で換算。短い入力でも下限 256 + 1.5x で min 384 を
        // 確保し、最大は 32768 token 程度 (Gemini Flash 上限内)。
        let output_cap_bytes = output_cap_for(text);
        // i32 saturating で安全に。Gemini API は max 32k 程度なので 32k clamp。
        let max_output_tokens: u32 = (output_cap_bytes.min(32_768)).try_into().unwrap_or(8_192);
        let body = serde_json::json!({
            "contents": [{
                "role": "user",
                "parts": [{"text": prompt}]
            }],
            "generationConfig": {
                // 決定論的に動かしたい (= 同じ入力 → 同じ rewrite)。
                "temperature": 0.0,
                "maxOutputTokens": max_output_tokens,
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
            // body は provider 側に prompt/catalogue が echo される可能性
            // (PII / 登録 entity 名) があるため log に乗せない。status のみ
            // surface し、呼び出し側で warn + fallback。debug log でも body は
            // 出さない (= log されない = leak しない、強い保証)。
            // body は drop (= read もしない)。
            let _ = resp;
            return Err(CanonicalizeError::Api {
                status: status.as_u16(),
            });
        }
        let payload: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| CanonicalizeError::Parse(format!("json decode: {e}")))?;
        let output = extract_text(&payload)?;
        validate_output_size(text, &output)?;
        Ok(output)
    }
}

/// LLM 出力 byte 数の上限。`max(OUTPUT_ABSOLUTE_MIN_BYTES, input * MAX_OUTPUT_RATIO)`
/// で計算する (= 短い入力は 256 B の絶対 floor、長い入力は 1.5x で許容)。
/// `validate_output_size` の判定軸であると同時に、API 呼び出し時の
/// `maxOutputTokens` の計算根拠でもある (= 受け取り後拒否されるサイズを
/// provider 側で生成させない)。
fn output_cap_for(input: &str) -> usize {
    OUTPUT_ABSOLUTE_MIN_BYTES.max(((input.len() as f32) * MAX_OUTPUT_RATIO).ceil() as usize)
}

/// LLM 出力サイズの post-validation。`output_cap_for(input)` を上限にして、
/// それを超えたら拒否する。規約上は「rewrite した text のみを返す」想定で
/// 長さほぼ等倍 (typo を canonical に変える程度で多少増減) のはず。極端な
/// 肥大は prompt injection (= LLM が prompt の出力規約を無視して別物を返した)
/// を示唆。
fn validate_output_size(input: &str, output: &str) -> Result<(), CanonicalizeError> {
    let cap = output_cap_for(input);
    if output.len() > cap {
        return Err(CanonicalizeError::OutputRejected(format!(
            "output bytes {} exceed cap {} (input bytes {})",
            output.len(),
            cap,
            input.len()
        )));
    }
    Ok(())
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

    fn test_canon_with_base(base: String) -> GeminiCanonicalizer {
        GeminiCanonicalizer::new(
            "test-key".into(),
            "gemini-3.5-flash".into(),
            base,
            Duration::from_secs(5),
        )
        .unwrap()
    }

    fn test_canon() -> GeminiCanonicalizer {
        // 早期 return path 用 (= 実 URL は呼ばれない)。
        test_canon_with_base("https://invalid.test/v1beta".into())
    }

    #[test]
    fn empty_catalogue_returns_text_unchanged() {
        // tokio runtime 不要の path (= 早期 return)。
        let rt = tokio::runtime::Runtime::new().unwrap();
        let canon = test_canon();
        let out = rt.block_on(canon.canonicalize(&[], "hello world")).unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn empty_text_returns_unchanged() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let canon = test_canon();
        let names = vec!["Vegapunk".to_string()];
        let out = rt.block_on(canon.canonicalize(&names, "")).unwrap();
        assert_eq!(out, "");
        let out = rt.block_on(canon.canonicalize(&names, "   ")).unwrap();
        assert_eq!(out, "   ");
    }

    #[test]
    fn oversize_text_returns_unchanged_without_api_call() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let canon = test_canon();
        let big_text = "x".repeat(MAX_TEXT_BYTES + 1);
        let names = vec!["Vegapunk".to_string()];
        // API key は dummy だが呼び出し前に早期 return するので Ok を返す。
        let out = rt.block_on(canon.canonicalize(&names, &big_text)).unwrap();
        assert_eq!(out, big_text);
    }

    #[test]
    fn validate_output_size_accepts_modest_growth() {
        // typo 補正で多少伸びる程度は許容 (= 入力の 1.5x 以下)。
        let input = "We use vegpunk for graphs.";
        let output = "We use Vegapunk for graphs.";
        assert!(validate_output_size(input, output).is_ok());
    }

    #[test]
    fn validate_output_size_rejects_huge_blowup() {
        let input = "short";
        // 出力が >> 入力 * 1.5 + min(256) で拒否される。
        let output = "x".repeat(OUTPUT_ABSOLUTE_MIN_BYTES + 1);
        let err = validate_output_size(input, output.as_str()).unwrap_err();
        assert!(matches!(err, CanonicalizeError::OutputRejected(_)));
    }

    #[test]
    fn validate_output_size_uses_absolute_minimum_for_tiny_input() {
        // 入力 5 byte で 1.5x = 7.5 byte だが、絶対最低 256 byte までは許容
        // (= 比率検査が誤検知しないための floor)。
        let input = "tiny!";
        let output_within = "x".repeat(OUTPUT_ABSOLUTE_MIN_BYTES);
        assert!(validate_output_size(input, &output_within).is_ok());
    }

    // ── HTTP mock-based tests (wiremock) ─────────────────────────────────

    /// wiremock の base URL から GeminiCanonicalizer を作る helper。
    async fn mock_canon(server: &wiremock::MockServer) -> GeminiCanonicalizer {
        test_canon_with_base(server.uri())
    }

    #[tokio::test]
    async fn success_returns_rewritten_text() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/gemini-3.5-flash:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [{
                    "content": {
                        "parts": [{"text": "We use Vegapunk for graphs."}]
                    }
                }]
            })))
            .mount(&server)
            .await;
        let canon = mock_canon(&server).await;
        let names = vec!["Vegapunk".to_string()];
        let out = canon
            .canonicalize(&names, "We use vegpunk for graphs.")
            .await
            .unwrap();
        assert_eq!(out, "We use Vegapunk for graphs.");
    }

    #[tokio::test]
    async fn non_2xx_returns_api_error_without_body_leak() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        // provider error body に prompt が echo されたケースを再現
        // → CanonicalizeError::Api { status } には body が含まれないことを確認。
        Mock::given(method("POST"))
            .and(path("/models/gemini-3.5-flash:generateContent"))
            .respond_with(
                ResponseTemplate::new(500).set_body_string(
                    "internal error: catalogue contained PII (Person 'Ryugo'), secret tokens echoed back",
                ),
            )
            .mount(&server)
            .await;
        let canon = mock_canon(&server).await;
        let names = vec!["Vegapunk".to_string()];
        let err = canon
            .canonicalize(&names, "We use vegpunk.")
            .await
            .unwrap_err();
        match err {
            CanonicalizeError::Api { status } => assert_eq!(status, 500),
            other => panic!("expected Api error, got {other:?}"),
        }
        // error の Display 表現に body が含まれないことを確認 (= 公開時の leak 防止)。
        let display = format!("{}", CanonicalizeError::Api { status: 500 });
        assert!(!display.contains("PII"));
        assert!(!display.contains("secret"));
        assert!(display.contains("500"));
    }

    #[tokio::test]
    async fn malformed_json_response_returns_parse_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/gemini-3.5-flash:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not actually json{["))
            .mount(&server)
            .await;
        let canon = mock_canon(&server).await;
        let names = vec!["Vegapunk".to_string()];
        let err = canon
            .canonicalize(&names, "We use vegpunk.")
            .await
            .unwrap_err();
        assert!(matches!(err, CanonicalizeError::Parse(_)));
    }

    #[tokio::test]
    async fn valid_json_missing_candidates_returns_parse_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/gemini-3.5-flash:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"foo": 1})))
            .mount(&server)
            .await;
        let canon = mock_canon(&server).await;
        let names = vec!["Vegapunk".to_string()];
        let err = canon
            .canonicalize(&names, "We use vegpunk.")
            .await
            .unwrap_err();
        assert!(matches!(err, CanonicalizeError::Parse(_)));
    }

    #[tokio::test]
    async fn huge_output_is_rejected() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        // input ~30 byte に対し output 8 KiB を返す (prompt injection で
        // 暴走したケース)。MAX_OUTPUT_RATIO * 30 ≈ 45、absolute floor 256
        // を超えるので拒否されるはず。
        let blown_up: String = "X".repeat(8 * 1024);
        Mock::given(method("POST"))
            .and(path("/models/gemini-3.5-flash:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [{
                    "content": {"parts": [{"text": blown_up}]}
                }]
            })))
            .mount(&server)
            .await;
        let canon = mock_canon(&server).await;
        let names = vec!["Vegapunk".to_string()];
        let err = canon
            .canonicalize(&names, "We use vegpunk for graphs.")
            .await
            .unwrap_err();
        assert!(matches!(err, CanonicalizeError::OutputRejected(_)));
    }

    #[tokio::test]
    async fn timeout_returns_http_transport_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        // server は応答に 5 秒以上かける一方、canon の timeout は test
        // 用に 1 秒に短縮する。
        Mock::given(method("POST"))
            .and(path("/models/gemini-3.5-flash:generateContent"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(5))
                    .set_body_json(serde_json::json!({
                        "candidates": [{"content": {"parts": [{"text": "ok"}]}}]
                    })),
            )
            .mount(&server)
            .await;
        let canon = GeminiCanonicalizer::new(
            "test-key".into(),
            "gemini-3.5-flash".into(),
            server.uri(),
            Duration::from_secs(1),
        )
        .unwrap();
        let names = vec!["Vegapunk".to_string()];
        let err = canon
            .canonicalize(&names, "We use vegpunk.")
            .await
            .unwrap_err();
        assert!(matches!(err, CanonicalizeError::Http(_)));
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
