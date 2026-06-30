mod db;
mod extract;
mod models;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Form, Router,
};
use std::collections::HashMap;
use std::sync::Arc;

use db::Db;
use extract::Extractor;
use models::{Category, EntryInput};

#[derive(Clone)]
struct AppState {
    db: Db,
    extractor: Arc<Extractor>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let node = std::env::var("COREDB_NODE").unwrap_or_else(|_| "127.0.0.1:9042".into());
    let bind = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");
    let model =
        std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into());

    let db = Db::connect(&node).await?;
    let extractor = Arc::new(Extractor::new(api_key, model));
    let state = AppState { db, extractor };

    // NOTE: 인증(Google OAuth)은 다음 단계에서 미들웨어로 추가 (스펙 5번 참조)
    let app = Router::new()
        .route("/", get(index))
        .route("/entries", post(create_entry))
        .route("/words", get(list_words))
        .route("/words/known", post(mark_known))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

/// 본문 붙여넣기 → 원문 저장 → Claude 추출 → 단어/문장 저장
async fn create_entry(
    State(st): State<AppState>,
    Form(input): Form<EntryInput>,
) -> Result<Redirect, AppError> {
    let cat = Category::parse(&input.category)
        .ok_or_else(|| AppError(format!("invalid category: {}", input.category)))?;
    let detail = input.source_detail.as_deref();
    let url = input.source_url.as_deref();

    let entry_id = st
        .db
        .insert_entry(cat, &input.raw_text, detail, url)
        .await
        .map_err(AppError::from)?;

    let known = st.db.known_terms().await.map_err(AppError::from)?;
    let extraction = st
        .extractor
        .extract(&input.raw_text, &known)
        .await
        .map_err(AppError::from)?;

    for w in &extraction.words {
        st.db
            .insert_word(cat, entry_id, w, detail, url)
            .await
            .map_err(AppError::from)?;
    }
    for s in &extraction.sentences {
        st.db
            .insert_sentence(cat, entry_id, s, detail, url)
            .await
            .map_err(AppError::from)?;
    }

    Ok(Redirect::to("/words"))
}

async fn list_words(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, AppError> {
    let cat = q.get("category").and_then(|s| Category::parse(s));
    let words = st.db.list_words(cat).await.map_err(AppError::from)?;

    let mut html = String::from("<h1>단어장</h1><ul>");
    for (term, def, ex) in words {
        html.push_str(&format!(
            "<li><b>{}</b> — {}<br><i>{}</i></li>",
            esc(&term),
            esc(&def),
            esc(&ex)
        ));
    }
    html.push_str("</ul><a href=\"/\">← 붙여넣기</a>");
    Ok(Html(html))
}

async fn mark_known(
    State(st): State<AppState>,
    Form(f): Form<HashMap<String, String>>,
) -> Result<Redirect, AppError> {
    if let Some(term) = f.get("term") {
        st.db.mark_known(term).await.map_err(AppError::from)?;
    }
    Ok(Redirect::to("/words"))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

// 간단한 에러 → 500 응답
struct AppError(String);

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError(e.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        tracing::error!("{}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, self.0).into_response()
    }
}
