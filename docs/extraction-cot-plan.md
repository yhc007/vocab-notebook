# 단어/문장 추출 품질 개선 계획 (CoT 기반 프롬프팅)

> 이 문서는 리눅스의 Claude Code 세션이 단독으로 이어받아 실행하기 위한 개발 브리프다.
> Windows 머신에서 초안을 잡았고, **단계 1은 이미 `src/extract.rs`에 적용된 상태**이지만
> 그 변경이 이 저장소에 커밋/푸시되어 있지 않다면 아래 "단계 1" 절의 원문으로 재현하라.

## 배경 / 목표

이 앱은 영어 본문에서 학습 가치 있는 단어·문장을 골라 **한국어로 해설**한다
(`Word.definition`, `Word.example`, `Sentence.reason`). 즉 사실상 "문맥 속 해석" 과제다.

개선의 근거는 COLING 2025 논문 **"A Testset for Context-Aware LLM Translation in
Korean-to-English Discourse-Level Translation"** (https://aclanthology.org/2025.coling-main.110/).
과제 방향(한→영 번역)은 다르지만, 이 프로젝트로 이전 가능한 두 결론을 채택한다:

1. **단계적 추론(Chain-of-Thought)이 zero-shot을 유의미하게 능가한다.**
2. 번역을 **"현상 탐지 → 전략 → 후보 → 선택"**으로 분해한 CAP가 end-to-end보다 낫다.

원본 CAP는 단계마다 별도 호출이라 비용이 ~4×로 커진다(논문도 한계로 지적). 개인용 앱이므로
**핵심인 "문맥 추론을 프롬프트에 내재화"**만 가져오는 경량 설계를 채택한다.

## 현재 구조의 약점 (`src/extract.rs`)

`extract()`의 프롬프트가 단일 zero-shot이라 다음을 손해 본다:

1. **다의어 미해소** — `definition`이 사전 대표뜻으로 나오기 쉬움.
2. **관용구/구동사 누락** — 단어 단위로만 뽑아 `take on`, `by and large` 같은 구 표현이 빠짐.
3. **선정 근거 얕음** — `reason`이 "인상적이라서" 수준.

참고: 긴 본문은 `extract_chunked()`가 청크로 나눠 **병렬(CONCURRENCY=4)** 호출하고 결과를
병합(단어는 소문자 term 기준, 문장은 text 기준 중복 제거)한다. 단계를 늘려도 **글 전체가 아니라
청크당** 추가 호출이면 감당 가능하다는 뜻.

---

## 단계 1 — CoT를 프롬프트에 내장 (이미 적용, 비용 변화 없음) ✅

**변경 위치:** `src/extract.rs`의 `Extractor::extract()` 안 `let prompt = format!( ... )`.
출력은 여전히 JSON만 나오게 못박아 **스키마·`extract_json_block` 파서·`extract_chunked`
병합 로직은 그대로**다. 호출 수 불변, 토큰만 소폭 증가.

적용되어 있어야 할 프롬프트 원문(없으면 이대로 교체):

```rust
// 문맥 의존 해석(다의어·구 표현·비유)을 놓치지 않도록, 출력 전에
// 아래 항목을 "내부적으로" 판단하게 지시한다(단계적 추론을 프롬프트에 내장).
// 출력은 여전히 JSON만 — 스키마와 파서(extract_json_block)는 그대로다.
let prompt = format!(
    "다음 글에서 학습 가치가 높은(난이도 있는) 단어/표현과 인상적인 문장을 골라줘.\n\
     - 이미 아는 단어 목록에 있는 단어는 제외: {known}\n\n\
     먼저(출력하지 말고 속으로) 각 후보에 대해 다음을 판단하라:\n\
     1) 이 문맥에서의 실제 의미가 사전 대표뜻과 다른가? → definition은 반드시 \
        '이 문맥에서의 뜻'으로 (대표뜻이 아니라).\n\
     2) 단어가 아니라 구 단위 표현인가(관용구·구동사·연어, 예: take on, by and large)? \
        → 그렇다면 구 전체를 term으로.\n\
     3) 비유·완곡·전문용어라 직역이 오해를 부르는가? → definition에서 풀어서 설명.\n\
     그다음 아래 형식으로만 응답:\n\
     - 각 단어/표현: term(원형 또는 구 전체), definition(위 판단을 반영한 한국어 뜻), \
       example(글 속 실제 예문)\n\
     - 베스트 문장: text(원문 그대로)와 reason(왜 학습가치가 있는지: 구조·표현·통찰 관점에서 구체적으로)\n\
     - 반드시 아래 JSON 스키마로만 응답(추론 과정은 출력하지 말 것):\n\
     {{\"words\":[{{\"term\":\"\",\"definition\":\"\",\"example\":\"\"}}],\
     \"sentences\":[{{\"text\":\"\",\"reason\":\"\"}}]}}\n\n\
     === 본문 ===\n{body}",
    known = known_list,
    body = text
);
```

**주의:** `format!` 안이라 리터럴 `{`/`}`는 `{{`/`}}`로, 자리표시자는 `{known}`/`{body}`로
유지해야 한다.

---

## 단계 2 — 선택적 정제 패스 (품질 더 필요할 때, 청크당 +1 호출)

1차 추출된 단어 리스트에 대해서만 "각 단어의 문맥 의미를 검증/교정"하는 2차 호출을 추가.
`analyze_roots()`가 이미 단어 단위 후속 호출 패턴을 쓰므로 같은 구조로 붙인다.

구현 스케치:

- `Extractor`에 `refine_definitions(&self, words: &[Word], context: &str) -> Result<Vec<Word>>`
  같은 메서드 추가. 입력: 1차 단어들 + 그 청크 본문. 출력: definition이 교정된 단어들.
- 프롬프트: "다음 단어들의 definition이 주어진 문맥에서 정확한지 검토하고, 부정확하면
  이 문맥에 맞게 고쳐라. term/example은 유지. JSON 배열로만 응답."
- `extract_chunked()`의 각 청크 파이프라인에서 `extract()` 뒤에 선택적으로 호출.
  비용을 고려해 **기본은 off**, 환경변수(예: `EXTRACT_REFINE=1`)나 설정으로 켜는 것을 권장.

이 단계는 정확도를 더 끌어올리지만 비용이 청크당 2배가 되므로, 단계 1의 효과를 먼저
확인한 뒤 필요할 때만 진행.

---

## 검증 (리눅스에서)

### 빌드 / 실행 전제
- **rustc 1.88 이상** 필요. 커밋된 `Cargo.lock`이 `time 0.3.53`(rustc 1.88 요구)으로
  고정돼 있어, 1.86 등 구버전에서는 `cargo check`가 MSRV 에러로 실패한다.
  `rustup update stable`로 올릴 것.
- 앱 실행에는 **live CoreDB**(HTTP `/query`)와 `ANTHROPIC_API_KEY`가 필요
  (자세한 건 `CLAUDE.md`, `.env.example`). 로컬 로그인 우회는 `AUTH_DISABLED=1`.
- `ANTHROPIC_MODEL`은 env로 조절 가능(기본 `claude-sonnet-4-6`). 추출 품질을 보려면
  `claude-opus-4-8` 등 상위 모델로 실험 가능.

### 절차
1. `cargo check` — 컴파일 통과 확인(순수 프롬프트 변경이라 통과해야 정상).
2. `cargo test` — `split_chunks` 유닛테스트 회귀 없음 확인.
3. 짧은 영어 문단(다의어·구동사·비유가 섞인 것)으로 **수정 전/후 추출 비교**:
   - 다의어 단어의 `definition`이 문맥 뜻으로 나오는가?
   - 구 표현(예: phrasal verb)이 `term`으로 잡히는가?
   - `reason`이 구체적인가?

### 완료 기준 (Acceptance)
- [ ] `cargo check` / `cargo test` 통과
- [ ] JSON 스키마·파서 변경 없음(회귀 없음)
- [ ] 샘플 문단에서 다의어 definition이 문맥 반영, 구 표현 1개 이상 포착 확인
- [ ] (선택) 단계 2를 켰을 때만 추가 호출이 발생하고 기본값은 off

---

## 범위 밖 / 주의
- `Word`/`Sentence`/`Extraction` 필드를 바꾸면 **프롬프트 스키마 문자열도 lockstep으로**
  고쳐야 파싱이 깨지지 않는다(`CLAUDE.md`의 추출 계약 참고).
- CoreDB CQL 방언 제약(HTTP `/query`, 바인드 파라미터 없음)은 이 작업과 무관하지만
  DB 근처를 건드릴 일이 생기면 `CLAUDE.md`의 데이터 모델 노트를 먼저 볼 것.
- 프로젝트 관례상 프롬프트·주석은 **한국어**로 유지.
