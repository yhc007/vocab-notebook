use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::json;

use crate::models::{Extraction, MindMap, RootAnalysis, Summary};

/// 추출 청크 하나의 최대 문자 수(Claude 호출당 크기를 제한해 응답을 안정화).
const CHUNK_CHARS: usize = 12_000;
/// 청크 병렬 호출 동시성 상한(429/레이트리밋 회피, Cloudflare 100s 타임아웃 대비).
const CONCURRENCY: usize = 4;
/// 추출을 실제로 돌릴 최대 청크 수(비용 폭주 방지 백스톱). 본문 자체는 전체 저장된다.
const MAX_CHUNKS: usize = 30;

/// Claude API를 호출해 본문에서 모르는 단어와 베스트 문장을 추출한다.
/// known_terms에 있는 단어는 제외하도록 프롬프트로 지시한다.
#[derive(Clone)]
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

        let content = self.message(&prompt, 4096).await?;
        let json_str = extract_json_block(&content);
        serde_json::from_str(&json_str)
            .map_err(|e| anyhow!("failed to parse extraction JSON: {e}; raw: {content}"))
    }

    /// 긴 본문을 청크로 나눠 **병렬로** 추출한 뒤 결과를 병합(단어/문장 중복 제거)한다.
    /// - 청크가 하나뿐이면 `extract`와 동일하게 동작.
    /// - 청크는 동시성 제한(CONCURRENCY) 하에 병렬 호출 → 긴 문서도 대기시간을 줄인다.
    /// - 일부 청크가 실패해도 성공한 청크의 결과는 반환(전부 실패일 때만 에러).
    pub async fn extract_chunked(&self, text: &str, known_terms: &[String]) -> Result<Extraction> {
        let mut chunks = split_chunks(text, CHUNK_CHARS);
        if chunks.len() <= 1 {
            return self.extract(text, known_terms).await;
        }
        if chunks.len() > MAX_CHUNKS {
            tracing::warn!(
                "본문이 매우 깁니다: 청크 {}개 중 {}개까지만 추출합니다(본문은 전체 저장됨).",
                chunks.len(),
                MAX_CHUNKS
            );
            chunks.truncate(MAX_CHUNKS);
        }

        // 동시성 제한 병렬 호출. 각 청크 추출은 독립적이라 병렬로 안전하다.
        let sem = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
        let mut set = tokio::task::JoinSet::new();
        for (i, chunk) in chunks.into_iter().enumerate() {
            let this = self.clone();
            let known = known_terms.to_vec();
            let sem = sem.clone();
            set.spawn(async move {
                let _permit = sem.acquire_owned().await.expect("semaphore not closed");
                (i, this.extract(&chunk, &known).await)
            });
        }

        // 완료 순서가 뒤섞이므로 청크 인덱스로 다시 정렬해 원문 순서를 보존.
        let mut ordered: Vec<(usize, Result<Extraction>)> = Vec::new();
        while let Some(res) = set.join_next().await {
            ordered.push(res.map_err(|e| anyhow!("chunk task join error: {e}"))?);
        }
        ordered.sort_by_key(|(i, _)| *i);

        let mut words = Vec::new();
        let mut sentences = Vec::new();
        let mut seen_terms: HashSet<String> = HashSet::new();
        let mut seen_sents: HashSet<String> = HashSet::new();
        let mut ok = 0usize;
        let mut first_err: Option<anyhow::Error> = None;

        for (i, r) in ordered {
            match r {
                Ok(ex) => {
                    ok += 1;
                    for w in ex.words {
                        // term 소문자 기준으로 청크 간 중복 제거(첫 등장 유지).
                        if seen_terms.insert(w.term.trim().to_lowercase()) {
                            words.push(w);
                        }
                    }
                    for s in ex.sentences {
                        if seen_sents.insert(s.text.trim().to_string()) {
                            sentences.push(s);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("청크 {i} 추출 실패(건너뜀): {e}");
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }

        // 전부 실패면 에러, 하나라도 성공하면 부분 결과라도 반환한다.
        if ok == 0 {
            return Err(first_err.unwrap_or_else(|| anyhow!("추출 결과가 없습니다")));
        }
        Ok(Extraction { words, sentences })
    }

    /// 단어의 어근을 분석한다(접두사/어근/접미사 + 어원 + 관련어 + 연상).
    pub async fn analyze_roots(&self, term: &str) -> Result<RootAnalysis> {
        let prompt = format!(
            "영어 단어 \"{term}\"을(를) 어원 기반으로 분석해줘.\n\
             - 접두사/어근/접미사로 분해(parts): 각 piece(조각), kind(prefix|root|suffix), meaning(한국어 뜻)\n\
             - origin: 어원(라틴/그리스 등)을 한국어로 한 줄\n\
             - related: 같은 어근을 공유하는 관련 영어 단어 3~6개\n\
             - mnemonic: 뜻을 기억하게 돕는 한국어 한 줄 연상\n\
             - 반드시 아래 JSON 스키마로만 응답:\n\
             {{\"parts\":[{{\"piece\":\"\",\"kind\":\"\",\"meaning\":\"\"}}],\
             \"origin\":\"\",\"related\":[\"\"],\"mnemonic\":\"\"}}",
        );

        let content = self.message(&prompt, 1024).await?;
        let json_str = extract_json_block(&content);
        serde_json::from_str(&json_str)
            .map_err(|e| anyhow!("failed to parse roots JSON: {e}; raw: {content}"))
    }

    /// 기사 전체 구조를 마인드맵(중앙 제목 + 주요 섹션/서브헤딩 + 핵심 키워드)으로 요약.
    pub async fn analyze_mindmap(&self, article: &str) -> Result<MindMap> {
        let prompt = format!(
            "다음 글의 전체 구조를 한눈에 파악할 수 있는 마인드맵으로 요약해줘.\n\
             - title: 중심 주제(기사 핵심을 짧게)\n\
             - branches: 주요 섹션/서브헤딩 4~6개. 각 heading은 짧게(최대 15자 내외), \
               keywords는 그 섹션의 핵심 키워드 3~5개(각각 짧은 단어/구)\n\
             - 설명 없이 구조/키워드만. 한국어로.\n\
             - 반드시 아래 JSON 스키마로만 응답:\n\
             {{\"title\":\"\",\"branches\":[{{\"heading\":\"\",\"keywords\":[\"\"]}}]}}\n\n\
             === 본문 ===\n{body}",
            body = article
        );

        let content = self.message(&prompt, 1024).await?;
        let json_str = extract_json_block(&content);
        serde_json::from_str(&json_str)
            .map_err(|e| anyhow!("failed to parse mindmap JSON: {e}; raw: {content}"))
    }

    /// 기사/논문을 한국어로 정리해 블로그용(마크다운) + X 스레드 초안을 만든다.
    pub async fn summarize_korean(&self, article: &str) -> Result<Summary> {
        let prompt = format!(
            "다음 글을 한국어로 정리해서 (1) 블로그 글과 (2) X(트위터) 스레드 초안을 만들어줘.\n\
             - blog: 마크다운. 첫 줄은 '# 제목', 이어서 '## 소제목'과 핵심 내용을 간결하고 \
               읽기 좋게. 군더더기 없이 핵심·인사이트 위주로.\n\
             - thread: 5~8개 트윗의 배열. 첫 트윗은 눈길을 끄는 훅. 각 트윗은 한국어 130자 이내로 \
               짧고 명확하게. 번호(1/n)는 붙이지 말 것(화면에서 자동 표시).\n\
             - 원문에 없는 사실을 지어내지 말 것.\n\
             - 반드시 아래 JSON 스키마로만 응답:\n\
             {{\"blog\":\"\",\"thread\":[\"\"]}}\n\n\
             === 본문 ===\n{body}",
            body = article
        );

        let content = self.message(&prompt, 3000).await?;
        let json_str = extract_json_block(&content);
        serde_json::from_str(&json_str)
            .map_err(|e| anyhow!("failed to parse summary JSON: {e}; raw: {content}"))
    }

    /// Claude Messages API에 프롬프트를 보내고 첫 텍스트 블록을 돌려준다.
    async fn message(&self, prompt: &str, max_tokens: u32) -> Result<String> {
        let body = json!({
            "model": self.model,
            "max_tokens": max_tokens,
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
        v["content"][0]["text"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("unexpected Claude response shape"))
    }
}

fn extract_json_block(s: &str) -> String {
    match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if b > a => s[a..=b].to_string(),
        _ => s.to_string(),
    }
}

/// 긴 본문을 최대 `max`자 청크로 나눈다. 가능한 한 문단(`\n\n`) 경계에서 자르고,
/// 한 문단이 `max`보다 크면 문자 경계로 강제 분할한다(문맥이 문단 안에 최대한 보존됨).
fn split_chunks(text: &str, max: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize; // cur의 문자 수(캐시)

    for para in text.split("\n\n") {
        let plen = para.chars().count();

        // 한 문단이 통째로 max를 넘으면: 현재 청크를 확정하고 문단을 문자 단위로 분할.
        if plen > max {
            if !cur.is_empty() {
                chunks.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            let mut piece = String::new();
            let mut piece_len = 0usize;
            for ch in para.chars() {
                if piece_len >= max {
                    chunks.push(std::mem::take(&mut piece));
                    piece_len = 0;
                }
                piece.push(ch);
                piece_len += 1;
            }
            // 남은 조각은 다음 문단과 이어 붙일 수 있게 cur로 넘긴다.
            if !piece.is_empty() {
                cur = piece;
                cur_len = piece_len;
            }
            continue;
        }

        // 현재 청크에 이 문단을 더하면 max 초과 → 청크 확정 후 새로 시작.
        if cur_len > 0 && cur_len + 2 + plen > max {
            chunks.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        if cur_len > 0 {
            cur.push_str("\n\n");
            cur_len += 2;
        }
        cur.push_str(para);
        cur_len += plen;
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::split_chunks;

    #[test]
    fn short_text_is_single_chunk() {
        let c = split_chunks("hello world", 100);
        assert_eq!(c, vec!["hello world".to_string()]);
    }

    #[test]
    fn splits_on_paragraph_boundary_under_max() {
        // 두 문단, 각각 6자("aaaaaa"/"bbbbbb"). max=8이면 한 청크에 둘 다 못 들어감.
        let text = "aaaaaa\n\nbbbbbb";
        let c = split_chunks(text, 8);
        assert_eq!(c, vec!["aaaaaa".to_string(), "bbbbbb".to_string()]);
    }

    #[test]
    fn hard_splits_oversized_paragraph() {
        let text = "x".repeat(25); // 문단 하나가 max=10 초과
        let c = split_chunks(&text, 10);
        assert_eq!(c.len(), 3); // 10 + 10 + 5
        assert_eq!(c.iter().map(|s| s.chars().count()).sum::<usize>(), 25);
    }

    #[test]
    fn preserves_all_content() {
        let text = "문단 하나.\n\n두 번째 문단입니다.\n\n세 번째.";
        let joined = split_chunks(text, 6).concat();
        // 청크를 이으면 문단 구분(\n\n)만 빠질 수 있으나 글자는 모두 보존.
        let orig: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let got: String = joined.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig, got);
    }
}
