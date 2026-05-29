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
                // Gemini 2.5 Flash の max output。1 batch あたり最大 20 件程度の raws を
                // 集約するため、affected_existing + new_concepts の output と
                // safety margin に 8K 確保。1024 だと thinking tokens を含めて足りず
                // finishReason=MAX_TOKENS で truncate する事故が起きていた。
                max_tokens: 8192,
                response_schema: Some(extract_response_schema()),
                // extract phase は structured output で短い JSON を返すため thinking は要らない。
                // ただし thinking 無効化の可否はモデルごとに異なる:
                //   Flash 2.5: budget=0 OK / Pro 2.5: 128 以上必須 / 1.5 系: thinking 非対応
                // self.model に基づき安全な値を選び、`MODEL_EXTRACT` をどのモデルに
                // 切り替えても API 400 で落ちないようにする (1.5 系では `None` →
                // payload で thinkingConfig 自体が省略される)。
                thinking_budget: thinking_budget_for_model(&self.model),
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

/// Vertex AI の `thinkingConfig.thinkingBudget` をモデル名から選ぶ。
/// - `gemini-2.5-*-flash*`: 完全無効化可能なので `Some(0)`
/// - `gemini-2.5-*` (Pro 等): 完全無効化不可なので最小値 `Some(128)`
/// - それ以外 (`gemini-1.5-*` 等の thinking 非対応モデル): `None` を返し、payload で
///   `thinkingConfig` 自体を省略する (= Vertex AI が unknown field エラーを返さない)
///
/// extract / synth phase の caller 双方で再利用される。本関数は文字列ベース判定で、
/// 将来 Vertex AI の model naming 規約が変わった場合は判定ロジックを更新する必要がある。
pub(crate) fn thinking_budget_for_model(model: &str) -> Option<u32> {
    if !model.contains("2.5") {
        return None;
    }
    if model.contains("flash") {
        Some(0)
    } else {
        Some(128)
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
    async fn extract_passes_flash_token_budget_to_client() {
        // Vertex AI 上の Gemini 2.5 Flash 仕様を HaikuExtractor の production code
        // 経路で固定する。max_tokens=8192, thinking_budget=Some(0) (= thinking 無効化)
        // が必ず client に届くこと。Pro と Flash の budget 差分を test で守る。
        let mock = MockClient::new();
        mock.push_text(r#"{"affected_existing":[],"new_concepts":[]}"#)
            .await;
        let e = HaikuExtractor {
            client: &mock,
            model: "gemini-2.5-flash".into(),
        };
        e.extract(&[("t", "c")], &[]).await.unwrap();
        let captured = mock.captured().await;
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].max_tokens, 8192);
        assert_eq!(captured[0].thinking_budget, Some(0));
    }

    #[tokio::test]
    async fn extract_uses_safe_budget_when_model_overridden_to_pro() {
        // MODEL_EXTRACT を Pro に切り替えた構成 (Vertex AI 仕様で thinking off 不可)
        // でも HaikuExtractor が API 400 で落ちる Some(0) を送らないこと。
        // model 名に "flash" を含まないと最小値 128 にフォールバックする。
        let mock = MockClient::new();
        mock.push_text(r#"{"affected_existing":[],"new_concepts":[]}"#)
            .await;
        let e = HaikuExtractor {
            client: &mock,
            model: "gemini-2.5-pro".into(),
        };
        e.extract(&[("t", "c")], &[]).await.unwrap();
        let captured = mock.captured().await;
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].thinking_budget, Some(128));
    }

    #[test]
    fn thinking_budget_for_model_returns_zero_for_flash_25() {
        assert_eq!(thinking_budget_for_model("gemini-2.5-flash"), Some(0));
        assert_eq!(
            thinking_budget_for_model("gemini-2.5-flash-lite"),
            Some(0)
        );
    }

    #[test]
    fn thinking_budget_for_model_returns_128_for_pro_25() {
        assert_eq!(thinking_budget_for_model("gemini-2.5-pro"), Some(128));
    }

    #[test]
    fn thinking_budget_for_model_returns_none_for_pre_25() {
        // Gemini 1.5 系は thinking モードに対応しない。Some(0) を送ると Vertex AI が
        // unknown field エラーを返す可能性があるため None で payload omit する。
        assert_eq!(thinking_budget_for_model("gemini-1.5-flash"), None);
        assert_eq!(thinking_budget_for_model("gemini-1.5-pro"), None);
        assert_eq!(thinking_budget_for_model("unknown-model"), None);
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
