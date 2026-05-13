use async_trait::async_trait;
use crate::error::LlmError;

#[derive(Debug, Clone)]
pub struct Message {
    pub role: &'static str,    // "user" or "assistant"
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct CompleteRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct CompleteResponse {
    pub content: String,
}

#[async_trait]
pub trait AnthropicClient: Send + Sync {
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, LlmError>;
}
