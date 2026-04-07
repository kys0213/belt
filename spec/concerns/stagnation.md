# Stagnation Detection — 반복 실행 패턴 감지 + 사고 전환

> LLM이 같은 실수를 반복하거나 A↔B 왕복하는 패턴을 감지하고, 접근법을 전환하여 재시도한다.
> "몇 번 실패했는가"가 아니라 "어떻게 실패했는가"를 보고, "다르게 시도"한다.
>
> 참고: [Ouroboros](https://github.com/kys0213/ouroboros) 프로젝트의 이중 계층 탐지 + lateral thinking을 Belt에 적용.

---

## 설계 요약

```
handler 실패
    │
    ▼
Stagnation Analyzer (항상 실행)
    │
    ├── ① 유사도 판단 (CompositeSimilarity)
    │     outputs/errors 별도 검사
    │     → 4가지 패턴 탐지
    │
    ├── ② Lateral Plan 생성 (패턴 감지 시)
    │     내장 페르소나가 대안 접근법 분석
    │     → lateral_plan 출력
    │
    └── ③ Escalation 적용 (기존 failure_count 기반)
          retry            → lateral_plan 주입하여 재시도
          retry_with_comment → lateral_plan 주입 + on_fail
          hitl             → lateral_report를 hitl_notes에 첨부
```

---

## 탐지 대상: 4가지 정체 패턴

| 패턴 | 정의 | Belt에서의 예시 |
|------|------|----------------|
| **SPINNING** | A→A→A (동일/유사 반복) | 같은 코드 생성 → 같은 컴파일 에러 반복 |
| **OSCILLATION** | A→B→A→B (교대 반복) | 리팩토링 → 원복 → 리팩토링, 설정 A↔B 왕복 |
| **NO_DRIFT** | 진행 점수 정체 | 테스트 통과율이 변하지 않음 |
| **DIMINISHING_RETURNS** | 개선폭 감소 | 매 시도마다 개선은 있으나 점점 미미 |

---

## Core Types

### StagnationPattern

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StagnationPattern {
    Spinning,
    Oscillation,
    NoDrift,
    DiminishingReturns,
}
```

### StagnationDetection

```rust
pub struct StagnationDetection {
    pub pattern: StagnationPattern,
    pub detected: bool,
    pub confidence: f64,       // 0.0 ~ 1.0
    pub evidence: serde_json::Value,
}
```

### PatternDetector trait

각 패턴 탐지기가 DB에서 자기 관심사에 맞는 데이터를 직접 조회한다. 중간 구조체(`ExecutionHistory`) 없이, 각 detector가 필요한 쿼리만 수행.

```rust
/// 정체 패턴을 탐지하는 단일 알고리즘.
/// 각 impl이 DB에서 자기 관심사에 맞는 데이터를 직접 조회한다.
#[async_trait]
pub trait PatternDetector: Send + Sync {
    fn pattern(&self) -> StagnationPattern;
    async fn detect(&self, source_id: &str, state: &str, db: &dyn Db) -> Result<StagnationDetection>;
}
```

| Detector | DB 조회 | 구현 시점 |
|----------|---------|----------|
| `SpinningDetector` | `history.summary`, `history.error` (최근 N개) | v6 |
| `OscillationDetector` | `history.summary` (최근 2N개) | v6 |
| `DriftDetector` | `history.summary` + `source_data` (목표 vs 결과) | Phase 2 |
| `DiminishingDetector` | drift score 이력 | Phase 2 |

- v6: SPINNING + OSCILLATION — 텍스트 유사도 기반, DB에 데이터 이미 있음
- Phase 2: NO_DRIFT + DIMINISHING — drift score 산출 파이프라인 구축 후 detector impl 추가, 코어 변경 0

---

## Similarity — Composite Pattern

유사도 판단을 단일 trait으로 추상화하고, Composite Pattern으로 여러 알고리즘을 가중 합산한다. belt-core는 `SimilarityJudge` trait 하나만 의존한다.

### trait 정의

```rust
/// 두 텍스트의 유사도를 판단하는 단일 알고리즘
pub trait SimilarityJudge: Send + Sync {
    fn name(&self) -> &str;
    fn score(&self, a: &str, b: &str) -> f64;  // 0.0 (다름) ~ 1.0 (동일)
}
```

### CompositeSimilarity

Composite 자체도 `SimilarityJudge`를 구현한다. leaf와 composite가 같은 인터페이스이므로 중첩 가능.

```rust
pub struct CompositeSimilarity {
    judges: Vec<(Box<dyn SimilarityJudge>, f64)>,  // (judge, weight)
}

impl SimilarityJudge for CompositeSimilarity {
    fn name(&self) -> &str { "composite" }

    fn score(&self, a: &str, b: &str) -> f64 {
        let (sum, w_sum) = self.judges.iter()
            .map(|(j, w)| (j.score(a, b) * w, w))
            .fold((0.0, 0.0), |(s, ws), (v, w)| (s + v, ws + w));
        sum / w_sum
    }
}
```

### 내장 Judge 구현체

| Judge | 원리 | 출력 | 용도 |
|-------|------|------|------|
| **ExactHash** | SHA-256 해시 비교 | 동일=1.0, 다름=0.0 | 완전 동일 감지 (빠름) |
| **TokenFingerprint** | 숫자/경로/해시값 정규화 후 해시 | 구조동일=1.0, 다름=0.0 | "line 42" vs "line 58" 같은 차이 무시 |
| **NCD** | Normalized Compression Distance | 0.0~1.0 연속값 | 구조적 유사도 측정 |

#### ExactHash

```rust
impl SimilarityJudge for ExactHash {
    fn score(&self, a: &str, b: &str) -> f64 {
        if sha256(a) == sha256(b) { 1.0 } else { 0.0 }
    }
}
```

#### TokenFingerprint

변하는 부분을 정규화한 뒤 해시 비교:

```
정규화 규칙:
  숫자     → <N>       "line 42" → "line <N>"
  파일경로 → <PATH>    "/tmp/abc123/foo.rs" → "<PATH>"
  해시값   → <HASH>    "0x7f3a2b" → "<HASH>"
  UUID     → <UUID>    "550e8400-..." → "<UUID>"

예시:
  "error[E0433]: not found in auth::middleware (line 42)" 
  "error[E0433]: not found in auth::middleware (line 58)"
  → 정규화: "error[E<N>]: not found in auth::middleware (line <N>)"
  → 해시 동일 → score = 1.0
```

#### NCD (Normalized Compression Distance)

```rust
impl SimilarityJudge for Ncd {
    fn score(&self, a: &str, b: &str) -> f64 {
        let ca = compress(a).len() as f64;
        let cb = compress(b).len() as f64;
        let cab = compress(&format!("{a}{b}")).len() as f64;
        let ncd = (cab - ca.min(cb)) / ca.max(cb);
        1.0 - ncd  // NCD를 유사도로 변환
    }
}
```

- 외부 의존성 없음 (flate2 crate의 deflate 사용)
- 언어 무관, 구현 단순

### 구성 예시

```
기본 프리셋:
┌─ CompositeSimilarity ─────────────────────┐
│  ├── ExactHash           (weight: 0.5)   │
│  ├── TokenFingerprint    (weight: 0.3)   │
│  └── NCD                 (weight: 0.2)   │
└───────────────────────────────────────────┘

동작 예시:
  "error line 42" vs "error line 58"
    ExactHash:        0.0 × 0.5 = 0.0
    TokenFingerprint: 1.0 × 0.3 = 0.3
    NCD:              0.92 × 0.2 = 0.184
    composite score = 0.484 / 1.0 = 0.484  → threshold 0.8 미달

  "error line 42" vs "error line 42"
    ExactHash:        1.0 × 0.5 = 0.5
    TokenFingerprint: 1.0 × 0.3 = 0.3
    NCD:              1.0 × 0.2 = 0.2
    composite score = 1.0  → threshold 초과 → 유사

중첩 Composite:
┌─ CompositeSimilarity (root) ──────────────┐
│  ├── ExactHash           (weight: 0.4)   │
│  └── CompositeSimilarity (weight: 0.6)   │
│        ├── TokenFingerprint (weight: 0.5)│
│        ├── NCD              (weight: 0.3)│
│        └── LineJaccard      (weight: 0.2)│
└───────────────────────────────────────────┘
```

---

## StagnationDetector — PatternDetector 컴포지트

```rust
pub struct StagnationDetector {
    pub detectors: Vec<Box<dyn PatternDetector>>,
    pub config: StagnationConfig,
}

impl StagnationDetector {
    /// 등록된 모든 PatternDetector를 실행하고 결과를 합산.
    pub async fn detect(&self, source_id: &str, state: &str, db: &dyn Db)
        -> Result<Vec<StagnationDetection>>;
}
```

v6에서는 `SpinningDetector` + `OscillationDetector`만 등록. Phase 2에서 `DriftDetector` + `DiminishingDetector`를 추가하면 코어 변경 없이 동작한다.

각 PatternDetector는 내부적으로 `SimilarityJudge`를 사용할 수 있다 (SPINNING, OSCILLATION). drift 기반 detector는 SimilarityJudge 불필요.

### 탐지 알고리즘

#### SpinningDetector (v6) — 유사 출력 반복

DB에서 summaries와 errors를 **별도로** 조회·검사한다. 어느 쪽이든 감지되면 SPINNING.

```
SpinningDetector.detect(source_id, state, db):
  // 1. outputs 검사 — DB에서 직접 조회
  recent_outputs = db.query("SELECT summary FROM history
      WHERE source_id=? AND state=? ORDER BY created_at DESC LIMIT ?",
      source_id, state, spinning_threshold)
  if all_pairs_similar(recent_outputs, judge, similarity_threshold):
      return SPINNING(source: "outputs")

  // 2. errors 검사 — DB에서 직접 조회
  recent_errors = db.query("SELECT error FROM history
      WHERE source_id=? AND state=? AND error IS NOT NULL
      ORDER BY created_at DESC LIMIT ?",
      source_id, state, spinning_threshold)
  if all_pairs_similar(recent_errors, judge, similarity_threshold):
      return SPINNING(source: "errors")

all_pairs_similar(items, judge, threshold):
  for i in 1..items.len():
      if judge.score(items[0], items[i]) < threshold:
          return false
  return true
```

- threshold 개수: `spinning_threshold` (기본 3)
- 유사도 기준: `similarity_threshold` (기본 0.8)
- confidence: 유사도 점수의 평균값

#### OscillationDetector (v6) — 교대 반복

```
OscillationDetector.detect(source_id, state, db):
  recent = db.query("SELECT summary FROM history
      WHERE source_id=? AND state=? ORDER BY created_at DESC LIMIT ?",
      source_id, state, cycles * 2)
  
  // 짝수 그룹 내 유사
  even_similar = all_pairs_similar(recent[::2], judge, similarity_threshold)
  // 홀수 그룹 내 유사
  odd_similar = all_pairs_similar(recent[1::2], judge, similarity_threshold)
  // 짝수↔홀수 비유사
  cross_different = judge.score(recent[0], recent[1]) < 0.3

  if even_similar && odd_similar && cross_different:
      return OSCILLATION
```

#### DriftDetector / DiminishingDetector (Phase 2) — drift score 기반

Phase 2에서 `PatternDetector` impl로 추가. SimilarityJudge 불필요.

DB에서 작업 결과(history.summary)와 원래 목표(source_data)를 조회하여 goal_drift를 산출한다. Ouroboros의 가중 합산 모델을 차용:

```
combined_drift = (goal_drift × 0.5) + (constraint_drift × 0.3) + (ontology_drift × 0.2)
```

- **goal_drift**: 원래 목표(이슈 본문) vs 현재 결과(summary)의 Jaccard 거리
- **constraint_drift**: 제약 위반 추적 (workspace별 이력 필요)
- **ontology_drift**: 개념 공간 변화 (workspace별 이력 필요)

v6에서는 trait 경계만 정의하고 impl 없음 — `StagnationDetector`에 등록되지 않으므로 동작하지 않는다. Phase 2에서 impl 추가 시 코어 변경 0.

```
DriftDetector.detect(source_id, state, db):
  summaries = db.query("SELECT summary FROM history WHERE ...")
  source_data = db.query("SELECT source_data FROM queue_items WHERE ...")
  goal = extract_goal(source_data)  // 이슈 본문 등
  drift = compute_goal_drift(goal, summaries.last())
  store_drift_score(db, source_id, state, drift)
  // NO_DRIFT: 최근 N개 drift 변화량 < epsilon
  // DIMINISHING: 개선폭이 감소 추세
```

---

## Lateral Thinking — 내장 페르소나에 의한 사고 전환

Stagnation이 감지되면 **모든 retry에 lateral plan이 자동 주입**된다. 이것이 retry의 기본 동작이다.

### 페르소나

5가지 사고 페르소나가 belt-core에 내장된다. 각 페르소나는 `include_str!`로 바이너리에 임베딩된 prompt template이다.

```
crates/belt-core/src/stagnation/
  personas/
    hacker.md          # include_str!로 바이너리에 포함
    architect.md
    researcher.md
    simplifier.md
    contrarian.md
```

| 페르소나 | 패턴 친화도 | 전략 |
|----------|-----------|------|
| **HACKER** | SPINNING | 제약 우회, 워크어라운드, 다른 도구/라이브러리 시도 |
| **ARCHITECT** | OSCILLATION | 구조 재설계, 관점 전환, 근본 원인 분석 |
| **RESEARCHER** | NO_DRIFT | 정보 수집, 문서/테스트 조사, 체계적 디버깅 |
| **SIMPLIFIER** | DIMINISHING | 복잡도 축소, 가정 제거, 최소 구현 |
| **CONTRARIAN** | 복합/기타 | 가정 뒤집기, 문제 역전, 완전히 다른 접근 |

### 패턴 → 페르소나 선택

```rust
fn select_persona(
    pattern: StagnationPattern,
    tried: &[Persona],          // 이전에 시도한 페르소나 제외
) -> Option<Persona> {
    let affinity = match pattern {
        Spinning          => [Hacker, Contrarian, Simplifier, Architect, Researcher],
        Oscillation       => [Architect, Contrarian, Simplifier, Hacker, Researcher],
        NoDrift           => [Researcher, Contrarian, Architect, Hacker, Simplifier],
        DiminishingReturns => [Simplifier, Contrarian, Researcher, Architect, Hacker],
    };
    affinity.iter().find(|p| !tried.contains(p)).copied()
}
```

### LateralAnalyzer

```rust
pub struct LateralAnalyzer;

impl LateralAnalyzer {
    /// 감지된 패턴에 대해 lateral plan을 생성
    /// 내부적으로 belt agent -p를 호출하여 LLM이 분석
    pub async fn analyze(
        &self,
        detection: &StagnationDetection,
        history: &ExecutionHistory,
        persona: Persona,
        workspace: &str,
    ) -> Result<LateralPlan>;
}

pub struct LateralPlan {
    pub persona: Persona,
    pub failure_analysis: String,    // 이전 실패 원인 분석
    pub alternative_approach: String, // 대안 접근법
    pub execution_plan: String,       // 구체적 실행 계획
    pub warnings: String,             // 주의사항
}
```

실행: `belt agent --workspace <path> -p "{persona.prompt}\n\n{failure_context}"`

### Retry에 lateral_plan 주입

```
원래 handler prompt:
  "이슈를 구현해줘"

lateral retry 시 합성:
  "이슈를 구현해줘"
  +
  "⚠ Stagnation Analysis (attempt 2/3)
   Pattern: SPINNING | Persona: HACKER
   
   실패 원인: 이전 2회 시도에서 동일한 컴파일 에러 반복
   대안 접근법: 기존 Session 직접 구현 대신 tower-sessions crate 활용
   실행 계획: 1. Cargo.toml 수정  2. 타입 교체  3. middleware 등록
   주의: 이전과 동일한 접근은 같은 실패를 반복합니다"
```

### HITL에 lateral report 첨부

모든 페르소나가 소진되거나 failure_count가 hitl에 도달하면, 지금까지의 lateral 시도 이력이 `hitl_notes`에 첨부된다.

```
HITL Event:
  reason: retry_max_exceeded
  hitl_notes:
    "Stagnation Report:
     pattern: SPINNING (3회 유사 에러)
     
     attempt 1: compile error (Session not found)
     attempt 2: HACKER 제안 → tower-sessions 시도 → 다른 에러
     attempt 3: CONTRARIAN 제안 → trait object 시도 → 컴파일 성공, 테스트 실패
     
     2회 접근 전환 후에도 미해결. 구조적 문제일 가능성."
```

---

## Integration Points

### Daemon 실행 루프

```
handler/on_enter 실행 실패
    │
    ▼
① ExecutionHistory 구성
   outputs = DB에서 최근 N개 history.summary
   errors  = DB에서 최근 N개 history.error (별도)
   drifts  = (Phase 2) drift scores
    │
    ▼
② StagnationDetector.detect(history)
   내부: CompositeSimilarity로 유사도 판단
   → Vec<StagnationDetection>
    │
    ▼
③ Lateral Plan 생성 (패턴 감지 시)
   패턴 → 페르소나 선택 (이전 시도 제외)
   belt agent -p로 LLM 분석 → lateral_plan
    │
    ▼
④ Escalation 적용 (failure_count 기반, 기존과 동일)
   retry            → lateral_plan 주입하여 재시도
   retry_with_comment → lateral_plan 주입 + on_fail
   hitl             → lateral_report를 hitl_notes에 첨부
    │
    ▼
⑤ transition_events에 기록
   event_type: 'stagnation'
   detail: { pattern, confidence, evidence,
             persona, lateral_plan, judge_scores }
```

### 데이터 소스 — 각 Detector가 DB에서 직접 조회

| 데이터 | DB 테이블/컬럼 | Detector | 구현 시점 |
|--------|---------------|----------|----------|
| handler 출력 | `history.summary` | SpinningDetector, OscillationDetector | v6 |
| 에러 메시지 | `history.error` | SpinningDetector (별도 검사) | v6 |
| 시도 번호 | `history.attempt` | sliding window 범위 결정 | v6 |
| 원래 목표 | `queue_items.source_data` | DriftDetector (goal_drift 산출) | Phase 2 |
| drift score 이력 | (Phase 2 테이블) | DiminishingDetector | Phase 2 |
| 이전 lateral | `transition_events` (stagnation) | 페르소나 중복 제외 | v6 |

### 이벤트 기록

```
event_type = 'stagnation'
detail = JSON {
    "pattern": "spinning",
    "confidence": 0.95,
    "evidence": {
        "source": "errors",
        "judge_scores": { "exact_hash": 0.0, "token_fp": 1.0, "ncd": 0.92 },
        "composite_score": 0.484
    },
    "persona": "hacker",
    "lateral_plan": "tower-sessions crate로 전환...",
    "escalation": "retry"
}
```

---

## Configuration

```yaml
# workspace.yaml
stagnation:
  enabled: true                    # 기본 true
  spinning_threshold: 3            # 최소 연속 유사 출력 수 (기본 3)
  oscillation_cycles: 2            # 최소 교대 사이클 수 (기본 2, → 4회 출력)
  similarity_threshold: 0.8        # composite score 유사 판정 기준 (기본 0.8)
  no_drift_epsilon: 0.01           # drift score 변화 임계값 (기본 0.01)
  no_drift_iterations: 3           # drift 정체 판정 반복 수 (기본 3)
  diminishing_threshold: 0.01      # 개선폭 임계값 (기본 0.01)
  confidence_threshold: 0.5        # 탐지 유효 최소 confidence (기본 0.5)

  similarity:                      # CompositeSimilarity 구성 (기본 프리셋 제공)
    - judge: exact_hash
      weight: 0.5
    - judge: token_fingerprint
      weight: 0.3
    - judge: ncd
      weight: 0.2

  lateral:
    enabled: true                  # 기본 true
    max_attempts: 3                # 페르소나 최대 시도 횟수 (기본 3)
```

### StagnationConfig

```rust
pub struct StagnationConfig {
    pub enabled: bool,
    pub spinning_threshold: u32,
    pub oscillation_cycles: u32,
    pub similarity_threshold: f64,
    pub no_drift_epsilon: f64,
    pub no_drift_iterations: u32,
    pub diminishing_threshold: f64,
    pub confidence_threshold: f64,
    pub similarity: Vec<JudgeConfig>,  // judge name + weight
    pub lateral: LateralConfig,
}

pub struct LateralConfig {
    pub enabled: bool,
    pub max_attempts: u32,
}

pub struct JudgeConfig {
    pub judge: String,   // "exact_hash" | "token_fingerprint" | "ncd"
    pub weight: f64,
}
```

---

## 모듈 구조

```
crates/belt-core/src/stagnation/
  mod.rs                    # pub exports
  pattern.rs                # StagnationPattern, StagnationDetection
  history.rs                # ExecutionHistory
  detector.rs               # StagnationDetector
  similarity/
    mod.rs                  # SimilarityJudge trait
    composite.rs            # CompositeSimilarity (impl SimilarityJudge)
    exact_hash.rs           # ExactHash judge
    token_fingerprint.rs    # TokenFingerprint judge
    ncd.rs                  # NCD judge
  lateral/
    mod.rs                  # LateralAnalyzer, LateralPlan
    persona.rs              # Persona enum, select_persona()
    personas/
      hacker.md             # include_str! 내장
      architect.md
      researcher.md
      simplifier.md
      contrarian.md
```

---

## 수용 기준

### Similarity (Composite Pattern)

- [ ] `SimilarityJudge` trait이 단일 인터페이스로 유사도를 제공한다
- [ ] `CompositeSimilarity`가 `SimilarityJudge`를 구현하여 중첩 가능하다
- [ ] `StagnationDetector`는 `Box<dyn SimilarityJudge>` 하나만 의존한다
- [ ] yaml의 `similarity` 설정으로 judge 구성을 변경할 수 있다
- [ ] 기본 프리셋(exact_hash + token_fp + ncd)이 설정 생략 시 적용된다

### Detection (4 Patterns)

- [ ] outputs에서 최근 N개가 유사(composite score ≥ threshold)하면 SPINNING이 감지된다
- [ ] errors에서 최근 N개가 유사하면 SPINNING이 감지된다 (별도 검사)
- [ ] 최근 2N개 outputs이 짝수/홀수 교대 패턴이면 OSCILLATION이 감지된다
- [ ] drift score 변화량이 epsilon 미만이면 NO_DRIFT가 감지된다
- [ ] 개선폭이 threshold 미만이면 DIMINISHING_RETURNS가 감지된다
- [ ] stagnation.enabled=false이면 탐지를 수행하지 않는다

### Lateral Thinking

- [ ] 패턴 감지 시 패턴 친화도 순으로 페르소나가 선택된다
- [ ] 이전에 시도한 페르소나는 제외된다
- [ ] 선택된 페르소나의 내장 prompt로 `belt agent -p`를 호출하여 lateral_plan을 생성한다
- [ ] lateral_plan이 retry 시 handler prompt에 추가 컨텍스트로 주입된다
- [ ] hitl 도달 시 모든 lateral 시도 이력이 hitl_notes에 첨부된다
- [ ] lateral.enabled=false이면 lateral plan 없이 기존 escalation만 적용된다
- [ ] lateral.max_attempts를 초과하면 더 이상 페르소나를 시도하지 않는다

### 이벤트

- [ ] 탐지 이벤트가 transition_events에 event_type='stagnation'으로 기록된다
- [ ] evidence에 각 judge별 score가 포함된다
- [ ] lateral plan과 페르소나 정보가 event detail에 포함된다

---

### 관련 문서

- [DESIGN-v6](../DESIGN.md) — 설계 철학 #11
- [Daemon](./daemon.md) — 실행 루프 통합 지점
- [QueuePhase 상태 머신](./queue-state-machine.md) — escalation 정책
- [Data Model](./data-model.md) — StagnationPattern enum, HitlReason 확장
- [실패 복구와 HITL](../flows/04-failure-and-hitl.md) — 실패 경로 통합
