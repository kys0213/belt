# Stagnation Detection — 반복 실행 패턴 감지 + 사고 전환

> LLM이 같은 실수를 반복하는 패턴을 감지하고, 접근법을 전환하여 재시도한다.
> "몇 번 실패했는가"가 아니라 "어떻게 실패했는가"를 보고, "다르게 시도"한다.
>
> 참고: [Ouroboros](https://github.com/kys0213/ouroboros) 프로젝트의 이중 계층 탐지 + lateral thinking을 Belt에 적용.

---

## 설계 요약 (현재 배선)

```
handler 실패
    │
    ▼
Stagnation 분석 (같은 source_id + state에서 실패 이력이 있으면 항상 실행)
    │
    ├── ① 유사도 판단 (SpinningDetector + ExactHash, threshold 0.9 / 최소 연속 2회)
    │     과거 실패 error 메시지 + 이번 error를 순서대로 비교
    │
    ├── ② Lateral Plan 생성 (패턴 감지 시)
    │     페르소나 선택 후 정적 directive 텍스트를 조합 (LLM 미호출)
    │
    └── ③ Escalation 적용 (failure_count 기반, 기존 로직 그대로)
          retry            → lateral_plan을 handler prompt에 주입하여 재시도
          retry_with_comment → lateral_plan 주입 + on_fail
          hitl             → lateral 이력을 hitl_notes에 첨부
```

core에는 이보다 넓은 설계(가중 합산 유사도, Oscillation 탐지, LLM 기반 lateral 분석)가 구현·테스트되어 있지만 daemon에는 위 범위만 배선되어 있다. 상세는 [core 구현 완료, daemon 미배선](#core-구현-완료-daemon-미배선) 참조.

---

## 탐지 대상: 4가지 정체 패턴 (개념)

`StagnationPattern` enum은 4가지 값을 정의하지만, 실제 detector 구현이 있는 것은 SPINNING과 OSCILLATION뿐이다. NO_DRIFT/DIMINISHING_RETURNS는 enum variant만 존재하고 대응하는 detector 구현은 없다.

| 패턴 | 정의 | Belt에서의 예시 | detector 구현 |
|------|------|----------------|--------------|
| **SPINNING** | A→A→A (동일/유사 반복) | 같은 코드 생성 → 같은 컴파일 에러 반복 | `SpinningDetector` (daemon에 배선됨) |
| **OSCILLATION** | A→B→A→B (교대 반복) | 리팩토링 → 원복 → 리팩토링, 설정 A↔B 왕복 | `OscillationDetector` (core 구현, daemon 미배선) |
| **NO_DRIFT** | 진행 점수 정체 | 테스트 통과율이 변하지 않음 | 미구현 (enum만 존재) |
| **DIMINISHING_RETURNS** | 개선폭 감소 | 매 시도마다 개선은 있으나 점점 미미 | 미구현 (enum만 존재) |

---

## Core Types

`crates/belt-core/src/stagnation/pattern.rs` 기준.

### StagnationPattern

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    pub confidence: f64,   // 0.0 ~ 1.0
    pub reason: String,    // 사람이 읽을 수 있는 탐지 근거
}
```

### PatternDetector trait

각 detector는 DB나 비동기 조회에 의존하지 않는다. 시간순으로 정렬된 텍스트 배열(`outputs`)을 받아 동기적으로 판정한다. DB 조회·필터링은 호출자(daemon)의 책임이다.

```rust
pub trait PatternDetector: Send + Sync {
    /// outputs는 오래된 것부터 최신 순.
    fn detect(&self, outputs: &[&str]) -> Option<StagnationDetection>;
    fn target_pattern(&self) -> StagnationPattern;
}
```

---

## Similarity — Composite Pattern

유사도 판단을 단일 trait으로 추상화하고, Composite Pattern으로 여러 알고리즘을 가중 합산할 수 있다. `crates/belt-core/src/stagnation/similarity.rs` 기준.

### trait 정의

```rust
pub trait SimilarityJudge: Send + Sync {
    fn score(&self, a: &str, b: &str) -> f64;  // [0.0, 1.0], 1.0 = 동일
    fn name(&self) -> &str;
}
```

### 내장 Judge 구현체

| Judge | 원리 | 출력 | 용도 |
|-------|------|------|------|
| **ExactHash** | 해시 비교 | 동일=1.0, 다름=0.0 | 완전 동일 감지 (빠름). daemon이 직접 사용 |
| **TokenFingerprint** | 숫자/경로/해시값/UUID를 정규화 후 토큰 Jaccard 지수 계산 | 0.0~1.0 연속값 | "line 42" vs "line 58" 같은 차이 무시 |
| **NcdJudge** | Normalized Compression Distance (flate2 gzip) | 0.0~1.0 연속값 | 구조적 유사도 측정 |

### CompositeSimilarity

여러 Judge를 가중 평균으로 합성한다. Composite 자체도 `SimilarityJudge`를 구현하므로 중첩 가능하다.

```rust
pub struct CompositeSimilarity {
    judges: Vec<(Box<dyn SimilarityJudge>, f64)>,  // (judge, weight)
}

impl SimilarityJudge for CompositeSimilarity {
    fn score(&self, a: &str, b: &str) -> f64 {
        let (sum, w_sum) = self.judges.iter()
            .map(|(j, w)| (j.score(a, b) * w, w))
            .fold((0.0, 0.0), |(s, ws), (v, w)| (s + v, ws + w));
        (sum / w_sum).clamp(0.0, 1.0)
    }
    fn name(&self) -> &str { "composite" }
}
```

기본 프리셋(`Default` impl): `ExactHash(0.5) + TokenFingerprint(0.3) + NcdJudge(0.2)`.

> daemon은 이 절의 Judge/Composite를 호출하지 않는다 — `ExactHash`만 직접 사용한다. 상세: [core 구현 완료, daemon 미배선](#core-구현-완료-daemon-미배선)

---

## StagnationDetector — PatternDetector 컴포지트

```rust
pub struct StagnationDetector {
    detectors: Vec<Box<dyn PatternDetector>>,
}

impl StagnationDetector {
    pub fn new(detectors: Vec<Box<dyn PatternDetector>>) -> Self;

    /// 등록된 모든 detector를 실행하고, 감지된 것 중 confidence가 가장 높은 것을 반환.
    pub fn detect(&self, outputs: &[&str]) -> Option<StagnationDetection>;
}
```

### 현재 daemon 배선

daemon은 실패마다 아래 형태로 `StagnationDetector`를 구성한다 (`crates/belt-daemon/src/daemon.rs`):

```rust
let detector = StagnationDetector::new(vec![
    Box::new(SpinningDetector::new(Box::new(ExactHash), 0.9, 2)),
]);
```

- threshold(0.9)와 min_consecutive(2)는 코드에 하드코딩되어 있다 — yaml로 노출되지 않는다.
- `outputs`는 같은 `source_id` + `state`의 과거 실패 error 메시지(DB `history` 조회, DB 조회 실패 시 in-memory 이력으로 폴백)에 이번 실패의 error를 이어붙인 배열이다.
- 과거 실패 이력이 하나도 없으면(첫 실패) stagnation 분석 자체를 생략한다.

### SpinningDetector 알고리즘

연속된 두 출력을 `judge.score()`로 비교해, threshold 이상인 쌍이 `min_consecutive`회 연속되면 SPINNING으로 판정한다.

```
for pair in outputs.windows(2):
    score = judge.score(pair[0], pair[1])
    if score >= threshold:
        consecutive += 1
    else:
        consecutive = 0
    if consecutive >= min_consecutive:
        return Spinning(confidence = 연속 구간 평균 score)
```

### OscillationDetector 알고리즘 (core 구현, daemon 미배선)

`outputs[i]`와 두 칸 전인 `outputs[i-2]`를 비교해 A-B-A-B 교대 패턴을 감지한다. `min_cycles`회 이상 일치하면 OSCILLATION으로 판정한다.

```
for i in 2..outputs.len():
    score = judge.score(outputs[i], outputs[i-2])
    if score >= threshold:
        cycles += 1
if cycles >= min_cycles:
    return Oscillation(confidence = 평균 score)
```

---

## Lateral Thinking — 내장 페르소나에 의한 사고 전환

Stagnation이 감지되면 다음 retry에 lateral plan이 주입된다.

### 페르소나

5가지 사고 페르소나가 belt-core에 내장된다. 각 페르소나는 `include_str!`로 바이너리에 임베딩된 prompt template과, 코드에 고정된 한 줄 directive를 가진다 (`crates/belt-core/src/stagnation/lateral.rs`, `personas/*.md`).

| 페르소나 | 패턴 친화도 | 전략 |
|----------|-----------|------|
| **HACKER** | SPINNING | 제약 우회, 워크어라운드, 다른 도구/라이브러리 시도 |
| **ARCHITECT** | OSCILLATION | 구조 재설계, 관점 전환, 근본 원인 분석 |
| **RESEARCHER** | NO_DRIFT | 정보 수집, 문서/테스트 조사, 체계적 디버깅 |
| **SIMPLIFIER** | DIMINISHING | 복잡도 축소, 가정 제거, 최소 구현 |
| **CONTRARIAN** | 복합/기타 | 가정 뒤집기, 문제 역전, 완전히 다른 접근 |

### 패턴 → 페르소나 선택

`Persona::affinity_order(pattern)`이 패턴별 우선순위 배열을 반환하고, `LateralAnalyzer::select_persona()`가 이미 시도한 페르소나(같은 source_id/state의 과거 `lateral_plan`에서 추출)를 제외한 첫 번째 페르소나를 고른다. 모든 페르소나를 소진하면 `None`을 반환하고, 이 경우 daemon은 lateral plan 생성을 건너뛴다.

### daemon이 실제로 만드는 lateral plan

`LateralAnalyzer`에는 `belt agent -p`를 서브프로세스로 호출해 LLM에게 실패 분석·대안 접근·실행 계획을 생성시키는 `analyze()` 메서드가 구현·테스트되어 있다. 하지만 **daemon은 이 메서드를 호출하지 않는다.** daemon은 선택된 persona의 고정 `directive()` 문자열만으로 아래 형태의 텍스트를 조립해 retry 시 handler prompt 뒤에 붙인다.

```
## Lateral Plan
Stagnation Analysis (attempt {failure_count})
Pattern: {pattern} | Persona: {persona}

{persona.directive()}

Warning: Previous approaches produced similar failures. You MUST try a fundamentally different approach.
```

LLM 기반 `analyze()` 경로(failure_analysis/alternative_approach/execution_plan/warnings를 LLM이 채우는 `LateralPlan` 구조체)는 core에 구현되어 있으나 미배선이다. 상세: [core 구현 완료, daemon 미배선](#core-구현-완료-daemon-미배선)

### HITL에 lateral 이력 첨부

failure_count가 hitl에 도달하면, `HitlService::build_lateral_hitl_notes()`가 현재 lateral_plan과 해당 work_id의 `stagnation` 이벤트 이력을 `hitl_notes`에 첨부한다 (`crates/belt-daemon/src/hitl_service.rs`).

```
## Lateral Thinking History
- Current lateral plan: {lateral_plan}
- Stagnation events: {N}건
- Pattern: {pattern_type} (confidence: {confidence})
- Persona: {recommended_persona}
```

---

## Integration Points

### Daemon 실행 루프

```
handler/on_enter 실행 실패
    │
    ▼
① 실패 이력 조회
   같은 source_id + state의 과거 failed error 메시지(DB history 우선, 실패 시 in-memory 폴백)
   + 이번 실패의 error를 이어붙여 outputs 구성
    │
    ▼
② StagnationDetector.detect(outputs)
   등록된 detector: SpinningDetector(ExactHash, threshold=0.9, min_consecutive=2)
    │
    ▼
③ Lateral Plan 생성 (패턴 감지 시)
   페르소나 선택 (이전 시도 제외) → 정적 directive 텍스트로 조합 (LLM 미호출)
    │
    ▼
④ Escalation 적용 (failure_count 기반, 기존과 동일)
   retry            → lateral_plan 주입하여 재시도
   retry_with_comment → lateral_plan 주입 + on_fail
   hitl             → lateral 이력을 hitl_notes에 첨부
    │
    ▼
⑤ transition_events에 기록
   event_type: 'stagnation'
   detail: { pattern_type, confidence, reason, recommended_persona, failure_count }
```

### 이벤트 기록

```
event_type = 'stagnation'
detail = JSON {
    "pattern_type": "spinning",
    "confidence": 0.95,
    "reason": "2 consecutive pairs above threshold 0.90 (avg similarity 1.000)",
    "recommended_persona": "hacker",
    "failure_count": 2
}
```

---

## Configuration

> 전체 yaml 스키마: [workspace-schema.md](./workspace-schema.md)

```yaml
# workspace.yaml
stagnation:
  enabled: true          # 기본 true — false면 stagnation 탐지 자체를 건너뛴다
  lateral:
    enabled: true         # 기본 true — false면 탐지는 하되 lateral plan은 생성하지 않는다
```

### StagnationConfig (실제 필드)

```rust
pub struct StagnationConfig {
    pub enabled: bool,
    pub lateral: LateralConfig,
}

pub struct LateralConfig {
    pub enabled: bool,
}
```

SpinningDetector의 threshold/min_consecutive, CompositeSimilarity의 judge 구성 등은 yaml로 노출되지 않는다 — 필요해지면 `StagnationConfig`에 필드를 추가하고 daemon의 하드코딩된 생성자 호출을 교체해야 한다.

---

## core 구현 완료, daemon 미배선

아래 항목들은 `crates/belt-core/src/stagnation/`에 구현되고 단위 테스트도 갖춰져 있지만, daemon 실행 경로에서는 호출되지 않는 OCP 확장점이다. 새로 배선하려면 daemon 코드 변경만 필요하고 core 변경은 필요 없다.

| 항목 | 위치 | 현재 상태 |
|------|------|----------|
| `CompositeSimilarity` (가중 합산 유사도) | `similarity.rs` | daemon은 `ExactHash`를 직접 사용 |
| `TokenFingerprint` / `NcdJudge` | `similarity.rs` | `CompositeSimilarity`를 통해서만 조합되므로 미배선과 함께 미사용 |
| `OscillationDetector` | `pattern.rs` | `StagnationDetector`에 등록되지 않음 |
| `LateralAnalyzer::analyze()` (LLM 서브프로세스 호출) | `lateral.rs` | daemon은 `select_persona()` + 고정 directive만 사용 |
| `LateralPlan` 구조체 (failure_analysis/alternative_approach/execution_plan/warnings) | `lateral.rs` | daemon은 이 구조체를 생성하지 않고 평문 문자열만 조립 |

NO_DRIFT/DIMINISHING_RETURNS 패턴에 대한 detector(goal_drift 산출 등)는 core에도 구현이 없다 — 설계 로드맵은 [source_data와 stagnation 로드맵](../../plans/source-data-and-stagnation-roadmap.md) 참조.

---

### 관련 문서

- [DESIGN](../DESIGN.md) — 설계 철학 #11
- [Daemon](./daemon.md) — 실행 루프 통합 지점
- [QueuePhase 상태 머신](./queue-state-machine.md) — escalation 정책
- [Data Model](./data-model.md) — StagnationPattern enum, HitlReason 확장
- [실패 복구와 HITL](../flows/04-failure-and-hitl.md) — 실패 경로 통합
