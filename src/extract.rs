use anyhow::{anyhow, Result};
use serde_json::json;

use crate::models::Extraction;

/// Claude API를 호출해 본문에서 모르는 단어와 베스트 문장을 추출한다.
/// known_terms에 있는 단어는 제외하도록 프롬프트로 지시한다.
pub struct Extractor {
    api_key: String,
    model: String,
    http: reqwest::Client,
}

impl Extractor {
    pub fn new(api_key: String, model: String) -> Self {
        Extractor {
            api_key,
            model,
            http: reqwest::Client::new(),
        }
    }

    pub async fn extract(&self, text: &str, known_terms: &[String]) -> Result<Extraction> {
        let known_list = if known_terms.is_empty() {
            "(없음)".to_string()
        } else {
            known_terms.join(", ")
        };

        let prompt = format!(
            "다음 글에서 학습 가치가 높은(난이도 있는) 단어와 인상적인 문장을 골라줘.\n\
             - 이미 아는 단어 목록에 있는 단어는 제외: {known}\n\
             - 각 단어는 term(원형), definition(한국어 뜻), example(글 속 예문)을 포함\n\
             - 베스트 문장은 text와 선정 이유(reason)를 포함\n\
             - 반드시 아래 JSON 스키마로만 응답:\n\
             {{\"words\":[{{\"term\":\"\",\"definition\":\"\",\"example\":\"\"}}],\
             \"sentences\":[{{\"text\":\"\",\"reason\":\"\"}}]}}\n\n\
             === 본문 ===\n{body}",
            known = known_list,
            body = text
        );

        let body = json!({
            "model": self.model,
            "max_tokens": 2048,
            "messages": [{ "role": "user", "content": prompt }]
        });

        let resp = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Claude API error {status}: {txt}"));
        }

        let v: serde_json::Value = resp.json().await?;
        let content = v["content"][0]["text"]
            .as_str()
            .ok_or_else(|| anyhow!("unexpected Claude response shape"))?;

        // 모델이 코드펜스로 감쌀 수 있으니 JSON 영역만 추출
        let json_str = extract_json_block(content);
        let extraction: Extraction = serde_json::from_str(&json_str)
            .map_err(|e| anyhow!("failed to parse extraction JSON: {e}; raw: {content}"))?;
        Ok(extraction)
    }
}

fn extract_json_block(s: &str) -> String {
    match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if b > a => s[a..=b].to_string(),
        _ => s.to_string(),
    }
}
