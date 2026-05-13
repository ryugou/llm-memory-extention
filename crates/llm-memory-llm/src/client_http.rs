use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use crate::client::{AnthropicClient, CompleteRequest, CompleteResponse};
use crate::error::LlmError;

#[derive(Clone)]
pub struct AnthropicHttp {
    api_key: String,
    base_url: String,
    http: Client,
}

impl AnthropicHttp {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com".into(),
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("reqwest client build should not fail"),
        }
    }

    /// Override base URL (for testing with a mock server).
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    system: &'a str,
    messages: Vec<ApiMessage<'a>>,
    max_tokens: u32,
}

#[derive(Deserialize)]
struct ApiContent {
    text: String,
}

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ApiContent>,
}

#[async_trait]
impl AnthropicClient for AnthropicHttp {
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, LlmError> {
        let msgs: Vec<ApiMessage> = req
            .messages
            .iter()
            .map(|m| ApiMessage { role: m.role.as_str(), content: &m.content })
            .collect();
        let payload = ApiRequest {
            model: &req.model,
            system: &req.system,
            messages: msgs,
            max_tokens: req.max_tokens,
        };
        let res = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&payload)
            .send()
            .await?;
        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            return Err(LlmError::Api { status: status.as_u16(), message: body });
        }
        let resp: ApiResponse = res.json().await?;
        let content = resp.content.into_iter().map(|c| c.text).collect::<Vec<_>>().join("");
        Ok(CompleteResponse { content })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Message;

    // 実 API 鍵が要らない、コンパイル可能性 + URL 構築の確認のみ。
    #[test]
    fn build_request_payload_is_serializable() {
        let req = CompleteRequest {
            model: "haiku".into(),
            system: "sys".into(),
            messages: vec![Message { role: "user".into(), content: "hi".into() }],
            max_tokens: 100,
        };
        let payload = ApiRequest {
            model: &req.model,
            system: &req.system,
            messages: req.messages.iter().map(|m| ApiMessage { role: m.role.as_str(), content: &m.content }).collect(),
            max_tokens: req.max_tokens,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"model\":\"haiku\""));
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"hi\""));
    }

    #[test]
    fn anthropic_http_can_be_constructed() {
        let c = AnthropicHttp::new("sk-test");
        // base_url は private なので公開状態確認は不可。型が出来ることだけ確認。
        let _ = c.with_base_url("http://localhost:8080");
    }
}
