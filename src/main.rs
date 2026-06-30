mod auth;
mod db;
mod extract;
mod models;

use axum::{
    extract::{FromRef, Query, State},
    http::StatusCode,
    middleware,
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Form, Router,
};
use axum_extra::extract::cookie::Key;
use std::collections::HashMap;
use std::sync::Arc;

use auth::OAuthConfig;
use db::Db;
use extract::Extractor;
use models::{Category, EntryInput};

#[derive(Clone)]
struct AppState {
    db: Db,
    extractor: Arc<Extractor>,
    oauth: Arc<OAuthConfig>,
    /// 세션 쿠키 암호화 키 (PrivateCookieJar용).
    key: Key,
}

// PrivateCookieJar가 AppState에서 쿠키 키를 꺼낼 수 있게 한다.
impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Self {
        state.key.clone()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // CoreDB HTTP /query 엔드포인트 (host:port 또는 전체 URL)
    let node = std::env::var("COREDB_NODE").unwrap_or_else(|_| "127.0.0.1:9142".into());
    let bind = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");
    let model =
        std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into());

    let db = Db::connect(&node).await?;
    let extractor = Arc::new(Extractor::new(api_key, model));
    let oauth = OAuthConfig::from_env();
    let key = auth::cookie_key();
    let state = AppState {
        db,
        extractor,
        oauth,
        key,
    };

    // 앱 라우트는 require_auth 게이트 뒤에 두고, /auth/* 는 공개로 둔다.
    let protected = Router::new()
        .route("/", get(index))
        .route("/entries", post(create_entry))
        .route("/words", get(list_words))
        .route("/words/known", post(mark_known))
        .route("/sentences", get(list_sentences))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_auth));

    let app = Router::new()
        .merge(protected)
        .route("/auth/login", get(auth::auth_login))
        .route("/auth/callback", get(auth::auth_callback))
        .route("/auth/logout", get(auth::auth_logout))
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

    let mut body = format!("{}<h1>단어장</h1>{}", nav("words"), category_filter("/words", cat));
    if words.is_empty() {
        body.push_str("<p class=\"empty\">아직 단어가 없습니다. <a href=\"/\">본문을 붙여넣어</a> 추출해 보세요.</p>");
    } else {
        body.push_str(&format!("<p class=\"count\">{}개</p><ul class=\"cards\">", words.len()));
        for (c, term, def, ex) in &words {
            // '안다' 버튼: known_words 등록 후 현재 필터 카테고리로 복귀
            body.push_str(&format!(
                "<li class=\"card\">\
                   <div class=\"head\">\
                     <span class=\"badge\">{cat}</span>\
                     <b class=\"term\">{term}</b>\
                     <form class=\"known\" method=\"post\" action=\"/words/known\">\
                       <input type=\"hidden\" name=\"term\" value=\"{term}\">\
                       <input type=\"hidden\" name=\"category\" value=\"{catkey}\">\
                       <button title=\"이 단어를 ‘안다’로 표시 — 다음 추출에서 제외\">안다</button>\
                     </form>\
                   </div>\
                   <div class=\"def\">{def}</div>\
                   <div class=\"ex\">{ex}</div>\
                 </li>",
                cat = esc(c.label()),
                catkey = esc(c.as_str()),
                term = esc(term),
                def = esc(def),
                ex = esc(ex),
            ));
        }
        body.push_str("</ul>");
    }
    Ok(page("단어장", &body))
}

async fn list_sentences(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, AppError> {
    let cat = q.get("category").and_then(|s| Category::parse(s));
    let sentences = st.db.list_sentences(cat).await.map_err(AppError::from)?;

    let mut body = format!(
        "{}<h1>베스트 문장</h1>{}",
        nav("sentences"),
        category_filter("/sentences", cat)
    );
    if sentences.is_empty() {
        body.push_str("<p class=\"empty\">아직 문장이 없습니다. <a href=\"/\">본문을 붙여넣어</a> 추출해 보세요.</p>");
    } else {
        body.push_str(&format!(
            "<p class=\"count\">{}개</p><ul class=\"cards\">",
            sentences.len()
        ));
        for (c, text, reason) in &sentences {
            body.push_str(&format!(
                "<li class=\"card\">\
                   <div class=\"head\"><span class=\"badge\">{cat}</span></div>\
                   <blockquote class=\"sentence\">{text}</blockquote>\
                   <div class=\"reason\">💡 {reason}</div>\
                 </li>",
                cat = esc(c.label()),
                text = esc(text),
                reason = esc(reason),
            ));
        }
        body.push_str("</ul>");
    }
    Ok(page("베스트 문장", &body))
}

async fn mark_known(
    State(st): State<AppState>,
    Form(f): Form<HashMap<String, String>>,
) -> Result<Redirect, AppError> {
    if let Some(term) = f.get("term") {
        st.db.mark_known(term).await.map_err(AppError::from)?;
    }
    // 표시 후 보고 있던 카테고리 필터를 유지한다.
    let dest = match f.get("category").filter(|c| Category::parse(c).is_some()) {
        Some(c) => format!("/words?category={c}"),
        None => "/words".to_string(),
    };
    Ok(Redirect::to(&dest))
}

/// HTML 텍스트/속성 양쪽에 안전하도록 이스케이프(따옴표 포함).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// 공통 HTML 셸(스타일 포함)로 본문을 감싼다.
fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!DOCTYPE html><html lang=\"ko\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title><style>{CSS}</style></head><body>{body}</body></html>",
        title = esc(title),
    ))
}

/// 상단 네비게이션. `active`는 현재 페이지 키(home/words/sentences).
fn nav(active: &str) -> String {
    let link = |href: &str, label: &str, key: &str| {
        let cls = if key == active { " class=\"active\"" } else { "" };
        format!("<a href=\"{href}\"{cls}>{label}</a>")
    };
    format!(
        "<nav>{}{}{}<a class=\"right\" href=\"/auth/logout\">로그아웃</a></nav>",
        link("/", "붙여넣기", "home"),
        link("/words", "단어장", "words"),
        link("/sentences", "베스트 문장", "sentences"),
    )
}

/// 카테고리 필터 칩. `base`는 대상 경로(/words 또는 /sentences).
fn category_filter(base: &str, active: Option<Category>) -> String {
    let chip = |href: String, label: &str, on: bool| {
        let cls = if on { "chip active" } else { "chip" };
        format!("<a class=\"{cls}\" href=\"{href}\">{label}</a>")
    };
    let mut s = String::from("<div class=\"filter\">");
    s.push_str(&chip(base.to_string(), "전체", active.is_none()));
    for c in Category::ALL {
        s.push_str(&chip(
            format!("{base}?category={}", c.as_str()),
            c.label(),
            active == Some(c),
        ));
    }
    s.push_str("</div>");
    s
}

const CSS: &str = "\
:root { color-scheme: light dark; }
body { font-family: system-ui, sans-serif; max-width: 760px; margin: 0 auto; padding: 1rem; line-height: 1.5; }
nav { display: flex; gap: 1rem; align-items: center; padding: .75rem 0; border-bottom: 1px solid #8884; margin-bottom: 1rem; }
nav a { text-decoration: none; color: inherit; opacity: .7; }
nav a.active { font-weight: 700; opacity: 1; }
nav a.right { margin-left: auto; font-size: .9rem; }
h1 { font-size: 1.4rem; margin: .5rem 0; }
.filter { display: flex; flex-wrap: wrap; gap: .4rem; margin: .75rem 0; }
.chip { text-decoration: none; color: inherit; border: 1px solid #8886; border-radius: 999px; padding: .2rem .8rem; font-size: .9rem; }
.chip.active { background: #2563eb; color: #fff; border-color: #2563eb; }
.count { color: #8889; font-size: .85rem; margin: .25rem 0 .75rem; }
.empty { color: #8889; padding: 2rem 0; }
.cards { list-style: none; padding: 0; margin: 0; display: flex; flex-direction: column; gap: .75rem; }
.card { border: 1px solid #8884; border-radius: 10px; padding: .8rem 1rem; }
.head { display: flex; align-items: center; gap: .6rem; }
.badge { font-size: .7rem; font-weight: 700; background: #8882; border-radius: 6px; padding: .1rem .5rem; }
.term { font-size: 1.1rem; }
.known { margin-left: auto; }
.known button { cursor: pointer; font-size: .8rem; padding: .25rem .7rem; border: 1px solid #8886; border-radius: 6px; background: transparent; color: inherit; }
.known button:hover { background: #8882; }
.def { margin-top: .4rem; }
.ex { margin-top: .25rem; color: #8889; font-style: italic; }
.sentence { margin: .4rem 0 0; padding-left: .8rem; border-left: 3px solid #2563eb; }
.reason { margin-top: .4rem; color: #8889; font-size: .9rem; }
";

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
