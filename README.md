# Vocabulary Notebook (MVP 골격)

기사·책·논문을 붙여넣으면 모르는 단어와 베스트 문장을 추출·누적하는 개인 학습용 웹앱.
스택: Rust(axum) + CoreDB(Cassandra 스타일 NoSQL, HTTP `/query` API) + Claude API.

## 구성

```
src/
  main.rs      # axum 서버, 라우트
  db.rs        # CoreDB HTTP /query API 연결 + 스키마 부트스트랩 + 쿼리
  extract.rs   # Claude API 호출로 단어/문장 추출
  models.rs    # 카테고리·데이터 구조
static/
  index.html   # 붙여넣기 폼
```

## 사전 준비

1. **CoreDB 실행** (https://github.com/yhc007/coredb) — HTTP `/query` 서버를 띄운다.
   바이너리는 **실행할 머신의 아키텍처로 직접 빌드**한다(미리 받은 바이너리가 OS/arch가
   다르면 "Exec format error"). `--data-dir`/`--commitlog-dir`는 서브커맨드 `start` *앞*에 온다.
   ```
   git clone https://github.com/yhc007/coredb && cd coredb
   cargo run -- --data-dir ./data --commitlog-dir ./commitlog start --host 127.0.0.1 --port 9142
   # 확인: curl -s localhost:9142/stats
   ```
2. **환경변수** — `.env.example`를 복사해 채운다.
   ```
   export COREDB_NODE=127.0.0.1:9142          # CoreDB HTTP /query 엔드포인트
   export ANTHROPIC_API_KEY=sk-ant-...        # 실제 API 키(sk-ant-api…). OAuth 토큰 아님
   export ANTHROPIC_MODEL=claude-sonnet-4-6
   export BIND_ADDR=0.0.0.0:8080
   # 로컬에서 Google 로그인 없이 돌리려면 인증 게이트를 끈다:
   export AUTH_DISABLED=1
   ```
   인증을 켜려면 `GOOGLE_CLIENT_ID`/`GOOGLE_CLIENT_SECRET`/`OAUTH_REDIRECT_URL`과
   화이트리스트(`ALLOWED_EMAIL` 또는 `ALLOWED_HD`), 세션 키(`SESSION_SECRET`)를 채운다.
   자세한 항목은 `.env.example` 참고.

## 실행

```
cargo run
# http://localhost:8080 접속 → 본문 붙여넣기 → /words 에서 누적 확인
```

## 현재 범위 (MVP)

- [x] CoreDB 연결 + `vocab` 키스페이스/테이블 부트스트랩
- [x] 붙여넣기 → 원문 저장 → Claude 추출 → 단어/문장 저장
- [x] 카테고리별 단어 목록 조회
- [x] 단어 '안다' 표시 → known_words 등록(다음 추출에서 제외)

## 다음 단계 (스펙 문서 참조)

- [x] Google OAuth 로그인 게이트 + 이메일 화이트리스트 (스펙 5번) — `src/auth.rs`
- [x] 베스트 문장 조회 UI(`/sentences`) / 카테고리 필터 + '안다' 버튼
- [x] 복습 모드(`/review`) — 미숙지 단어 플래시카드(셔플·뜻 보기·알아요/또 볼래요)
- [ ] VM 배포(systemd + 리버스 프록시) + CoreDB 백업 cron (스펙 7번)
- [ ] CSV/Anki 내보내기

## 주의

- **통신 방식**: 앱은 CoreDB의 **HTTP `/query` JSON API**로 접속한다(scylla 네이티브
  프로토콜 아님). CoreDB의 Native Protocol(9042) 구현은 scylla 드라이버와 DML 결과 프레임이
  호환되지 않아(SELECT/INSERT 응답 파싱 실패) HTTP로 전환했다. `db.rs` 참고.
- **CoreDB CQL 방언** (제한된 CQL): 부트스트랩 스키마는 이에 맞춰져 있다 —
  `CREATE KEYSPACE`는 `WITH REPLICATION` 필수, `CREATE TABLE`은 `WITH CLUSTERING ORDER BY`
  등 `WITH` 절 미지원, `CREATE INDEX`는 `IF NOT EXISTS` 미지원. 최초 1회는 부트스트랩 로그를
  확인할 것(이미 존재 에러는 무시하도록 처리됨).
- **문자열 처리**: HTTP `/query`는 바인드 파라미터가 없어 CQL에 값을 인라인한다. CoreDB는
  표준 `''` 이스케이프를 해제하지 않고 raw `'`는 파싱을 깨뜨리므로, `db.rs`가 텍스트의 작은
  따옴표를 타이포그래픽 따옴표(’, U+2019)로 치환한다.
- **API 키**: `ANTHROPIC_API_KEY`는 콘솔에서 발급한 실제 키(`sk-ant-api…`)여야 한다.
  Claude Code의 OAuth 토큰(`sk-ant-oat…`)은 앱의 `x-api-key` 헤더로 인증되지 않는다.
