mod auth;
mod db;
mod extract;
mod fetch;
mod models;
mod tts;

use axum::{
    extract::{DefaultBodyLimit, FromRef, Multipart, Path, Query, State},
    http::{header, StatusCode},
    middleware,
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Form, Router,
};
use axum_extra::extract::cookie::Key;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

use auth::OAuthConfig;
use db::Db;
use extract::Extractor;
use models::Category;

#[derive(Clone)]
struct AppState {
    db: Db,
    extractor: Arc<Extractor>,
    oauth: Arc<OAuthConfig>,
    /// ElevenLabs TTS(설정됐을 때만). 없으면 읽어주기 버튼 미노출.
    tts: Option<Arc<tts::Tts>>,
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
    let tts = tts::Tts::from_env().map(Arc::new);
    if tts.is_some() {
        tracing::info!("ElevenLabs TTS 활성화(기사 읽어주기)");
    }
    let key = auth::cookie_key();
    let state = AppState {
        db,
        extractor,
        oauth,
        tts,
        key,
    };

    // 앱 라우트는 require_auth 게이트 뒤에 두고, /auth/* 는 공개로 둔다.
    let protected = Router::new()
        .route("/", get(index))
        .route("/entries", get(list_entries_page).post(create_entry))
        .route("/entries/:id", get(entry_detail))
        .route("/entries/:id/print", get(print_entry))
        .route("/entries/:id/tts", get(entry_tts))
        .route("/entries/:id/chunks", get(entry_chunks))
        .route("/entries/:id/mindmap", get(entry_mindmap))
        .route("/entries/:id/summary", get(entry_summary))
        .route("/entries/:id/append", post(append_entry))
        .route("/entries/:id/edit", post(edit_entry))
        .route("/entries/:id/delete", post(delete_entry))
        .route("/words", get(list_words))
        .route("/words/known", post(mark_known))
        .route("/words/roots", get(word_roots))
        .route("/words/print", get(print_words))
        .route("/sentences", get(list_sentences))
        .route("/sentences/print", get(print_sentences))
        .route("/sentences/review", get(grammar_review))
        .route("/sentences/grammar", get(sentence_grammar))
        .route("/sentences/point", get(sentence_point))
        .route("/review", get(review))
        .route("/export/words.csv", get(export_words_csv).post(export_words_csv_sel))
        .route("/export/words.tsv", get(export_words_anki).post(export_words_anki_sel))
        .route("/export/sentences.csv", get(export_sentences_csv))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_auth));

    let app = Router::new()
        .merge(protected)
        .route("/auth/login", get(auth::auth_login))
        .route("/auth/callback", get(auth::auth_callback))
        .route("/auth/logout", get(auth::auth_logout))
        // PDF 업로드를 위해 기본 2MB 본문 제한을 25MB로 확대.
        .layer(DefaultBodyLimit::max(25 * 1024 * 1024))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<String> {
    let body = format!("{}{}", nav("home"), include_str!("../static/index.html"));
    page("붙여넣기", &body)
}

/// 본문 붙여넣기 / URL / PDF 업로드 → 원문 저장 → Claude 추출 → 단어/문장 저장.
/// 입력은 multipart. 우선순위: PDF > 붙여넣기 > URL.
/// 저장 후에는 해당 기사 상세로 보내 원문이 사라지지 않게 한다.
async fn create_entry(
    State(st): State<AppState>,
    mut mp: Multipart,
) -> Result<Redirect, AppError> {
    let mut category = String::new();
    let mut source_detail: Option<String> = None;
    let mut source_url: Option<String> = None;
    let mut raw_text = String::new();
    let mut pdf_bytes: Option<Vec<u8>> = None;
    let mut pdf_name: Option<String> = None;

    while let Some(field) = mp.next_field().await.map_err(|e| AppError(e.to_string()))? {
        match field.name().unwrap_or("") {
            "category" => category = field.text().await.map_err(|e| AppError(e.to_string()))?,
            "source_detail" => {
                source_detail = nonempty(Some(field.text().await.map_err(|e| AppError(e.to_string()))?))
            }
            "source_url" => {
                source_url = nonempty(Some(field.text().await.map_err(|e| AppError(e.to_string()))?))
            }
            "raw_text" => raw_text = field.text().await.map_err(|e| AppError(e.to_string()))?,
            "pdf" => {
                pdf_name = nonempty(field.file_name().map(str::to_string));
                let data = field.bytes().await.map_err(|e| AppError(e.to_string()))?;
                if !data.is_empty() {
                    pdf_bytes = Some(data.to_vec());
                }
            }
            _ => {}
        }
    }

    let cat = Category::parse(&category)
        .ok_or_else(|| AppError(format!("invalid category: {category}")))?;
    let url = source_url;

    // 입력 우선순위: PDF > 붙여넣기 > URL.
    let text = if let Some(bytes) = pdf_bytes {
        // PDF 파싱은 CPU 바운드라 블로킹 풀에서 처리한다.
        tokio::task::spawn_blocking(move || fetch::extract_pdf_text(&bytes))
            .await
            .map_err(|e| AppError(e.to_string()))?
            .map_err(AppError::from)?
    } else if !raw_text.trim().is_empty() {
        raw_text.trim().to_string()
    } else if let Some(u) = url.as_deref() {
        fetch::fetch_article(u).await.map_err(AppError::from)?
    } else {
        return Err(AppError("본문을 붙여넣거나 URL·PDF를 입력해 주세요.".into()));
    };

    // 출처 제목이 비었으면 PDF 파일명을 제목으로 대신 쓴다.
    let detail = source_detail.or_else(|| {
        pdf_name.map(|n| n.trim_end_matches(".pdf").trim_end_matches(".PDF").to_string())
    });

    let entry_id = st
        .db
        .insert_entry(cat, &text, detail.as_deref(), url.as_deref())
        .await
        .map_err(AppError::from)?;

    let known = st.db.known_terms().await.map_err(AppError::from)?;
    let extraction = st
        .extractor
        .extract_chunked(&text, &known)
        .await
        .map_err(AppError::from)?;

    for w in &extraction.words {
        st.db
            .insert_word(cat, entry_id, w, detail.as_deref(), url.as_deref())
            .await
            .map_err(AppError::from)?;
    }
    for s in &extraction.sentences {
        st.db
            .insert_sentence(cat, entry_id, s, detail.as_deref(), url.as_deref())
            .await
            .map_err(AppError::from)?;
    }

    Ok(Redirect::to(&format!("/entries/{entry_id}")))
}

/// 저장된 기사 목록.
async fn list_entries_page(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, AppError> {
    let cat = q.get("category").and_then(|s| Category::parse(s));
    let entries: Vec<_> = st
        .db
        .list_entries()
        .await
        .map_err(AppError::from)?
        .into_iter()
        .filter(|e| cat.is_none_or(|c| e.category == c))
        .collect();

    let mut body = format!(
        "{}<h1>내 기사</h1>{}",
        nav("entries"),
        category_filter("/entries", cat)
    );
    if entries.is_empty() {
        body.push_str(
            "<p class=\"empty\">이 카테고리에는 저장된 기사가 없습니다. \
             <a href=\"/\">본문을 붙여넣거나 URL을 입력</a>해 보세요.</p>",
        );
    } else {
        body.push_str(&format!("<p class=\"count\">{}건</p><ul class=\"cards\">", entries.len()));
        for e in &entries {
            let title = e
                .source_detail
                .clone()
                .unwrap_or_else(|| snippet(&e.raw_text, 40));
            body.push_str(&format!(
                "<li class=\"card entry\">\
                   <a class=\"entry-link\" href=\"/entries/{id}\">\
                     <div class=\"head\"><span class=\"badge\">{cat}</span>\
                       <b class=\"term\">{title}</b></div>\
                     <div class=\"ex\">{snippet}</div>\
                     <div class=\"reason\">{time}</div>\
                   </a>\
                   <form class=\"del\" method=\"post\" action=\"/entries/{id}/delete\" \
                     onsubmit=\"return confirm('이 기사를 삭제할까요? 추출된 단어·문장도 목록에서 사라집니다.')\">\
                     <button title=\"기사 삭제\" aria-label=\"삭제\">🗑</button>\
                   </form>\
                 </li>",
                id = e.id,
                cat = esc(e.category.label()),
                title = esc(&title),
                snippet = esc(&snippet(&e.raw_text, 160)),
                time = esc(&fmt_time(e.created_at)),
            ));
        }
        body.push_str("</ul>");
    }
    Ok(page("내 기사", &body))
}

/// 기사 상세: 원문 + 이어쓰기 폼 + 이 기사에서 뽑힌 단어/문장.
async fn entry_detail(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, AppError> {
    let Some(e) = st.db.get_entry(id).await.map_err(AppError::from)? else {
        let body = format!(
            "{}<h1>기사를 찾을 수 없습니다</h1>\
             <p class=\"empty\"><a href=\"/entries\">기사 목록으로</a></p>",
            nav("entries")
        );
        return Ok(page("기사 없음", &body));
    };
    let words = st.db.words_for_entry(id).await.map_err(AppError::from)?;
    let sentences = st.db.sentences_for_entry(id).await.map_err(AppError::from)?;

    let mut body = format!("{}<h1>기사</h1>", nav("entries"));

    // 메타(카테고리/출처/시각)
    body.push_str(&format!(
        "<div class=\"head\"><span class=\"badge\">{cat}</span>{detail}<span class=\"reason\">{time}</span></div>",
        cat = esc(e.category.label()),
        detail = e
            .source_detail
            .as_deref()
            .map(|d| format!("<b class=\"term\">{}</b>", esc(d)))
            .unwrap_or_default(),
        time = esc(&fmt_time(e.created_at)),
    ));
    if let Some(u) = e.source_url.as_deref() {
        body.push_str(&format!(
            "<div class=\"export\"><a href=\"{u}\" target=\"_blank\" rel=\"noreferrer\">원문 링크 ↗</a></div>",
            u = esc(u),
        ));
    }
    // PDF 인쇄(2단) 링크
    body.push_str(&format!(
        "<div class=\"export\"><a href=\"/entries/{id}/print\">🖨 PDF 인쇄 (2단)</a></div>"
    ));

    // 삭제 버튼
    body.push_str(&format!(
        "<form class=\"del detail\" method=\"post\" action=\"/entries/{id}/delete\" \
           onsubmit=\"return confirm('이 기사를 삭제할까요? 추출된 단어·문장도 목록에서 사라집니다.')\">\
           <button>🗑 기사 삭제</button>\
         </form>"
    ));

    // 기사 읽기 전에: 구조 마인드맵(중앙 제목 + 주요 섹션/키워드).
    // 로드 시 클라이언트가 /entries/:id/mindmap 을 fetch해 그린다(캐시 재사용).
    body.push_str(&format!(
        "<h2 class=\"mm-h2\">🧭 구조 한눈에 보기</h2>\
         <div class=\"card mindmap-card\">\
           <div id=\"mindmap\" data-entry=\"{id}\"><span class=\"muted\">기사 구조 분석 중…</span></div>\
         </div>"
    ));

    // 원문(리더 뷰 + 어휘 하이라이트).
    // 이 기사의 단어를 소문자 변형 → 뜻 맵으로 만들어 본문 속 등장 위치에 밑줄/뜻을 붙인다.
    let vocab = build_vocab(&words);
    let tts_ctrl = if st.tts.is_some() {
        format!(
            "<button type=\"button\" id=\"ttsbtn\" class=\"edit-toggle\" data-entry=\"{id}\">🔊 읽어주기</button>\
             <select id=\"ttsrate\" class=\"edit-toggle\" title=\"재생 속도\">\
               <option value=\"0.75\">0.75×</option><option value=\"1\" selected>1.0×</option>\
               <option value=\"1.25\">1.25×</option><option value=\"1.5\">1.5×</option>\
             </select>"
        )
    } else {
        String::new()
    };
    body.push_str(&format!(
        "<div class=\"reader-head\"><h2>📖 기사 읽기</h2>{tts_ctrl}\
         <button type=\"button\" id=\"chunkbtn\" class=\"edit-toggle\" data-entry=\"{id}\" title=\"의미 단위(구)로 끊어 계단식으로 보기\">🧩 청크 리딩</button>\
         <select id=\"toeic\" class=\"edit-toggle\" title=\"토익 점수대(CEFR) = 읽기 속도(WPM). 연구 규준 기반\">\
           <option value=\"95\">TOEIC ~224 (A1) · 95 WPM</option>\
           <option value=\"115\">TOEIC 225–549 (A2) · 115 WPM</option>\
           <option value=\"140\" selected>TOEIC 550–784 (B1) · 140 WPM</option>\
           <option value=\"170\">TOEIC 785–944 (B2) · 170 WPM</option>\
           <option value=\"200\">TOEIC 945+ (C1) · 200 WPM</option>\
           <option value=\"240\">원어민 수준 · 240 WPM</option>\
         </select>\
         <button type=\"button\" id=\"pacebtn\" class=\"edit-toggle\" title=\"선택 속도로 하이라이트만 진행(음성 없음)\">🏃 속도 읽기</button>\
         <button type=\"button\" id=\"editbtn\" class=\"edit-toggle\">✏️ 편집</button></div>"
    ));
    // 청크 리딩/read-along에서 어휘 밑줄을 다시 그리도록 vocab 맵(소문자 변형 키→뜻)을 실어보낸다.
    if !vocab.is_empty() {
        let vjson = serde_json::to_string(&vocab)
            .unwrap_or_else(|_| "{}".into())
            .replace('<', "\\u003c");
        body.push_str(&format!(
            "<script id=\"reader-vocab\" type=\"application/json\">{vjson}</script>"
        ));
    }
    if st.tts.is_some() {
        body.push_str(
            "<audio id=\"ttsaudio\" class=\"tts-audio noprint\" controls hidden preload=\"none\"></audio>",
        );
    }
    if !vocab.is_empty() {
        body.push_str(
            "<div class=\"reader-hint\">밑줄 친 단어에 마우스를 올리면 뜻이 보여요.</div>",
        );
    }
    // 읽기 뷰(렌더) + 편집 폼(원문 textarea). 편집 폼은 기본 숨김, JS로 토글.
    body.push_str(&format!(
        "<div id=\"reader-view\" class=\"card article\"><article class=\"reader\">{view}</article></div>\
         <form id=\"reader-edit\" class=\"card reader-edit\" action=\"/entries/{id}/edit\" method=\"post\">\
           <div class=\"edit-hint\">필요 없는 부분을 지우고 저장하세요. 추출된 단어는 그대로 유지되고, \
            마인드맵·요약은 다음에 열 때 새로 생성됩니다.</div>\
           <textarea name=\"raw_text\" required>{body_text}</textarea>\
           <div class=\"edit-actions\">\
             <button type=\"submit\">저장</button>\
             <button type=\"button\" id=\"editcancel\" class=\"ghost\">취소</button>\
           </div>\
         </form>",
        view = render_article(&e.raw_text, &vocab),
        id = id,
        body_text = esc(&e.raw_text),
    ));

    // 한글 요약 초안(블로그 + X 스레드) — 온디맨드 생성 후 편집/복사.
    body.push_str(&format!(
        "<h2>📝 한글 요약 (블로그·X 초안)</h2>\
         <div class=\"card\">\
           <button id=\"sumbtn\" class=\"gen-btn\" data-entry=\"{id}\">📝 한글 요약 초안 생성</button>\
           <div id=\"summary\"></div>\
         </div>"
    ));

    // 이어쓰기: 새 텍스트를 붙이고 그 부분만 추가 추출한다.
    body.push_str(&format!(
        "<h2>이어서 보완</h2>\
         <form class=\"paste\" action=\"/entries/{id}/append\" method=\"post\">\
           <textarea name=\"raw_text\" required placeholder=\"이어질 본문을 붙여넣으면 그 부분만 추가로 추출합니다\"></textarea>\
           <button type=\"submit\">이어서 추출</button>\
         </form>"
    ));

    // 이 기사에서 뽑힌 단어
    body.push_str(&format!("<h2>이 기사의 단어 <span class=\"count\">{}개</span></h2>", words.len()));
    if words.is_empty() {
        body.push_str("<p class=\"empty\">추출된 단어가 없습니다.</p>");
    } else {
        body.push_str("<ul class=\"cards\">");
        for (term, def, ex) in &words {
            body.push_str(&format!(
                "<li class=\"card\"><div class=\"head\"><b class=\"term\">{term}</b></div>\
                 <div class=\"def\">{def}</div><div class=\"ex\">{ex}</div></li>",
                term = esc(term),
                def = esc(def),
                ex = esc(ex),
            ));
        }
        body.push_str("</ul>");
    }

    // 이 기사에서 뽑힌 문장
    body.push_str(&format!(
        "<h2>이 기사의 문장 <span class=\"count\">{}개</span></h2>",
        sentences.len()
    ));
    if sentences.is_empty() {
        body.push_str("<p class=\"empty\">추출된 문장이 없습니다.</p>");
    } else {
        body.push_str("<ul class=\"cards\">");
        for (t, reason) in &sentences {
            body.push_str(&format!(
                "<li class=\"card\"><blockquote class=\"sentence\">{t}</blockquote>\
                 <div class=\"reason\">💡 {reason}</div></li>",
                t = esc(t),
                reason = esc(reason),
            ));
        }
        body.push_str("</ul>");
    }

    body.push_str(&format!("<script>{MINDMAP_JS}</script>"));
    body.push_str(&format!("<script>{SUMMARY_JS}</script>"));
    body.push_str(&format!("<script>{READER_EDIT_JS}</script>"));
    body.push_str(&format!("<script>{READER_JS}</script>"));
    Ok(page("기사", &body))
}

/// 기사 인쇄(PDF) 뷰: 본문을 2단(신문식)으로 흐르게 배치. 어휘 하이라이트도 유지.
async fn print_entry(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, AppError> {
    let Some(e) = st.db.get_entry(id).await.map_err(AppError::from)? else {
        let body = format!(
            "{}<h1>기사를 찾을 수 없습니다</h1>\
             <p class=\"empty\"><a href=\"/entries\">기사 목록으로</a></p>",
            nav("entries")
        );
        return Ok(page("기사 없음", &body));
    };
    let words = st.db.words_for_entry(id).await.map_err(AppError::from)?;
    let vocab = build_vocab(&words);

    let title = e.source_detail.clone().unwrap_or_else(|| "기사".to_string());
    let mut body = format!(
        "<div class=\"toolbar noprint\">\
           <a class=\"chip\" href=\"/entries/{id}\">← 기사</a>\
           <button id=\"printbtn\" onclick=\"window.print()\">🖨 PDF로 저장 / 인쇄</button>\
         </div>\
         <h1 class=\"print-title\">{title}</h1>\
         <div class=\"reason print-meta\">{cat} · {time}</div>",
        title = esc(&title),
        cat = esc(e.category.label()),
        time = esc(&fmt_time(e.created_at)),
    );
    body.push_str(&format!(
        "<div class=\"reader cols\">{}</div>",
        render_article(&e.raw_text, &vocab)
    ));
    Ok(page("기사 인쇄", &body))
}

/// 기존 기사에 본문을 이어 붙이고, 추가된 부분만 추출한다.
async fn append_entry(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Form(f): Form<HashMap<String, String>>,
) -> Result<Redirect, AppError> {
    let back = format!("/entries/{id}");
    let added = f.get("raw_text").map(|s| s.trim().to_string()).unwrap_or_default();
    if added.is_empty() {
        return Ok(Redirect::to(&back));
    }

    let mut entry = st
        .db
        .get_entry(id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError("entry not found".into()))?;

    // 원문에 이어 붙여 전체 행을 다시 저장(REPLACE).
    entry.raw_text = format!("{}\n\n{}", entry.raw_text, added);
    st.db.save_entry(&entry).await.map_err(AppError::from)?;
    // 본문이 바뀌었으니 마인드맵·요약 캐시를 무효화(빈 값으로 덮어써 재생성 유도).
    st.db.save_entry_mindmap(id, "").await.map_err(AppError::from)?;
    st.db.save_entry_summary(id, "").await.map_err(AppError::from)?;

    // 추가된 부분만 추출해 중복을 줄인다(길면 청크로 나눠 처리).
    let known = st.db.known_terms().await.map_err(AppError::from)?;
    let extraction = st.extractor.extract_chunked(&added, &known).await.map_err(AppError::from)?;
    for w in &extraction.words {
        st.db
            .insert_word(entry.category, id, w, entry.source_detail.as_deref(), entry.source_url.as_deref())
            .await
            .map_err(AppError::from)?;
    }
    for s in &extraction.sentences {
        st.db
            .insert_sentence(entry.category, id, s, entry.source_detail.as_deref(), entry.source_url.as_deref())
            .await
            .map_err(AppError::from)?;
    }

    Ok(Redirect::to(&back))
}

/// 기사 본문 편집: 리더 뷰에서 필요 없는 부분을 지우고 저장한다.
/// 추출된 단어/문장은 건드리지 않고 원문만 교체하며, 본문이 바뀌었으니
/// 마인드맵·요약 캐시를 무효화(빈 값)해 다음 조회 때 새로 생성되게 한다.
async fn edit_entry(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Form(f): Form<HashMap<String, String>>,
) -> Result<Redirect, AppError> {
    let back = format!("/entries/{id}");
    let new_text = f.get("raw_text").map(|s| s.trim()).unwrap_or("").to_string();
    // 빈 저장은 사고 방지를 위해 무시(본문을 통째로 비우지 않도록).
    if new_text.is_empty() {
        return Ok(Redirect::to(&back));
    }

    let mut entry = st
        .db
        .get_entry(id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError("entry not found".into()))?;

    // 변경이 없으면 그대로 돌아간다(불필요한 쓰기·캐시 무효화 회피).
    if entry.raw_text.trim() == new_text {
        return Ok(Redirect::to(&back));
    }

    entry.raw_text = new_text;
    st.db.save_entry(&entry).await.map_err(AppError::from)?;
    st.db.save_entry_mindmap(id, "").await.map_err(AppError::from)?;
    st.db.save_entry_summary(id, "").await.map_err(AppError::from)?;

    Ok(Redirect::to(&back))
}

/// 기사 삭제 → 기사 목록으로. 이 기사의 단어/문장은 조회에서 걸러진다.
async fn delete_entry(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Redirect, AppError> {
    st.db.delete_entry(id).await.map_err(AppError::from)?;
    Ok(Redirect::to("/entries"))
}

async fn list_words(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, AppError> {
    let cat = q.get("category").and_then(|s| Category::parse(s));
    let words = st.db.list_words(cat).await.map_err(AppError::from)?;

    let mut body = format!("{}<h1>단어장</h1>{}", nav("words"), category_filter("/words", cat));
    body.push_str(&format!(
        "<div class=\"export\">내보내기 · <a href=\"/export/words.csv{q}\">CSV</a> · \
         <a href=\"/export/words.tsv{q}\">Anki(TSV)</a> · \
         <a href=\"/words/print{q}\">🖨 PDF 인쇄</a> · \
         <a href=\"/words/print{qd}\">📅 날짜별 인쇄</a></div>",
        q = cat_query(cat),
        qd = match cat {
            Some(c) => format!("?category={}&by=date", c.as_str()),
            None => "?by=date".to_string(),
        },
    ));
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
                   <div class=\"roots-row\">\
                     <button class=\"roots-btn\" data-term=\"{term}\">🌱 어근 분석</button>\
                   </div>\
                   <div class=\"roots\" hidden></div>\
                 </li>",
                cat = esc(c.label()),
                catkey = esc(c.as_str()),
                term = esc(term),
                def = esc(def),
                ex = esc(ex),
            ));
        }
        body.push_str("</ul>");
        body.push_str(&format!("<script>{ROOTS_JS}</script>"));
    }
    Ok(page("단어장", &body))
}

/// 인쇄(PDF) 전용 단어장 뷰: 2단 레이아웃 + 각 단어의 어근 분석까지 포함.
/// 어근 분석은 클라이언트가 /words/roots로 자동 로드(캐시 재사용, 미분석분은 생성)한다.
async fn print_words(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, AppError> {
    let cat = q.get("category").and_then(|s| Category::parse(s));
    let by_date = q.get("by").map(|v| v == "date").unwrap_or(false);
    let mut words = st.db.list_words_dated(cat).await.map_err(AppError::from)?;
    if by_date {
        // 날짜(최신순) → 같은 날 안에서는 알파벳순.
        words.sort_by(|a, b| {
            fmt_date(b.4)
                .cmp(&fmt_date(a.4))
                .then(a.1.to_lowercase().cmp(&b.1.to_lowercase()))
        });
    } else {
        // 사전처럼 찾기 쉽게 알파벳순.
        words.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    }

    let scope = cat.map_or("전체", |c| c.label());
    let mut body = format!(
        "{}<h1>단어장 인쇄 · {scope}{by}</h1>\
         <div class=\"toolbar noprint\">\
           <a class=\"chip\" href=\"/words{q}\">← 단어장</a>\
           <button type=\"button\" class=\"chip\" id=\"selall\">전체 선택</button>\
           <button type=\"button\" class=\"chip\" id=\"selnone\">전체 해제</button>\
           <span id=\"prog\"></span>\
           <button type=\"button\" class=\"chip\" id=\"expcsv\">⬇ 선택 CSV</button>\
           <button type=\"button\" class=\"chip\" id=\"expanki\">⬇ 선택 Anki</button>\
           <button id=\"printbtn\">🖨 선택 단어 인쇄</button>\
         </div>\
         <form id=\"expform\" method=\"post\" class=\"noprint\" hidden>\
           <input type=\"hidden\" name=\"category\" value=\"{catkey}\">\
           <input type=\"hidden\" name=\"terms\" id=\"expterms\">\
         </form>\
         <p class=\"noprint muted pick-hint\">체크한 단어만 인쇄·내보내기됩니다. 어근 분석은 인쇄할 때 선택한 단어만 불러옵니다.</p>",
        nav("words"),
        by = if by_date { " · 날짜별" } else { "" },
        q = cat_query(cat),
        catkey = cat.map_or("", |c| c.as_str()),
    );

    if words.is_empty() {
        body.push_str("<p class=\"empty\">인쇄할 단어가 없습니다.</p>");
    } else {
        // 날짜별 모드: 상단에 날짜 선택 바(고유 날짜 + 개수). 체크한 날짜만 인쇄된다.
        if by_date {
            let mut dates: Vec<(String, usize)> = Vec::new();
            for w in &words {
                let d = fmt_date(w.4);
                match dates.last_mut() {
                    Some((ld, cnt)) if *ld == d => *cnt += 1,
                    _ => dates.push((d, 1)),
                }
            }
            body.push_str("<div class=\"datebar noprint\"><span class=\"muted\">날짜 선택:</span>");
            for (d, cnt) in &dates {
                body.push_str(&format!(
                    "<label class=\"datechip\"><input type=\"checkbox\" class=\"datebox\" data-date=\"{d}\" checked> 📅 {d} <span class=\"muted\">({cnt})</span></label>",
                    d = esc(d),
                ));
            }
            body.push_str("</div>");
        }

        body.push_str("<ul class=\"print-words\">");
        let mut cur_date = String::new();
        for (c, term, def, ex, ts) in &words {
            let d = fmt_date(*ts);
            // 날짜별 모드: 날짜가 바뀌면 그 날 헤더(개수 포함)를 먼저 넣는다.
            if by_date && d != cur_date {
                let cnt = words.iter().filter(|w| fmt_date(w.4) == d).count();
                body.push_str(&format!(
                    "<li class=\"date-sep\" data-date=\"{d}\">📅 {d} <span class=\"muted\">({cnt}개)</span></li>",
                    d = esc(&d),
                ));
                cur_date = d.clone();
            }
            // 날짜별 모드에서는 각 단어에도 추가된 날짜를 붙이고(인쇄 표시), data-date로 묶는다.
            let datelbl = if by_date {
                format!("<span class=\"wdate\">🗓 {}</span>", esc(&d))
            } else {
                String::new()
            };
            let dateattr = if by_date {
                format!(" data-date=\"{}\"", esc(&d))
            } else {
                String::new()
            };
            body.push_str(&format!(
                "<li class=\"card\"{dateattr}>\
                   <div class=\"head\">\
                     <label class=\"pick noprint\"><input type=\"checkbox\" class=\"pickbox\" checked></label>\
                     <span class=\"badge\">{cat}</span><b class=\"term\">{term}</b>{datelbl}</div>\
                   <div class=\"def\">{def}</div>\
                   <div class=\"ex\">{ex}</div>\
                   <div class=\"roots\" data-term=\"{term}\"><span class=\"muted\">인쇄 시 어근 분석 포함</span></div>\
                 </li>",
                cat = esc(c.label()),
                term = esc(term),
                def = esc(def),
                ex = esc(ex),
            ));
        }
        body.push_str("</ul>");
        body.push_str(&format!("<script>{PRINT_JS}</script>"));
    }
    Ok(page("단어장 인쇄", &body))
}

/// 인쇄(PDF) 전용 베스트 문장 뷰. `?by=date`면 날짜별로 묶고 날짜 선택 바를 둔다.
/// 각 문장에 출처 기사 제목(source_detail)을 함께 출력한다.
async fn print_sentences(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, AppError> {
    let cat = q.get("category").and_then(|s| Category::parse(s));
    let by_date = q.get("by").map(|v| v == "date").unwrap_or(false);
    let mut sents = st.db.list_sentences_dated(cat).await.map_err(AppError::from)?;
    if by_date {
        // 날짜(최신순) → 같은 날 안에서는 기사 제목별 → 그 안에서는 최신순.
        sents.sort_by(|a, b| {
            fmt_date(b.4)
                .cmp(&fmt_date(a.4))
                .then(a.3.cmp(&b.3))
                .then(b.4.cmp(&a.4))
        });
    } else {
        // 최신순.
        sents.sort_by(|a, b| b.4.cmp(&a.4));
    }

    let scope = cat.map_or("전체", |c| c.label());
    let mut body = format!(
        "{}<h1>베스트 문장 인쇄 · {scope}{by}</h1>\
         <div class=\"toolbar noprint\">\
           <a class=\"chip\" href=\"/sentences{q}\">← 문장</a>\
           <button type=\"button\" class=\"chip\" id=\"selall\">전체 선택</button>\
           <button type=\"button\" class=\"chip\" id=\"selnone\">전체 해제</button>\
           <span id=\"prog\"></span>\
           <button id=\"printbtn\">🖨 선택 문장 인쇄</button>\
         </div>\
         <p class=\"noprint muted pick-hint\">체크한 문장만 인쇄됩니다. 각 문장에 출처 기사 제목이 함께 출력됩니다.</p>",
        nav("sentences"),
        by = if by_date { " · 날짜별" } else { "" },
        q = cat_query(cat),
    );

    if sents.is_empty() {
        body.push_str("<p class=\"empty\">인쇄할 문장이 없습니다.</p>");
    } else {
        // 날짜별: 상단에 날짜 선택 바(고유 날짜 + 개수). 체크한 날짜만 인쇄된다.
        if by_date {
            let mut dates: Vec<(String, usize)> = Vec::new();
            for s in &sents {
                let d = fmt_date(s.4);
                match dates.last_mut() {
                    Some((ld, cnt)) if *ld == d => *cnt += 1,
                    _ => dates.push((d, 1)),
                }
            }
            body.push_str("<div class=\"datebar noprint\"><span class=\"muted\">날짜 선택:</span>");
            for (d, cnt) in &dates {
                body.push_str(&format!(
                    "<label class=\"datechip\"><input type=\"checkbox\" class=\"datebox\" data-date=\"{d}\" checked> 📅 {d} <span class=\"muted\">({cnt})</span></label>",
                    d = esc(d),
                ));
            }
            body.push_str("</div>");
        }

        body.push_str("<ul class=\"print-sents\">");
        let mut cur_date = String::new();
        let mut cur_src = String::new();
        for (c, textv, reason, title, ts) in &sents {
            let d = fmt_date(*ts);
            if by_date && d != cur_date {
                let cnt = sents.iter().filter(|s| fmt_date(s.4) == d).count();
                body.push_str(&format!(
                    "<li class=\"date-sep\" data-date=\"{d}\">📅 {d} <span class=\"muted\">({cnt}개)</span></li>",
                    d = esc(&d),
                ));
                cur_date = d.clone();
                cur_src = String::new(); // 새 날짜 → 제목 그룹 초기화
            }
            // 날짜별: 같은 날 안에서 기사 제목이 바뀌면 제목 소제목(개수 포함).
            if by_date && *title != cur_src {
                let tcnt = sents
                    .iter()
                    .filter(|s| fmt_date(s.4) == d && &s.3 == title)
                    .count();
                let tlabel = if title.trim().is_empty() {
                    "(제목 없음)".to_string()
                } else {
                    esc(title)
                };
                body.push_str(&format!(
                    "<li class=\"src-sep\" data-date=\"{d}\" data-src=\"{srck}\">📄 {tlabel} <span class=\"muted\">({tcnt}개)</span></li>",
                    d = esc(&d),
                    srck = esc(title),
                ));
                cur_src = title.clone();
            }
            let datelbl = if by_date {
                format!("<span class=\"wdate\">🗓 {}</span>", esc(&d))
            } else {
                String::new()
            };
            let dateattr = if by_date {
                format!(" data-date=\"{}\" data-src=\"{}\"", esc(&d), esc(title))
            } else {
                String::new()
            };
            // 날짜별 모드에서는 제목이 소제목으로 묶이므로 카드별 제목은 생략(중복 방지).
            // 일반 인쇄에서는 각 문장에 제목을 붙인다.
            let src = if !by_date && !title.trim().is_empty() {
                format!("<div class=\"src\">📄 {}</div>", esc(title))
            } else {
                String::new()
            };
            body.push_str(&format!(
                "<li class=\"card\"{dateattr}>\
                   <div class=\"head\">\
                     <label class=\"pick noprint\"><input type=\"checkbox\" class=\"pickbox\" checked></label>\
                     <span class=\"badge\">{cat}</span>{datelbl}</div>\
                   {src}\
                   <blockquote class=\"sentence\">{textv}</blockquote>\
                   <div class=\"reason\">💡 {reason}</div>\
                 </li>",
                cat = esc(c.label()),
                textv = esc(textv),
                reason = esc(reason),
            ));
        }
        body.push_str("</ul>");
        body.push_str(&format!("<script>{SENT_PRINT_JS}</script>"));
    }
    Ok(page("베스트 문장 인쇄", &body))
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
    body.push_str(&format!(
        "<div class=\"export\">내보내기 · <a href=\"/export/sentences.csv{q}\">CSV</a> · \
         <a href=\"/sentences/print{q}\">🖨 PDF 인쇄</a> · \
         <a href=\"/sentences/print{qd}\">📅 날짜별 인쇄</a> · \
         <a href=\"/sentences/review\">🎴 문법 카드로 복습</a></div>",
        q = cat_query(cat),
        qd = match cat {
            Some(c) => format!("?category={}&by=date", c.as_str()),
            None => "?by=date".to_string(),
        },
    ));
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
                   <div class=\"gram-actions\">\
                     <button class=\"gram-btn\" title=\"이 문장의 구조 그래프 + 문법 포인트\">🔍 문법 분석</button>\
                   </div>\
                   <div class=\"gram\" data-text=\"{text}\" hidden></div>\
                 </li>",
                cat = esc(c.label()),
                text = esc(text),
                reason = esc(reason),
            ));
        }
        body.push_str("</ul>");
        body.push_str(&format!(
            "<script>{GRAPH_RENDER_JS}</script><script>{SENTENCE_GRAPH_JS}</script>"
        ));
    }
    Ok(page("베스트 문장", &body))
}

/// 문법 카드 복습: 모든 베스트 문장을 덱(JSON)으로 내려보내고, 클라이언트가 한 장씩
/// 넘기며 '구조 보기' 시 /sentences/grammar를 fetch해 공유 렌더러로 그래프를 그린다.
async fn grammar_review(State(st): State<AppState>) -> Result<Html<String>, AppError> {
    let sentences = st.db.list_sentences(None).await.map_err(AppError::from)?;
    let deck: Vec<serde_json::Value> = sentences
        .iter()
        .map(|(c, text, _reason)| serde_json::json!({ "category": c.label(), "text": text }))
        .collect();
    // `</script>` 깨짐 방지로 '<'를 유니코드 이스케이프(REVIEW_JS와 동일).
    let data = serde_json::to_string(&deck)
        .unwrap_or_else(|_| "[]".into())
        .replace('<', "\\u003c");

    let body = format!(
        "{nav}<h1>문법 카드 복습</h1><div id=\"rv\"></div>\
         <script id=\"deck\" type=\"application/json\">{data}</script>\
         <script>{render}</script><script>{js}</script>",
        nav = nav("sentences"),
        render = GRAPH_RENDER_JS,
        js = GRAMMAR_REVIEW_JS,
    );
    Ok(page("문법 카드 복습", &body))
}

/// 플래시카드 복습. 복습 대상 단어를 덱(JSON)으로 내려보내고,
/// 카드 넘김/뜻 보기/'안다' 표시는 클라이언트 JS가 처리한다('안다'는 /words/known 재사용).
async fn review(State(st): State<AppState>) -> Result<Html<String>, AppError> {
    let words = st.db.review_words().await.map_err(AppError::from)?;
    let deck: Vec<serde_json::Value> = words
        .iter()
        .map(|(c, term, def, ex)| {
            serde_json::json!({
                "term": term, "definition": def, "example": ex, "category": c.label(),
            })
        })
        .collect();
    // `</script>` 깨짐 방지로 '<'를 유니코드 이스케이프.
    let data = serde_json::to_string(&deck)
        .unwrap_or_else(|_| "[]".into())
        .replace('<', "\\u003c");

    let body = format!(
        "{nav}<h1>복습</h1><div id=\"rv\"></div>\
         <script id=\"deck\" type=\"application/json\">{data}</script>\
         <script>{js}</script>",
        nav = nav("review"),
        js = REVIEW_JS,
    );
    Ok(page("복습", &body))
}

type WordRow = (Category, String, String, String);

/// 단어 목록을 알파벳순(대소문자 무시)으로 정렬한 참조 벡터. 내보내기 출력용.
fn sorted_word_refs(words: &[WordRow]) -> Vec<&WordRow> {
    let mut rows: Vec<&WordRow> = words.iter().collect();
    rows.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    rows
}

/// 단어 목록 → CSV 본문 (term,definition,example,category). 알파벳순 정렬.
fn build_words_csv(words: &[WordRow]) -> String {
    let mut out = String::from("term,definition,example,category\r\n");
    for (c, term, def, ex) in sorted_word_refs(words) {
        out.push_str(&format!(
            "{},{},{},{}\r\n",
            csv_field(term),
            csv_field(def),
            csv_field(ex),
            csv_field(c.label()),
        ));
    }
    out
}

/// 단어 목록 → Anki TSV 본문. `#separator`/`#columns` 디렉티브로 import를 단순화. 알파벳순 정렬.
fn build_words_tsv(words: &[WordRow]) -> String {
    let mut out =
        String::from("#separator:tab\n#html:false\n#columns:term\tdefinition\texample\tcategory\n");
    for (c, term, def, ex) in sorted_word_refs(words) {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            tsv_field(term),
            tsv_field(def),
            tsv_field(ex),
            tsv_field(c.label()),
        ));
    }
    out
}

/// 선택된 term 집합(개행 구분)으로 필터. 비어 있으면 전체를 그대로 둔다.
fn filter_words_by_terms(words: Vec<WordRow>, terms: &str) -> Vec<WordRow> {
    let set: std::collections::HashSet<&str> =
        terms.lines().map(str::trim).filter(|s| !s.is_empty()).collect();
    if set.is_empty() {
        return words;
    }
    words.into_iter().filter(|(_, t, _, _)| set.contains(t.trim())).collect()
}

/// 단어를 CSV로 내보낸다. 카테고리 필터 존중(GET, 전체).
async fn export_words_csv(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, AppError> {
    let cat = q.get("category").and_then(|s| Category::parse(s));
    let words = st.db.list_words(cat).await.map_err(AppError::from)?;
    Ok(download(
        "text/csv; charset=utf-8",
        fname("words", cat, "csv"),
        build_words_csv(&words),
    ))
}

/// 선택한 단어만 CSV로 내보낸다(POST, 인쇄 페이지의 선택 → terms).
async fn export_words_csv_sel(
    State(st): State<AppState>,
    Form(f): Form<HashMap<String, String>>,
) -> Result<impl IntoResponse, AppError> {
    let cat = f.get("category").and_then(|s| Category::parse(s));
    let words = st.db.list_words(cat).await.map_err(AppError::from)?;
    let words = filter_words_by_terms(words, f.get("terms").map(String::as_str).unwrap_or(""));
    Ok(download(
        "text/csv; charset=utf-8",
        fname("words", cat, "csv"),
        build_words_csv(&words),
    ))
}

/// 단어를 Anki용 TSV로 내보낸다. 카테고리 필터 존중(GET, 전체).
async fn export_words_anki(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, AppError> {
    let cat = q.get("category").and_then(|s| Category::parse(s));
    let words = st.db.list_words(cat).await.map_err(AppError::from)?;
    Ok(download(
        "text/tab-separated-values; charset=utf-8",
        fname("words-anki", cat, "tsv"),
        build_words_tsv(&words),
    ))
}

/// 선택한 단어만 Anki TSV로 내보낸다(POST).
async fn export_words_anki_sel(
    State(st): State<AppState>,
    Form(f): Form<HashMap<String, String>>,
) -> Result<impl IntoResponse, AppError> {
    let cat = f.get("category").and_then(|s| Category::parse(s));
    let words = st.db.list_words(cat).await.map_err(AppError::from)?;
    let words = filter_words_by_terms(words, f.get("terms").map(String::as_str).unwrap_or(""));
    Ok(download(
        "text/tab-separated-values; charset=utf-8",
        fname("words-anki", cat, "tsv"),
        build_words_tsv(&words),
    ))
}

/// 베스트 문장을 CSV로 내보낸다 (text,reason,category). 카테고리 필터 존중.
async fn export_sentences_csv(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, AppError> {
    let cat = q.get("category").and_then(|s| Category::parse(s));
    let sentences = st.db.list_sentences(cat).await.map_err(AppError::from)?;
    let mut out = String::from("text,reason,category\r\n");
    for (c, text, reason) in &sentences {
        out.push_str(&format!(
            "{},{},{}\r\n",
            csv_field(text),
            csv_field(reason),
            csv_field(c.label()),
        ));
    }
    Ok(download(
        "text/csv; charset=utf-8",
        fname("sentences", cat, "csv"),
        out,
    ))
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

/// Option<String>에서 공백만 있거나 빈 값은 None으로 정리.
fn nonempty(o: Option<String>) -> Option<String> {
    o.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 원문 앞부분 미리보기(문자 기준, 넘치면 …).
fn snippet(s: &str, max: usize) -> String {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > max {
        let cut: String = t.chars().take(max).collect();
        format!("{cut}…")
    } else {
        t
    }
}

/// epoch millis → "YYYY-MM-DD HH:MM" (KST, UTC+9).
fn fmt_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| (dt + chrono::Duration::hours(9)).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// created_at(ms) → KST 기준 날짜(YYYY-MM-DD). 날짜별 그룹핑/인쇄용.
fn fmt_date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| (dt + chrono::Duration::hours(9)).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "(날짜 없음)".to_string())
}

/// 단어 어근 분석 JSON을 반환한다. 캐시에 있으면 즉시, 없으면 Claude로 생성 후 캐시.
async fn word_roots(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, AppError> {
    let term = q
        .get("term")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError("term이 필요합니다".into()))?;

    if let Some(cached) = st.db.get_word_roots(&term).await.map_err(AppError::from)? {
        return Ok(json_response(cached));
    }

    let analysis = st.extractor.analyze_roots(&term).await.map_err(AppError::from)?;
    let body = serde_json::to_string(&analysis).map_err(|e| AppError(e.to_string()))?;
    st.db.save_word_roots(&term, &body).await.map_err(AppError::from)?;
    Ok(json_response(body))
}

/// 문장 문법 그래프 JSON을 반환한다. 캐시에 있으면 즉시, 없으면 Claude로 생성 후 캐시.
/// 키는 문장 텍스트(?text=) — sentences 복합 PK를 건드리지 않고 word_roots와 같은 패턴.
async fn sentence_grammar(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, AppError> {
    let text = q
        .get("text")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError("text가 필요합니다".into()))?;

    // refresh=1이면 캐시를 무시하고 재생성(프롬프트 개선 후 낡은 캐시 갱신용).
    let refresh = q.get("refresh").map(|v| v == "1").unwrap_or(false);
    if !refresh {
        if let Some(cached) = st
            .db
            .get_sentence_grammar(&text)
            .await
            .map_err(AppError::from)?
        {
            return Ok(json_response(cached));
        }
    }

    let analysis = st
        .extractor
        .analyze_grammar(&text)
        .await
        .map_err(AppError::from)?;
    let body = serde_json::to_string(&analysis).map_err(|e| AppError(e.to_string()))?;
    st.db
        .save_sentence_grammar(&text, &body)
        .await
        .map_err(AppError::from)?;
    Ok(json_response(body))
}

/// 문법 포인트 상세(강의 본문) JSON을 반환한다. 캐시에 있으면 즉시, 없으면 Claude로 생성 후 캐시.
/// 키는 문장 텍스트(?text=) + 포인트(?point=). 사용자가 '자세히'를 누를 때만 호출된다.
async fn sentence_point(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, AppError> {
    let text = q
        .get("text")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError("text가 필요합니다".into()))?;
    let point = q
        .get("point")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError("point가 필요합니다".into()))?;

    if let Some(cached) = st
        .db
        .get_point_detail(&text, &point)
        .await
        .map_err(AppError::from)?
    {
        return Ok(json_response(cached));
    }

    let detail = st
        .extractor
        .analyze_point(&text, &point)
        .await
        .map_err(AppError::from)?;
    let body = serde_json::to_string(&detail).map_err(|e| AppError(e.to_string()))?;
    st.db
        .save_point_detail(&text, &point, &body)
        .await
        .map_err(AppError::from)?;
    Ok(json_response(body))
}

/// TTS 크레딧 보호용 본문 상한(글자). 넘으면 앞부분만 읽는다.
const MAX_TTS_CHARS: usize = 8000;
/// 청크 리딩 LLM 청킹 본문 상한(글자). 넘으면 LLM을 건너뛰고 빈 응답 → 클라이언트가 전체를
/// 규칙 기반으로 렌더(부분 잘림 방지). 이하 기사는 chunk_article이 조각내 전체를 LLM 청킹.
const MAX_CHUNK_CHARS: usize = 30000;

/// FNV-1a 64bit(캐시 파일명용). 본문이 바뀌면 해시도 바뀌어 자동으로 새로 생성된다.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 기사 본문을 ElevenLabs로 읽어 mp3로 반환. 같은 (기사·음성·모델·본문) 조합은 디스크에
/// 캐시해 재생 시 재과금하지 않는다. 크레딧 보호로 본문은 MAX_TTS_CHARS까지만 읽는다.
async fn entry_tts(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let tts = st
        .tts
        .clone()
        .ok_or_else(|| AppError("TTS가 설정되지 않았습니다(ELEVENLABS_API_KEY).".into()))?;
    let e = st
        .db
        .get_entry(id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError("기사를 찾을 수 없습니다".into()))?;

    let full = e.raw_text.trim();
    // 상한 넘으면 앞부분만(문자 경계로).
    let text: String = if full.chars().count() > MAX_TTS_CHARS {
        full.chars().take(MAX_TTS_CHARS).collect()
    } else {
        full.to_string()
    };
    if text.is_empty() {
        return Err(AppError("읽을 본문이 없습니다".into()));
    }

    // 디스크 캐시: <dir>/<id>-<voice-model>-<본문해시>.json (audio_base64 + 타임스탬프)
    let dir = std::env::var("TTS_CACHE_DIR").unwrap_or_else(|_| "tts-cache".into());
    let fname = format!("{}-{}-{:016x}.json", id, tts.cache_tag(), fnv1a(&text));
    let path = std::path::Path::new(&dir).join(fname);

    let json = if let Ok(s) = tokio::fs::read_to_string(&path).await {
        s
    } else {
        let s = tts.synthesize_json(&text).await.map_err(AppError::from)?;
        let _ = tokio::fs::create_dir_all(&dir).await;
        if let Err(e) = tokio::fs::write(&path, &s).await {
            tracing::warn!("TTS 캐시 저장 실패(무시): {e}");
        }
        s
    };

    Ok((
        [
            (header::CONTENT_TYPE, "application/json; charset=utf-8"),
            (header::CACHE_CONTROL, "private, max-age=86400"),
        ],
        json,
    ))
}

/// 청크 리딩 JSON(문단별 구 단위)을 반환한다. 캐시에 있으면 즉시, 없으면 Claude로 생성 후 캐시.
/// 크레딧/토큰 보호로 본문은 MAX_CHUNK_CHARS까지만 청킹한다(나머지는 클라이언트 규칙 기반 폴백).
async fn entry_chunks(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    if let Some(cached) = st
        .db
        .get_entry_chunks(id)
        .await
        .map_err(AppError::from)?
    {
        return Ok(json_response(cached));
    }
    let e = st
        .db
        .get_entry(id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError("기사를 찾을 수 없습니다".into()))?;
    let full = e.raw_text.trim();
    // 너무 긴 기사는 LLM 청킹을 건너뛰고 빈 응답 → 클라이언트가 전체를 규칙 기반으로 렌더.
    if full.chars().count() > MAX_CHUNK_CHARS {
        return Ok(json_response("{\"paras\":[]}".to_string()));
    }
    let chunks = st
        .extractor
        .chunk_article(full)
        .await
        .map_err(AppError::from)?;
    let body = serde_json::to_string(&chunks).map_err(|e| AppError(e.to_string()))?;
    st.db
        .save_entry_chunks(id, &body)
        .await
        .map_err(AppError::from)?;
    Ok(json_response(body))
}

/// 기사 구조 마인드맵 JSON을 반환한다. 캐시에 있으면 즉시, 없으면 Claude로 생성 후 캐시.
async fn entry_mindmap(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    if let Some(cached) = st.db.get_entry_mindmap(id).await.map_err(AppError::from)? {
        return Ok(json_response(cached));
    }
    let entry = st
        .db
        .get_entry(id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError("entry not found".into()))?;

    let mm = st
        .extractor
        .analyze_mindmap(&entry.raw_text)
        .await
        .map_err(AppError::from)?;
    let body = serde_json::to_string(&mm).map_err(|e| AppError(e.to_string()))?;
    st.db.save_entry_mindmap(id, &body).await.map_err(AppError::from)?;
    Ok(json_response(body))
}

/// 한글 요약 초안(블로그 + X 스레드) JSON. 캐시에 있으면 즉시, 없으면 Claude로 생성 후 캐시.
/// `?force=1`이면 캐시를 무시하고 새로 생성한다(다시 생성 버튼용).
async fn entry_summary(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, AppError> {
    let force = q.contains_key("force");

    // 캐시에 있으면 그대로, 없으면 Claude로 생성 후 캐시.
    let sum: models::Summary = if !force {
        match st.db.get_entry_summary(id).await.map_err(AppError::from)? {
            Some(cached) => serde_json::from_str(&cached).map_err(|e| AppError(e.to_string()))?,
            None => generate_summary(&st, id).await?,
        }
    } else {
        generate_summary(&st, id).await?
    };

    // 블로그용 마크다운은 서버에서 HTML로 렌더해 함께 내려준다(클라이언트 MD 뷰어용).
    let resp = serde_json::json!({
        "blog": sum.blog,
        "blog_html": md_to_html(&sum.blog),
        "thread": sum.thread,
    });
    Ok(json_response(resp.to_string()))
}

/// 기사 본문으로 한글 요약을 Claude에 요청하고 캐시에 저장한 뒤 반환한다.
async fn generate_summary(st: &AppState, id: Uuid) -> Result<models::Summary, AppError> {
    let entry = st
        .db
        .get_entry(id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError("entry not found".into()))?;

    let sum = st
        .extractor
        .summarize_korean(&entry.raw_text)
        .await
        .map_err(AppError::from)?;
    let body = serde_json::to_string(&sum).map_err(|e| AppError(e.to_string()))?;
    st.db.save_entry_summary(id, &body).await.map_err(AppError::from)?;
    Ok(sum)
}

/// application/json 본문 응답(문자열 그대로).
fn json_response(body: String) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
}

/// 이 기사의 단어들 → 하이라이트용 (소문자 변형 → 뜻) 맵. 단일 토큰 term만 등록.
fn build_vocab(words: &[(String, String, String)]) -> HashMap<String, String> {
    let mut vocab: HashMap<String, String> = HashMap::new();
    for (term, def, _ex) in words {
        if term.split_whitespace().count() != 1 {
            continue; // 다단어 term은 토큰 매칭에서 제외
        }
        for key in vocab_variants(term) {
            vocab.entry(key).or_insert_with(|| def.clone());
        }
    }
    vocab
}

/// term의 소문자 + 간단한 규칙 굴절형(매칭용). base형만 저장돼 있어도 일부
/// 굴절형(espouse↔espoused 등)을 함께 잡는다.
fn vocab_variants(term: &str) -> Vec<String> {
    let t = term.trim().to_lowercase();
    if t.is_empty() {
        return Vec::new();
    }
    let mut v = vec![t.clone()];
    let mut add = |s: String| {
        if !v.contains(&s) {
            v.push(s);
        }
    };
    add(format!("{t}s"));
    add(format!("{t}es"));
    add(format!("{t}ed"));
    add(format!("{t}ing"));
    add(format!("{t}d"));
    add(format!("{t}ly"));
    if let Some(stem) = t.strip_suffix('e') {
        add(format!("{stem}ing")); // espouse → espousing
    }
    if let Some(stem) = t.strip_suffix('y') {
        add(format!("{stem}ies")); // study → studies
        add(format!("{stem}ied")); // study → studied
    }
    v
}

/// 기사 원문을 단락으로 나눠 `<p class="para">`로 렌더하고, 각 토큰이 vocab 맵에
/// 있으면 `<mark class="vocab" data-def=...>`로 감싼다. 모든 텍스트는 esc()로 이스케이프.
fn render_article(raw: &str, vocab: &HashMap<String, String>) -> String {
    let mut out = String::new();
    for para in split_paragraphs(raw) {
        out.push_str("<p class=\"para\">");
        out.push_str(&highlight_paragraph(&para, vocab));
        out.push_str("</p>");
    }
    out
}

/// 빈 줄(하나 이상의 연속 개행) 기준으로 단락 분할. 단락 내 단일 개행은 공백으로 합침.
fn split_paragraphs(raw: &str) -> Vec<String> {
    raw.replace('\r', "")
        .split("\n\n")
        .map(|chunk| {
            chunk
                .split('\n')
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ")
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// 한 단락을 토큰 단위로 훑어 vocab에 있는 토큰만 <mark>로 감싼다.
fn highlight_paragraph(text: &str, vocab: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut token = String::new();
    for c in text.chars() {
        if is_token_char(c) {
            token.push(c);
        } else {
            flush_token(&mut out, &mut token, vocab);
            out.push_str(&esc(&c.to_string()));
        }
    }
    flush_token(&mut out, &mut token, vocab);
    out
}

/// 토큰을 vocab에서 찾아 있으면 <mark>, 없으면 그대로(이스케이프) 출력하고 비운다.
fn flush_token(out: &mut String, token: &mut String, vocab: &HashMap<String, String>) {
    if token.is_empty() {
        return;
    }
    match vocab.get(&token.to_lowercase()) {
        Some(def) => out.push_str(&format!(
            "<mark class=\"vocab\" data-def=\"{}\">{}</mark>",
            esc(def),
            esc(token)
        )),
        None => out.push_str(&esc(token)),
    }
    token.clear();
}

/// 단어 토큰 구성 문자: 알파벳 + 아포스트로피(’/').
fn is_token_char(c: char) -> bool {
    c.is_alphabetic() || c == '\u{2019}' || c == '\''
}

/// HTML 텍스트/속성 양쪽에 안전하도록 이스케이프(따옴표 포함).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// CommonMark 강조 규칙은 CJK와 문장부호가 맞닿을 때 흔한 한글 패턴을 놓친다.
/// 예: `**중첩(superposition)**과` — 닫는 `**` 앞이 문장부호 `)`이고 뒤가 한글이라
/// right-flanking 조건을 못 채워 강조가 풀리고 리터럴 `*`가 남는다.
/// 구분자(`*` 런) 바로 앞이 문장부호, 바로 뒤가 CJK 글자이면 그 사이에
/// 폭 없는 공백(U+200B)을 끼워 넣어 정상 파싱되게 한다(보이지 않고, 원문 md는 안 건드림).
fn cjk_emphasis_fix(md: &str) -> String {
    let chars: Vec<char> = md.chars().collect();
    let mut out = String::with_capacity(md.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '*' {
            let start = i;
            while i < chars.len() && chars[i] == '*' {
                i += 1;
            }
            let before = start.checked_sub(1).map(|j| chars[j]);
            let after = chars.get(i).copied();
            let punct_before = before.is_some_and(|c| !c.is_whitespace() && !c.is_alphanumeric());
            let cjk_after = after.is_some_and(|c| c.is_alphabetic() && !c.is_ascii());
            if punct_before && cjk_after {
                out.push('\u{200B}');
            }
            for _ in start..i {
                out.push('*');
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// 마크다운 → HTML(서버 사이드 렌더). 블로그용 요약을 MD 뷰어로 보여주기 위함.
/// 입력은 우리가 프롬프트한 Claude의 마크다운이라 원본 신뢰도가 높지만,
/// 안전을 위해 원시 HTML/HTML 블록은 통과시키지 않고 텍스트로 이스케이프한다.
fn md_to_html(md: &str) -> String {
    use pulldown_cmark::{html, Event, Options, Parser};
    let md = cjk_emphasis_fix(md);
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    // 원시 HTML 이벤트는 이스케이프된 텍스트로 바꿔 스크립트 주입을 막는다.
    let parser = Parser::new_ext(&md, opts).map(|ev| match ev {
        Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

/// 다운로드 응답(Content-Type + 첨부 파일명).
fn download(content_type: &'static str, filename: String, body: String) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        body,
    )
}

/// 카테고리 필터가 있으면 파일명에 접미사를 붙인다(words-nyt.csv 등).
fn fname(base: &str, cat: Option<Category>, ext: &str) -> String {
    match cat {
        Some(c) => format!("{base}-{}.{ext}", c.as_str()),
        None => format!("{base}.{ext}"),
    }
}

/// 내보내기 링크용 쿼리스트링(`?category=…` 또는 빈 문자열).
fn cat_query(cat: Option<Category>) -> String {
    cat.map_or_else(String::new, |c| format!("?category={}", c.as_str()))
}

/// RFC4180 CSV 필드(쉼표/따옴표/개행 포함 시 인용, 내부 따옴표는 중복).
fn csv_field(s: &str) -> String {
    if s.contains(['"', ',', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// TSV 필드: 탭/개행은 라인 구조를 깨므로 공백으로 치환.
fn tsv_field(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

/// 공통 HTML 셸(스타일 포함)로 본문을 감싼다.
fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!DOCTYPE html><html lang=\"ko\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title><style>{CSS}</style></head>\
         <body><div class=\"wrap\">{body}</div></body></html>",
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
        "<nav>{}{}{}{}{}<a class=\"right\" href=\"/auth/logout\">로그아웃</a></nav>",
        link("/", "붙여넣기", "home"),
        link("/entries", "내 기사", "entries"),
        link("/words", "단어장", "words"),
        link("/sentences", "베스트 문장", "sentences"),
        link("/review", "복습", "review"),
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

// Apple 스타일 글래스(글래스모피즘): 컬러 그라디언트 배경 위에 반투명 + backdrop-blur 패널.
const CSS: &str = "\
:root {
  --ink: #14142b; --muted: #5b5b78; --accent: #0a84ff; --accent2: #bf5af2;
  --glass: rgba(255,255,255,.55); --glass-2: rgba(255,255,255,.38);
  --brd: rgba(255,255,255,.7); --shadow: 0 10px 40px rgba(31,38,135,.18);
  --gtline: rgba(60,60,90,.32);
}
* { box-sizing: border-box; }
html { -webkit-text-size-adjust: 100%; }
body {
  font-family: -apple-system, BlinkMacSystemFont, 'SF Pro Text', system-ui, 'Apple SD Gothic Neo', sans-serif;
  color: var(--ink); margin: 0; min-height: 100vh; line-height: 1.55;
  background: linear-gradient(135deg, #a8c9ff 0%, #c9e0ff 22%, #e7d4ff 55%, #ffd9ec 78%, #cfe6ff 100%) fixed;
}
/* 배경에 은은한 컬러 블롭 */
body::before {
  content: ''; position: fixed; inset: -20vmax; z-index: -1; pointer-events: none;
  background:
    radial-gradient(38vmax 38vmax at 12% 8%, rgba(10,132,255,.35), transparent 60%),
    radial-gradient(34vmax 34vmax at 88% 22%, rgba(191,90,242,.30), transparent 60%),
    radial-gradient(40vmax 40vmax at 70% 96%, rgba(255,120,180,.28), transparent 60%);
  filter: blur(10px);
}
.wrap { max-width: 800px; margin: 0 auto; padding: 1.1rem 1rem 4rem; }

/* 유리 패널 공통 */
nav, .card, .filter, .paste, .article {
  background: var(--glass);
  -webkit-backdrop-filter: blur(22px) saturate(170%);
  backdrop-filter: blur(22px) saturate(170%);
  border: 1px solid var(--brd);
  box-shadow: var(--shadow);
}

nav {
  position: sticky; top: .6rem; z-index: 10;
  display: flex; gap: .35rem; align-items: center; flex-wrap: wrap;
  padding: .5rem .6rem; border-radius: 16px; margin-bottom: 1.1rem;
}
nav a { text-decoration: none; color: var(--muted); font-weight: 600; font-size: .92rem;
  padding: .32rem .7rem; border-radius: 10px; transition: background .15s, color .15s; }
nav a:hover { background: rgba(255,255,255,.5); color: var(--ink); }
nav a.active { color: #fff; background: linear-gradient(135deg, var(--accent), var(--accent2)); box-shadow: 0 4px 14px rgba(10,132,255,.35); }
nav a.right { margin-left: auto; }

h1 { font-size: 1.5rem; font-weight: 700; letter-spacing: -.02em; margin: .3rem 0 .9rem; }
h2 { font-size: 1.05rem; font-weight: 700; letter-spacing: -.01em; margin: 1.6rem 0 .6rem; }

.filter { display: flex; flex-wrap: wrap; gap: .4rem; padding: .5rem; border-radius: 14px; margin: .75rem 0 1rem; }
.chip { text-decoration: none; color: var(--muted); font-weight: 600; border-radius: 999px;
  padding: .25rem .85rem; font-size: .88rem; transition: background .15s, color .15s; }
.chip:hover { background: rgba(255,255,255,.5); color: var(--ink); }
.chip.active { background: linear-gradient(135deg, var(--accent), var(--accent2)); color: #fff; }

.count { color: var(--muted); font-size: .85rem; font-weight: 600; }
.export { margin: .25rem 0 .75rem; font-size: .88rem; color: var(--muted); }
.export a { color: var(--accent); text-decoration: none; font-weight: 600; }
.empty { color: var(--muted); padding: 1.5rem 0; }

.cards { list-style: none; padding: 0; margin: .5rem 0 0; display: flex; flex-direction: column; gap: .8rem; }
.card { border-radius: 18px; padding: 1rem 1.1rem; transition: transform .12s, box-shadow .12s; }
li.card:hover { transform: translateY(-2px); box-shadow: 0 16px 44px rgba(31,38,135,.22); }
.head { display: flex; align-items: center; gap: .55rem; flex-wrap: wrap; }
.badge { font-size: .68rem; font-weight: 800; letter-spacing: .02em; color: #fff;
  background: linear-gradient(135deg, var(--accent), var(--accent2));
  border-radius: 8px; padding: .16rem .5rem; }
.term { font-size: 1.12rem; font-weight: 700; letter-spacing: -.01em; }
.known { margin-left: auto; }
.known button, .paste button {
  cursor: pointer; font-weight: 600; color: var(--accent);
  background: rgba(255,255,255,.6); border: 1px solid var(--brd); border-radius: 10px;
  padding: .3rem .8rem; font-size: .85rem; transition: background .15s, transform .1s;
}
.known button:hover { background: #fff; }
.def { margin-top: .5rem; }
.ex { margin-top: .3rem; color: var(--muted); font-style: italic; }
.sentence { margin: 0; padding-left: .9rem; font-size: 1.02rem;
  border-left: 3px solid transparent;
  border-image: linear-gradient(var(--accent), var(--accent2)) 1; }
.reason { margin-top: .5rem; color: var(--muted); font-size: .9rem; }

/* 어근 분석 */
.roots-row { margin-top: .6rem; }
.roots-btn { cursor: pointer; font-weight: 600; font-size: .82rem; color: var(--accent);
  background: rgba(255,255,255,.6); border: 1px solid var(--brd); border-radius: 10px; padding: .3rem .75rem; transition: background .15s; }
.roots-btn:hover { background: #fff; }
.roots-btn:disabled { opacity: .6; cursor: default; }
.roots { margin-top: .6rem; padding: .75rem .85rem; border-radius: 14px;
  background: rgba(255,255,255,.4); border: 1px solid var(--brd); font-size: .9rem; }
.roots .parts { display: flex; flex-wrap: wrap; gap: .4rem; margin-bottom: .5rem; }
.roots .part { background: rgba(255,255,255,.7); border: 1px solid var(--brd); border-radius: 999px; padding: .2rem .7rem; }
.roots .part b { font-weight: 700; }
.roots .part i { color: var(--accent); font-style: normal; font-size: .74rem; text-transform: uppercase; letter-spacing: .03em; }
.roots .origin, .roots .related { color: var(--muted); margin-top: .3rem; }
.roots .mnemonic { margin-top: .45rem; }

/* 문장 문법 그래프(아크 다이어그램) */
.gram-actions { margin-top: .6rem; }
.gram-btn { cursor: pointer; font-weight: 600; font-size: .82rem; color: var(--accent);
  background: rgba(255,255,255,.6); border: 1px solid var(--brd); border-radius: 10px; padding: .3rem .75rem; transition: background .15s; }
.gram-btn:hover { background: #fff; }
.gram-btn:disabled { opacity: .6; cursor: default; }
.gram { margin-top: .6rem; padding: .75rem .85rem; border-radius: 14px;
  background: rgba(255,255,255,.4); border: 1px solid var(--brd); font-size: .9rem; }
.gram-summary { font-weight: 600; margin-bottom: .5rem; }
/* 문법 분석 팝업(모달) */
.gram-modal { position: fixed; inset: 0; z-index: 1000; background: rgba(0,0,0,.45);
  display: flex; align-items: flex-start; justify-content: center; padding: 4vh 3vw; overflow: auto; }
.gram-modal[hidden] { display: none !important; }
.gram-modal-box { width: min(920px, 96vw); max-height: 92vh; overflow: auto;
  background: rgba(252,252,255,.98); border: 1px solid var(--brd); border-radius: 18px;
  box-shadow: 0 20px 60px rgba(0,0,0,.35); padding: 1rem 1.1rem 1.2rem; }
.gram-modal-bar { display: flex; align-items: flex-start; gap: .8rem; margin-bottom: .7rem;
  position: sticky; top: -.2rem; background: inherit; padding: .2rem 0; }
.gram-modal-title { flex: 1; font-weight: 600; font-size: .96rem; line-height: 1.45; }
.gram-modal-close { flex: 0 0 auto; cursor: pointer; font-size: 1rem; font-weight: 700; color: var(--muted);
  background: rgba(255,255,255,.7); border: 1px solid var(--brd); border-radius: 10px; padding: .2rem .6rem; }
.gram-modal-close:hover { background: #fff; }
.gram-scroll { overflow-x: auto; padding-bottom: .2rem; }
.gram-wrap { position: relative; display: inline-block; min-width: 100%; }
.gram-svg { position: absolute; left: 0; top: 0; overflow: visible; pointer-events: none; }
.gram-row { display: flex; flex-wrap: nowrap; align-items: flex-end; gap: .5rem; padding-top: 92px; }
.gram-node { display: flex; flex-direction: column; align-items: center; gap: .15rem;
  background: rgba(255,255,255,.75); border: 1px solid var(--brd); border-radius: 12px;
  padding: .3rem .55rem; white-space: nowrap; }
.gram-text { font-weight: 600; }
.gram-role { font-size: .68rem; color: var(--accent); }
.gram-label { font-size: 10px; font-weight: 600; paint-order: stroke; stroke: #fff; stroke-width: 3px; stroke-linejoin: round; }
.gram-bar { display: flex; align-items: center; flex-wrap: wrap; gap: .5rem; margin-bottom: .5rem; }
.gram-toggle { display: inline-flex; gap: .15rem; padding: .12rem;
  background: rgba(255,255,255,.4); border: 1px solid var(--brd); border-radius: 10px; }
.gv-refresh { cursor: pointer; font-size: .72rem; font-weight: 600; color: var(--muted);
  background: rgba(255,255,255,.6); border: 1px solid var(--brd); border-radius: 8px; padding: .18rem .6rem; transition: background .15s; }
.gv-refresh:hover { background: #fff; }
.gv-refresh:disabled { opacity: .6; cursor: default; }
.gv-btn { cursor: pointer; font-size: .74rem; font-weight: 600; color: var(--muted);
  background: transparent; border: 0; border-radius: 8px; padding: .12rem .65rem; transition: background .15s; }
.gv-btn.on { background: linear-gradient(135deg, var(--accent), var(--accent2)); color: #fff; }
.gram-legend { display: flex; flex-wrap: wrap; gap: .25rem .7rem; margin-bottom: .55rem; font-size: .7rem; color: var(--muted); }
.gram-legend span { display: inline-flex; align-items: center; gap: .25rem; }
.gram-legend i { width: .7rem; height: .7rem; border-radius: 3px; display: inline-block; }
ul.gt-root, ul.gt-kids { list-style: none; margin: 0; padding: 0; }
ul.gt-kids { margin-left: .55rem; padding-left: 1rem; }
.gt-node { position: relative; margin: .2rem 0; }
/* 파일트리식 연결선: 자식마다 세로선(위→가지선) + 가로 가지선. 마지막 자식은 가지선까지만. */
.gt-kids > .gt-node::before { content: ''; position: absolute; left: -.5rem; top: 0; height: 100%;
  border-left: 1.5px solid var(--gtline); }
.gt-kids > .gt-node:last-child::before { height: .8rem; }
.gt-kids > .gt-node::after { content: ''; position: absolute; left: -.5rem; top: .8rem; width: .5rem;
  border-top: 1.5px solid var(--gtline); }
.gt-head { display: inline-flex; align-items: center; gap: .35rem;
  background: rgba(255,255,255,.72); border: 1px solid var(--brd); border-radius: 10px; padding: .18rem .5rem; }
.gt-toggle { cursor: pointer; flex: 0 0 auto; width: 1.05rem; height: 1.05rem; padding: 0; line-height: 1;
  font-size: .82rem; font-weight: 700; color: var(--accent); background: rgba(255,255,255,.6);
  border: 1px solid var(--brd); border-radius: 5px; }
.gt-toggle:hover { background: #fff; }
.gt-lead { display: inline-block; width: 1.05rem; flex: 0 0 auto; }
.gt-rel { font-size: .64rem; font-weight: 700; color: #fff; background: var(--accent); border-radius: 6px; padding: .05rem .4rem; }
.gt-text { font-weight: 600; }
.gt-ko { font-size: .72rem; color: var(--muted); }
.gt-role { font-size: .66rem; color: var(--muted); }
/* 쉬운 뷰(색칠 문장) */
.easy { display: flex; flex-direction: column; gap: .4rem; }
.easy-item { display: flex; flex-direction: column; gap: .35rem; }
.easy-part { display: flex; align-items: center; gap: .6rem; padding: .45rem .6rem;
  background: rgba(255,255,255,.55); border: 1px solid var(--brd); border-left: 5px solid var(--muted); border-radius: 12px; }
.easy-tag { flex: 0 0 auto; font-size: .74rem; font-weight: 700; color: #fff; border-radius: 8px; padding: .2rem .55rem; white-space: nowrap; }
.easy-body { flex: 1; }
.easy-en { font-weight: 600; }
.easy-ko { color: var(--muted); font-size: .92rem; margin-top: .1rem; }
.easy-more { cursor: pointer; flex: 0 0 auto; align-self: center; font-size: .7rem; font-weight: 600;
  color: var(--accent); background: rgba(255,255,255,.6); border: 1px solid var(--brd); border-radius: 8px; padding: .15rem .5rem; white-space: nowrap; }
.easy-more:hover { background: #fff; }
.easy-kids { display: flex; flex-direction: column; gap: .35rem; margin-left: 1rem; padding-left: .6rem; border-left: 2px dashed var(--gtline); }
.gram-plabel { font-weight: 700; margin: .7rem 0 .35rem; }
.gram-points { margin: 0; padding-left: 1.1rem; }
.gram-points li { margin: .35rem 0; line-height: 1.5; }
.pt-btn { cursor: pointer; font-size: .72rem; font-weight: 600; color: var(--accent);
  background: rgba(255,255,255,.6); border: 1px solid var(--brd); border-radius: 8px;
  padding: .05rem .5rem; margin-left: .4rem; vertical-align: middle; transition: background .15s; }
.pt-btn:hover { background: #fff; }
.pt-btn:disabled { opacity: .6; cursor: default; }
.pt-detail { margin: .4rem 0 .2rem; padding: .55rem .7rem; border-radius: 10px;
  background: rgba(255,255,255,.55); border: 1px solid var(--brd); }
.pt-expl { margin-bottom: .4rem; }
.pt-ex-item { margin: .3rem 0; }
.pt-en { font-weight: 600; }
.pt-ko { color: var(--muted); font-size: .85rem; }

.muted { color: var(--muted); }

/* 인쇄(PDF) 뷰: 2단 레이아웃 */
.toolbar { display: flex; align-items: center; gap: .8rem; flex-wrap: wrap; margin: .5rem 0 1rem; }
.toolbar #prog { color: var(--muted); font-size: .9rem; }
#printbtn { cursor: pointer; font-weight: 600; color: #fff; border: 0; border-radius: 12px;
  padding: .5rem 1.15rem; font-size: .95rem; box-shadow: 0 6px 18px rgba(10,132,255,.3);
  background: linear-gradient(135deg, var(--accent), var(--accent2)); }
#printbtn:disabled { opacity: .55; cursor: default; box-shadow: none; }
.print-words { list-style: none; padding: 0; margin: .5rem 0 0; columns: 2; column-gap: 1.3rem; }
.print-words .card { break-inside: avoid; -webkit-column-break-inside: avoid; margin: 0 0 .85rem; }
.print-words li.unsel { opacity: .4; }
.print-words .date-sep { column-span: all; -webkit-column-span: all; list-style: none;
  font-weight: 700; font-size: 1rem; margin: .7rem 0 .5rem; padding: .25rem .1rem;
  border-bottom: 2px solid var(--accent); break-after: avoid; break-inside: avoid; }
.wdate { color: var(--muted); font-size: .76rem; font-weight: 600; margin-left: .5rem; white-space: nowrap; }
.datebar { display: flex; flex-wrap: wrap; align-items: center; gap: .5rem; margin: 0 0 1rem; padding: .5rem .6rem;
  background: rgba(255,255,255,.4); border: 1px solid var(--brd); border-radius: 12px; }
.datechip { display: inline-flex; align-items: center; gap: .3rem; font-size: .84rem; font-weight: 600;
  background: rgba(255,255,255,.6); border: 1px solid var(--brd); border-radius: 999px; padding: .2rem .7rem; cursor: pointer; }
.print-sents { list-style: none; padding: 0; margin: .5rem 0 0; }
.print-sents .card { break-inside: avoid; margin: 0 0 .9rem; }
.print-sents li.unsel { opacity: .4; }
.print-sents .date-sep { list-style: none; font-weight: 700; font-size: 1rem; margin: .7rem 0 .5rem;
  padding: .25rem .1rem; border-bottom: 2px solid var(--accent); break-after: avoid; break-inside: avoid; }
.src { color: var(--muted); font-size: .82rem; font-weight: 600; margin: .1rem 0 .35rem; }
.print-sents .src-sep { list-style: none; font-weight: 700; font-size: .92rem; color: var(--accent);
  margin: .45rem 0 .3rem .2rem; padding: .1rem 0; break-after: avoid; break-inside: avoid; }
.pick { display: inline-flex; align-items: center; margin-right: .15rem; cursor: pointer; }
.pickbox { width: 1.05rem; height: 1.05rem; cursor: pointer; accent-color: var(--accent); }
.pick-hint { font-size: .84rem; margin: -.3rem 0 .7rem; }

@media print {
  nav, .toolbar, .noprint, .filter, .export { display: none !important; }
  .print-words li.unsel, .print-sents li.unsel { display: none !important; }
  html, body { background: #fff !important; color: #000 !important; }
  body::before { display: none !important; }
  .wrap { max-width: none; margin: 0; padding: 0; }
  h1 { font-size: 12pt; margin: 0 0 6pt; color: #000; }
  .print-words { columns: 2; column-gap: 8mm; font-size: 7.5pt; line-height: 1.32; }
  .print-words .card {
    break-inside: avoid; page-break-inside: avoid; margin: 0 0 5pt; padding: 5pt 7pt;
    background: #fff !important; border: 1px solid #ccc !important; border-radius: 5px;
    box-shadow: none !important; -webkit-backdrop-filter: none !important; backdrop-filter: none !important;
  }
  .head { gap: .35rem; }
  .term { color: #000; font-size: 9pt; }
  .badge { background: #eaeaea !important; color: #333 !important; font-size: 6pt; padding: .08rem .35rem; }
  .def { font-size: 7.5pt; margin-top: 2pt; }
  .ex { font-size: 7pt; margin-top: 1pt; color: #333 !important; }
  .reason, .origin, .related, .mnemonic { color: #000 !important; }
  .roots { background: #f5f5f7 !important; border: 1px solid #ddd !important;
    font-size: 7pt; padding: 4pt 6pt; margin-top: 4pt; }
  .roots .parts { gap: .25rem; margin-bottom: 3pt; }
  .roots .part { background: #fff !important; border: 1px solid #ddd !important; padding: .08rem .4rem; }
  .roots .part i { color: #555 !important; font-size: 5.5pt; }
  .roots .origin, .roots .related, .roots .mnemonic { margin-top: 2pt; }
  a { color: inherit; text-decoration: none; }
  .vocab { background: transparent !important; text-decoration-color: #999 !important; }
  .vocab:hover::after { display: none !important; }
  .mindmap-card, .mm-h2 { display: none !important; }
  /* 기사 인쇄: 제목 전체폭 + 본문 2단 흐름 */
  .print-title { font-size: 15pt; margin: 0 0 4pt; color: #000; }
  .print-meta { font-size: 8.5pt; margin: 0 0 8pt; color: #333 !important; }
  .reader.cols { columns: 2; column-gap: 8mm; max-width: none; margin: 0;
    font-size: 10pt; line-height: 1.55; font-family: Georgia, 'Times New Roman', serif; }
  .reader.cols .para { margin: 0 0 6pt; }
  @page { margin: 11mm; }
}

/* 기사 목록 카드 링크 + 삭제 아이콘 */
.card.entry { position: relative; }
.entry-link { text-decoration: none; color: inherit; display: block; padding-right: 2.6rem; }
.del { position: absolute; top: .8rem; right: .8rem; margin: 0; }
.del button { cursor: pointer; width: 2.1rem; height: 2.1rem; padding: 0; font-size: .95rem; line-height: 1;
  display: flex; align-items: center; justify-content: center; color: var(--muted);
  background: rgba(255,255,255,.55); border: 1px solid var(--brd); border-radius: 11px; transition: all .12s; }
.del button:hover { background: #fff; color: #e5484d; border-color: #e5484d; transform: translateY(-1px); }
.del.detail { position: static; display: inline-block; margin: .2rem 0 .4rem; }
.del.detail button { width: auto; height: auto; padding: .45rem 1rem; font-size: .9rem; font-weight: 600; border-radius: 12px; }

/* 기사 원문 — 읽기 리더 뷰 */
.article { border-radius: 18px; padding: 1.4rem 1.5rem; margin: .8rem 0; }
/* 리더 헤더(제목 + 편집 버튼) & 본문 편집 폼 */
.reader-head { display: flex; align-items: center; gap: .5rem; flex-wrap: wrap; }
.reader-head h2 { margin-right: auto; }
.tts-audio { width: 100%; margin: .35rem 0 .8rem; }
#ttsrate.edit-toggle { padding: .3rem .5rem; cursor: pointer; }
.tts-readalong { white-space: pre-wrap; }
/* 청크 리딩(VSTF 계단식) */
.chunk-para { margin: 0 0 1.1rem; }
.chunk-line { padding: .06rem 0; }
.chunk-sub { margin-left: 1.7rem; }
.fn-word { opacity: .42; }
/* 읽기 멈춤/이어 읽기 플로팅 버튼(스크롤해도 우하단 고정) */
.reader-fab { position: fixed; right: 1.2rem; bottom: 1.2rem; z-index: 900;
  width: 3.4rem; height: 3.4rem; border-radius: 50%; border: none; cursor: pointer;
  font-size: 1.35rem; line-height: 1; color: #fff; background: linear-gradient(135deg, var(--accent), var(--accent2));
  box-shadow: 0 6px 22px rgba(10,132,255,.45); transition: transform .12s; }
.reader-fab[hidden] { display: none; }
.reader-fab:hover { transform: scale(1.06); }
@media print { .reader-fab { display: none !important; } }
.tts-sent { border-radius: 5px; transition: background .12s; }
.tts-on, .chunk-line.chunk-on { background: rgba(10,132,255,.16); box-shadow: 0 0 0 3px rgba(10,132,255,.16); border-radius: 5px; }
@media (prefers-color-scheme: dark) { .tts-on, .chunk-line.chunk-on { background: rgba(100,180,255,.22); box-shadow: 0 0 0 3px rgba(100,180,255,.22); } }
.edit-toggle, .ghost { cursor: pointer; font-weight: 600; font-size: .85rem; color: var(--accent);
  background: rgba(255,255,255,.6); border: 1px solid var(--brd); border-radius: 10px; padding: .35rem .8rem; }
.edit-toggle:hover, .ghost:hover { background: #fff; }
.reader-edit { display: none; }
.reader-edit textarea { width: 100%; box-sizing: border-box; min-height: 22rem; font: inherit;
  line-height: 1.7; color: var(--ink); background: rgba(255,255,255,.65); border: 1px solid var(--brd);
  border-radius: 12px; padding: .8rem .9rem; resize: vertical; }
.reader-edit textarea:focus { outline: none; border-color: var(--accent); box-shadow: 0 0 0 3px rgba(10,132,255,.18); }
.edit-hint { color: var(--muted); font-size: .85rem; margin-bottom: .6rem; }
.edit-actions { display: flex; gap: .6rem; margin-top: .7rem; }
@media (prefers-color-scheme: dark) {
  .edit-toggle, .ghost { background: rgba(255,255,255,.08); }
  .edit-toggle:hover, .ghost:hover { background: rgba(255,255,255,.16); }
  .reader-edit textarea { background: rgba(255,255,255,.08); }
  .gram-modal-box { background: rgba(24,24,38,.98); }
  .gram-modal-close { background: rgba(255,255,255,.1); }
  .gram-modal-close:hover { background: rgba(255,255,255,.2); }
}
.reader-hint { color: var(--muted); font-size: .84rem; margin: .2rem 0 .5rem; }
.reader { max-width: 38rem; margin: 0 auto;
  font-family: Georgia, 'Times New Roman', 'Noto Serif KR', 'Apple SD Gothic Neo', serif;
  font-size: 1.14rem; line-height: 1.8; word-break: break-word; }
.reader .para { margin: 0 0 1rem; }
.reader .para:last-child { margin-bottom: 0; }
/* 인쇄용 2단 리더(신문식으로 본문이 두 열로 흐름) */
.reader.cols { max-width: none; margin: 0; columns: 2; column-gap: 1.6rem; }
.print-title { margin: .3rem 0 .2rem; }
.print-meta { margin: 0 0 1rem; }

/* 구조 마인드맵 */
.mindmap-card { padding: 1.2rem 1rem; overflow-x: auto; }
.mm-wrap { position: relative; }
.mm-svg { position: absolute; inset: 0; z-index: 0; pointer-events: none; overflow: visible; }
.mm-grid { position: relative; z-index: 1; display: grid; grid-template-columns: 1fr auto 1fr;
  align-items: center; gap: .9rem 2.6rem; }
.mm-col { display: flex; flex-direction: column; gap: .9rem; justify-content: center; }
.mm-col.left { align-items: flex-end; }
.mm-col.right { align-items: flex-start; }
.mm-col.center { align-items: center; }
.mm-center { background: linear-gradient(135deg, var(--accent), var(--accent2)); color: #fff;
  font-weight: 700; padding: .6rem 1rem; border-radius: 14px; text-align: center; max-width: 12rem;
  box-shadow: 0 6px 18px rgba(10,132,255,.35); }
.mm-branch { background: rgba(255,255,255,.55); border: 1px solid var(--brd); border-left: 3px solid var(--c);
  border-radius: 12px; padding: .5rem .7rem; max-width: 15rem; }
.mm-head { font-weight: 700; font-size: .95rem; }
.mm-kws { display: flex; flex-wrap: wrap; gap: .3rem; margin-top: .4rem; }
.mm-kw { font-size: .76rem; color: var(--muted); background: rgba(120,120,140,.1);
  border: 1px solid var(--brd); border-radius: 999px; padding: .1rem .5rem; }
@media (max-width: 640px) {
  .mm-grid { grid-template-columns: 1fr; gap: .7rem; }
  .mm-col.left, .mm-col.right, .mm-col.center { align-items: stretch; }
  .mm-svg { display: none; }
  .mm-branch, .mm-center { max-width: none; }
}

/* 한글 요약 초안(블로그·X) */
.gen-btn { cursor: pointer; font-weight: 600; color: #fff; border: 0; border-radius: 12px;
  padding: .55rem 1.2rem; font-size: .95rem; box-shadow: 0 6px 18px rgba(10,132,255,.3);
  background: linear-gradient(135deg, var(--accent), var(--accent2)); }
.gen-btn:hover { transform: translateY(-1px); }
.gen-btn:disabled { opacity: .6; cursor: default; box-shadow: none; }
.sum-h { font-weight: 700; margin: 1rem 0 .4rem; }
/* 블로그용 마크다운 렌더 뷰 */
.md-view { background: rgba(255,255,255,.65); border: 1px solid var(--brd); border-radius: 12px;
  padding: .7rem 1rem; line-height: 1.7; overflow-wrap: break-word; }
.md-view > :first-child { margin-top: 0; }
.md-view > :last-child { margin-bottom: 0; }
.md-view h1 { font-size: 1.4rem; margin: 1rem 0 .5rem; }
.md-view h2 { font-size: 1.2rem; margin: 1rem 0 .5rem; }
.md-view h3 { font-size: 1.05rem; margin: .9rem 0 .4rem; }
.md-view p { margin: .5rem 0; }
.md-view ul, .md-view ol { margin: .5rem 0; padding-left: 1.4rem; }
.md-view li { margin: .2rem 0; }
.md-view blockquote { margin: .6rem 0; padding: .2rem .9rem; border-left: 3px solid var(--accent);
  color: var(--muted); }
.md-view code { background: rgba(120,120,140,.14); border-radius: 5px; padding: .1rem .35rem;
  font-size: .9em; }
.md-view pre { background: rgba(120,120,140,.12); border-radius: 10px; padding: .7rem .9rem;
  overflow-x: auto; }
.md-view pre code { background: none; padding: 0; }
.md-view a { color: var(--accent); }
.md-view table { border-collapse: collapse; margin: .6rem 0; }
.md-view th, .md-view td { border: 1px solid var(--brd); padding: .3rem .6rem; }
.sum-ta, .tw-ta { width: 100%; box-sizing: border-box; font: inherit; color: var(--ink);
  background: rgba(255,255,255,.65); border: 1px solid var(--brd); border-radius: 12px;
  padding: .6rem .7rem; resize: vertical; overflow: hidden; line-height: 1.55; }
.sum-ta { min-height: 8rem; }
.sum-ta:focus, .tw-ta:focus { outline: none; border-color: var(--accent); box-shadow: 0 0 0 3px rgba(10,132,255,.18); }
.sum-row { display: flex; justify-content: flex-end; margin: .4rem 0 .2rem; }
.copy-btn { cursor: pointer; font-weight: 600; font-size: .82rem; color: var(--accent);
  background: rgba(255,255,255,.6); border: 1px solid var(--brd); border-radius: 10px; padding: .3rem .75rem; }
.copy-btn:hover { background: #fff; }
.tweet { margin: .55rem 0; }
.tw-ta { min-height: 3.4rem; }
.tw-meta { display: flex; align-items: center; gap: .6rem; margin-top: .25rem; }
.tw-n { color: var(--muted); font-size: .8rem; font-weight: 600; }
.cc { color: var(--muted); font-size: .8rem; margin-left: auto; }
.cc.over { color: #e5484d; font-weight: 700; }
@media (prefers-color-scheme: dark) {
  .sum-ta, .tw-ta, .md-view { background: rgba(255,255,255,.08); }
  .copy-btn { background: rgba(255,255,255,.08); }
  .copy-btn:hover { background: rgba(255,255,255,.16); }
}
/* 본문 속 어휘 하이라이트 + hover 뜻 말풍선 */
.vocab { cursor: help; position: relative; border-radius: 4px; padding: 0 1px;
  background: rgba(10,132,255,.09);
  text-decoration: underline dotted; text-decoration-color: var(--accent); text-underline-offset: 3px; }
.vocab:hover { background: rgba(10,132,255,.2); }
.vocab:hover::after {
  content: attr(data-def); position: absolute; left: 0; top: 100%; z-index: 30; margin-top: 6px;
  width: max-content; max-width: 260px; white-space: normal; pointer-events: none;
  font-family: -apple-system, BlinkMacSystemFont, system-ui, sans-serif; font-size: .82rem; font-style: normal;
  line-height: 1.45; color: var(--ink); background: var(--glass);
  -webkit-backdrop-filter: blur(18px) saturate(160%); backdrop-filter: blur(18px) saturate(160%);
  border: 1px solid var(--brd); border-radius: 10px; box-shadow: var(--shadow); padding: .45rem .6rem; }

/* 붙여넣기/이어쓰기 폼 */
.paste { border-radius: 18px; padding: 1.1rem 1.2rem; margin: .6rem 0 1rem; }
.paste label { display: block; margin: .7rem 0 .3rem; font-weight: 700; font-size: .92rem; }
.paste .row { display: flex; gap: 1rem; }
.paste .row > div { flex: 1; }
.paste textarea, .paste input, .paste select {
  width: 100%; padding: .6rem .7rem; font: inherit; color: var(--ink);
  background: rgba(255,255,255,.65); border: 1px solid var(--brd); border-radius: 12px;
  outline: none; transition: border-color .15s, box-shadow .15s;
}
.paste textarea { min-height: 220px; resize: vertical; }
.paste textarea:focus, .paste input:focus, .paste select:focus {
  border-color: var(--accent); box-shadow: 0 0 0 3px rgba(10,132,255,.18);
}
.paste .hint { color: var(--muted); font-size: .85rem; margin: .4rem 0 0; }
.paste button {
  margin-top: 1rem; padding: .6rem 1.4rem; font-size: 1rem; color: #fff;
  background: linear-gradient(135deg, var(--accent), var(--accent2)); border: 0;
  box-shadow: 0 8px 22px rgba(10,132,255,.35);
}
.paste button:hover { transform: translateY(-1px); }

/* 복습 카드 */
.card.review { text-align: center; padding: 2.2rem 1rem; }
.card.review .head { justify-content: center; }
.card.review .term { font-size: 1.7rem; }
.actions { margin-top: 1.2rem; display: flex; gap: .5rem; justify-content: center; align-items: center; }
.actions button { cursor: pointer; font-size: .95rem; font-weight: 600; padding: .45rem 1.1rem;
  border: 1px solid var(--brd); border-radius: 12px; background: rgba(255,255,255,.6); color: var(--ink); }
.actions button:hover { background: #fff; }
.actions #know { color: var(--accent); border-color: var(--accent); }
.kbd { color: var(--muted); font-size: .8rem; margin-top: .8rem; }

@media (prefers-color-scheme: dark) {
  :root {
    --ink: #f2f2fa; --muted: #b9b9d0;
    --glass: rgba(30,30,50,.5); --glass-2: rgba(30,30,50,.4);
    --brd: rgba(255,255,255,.14); --shadow: 0 10px 40px rgba(0,0,0,.45);
    --gtline: rgba(255,255,255,.28);
  }
  body { background: linear-gradient(135deg, #10131f 0%, #1a1030 45%, #2a1030 78%, #101828 100%) fixed; }
  nav a:hover, .chip:hover { background: rgba(255,255,255,.12); }
  .known button, .paste textarea, .paste input, .paste select, .actions button,
  .roots-btn, .del button { background: rgba(255,255,255,.08); }
  .known button:hover, .actions button:hover, .roots-btn:hover { background: rgba(255,255,255,.16); }
  .roots { background: rgba(255,255,255,.06); }
  .roots .part { background: rgba(255,255,255,.12); }
  .mm-branch { background: rgba(255,255,255,.08); }
  .mm-kw { background: rgba(255,255,255,.1); }
}
";

/// 복습 플래시카드 위젯. 덱은 #deck(JSON)에서 읽고, 셔플 후 한 장씩 진행한다.
/// '알아요'는 /words/known 로 fetch POST(다음 추출/복습에서 제외).
const REVIEW_JS: &str = r#"
(function(){
  var deck = JSON.parse(document.getElementById('deck').textContent);
  for (var i=deck.length-1;i>0;i--){var j=Math.floor(Math.random()*(i+1));var t=deck[i];deck[i]=deck[j];deck[j]=t;}
  var root = document.getElementById('rv'), idx = 0, known = 0;
  function esc(s){var d=document.createElement('div');d.textContent=(s==null?'':s);return d.innerHTML;}
  function done(){
    root.innerHTML = '<div class="card"><p>복습 완료! 🎉 '+deck.length+'개 중 '+known+'개를 ‘안다’로 표시했어요.</p>'+
      '<p class="actions"><a class="chip" href="/review">다시 복습</a><a class="chip" href="/words">단어장</a></p></div>';
  }
  function render(){
    if (idx >= deck.length){ done(); return; }
    var c = deck[idx];
    root.innerHTML =
      '<p class="count">'+(idx+1)+' / '+deck.length+'</p>'+
      '<div class="card review">'+
        '<div class="head"><span class="badge">'+esc(c.category)+'</span><b class="term">'+esc(c.term)+'</b></div>'+
        '<div id="ans" hidden><div class="def">'+esc(c.definition)+'</div><div class="ex">'+esc(c.example)+'</div></div>'+
        '<div class="actions">'+
          '<button id="reveal">뜻 보기</button>'+
          '<span id="rate" hidden><button id="know">알아요</button> <button id="again">또 볼래요</button></span>'+
        '</div>'+
        '<div class="kbd">Space 뜻 보기 · y 알아요 · n 또 볼래요</div>'+
      '</div>';
    document.getElementById('reveal').onclick = reveal;
    document.getElementById('again').onclick = next;
    document.getElementById('know').onclick = function(){
      try { fetch('/words/known', {method:'POST', headers:{'Content-Type':'application/x-www-form-urlencoded'},
        body: new URLSearchParams({term: c.term})}); } catch(e){}
      known++; next();
    };
  }
  function reveal(){
    var a = document.getElementById('ans'); if(!a || !a.hidden) return;
    a.hidden = false;
    document.getElementById('reveal').hidden = true;
    document.getElementById('rate').hidden = false;
  }
  function next(){ idx++; render(); }
  document.addEventListener('keydown', function(e){
    if (idx >= deck.length) return;
    var a = document.getElementById('ans'); var revealed = a && !a.hidden;
    if (!revealed && (e.key===' '||e.key==='Enter')){ e.preventDefault(); reveal(); }
    else if (revealed && (e.key==='y'||e.key==='1')){ var k=document.getElementById('know'); if(k) k.click(); }
    else if (revealed && (e.key==='n'||e.key==='2')){ next(); }
  });
  if (!deck.length){ root.innerHTML = '<p class="empty">복습할 단어가 없습니다. <a href="/">본문을 붙여넣어</a> 추출해 보세요.</p>'; }
  else render();
})();
"#;

/// 단어 카드의 '어근 분석' 버튼: 클릭 시 /words/roots 를 fetch해 카드 안에 렌더/토글.
/// 응답은 신뢰 경계 밖(모델 생성)이라 textContent로만 DOM을 구성한다.
const ROOTS_JS: &str = r#"
(function(){
  function el(tag, cls, txt){ var e=document.createElement(tag); if(cls)e.className=cls; if(txt!=null)e.textContent=txt; return e; }
  function render(box, d){
    box.textContent='';
    if(d.parts && d.parts.length){
      var pr=el('div','parts');
      d.parts.forEach(function(p){
        var chip=el('span','part');
        chip.appendChild(el('b',null,p.piece||''));
        if(p.kind) chip.appendChild(el('i',null,' '+p.kind));
        chip.appendChild(el('span',null,' — '+(p.meaning||'')));
        pr.appendChild(chip);
      });
      box.appendChild(pr);
    }
    if(d.origin) box.appendChild(el('div','origin','어원: '+d.origin));
    if(d.related && d.related.length) box.appendChild(el('div','related','관련어: '+d.related.join(', ')));
    if(d.mnemonic) box.appendChild(el('div','mnemonic','💡 '+d.mnemonic));
    if(!box.childNodes.length) box.appendChild(el('div',null,'분석 결과가 비어 있어요.'));
  }
  document.querySelectorAll('.roots-btn').forEach(function(btn){
    btn.addEventListener('click', function(){
      var card=btn.closest('.card'); var box=card.querySelector('.roots');
      if(box.dataset.loaded){ box.hidden=!box.hidden; return; }
      box.hidden=false; box.textContent='분석 중…'; btn.disabled=true;
      fetch('/words/roots?term='+encodeURIComponent(btn.dataset.term))
        .then(function(r){ if(!r.ok) throw 0; return r.json(); })
        .then(function(d){ render(box,d); box.dataset.loaded='1'; })
        .catch(function(){ box.textContent='분석을 불러오지 못했어요. 잠시 후 다시 시도해 주세요.'; })
        .then(function(){ btn.disabled=false; });
    });
  });
})();
"#;

/// 인쇄 뷰: 모든 단어의 어근 분석을 동시성 제한(3)으로 자동 로드하고,
/// 완료되면 인쇄 버튼을 활성화한다. render는 ROOTS_JS와 동일 구조.
const PRINT_JS: &str = r#"
(function(){
  function el(t,c,x){ var e=document.createElement(t); if(c)e.className=c; if(x!=null)e.textContent=x; return e; }
  function render(box,d){
    box.textContent='';
    if(d.parts && d.parts.length){
      var pr=el('div','parts');
      d.parts.forEach(function(p){
        var chip=el('span','part');
        chip.appendChild(el('b',null,p.piece||''));
        if(p.kind) chip.appendChild(el('i',null,' '+p.kind));
        chip.appendChild(el('span',null,' — '+(p.meaning||'')));
        pr.appendChild(chip);
      });
      box.appendChild(pr);
    }
    if(d.origin) box.appendChild(el('div','origin','어원: '+d.origin));
    if(d.related && d.related.length) box.appendChild(el('div','related','관련어: '+d.related.join(', ')));
    if(d.mnemonic) box.appendChild(el('div','mnemonic','💡 '+d.mnemonic));
    if(!box.childNodes.length) box.appendChild(el('div','muted','(분석 없음)'));
  }
  var lis=Array.prototype.slice.call(document.querySelectorAll('.print-words li.card'));
  var prog=document.getElementById('prog'), btn=document.getElementById('printbtn');
  var selall=document.getElementById('selall'), selnone=document.getElementById('selnone');

  var dateboxes=Array.prototype.slice.call(document.querySelectorAll('.datebox'));
  var seps=Array.prototype.slice.call(document.querySelectorAll('.print-words .date-sep'));
  function cardsOfDate(d){ return lis.filter(function(li){ return li.dataset.date===d; }); }

  function selectedLis(){ return lis.filter(function(li){ return !li.classList.contains('unsel'); }); }
  function updateCount(){ if(prog) prog.textContent = selectedLis().length+' / '+lis.length+' 선택'; }

  // 날짜 헤더/체크박스 상태를 단어 선택 상태에 맞춰 갱신. 그 날 선택 단어가 0이면
  // 헤더도 숨겨(unsel) 빈 날짜가 인쇄되지 않게 한다.
  function syncDates(){
    seps.forEach(function(h){
      var any=cardsOfDate(h.dataset.date).some(function(li){ return !li.classList.contains('unsel'); });
      h.classList.toggle('unsel', !any);
    });
    dateboxes.forEach(function(cb){
      var any=cardsOfDate(cb.dataset.date).some(function(li){ return !li.classList.contains('unsel'); });
      cb.checked=any;
    });
  }

  // 체크박스 ↔ li.unsel 동기화
  lis.forEach(function(li){
    var cb=li.querySelector('.pickbox'); if(!cb) return;
    cb.addEventListener('change', function(){ li.classList.toggle('unsel', !cb.checked); updateCount(); syncDates(); });
  });
  function setAll(on){
    lis.forEach(function(li){ var cb=li.querySelector('.pickbox'); if(cb) cb.checked=on; li.classList.toggle('unsel', !on); });
    updateCount(); syncDates();
  }
  if(selall) selall.addEventListener('click', function(){ setAll(true); });
  if(selnone) selnone.addEventListener('click', function(){ setAll(false); });

  // 날짜 체크박스: 그 날짜의 단어 전체를 켜고/끄고, 헤더 표시도 갱신.
  dateboxes.forEach(function(cb){
    cb.addEventListener('change', function(){
      cardsOfDate(cb.dataset.date).forEach(function(li){
        var pb=li.querySelector('.pickbox'); if(pb) pb.checked=cb.checked;
        li.classList.toggle('unsel', !cb.checked);
      });
      updateCount(); syncDates();
    });
  });

  // 선택된 단어 중 아직 어근을 안 불러온 것만 로드(동시성 제한) 후 콜백.
  function loadRoots(boxes, cb){
    var total=boxes.length, done=0, i=0, active=0, CONC=3;
    if(total===0){ cb(); return; }
    function pump(){
      while(active<CONC && i<total){
        (function(box){
          active++;
          fetch('/words/roots?term='+encodeURIComponent(box.dataset.term))
            .then(function(r){ if(!r.ok) throw 0; return r.json(); })
            .then(function(d){ render(box,d); box.dataset.loaded='1'; })
            .catch(function(){ box.textContent=''; box.appendChild(el('div','muted','(분석 실패)')); })
            .then(function(){ done++; active--; if(prog) prog.textContent='어근 분석 불러오는 중… '+done+' / '+total; if(done>=total) cb(); else pump(); });
        })(boxes[i++]);
      }
    }
    pump();
  }

  if(btn) btn.addEventListener('click', function(){
    var sel=selectedLis();
    if(sel.length===0){ alert('선택된 단어가 없습니다.'); return; }
    var pending=sel.map(function(li){ return li.querySelector('.roots[data-term]'); })
                   .filter(function(b){ return b && !b.dataset.loaded; });
    btn.disabled=true;
    loadRoots(pending, function(){
      btn.disabled=false; updateCount();
      setTimeout(function(){ window.print(); }, 60);
    });
  });

  // 선택한 단어만 CSV/Anki로 내보내기: 선택 term을 폼에 담아 export 엔드포인트로 POST.
  function selectedTerms(){
    return selectedLis()
      .map(function(li){ var b=li.querySelector('.roots[data-term]'); return b ? b.dataset.term : null; })
      .filter(Boolean);
  }
  var expform=document.getElementById('expform'), expterms=document.getElementById('expterms');
  function exportTo(action){
    var terms=selectedTerms();
    if(terms.length===0){ alert('선택된 단어가 없습니다.'); return; }
    if(!expform||!expterms) return;
    expterms.value=terms.join('\n');
    expform.action=action;
    expform.submit();
  }
  var ec=document.getElementById('expcsv'), ea=document.getElementById('expanki');
  if(ec) ec.addEventListener('click', function(){ exportTo('/export/words.csv'); });
  if(ea) ea.addEventListener('click', function(){ exportTo('/export/words.tsv'); });

  updateCount(); syncDates();
})();
"#;

/// 베스트 문장 인쇄: 문장 개별 선택 + 날짜 체크박스(그 날짜 문장 일괄 토글, 빈 날짜 헤더
/// 자동 숨김) + 인쇄. 어근/CSV 없이 PRINT_JS의 선택·날짜 로직만 담은 경량 버전.
const SENT_PRINT_JS: &str = r#"
(function(){
  var lis=Array.prototype.slice.call(document.querySelectorAll('.print-sents li.card'));
  var prog=document.getElementById('prog'), btn=document.getElementById('printbtn');
  var selall=document.getElementById('selall'), selnone=document.getElementById('selnone');
  var dateboxes=Array.prototype.slice.call(document.querySelectorAll('.datebox'));
  var seps=Array.prototype.slice.call(document.querySelectorAll('.print-sents .date-sep'));
  var srcSeps=Array.prototype.slice.call(document.querySelectorAll('.print-sents .src-sep'));
  function sel(li){ return !li.classList.contains('unsel'); }
  function cardsOfDate(d){ return lis.filter(function(li){ return li.dataset.date===d; }); }
  function selectedLis(){ return lis.filter(sel); }
  function updateCount(){ if(prog) prog.textContent = selectedLis().length+' / '+lis.length+' 선택'; }
  function syncDates(){
    seps.forEach(function(h){ h.classList.toggle('unsel', !cardsOfDate(h.dataset.date).some(sel)); });
    // 제목 소제목: 같은 날짜+제목 그룹에 선택된 문장이 없으면 숨김.
    srcSeps.forEach(function(h){
      var any=lis.some(function(li){ return li.dataset.date===h.dataset.date && li.dataset.src===h.dataset.src && sel(li); });
      h.classList.toggle('unsel', !any);
    });
    dateboxes.forEach(function(cb){ cb.checked=cardsOfDate(cb.dataset.date).some(sel); });
  }
  lis.forEach(function(li){ var cb=li.querySelector('.pickbox'); if(!cb) return; cb.addEventListener('change', function(){ li.classList.toggle('unsel', !cb.checked); updateCount(); syncDates(); }); });
  function setAll(on){ lis.forEach(function(li){ var cb=li.querySelector('.pickbox'); if(cb) cb.checked=on; li.classList.toggle('unsel', !on); }); updateCount(); syncDates(); }
  if(selall) selall.addEventListener('click', function(){ setAll(true); });
  if(selnone) selnone.addEventListener('click', function(){ setAll(false); });
  dateboxes.forEach(function(cb){ cb.addEventListener('change', function(){ cardsOfDate(cb.dataset.date).forEach(function(li){ var pb=li.querySelector('.pickbox'); if(pb) pb.checked=cb.checked; li.classList.toggle('unsel', !cb.checked); }); updateCount(); syncDates(); }); });
  if(btn) btn.addEventListener('click', function(){ if(selectedLis().length===0){ alert('선택된 문장이 없습니다.'); return; } setTimeout(function(){ window.print(); }, 30); });
  updateCount(); syncDates();
})();
"#;

/// 기사 상세의 구조 마인드맵: /entries/:id/mindmap 을 fetch해 중앙 제목 + 좌우 가지
/// 카드로 그리고, 카드 위치를 측정해 SVG 곡선 커넥터를 얹는다. 응답은 모델 생성이라
/// 텍스트는 textContent로만 넣는다.
const MINDMAP_JS: &str = r#"
(function(){
  var NS='http://www.w3.org/2000/svg';
  var root=document.getElementById('mindmap');
  if(!root) return;
  function div(c,x){ var e=document.createElement('div'); if(c)e.className=c; if(x!=null)e.textContent=x; return e; }
  var PAL=['#0a84ff','#bf5af2','#ff375f','#30d158','#ff9f0a','#5e5ce6','#64d2ff','#ff6482'];

  fetch('/entries/'+root.dataset.entry+'/mindmap')
    .then(function(r){ if(!r.ok) throw 0; return r.json(); })
    .then(function(d){ render(d); })
    .catch(function(){ root.textContent=''; root.appendChild(div('muted','구조를 분석하지 못했어요.')); });

  function render(d){
    root.textContent='';
    var branches=(d.branches||[]);
    var wrap=div('mm-wrap');
    var svg=document.createElementNS(NS,'svg'); svg.setAttribute('class','mm-svg'); wrap.appendChild(svg);
    var grid=div('mm-grid');
    var left=div('mm-col left'), center=div('mm-col center'), right=div('mm-col right');
    grid.appendChild(left); grid.appendChild(center); grid.appendChild(right);
    var title=div('mm-center', d.title||'기사'); center.appendChild(title);
    var cards=[];
    branches.forEach(function(b,i){
      var side=(i%2===0)?'left':'right';
      var card=div('mm-branch'); var color=PAL[i%PAL.length]; card.style.setProperty('--c',color);
      card.appendChild(div('mm-head', b.heading||''));
      if(b.keywords && b.keywords.length){
        var kws=div('mm-kws');
        b.keywords.forEach(function(k){ kws.appendChild(div('mm-kw', k)); });
        card.appendChild(kws);
      }
      (side==='left'?left:right).appendChild(card);
      cards.push({el:card, side:side, color:color});
    });
    wrap.appendChild(grid); root.appendChild(wrap);
    if(!branches.length){ root.appendChild(div('muted','구조 정보가 없습니다.')); return; }

    function draw(){
      var wr=wrap.getBoundingClientRect();
      svg.setAttribute('viewBox','0 0 '+wr.width+' '+wr.height);
      svg.setAttribute('width',wr.width); svg.setAttribute('height',wr.height);
      while(svg.firstChild) svg.removeChild(svg.firstChild);
      var t=title.getBoundingClientRect();
      var cy=(t.top+t.bottom)/2-wr.top;
      cards.forEach(function(c){
        var r=c.el.getBoundingClientRect();
        var sx=(c.side==='left'?t.left:t.right)-wr.left;
        var ex=(c.side==='left'?r.right:r.left)-wr.left;
        var ey=(r.top+r.bottom)/2-wr.top;
        var mx=(sx+ex)/2;
        var p=document.createElementNS(NS,'path');
        p.setAttribute('d','M '+sx+' '+cy+' C '+mx+' '+cy+', '+mx+' '+ey+', '+ex+' '+ey);
        p.setAttribute('fill','none'); p.setAttribute('stroke',c.color);
        p.setAttribute('stroke-width','2'); p.setAttribute('opacity','.65');
        svg.appendChild(p);
      });
    }
    draw();
    var tmr; window.addEventListener('resize', function(){ clearTimeout(tmr); tmr=setTimeout(draw,150); });
  }
})();
"#;

/// 문장 문법 그래프 공유 렌더러: 토큰 칩을 가로로 놓고 그 위에 head→dependent 관계를
/// SVG 아크(라벨·화살표)로 그리고, 아래에 구조 요약 + 문법 포인트(각 포인트 '자세히'로
/// 상세 로드)를 붙인다. `window.gramRender(box, data)`로 노출해 /sentences와 복습 페이지가
/// 공유한다. MINDMAP_JS처럼 칩 위치를 측정해 곡선을 얹고 리사이즈 시 다시 그린다.
/// 응답은 모델 생성이라 텍스트는 textContent로만 넣는다.
const GRAPH_RENDER_JS: &str = r#"
(function(){
  var NS='http://www.w3.org/2000/svg';
  var PAL=['#0a84ff','#bf5af2','#ff375f','#30d158','#ff9f0a','#5e5ce6','#64d2ff','#ff6482'];
  var drawers=[];
  function el(t,c,x){ var e=document.createElement(t); if(c)e.className=c; if(x!=null)e.textContent=x; return e; }
  function sv(n){ return document.createElementNS(NS,n); }

  // 문법 역할 → 색. 라벨/역할 문자열에 키워드가 있으면 그 색을 준다(순서=우선순위).
  // 아크(선·라벨)와 트리(역할 배지)가 같은 색 언어를 공유해 색만 봐도 역할을 알 수 있다.
  var ROLE_COLORS=[
    {k:['주어'],                c:'#0a84ff', n:'주어',   kid:'👤 누가·무엇이'},
    {k:['술어','동사'],          c:'#ff375f', n:'술어',   kid:'🏃 한다·이다'},
    {k:['목적'],                c:'#30d158', n:'목적어', kid:'🎯 무엇을'},
    {k:['보어'],                c:'#ff9f0a', n:'보어',   kid:'✨ 어떠한지'},
    {k:['관계절'],              c:'#5e5ce6', n:'관계절', kid:'🔍 자세히 설명'},
    {k:['관사'],                c:'#8e8e93', n:'기능어', kid:'🔧 작은 말'}, // 수식보다 앞: '수식(관사)'도 회색
    {k:['수식'],                c:'#bf5af2', n:'수식',   kid:'🎨 꾸며 주는 말'},
    {k:['병렬','대등','등위'],     c:'#a2845e', n:'병렬',   kid:'➕ 나란히'},
    {k:['종속','접속','절'],       c:'#0aa2c0', n:'종속/절', kid:'🔗 이어 주는 말'},
    {k:['전치','한정','관계대명사'], c:'#8e8e93', n:'기능어', kid:'🔧 작은 말'}
  ];
  var ROLE_DEFAULT={c:'#8e8e93', n:'기능어', kid:'🔧 작은 말'};
  function roleInfo(s){
    s=s||'';
    for(var i=0;i<ROLE_COLORS.length;i++){
      for(var j=0;j<ROLE_COLORS[i].k.length;j++){ if(s.indexOf(ROLE_COLORS[i].k[j])>=0) return ROLE_COLORS[i]; }
    }
    return ROLE_DEFAULT;
  }
  function roleColor(s){ return roleInfo(s).c; }
  function legendEl(){
    var lg=el('div','gram-legend'), seen={};
    ROLE_COLORS.forEach(function(b){
      if(seen[b.n]) return; seen[b.n]=1; // 같은 이름(기능어) 중복 표시 방지
      var it=el('span'); var sw=el('i'); sw.style.background=b.c;
      it.appendChild(sw); it.appendChild(document.createTextNode(b.n)); lg.appendChild(it);
    });
    return lg;
  }

  // 짧은 문장은 아크(어순+관계), 노드가 이보다 많으면 기본을 트리(위계)로.
  var TREE_MIN=12;

  // 아크 다이어그램: 토큰 칩을 가로로 놓고 그 위에 head→dependent 관계를 SVG 곡선으로.
  function renderArc(host, nodes, edges){
    var scroll=el('div','gram-scroll');
    var wrap=el('div','gram-wrap');
    var svg=sv('svg'); svg.setAttribute('class','gram-svg'); wrap.appendChild(svg);
    var row=el('div','gram-row'), byId={};
    nodes.forEach(function(n){
      var chip=el('div','gram-node');
      if(n.ko) chip.title=n.ko; // 마우스 올리면 우리말 뜻
      chip.appendChild(el('span','gram-text', n.text||''));
      if(n.role){ var rr=el('span','gram-role', n.role); rr.style.color=roleColor(n.role); chip.appendChild(rr); }
      row.appendChild(chip);
      if(n.id) byId[n.id]=chip;
    });
    wrap.appendChild(row); scroll.appendChild(wrap); host.appendChild(scroll);

    function draw(){
      var wr=wrap.getBoundingClientRect();
      svg.setAttribute('width',wr.width); svg.setAttribute('height',wr.height);
      svg.setAttribute('viewBox','0 0 '+wr.width+' '+wr.height);
      while(svg.firstChild) svg.removeChild(svg.firstChild);
      var any=row.querySelector('.gram-node'); if(!any) return;
      var baseY=any.getBoundingClientRect().top - wr.top; // 칩 상단 = 아크가 만나는 선
      edges.forEach(function(e,i){
        var a=byId[e.from], b=byId[e.to]; if(!a||!b) return;
        var ra=a.getBoundingClientRect(), rb=b.getBoundingClientRect();
        var sx=(ra.left+ra.right)/2 - wr.left, ex=(rb.left+rb.right)/2 - wr.left;
        var color=roleColor(e.label); // 관계 종류(주어·목적어·수식…)별 색
        // 아크 높이. 위쪽에 라벨 글자가 잘리지 않도록 apex는 최소 24px(=baseY-24 상한)까지만.
        var apexY=baseY - Math.max(16, Math.min(baseY-24, 22 + Math.abs(ex-sx)*0.34));
        var p=sv('path');
        p.setAttribute('d','M '+sx+' '+baseY+' C '+sx+' '+apexY+', '+ex+' '+apexY+', '+ex+' '+baseY);
        p.setAttribute('fill','none'); p.setAttribute('stroke',color);
        p.setAttribute('stroke-width','1.8'); p.setAttribute('opacity','.7');
        svg.appendChild(p);
        var ah=sv('path'); // dependent(도착) 쪽 화살표
        ah.setAttribute('d','M '+(ex-4)+' '+(baseY-7)+' L '+ex+' '+(baseY-1)+' L '+(ex+4)+' '+(baseY-7));
        ah.setAttribute('fill','none'); ah.setAttribute('stroke',color);
        ah.setAttribute('stroke-width','1.8'); ah.setAttribute('opacity','.7');
        svg.appendChild(ah);
        if(e.label){
          var tx=sv('text'); tx.setAttribute('x',(sx+ex)/2); tx.setAttribute('y',apexY-2);
          tx.setAttribute('text-anchor','middle'); tx.setAttribute('class','gram-label');
          tx.setAttribute('fill',color); tx.textContent=e.label;
          svg.appendChild(tx);
        }
      });
    }
    drawers.push(draw);
    requestAnimationFrame(draw);
  }

  // 구성성분 트리: 의존 엣지에서 루트(들어오는 엣지 없는 노드=대개 본동사)를 찾아,
  // 자식을 원문 순서로 정렬해 들여쓰기 트리로 그린다. 위계·내포가 명확하고 긴 문장에 강함.
  function renderTree(host, nodes, edges){
    var byId={}; nodes.forEach(function(n,i){ n.__i=i; byId[n.id]=n; });
    var children={}, hasParent={};
    edges.forEach(function(e){
      if(!byId[e.from]||!byId[e.to]) return;
      (children[e.from]=children[e.from]||[]).push({id:e.to, label:e.label});
      hasParent[e.to]=true;
    });
    Object.keys(children).forEach(function(k){
      children[k].sort(function(a,b){ return byId[a.id].__i - byId[b.id].__i; });
    });
    var roots=nodes.filter(function(n){ return !hasParent[n.id]; });
    if(!roots.length && nodes.length) roots=[nodes[0]];
    var seen={};
    function build(nid, rel){
      if(seen[nid]) return null; seen[nid]=1; // 사이클 방어
      var n=byId[nid]; if(!n) return null;
      var li=el('li','gt-node');
      var head=el('div','gt-head');
      // 자식을 먼저 만들어(재귀) sub에 담고, 있으면 +/- 토글을 붙인다.
      var kids=children[nid], sub=null;
      if(kids && kids.length){
        sub=el('ul','gt-kids');
        kids.forEach(function(k){ var c=build(k.id, k.label); if(c) sub.appendChild(c); });
        if(!sub.childNodes.length) sub=null;
      }
      if(sub){
        var tg=el('button','gt-toggle','−');
        tg.setAttribute('aria-label','접기/펼치기');
        tg.onclick=function(){
          if(sub.hasAttribute('hidden')){ sub.removeAttribute('hidden'); tg.textContent='−'; }
          else { sub.setAttribute('hidden',''); tg.textContent='+'; }
        };
        head.appendChild(tg);
      } else {
        head.appendChild(el('span','gt-lead')); // 리프: 토글 자리 정렬용 여백
      }
      if(rel){ var rp=el('span','gt-rel', rel); rp.style.background=roleColor(rel); head.appendChild(rp); }
      head.appendChild(el('span','gt-text', n.text||''));
      if(n.ko) head.appendChild(el('span','gt-ko', n.ko));
      if(n.role && n.role!==rel){ var rr=el('span','gt-role', n.role); rr.style.color=roleColor(n.role); head.appendChild(rr); }
      li.appendChild(head);
      if(sub) li.appendChild(sub);
      return li;
    }
    var rootUl=el('ul','gt-root');
    roots.forEach(function(r){ var li=build(r.id, ''); if(li) rootUl.appendChild(li); });
    host.appendChild(rootUl);
  }

  // 쉬운 뷰(초등학생용): 다이어그램 없이, 문장의 큰 덩어리(주절 본동사 + 그 직속 성분)를
  // 원문 순서대로 색칠 카드로 보여준다 — 쉬운 말 라벨 + 영어 + 우리말 뜻.
  // 종속절·관계절처럼 자식이 있는 덩어리는 '＋ 자세히'로 그 속을 펼쳐 볼 수 있다.
  function renderEasy(host, nodes, edges){
    var byId={}; nodes.forEach(function(n,i){ n.__i=i; byId[n.id]=n; });
    var children={}, hasParent={};
    edges.forEach(function(e){
      if(!byId[e.from]||!byId[e.to]) return;
      (children[e.from]=children[e.from]||[]).push(e.to); hasParent[e.to]=true;
    });
    Object.keys(children).forEach(function(k){
      children[k].sort(function(a,b){ return byId[a].__i - byId[b].__i; });
    });
    var roots=nodes.filter(function(n){ return !hasParent[n.id]; });
    if(!roots.length && nodes.length) roots=[nodes[0]];
    var rootIds={}; roots.forEach(function(r){ rootIds[r.id]=1; });
    var built={};

    // 노드 하나를 카드로. 자식이 있고 루트가 아니면 '＋ 자세히'로 하위를 지연 렌더.
    function card(nid){
      if(built[nid]) return null; built[nid]=1; // 사이클/중복 방어
      var n=byId[nid]; if(!n) return null;
      var info=roleInfo(n.role);
      var item=el('div','easy-item');
      var c=el('div','easy-part'); c.style.borderLeftColor=info.c;
      var tag=el('span','easy-tag', info.kid); tag.style.background=info.c;
      var body=el('div','easy-body');
      body.appendChild(el('div','easy-en', n.text||''));
      if(n.ko) body.appendChild(el('div','easy-ko', n.ko));
      c.appendChild(tag); c.appendChild(body);
      var kids=children[nid];
      if(kids && kids.length && !rootIds[nid]){ // 루트 자식은 이미 최상위에 있어 제외
        var kidbox=el('div','easy-kids'); kidbox.hidden=true;
        var tg=el('button','easy-more','＋ 자세히');
        tg.onclick=function(){
          if(!kidbox.dataset.built){
            kids.forEach(function(cid){ var cc=card(cid); if(cc) kidbox.appendChild(cc); });
            kidbox.dataset.built='1';
          }
          kidbox.hidden=!kidbox.hidden;
          tg.textContent=kidbox.hidden?'＋ 자세히':'－ 접기';
        };
        c.appendChild(tg);
        item.appendChild(c); item.appendChild(kidbox);
      } else {
        item.appendChild(c);
      }
      return item;
    }

    // 최상위 = 루트(본동사) + 루트의 직속 자식. 원문 순서로.
    var topIds=[], seen={};
    roots.forEach(function(r){
      [r.id].concat(children[r.id]||[]).forEach(function(id){ if(!seen[id]){ seen[id]=1; topIds.push(id); } });
    });
    topIds.sort(function(a,b){ return byId[a].__i - byId[b].__i; });

    var wrap=el('div','easy');
    topIds.forEach(function(id){ var cc=card(id); if(cc) wrap.appendChild(cc); });
    if(!topIds.length) wrap.appendChild(el('div','muted','보여줄 내용이 없어요.'));
    host.appendChild(wrap);
  }

  function render(box, d){
    box.textContent='';
    var nodes=d.nodes||[], edges=d.edges||[];
    if(d.summary) box.appendChild(el('div','gram-summary','🔎 '+d.summary));
    if(nodes.length){
      var bar=el('div','gram-bar');
      var toggle=el('div','gram-toggle');
      var bEasy=el('button','gv-btn','🌱 쉬운'), bArc=el('button','gv-btn','아크'), bTree=el('button','gv-btn','트리');
      var view=el('div','gram-view');
      function setMode(m){
        bEasy.classList.toggle('on', m==='easy'); bArc.classList.toggle('on', m==='arc'); bTree.classList.toggle('on', m==='tree');
        view.textContent='';
        if(m==='tree') renderTree(view, nodes, edges);
        else if(m==='easy') renderEasy(view, nodes, edges);
        else renderArc(view, nodes, edges);
      }
      bEasy.onclick=function(){ setMode('easy'); };
      bArc.onclick=function(){ setMode('arc'); };
      bTree.onclick=function(){ setMode('tree'); };
      toggle.appendChild(bEasy); toggle.appendChild(bArc); toggle.appendChild(bTree);
      // 캐시 무시하고 재분석(프롬프트 개선 후 낡은 결과 갱신용). 성공 시 박스 전체 재렌더.
      var bRe=el('button','gv-refresh','🔄 다시 분석');
      bRe.onclick=function(){
        bRe.disabled=true; bRe.textContent='분석 중…';
        fetch('/sentences/grammar?refresh=1&text='+encodeURIComponent(box.dataset.text))
          .then(function(r){ if(!r.ok) throw 0; return r.json(); })
          .then(function(nd){ render(box, nd); })
          .catch(function(){ bRe.disabled=false; bRe.textContent='🔄 다시 분석'; });
      };
      bar.appendChild(toggle); bar.appendChild(bRe);
      box.appendChild(bar); box.appendChild(legendEl()); box.appendChild(view);
      setMode('easy'); // 기본은 쉬운 뷰(초등학생 우선). 아크/트리는 클릭으로.
    }
    if(d.points && d.points.length){
      box.appendChild(el('div','gram-plabel','📖 문법 포인트'));
      var ul=el('ul','gram-points');
      d.points.forEach(function(p){
        var li=el('li');
        li.appendChild(el('span','pt-text', p));
        var btn=el('button','pt-btn','자세히');
        var det=el('div','pt-detail'); det.hidden=true;
        btn.addEventListener('click', function(){
          if(det.dataset.loaded){ det.hidden=!det.hidden; return; }
          det.hidden=false; det.textContent='불러오는 중…'; btn.disabled=true;
          fetch('/sentences/point?text='+encodeURIComponent(box.dataset.text)+'&point='+encodeURIComponent(p))
            .then(function(r){ if(!r.ok) throw 0; return r.json(); })
            .then(function(pd){ renderDetail(det, pd); det.dataset.loaded='1'; })
            .catch(function(){ det.textContent='설명을 불러오지 못했어요. 잠시 후 다시 시도해 주세요.'; })
            .then(function(){ btn.disabled=false; });
        });
        li.appendChild(btn); li.appendChild(det);
        ul.appendChild(li);
      });
      box.appendChild(ul);
    }
    if(!box.childNodes.length) box.appendChild(el('div','muted','분석 결과가 비어 있어요.'));
  }

  function renderDetail(box, pd){
    box.textContent='';
    if(pd.explanation) box.appendChild(el('div','pt-expl', pd.explanation));
    if(pd.examples && pd.examples.length){
      var wrap=el('div','pt-ex');
      pd.examples.forEach(function(e){
        var item=el('div','pt-ex-item');
        item.appendChild(el('div','pt-en', e.en||''));
        if(e.ko) item.appendChild(el('div','pt-ko', e.ko||''));
        wrap.appendChild(item);
      });
      box.appendChild(wrap);
    }
    if(!box.childNodes.length) box.appendChild(el('div','muted','(내용 없음)'));
  }

  window.gramRender = render;
  var tmr; window.addEventListener('resize', function(){ clearTimeout(tmr); tmr=setTimeout(function(){ drawers.forEach(function(f){ f(); }); },150); });
})();
"#;

/// 문장 카드의 '🔍 문법 분석' 버튼 wiring: /sentences/grammar를 fetch해 공유 렌더러
/// (window.gramRender)로 그래프를 그린다. 렌더 로직 자체는 GRAPH_RENDER_JS에 있음.
const SENTENCE_GRAPH_JS: &str = r#"
(function(){
  var modal, mBody, mTitle;
  function ensureModal(){
    if(modal) return;
    modal=document.createElement('div'); modal.className='gram-modal'; modal.hidden=true;
    var box=document.createElement('div'); box.className='gram-modal-box';
    var bar=document.createElement('div'); bar.className='gram-modal-bar';
    mTitle=document.createElement('div'); mTitle.className='gram-modal-title';
    var close=document.createElement('button'); close.className='gram-modal-close'; close.textContent='✕';
    close.setAttribute('aria-label','닫기'); close.onclick=hide;
    bar.appendChild(mTitle); bar.appendChild(close);
    mBody=document.createElement('div'); mBody.className='gram-modal-body';
    box.appendChild(bar); box.appendChild(mBody); modal.appendChild(box);
    modal.addEventListener('click', function(e){ if(e.target===modal) hide(); }); // 바깥 클릭 닫기
    document.addEventListener('keydown', function(e){ if(!modal.hidden && e.key==='Escape') hide(); });
    document.body.appendChild(modal);
  }
  function show(){ modal.hidden=false; document.body.style.overflow='hidden'; }
  function hide(){ if(modal){ modal.hidden=true; document.body.style.overflow=''; } }

  document.querySelectorAll('.gram-btn').forEach(function(btn){
    btn.addEventListener('click', function(){
      var card=btn.closest('.card'); var holder=card.querySelector('.gram');
      var sentence=holder ? holder.dataset.text : '';
      ensureModal();
      mTitle.textContent=sentence;
      mBody.dataset.text=sentence;
      mBody.textContent='분석 중…';
      show(); btn.disabled=true;
      fetch('/sentences/grammar?text='+encodeURIComponent(sentence))
        .then(function(r){ if(!r.ok) throw 0; return r.json(); })
        .then(function(d){ window.gramRender(mBody,d); })
        .catch(function(){ mBody.textContent='분석을 불러오지 못했어요. 잠시 후 다시 시도해 주세요.'; })
        .then(function(){ btn.disabled=false; });
    });
  });
})();
"#;

/// 문법 카드 복습 덱: #deck(문장 JSON)을 셔플해 한 장씩 보여준다. 앞면은 영어 문장,
/// '구조 보기'를 누르면 /sentences/grammar로 그래프를 로드해 공유 렌더러로 그린다.
const GRAMMAR_REVIEW_JS: &str = r#"
(function(){
  var deck = JSON.parse(document.getElementById('deck').textContent);
  for (var i=deck.length-1;i>0;i--){var j=Math.floor(Math.random()*(i+1));var t=deck[i];deck[i]=deck[j];deck[j]=t;}
  var root=document.getElementById('rv'), idx=0;
  function esc(s){var d=document.createElement('div');d.textContent=(s==null?'':s);return d.innerHTML;}
  function done(){
    root.innerHTML='<div class="card"><p>복습 완료! 🎉 '+deck.length+'개 문장을 봤어요.</p>'+
      '<p class="actions"><a class="chip" href="/sentences/review">다시</a> <a class="chip" href="/sentences">문장 목록</a></p></div>';
  }
  function render(){
    if(idx>=deck.length){ done(); return; }
    var c=deck[idx];
    root.innerHTML=
      '<p class="count">'+(idx+1)+' / '+deck.length+'</p>'+
      '<div class="card review">'+
        '<div class="head"><span class="badge">'+esc(c.category)+'</span></div>'+
        '<blockquote class="sentence">'+esc(c.text)+'</blockquote>'+
        '<div class="actions">'+
          '<button id="reveal">구조 보기</button>'+
          '<span id="nav" hidden><button id="next">다음 ▶</button></span>'+
        '</div>'+
        '<div class="gram" hidden></div>'+
        '<div class="kbd">Space 구조 보기 · n 다음</div>'+
      '</div>';
    root.querySelector('.gram').dataset.text=c.text;
    document.getElementById('reveal').onclick=reveal;
    document.getElementById('next').onclick=next;
  }
  function reveal(){
    var box=root.querySelector('.gram'); if(!box||box.dataset.loaded) return;
    document.getElementById('reveal').disabled=true;
    box.hidden=false; box.textContent='분석 중…';
    fetch('/sentences/grammar?text='+encodeURIComponent(box.dataset.text))
      .then(function(r){ if(!r.ok) throw 0; return r.json(); })
      .then(function(d){ window.gramRender(box,d); box.dataset.loaded='1'; })
      .catch(function(){ box.textContent='분석을 불러오지 못했어요. 잠시 후 다시 시도해 주세요.'; })
      .then(function(){ document.getElementById('reveal').hidden=true; document.getElementById('nav').hidden=false; });
  }
  function next(){ idx++; render(); }
  document.addEventListener('keydown', function(e){
    if(idx>=deck.length) return;
    var box=root.querySelector('.gram'); var revealed=box && !box.hidden;
    if(!revealed && (e.key===' '||e.key==='Enter')){ e.preventDefault(); reveal(); }
    else if(revealed && (e.key==='n'||e.key==='2'||e.key==='ArrowRight')){ next(); }
  });
  if(!deck.length){ root.innerHTML='<p class="empty">복습할 문장이 없습니다. <a href="/">본문을 붙여넣어</a> 추출해 보세요.</p>'; }
  else render();
})();
"#;

/// 한글 요약 초안: 버튼 클릭 시 /entries/:id/summary 를 fetch해 블로그용 textarea +
/// X 스레드(트윗별 textarea·글자수·복사)를 그린다. 편집은 브라우저에서만(서버 저장 안 함).
const SUMMARY_JS: &str = r#"
(function(){
  var btn=document.getElementById('sumbtn'); if(!btn) return;
  var box=document.getElementById('summary');
  function el(t,c,x){ var e=document.createElement(t); if(c)e.className=c; if(x!=null)e.textContent=x; return e; }
  // X 가중 글자수: 한글/CJK 등은 2로 계산(트위터 방식 근사).
  function weight(s){ var n=0; for(var i=0;i<s.length;i++){ var c=s.charCodeAt(i); n += (c>=0x1100 ? 2 : 1); } return n; }
  function copyBtn(label, getText){
    var b=el('button','copy-btn',label);
    b.onclick=function(){
      var t=getText();
      var done=function(){ var o=b.textContent; b.textContent='복사됨!'; setTimeout(function(){ b.textContent=o; },1200); };
      if(navigator.clipboard && navigator.clipboard.writeText){ navigator.clipboard.writeText(t).then(done).catch(fallback); }
      else fallback();
      function fallback(){ var ta=document.createElement('textarea'); ta.value=t; document.body.appendChild(ta); ta.select(); try{document.execCommand('copy');}catch(e){} document.body.removeChild(ta); done(); }
    };
    return b;
  }
  function autorows(ta){ ta.style.height='auto'; ta.style.height=(ta.scrollHeight+4)+'px'; }

  btn.addEventListener('click', function(){
    var orig='📝 한글 요약 초안 생성';
    var url='/entries/'+btn.dataset.entry+'/summary'+(btn.dataset.loaded?'?force=1':'');
    btn.disabled=true; btn.textContent='생성 중… (10~20초)';
    fetch(url)
      .then(function(r){ if(!r.ok) throw 0; return r.json(); })
      .then(function(d){ render(d); btn.dataset.loaded='1'; btn.textContent='🔄 다시 생성'; btn.disabled=false; })
      .catch(function(){ btn.textContent=btn.dataset.loaded?'🔄 다시 생성':orig; btn.disabled=false; box.textContent='생성에 실패했어요. 잠시 후 다시 시도해 주세요.'; });
  });

  function render(d){
    box.textContent='';
    // 블로그용: 렌더된 마크다운(MD 뷰어)로 보여주고, 소스는 토글로 편집/복사.
    box.appendChild(el('div','sum-h','📄 블로그용'));
    var view=el('div','md-view'); view.innerHTML=d.blog_html||''; box.appendChild(view);

    var bta=document.createElement('textarea'); bta.className='sum-ta'; bta.value=d.blog||'';
    bta.style.display='none';
    box.appendChild(bta); autorows(bta);
    bta.addEventListener('input',function(){ autorows(bta); });

    var brow=el('div','sum-row');
    var srcBtn=el('button','copy-btn','</> 소스 보기');
    srcBtn.onclick=function(){
      var showing=bta.style.display!=='none';
      bta.style.display=showing?'none':'block';
      view.style.display=showing?'block':'none';
      srcBtn.textContent=showing?'</> 소스 보기':'👁 미리보기';
      if(!showing) autorows(bta);
    };
    brow.appendChild(srcBtn);
    brow.appendChild(copyBtn('블로그 전체 복사',function(){return bta.value;}));
    box.appendChild(brow);

    // X 스레드
    box.appendChild(el('div','sum-h','🧵 X 스레드'));
    var tweets=(d.thread||[]); var tareas=[];
    tweets.forEach(function(t,i){
      var wrap=el('div','tweet');
      var ta=document.createElement('textarea'); ta.className='tw-ta'; ta.value=t; tareas.push(ta);
      wrap.appendChild(ta); autorows(ta);
      var meta=el('div','tw-meta');
      meta.appendChild(el('span','tw-n',(i+1)+'/'+tweets.length));
      var cc=el('span','cc');
      function upd(){ var w=weight(ta.value); cc.textContent=w+' / 280'; cc.className='cc'+(w>280?' over':''); }
      ta.addEventListener('input',function(){ autorows(ta); upd(); }); upd();
      meta.appendChild(cc);
      meta.appendChild(copyBtn('복사',(function(x){return function(){return x.value;};})(ta)));
      wrap.appendChild(meta); box.appendChild(wrap);
    });
    var allrow=el('div','sum-row');
    allrow.appendChild(copyBtn('스레드 전체 복사',function(){ return tareas.map(function(x){return x.value;}).join('\n\n'); }));
    box.appendChild(allrow);
  }
})();
"#;

/// 리더 뷰 ↔ 편집 폼 토글. "편집"을 누르면 렌더 뷰를 숨기고 원문 textarea를 보여준다.
const READER_EDIT_JS: &str = r#"
(function(){
  var btn=document.getElementById('editbtn');
  var view=document.getElementById('reader-view');
  var edit=document.getElementById('reader-edit');
  var hint=document.querySelector('.reader-hint');
  var cancel=document.getElementById('editcancel');
  if(!btn||!view||!edit) return;
  function show(editing){
    view.style.display=editing?'none':'';
    edit.style.display=editing?'block':'none';
    btn.style.display=editing?'none':'';
    if(hint) hint.style.display=editing?'none':'';
    if(editing){
      var ta=edit.querySelector('textarea');
      if(ta){ ta.style.height='auto'; ta.style.height=Math.min(ta.scrollHeight+4,600)+'px'; ta.focus(); }
    }
  }
  btn.addEventListener('click',function(){ show(true); });
  if(cancel) cancel.addEventListener('click',function(){ show(false); });
})();
"#;

/// 리더 컨트롤 통합: 청크 리딩(VSTF 계단식) + 읽어주기(TTS 하이라이트)를 한 스크립트로 묶어
/// 서로 협조하게 한다. 특히 '청크 모드에서 재생하면 청크 줄을 시간에 맞춰 하이라이트'한다.
/// 어휘 밑줄·기능어 흐리게는 두 뷰 공용. 근거: 안구운동(내용어 고정), 청크 리딩(CRST), VSTF.
const READER_JS: &str = r#"
(function(){
  var readerView=document.getElementById('reader-view'); if(!readerView) return;
  var chunkBtn=document.getElementById('chunkbtn');
  var ttsBtn=document.getElementById('ttsbtn');
  var audio=document.getElementById('ttsaudio');
  var rate=document.getElementById('ttsrate');
  var ta=document.querySelector('#reader-edit textarea');
  var pacebtn=document.getElementById('pacebtn');
  var toeicSel=document.getElementById('toeic');
  var vocab={}; try{ var vn=document.getElementById('reader-vocab'); if(vn) vocab=JSON.parse(vn.textContent||'{}'); }catch(e){}
  var baseHTML=readerView.innerHTML;
  var chunkOn=false, align=null, audioReady=false, units=[], cur=-1;
  var paceOn=false, paceIdx=0, paceTimer=null, paceList=[];
  // 토익 선택 기억(다른 기사에서도 유지).
  try{ var sv=localStorage.getItem('toeicWpm'); if(sv&&toeicSel) toeicSel.value=sv; }catch(e){}
  if(toeicSel) toeicSel.addEventListener('change', function(){ try{ localStorage.setItem('toeicWpm', toeicSel.value); }catch(e){} });

  // 플로팅 멈춤/이어 읽기 버튼(스크롤해도 항상 우하단에 뜬다). 진행 중인 읽기(속도 읽기 또는
  // 읽어주기)가 있을 때만 보이며, 클릭하면 그 읽기의 재생/일시정지를 토글한다.
  var fab=document.createElement('button'); fab.className='reader-fab'; fab.hidden=true;
  fab.setAttribute('aria-label','멈춤/이어 읽기'); document.body.appendChild(fab);
  function ttsBusy(){ return !!(audio && audioReady && !audio.ended); }
  function readerPlaying(){ if(ttsBusy()) return !audio.paused; if(paceList.length) return paceOn; return false; }
  function refreshFab(){ var busy=ttsBusy()||paceList.length>0; fab.hidden=!busy; if(busy) fab.textContent=readerPlaying()?'⏸':'▶'; }
  fab.addEventListener('click', function(){ if(ttsBusy()){ if(ttsBtn) ttsBtn.click(); } else if(paceList.length){ if(pacebtn) pacebtn.click(); } });

  var FN={}, TRIG={};
  ("a an the of to in on at for with from by as into onto about over under above below after before between through during without within is are was were be been being am do does did have has had will would can could may might must shall should and or but nor so yet not no than then there here it its his her their our your my we you they them").split(' ').forEach(function(w){ FN[w]=1; });
  ("to of in on at for with from by as into onto about over under after before between through that which who whom whose because although though while when where if since unless and but or so yet").split(' ').forEach(function(w){ TRIG[w]=1; });
  function isTok(ch){ return /[\p{L}'’]/u.test(ch); }
  function tokenize(s){ var t=[],i=0,n=s.length; while(i<n){ var j=i; while(j<n&&isTok(s[j]))j++; var w=s.slice(i,j),k=j; while(k<n&&!isTok(s[k]))k++; var sep=s.slice(j,k); if(w||sep)t.push({w:w,sep:sep}); i=k; } return t; }
  function b64ToBlob(b64,type){ var bin=atob(b64),n=bin.length,a=new Uint8Array(n); for(var i=0;i<n;i++)a[i]=bin.charCodeAt(i); return new Blob([a],{type:type}); }
  // 토큰들을 host에 채우되 vocab은 밑줄(mark), 기능어는 흐리게(fn-word).
  function fillTokens(host, toks){ toks.forEach(function(t){ if(t.w){ var lw=t.w.toLowerCase(), el, def=vocab[lw]; if(def){ el=document.createElement('mark'); el.className='vocab'; el.setAttribute('data-def',def); el.textContent=t.w; } else { el=document.createElement('span'); el.textContent=t.w; if(FN[lw]) el.className='fn-word'; } host.appendChild(el); } if(t.sep) host.appendChild(document.createTextNode(t.sep)); }); }

  // ---- 청크 뷰 ----
  function chunkLine(host, toks, sub){ var line=document.createElement('div'); line.className='chunk-line'+(sub?' chunk-sub':''); fillTokens(line, toks); host.appendChild(line); }
  function chunkSentence(toks){ var out=[],c=[]; for(var i=0;i<toks.length;i++){ var t=toks[i],lw=(t.w||'').toLowerCase(); if(c.length&&(TRIG[lw]||c.length>=5)){ out.push(c); c=[]; } c.push(t); if(/[,;:—–]/.test(t.sep)){ out.push(c); c=[]; } } if(c.length)out.push(c); return out; }
  function splitSents(p){ return p.replace(/\s+/g,' ').trim().split(/(?<=[.!?])\s+/); }
  function buildRule(){ var raw=ta?ta.value:(readerView.textContent||''); var art=document.createElement('article'); art.className='reader chunk-view';
    raw.replace(/\r/g,'').split(/\n\n+/).forEach(function(p){ p=p.replace(/\n/g,' ').trim(); if(!p)return; var pd=document.createElement('div'); pd.className='chunk-para';
      splitSents(p).forEach(function(s){ chunkSentence(tokenize(s)).forEach(function(ch){ var f=(ch[0]&&ch[0].w||'').toLowerCase(); chunkLine(pd, ch, TRIG[f]); }); }); art.appendChild(pd); });
    return art; }
  function buildParas(paras){ var art=document.createElement('article'); art.className='reader chunk-view';
    paras.forEach(function(chunks){ var pd=document.createElement('div'); pd.className='chunk-para';
      chunks.forEach(function(ct){ var toks=tokenize(String(ct)); var f=(toks[0]&&toks[0].w||'').toLowerCase(); chunkLine(pd, toks, TRIG[f]); }); art.appendChild(pd); });
    return art; }

  // ---- read-along(문장) 뷰 ----
  function buildSentences(chars,starts,ends){ var out=[],st=-1; for(var i=0;i<chars.length;i++){ if(st<0)st=i; var c=chars[i]; var endMark=/[.!?]/.test(c)&&((i+1>=chars.length)||/\s/.test(chars[i+1])); if(endMark||c==='\n'||i===chars.length-1){ out.push({start:starts[st]||0,end:ends[i]||0,text:chars.slice(st,i+1).join('')}); st=-1; } } return out; }
  function renderSentences(){ var art=document.createElement('article'); art.className='reader tts-readalong'; var sents=align?buildSentences(align.chars,align.starts,align.ends):[], els=[];
    sents.forEach(function(s){ var sp=document.createElement('span'); sp.className='tts-sent'; fillTokens(sp, tokenize(s.text)); art.appendChild(sp); els.push(sp); });
    readerView.innerHTML=''; readerView.appendChild(art); cur=-1;
    units=sents.map(function(s,k){ return {el:els[k], start:s.start, end:s.end}; }); }

  // ---- 청크 줄에 시각 부여(오디오 하이라이트용): 청크 텍스트를 TTS 문자열에 순서대로 정렬 ----
  function assignChunkTimes(){ units=[]; cur=-1; if(!align) return; var lines=readerView.querySelectorAll('.chunk-line'), chars=align.chars, pos=0;
    lines.forEach(function(line){ var txt=line.textContent, s=-1, e=-1;
      for(var k=0;k<txt.length;k++){ if(/\s/.test(txt[k])) continue; while(pos<chars.length&&/\s/.test(chars[pos]))pos++; if(pos>=chars.length)break; if(s<0)s=pos; e=pos; pos++; }
      if(s>=0) units.push({el:line, start:align.starts[s]||0, end:align.ends[e]||0}); }); }

  function highlight(t){ var idx=-1; for(var k=0;k<units.length;k++){ if(t>=units[k].start && t<units[k].end){ idx=k; break; } } if(idx===cur) return; cur=idx;
    var cls=chunkOn?'chunk-on':'tts-on'; units.forEach(function(u,k){ u.el.classList.toggle(cls, k===idx); }); if(idx>=0&&units[idx]) units[idx].el.scrollIntoView({block:'center',behavior:'smooth'}); }
  // 현재 뷰에 맞춰 하이라이트 대상 재설정.
  function prepareUnits(){ if(chunkOn) assignChunkTimes(); else renderSentences(); }
  function audioActive(){ return audioReady && !audio.ended; }

  // ---- 청크 토글 ----
  function showChunk(cb){ readerView.innerHTML='<p class="muted">🧩 분석 중…</p>';
    fetch('/entries/'+chunkBtn.dataset.entry+'/chunks').then(function(r){ if(!r.ok)throw 0; return r.json(); })
      .then(function(d){ var paras=(d&&d.paras)||[]; readerView.innerHTML=''; readerView.appendChild(paras.length?buildParas(paras):buildRule()); })
      .catch(function(){ readerView.innerHTML=''; readerView.appendChild(buildRule()); })
      .then(function(){ if(cb)cb(); }); }
  if(chunkBtn) chunkBtn.addEventListener('click', function(){
    stopPace();
    if(chunkOn){ chunkOn=false; chunkBtn.textContent='🧩 청크 리딩';
      if(audio && audioActive()){ renderSentences(); if(!audio.paused) highlight(audio.currentTime); }
      else { readerView.innerHTML=baseHTML; units=[]; cur=-1; }
      return; }
    chunkOn=true; chunkBtn.disabled=true; chunkBtn.textContent='📖 일반 보기';
    showChunk(function(){ chunkBtn.disabled=false; if(audio && audioActive()){ assignChunkTimes(); if(!audio.paused) highlight(audio.currentTime); } });
  });

  // ---- 읽어주기 ----
  function applyRate(){ if(rate&&audio) audio.playbackRate=parseFloat(rate.value)||1; }
  if(ttsBtn && audio){
    audio.addEventListener('timeupdate', function(){ if(units.length) highlight(audio.currentTime); });
    audio.addEventListener('play', function(){ ttsBtn.textContent='⏸ 일시정지'; refreshFab(); });
    audio.addEventListener('pause', function(){ if(!audio.ended) ttsBtn.textContent='▶ 이어 듣기'; refreshFab(); });
    audio.addEventListener('ended', function(){ ttsBtn.textContent='🔊 다시 듣기'; if(chunkOn){ units.forEach(function(u){ u.el.classList.remove('chunk-on'); }); cur=-1; } else { readerView.innerHTML=baseHTML; units=[]; cur=-1; } refreshFab(); });
    ttsBtn.addEventListener('click', function(){
      stopPace();
      if(!audioReady){
        ttsBtn.disabled=true; ttsBtn.textContent='🔊 생성 중…';
        fetch('/entries/'+ttsBtn.dataset.entry+'/tts').then(function(r){ if(!r.ok)throw 0; return r.json(); })
          .then(function(d){ var a=d.alignment||{}; align={chars:a.characters||[],starts:a.character_start_times_seconds||[],ends:a.character_end_times_seconds||[]};
            audio.hidden=false; audio.src=URL.createObjectURL(b64ToBlob(d.audio_base64||'','audio/mpeg')); applyRate(); audioReady=true; ttsBtn.disabled=false;
            prepareUnits(); audio.play().catch(function(){}); })
          .catch(function(){ ttsBtn.disabled=false; ttsBtn.textContent='🔊 읽어주기'; audio.hidden=true; alert('오디오를 만들지 못했어요. (키·크레딧을 확인하세요)'); });
        return;
      }
      if(audio.ended){ prepareUnits(); audio.currentTime=0; audio.play(); return; }
      if(audio.paused){ if(!units.length) prepareUnits(); audio.play(); } else audio.pause();
    });
    if(rate) rate.addEventListener('change', applyRate);
  }

  // ---- 속도 읽기: 토익 점수대 WPM으로 하이라이트만 진행(음성 없음) ----
  function curWpm(){ return (toeicSel && parseInt(toeicSel.value,10)) || 140; }
  // 완전 정지(모드 전환·읽어주기·완료 시): 상태 초기화해 다음엔 현재 뷰로 새로 시작한다.
  // (일시정지는 pacebtn 핸들러에서 인라인으로 처리 — paceList/paceIdx 유지해 이어 읽기 가능)
  function stopPace(){ if(paceTimer){ clearTimeout(paceTimer); paceTimer=null; } paceOn=false; paceList=[]; paceIdx=0; if(pacebtn) pacebtn.textContent='🏃 속도 읽기'; refreshFab(); }
  // 일반 모드용: 원문을 문장 span으로 렌더(어휘 밑줄 포함).
  function renderTextSentences(){
    var raw=ta?ta.value:''; var art=document.createElement('article'); art.className='reader';
    raw.replace(/\r/g,'').split(/\n\n+/).forEach(function(p){ p=p.replace(/\n/g,' ').trim(); if(!p)return; var pp=document.createElement('p'); pp.className='para';
      splitSents(p).forEach(function(s){ var sp=document.createElement('span'); sp.className='tts-sent'; fillTokens(sp, tokenize(s)); pp.appendChild(sp); pp.appendChild(document.createTextNode(' ')); }); art.appendChild(pp); });
    readerView.innerHTML=''; readerView.appendChild(art);
  }
  function paceStep(){
    if(!paceOn) return;
    if(paceIdx>=paceList.length){ stopPace(); return; }
    var cls=chunkOn?'chunk-on':'tts-on';
    paceList.forEach(function(u,k){ u.el.classList.toggle(cls, k===paceIdx); });
    paceList[paceIdx].el.scrollIntoView({block:'center',behavior:'smooth'});
    var dur=Math.max(300, paceList[paceIdx].words/curWpm()*60000); // 단어수/WPM
    paceTimer=setTimeout(function(){ paceIdx++; paceStep(); }, dur);
  }
  function startPace(){
    if(audio && !audio.paused) audio.pause();
    if(!chunkOn) renderTextSentences(); // 일반 모드면 문장 span 뷰로
    var els=readerView.querySelectorAll(chunkOn?'.chunk-line':'.tts-sent');
    paceList=Array.prototype.slice.call(els).map(function(el){ return {el:el, words:(el.textContent.trim().match(/\S+/g)||[]).length||1}; });
    paceIdx=0; paceOn=true; if(pacebtn) pacebtn.textContent='⏸ 멈춤'; paceStep(); refreshFab();
  }
  if(pacebtn) pacebtn.addEventListener('click', function(){
    if(paceOn){ if(paceTimer){ clearTimeout(paceTimer); paceTimer=null; } paceOn=false; pacebtn.textContent='▶ 이어 읽기'; refreshFab(); return; }
    if(paceList.length && paceIdx>0 && paceIdx<paceList.length){ paceOn=true; pacebtn.textContent='⏸ 멈춤'; paceStep(); refreshFab(); return; }
    startPace();
  });
})();
"#;

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
