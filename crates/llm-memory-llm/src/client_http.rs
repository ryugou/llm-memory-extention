use crate::client::{CompleteRequest, CompleteResponse, LlmClient};
use crate::error::LlmError;
use async_trait::async_trait;
use gcp_auth::TokenProvider;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Vertex AI Gemini API client.
///
/// 認証は ADC (Application Default Credentials) 経由:
/// - 本番 (GCE VM): instance service account から自動取得
/// - ローカル開発: `gcloud auth application-default login` で取得した token
///
/// Vertex AI endpoint:
/// `https://{LOCATION}-aiplatform.googleapis.com/v1/projects/{PROJECT}/locations/{LOCATION}/publishers/google/models/{MODEL}:generateContent`
#[derive(Clone)]
pub struct VertexAi {
    project: String,
    location: String,
    token_provider: Arc<dyn TokenProvider>,
    base_url: String,
    http: Client,
}

impl VertexAi {
    pub async fn new(
        project: impl Into<String>,
        location: impl Into<String>,
    ) -> Result<Self, LlmError> {
        // HTTP status ではなく内部エラーだが、観測しやすいよう 503 を割り当てる。
        let token_provider = gcp_auth::provider().await.map_err(|e| LlmError::Api {
            status: 503,
            message: format!("ADC provider init failed: {e}"),
        })?;
        let location = location.into();
        let base_url = format!("https://{location}-aiplatform.googleapis.com");
        Ok(Self {
            project: project.into(),
            location,
            token_provider,
            base_url,
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("reqwest client build should not fail"),
        })
    }

    /// Override base URL (for tests / staging endpoints).
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    async fn access_token(&self) -> Result<String, LlmError> {
        let scopes = &["https://www.googleapis.com/auth/cloud-platform"];
        let token = self
            .token_provider
            .token(scopes)
            .await
            .map_err(|e| LlmError::Api {
                // HTTP status ではなく ADC 内部エラーだが、観測しやすいよう 503。
                status: 503,
                message: format!("ADC token fetch failed: {e}"),
            })?;
        Ok(token.as_str().to_string())
    }
}

/// Gemini generateContent payload subset.
#[derive(Serialize)]
struct GeminiPart<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct GeminiContent<'a> {
    role: &'a str,
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Serialize)]
struct GeminiSystemInstruction<'a> {
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig<'a> {
    max_output_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_mime_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_schema: Option<&'a serde_json::Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest<'a> {
    contents: Vec<GeminiContent<'a>>,
    system_instruction: GeminiSystemInstruction<'a>,
    generation_config: GeminiGenerationConfig<'a>,
}

#[derive(Deserialize)]
struct RespPart {
    text: Option<String>,
}

#[derive(Deserialize)]
struct RespContent {
    parts: Option<Vec<RespPart>>,
}

#[derive(Deserialize)]
struct RespCandidate {
    content: Option<RespContent>,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Option<Vec<RespCandidate>>,
    /// safety filter で prompt 自体がブロックされた場合、candidates が無く
    /// `promptFeedback.blockReason` のみが返る。
    #[serde(rename = "promptFeedback")]
    prompt_feedback: Option<PromptFeedback>,
}

#[derive(Deserialize)]
struct PromptFeedback {
    #[serde(rename = "blockReason")]
    block_reason: Option<String>,
}

#[async_trait]
impl LlmClient for VertexAi {
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, LlmError> {
        // Gemini が許容するのは "user" / "model" の 2 role のみ。
        // Anthropic 由来の "assistant" は "model" にマップ、その他はすべて "user"
        // として扱う (今回の用途では multi-turn を組まないので user 中心)。
        let contents: Vec<GeminiContent> = req
            .messages
            .iter()
            .map(|m| GeminiContent {
                role: if m.role == "assistant" || m.role == "model" {
                    "model"
                } else {
                    "user"
                },
                parts: vec![GeminiPart { text: &m.content }],
            })
            .collect();

        let (mime, schema) = match &req.response_schema {
            Some(s) => (Some("application/json"), Some(s)),
            None => (None, None),
        };

        let payload = GeminiRequest {
            contents,
            system_instruction: GeminiSystemInstruction {
                parts: vec![GeminiPart { text: &req.system }],
            },
            generation_config: GeminiGenerationConfig {
                max_output_tokens: req.max_tokens,
                response_mime_type: mime,
                response_schema: schema,
            },
        };

        let token = self.access_token().await?;
        let url = format!(
            "{}/v1/projects/{}/locations/{}/publishers/google/models/{}:generateContent",
            self.base_url, self.project, self.location, req.model
        );
        let res = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&payload)
            .send()
            .await?;
        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status: status.as_u16(),
                message: body,
            });
        }
        let resp: GeminiResponse = res.json().await?;
        let content = parse_gemini_response(resp)?;
        Ok(CompleteResponse { content })
    }
}

/// Vertex AI Gemini レスポンスを text に展開する純関数。
/// HTTP I/O から切り離してあるので、`finishReason` / `promptFeedback` /
/// 空 parts などの分岐を unit test で固定 JSON 経由で検証できる。
fn parse_gemini_response(resp: GeminiResponse) -> Result<String, LlmError> {
    // prompt 自体が safety filter で reject された場合
    if let Some(fb) = resp.prompt_feedback.as_ref() {
        if let Some(reason) = fb.block_reason.as_deref() {
            return Err(LlmError::Parse(format!(
                "vertex: prompt blocked by safety filter ({reason})"
            )));
        }
    }

    // 最初の candidate を取り出す (Gemini は通常 1 candidate のみだが、
    // 仕様上は複数返り得るので「先頭」を明示的に選ぶ)。
    let candidate = resp
        .candidates
        .and_then(|cs| cs.into_iter().next())
        .ok_or_else(|| LlmError::Parse("vertex: no candidate in response".into()))?;

    // finish_reason が `STOP` (正常終了) でない場合は明示的にエラー化。
    // - `MAX_TOKENS`: max_output_tokens に到達して切れた (JSON が途中で破損)
    // - `SAFETY`: safety filter に引っかかって部分出力
    // - `RECITATION` / `OTHER` 等
    // これらを通すと extract_json が「壊れた JSON」を後で失敗するだけ
    // なので、早めに reason を含めて Err にする。
    // 注: `finish_reason` が `None` のレスポンスは Vertex AI の挙動として
    //   想定外だが、互換性のため通している (空 parts ガードが後段で拾う)。
    //   将来 API 仕様が変わったら厳格化を検討する。
    if let Some(reason) = candidate.finish_reason.as_deref() {
        if reason != "STOP" {
            return Err(LlmError::Parse(format!(
                "vertex: candidate finishReason={reason} (output may be truncated or blocked)"
            )));
        }
    }

    // candidates の content / parts を text に展開。空なら Parse error。
    candidate
        .content
        .and_then(|c| c.parts)
        .and_then(|ps| {
            let joined: String = ps.into_iter().filter_map(|p| p.text).collect();
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        })
        .ok_or_else(|| LlmError::Parse("vertex: empty candidate content".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Message;
    use serde_json::json;

    #[test]
    fn gemini_request_payload_is_serializable() {
        let req = CompleteRequest {
            model: "gemini-2.5-flash".into(),
            system: "sys".into(),
            messages: vec![Message {
                role: "user".into(),
                content: "hi".into(),
            }],
            max_tokens: 100,
            response_schema: Some(json!({"type": "object"})),
        };
        let contents: Vec<GeminiContent> = req
            .messages
            .iter()
            .map(|m| GeminiContent {
                role: "user",
                parts: vec![GeminiPart { text: &m.content }],
            })
            .collect();
        let payload = GeminiRequest {
            contents,
            system_instruction: GeminiSystemInstruction {
                parts: vec![GeminiPart { text: &req.system }],
            },
            generation_config: GeminiGenerationConfig {
                max_output_tokens: req.max_tokens,
                response_mime_type: Some("application/json"),
                response_schema: req.response_schema.as_ref(),
            },
        };
        let s = serde_json::to_string(&payload).unwrap();
        assert!(s.contains("\"role\":\"user\""));
        assert!(s.contains("\"text\":\"hi\""));
        assert!(s.contains("\"systemInstruction\""));
        assert!(s.contains("\"responseMimeType\":\"application/json\""));
        assert!(s.contains("\"maxOutputTokens\":100"));
    }

    fn parse_from_json(raw: &str) -> Result<String, LlmError> {
        let resp: GeminiResponse = serde_json::from_str(raw).expect("test fixture parses");
        parse_gemini_response(resp)
    }

    #[test]
    fn parse_returns_text_on_normal_stop() {
        let raw = r#"{
          "candidates": [{
            "content": { "parts": [{"text": "hello world"}] },
            "finishReason": "STOP"
          }]
        }"#;
        assert_eq!(parse_from_json(raw).unwrap(), "hello world");
    }

    #[test]
    fn parse_picks_first_candidate_not_last() {
        // 万一複数 candidate が返っても「先頭」を選ぶこと (pop ではない)。
        let raw = r#"{
          "candidates": [
            { "content": { "parts": [{"text": "first"}] }, "finishReason": "STOP" },
            { "content": { "parts": [{"text": "second"}] }, "finishReason": "STOP" }
          ]
        }"#;
        assert_eq!(parse_from_json(raw).unwrap(), "first");
    }

    #[test]
    fn parse_rejects_prompt_blocked_by_safety_filter() {
        // prompt 自体が reject → candidates 不在、promptFeedback.blockReason のみ
        let raw = r#"{
          "promptFeedback": { "blockReason": "SAFETY" }
        }"#;
        let err = parse_from_json(raw).unwrap_err();
        match err {
            LlmError::Parse(msg) => assert!(msg.contains("SAFETY")),
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_max_tokens_truncation() {
        let raw = r#"{
          "candidates": [{
            "content": { "parts": [{"text": "{\"content\":\"truncated"}] },
            "finishReason": "MAX_TOKENS"
          }]
        }"#;
        let err = parse_from_json(raw).unwrap_err();
        match err {
            LlmError::Parse(msg) => assert!(msg.contains("MAX_TOKENS")),
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_safety_finish_reason() {
        let raw = r#"{
          "candidates": [{
            "content": { "parts": [{"text": "partial"}] },
            "finishReason": "SAFETY"
          }]
        }"#;
        let err = parse_from_json(raw).unwrap_err();
        match err {
            LlmError::Parse(msg) => assert!(msg.contains("SAFETY")),
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_empty_candidates() {
        let raw = r#"{ "candidates": [] }"#;
        assert!(matches!(parse_from_json(raw), Err(LlmError::Parse(_))));
    }

    #[test]
    fn parse_rejects_empty_parts() {
        let raw = r#"{
          "candidates": [{
            "content": { "parts": [] },
            "finishReason": "STOP"
          }]
        }"#;
        assert!(matches!(parse_from_json(raw), Err(LlmError::Parse(_))));
    }
}
