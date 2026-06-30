# Vocabulary Notebook (MVP 골격)

기사·책·논문을 붙여넣으면 모르는 단어와 베스트 문장을 추출·누적하는 개인 학습용 웹앱.
스택: Rust(axum) + CoreDB(Cassandra 스타일 NoSQL) + Claude API.

## 구성

```
src/
  main.rs      # axum 서버, 라우트
  db.rs        # CoreDB(scylla 드라이버) 연결 + 스키마 부트스트랩 + 쿼리
  extract.rs   # Claude API 호출로 단어/문장 추출
  models.rs    # 카테고리·데이터 구조
static/
  index.html   # 붙여넣기 폼
```

## 사전 준비

1. **CoreDB 실행** (https://github.com/yhc007/coredb)
   ```
   git clone https://github.com/yhc007/coredb && cd coredb
   cargo run -- start --host 127.0.0.1 --port 9042
   ```
2. **환경변수** — `.env.example`를 복사해 채운다.
   ```
   export COREDB_NODE=127.0.0.1:9042
   export ANTHROPIC_API_KEY=sk-ant-...
   export ANTHROPIC_MODEL=claude-sonnet-4-6
   export BIND_ADDR=0.0.0.0:8080
   ```

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

- [ ] Google OAuth 로그인 게이트 + 이메일 화이트리스트 (스펙 5번)
- [ ] 베스트 문장 조회 UI / 카테고리 필터 UI 다듬기
- [ ] VM 배포(systemd + 리버스 프록시) + CoreDB 백업 cron (스펙 7번)
- [ ] 복습 모드, CSV/Anki 내보내기

## 주의

- CoreDB는 단일 노드·제한된 CQL이며 "프로덕션 전 추가 테스트 필요" 상태다.
  `CREATE INDEX`/`IF NOT EXISTS` 등 일부 구문은 CoreDB 버전에 따라 조정이 필요할 수 있으니,
  최초 1회는 `cargo run` 부트스트랩 로그를 확인할 것.
- scylla 드라이버 버전(0.13)의 API는 CoreDB의 Native Protocol v4 구현과 맞물려 동작을
  검증해야 한다. 연결이 안 되면 `db.rs`의 쿼리를 HTTP API(`POST /query`)로 대체 가능.
