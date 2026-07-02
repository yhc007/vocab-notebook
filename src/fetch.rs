use anyhow::{anyhow, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE};
use scraper::{Html, Selector};

/// 저장 본문 안전 상한(문자). 추출은 청크로 나눠 처리하므로(extract_chunked) 토큰
/// 폭주 걱정은 없고, 여기선 비정상적으로 큰 입력이 메모리/DB를 압박하지 않도록
/// 하드 상한만 둔다. 일반 기사·논문·책 챕터는 이 안에 전부 들어온다.
const MAX_CHARS: usize = 200_000;

/// 실제 브라우저(Chrome) UA. 단순한 봇 차단을 통과하기 위함.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";

/// URL을 가져와 기사 본문 텍스트를 뽑아낸다.
///
/// 정적 HTML의 `<p>`(가능하면 `<article>`/`<main>` 안쪽)만 모아 붙인다.
/// JS로 렌더링되거나 봇/페이월로 보호된 페이지(NYT 등)는 실패할 수 있다.
pub async fn fetch_article(url: &str) -> Result<String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(anyhow!("http/https URL만 지원합니다"));
    }

    // 브라우저처럼 보이도록 UA + Accept 헤더를 갖추고(gzip/br 자동 해제),
    // 리다이렉트를 따르며 넉넉한 타임아웃을 둔다.
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        ),
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-US,en;q=0.9,ko;q=0.8"),
    );
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(20))
        .build()?;

    let resp = client.get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        // 403/401 등은 대개 봇 차단·페이월이라, 직접 붙여넣기를 안내한다.
        if status.as_u16() == 403 || status.as_u16() == 401 || status.as_u16() == 429 {
            return Err(anyhow!(
                "이 사이트가 자동 접근을 차단했습니다(HTTP {}). 페이월/봇 차단일 수 있어요. \
                 기사 본문을 직접 복사해 붙여넣어 주세요.",
                status.as_u16()
            ));
        }
        return Err(anyhow!("URL 가져오기 실패: HTTP {status}"));
    }
    let html = resp.text().await?;

    // scraper 타입(Html)은 Send가 아니므로, 파싱은 await 사이에 두지 않고
    // 이 동기 함수 안에서 끝낸다.
    let text = html_to_article(&html);
    if text.trim().is_empty() {
        return Err(anyhow!(
            "본문을 추출하지 못했습니다 (로그인/JS 렌더링 페이지일 수 있어요). 본문을 직접 붙여넣어 주세요."
        ));
    }
    Ok(text)
}

/// 업로드된 PDF 바이트에서 텍스트를 뽑아낸다(동기·CPU 바운드 → spawn_blocking에서 호출).
/// 스캔 이미지 PDF는 추출 가능한 텍스트가 없어 에러가 난다(OCR 미지원).
///
/// 1순위로 poppler `pdftotext`(설치돼 있으면)를 쓴다. 순수 Rust `pdf_extract`는
/// 일부 폰트/인코딩의 PDF에서 뒷페이지를 놓치는 약점이 있어, 더 견고한 pdftotext를
/// 우선하고 없거나 실패하면 pdf_extract로 폴백한다.
pub fn extract_pdf_text(bytes: &[u8]) -> Result<String> {
    let text = match pdftotext(bytes) {
        Ok(t) if !t.trim().is_empty() => t,
        Ok(_) => {
            // pdftotext가 빈 텍스트 → pdf_extract로 재시도(둘 다 비면 스캔 PDF로 간주).
            fallback_pdf_extract(bytes)?
        }
        Err(e) => {
            // pdftotext 미설치/실행 실패 → pdf_extract로 폴백.
            tracing::warn!("pdftotext 사용 불가({e}), pdf_extract로 폴백합니다.");
            fallback_pdf_extract(bytes)?
        }
    };
    let cleaned = text.trim();
    if cleaned.is_empty() {
        return Err(anyhow!(
            "PDF에서 텍스트를 찾지 못했습니다(스캔 이미지 PDF일 수 있어요). 텍스트를 직접 붙여넣어 주세요."
        ));
    }
    Ok(truncate_chars(cleaned, MAX_CHARS))
}

/// 순수 Rust PDF 파서 폴백.
fn fallback_pdf_extract(bytes: &[u8]) -> Result<String> {
    pdf_extract::extract_text_from_mem(bytes).map_err(|e| anyhow!("PDF 텍스트 추출 실패: {e}"))
}

/// poppler `pdftotext`로 PDF 텍스트를 추출한다(stdin → stdout, UTF-8).
/// 바이너리가 없으면 `spawn`이 에러를 돌려주므로 호출부에서 폴백한다.
/// stdout이 파이프 버퍼를 채워 교착되지 않도록 stdin 쓰기는 별도 스레드에서 처리.
fn pdftotext(bytes: &[u8]) -> Result<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("pdftotext")
        .args(["-q", "-enc", "UTF-8", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("pdftotext 실행 불가: {e}"))?;

    let mut stdin = child.stdin.take().expect("stdin piped");
    let buf = bytes.to_vec();
    let writer = std::thread::spawn(move || {
        // 실패(예: pdftotext가 먼저 종료해 파이프가 닫힘)는 무시 — 상태코드로 판정.
        let _ = stdin.write_all(&buf);
        // stdin이 여기서 드롭되며 EOF를 알린다.
    });

    let out = child
        .wait_with_output()
        .map_err(|e| anyhow!("pdftotext 대기 실패: {e}"))?;
    let _ = writer.join();

    if !out.status.success() {
        return Err(anyhow!("pdftotext 비정상 종료: {:?}", out.status.code()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// HTML에서 제목 + 본문 문단을 뽑아 하나의 평문으로 만든다.
fn html_to_article(html: &str) -> String {
    let doc = Html::parse_document(html);
    let mut out = String::new();

    if let Ok(sel) = Selector::parse("title") {
        if let Some(t) = doc.select(&sel).next() {
            let title = collapse_ws(&t.text().collect::<String>());
            if !title.is_empty() {
                out.push_str(&title);
                out.push_str("\n\n");
            }
        }
    }

    // 본문 후보: 먼저 article/main 안쪽 문단만, 없으면 전체 <p>로 폴백.
    let mut paras = paragraphs(&doc, "article p, main p");
    if paras.is_empty() {
        paras = paragraphs(&doc, "p");
    }
    out.push_str(&paras.join("\n\n"));

    truncate_chars(out.trim(), MAX_CHARS)
}

/// 선택자에 걸린 문단들의 텍스트(짧은 잡음은 버림).
fn paragraphs(doc: &Html, selector: &str) -> Vec<String> {
    let Ok(sel) = Selector::parse(selector) else {
        return Vec::new();
    };
    doc.select(&sel)
        .map(|e| collapse_ws(&e.text().collect::<String>()))
        .filter(|s| s.chars().count() > 40)
        .collect()
}

/// 연속 공백/개행을 단일 공백으로 접는다.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// char 경계를 지키며 최대 길이로 자른다.
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s.to_string(),
    }
}
