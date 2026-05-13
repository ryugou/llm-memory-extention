use crate::client::{AnthropicClient, CompleteRequest, CompleteResponse};
use crate::error::LlmError;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Default)]
pub struct MockClient {
    pub responses: Arc<Mutex<Vec<Result<CompleteResponse, String>>>>,
    pub captured: Arc<Mutex<Vec<CompleteRequest>>>,
}

impl MockClient {
    pub fn new() -> Self {
        Self::default()
    }
    pub async fn push_text(&self, s: impl Into<String>) {
        self.responses
            .lock()
            .await
            .push(Ok(CompleteResponse { content: s.into() }));
    }
    pub async fn push_error(&self, msg: impl Into<String>) {
        self.responses.lock().await.push(Err(msg.into()));
    }
    pub async fn captured(&self) -> Vec<CompleteRequest> {
        self.captured.lock().await.clone()
    }
}

#[async_trait]
impl AnthropicClient for MockClient {
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, LlmError> {
        self.captured.lock().await.push(req);
        let mut responses = self.responses.lock().await;
        if responses.is_empty() {
            return Err(LlmError::Api {
                status: 500,
                message: "mock: no response queued".into(),
            });
        }
        let resp = responses.remove(0);
        match resp {
            Ok(r) => Ok(r),
            Err(e) => Err(LlmError::Api {
                status: 500,
                message: e,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::AnthropicClient;

    #[tokio::test]
    async fn mock_returns_pushed_responses_in_order() {
        let m = MockClient::new();
        m.push_text("hello").await;
        let r = m
            .complete(CompleteRequest {
                model: "x".into(),
                system: "".into(),
                messages: vec![],
                max_tokens: 10,
            })
            .await
            .unwrap();
        assert_eq!(r.content, "hello");
        assert_eq!(m.captured().await.len(), 1);
    }

    #[tokio::test]
    async fn mock_returns_error_when_no_response_queued() {
        let m = MockClient::new();
        let r = m
            .complete(CompleteRequest {
                model: "x".into(),
                system: "".into(),
                messages: vec![],
                max_tokens: 10,
            })
            .await;
        assert!(r.is_err());
    }
}
