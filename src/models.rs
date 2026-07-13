use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 출처 카테고리
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Nyt,
    Book,
    Paper,
    Other,
}

impl Category {
    /// UI 필터/네비게이션에서 순회할 전체 카테고리.
    pub const ALL: [Category; 4] = [
        Category::Nyt,
        Category::Book,
        Category::Paper,
        Category::Other,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Category::Nyt => "nyt",
            Category::Book => "book",
            Category::Paper => "paper",
            Category::Other => "other",
        }
    }

    /// 화면 표시용 한국어 라벨.
    pub fn label(&self) -> &'static str {
        match self {
            Category::Nyt => "NYT",
            Category::Book => "책",
            Category::Paper => "논문",
            Category::Other => "기타",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "nyt" => Some(Category::Nyt),
            "book" => Some(Category::Book),
            "paper" => Some(Category::Paper),
            "other" => Some(Category::Other),
            _ => None,
        }
    }
}

/// 저장된 기사 한 건(원문 보관/이어보기용).
/// `created_at`은 epoch millis.
#[derive(Debug, Clone)]
pub struct EntryRow {
    pub id: Uuid,
    pub category: Category,
    pub raw_text: String,
    pub source_detail: Option<String>,
    pub source_url: Option<String>,
    pub created_at: i64,
}

/// Claude가 추출한 단어
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Word {
    #[serde(default = "Uuid::new_v4")]
    pub id: Uuid,
    pub term: String,
    pub definition: String,
    pub example: String,
}

/// Claude가 추출한 베스트 문장
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sentence {
    #[serde(default = "Uuid::new_v4")]
    pub id: Uuid,
    pub text: String,
    pub reason: String,
}

/// Claude 추출 결과 묶음
#[derive(Debug, Deserialize)]
pub struct Extraction {
    pub words: Vec<Word>,
    pub sentences: Vec<Sentence>,
}

/// 정제 패스(Stage 2) 응답 한 건: term은 원본 대조용, definition은 문맥 교정본.
/// example/id는 이 패스에서 다루지 않고 원본을 그대로 유지한다.
#[derive(Debug, Deserialize)]
pub struct RefinedWord {
    pub term: String,
    pub definition: String,
}

/// 정제 패스 응답 묶음(`{"words":[...]}`).
#[derive(Debug, Deserialize)]
pub struct RefinedWords {
    #[serde(default)]
    pub words: Vec<RefinedWord>,
}

/// 어근 분해 조각(접두사/어근/접미사).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootPart {
    pub piece: String,
    /// prefix / root / suffix 등.
    #[serde(default)]
    pub kind: String,
    pub meaning: String,
}

/// 단어의 어근 분석(어원 기반 습득용). 필드가 빠져도 파싱이 깨지지 않게 default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootAnalysis {
    #[serde(default)]
    pub parts: Vec<RootPart>,
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub related: Vec<String>,
    #[serde(default)]
    pub mnemonic: String,
}

/// 마인드맵 가지: 주요 섹션/서브헤딩 + 핵심 키워드.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MindBranch {
    pub heading: String,
    #[serde(default)]
    pub keywords: Vec<String>,
}

/// 기사 구조 마인드맵(중앙 제목 + 가지들).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MindMap {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub branches: Vec<MindBranch>,
}

/// 한글 정리 초안: 블로그용(마크다운) + X 스레드(트윗 배열).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    #[serde(default)]
    pub blog: String,
    #[serde(default)]
    pub thread: Vec<String>,
}

/// 문법 그래프 노드: 문장 속 토큰/구 하나. id는 엣지 참조용.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GramNode {
    pub id: String,
    pub text: String,
    /// 주어/술어/목적어/보어/수식어/접속 등 한국어 문법 역할.
    #[serde(default)]
    pub role: String,
    /// 품사/구 유형(명사구/동사/전치사구 등), 선택.
    #[serde(default)]
    pub pos: String,
    /// 이 조각의 짧은 우리말 뜻(초등학생도 이해할 쉬운 한국어).
    #[serde(default)]
    pub ko: String,
}

/// 문법 그래프 엣지: head(from)→dependent(to) 문법 관계.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GramEdge {
    pub from: String,
    pub to: String,
    /// 관계명(주어·목적어·수식·종속절·병렬 등 한국어).
    #[serde(default)]
    pub label: String,
}

/// 베스트 문장의 문법 분석 = 강의 도입부(요약·포인트) + 구조 그래프(노드·엣지).
/// 필드가 빠져도 파싱이 깨지지 않게 모두 default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentenceGrammar {
    /// 문장 구조 한 줄 요약(주절/종속절, 핵심 구문).
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub nodes: Vec<GramNode>,
    #[serde(default)]
    pub edges: Vec<GramEdge>,
    /// "이 문장에서 배울 문법 포인트" 2~3개(강의 시작).
    #[serde(default)]
    pub points: Vec<String>,
}

/// 문법 포인트의 연습 예문(같은 구조의 새 문장 + 한국어 해석).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GramExample {
    #[serde(default)]
    pub en: String,
    #[serde(default)]
    pub ko: String,
}

/// 문법 포인트 상세(강의 본문): 강의체 설명 + 같은 구조의 연습 예문들.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointDetail {
    #[serde(default)]
    pub explanation: String,
    #[serde(default)]
    pub examples: Vec<GramExample>,
}
