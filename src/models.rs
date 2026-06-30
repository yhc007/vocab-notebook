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

/// 붙여넣기 폼 입력
#[derive(Debug, Deserialize)]
pub struct EntryInput {
    pub raw_text: String,
    pub category: String,
    pub source_detail: Option<String>,
    pub source_url: Option<String>,
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
