# source_data와 stagnation 로드맵

> 이 문서는 작성 시점의 의사결정·로드맵 기록이다. 현재 동작·규격의 신뢰 소스가 아니다 — 현재 상태는 spec/ 과 코드로 판단한다.

---

## source_data 마이그레이션 전략

배경: `ItemContext.source_data`는 DataSource가 자유 스키마로 자기 고유 데이터를 채울 수 있도록 예약된 필드다 (OCP 확장점). 도입 당시 계획된 단계적 마이그레이션은 다음과 같았다.

| Phase | 상태 | 설명 |
|-------|------|------|
| **1** | `source_data` + `issue`/`pr` 양쪽 채움 | 하위 호환. 기존 script 수정 불필요 |
| **2** | `issue`/`pr` deprecated | script가 `source_data` 경로로 전환 |
| **3** | `issue`/`pr` 제거 | `source_data`만 사용 |

script에서의 접근 예시 (계획 당시):
```bash
# Phase 1 (양쪽 모두 가능)
belt context $WORK_ID --json | jq '.issue.number'
belt context $WORK_ID --json | jq '.source_data.issue.number'

# Phase 2 이후 (source_data 권장)
belt context $WORK_ID --json | jq '.source_data.issue.number'

# Jira DataSource 추가 시
belt context $WORK_ID --json | jq '.source_data.ticket.key'
```

**현재 실제 상태(코드 확인, spec 최신화 시점 기준)**: Phase 1조차 완료되지 않았다. GitHub/Mock 등 현재 구현된 모든 DataSource는 `source_data`를 항상 `Null`로 두고 `issue`/`pr` 필드만 채운다(`crates/belt-infra/src/sources/github.rs`, `crates/belt-infra/src/sources/mock.rs` 등). `source_data`는 필드 자체와 직렬화 스킵 로직(`crates/belt-core/src/context.rs`)만 존재하는 예약 필드다. Phase 2/3로 진행하려면 먼저 최소 하나의 DataSource가 Phase 1(양쪽 채움)을 실제로 구현해야 한다.

---

## Stagnation 로드맵

배경: [Ouroboros](https://github.com/kys0213/ouroboros) 프로젝트의 이중 계층 탐지(정체 패턴 4종 + lateral thinking)를 Belt에 이식하는 것이 원래 설계 목표였다. 계획된 범위는 다음과 같다.

### 로드맵 단계

| 단계 | 내용 | 현재 상태 |
|------|------|----------|
| 1 | SPINNING + OSCILLATION — 텍스트 유사도 기반 detector | SPINNING만 daemon에 배선. OSCILLATION은 core 구현·테스트 완료, daemon 미배선 |
| 2 | NO_DRIFT + DIMINISHING_RETURNS — drift score 기반 detector 추가 (코어 변경 0, OCP) | 미착수. detector 구현 자체가 없음 |
| — | Lateral plan을 LLM(`belt agent -p` 서브프로세스)이 생성하도록 고도화 (`LateralAnalyzer::analyze()`) | core 구현·테스트 완료, daemon 미배선. daemon은 여전히 고정 persona directive 텍스트만 사용 |
| — | yaml에서 유사도 threshold·window·judge 가중치를 설정 가능하게 함 | 미착수. `StagnationConfig`에는 `enabled`/`lateral.enabled`만 있고, threshold(0.9)·min_consecutive(2)는 daemon 코드에 하드코딩 |

### 검토했던 설계 — NO_DRIFT / DIMINISHING_RETURNS

drift score 기반 탐지 스케치 (구현 없음, 설계 스케치만 존재):

```
combined_drift = (goal_drift × 0.5) + (constraint_drift × 0.3) + (ontology_drift × 0.2)

DriftDetector.detect(source_id, state, db):
  summaries = db.query("SELECT summary FROM history WHERE ...")
  source_data = db.query("SELECT source_data FROM queue_items WHERE ...")
  goal = extract_goal(source_data)  // 이슈 본문 등
  drift = compute_goal_drift(goal, summaries.last())
  store_drift_score(db, source_id, state, drift)
  // NO_DRIFT: 최근 no_drift_iterations개(기본 3) drift 변화량 < epsilon
  // DIMINISHING: 개선폭이 감소 추세
```

- **goal_drift**: 원래 목표(이슈 본문) vs 현재 결과(summary)의 Jaccard 거리
- **constraint_drift**: 제약 위반 추적 (workspace별 이력 필요)
- **ontology_drift**: 개념 공간 변화 (workspace별 이력 필요)

이 설계는 `source_data`가 실제로 채워지는 것을 전제한다 — 위 source_data 마이그레이션이 먼저 끝나야 goal_drift 계산이 의미를 가진다.

### 검토했던 설정 스키마 (미구현)

당시 구상했던 `workspace.yaml`의 `stagnation` 절 전체 형태:

```yaml
stagnation:
  enabled: true
  spinning_threshold: 3
  oscillation_cycles: 2
  similarity_threshold: 0.8
  no_drift_epsilon: 0.01
  no_drift_iterations: 3
  diminishing_threshold: 0.01
  confidence_threshold: 0.5

  similarity:
    - judge: exact_hash
      weight: 0.5
    - judge: token_fingerprint
      weight: 0.3
    - judge: ncd
      weight: 0.2

  lateral:
    enabled: true
    max_attempts: 3
```

실제 `StagnationConfig`(`crates/belt-core/src/stagnation/mod.rs`)에는 `enabled`와 `lateral.enabled`만 존재한다. 나머지 키를 추가하려면 daemon의 하드코딩된 `StagnationDetector`/`SpinningDetector` 생성 코드를 설정 기반으로 바꿔야 한다.

### 원래 수용 기준 체크리스트 (계획 당시 작성, 구현 상태 미반영)

Similarity (Composite Pattern):
- [ ] `SimilarityJudge` trait이 단일 인터페이스로 유사도를 제공한다
- [ ] `CompositeSimilarity`가 `SimilarityJudge`를 구현하여 중첩 가능하다
- [ ] `StagnationDetector`는 `Box<dyn SimilarityJudge>` 하나만 의존한다
- [ ] yaml의 `similarity` 설정으로 judge 구성을 변경할 수 있다
- [ ] 기본 프리셋(exact_hash + token_fp + ncd)이 설정 생략 시 적용된다

Detection (4 Patterns):
- [ ] outputs에서 최근 `spinning_threshold`개(기본 3)가 유사(composite score ≥ `similarity_threshold`, 기본 0.8)하면 SPINNING이 감지된다
- [ ] errors에서 최근 `spinning_threshold`개(기본 3)가 유사하면 SPINNING이 감지된다 (별도 검사)
- [ ] 최근 `oscillation_cycles * 2`개(기본 4) outputs이 짝수/홀수 교대 패턴이면 OSCILLATION이 감지된다
- [ ] drift score 변화량이 epsilon 미만이면 NO_DRIFT가 감지된다
- [ ] 개선폭이 threshold 미만이면 DIMINISHING_RETURNS가 감지된다
- [ ] stagnation.enabled=false이면 탐지를 수행하지 않는다

Lateral Thinking:
- [ ] 패턴 감지 시 패턴 친화도 순으로 페르소나가 선택된다
- [ ] 이전에 시도한 페르소나는 제외된다
- [ ] 선택된 페르소나의 내장 prompt로 `belt agent -p`를 호출하여 lateral_plan을 생성한다
- [ ] lateral_plan이 retry 시 handler prompt에 추가 컨텍스트로 주입된다
- [ ] hitl 도달 시 모든 lateral 시도 이력이 hitl_notes에 첨부된다
- [ ] lateral.enabled=false이면 lateral plan 없이 기존 escalation만 적용된다
- [ ] lateral.max_attempts를 초과하면 더 이상 페르소나를 시도하지 않는다

이벤트:
- [ ] 탐지 이벤트가 transition_events에 event_type='stagnation'으로 기록된다
- [ ] evidence에 각 judge별 score가 포함된다
- [ ] lateral plan과 페르소나 정보가 event detail에 포함된다

현재 구현 기준으로 이 체크리스트를 다시 검증하려면 [Stagnation Detection](../spec/concerns/stagnation.md)의 "core 구현 완료, daemon 미배선" 절을 참조한다 — 다수 항목이 미충족 상태다.
