use crate::client::{CompleteRequest, LlmClient, Message};
use crate::error::LlmError;
use crate::haiku::{extract_json, thinking_budget_for_model};
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
                // Gemini 2.5 Pro の max output。wiki content (Markdown) + source_refs
                // の JSON 出力で複数 raws を集約する synth phase は output が長くなる。
                // 8K だと thinking tokens を含めて足りず MAX_TOKENS truncate が起きていた。
                // Vertex AI 上の 2.5 Pro は max_output_tokens の上限が 65536 だが、
                // thinking_budget の上限と揃えてコスト/品質のトレードオフを取り 32K に。
                max_tokens: 32768,
                response_schema: Some(synth_response_schema()),
                // synth phase は thinking を最小化したい (= structured JSON 出力)。
                // thinking 無効化の可否はモデルごとに異なる:
                //   Pro 2.5: 128 最小 / Flash 2.5: 0 無効化可 / 1.5 系: thinking 非対応
                // self.model に応じて安全な値を選ぶ。1.5 系では None → payload で
                // thinkingConfig 自体が省略される。
                // NOTE: max_tokens=32768 は Gemini 2.5 Pro 想定値。`MODEL_SYNTH` を
                // gemini-1.5-pro 等 (output 上限 8192) に override すると別途
                // 拒否される可能性があるが、本 PR スコープ外 (= model 別 validation
                // は別 PR で対応予定)。
                thinking_budget: thinking_budget_for_model(&self.model),
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

    #[tokio::test]
    async fn synthesize_passes_pro_token_budget_to_client() {
        // Vertex AI 上の Gemini 2.5 Pro 仕様 (Round 1 で誤って 0 を渡した事故の
        // 再発防止) を SonnetSynthesizer の production code 経路で固定する。
        // max_tokens=32768, thinking_budget=Some(128) が必ず client に届くこと。
        let mock = MockClient::new();
        mock.push_text(r##"{"content":"x","source_refs":[]}"##)
            .await;
        let s = SonnetSynthesizer {
            client: &mock,
            model: "gemini-2.5-pro".into(),
        };
        let raws = vec![("01HJ1".to_string(), "t".to_string(), "c".to_string())];
        s.synthesize(SynthInput {
            concept: "c",
            existing_wiki: None,
            raws: &raws,
        })
        .await
        .unwrap();
        let captured = mock.captured().await;
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].max_tokens, 32768);
        assert_eq!(captured[0].thinking_budget, Some(128));
    }

    #[tokio::test]
    async fn synthesize_uses_safe_budget_when_model_overridden_to_flash() {
        // MODEL_SYNTH を Flash に切り替えた構成でも SonnetSynthesizer が
        // Pro 用 Some(128) を hard-code せず、Flash で thinking 完全無効化
        // (Some(0)) にフォールバックすること。コスト最小化と API 互換の両立。
        let mock = MockClient::new();
        mock.push_text(r##"{"content":"x","source_refs":[]}"##)
            .await;
        let s = SonnetSynthesizer {
            client: &mock,
            model: "gemini-2.5-flash".into(),
        };
        let raws = vec![("01HJ1".to_string(), "t".to_string(), "c".to_string())];
        s.synthesize(SynthInput {
            concept: "c",
            existing_wiki: None,
            raws: &raws,
        })
        .await
        .unwrap();
        let captured = mock.captured().await;
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].thinking_budget, Some(0));
    }
}
