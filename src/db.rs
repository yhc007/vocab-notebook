use anyhow::Result;
use chrono::Utc;
use scylla::{Session, SessionBuilder};
use std::sync::Arc;
use uuid::Uuid;

use crate::models::{Category, Sentence, Word};

#[derive(Clone)]
pub struct Db {
    session: Arc<Session>,
}

impl Db {
    /// CoreDB(Native Protocol v4)에 연결하고 스키마를 부트스트랩한다.
    pub async fn connect(node: &str) -> Result<Self> {
        let session = SessionBuilder::new().known_node(node).build().await?;
        let db = Db {
            session: Arc::new(session),
        };
        db.bootstrap().await?;
        Ok(db)
    }

    /// 키스페이스/테이블/인덱스 생성 (IF NOT EXISTS).
    /// 주: CoreDB는 제한된 CQL을 지원하므로, 일부 구문은 버전에 따라 조정이 필요할 수 있다.
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
                PRIMARY KEY (category, created_at, id)) \
                WITH CLUSTERING ORDER BY (created_at DESC)",
            "CREATE INDEX IF NOT EXISTS idx_words_term ON vocab.words (term)",
            "CREATE TABLE IF NOT EXISTS vocab.sentences (\
                category text, created_at timestamp, id uuid, entry_id uuid, \
                text text, reason text, source_detail text, source_url text, \
                PRIMARY KEY (category, created_at, id)) \
                WITH CLUSTERING ORDER BY (created_at DESC)",
            "CREATE TABLE IF NOT EXISTS vocab.known_words (\
                term text PRIMARY KEY, created_at timestamp)",
        ];
        for s in stmts {
            self.session.query_unpaged(s, &[]).await?;
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
        self.session
            .query_unpaged(
                "INSERT INTO vocab.entries \
                 (id, raw_text, category, source_detail, source_url, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                (id, raw_text, cat.as_str(), source_detail, source_url, now),
            )
            .await?;
        Ok(id)
    }

    /// 현재 등록된 known_words 목록 (추출 시 Claude에 전달).
    pub async fn known_terms(&self) -> Result<Vec<String>> {
        let rows = self
            .session
            .query_unpaged("SELECT term FROM vocab.known_words", &[])
            .await?
            .into_rows_result()?;
        let mut out = Vec::new();
        for row in rows.rows::<(String,)>()? {
            out.push(row?.0);
        }
        Ok(out)
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
        self.session
            .query_unpaged(
                "INSERT INTO vocab.words \
                 (category, created_at, id, entry_id, term, definition, example, \
                  source_detail, source_url, known) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, false)",
                (
                    cat.as_str(),
                    now,
                    w.id,
                    entry_id,
                    &w.term,
                    &w.definition,
                    &w.example,
                    source_detail,
                    source_url,
                ),
            )
            .await?;
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
        self.session
            .query_unpaged(
                "INSERT INTO vocab.sentences \
                 (category, created_at, id, entry_id, text, reason, \
                  source_detail, source_url) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    cat.as_str(),
                    now,
                    s.id,
                    entry_id,
                    &s.text,
                    &s.reason,
                    source_detail,
                    source_url,
                ),
            )
            .await?;
        Ok(())
    }

    /// 카테고리별 단어 조회 (최신순). cat=None이면 전체 카테고리 합쳐서 조회.
    pub async fn list_words(&self, cat: Option<Category>) -> Result<Vec<(String, String, String)>> {
        let mut out = Vec::new();
        let cats: Vec<Category> = match cat {
            Some(c) => vec![c],
            None => vec![Category::Nyt, Category::Book, Category::Paper, Category::Other],
        };
        for c in cats {
            let rows = self
                .session
                .query_unpaged(
                    "SELECT term, definition, example FROM vocab.words WHERE category = ?",
                    (c.as_str(),),
                )
                .await?
                .into_rows_result()?;
            for row in rows.rows::<(String, String, String)>()? {
                out.push(row?);
            }
        }
        Ok(out)
    }

    /// 단어를 '안다'로 표시 → known_words에 등록.
    pub async fn mark_known(&self, term: &str) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        self.session
            .query_unpaged(
                "INSERT INTO vocab.known_words (term, created_at) VALUES (?, ?)",
                (term, now),
            )
            .await?;
        Ok(())
    }
}
