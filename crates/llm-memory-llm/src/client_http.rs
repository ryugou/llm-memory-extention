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
        let token_provider = gcp_auth::provider().await.map_err(|e| LlmError::Api {
            status: 0,
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
                status: 0,
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
}

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Option<Vec<RespCandidate>>,
}

#[async_trait]
impl LlmClient for VertexAi {
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, LlmError> {
        // Gemini は "user" / "model" の 2 role のみ。input message は user 固定で扱う。
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
        let content = resp
            .candidates
            .and_then(|cs| cs.into_iter().next())
            .and_then(|c| c.content)
            .and_then(|c| c.parts)
            .map(|ps| {
                ps.into_iter()
                    .filter_map(|p| p.text)
                    .collect::<Vec<_>>()
                    .join("")
            })
            .ok_or_else(|| LlmError::Parse("vertex: empty candidate content".into()))?;
        Ok(CompleteResponse { content })
    }
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
}
