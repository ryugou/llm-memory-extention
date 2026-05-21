use crate::error::LlmError;
use async_trait::async_trait;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    /// `"user"` または `"model"`。互換性のため `"assistant"` も受け取り、
    /// Vertex AI 実装側で `"model"` にマップする (Anthropic 由来の表記対策)。
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct CompleteRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    /// Optional JSON Schema (Gemini `responseSchema`) for structured output.
    /// 指定すると、モデルは厳密にこの schema に従う JSON のみを返す。
    pub response_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct CompleteResponse {
    pub content: String,
}

/// LLM provider abstraction. Production impl is `VertexAi` (Vertex AI Gemini),
/// tests use `MockClient`.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, LlmError>;
}
