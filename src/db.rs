use anyhow::{anyhow, Result};
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::models::{Category, Sentence, Word};

/// CoreDB 접근 계층.
///
/// CoreDB의 Native Protocol(9042) 구현은 scylla 드라이버와 DML 결과 프레임이
/// 호환되지 않아(SELECT/INSERT 응답 파싱 실패), README가 안내하는 대로
/// HTTP `/query` JSON API를 사용한다. 요청: `{"query": "<CQL>"}`,
/// 응답: `{"status":"success","data":[{"columns":{...}}]}` 또는
/// `{"status":"error","message":"..."}`.
#[derive(Clone)]
pub struct Db {
    http: reqwest::Client,
    url: String,
}

impl Db {
    /// `node`는 `host:port`(예: `127.0.0.1:9142`) 또는 전체 URL을 받는다.
    pub async fn connect(node: &str) -> Result<Self> {
        let base = if node.starts_with("http://") || node.starts_with("https://") {
            node.trim_end_matches('/').to_string()
        } else {
            format!("http://{}", node)
        };
        let db = Db {
            http: reqwest::Client::new(),
            url: format!("{base}/query"),
        };
        db.bootstrap().await?;
        Ok(db)
    }

    /// HTTP `/query`로 CQL 한 문장을 실행하고 JSON 응답을 돌려준다.
    /// `status == "error"`면 메시지를 담아 Err를 반환한다.
    async fn exec(&self, cql: &str) -> Result<Value> {
        let resp = self
            .http
            .post(&self.url)
            .json(&json!({ "query": cql }))
            .send()
            .await?;
        let v: Value = resp.json().await?;
        if v.get("status").and_then(Value::as_str) == Some("error") {
            let msg = v.get("message").and_then(Value::as_str).unwrap_or("unknown");
            return Err(anyhow!("CoreDB error: {msg}"));
        }
        Ok(v)
    }

    /// 키스페이스/테이블/인덱스 생성.
    ///
    /// CoreDB CQL 방언 주의 (coredb parser.rs 기준):
    ///  - CREATE KEYSPACE 는 WITH REPLICATION 절이 필수 (IF NOT EXISTS 지원).
    ///  - CREATE TABLE 은 WITH CLUSTERING ORDER BY 등 WITH 절 미지원 → 제거.
    ///  - CREATE INDEX 는 IF NOT EXISTS 미지원 → 빼고, 재실행 시 '이미 존재'
    ///    에러는 아래 루프에서 무시한다.
    async fn bootstrap(&self) -> Result<()> {
        let stmts = [
            "CREATE KEYSPACE IF NOT EXISTS vocab WITH REPLICATION = \
             {'class': 'SimpleStrategy', 'replication_factor': 1}",
            "CREATE TABLE IF NOT EXISTS vocab.entries (\
                id uuid PRIMARY KEY, raw_text text, category text, \
                source_detail text, source_url text, created_at timestamp)",
            "CREATE TABLE IF NOT EXISTS vocab.words (\
                category text, created_at timestamp, id uuid, entry_id uuid, \
                term text, definition text, example text, \
                source_detail text, source_url text, known boolean, \
                PRIMARY KEY (category, created_at, id))",
            "CREATE INDEX idx_words_term ON vocab.words (term)",
            "CREATE TABLE IF NOT EXISTS vocab.sentences (\
                category text, created_at timestamp, id uuid, entry_id uuid, \
                text text, reason text, source_detail text, source_url text, \
                PRIMARY KEY (category, created_at, id))",
            "CREATE TABLE IF NOT EXISTS vocab.known_words (\
                term text PRIMARY KEY, created_at timestamp)",
        ];
        for s in stmts {
            if let Err(e) = self.exec(s).await {
                let msg = e.to_string().to_lowercase();
                if msg.contains("exist") {
                    tracing::warn!("bootstrap: skipping (already exists): {e}");
                } else {
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// 원문 저장 후 entry_id 반환.
    pub async fn insert_entry(
        &self,
        cat: Category,
        raw_text: &str,
        source_detail: Option<&str>,
        source_url: Option<&str>,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let now = Utc::now().timestamp_millis();
        let cql = format!(
            "INSERT INTO vocab.entries \
             (id, raw_text, category, source_detail, source_url, created_at) \
             VALUES ({id}, {raw}, {cat}, {sd}, {su}, {now})",
            raw = cql_str(raw_text),
            cat = cql_str(cat.as_str()),
            sd = cql_opt(source_detail),
            su = cql_opt(source_url),
        );
        self.exec(&cql).await?;
        Ok(id)
    }

    /// 현재 등록된 known_words 목록 (추출 시 Claude에 전달).
    pub async fn known_terms(&self) -> Result<Vec<String>> {
        let v = self.exec("SELECT term FROM vocab.known_words").await?;
        Ok(rows(&v)
            .iter()
            .map(|r| text(r, "term"))
            .filter(|t| !t.is_empty())
            .collect())
    }

    pub async fn insert_word(
        &self,
        cat: Category,
        entry_id: Uuid,
        w: &Word,
        source_detail: Option<&str>,
        source_url: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let cql = format!(
            "INSERT INTO vocab.words \
             (category, created_at, id, entry_id, term, definition, example, \
              source_detail, source_url, known) \
             VALUES ({cat}, {now}, {id}, {entry_id}, {term}, {def}, {ex}, {sd}, {su}, false)",
            cat = cql_str(cat.as_str()),
            id = w.id,
            term = cql_str(&w.term),
            def = cql_str(&w.definition),
            ex = cql_str(&w.example),
            sd = cql_opt(source_detail),
            su = cql_opt(source_url),
        );
        self.exec(&cql).await?;
        Ok(())
    }

    pub async fn insert_sentence(
        &self,
        cat: Category,
        entry_id: Uuid,
        s: &Sentence,
        source_detail: Option<&str>,
        source_url: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let cql = format!(
            "INSERT INTO vocab.sentences \
             (category, created_at, id, entry_id, text, reason, \
              source_detail, source_url) \
             VALUES ({cat}, {now}, {id}, {entry_id}, {text}, {reason}, {sd}, {su})",
            cat = cql_str(cat.as_str()),
            id = s.id,
            text = cql_str(&s.text),
            reason = cql_str(&s.reason),
            sd = cql_opt(source_detail),
            su = cql_opt(source_url),
        );
        self.exec(&cql).await?;
        Ok(())
    }

    /// 카테고리별 단어 조회. cat=None이면 전체 카테고리 합쳐서 조회.
    /// 반환: (카테고리, term, definition, example).
    pub async fn list_words(
        &self,
        cat: Option<Category>,
    ) -> Result<Vec<(Category, String, String, String)>> {
        let cats: Vec<Category> = cat.map_or_else(|| Category::ALL.to_vec(), |c| vec![c]);
        let mut out = Vec::new();
        for c in cats {
            let cql = format!(
                "SELECT term, definition, example FROM vocab.words WHERE category = {}",
                cql_str(c.as_str())
            );
            let v = self.exec(&cql).await?;
            for r in rows(&v) {
                out.push((c, text(r, "term"), text(r, "definition"), text(r, "example")));
            }
        }
        Ok(out)
    }

    /// 카테고리별 베스트 문장 조회. cat=None이면 전체.
    /// 반환: (카테고리, text, reason).
    pub async fn list_sentences(
        &self,
        cat: Option<Category>,
    ) -> Result<Vec<(Category, String, String)>> {
        let cats: Vec<Category> = cat.map_or_else(|| Category::ALL.to_vec(), |c| vec![c]);
        let mut out = Vec::new();
        for c in cats {
            let cql = format!(
                "SELECT text, reason FROM vocab.sentences WHERE category = {}",
                cql_str(c.as_str())
            );
            let v = self.exec(&cql).await?;
            for r in rows(&v) {
                out.push((c, text(r, "text"), text(r, "reason")));
            }
        }
        Ok(out)
    }

    /// 단어를 '안다'로 표시 → known_words에 등록.
    pub async fn mark_known(&self, term: &str) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let cql = format!(
            "INSERT INTO vocab.known_words (term, created_at) VALUES ({}, {now})",
            cql_str(term)
        );
        self.exec(&cql).await?;
        Ok(())
    }
}

/// 텍스트를 CQL 문자열 리터럴로 변환.
///
/// CoreDB의 CQL 파서는 표준 `''` 이스케이프를 해제하지 않고(리터럴로 저장),
/// 값 안의 raw `'`는 VALUES 파싱을 깨뜨린다. 따라서 ASCII 작은따옴표를
/// 타이포그래픽 따옴표(U+2019)로 치환해 안전하게 인용한다.
fn cql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "\u{2019}"))
}

/// Option<&str> → CQL 리터럴(None은 NULL).
fn cql_opt(o: Option<&str>) -> String {
    match o {
        Some(s) => cql_str(s),
        None => "NULL".to_string(),
    }
}

/// 응답 JSON에서 행 배열을 꺼낸다(없으면 빈 슬라이스).
fn rows(v: &Value) -> &[Value] {
    v.get("data").and_then(Value::as_array).map_or(&[], |a| a.as_slice())
}

/// 행에서 텍스트 컬럼 값을 꺼낸다: `{"columns":{col:{"Text":"..."}}}`.
fn text(row: &Value, col: &str) -> String {
    row.get("columns")
        .and_then(|c| c.get(col))
        .and_then(|val| val.get("Text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}
