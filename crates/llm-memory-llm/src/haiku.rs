use serde::Deserialize;
use crate::client::{AnthropicClient, CompleteRequest, Message};
use crate::error::LlmError;
use crate::prompts::HAIKU_CONCEPT_EXTRACT_SYSTEM;

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct AffectedConcepts {
    pub affected_existing: Vec<String>,
    pub new_concepts: Vec<String>,
}

pub struct HaikuExtractor<'a, C: AnthropicClient> {
    pub client: &'a C,
    pub model: String,
}

impl<'a, C: AnthropicClient> HaikuExtractor<'a, C> {
    pub async fn extract(
        &self,
        new_raws: &[(&str, &str)],
        existing_concepts: &[String],
    ) -> Result<AffectedConcepts, LlmError> {
        let user = serde_json::to_string(&serde_json::json!({
            "new_raws": new_raws.iter().map(|(t, c)| serde_json::json!({"title": t, "content": c})).collect::<Vec<_>>(),
            "existing_concepts": existing_concepts,
        }))?;

        let resp = self.client.complete(CompleteRequest {
            model: self.model.clone(),
            system: HAIKU_CONCEPT_EXTRACT_SYSTEM.into(),
            messages: vec![Message { role: "user".into(), content: user }],
            max_tokens: 1024,
        }).await?;

        let json_text = extract_json(&resp.content)
            .ok_or_else(|| LlmError::Api { status: 0, message: format!("haiku: no JSON in response: {}", resp.content) })?;
        let parsed: AffectedConcepts = serde_json::from_str(&json_text)?;
        Ok(parsed)
    }
}

pub(crate) fn extract_json(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end > start { Some(text[start..=end].to_string()) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockClient;

    #[tokio::test]
    async fn extract_parses_mock_response() {
        let mock = MockClient::new();
        mock.push_text(r#"{"affected_existing":["alpha"],"new_concepts":["beta"]}"#).await;
        let e = HaikuExtractor { client: &mock, model: "claude-haiku-4-5".into() };
        let r = e.extract(&[("t","c")], &["alpha".into()]).await.unwrap();
        assert_eq!(r.affected_existing, vec!["alpha".to_string()]);
        assert_eq!(r.new_concepts, vec!["beta".to_string()]);
    }

    #[tokio::test]
    async fn extract_handles_json_wrapped_in_prose() {
        let mock = MockClient::new();
        mock.push_text("Here is the JSON:\n{\"affected_existing\":[],\"new_concepts\":[\"x\"]}\nDone.").await;
        let e = HaikuExtractor { client: &mock, model: "h".into() };
        let r = e.extract(&[], &[]).await.unwrap();
        assert_eq!(r.new_concepts, vec!["x".to_string()]);
    }
}
