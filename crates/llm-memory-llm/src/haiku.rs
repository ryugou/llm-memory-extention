use crate::client::{CompleteRequest, LlmClient, Message};
use crate::error::LlmError;
use crate::prompts::EXTRACT_CONCEPTS_SYSTEM;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct AffectedConcepts {
    pub affected_existing: Vec<String>,
    pub new_concepts: Vec<String>,
}

/// Concept extractor (Gemini Flash 想定の軽量モデル経由)。
/// 互換性のため struct 名は `HaikuExtractor` のまま保持しているが、
/// 実体は LLM provider に依存しない (LlmClient 経由)。
pub struct HaikuExtractor<'a, C: LlmClient + ?Sized> {
    pub client: &'a C,
    pub model: String,
}

fn extract_response_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "affected_existing": {
                "type": "array",
                "items": {"type": "string"}
            },
            "new_concepts": {
                "type": "array",
                "items": {"type": "string"}
            }
        },
        "required": ["affected_existing", "new_concepts"]
    })
}

impl<'a, C: LlmClient + ?Sized> HaikuExtractor<'a, C> {
    pub async fn extract(
        &self,
        new_raws: &[(&str, &str)],
        existing_concepts: &[String],
    ) -> Result<AffectedConcepts, LlmError> {
        let user = serde_json::to_string(&json!({
            "new_raws": new_raws.iter().map(|(t, c)| json!({"title": t, "content": c})).collect::<Vec<_>>(),
            "existing_concepts": existing_concepts,
        }))?;

        let resp = self
            .client
            .complete(CompleteRequest {
                model: self.model.clone(),
                system: EXTRACT_CONCEPTS_SYSTEM.into(),
                messages: vec![Message {
                    role: "user".into(),
                    content: user,
                }],
                max_tokens: 1024,
                response_schema: Some(extract_response_schema()),
            })
            .await?;

        // structured output モードでも、たまに JSON の前後に空白や fence が
        // 残るプロバイダがあるので、念のため JSON 抽出を試みる。
        let json_text = extract_json(&resp.content).ok_or_else(|| {
            LlmError::Parse(format!("extract: no JSON in response: {}", resp.content))
        })?;
        let parsed: AffectedConcepts = serde_json::from_str(&json_text)?;
        Ok(parsed)
    }
}

pub(crate) fn extract_json(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end > start {
        Some(text[start..=end].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockClient;

    #[tokio::test]
    async fn extract_parses_mock_response() {
        let mock = MockClient::new();
        mock.push_text(r#"{"affected_existing":["alpha"],"new_concepts":["beta"]}"#)
            .await;
        let e = HaikuExtractor {
            client: &mock,
            model: "gemini-2.5-flash".into(),
        };
        let r = e.extract(&[("t", "c")], &["alpha".into()]).await.unwrap();
        assert_eq!(r.affected_existing, vec!["alpha".to_string()]);
        assert_eq!(r.new_concepts, vec!["beta".to_string()]);
    }

    #[tokio::test]
    async fn extract_handles_json_wrapped_in_prose() {
        let mock = MockClient::new();
        mock.push_text(
            "Here is the JSON:\n{\"affected_existing\":[],\"new_concepts\":[\"x\"]}\nDone.",
        )
        .await;
        let e = HaikuExtractor {
            client: &mock,
            model: "h".into(),
        };
        let r = e.extract(&[], &[]).await.unwrap();
        assert_eq!(r.new_concepts, vec!["x".to_string()]);
    }
}
