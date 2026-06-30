//! Google OAuth2 로그인 게이트 + 이메일 화이트리스트 (스펙 5번).
//!
//! Authorization Code 흐름을 reqwest로 직접 구현한다(별도 oauth 크레이트 없음):
//!   `/auth/login`    → CSRF state 쿠키 설정 후 Google 동의 화면으로 리다이렉트
//!   `/auth/callback` → state 검증 → code를 토큰으로 교환 → userinfo 조회 →
//!                      화이트리스트 통과 시 암호화 세션 쿠키 발급
//!   `/auth/logout`   → 세션 쿠키 제거
//!
//! 로그인 상태는 `PrivateCookieJar`(암호화 쿠키)에 이메일로 보관한다(서버측 세션
//! 저장소 없음). `require_auth` 미들웨어가 보호 라우트를 게이트한다.
//!
//! 로컬/개발에서 Google 자격증명 없이 돌리려면 `AUTH_DISABLED=1`로 게이트를 끈다.

use std::collections::HashMap;
use std::env;
use std::sync::Arc;

use axum::{
    extract::{Query, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::{Cookie, Key, PrivateCookieJar, SameSite};
use sha2::{Digest, Sha512};
use uuid::Uuid;

use crate::AppState;

const SESSION_COOKIE: &str = "session";
const STATE_COOKIE: &str = "oauth_state";

/// OAuth 설정 + 화이트리스트. 환경변수에서 1회 로드한다.
pub struct OAuthConfig {
    pub enabled: bool,
    client_id: String,
    client_secret: String,
    redirect_url: String,
    allowed_emails: Vec<String>,
    allowed_hd: Option<String>,
    /// HTTPS 리다이렉트일 때만 Secure 쿠키(로컬 http 테스트에선 false).
    secure_cookies: bool,
}

impl OAuthConfig {
    pub fn from_env() -> Arc<Self> {
        let client_id = env::var("GOOGLE_CLIENT_ID").unwrap_or_default();
        let client_secret = env::var("GOOGLE_CLIENT_SECRET").unwrap_or_default();
        let redirect_url = env::var("OAUTH_REDIRECT_URL").unwrap_or_default();
        let allowed_emails: Vec<String> = env::var("ALLOWED_EMAIL")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let allowed_hd = env::var("ALLOWED_HD").ok().filter(|s| !s.is_empty());

        let disabled = env::var("AUTH_DISABLED")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let enabled = !disabled && !client_id.is_empty();
        let secure_cookies = redirect_url.starts_with("https://");

        if !enabled {
            tracing::warn!(
                "OAuth 게이트 비활성화: 모든 라우트가 공개됨. \
                 (GOOGLE_CLIENT_ID 설정 + AUTH_DISABLED 해제 시 활성화)"
            );
        } else if allowed_emails.is_empty() && allowed_hd.is_none() {
            tracing::warn!(
                "OAuth 활성화되었으나 ALLOWED_EMAIL/ALLOWED_HD 미설정 — 모든 로그인이 거부됨."
            );
        }

        Arc::new(Self {
            enabled,
            client_id,
            client_secret,
            redirect_url,
            allowed_emails,
            allowed_hd,
            secure_cookies,
        })
    }

    /// 이메일이 화이트리스트를 통과하는지 검사한다.
    /// 검증된(verified) 이메일이어야 하고, ALLOWED_EMAIL 정확 일치 또는
    /// ALLOWED_HD(호스티드 도메인) 일치 중 하나를 만족해야 한다.
    fn email_allowed(&self, email: &str, verified: bool, hd: Option<&str>) -> bool {
        if !verified || email.is_empty() {
            return false;
        }
        if self
            .allowed_emails
            .iter()
            .any(|e| e.eq_ignore_ascii_case(email))
        {
            return true;
        }
        matches!((&self.allowed_hd, hd), (Some(want), Some(got)) if want.eq_ignore_ascii_case(got))
    }
}

/// SESSION_SECRET을 SHA-512로 64바이트 키로 유도. 없으면 임시 키(재시작 시 세션 초기화).
pub fn cookie_key() -> Key {
    match env::var("SESSION_SECRET") {
        Ok(s) if !s.is_empty() => Key::from(Sha512::digest(s.as_bytes()).as_slice()),
        _ => {
            tracing::warn!("SESSION_SECRET 미설정 — 임시 키 사용(재시작 시 로그인 풀림).");
            Key::generate()
        }
    }
}

/// 보호 라우트 게이트. 세션 쿠키가 없으면 로그인으로 리다이렉트.
/// 게이트가 비활성화면(AUTH_DISABLED 등) 그냥 통과시킨다.
pub async fn require_auth(
    State(st): State<AppState>,
    jar: PrivateCookieJar,
    req: Request,
    next: Next,
) -> Response {
    if !st.oauth.enabled {
        return next.run(req).await;
    }
    let authed = jar
        .get(SESSION_COOKIE)
        .map(|c| !c.value().is_empty())
        .unwrap_or(false);
    if authed {
        next.run(req).await
    } else {
        Redirect::to("/auth/login").into_response()
    }
}

/// Google 동의 화면으로 리다이렉트. CSRF용 state를 쿠키에 저장한다.
pub async fn auth_login(State(st): State<AppState>, jar: PrivateCookieJar) -> impl IntoResponse {
    if !st.oauth.enabled {
        return (jar, Redirect::to("/")).into_response();
    }
    let state_token = Uuid::new_v4().to_string();
    let url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth\
         ?response_type=code&client_id={cid}&redirect_uri={redirect}\
         &scope={scope}&state={state}&access_type=online&prompt=select_account",
        cid = urlencoding::encode(&st.oauth.client_id),
        redirect = urlencoding::encode(&st.oauth.redirect_url),
        scope = urlencoding::encode("openid email"),
        state = state_token,
    );
    let jar = jar.add(make_cookie(STATE_COOKIE, state_token, st.oauth.secure_cookies));
    (jar, Redirect::to(&url)).into_response()
}

/// Google 콜백: state 검증 → 토큰 교환 → userinfo → 화이트리스트 → 세션 발급.
pub async fn auth_callback(
    State(st): State<AppState>,
    jar: PrivateCookieJar,
    Query(q): Query<HashMap<String, String>>,
) -> Result<(PrivateCookieJar, Redirect), AuthError> {
    if let Some(err) = q.get("error") {
        return Err(AuthError::Forbidden(format!("Google OAuth 거부: {err}")));
    }

    // CSRF: 콜백의 state와 쿠키의 state가 일치해야 한다.
    let got_state = q.get("state").cloned().unwrap_or_default();
    let exp_state = jar
        .get(STATE_COOKIE)
        .map(|c| c.value().to_string())
        .unwrap_or_default();
    if got_state.is_empty() || got_state != exp_state {
        return Err(AuthError::Forbidden("OAuth state 불일치(CSRF 의심)".into()));
    }

    let code = q
        .get("code")
        .filter(|c| !c.is_empty())
        .ok_or_else(|| AuthError::Bad("code 누락".into()))?;

    let http = reqwest::Client::new();

    // 1) code → access_token 교환
    let token: serde_json::Value = http
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("code", code.as_str()),
            ("client_id", st.oauth.client_id.as_str()),
            ("client_secret", st.oauth.client_secret.as_str()),
            ("redirect_uri", st.oauth.redirect_url.as_str()),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await
        .map_err(|e| AuthError::Internal(format!("토큰 요청 실패: {e}")))?
        .json()
        .await
        .map_err(|e| AuthError::Internal(format!("토큰 응답 파싱 실패: {e}")))?;

    let access = token["access_token"]
        .as_str()
        .ok_or_else(|| AuthError::Internal(format!("access_token 없음: {token}")))?;

    // 2) userinfo 조회
    let info: serde_json::Value = http
        .get("https://www.googleapis.com/oauth2/v2/userinfo")
        .bearer_auth(access)
        .send()
        .await
        .map_err(|e| AuthError::Internal(format!("userinfo 요청 실패: {e}")))?
        .json()
        .await
        .map_err(|e| AuthError::Internal(format!("userinfo 파싱 실패: {e}")))?;

    let email = info["email"].as_str().unwrap_or("");
    let verified = info["verified_email"].as_bool().unwrap_or(false);
    let hd = info["hd"].as_str();

    // 3) 화이트리스트 검사
    if !st.oauth.email_allowed(email, verified, hd) {
        return Err(AuthError::Forbidden(format!(
            "허용되지 않은 계정입니다: {}",
            if email.is_empty() { "(unknown)" } else { email }
        )));
    }

    // 4) state 쿠키 제거, 세션 쿠키 발급
    let jar = jar
        .remove(removal_cookie(STATE_COOKIE))
        .add(make_cookie(SESSION_COOKIE, email.to_string(), st.oauth.secure_cookies));
    tracing::info!("로그인 성공: {email}");
    Ok((jar, Redirect::to("/")))
}

/// 세션 쿠키 제거.
pub async fn auth_logout(jar: PrivateCookieJar) -> impl IntoResponse {
    let jar = jar.remove(removal_cookie(SESSION_COOKIE));
    (jar, Redirect::to("/auth/login"))
}

fn make_cookie(name: &'static str, value: String, secure: bool) -> Cookie<'static> {
    Cookie::build((name, value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(secure)
        .build()
}

fn removal_cookie(name: &'static str) -> Cookie<'static> {
    Cookie::build(name).path("/").build()
}

/// 인증 흐름 에러 → 적절한 상태코드 응답.
pub enum AuthError {
    Forbidden(String),
    Bad(String),
    Internal(String),
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            AuthError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            AuthError::Bad(m) => (StatusCode::BAD_REQUEST, m),
            AuthError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        tracing::warn!("auth error [{code}]: {msg}");
        (code, msg).into_response()
    }
}
