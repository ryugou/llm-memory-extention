use crate::client::{CompleteRequest, LlmClient, Message};
use crate::error::LlmError;
use crate::haiku::extract_json;
use crate::prompts::SYNTHESIZE_WIKI_SYSTEM;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
pub struct WikiSynth {
    pub content: String,
    pub source_refs: Vec<String>,
}

/// Wiki synthesizer (Gemini Pro 想定の本格モデル経由)。
/// 互換性のため struct 名は `SonnetSynthesizer` のまま保持しているが、
/// 実体は LLM provider に依存しない (LlmClient 経由)。
pub struct SonnetSynthesizer<'a, C: LlmClient + ?Sized> {
    pub client: &'a C,
    pub model: String,
}

pub struct SynthInput<'a> {
    pub concept: &'a str,
    pub existing_wiki: Option<&'a str>,
    pub raws: &'a [(String, String, String)], // (raw_id, title, content)
}

fn synth_response_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "content": {"type": "string"},
            "source_refs": {
                "type": "array",
                "items": {"type": "string"}
            }
        },
        "required": ["content", "source_refs"]
    })
}

impl<'a, C: LlmClient + ?Sized> SonnetSynthesizer<'a, C> {
    pub async fn synthesize(&self, input: SynthInput<'_>) -> Result<WikiSynth, LlmError> {
        let user = serde_json::to_string(&json!({
            "concept": input.concept,
            "existing_wiki": input.existing_wiki,
            "raws": input.raws.iter().map(|(id, t, c)| json!({"id": id, "title": t, "content": c})).collect::<Vec<_>>(),
        }))?;

        let resp = self
            .client
            .complete(CompleteRequest {
                model: self.model.clone(),
                system: SYNTHESIZE_WIKI_SYSTEM.into(),
                messages: vec![Message {
                    role: "user".into(),
                    content: user,
                }],
                max_tokens: 8192,
                response_schema: Some(synth_response_schema()),
            })
            .await?;

        let json_text = extract_json(&resp.content).ok_or_else(|| {
            LlmError::Parse(format!("synthesize: no JSON in response: {}", resp.content))
        })?;
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
        mock.push_text(r##"{"content":"# vegapunk\nGraphRAG...","source_refs":["01HJ1"]}"##)
            .await;
        let s = SonnetSynthesizer {
            client: &mock,
            model: "gemini-2.5-pro".into(),
        };
        let raws = vec![(
            "01HJ1".to_string(),
            "title".to_string(),
            "content".to_string(),
        )];
        let r = s
            .synthesize(SynthInput {
                concept: "vegapunk",
                existing_wiki: None,
                raws: &raws,
            })
            .await
            .unwrap();
        assert!(r.content.starts_with("# vegapunk"));
        assert_eq!(r.source_refs, vec!["01HJ1".to_string()]);
    }
}
