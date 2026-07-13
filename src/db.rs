use anyhow::{anyhow, Result};
use chrono::Utc;
use serde_json::{json, Value};
use std::collections::HashSet;
use uuid::Uuid;

use crate::models::{Category, EntryRow, Sentence, Word};

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
            // 삭제된 기사 id. CoreDB가 복합 PK(words/sentences) 행을 실제로 지우지
            // 못하므로, 삭제된 entry_id를 여기 기록해 조회 시 걸러낸다.
            "CREATE TABLE IF NOT EXISTS vocab.deleted_entries (\
                id uuid PRIMARY KEY, created_at timestamp)",
            // 단어 어근 분석 캐시(term → 분석 JSON). 같은 단어 재조회 시 Claude 재호출 회피.
            "CREATE TABLE IF NOT EXISTS vocab.word_roots (\
                term text PRIMARY KEY, analysis text, created_at timestamp)",
            // 기사 구조 마인드맵 캐시(entry_id → 마인드맵 JSON).
            "CREATE TABLE IF NOT EXISTS vocab.entry_mindmap (\
                entry_id uuid PRIMARY KEY, mindmap text, created_at timestamp)",
            // 한글 요약 초안 캐시(entry_id → 요약 JSON: 블로그 + X 스레드).
            "CREATE TABLE IF NOT EXISTS vocab.entry_summary (\
                entry_id uuid PRIMARY KEY, summary text, created_at timestamp)",
            // 문장 문법 그래프 캐시(문장 텍스트 → 분석 JSON: 노드/엣지/포인트).
            // word_roots처럼 단일 PK(=문장 텍스트)라 재조회 시 Claude 재호출 회피 + upsert 안전.
            "CREATE TABLE IF NOT EXISTS vocab.sentence_grammar (\
                sentence text PRIMARY KEY, analysis text, created_at timestamp)",
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
    /// 삭제된 기사(deleted_entries)에서 나온 단어는 제외한다.
    /// 반환: (카테고리, term, definition, example).
    pub async fn list_words(
        &self,
        cat: Option<Category>,
    ) -> Result<Vec<(Category, String, String, String)>> {
        let deleted = self.deleted_entry_ids().await?;
        let cats: Vec<Category> = cat.map_or_else(|| Category::ALL.to_vec(), |c| vec![c]);
        let mut out = Vec::new();
        for c in cats {
            let cql = format!(
                "SELECT term, definition, example, entry_id FROM vocab.words WHERE category = {}",
                cql_str(c.as_str())
            );
            let v = self.exec(&cql).await?;
            for r in rows(&v) {
                if is_deleted(r, &deleted) {
                    continue;
                }
                out.push((c, text(r, "term"), text(r, "definition"), text(r, "example")));
            }
        }
        Ok(out)
    }

    /// 카테고리별 베스트 문장 조회. cat=None이면 전체.
    /// 삭제된 기사에서 나온 문장은 제외한다. 반환: (카테고리, text, reason).
    pub async fn list_sentences(
        &self,
        cat: Option<Category>,
    ) -> Result<Vec<(Category, String, String)>> {
        let deleted = self.deleted_entry_ids().await?;
        let cats: Vec<Category> = cat.map_or_else(|| Category::ALL.to_vec(), |c| vec![c]);
        let mut out = Vec::new();
        for c in cats {
            let cql = format!(
                "SELECT text, reason, entry_id FROM vocab.sentences WHERE category = {}",
                cql_str(c.as_str())
            );
            let v = self.exec(&cql).await?;
            for r in rows(&v) {
                if is_deleted(r, &deleted) {
                    continue;
                }
                out.push((c, text(r, "text"), text(r, "reason")));
            }
        }
        Ok(out)
    }

    /// 복습 대상 단어: 아직 '안다'로 표시되지 않은(known_words에 없는) 전체 단어.
    /// CoreDB는 비-키 컬럼 필터가 제한적이라, 전체를 받아 Rust에서 걸러낸다.
    pub async fn review_words(&self) -> Result<Vec<(Category, String, String, String)>> {
        let known: std::collections::HashSet<String> =
            self.known_terms().await?.into_iter().collect();
        let all = self.list_words(None).await?;
        Ok(all
            .into_iter()
            .filter(|(_, term, _, _)| !known.contains(term))
            .collect())
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

    /// 저장된 기사 전체(최신순). entries는 id 파티션이라 WHERE 없이 스캔한다.
    pub async fn list_entries(&self) -> Result<Vec<EntryRow>> {
        let deleted = self.deleted_entry_ids().await?;
        let v = self
            .exec(
                "SELECT id, category, raw_text, source_detail, source_url, created_at \
                 FROM vocab.entries",
            )
            .await?;
        let mut out: Vec<EntryRow> = rows(&v)
            .iter()
            .filter_map(entry_from_row)
            .filter(|e| !deleted.contains(&e.id))
            .collect();
        // CoreDB는 서버측 정렬이 없어 Rust에서 최신순 정렬.
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    /// 기사 한 건을 id로 조회. 소프트 삭제된 기사는 없는 것으로 취급한다.
    pub async fn get_entry(&self, id: Uuid) -> Result<Option<EntryRow>> {
        if self.deleted_entry_ids().await?.contains(&id) {
            return Ok(None);
        }
        let cql = format!(
            "SELECT id, category, raw_text, source_detail, source_url, created_at \
             FROM vocab.entries WHERE id = {id}"
        );
        let v = self.exec(&cql).await?;
        Ok(rows(&v).first().and_then(entry_from_row))
    }

    /// 기사 원문을 갱신 저장(이어쓰기).
    ///
    /// 주의: CoreDB의 INSERT는 부분 upsert가 아니라 **전체 행 REPLACE**라서
    /// (지정하지 않은 컬럼은 NULL로 덮임) 모든 컬럼을 함께 다시 써야 한다.
    pub async fn save_entry(&self, e: &EntryRow) -> Result<()> {
        let cql = format!(
            "INSERT INTO vocab.entries \
             (id, raw_text, category, source_detail, source_url, created_at) \
             VALUES ({id}, {raw}, {cat}, {sd}, {su}, {ts})",
            id = e.id,
            raw = cql_str(&e.raw_text),
            cat = cql_str(e.category.as_str()),
            sd = cql_opt(e.source_detail.as_deref()),
            su = cql_opt(e.source_url.as_deref()),
            ts = e.created_at,
        );
        self.exec(&cql).await?;
        Ok(())
    }

    /// 특정 기사에서 추출된 단어들 (term, definition, example).
    /// words는 category 파티션이고 entry_id 인덱스가 없어, 전체를 받아 Rust에서 거른다.
    pub async fn words_for_entry(&self, entry_id: Uuid) -> Result<Vec<(String, String, String)>> {
        let mut out = Vec::new();
        for c in Category::ALL {
            let cql = format!(
                "SELECT term, definition, example, entry_id FROM vocab.words WHERE category = {}",
                cql_str(c.as_str())
            );
            let v = self.exec(&cql).await?;
            for r in rows(&v) {
                if uuid_col(r, "entry_id") == Some(entry_id) {
                    out.push((text(r, "term"), text(r, "definition"), text(r, "example")));
                }
            }
        }
        Ok(out)
    }

    /// 기사 삭제(소프트 삭제).
    ///
    /// 이 CoreDB 빌드는 DELETE가 status=success를 반환하면서도 **행을 실제로
    /// 지우지 않는다**(entries의 단일 PK, words/sentences의 복합 PK 모두). 따라서
    /// 물리 삭제 대신 deleted_entries에 id를 남기고, list_entries/list_words/
    /// list_sentences가 이 집합을 조회 시 걸러낸다.
    pub async fn delete_entry(&self, id: Uuid) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        self.exec(&format!(
            "INSERT INTO vocab.deleted_entries (id, created_at) VALUES ({id}, {now})"
        ))
        .await?;
        Ok(())
    }

    /// 삭제 표시된 기사 id 집합.
    pub async fn deleted_entry_ids(&self) -> Result<HashSet<Uuid>> {
        let v = self.exec("SELECT id FROM vocab.deleted_entries").await?;
        Ok(rows(&v).iter().filter_map(|r| uuid_col(r, "id")).collect())
    }

    /// 캐시된 어근 분석 JSON(term). 없으면 None.
    pub async fn get_word_roots(&self, term: &str) -> Result<Option<String>> {
        let cql = format!(
            "SELECT analysis FROM vocab.word_roots WHERE term = {}",
            cql_str(term)
        );
        let v = self.exec(&cql).await?;
        Ok(rows(&v)
            .first()
            .map(|r| text(r, "analysis"))
            .filter(|s| !s.is_empty()))
    }

    /// 어근 분석 JSON을 캐시에 저장(term 기준 upsert).
    pub async fn save_word_roots(&self, term: &str, analysis_json: &str) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let cql = format!(
            "INSERT INTO vocab.word_roots (term, analysis, created_at) VALUES ({}, {}, {now})",
            cql_str(term),
            cql_str(analysis_json),
        );
        self.exec(&cql).await?;
        Ok(())
    }

    /// 캐시된 문장 문법 분석 JSON(문장 텍스트). 없으면 None.
    pub async fn get_sentence_grammar(&self, sentence: &str) -> Result<Option<String>> {
        let cql = format!(
            "SELECT analysis FROM vocab.sentence_grammar WHERE sentence = {}",
            cql_str(sentence)
        );
        let v = self.exec(&cql).await?;
        Ok(rows(&v)
            .first()
            .map(|r| text(r, "analysis"))
            .filter(|s| !s.is_empty()))
    }

    /// 문장 문법 분석 JSON을 캐시에 저장(문장 텍스트 기준 upsert).
    pub async fn save_sentence_grammar(&self, sentence: &str, analysis_json: &str) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let cql = format!(
            "INSERT INTO vocab.sentence_grammar (sentence, analysis, created_at) \
             VALUES ({}, {}, {now})",
            cql_str(sentence),
            cql_str(analysis_json),
        );
        self.exec(&cql).await?;
        Ok(())
    }

    /// 캐시된 기사 마인드맵 JSON. 비어있으면(무효화됨) None.
    pub async fn get_entry_mindmap(&self, entry_id: Uuid) -> Result<Option<String>> {
        let cql = format!(
            "SELECT mindmap FROM vocab.entry_mindmap WHERE entry_id = {entry_id}"
        );
        let v = self.exec(&cql).await?;
        Ok(rows(&v)
            .first()
            .map(|r| text(r, "mindmap"))
            .filter(|s| !s.is_empty()))
    }

    /// 기사 마인드맵 JSON을 캐시에 저장(entry_id 기준 upsert). 빈 문자열이면 무효화.
    pub async fn save_entry_mindmap(&self, entry_id: Uuid, mindmap_json: &str) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let cql = format!(
            "INSERT INTO vocab.entry_mindmap (entry_id, mindmap, created_at) \
             VALUES ({entry_id}, {}, {now})",
            cql_str(mindmap_json),
        );
        self.exec(&cql).await?;
        Ok(())
    }

    /// 캐시된 한글 요약 초안 JSON. 비어있으면(무효화됨) None.
    pub async fn get_entry_summary(&self, entry_id: Uuid) -> Result<Option<String>> {
        let cql = format!(
            "SELECT summary FROM vocab.entry_summary WHERE entry_id = {entry_id}"
        );
        let v = self.exec(&cql).await?;
        Ok(rows(&v)
            .first()
            .map(|r| text(r, "summary"))
            .filter(|s| !s.is_empty()))
    }

    /// 한글 요약 초안 JSON을 캐시에 저장(entry_id 기준 upsert). 빈 문자열이면 무효화.
    pub async fn save_entry_summary(&self, entry_id: Uuid, summary_json: &str) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let cql = format!(
            "INSERT INTO vocab.entry_summary (entry_id, summary, created_at) \
             VALUES ({entry_id}, {}, {now})",
            cql_str(summary_json),
        );
        self.exec(&cql).await?;
        Ok(())
    }

    /// 특정 기사에서 추출된 문장들 (text, reason).
    pub async fn sentences_for_entry(&self, entry_id: Uuid) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for c in Category::ALL {
            let cql = format!(
                "SELECT text, reason, entry_id FROM vocab.sentences WHERE category = {}",
                cql_str(c.as_str())
            );
            let v = self.exec(&cql).await?;
            for r in rows(&v) {
                if uuid_col(r, "entry_id") == Some(entry_id) {
                    out.push((text(r, "text"), text(r, "reason")));
                }
            }
        }
        Ok(out)
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
/// (NULL 컬럼은 `{"col":"Null"}` 형태라 `Text` 키가 없어 빈 문자열로 떨어진다.)
fn text(row: &Value, col: &str) -> String {
    row.get("columns")
        .and_then(|c| c.get(col))
        .and_then(|val| val.get("Text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// UUID 컬럼: `{"col":{"UUID":"..."}}`.
fn uuid_col(row: &Value, col: &str) -> Option<Uuid> {
    row.get("columns")
        .and_then(|c| c.get(col))
        .and_then(|val| val.get("UUID"))
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
}

/// Timestamp 컬럼: `{"col":{"Timestamp":epoch_millis}}`.
fn ts_col(row: &Value, col: &str) -> Option<i64> {
    row.get("columns")
        .and_then(|c| c.get(col))
        .and_then(|val| val.get("Timestamp"))
        .and_then(Value::as_i64)
}

/// 행의 entry_id가 삭제된 기사 집합에 속하는지(삭제 필터용).
fn is_deleted(row: &Value, deleted: &HashSet<Uuid>) -> bool {
    uuid_col(row, "entry_id").is_some_and(|id| deleted.contains(&id))
}

/// 빈 문자열은 None으로(저장된 NULL/미입력 구분).
fn opt_text(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// entries 한 행 → EntryRow. id가 없으면(파싱 실패) 건너뛴다.
fn entry_from_row(r: &Value) -> Option<EntryRow> {
    let id = uuid_col(r, "id")?;
    Some(EntryRow {
        id,
        category: Category::parse(&text(r, "category")).unwrap_or(Category::Other),
        raw_text: text(r, "raw_text"),
        source_detail: opt_text(text(r, "source_detail")),
        source_url: opt_text(text(r, "source_url")),
        created_at: ts_col(r, "created_at").unwrap_or(0),
    })
}
