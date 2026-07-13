# 베스트 문장 → "문법 강의 시작" 그래프 분석 계획

> 이 문서는 리눅스 Claude Code 세션이 단독으로 이어받아 구현하기 위한 개발 브리프다.
> 아직 **코드는 미적용**, 설계/계약/단계만 확정한 상태다. `docs/extraction-cot-plan.md`와
> 같은 형식(배경 → 현재 구조 → 단계별 스케치 → 검증/Acceptance)을 따른다.

## 배경 / 목표

이 앱은 본문에서 **베스트 문장**(`Sentence{text, reason}`)을 골라 `/sentences`에 모은다.
지금 문장은 `reason`(왜 학습가치가 있는지) 한 줄만 붙는다. 목표는 각 베스트 문장을
**그래프 기반 문장 해석(구문 구조 시각화)** 으로 풀어, 그 그래프가 **문법 강의의 도입부**가
되도록 하는 것이다. 즉 문장 하나를 "구조 그림 + 이 문장에서 배울 문법 포인트"로 열어,
학습자가 문장을 문법 렌즈로 다시 보게 한다.

핵심 아이디어:

- **문장 → 의존/구 구조 그래프**: 노드(단어·구, 문법 역할·품사) + 엣지(주어→동사, 수식,
  종속절 연결 등 문법 관계)로 문장을 분해.
- **그래프 = 강의의 시작점**: 그래프 위에 "이 문장의 핵심 문법 포인트 2~3개"를 얹어
  강의 도입부로 만든다(추후 포인트별 상세 설명으로 확장 가능 — 단계 4 옵션).

## 설계 원칙 (기존 코드와 정합)

1. **파서 의존성 없이 LLM이 그래프를 생성**한다. 이 앱은 순수 Rust + Claude 호출이고
   spaCy/CoreNLP 같은 구문분석기가 없다. 따라서 그래프는 Claude가 **엄격한 JSON 스키마**로
   반환(노드/엣지/포인트). `extract_json_block` 파서·`Extraction` 계약 관례를 그대로 따른다.
2. **온디맨드 + 캐시** 패턴을 재사용한다(`word_roots`/`entry_mindmap`와 동일). 추출 흐름
   (`create_entry`)은 **건드리지 않는다** — 비용/지연 증가 없음. 문장을 처음 펼칠 때만
   문장당 Claude 1회 호출하고 결과 JSON을 캐시한다.
3. **그래프 시각화는 `MINDMAP_JS`(SVG)** 를 포크한다. 마인드맵은 이미 중심노드↔가지카드를
   SVG 베지어 `<path>`로 잇고 리사이즈 시 다시 그린다. 문장은 토큰을 가로로 배치하고
   그 위로 **아크(arc) 다이어그램**(head→dependent 곡선 + 관계 라벨)을 그리는 게
   "문법 강의" 시각으로 가장 자연스럽다.
4. **JSON 인용부호 가드**를 반드시 적용한다. 문장 텍스트/강의 문구를 되뱉기 때문에,
   최근 `summarize_korean` 파싱 버그(값 안의 raw `"`)와 동일 위험이 있다. 프롬프트에서
   문자열 안 인용은 `「 」`로 감싸고 raw `"`를 넣지 말도록 못박는다.

## 현재 구조에서 재사용할 선례

- **온디맨드 분석 라우트** — `word_roots`(`src/main.rs`):
  `GET /words/roots?term=` → `db.get_word_roots(term)` 캐시 조회 → 없으면
  `extractor.analyze_roots(term)` → `db.save_word_roots(term, json)` → JSON 반환.
  클라이언트 `ROOTS_JS`가 버튼 클릭/자동으로 fetch해 카드에 렌더.
- **단일 PK 캐시 테이블** — `vocab.word_roots(term PK, analysis text, created_at)`,
  `vocab.entry_mindmap(entry_id PK, mindmap text, created_at)`. INSERT가 사실상 upsert.
  (참고: 복합 PK 테이블은 CoreDB DELETE가 안 먹지만, **단일 PK 캐시 테이블은 덮어쓰기
  정상**이므로 재생성/갱신에 안전하다.)
- **그래프 SVG** — `MINDMAP_JS`: `createElementNS`로 `<svg>`/`<path>` 생성, 노드
  `getBoundingClientRect()`로 좌표 계산, 팔레트 색, resize 디바운스 재렌더.

---

## 단계 0 — 데이터/계약 설계

### models.rs (새 타입, 모두 `#[serde(default)]`로 회복탄력성 확보)

```rust
/// 문법 그래프 노드: 문장 속 토큰/구.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GramNode {
    pub id: String,             // "n1" 등 엣지 참조용 안정 id
    pub text: String,           // 표시할 단어/구(원문 조각)
    #[serde(default)]
    pub role: String,           // 주어/술어/목적어/수식어/접속 등 한국어 역할
    #[serde(default)]
    pub pos: String,            // 품사(명사구/동사/전치사구 등), 선택
}

/// 문법 그래프 엣지: head→dependent 관계.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GramEdge {
    pub from: String,           // head node id
    pub to: String,             // dependent node id
    #[serde(default)]
    pub label: String,          // 관계명(주어, 목적어, 수식, 종속절 등)
}

/// 문장 문법 분석 = 강의 도입부 + 구조 그래프.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentenceGrammar {
    #[serde(default)]
    pub summary: String,        // 문장 구조 한 줄 요약(주절/종속절 등)
    #[serde(default)]
    pub nodes: Vec<GramNode>,
    #[serde(default)]
    pub edges: Vec<GramEdge>,
    #[serde(default)]
    pub points: Vec<String>,    // "이 문장에서 배울 문법 포인트" 2~3개(강의 시작)
}
```

### db.rs (bootstrap에 캐시 테이블 + get/save)

```sql
CREATE TABLE IF NOT EXISTS vocab.sentence_grammar
  (sentence text PRIMARY KEY, analysis text, created_at timestamp)
```

- 키는 **문장 텍스트(trim)** — `word_roots`가 `term`을 키로 쓰는 것과 동일한 관례.
  (문장이 길어 PK 길이가 부담되면 대안으로 텍스트 해시를 PK로. 단, 일관성상 텍스트 권장.)
- `get_sentence_grammar(&self, text: &str) -> Result<Option<String>>` /
  `save_sentence_grammar(&self, text: &str, analysis_json: &str) -> Result<()>` —
  `get_word_roots`/`save_word_roots`를 그대로 복제하고 컬럼명만 교체. 값은 `cql_str`로
  인라인(따옴표/`'` 처리 포함).

---

## 단계 1 — Extractor::analyze_grammar (Claude 호출 + JSON 계약)

`analyze_roots()`와 같은 구조로 `src/extract.rs`에 추가:

```rust
/// 베스트 문장을 문법 강의 도입용 그래프로 분해한다.
/// 노드(토큰/구·역할·품사) + 엣지(문법 관계) + 강의 포인트를 JSON으로 받는다.
pub async fn analyze_grammar(&self, sentence: &str) -> Result<SentenceGrammar> {
    let prompt = format!(
        "다음 영어 문장을 '문법 강의의 도입부'로 삼을 수 있게 구조 그래프로 분해하라.\n\
         - nodes: 문장을 의미 단위(단어/구)로 나눈 노드. 각 node는 id(\"n1\"..), \
           text(원문 조각 그대로), role(한국어 문법 역할: 주어/술어/목적어/보어/수식어/\
           접속 등), pos(품사나 구 유형: 명사구/동사/전치사구 등).\n\
         - edges: head→dependent 문법 관계. from/to는 node id, label은 관계명(주어·목적어·\
           수식·종속절·병렬 등 한국어).\n\
         - summary: 이 문장의 구조를 한 줄로(주절/종속절, 핵심 구문).\n\
         - points: 이 문장으로 가르칠 핵심 문법 포인트 2~3개(한국어, 강의 시작용).\n\
         - 인용이 필요하면 문자열 안에서 큰따옴표(\") 대신 「 」를 쓰고, 값 안에 \
           이스케이프되지 않은 큰따옴표를 절대 넣지 말 것(JSON 파싱 깨짐 방지).\n\
         - 반드시 아래 JSON 스키마로만 응답:\n\
         {{\"summary\":\"\",\"nodes\":[{{\"id\":\"\",\"text\":\"\",\"role\":\"\",\"pos\":\"\"}}],\
         \"edges\":[{{\"from\":\"\",\"to\":\"\",\"label\":\"\"}}],\"points\":[\"\"]}}\n\n\
         === 문장 ===\n{sentence}",
    );
    let content = self.message(&prompt, 1536).await?;
    let json_str = extract_json_block(&content);
    serde_json::from_str(&json_str)
        .map_err(|e| anyhow!("failed to parse grammar JSON: {e}; raw: {content}"))
}
```

**주의:** `Word`/`Sentence`처럼 스키마 문자열과 struct 필드는 lockstep. 필드 바꾸면 둘 다.

---

## 단계 2 — 라우트 `/sentences/grammar` (캐시 조회 → 생성 → 저장)

`word_roots` 핸들러를 그대로 미러(`src/main.rs`):

```rust
// main()
.route("/sentences/grammar", get(sentence_grammar))

async fn sentence_grammar(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let text = q.get("text").cloned().unwrap_or_default();
    if let Some(cached) = st.db.get_sentence_grammar(&text).await.map_err(AppError::from)? {
        return Ok(Json(serde_json::from_str(&cached).unwrap_or(json!({}))));
    }
    let analysis = st.extractor.analyze_grammar(&text).await.map_err(AppError::from)?;
    let body = serde_json::to_string(&analysis).unwrap_or_default();
    st.db.save_sentence_grammar(&text, &body).await.map_err(AppError::from)?;
    Ok(Json(serde_json::from_str(&body).unwrap_or(json!({}))))
}
```

- 키를 `text`로 넘기므로 `/sentences` 리스트 렌더에서 문장 텍스트를 그대로 쿼리에 실으면
  된다(별도 id 노출 불필요, `sentences` 복합 PK 안 건드림).

---

## 단계 3 — `/sentences` 렌더링 + 버튼 + `SENTENCE_GRAPH_JS`

- `list_sentences`가 그리는 각 문장 카드에 **"🔍 문법 분석" 버튼**(어근 버튼 `roots-btn`
  스타일 재사용)과 결과 컨테이너 `<div class="gram" data-text="…"></div>`를 추가.
- `SENTENCE_GRAPH_JS`: `MINDMAP_JS`를 포크.
  1. 버튼 클릭 → `fetch('/sentences/grammar?text='+encodeURIComponent(text))`.
  2. **아크 다이어그램 렌더**: nodes를 문장 순서대로 가로 한 줄에 칩으로 배치
     (각 칩에 text + 작은 role 라벨), 그 위에 edges를 SVG 베지어 아크로 그리고 중앙에
     label. head/dependent 좌표는 `MINDMAP_JS`처럼 `getBoundingClientRect()`로 계산,
     resize 디바운스 재렌더도 동일.
  3. 그래프 아래 `summary`(구조 요약)와 `points`(강의 포인트 리스트)를 카드로.
- CSS는 `.roots`/`.mm-*` 톤을 재사용해 `.gram`/`.gram-node`/`.gram-svg` 추가.

레이아웃 노트: 문장이 길면 토큰이 한 줄을 넘길 수 있다. v1은 가로 스크롤
(`overflow-x:auto`, `.mindmap-card`가 이미 쓰는 방식)로 처리하고, 줄바꿈 대응은 후속.

---

## 단계 4 (옵션) — "강의"로 확장

v1은 그래프 + 포인트(도입부)까지. 필요 시:
- 각 `point`에 "자세히" → 포인트별 상세 설명을 추가 Claude 호출로 지연 로드(캐시).
- 예문 변형(같은 문법 구조의 다른 예문 생성)으로 연습 제공.
- `/review` 덱처럼 문장 문법 카드 모드.

비용이 늘므로 기본 off, 요청 시. (extraction CoT 계획의 단계 2와 같은 정책.)

---

## 검증 (리눅스에서)

### 전제
- rustc 1.88+ (기존과 동일). 앱 실행에 live CoreDB + `ANTHROPIC_API_KEY`.
  로컬은 `AUTH_DISABLED=1`, 포트 8137(기존 로컬 관례).
- 그래프 품질을 보려면 `ANTHROPIC_MODEL=claude-opus-4-8`로 실험.

### 절차
1. `cargo check` / `cargo clippy` 통과.
2. `cargo test` — 기존 `split_chunks` 회귀 없음(새 순수 코드라 영향 없음).
3. 라이브: 종속절/수식이 있는 문장으로 `/sentences/grammar?text=` 호출 →
   - JSON이 스키마대로 오는가(nodes/edges/points), **인용 있는 문장에서도 파싱 성공**?
   - `/sentences`에서 버튼 클릭 시 아크 그래프가 그려지고 role/label이 한국어로 붙나?
   - 두 번째 호출은 캐시로 즉시 반환되나(같은 text)?

### 완료 기준 (Acceptance)
- [ ] `cargo check`/`clippy`/`test` 통과, 기존 추출 흐름 무변경
- [ ] 새 캐시 테이블 bootstrap idempotent(already-exists 무시), 단일 PK 덮어쓰기 동작
- [ ] 인용부호 포함 문장에서 grammar JSON 파싱 성공(「 」 가드 유효)
- [ ] `/sentences`에서 문장별 그래프 + 강의 포인트가 렌더되고, 재조회는 캐시 히트
- [ ] (옵션) 단계 4는 기본 off

---

## 범위 밖 / 주의
- `SentenceGrammar` 등 필드 변경 시 **프롬프트 스키마 문자열도 lockstep**(추출 계약 관례).
- 그래프는 LLM 추정이라 100% 구문론적 정확성을 보장하지 않는다. "학습 도입"이 목적이며,
  틀린 관계가 보이면 role/label을 사람이 정정할 여지를 UI에 남길 수 있다(후속).
- CoreDB 제약(HTTP `/query`, 바인드 파라미터 없음, 복합 PK DELETE 미동작)은 캐시가
  **단일 PK**라 무관하지만, 값 인라인 시 `cql_str`로 따옴표/`'` 처리 필수.
- 프롬프트·주석·UI 문구는 프로젝트 관례상 **한국어** 유지.
