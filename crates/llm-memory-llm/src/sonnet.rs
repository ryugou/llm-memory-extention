use serde::Deserialize;
use crate::client::{AnthropicClient, CompleteRequest, Message};
use crate::error::LlmError;
use crate::haiku::extract_json;
use crate::prompts::SONNET_WIKI_SYNTHESIZE_SYSTEM;

#[derive(Debug, Deserialize)]
pub struct WikiSynth {
    pub content: String,
    pub source_refs: Vec<String>,
}

pub struct SonnetSynthesizer<'a, C: AnthropicClient> {
    pub client: &'a C,
    pub model: String,
}

pub struct SynthInput<'a> {
    pub concept: &'a str,
    pub existing_wiki: Option<&'a str>,
    pub raws: &'a [(String, String, String)],   // (raw_id, title, content)
}

impl<'a, C: AnthropicClient> SonnetSynthesizer<'a, C> {
    pub async fn synthesize(&self, input: SynthInput<'_>) -> Result<WikiSynth, LlmError> {
        let user = serde_json::to_string(&serde_json::json!({
            "concept": input.concept,
            "existing_wiki": input.existing_wiki,
            "raws": input.raws.iter().map(|(id, t, c)| serde_json::json!({"id": id, "title": t, "content": c})).collect::<Vec<_>>(),
        }))?;

        let resp = self.client.complete(CompleteRequest {
            model: self.model.clone(),
            system: SONNET_WIKI_SYNTHESIZE_SYSTEM.into(),
            messages: vec![Message { role: "user".into(), content: user }],
            max_tokens: 8192,
        }).await?;

        let json_text = extract_json(&resp.content)
            .ok_or_else(|| LlmError::Parse(format!("sonnet: no JSON in response: {}", resp.content)))?;
        Ok(serde_json::from_str(&json_text)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockClient;

    #[tokio::test]
    async fn synthesize_parses_mock_response() {
        let mock = MockClient::new();
        mock.push_text(r##"{"content":"# vegapunk\nGraphRAG...","source_refs":["01HJ1"]}"##).await;
        let s = SonnetSynthesizer { client: &mock, model: "claude-sonnet-4-6".into() };
        let raws = vec![("01HJ1".to_string(), "title".to_string(), "content".to_string())];
        let r = s.synthesize(SynthInput { concept: "vegapunk", existing_wiki: None, raws: &raws }).await.unwrap();
        assert!(r.content.starts_with("# vegapunk"));
        assert_eq!(r.source_refs, vec!["01HJ1".to_string()]);
    }
}
