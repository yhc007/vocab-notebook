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
