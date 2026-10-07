# Stagnation Detection — 반복 실행 패턴 감지 + 사고 전환

> LLM이 같은 실수를 반복하는 패턴을 감지하고, 접근법을 전환하여 재시도한다.
> "몇 번 실패했는가"가 아니라 "어떻게 실패했는가"를 보고, "다르게 시도"한다.
>
> 참고: [Ouroboros](https://github.com/kys0213/ouroboros) 프로젝트의 이중 계층 탐지 + lateral thinking을 Belt에 적용.

---

## 설계 요약

```
handler 실패
    │
    ▼
Stagnation 분석 (같은 source_id + state에서 실패 이력이 있으면 항상 실행)
    │
    ├── ① 유사도 판단 (완전 일치·토큰 중복도·압축 유사도의 가중 합성, threshold 0.9)
    │     과거 실패 error 메시지 + 이번 error를 순서대로 비교해
    │     SPINNING(동일 출력 3회 연속)·OSCILLATION(A→B→A→B 교대 반복) 여부를 판정
    │
    ├── ② Lateral Plan 생성 (패턴 감지 시)
    │     페르소나 선택 후 정적 directive 텍스트를 조합 (LLM 미호출)
    │
    └── ③ Escalation 적용 (failure_count 기반, 기존 로직 그대로)
          retry            → lateral_plan을 handler prompt에 주입하여 재시도
          retry_with_comment → lateral_plan 주입 + on_fail
          hitl             → lateral 이력을 hitl_notes에 첨부
```

유사도 판단·패턴 감지 로직은 코어 변경 없이 새 알고리즘을 추가할 수 있는 확장점(OCP)으로 설계되어 있다. 현재 배선 범위와 확장 로드맵은 [source_data와 stagnation 로드맵](../../plans/source-data-and-stagnation-roadmap.md) 참조.

---

## 탐지 대상: 정체 패턴

현재 실제로 감지되는 패턴은 **SPINNING**(A→A→A, 동일/유사 출력 반복)과 **OSCILLATION**(A→B→A→B, 두 출력 사이를 교대로 반복)이다. 예: 같은 컴파일 에러가 반복되면 SPINNING, 서로 다른 두 수정안을 번갈아 시도하면 OSCILLATION.

패턴 유형 전체 정의(enum)는 [Data Model](./data-model.md#stagnationpattern) 참조. SPINNING·OSCILLATION 외 패턴(NO_DRIFT, DIMINISHING_RETURNS)의 확장 로드맵은 [source_data와 stagnation 로드맵](../../plans/source-data-and-stagnation-roadmap.md) 참조.

---

## Core Types

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

## Similarity — 유사도 판단

유사도 판단은 단일 trait으로 추상화되어 있어, 새 알고리즘을 추가해도 코어 변경이 필요 없다 (OCP).

### trait 정의

```rust
pub trait SimilarityJudge: Send + Sync {
    fn score(&self, a: &str, b: &str) -> f64;  // [0.0, 1.0], 1.0 = 동일
    fn name(&self) -> &str;
}
```

현재 daemon은 완전 일치 비교·토큰 중복도(정규화 후 Jaccard 유사도)·압축 유사도(NCD)를 0.5 : 0.3 : 0.2 가중치로 합산한 판정을 사용한다. 가중치를 yaml로 노출하는 등의 추가 확장 로드맵은 [source_data와 stagnation 로드맵](../../plans/source-data-and-stagnation-roadmap.md) 참조.

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

### 현재 판정 기준

정체 판정은 완전 일치·토큰 중복도·압축 유사도의 가중 합성 점수(threshold 0.9)를 기준으로 한다. SPINNING은 동일 출력 3회 연속(인접 쌍 일치 2회), OSCILLATION은 두 출력이 2회 이상 교대로 반복되면(A→B→A→B) 판정한다 — threshold와 반복 횟수 모두 현재 고정값이며 yaml로 노출되지 않는다. 두 조건이 동시에 성립하면(예: 동일 출력이 4회 이상 이어지면 교대 조건도 우연히 성립한다) confidence가 더 높은 쪽을 채택하고, confidence가 같으면 SPINNING을 우선한다.

완전 일치가 아닌 유사 반복(near-miss)은 현재 가중치·threshold 조합에서 SPINNING·OSCILLATION 어느 쪽으로도 판정되지 않는다 — 완전 일치 요소(가중치 0.5)가 어긋나면 나머지 요소가 만점이어도 합성 점수 상한이 0.5로, threshold 0.9에 도달할 수 없다. 오탐을 늘리지 않기 위해 의도적으로 남겨둔 경계다.

- 입력(`outputs`)은 같은 `source_id` + `state`의 과거 실패 error 메시지(DB `history` 조회, DB 조회 실패 시 in-memory 이력으로 폴백)에 이번 실패의 error를 이어붙인 배열이다.
- 과거 실패 이력이 하나도 없으면(첫 실패) stagnation 분석 자체를 생략한다.

### 판정 알고리즘

**SPINNING**: 연속된 두 출력을 비교해, threshold 이상인 쌍이 최소 연속 횟수만큼 이어지면 판정한다.

```
for pair in outputs.windows(2):
    score = similarity(pair[0], pair[1])
    if score >= threshold:
        consecutive += 1
    else:
        consecutive = 0
    if consecutive >= min_consecutive:
        return Spinning(confidence = 연속 구간 평균 score)
```

**OSCILLATION**: 두 칸 떨어진 출력끼리 비교해(A→B→A 교대 패턴), threshold 이상인 쌍이 최소 반복 횟수 이상이면 판정한다.

```
for i in 2..outputs.len():
    score = similarity(outputs[i], outputs[i - 2])
    if score >= threshold:
        cycles += 1
if cycles >= min_cycles:
    return Oscillation(confidence = 일치한 쌍의 평균 score)
```

---

## Lateral Thinking — 내장 페르소나에 의한 사고 전환

Stagnation이 감지되면 다음 retry에 lateral plan이 주입된다.

### 페르소나

5가지 사고 페르소나가 바이너리에 내장된다. 각 페르소나는 임베딩된 prompt template과, 코드에 고정된 한 줄 directive를 가진다.

| 페르소나 | 패턴 친화도 | 전략 |
|----------|-----------|------|
| **HACKER** | SPINNING | 제약 우회, 워크어라운드, 다른 도구/라이브러리 시도 |
| **ARCHITECT** | OSCILLATION | 구조 재설계, 관점 전환, 근본 원인 분석 |
| **RESEARCHER** | NO_DRIFT | 정보 수집, 문서/테스트 조사, 체계적 디버깅 |
| **SIMPLIFIER** | DIMINISHING | 복잡도 축소, 가정 제거, 최소 구현 |
| **CONTRARIAN** | 복합/기타 | 가정 뒤집기, 문제 역전, 완전히 다른 접근 |

### 패턴 → 페르소나 선택

`Persona::affinity_order(pattern)`이 패턴별 우선순위 배열을 반환하고, `LateralAnalyzer::select_persona()`가 이미 시도한 페르소나(같은 source_id/state의 과거 `lateral_plan`에서 추출)를 제외한 첫 번째 페르소나를 고른다. 모든 페르소나를 소진하면 `None`을 반환하고, 이 경우 daemon은 lateral plan 생성을 건너뛴다.

### Lateral Plan 조립 방식

선택된 persona의 고정 directive 한 줄만으로 아래 형태의 텍스트를 조립해, retry 시 handler prompt 뒤에 붙인다. LLM은 호출하지 않는다.

```
## Lateral Plan
Stagnation Analysis (attempt {failure_count})
Pattern: {pattern} | Persona: {persona}

{persona.directive()}

Warning: Previous approaches produced similar failures. You MUST try a fundamentally different approach.
```

LLM이 실패 분석·대안 접근·실행 계획을 직접 생성하는 방식으로의 고도화는 로드맵에 있다. 상세: [source_data와 stagnation 로드맵](../../plans/source-data-and-stagnation-roadmap.md)

### HITL에 lateral 이력 첨부

failure_count가 hitl에 도달하면, 현재 lateral_plan과 **계열**(파생 원본으로 이어진 아이템들)의 `stagnation` 이벤트 이력이 HITL 메모에 첨부된다.

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
② 정체 패턴 판정
   기준: 완전 일치·토큰 중복도·압축 유사도 가중 합성, threshold=0.9
   (SPINNING: min_consecutive=2, OSCILLATION: min_cycles=2)
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

유사도 threshold/min_consecutive, 알고리즘 구성 등은 yaml로 노출되지 않는다.

---

## 확장 여지

SPINNING·OSCILLATION 이외의 패턴 감지(NO_DRIFT, DIMINISHING_RETURNS), 유사도 가중치의 yaml 노출, LLM 기반 lateral 분석은 코어 변경 없이 daemon 배선만 바꾸면 추가할 수 있는 OCP 확장점이다. 현재 구현 범위의 상세 인벤토리와 로드맵은 [source_data와 stagnation 로드맵](../../plans/source-data-and-stagnation-roadmap.md) 참조.

---

### 관련 문서

- [DESIGN](../DESIGN.md) — 설계 철학 #11
- [Daemon](./daemon.md) — 실행 루프 통합 지점
- [QueuePhase 상태 머신](./queue-state-machine.md) — escalation 정책
- [Data Model](./data-model.md) — StagnationPattern enum, HitlReason 확장
- [실패 복구와 HITL](../flows/04-failure-and-hitl.md) — 실패 경로 통합
