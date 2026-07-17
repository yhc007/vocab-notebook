use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::json;

use crate::models::{
    ChunkedText, Extraction, MindMap, PointDetail, RefinedWords, RootAnalysis, SentenceGrammar,
    Summary, Word,
};

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
    /// Stage 2 정제 패스 on/off. `EXTRACT_REFINE=1`일 때만 켜진다(기본 off).
    refine: bool,
}

impl Extractor {
    pub fn new(api_key: String, model: String) -> Self {
        Extractor {
            api_key,
            model,
            http: reqwest::Client::new(),
            // 청크당 +1 호출이 붙는 선택 기능이라 명시적으로 켤 때만 동작.
            refine: std::env::var("EXTRACT_REFINE").as_deref() == Ok("1"),
        }
    }

    pub async fn extract(&self, text: &str, known_terms: &[String]) -> Result<Extraction> {
        let known_list = if known_terms.is_empty() {
            "(없음)".to_string()
        } else {
            known_terms.join(", ")
        };

        // 문맥 의존 해석(다의어·구 표현·비유)을 놓치지 않도록, 출력 전에
        // 아래 항목을 "내부적으로" 판단하게 지시한다(단계적 추론을 프롬프트에 내장).
        // 출력은 여전히 JSON만 — 스키마와 파서(extract_json_block)는 그대로다.
        let prompt = format!(
            "다음 글에서 학습 가치가 높은(난이도 있는) 단어/표현과 인상적인 문장을 골라줘.\n\
             - 이미 아는 단어 목록에 있는 단어는 제외: {known}\n\n\
             먼저(출력하지 말고 속으로) 각 후보에 대해 다음을 판단하라:\n\
             1) 이 문맥에서의 실제 의미가 사전 대표뜻과 다른가? → definition은 반드시 \
                '이 문맥에서의 뜻'으로 (대표뜻이 아니라).\n\
             2) 단어가 아니라 구 단위 표현인가(관용구·구동사·연어, 예: take on, by and large)? \
                → 그렇다면 구 전체를 term으로.\n\
             3) 비유·완곡·전문용어라 직역이 오해를 부르는가? → definition에서 풀어서 설명.\n\
             그다음 아래 형식으로만 응답:\n\
             - 각 단어/표현: term(원형 또는 구 전체), definition(위 판단을 반영한 한국어 뜻), \
               example(글 속 실제 예문)\n\
             - 베스트 문장: text(원문 그대로)와 reason(왜 학습가치가 있는지: 구조·표현·통찰 관점에서 구체적으로)\n\
             - 반드시 아래 JSON 스키마로만 응답(추론 과정은 출력하지 말 것):\n\
             {{\"words\":[{{\"term\":\"\",\"definition\":\"\",\"example\":\"\"}}],\
             \"sentences\":[{{\"text\":\"\",\"reason\":\"\"}}]}}\n\n\
             === 본문 ===\n{body}",
            known = known_list,
            body = text
        );

        let content = self.message(&prompt, 4096).await?;
        let json_str = extract_json_block(&content);
        let mut ex: Extraction = serde_json::from_str(&json_str)
            .map_err(|e| anyhow!("failed to parse extraction JSON: {e}; raw: {content}"))?;

        // Stage 2(선택): EXTRACT_REFINE=1이면 definition을 문맥 기준으로 한 번 더 교정.
        // 정제가 실패해도 1차 결과를 살리도록 에러는 삼키고 원본을 유지한다.
        if self.refine && !ex.words.is_empty() {
            tracing::info!("definition 정제 패스 실행: 단어 {}개", ex.words.len());
            match self.refine_definitions(&ex.words, text).await {
                Ok(refined) => ex.words = refined,
                Err(e) => tracing::warn!("definition 정제 실패(1차 결과 유지): {e}"),
            }
        }
        Ok(ex)
    }

    /// Stage 2 정제 패스: 1차 추출된 단어들의 definition이 주어진 문맥에서 정확한지
    /// 검토·교정한다. term/example/id는 원본을 그대로 유지하고 **definition만** 교체한다.
    /// 청크 본문(`context`)을 함께 넘겨 문맥 정합성을 판단하게 한다(청크당 +1 호출).
    pub async fn refine_definitions(&self, words: &[Word], context: &str) -> Result<Vec<Word>> {
        // term + 현재 definition만 보내 교정을 받는다(example은 우리가 원본으로 유지).
        let list = words
            .iter()
            .enumerate()
            .map(|(i, w)| format!("{}. {} = {}", i + 1, w.term, w.definition))
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = format!(
            "아래는 어떤 글에서 뽑은 영어 단어/표현과 그 한국어 뜻(definition)이다.\n\
             각 항목의 definition이 주어진 '문맥'에서 정확한지 검토하고, 부정확하거나 \
             사전 대표뜻에 그친 것은 이 문맥에 맞는 뜻으로 고쳐라. 이미 정확하면 그대로 두라.\n\
             - term은 절대 바꾸지 말 것(입력과 동일한 문자열로).\n\
             - definition만 교정하고 한국어로 쓸 것.\n\
             - 반드시 아래 JSON 스키마로만 응답:\n\
             {{\"words\":[{{\"term\":\"\",\"definition\":\"\"}}]}}\n\n\
             === 단어 목록 ===\n{list}\n\n\
             === 문맥 ===\n{context}",
        );

        let content = self.message(&prompt, 2048).await?;
        let json_str = extract_json_block(&content);
        let refined: RefinedWords = serde_json::from_str(&json_str)
            .map_err(|e| anyhow!("failed to parse refine JSON: {e}; raw: {content}"))?;

        // term(소문자) 기준으로 교정된 definition을 원본에 병합.
        // 모델이 순서를 바꾸거나 일부를 빠뜨려도 원본 term/example/id는 온전히 보존된다.
        let map: HashMap<String, String> = refined
            .words
            .into_iter()
            .map(|w| (w.term.trim().to_lowercase(), w.definition))
            .collect();
        let merged = words
            .iter()
            .map(|w| {
                let mut w = w.clone();
                if let Some(def) = map.get(&w.term.trim().to_lowercase()) {
                    if !def.trim().is_empty() {
                        w.definition = def.clone();
                    }
                }
                w
            })
            .collect();
        Ok(merged)
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

    /// 베스트 문장을 '문법 강의 도입'용 그래프로 분해한다.
    /// 노드(토큰/구·문법역할·품사) + 엣지(head→dependent 관계) + 강의 포인트를 JSON으로 받는다.
    /// 문장 텍스트를 되뱉으므로 값 안 큰따옴표는 「 」로 치환하도록 지시(JSON 파싱 깨짐 방지).
    pub async fn analyze_grammar(&self, sentence: &str) -> Result<SentenceGrammar> {
        let prompt = format!(
            "다음 영어 문장을 '문법 강의의 도입부'로 삼을 수 있게 구조 그래프로 분해하라.\n\
             - nodes: 문장을 의미 단위(단어/구)로 나눈 노드. 각 node는 id(\"n1\",\"n2\"..), \
               text(원문 조각 그대로), role(한국어 문법 역할: 주어/술어/목적어/보어/수식어/접속 등), \
               pos(품사나 구 유형: 명사구/동사/전치사구 등), \
               ko(그 조각의 짧은 우리말 뜻 — 초등학생도 이해할 쉬운 한국어).\n\
             - edges: head→dependent 문법 관계. from/to는 node id, label은 관계명(주어·목적어·\
               수식·종속절·병렬 등 한국어). 루트(문장의 주절 본동사) 하나를 제외하고 모든 노드는 \
               정확히 하나의 head에 연결하라(수식어·전치사·관계사·관사·접속사 포함). 고립된 \
               노드를 남기지 말 것 — 트리로도 그릴 수 있어야 한다.\n\
             - 계층은 '큰 덩어리 → 작은 조각' 순으로: 주어·목적어·보어·종속절·관계절 같은 명사·절 \
               성분은 각각 그 구/절 '전체'를 한 노드로 만들고(핵심 단어만 떼어 최상위로 올리지 말 것), \
               그 안의 세부 단어(관사·전치사·형용사·관계절 등)는 그 구 노드의 하위 노드로 연결하라. \
               예: 주어는 'Apple's lawsuit against OpenAI' 전체가 한 노드이고 그 아래 'Apple's', \
               'against OpenAI'가 자식. 목적어도 딸린 관계절까지 포함한 구 전체를 한 노드로 두고 세부를 편다.\n\
             - 단, 술어 노드에는 동사(구)만 넣고 부사 등 수식어는 절대 포함하지 말 것(부사는 별도 \
               수식 성분 노드로). 또 같은 범위를 두 노드로 겹쳐 만들지 말 것 — 예를 들어 \
               'sharply escalates'와 'escalates'를 동시에 만들지 말고, 술어는 'escalates', \
               'sharply'는 별도 수식어로 둔다.\n\
             - summary: 이 문장의 구조를 한 줄로(주절/종속절, 핵심 구문).\n\
             - points: 이 문장으로 가르칠 핵심 문법 포인트 2~3개(한국어, 강의 시작용). \
               각 포인트는 2문장 이내로 간결하게.\n\
             - 인용이 필요하면 문자열 안에서 큰따옴표(\") 대신 「 」를 쓰고, 값 안에 \
               이스케이프되지 않은 큰따옴표를 절대 넣지 말 것.\n\
             - 각 최상위 키(summary/nodes/edges/points)는 정확히 한 번만 출력(중복 금지).\n\
             - 반드시 아래 JSON 스키마로만 응답:\n\
             {{\"summary\":\"\",\"nodes\":[{{\"id\":\"\",\"text\":\"\",\"role\":\"\",\"pos\":\"\",\"ko\":\"\"}}],\
             \"edges\":[{{\"from\":\"\",\"to\":\"\",\"label\":\"\"}}],\"points\":[\"\"]}}\n\n\
             === 문장 ===\n{sentence}",
        );

        let content = self.message(&prompt, 6000).await?;
        let json_str = extract_json_block(&content);
        // 모델이 이따금 같은 키(summary 등)를 중복 출력한다. 구조체로 직접 파싱하면
        // serde가 중복 필드에서 에러를 내므로, Value로 먼저 파싱해(중복 키는 마지막 값으로
        // 병합됨) 견고하게 만든 뒤 구조체로 변환한다.
        let v: serde_json::Value = serde_json::from_str(&json_str)
            .map_err(|e| anyhow!("failed to parse grammar JSON: {e}; raw: {content}"))?;
        serde_json::from_value(v)
            .map_err(|e| anyhow!("failed to convert grammar JSON: {e}; raw: {content}"))
    }

    /// 문법 포인트 하나를 '강의 본문'으로 확장한다: 강의체 상세 설명 + 같은 구조의
    /// 새 연습 예문(영어+한국어). 원문 문장을 문맥으로 함께 넘긴다.
    /// 문장 텍스트를 되뱉으므로 인용은 「 」로, 키 중복은 금지하도록 지시한다.
    pub async fn analyze_point(&self, sentence: &str, point: &str) -> Result<PointDetail> {
        let prompt = format!(
            "아래 '문법 포인트'를 영어 학습자에게 강의하듯 한국어로 자세히 설명하고, \
             같은 문법 구조를 쓴 새 연습 예문을 만들어라.\n\
             - explanation: 이 포인트의 규칙·쓰임·주의점을 3~5문장으로. 원문 문장을 근거로 들되 \
               일반화해 설명.\n\
             - examples: 같은 문법 구조를 쓴 새 예문 2~3개. 각 example은 en(원문과 다른 소재의 \
               영어 새 예문), ko(그 한국어 해석).\n\
             - 인용은 큰따옴표(\") 대신 「 」를 쓰고 값 안에 이스케이프되지 않은 큰따옴표 금지. \
               각 최상위 키(explanation/examples)는 한 번만 출력.\n\
             - 반드시 아래 JSON 스키마로만 응답:\n\
             {{\"explanation\":\"\",\"examples\":[{{\"en\":\"\",\"ko\":\"\"}}]}}\n\n\
             === 원문 문장 ===\n{sentence}\n\n=== 문법 포인트 ===\n{point}",
        );

        let content = self.message(&prompt, 2048).await?;
        let json_str = extract_json_block(&content);
        let v: serde_json::Value = serde_json::from_str(&json_str)
            .map_err(|e| anyhow!("failed to parse point JSON: {e}; raw: {content}"))?;
        serde_json::from_value(v)
            .map_err(|e| anyhow!("failed to convert point JSON: {e}; raw: {content}"))
    }

    /// 긴 기사도 커버하도록 문단 경계로 쪼개(≤PIECE) 병렬 청킹한 뒤 문단 배열을 순서대로 병합.
    /// 조각이 하나면 analyze_chunks와 동일. 조각은 CHUNK_MAX_PIECES까지만(비용 상한).
    pub async fn chunk_article(&self, article: &str) -> Result<ChunkedText> {
        const PIECE: usize = 5000;
        const CHUNK_MAX_PIECES: usize = 6;
        let mut pieces = split_chunks(article, PIECE);
        if pieces.len() <= 1 {
            return self.analyze_chunks(article).await;
        }
        if pieces.len() > CHUNK_MAX_PIECES {
            pieces.truncate(CHUNK_MAX_PIECES);
        }
        // 동시성 제한 병렬 호출(원문 순서 보존).
        let sem = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
        let mut set = tokio::task::JoinSet::new();
        for (i, piece) in pieces.into_iter().enumerate() {
            let this = self.clone();
            let sem = sem.clone();
            set.spawn(async move {
                let _permit = sem.acquire_owned().await.expect("semaphore not closed");
                (i, this.analyze_chunks(&piece).await)
            });
        }
        let mut ordered: Vec<(usize, Result<ChunkedText>)> = Vec::new();
        while let Some(res) = set.join_next().await {
            ordered.push(res.map_err(|e| anyhow!("chunk task join error: {e}"))?);
        }
        ordered.sort_by_key(|(i, _)| *i);
        let mut paras = Vec::new();
        for (i, r) in ordered {
            match r {
                Ok(ct) => paras.extend(ct.paras),
                Err(e) => tracing::warn!("청크 조각 {i} 실패(건너뜀): {e}"),
            }
        }
        if paras.is_empty() {
            return Err(anyhow!("청크 생성 결과가 없습니다"));
        }
        Ok(ChunkedText { paras })
    }

    /// 나만의 문법책 항목에 대한 질문에 답한다(문장 + 내 노트를 문맥으로). 노트에 바로 붙일 수
    /// 있게 간결한 한국어 마크다운으로 문법 규칙·구조·쓰임을 정리해준다.
    pub async fn ask_grammar(&self, sentence: &str, note: &str, question: &str) -> Result<String> {
        let ctx = if note.trim().is_empty() {
            "(아직 없음)".to_string()
        } else {
            note.to_string()
        };
        let prompt = format!(
            "나는 영어 문장으로 '나만의 문법책'을 만들고 있다. 아래 문장과 내 노트를 참고해 내 질문에 \
             한국어로 친절하고 구체적으로 답해줘. 문법 규칙·구조·쓰임을 노트에 바로 붙일 수 있게 \
             간결한 마크다운으로 정리하고, 필요하면 예문을 곁들여라.\n\n\
             === 문장 ===\n{sentence}\n\n=== 내 노트 ===\n{ctx}\n\n=== 질문 ===\n{question}",
        );
        self.message(&prompt, 1500).await
    }

    /// 기사를 '청크 리딩'용으로 문단별 구/절 단위(청크)로 끊는다(구문 인식 기반, 기사당 1회).
    /// 원문 단어는 그대로 두고 끊기만 하며, 청크를 공백으로 이으면 원문과 같아야 한다.
    pub async fn analyze_chunks(&self, article: &str) -> Result<ChunkedText> {
        let prompt = format!(
            "다음 영어 글을 '청크 리딩(직독직해)'용으로 의미 단위(구/절)로 끊어라. 각 청크는 한 번에 \
             눈에 담기는 짧은 구(주어구·동사구·전치사구·종속절·관계절 등, 보통 2~6단어).\n\
             - 원문 단어를 절대 바꾸거나 빠뜨리지 말고 순서 그대로 두고 '끊기만' 하라. 각 청크를 \
               공백으로 이으면 원문 문단과 정확히 같아야 한다.\n\
             - 빈 줄로 나뉜 문단 구조를 유지(문단별 배열).\n\
             - 문자열 안에 큰따옴표가 있으면 JSON에서 반드시 \\\" 로 이스케이프하라.\n\
             - 반드시 아래 JSON 스키마로만 응답(추론 과정 출력 금지):\n\
             {{\"paras\":[[\"chunk\",\"chunk\"]]}}\n\n\
             === 본문 ===\n{body}",
            body = article
        );
        let content = self.message(&prompt, 8000).await?;
        let json_str = extract_json_block(&content);
        let v: serde_json::Value = serde_json::from_str(&json_str)
            .map_err(|e| anyhow!("failed to parse chunks JSON: {e}; raw: {content}"))?;
        serde_json::from_value(v)
            .map_err(|e| anyhow!("failed to convert chunks JSON: {e}; raw: {content}"))
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
             - 인물의 발언 등을 인용할 때는 JSON 문자열(blog/thread) 안에 큰따옴표(\")를 \
               직접 넣지 말고 반드시 「 」(홑낫표)로 감쌀 것. 값 안에 이스케이프되지 않은 \
               큰따옴표가 하나라도 있으면 JSON 파싱이 깨진다.\n\
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
