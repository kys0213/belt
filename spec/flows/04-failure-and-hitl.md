# Flow 4: 실패 복구와 HITL

> handler 실패 시 stagnation 분석 + lateral thinking으로 사고를 전환하여 재시도하고, evaluate가 HITL로 분류하면 사람의 판단을 요청한다.

---

## 실패 경로

```
handler 또는 hook.on_enter() 실패
    │
    ▼
Stagnation 분석 (항상 실행):
  이전 시도 기록을 분석하여 정체 패턴을 감지:
    │
    ├── 같은 실패 반복 (SPINNING)
    ├── A↔B 교대 반복 (OSCILLATION)
    └── (Phase 2) 진행 정체, 개선폭 감소
    │
    ├── 패턴 없음 ─────── escalation 적용 (기존 방식으로 재시도)
    │
    └── 패턴 감지 ─┐
                    ▼
  사고 전환 (Lateral Thinking):
     패턴에 맞는 다른 접근법을 선택 (이전 시도와 중복 없이)
     예: 반복 실패 → 워크어라운드 시도, 교대 반복 → 구조 재설계
     → 대안 접근 계획(lateral plan) 생성
                    │
                    ▼
  Escalation (실패 횟수 기반, lateral plan 포함):
    │
    ├── retry             → hook.on_escalation(retry), lateral_plan 주입, 재시도 (worktree 보존)
    ├── retry_with_comment → hook.on_escalation + hook.on_fail, lateral_plan 주입, 재시도
    └── hitl              → hook.on_escalation + hook.on_fail, lateral_report 첨부, HITL 이벤트
                              └── 사람 응답: done / retry / skip / replan
                              └── timeout → terminal 액션 (skip 또는 replan)
```

`retry`만 on_fail hook을 트리거하지 않는다. "조용한 재시도"로 외부 시스템에 노이즈를 주지 않는다.
Daemon은 hook을 트리거만 하고, 실행 책임은 workspace의 LifecycleHook impl이 가진다.

---

## Lateral Plan 주입 예시

retry로 생성된 새 아이템이 다시 Running에 진입하면, lateral_plan이 handler prompt에 추가 컨텍스트로 주입된다:

```
원래 handler prompt:
  "이슈를 구현해줘"

lateral retry 시 합성:
  "이슈를 구현해줘

   ⚠ Stagnation Analysis (attempt 2/3)
   Pattern: SPINNING | Persona: HACKER

   실패 원인: 이전 2회 시도에서 동일한 컴파일 에러 반복
     error[E0433]: cannot find type Session in auth::middleware
   대안 접근법: 기존 Session 직접 구현 대신 tower-sessions crate 활용
   실행 계획:
     1. Cargo.toml에 tower-sessions 추가
     2. Session 타입 참조를 교체
     3. middleware에 SessionManagerLayer 등록
   주의: 이전과 동일한 접근은 같은 실패를 반복합니다"
```

---

## Escalation 정책 (workspace yaml 소유)

```yaml
sources:
  github:
    escalation:
      1: retry
      2: retry_with_comment
      3: hitl
      terminal: skip          # hitl timeout 시 (skip 또는 replan)

stagnation:
  enabled: true
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

- escalation 레벨은 기존과 동일 (failure_count 기반)
- stagnation + lateral은 **모든 retry의 품질을 투명하게 높이는 내장 레이어**
- `stagnation.enabled: false`이면 lateral 없이 기존 v5 동작

### on_fail script 예시

```yaml
on_fail:
  - script: |
      CTX=$(belt context $WORK_ID --json)
      ISSUE=$(echo $CTX | jq -r '.source_data.issue.number // .issue.number')
      REPO=$(echo $CTX | jq -r '.source.url')
      FAILURES=$(echo $CTX | jq '[.history[] | select(.status=="failed")] | length')
      gh issue comment $ISSUE --body "실패 (시도 횟수: $FAILURES)" -R $REPO
```

---

## HITL (Human-in-the-Loop)

### 생성 경로

| 경로 | 트리거 |
|------|--------|
| Escalation | handler/on_enter 실패 → failure_count=3 → hitl |
| evaluate | handler 성공 → evaluate가 "사람이 봐야 한다" 판단 |
| 스펙 완료 | 모든 linked issues Done → 최종 확인 요청 |
| 충돌 | DependencyGuard가 스펙 충돌 감지 |

### HITL 진입 — LLM이 질문을 구성

HITL에 진입하면 LLM이 상황(lateral report, 이력, HITL 경로)을 분석하여 **맥락에 맞는 질문과 추천 선택지**를 구성한다. 고정된 4개 선택지가 아니라, 상황별로 다른 제안이 나온다.

#### 예시: Escalation HITL (handler 3회 실패)

```
"JWT middleware 구현이 3회 실패했습니다.

 시도 이력:
   1회: compile error (Session not found)
   2회: tower-sessions 시도 → 다른 에러 발생
   3회: trait object 시도 → 컴파일 성공, 테스트 실패

 추천:
   1. axum-sessions crate로 전환하여 재시도
   2. Session 관련 코드를 별도 이슈로 분리
   3. 현재 결과로 PR 생성 (부분 완료)
   4. 이 이슈 건너뛰기
   또는 직접 지시를 입력하세요"
```

#### 예시: Evaluate HITL (완료 여부 불확실)

```
"JWT middleware 구현 결과를 검토했으나 확신이 부족합니다.

 현재 상태:
   - 컴파일 성공, 테스트 18/20 통과
   - 실패 테스트: session expiry, concurrent access

 추천:
   1. 실패 테스트 2건을 수정하여 재시도
   2. 현재 상태로 PR 생성 (실패 테스트는 후속 이슈로)
   3. 전체 접근 방식을 재검토
   또는 직접 지시를 입력하세요"
```

#### 예시: Spec 완료 HITL

```
"스펙 'JWT 인증 시스템'의 모든 이슈가 완료되었습니다.

 완료된 이슈: #42 middleware, #43 token 발급, #44 refresh
 gap-detection: 추가 gap 미발견
 테스트 커버리지: 87%

 추천:
   1. 스펙 완료 승인
   2. 추가 검증 항목 지정하여 재검토
   또는 직접 지시를 입력하세요"
```

### 응답 처리

사용자는 번호 선택 또는 자연어로 응답한다. LLM이 응답을 해석하여 시스템 액션으로 변환한다.

```
사용자 응답 (TUI / CLI / /agent 세션)
  │
  ▼
LLM이 응답 해석 → 시스템 액션으로 변환:
  │
  ├── Done(plan)   → hook.on_done() 트리거 → worktree 정리
  ├── Retry(plan)  → 사용자 지시를 lateral_plan으로 주입
  │                  새 아이템 생성 → Pending (worktree 보존)
  ├── Skip         → Skipped (worktree 정리)
  └── Replan       → 스펙 수정 제안 (아래 참조)
```

### Replan

```
replan (사용자 응답 또는 hitl timeout)
  → replan_count 증가 (max 3)
  → max 초과 시: Skipped (worktree 정리)
  → max 이내:
      1. Agent가 실패 컨텍스트 + lateral report를 분석하여 스펙 수정 제안
      2. 사용자가 /spec update로 스펙 수정 → 새 이슈 생성 → 파이프라인 재진입
```

### 타임아웃

```
기본: 24시간
초과 시: hitl-timeout cron (5분 주기)이 감지
  → escalation.terminal 설정에 따라:
      skip   → Skipped (worktree 정리)
      replan → replan 처리 (위 참조)
```

---

## Graceful Shutdown

```
SIGINT → on_shutdown:
  1. Running 아이템 완료 대기 (timeout: 30초)
     → timeout 초과: Pending으로 롤백, worktree 보존
  2. Cron engine 정지
```

---

## 검증 시나리오

| 시나리오 | 입력 | 기대 최종 phase | 기대 side effect |
|---------|------|----------------|-----------------|
| 1회 실패 | handler 실패 (failure_count=1) | 새 아이템 Pending | retry, on_fail 미실행, lateral plan 주입 |
| 2회 실패 | handler 실패 (failure_count=2) | 새 아이템 Pending | retry_with_comment, on_fail 실행, lateral plan 주입 |
| 3회 실패 | handler 실패 (failure_count=3) | HITL | hitl, on_fail 실행, lateral report 첨부 |
| SPINNING 감지 | 3회 연속 유사 출력 (score ≥ 0.8) | escalation에 따름 | StagnationDetector SPINNING, lateral plan에 대안 접근법 |
| HITL done 응답 | 사용자 done 선택 | Done | hook.on_done() 트리거, worktree 정리 |
| HITL retry 응답 | 사용자 retry + 지시 | 새 아이템 Pending | 사용자 지시를 lateral_plan으로 주입, worktree 보존 |
| HITL skip 응답 | 사용자 skip 선택 | Skipped (terminal) | worktree 정리 |
| HITL replan 응답 | 사용자 replan 선택 | replan_count 증가 | 스펙 수정 제안, max(3) 초과 시 Skipped |
| HITL timeout | 24시간 무응답 | terminal 액션 적용 | skip→Skipped, replan→replan 처리 |
| graceful shutdown | SIGINT + Running 아이템 | 완료 시 정상 처리, 30초 초과 시 Pending | worktree 보존, cron engine 정지 |
| on_enter 실패 | hook.on_enter() 에러 | escalation 경로 진입 | handler 건너뜀, failure_count 포함 |

---

### 관련 문서

- [Stagnation Detection](../concerns/stagnation.md) — 패턴 탐지 + Lateral Thinking
- [Daemon](../concerns/daemon.md) — handle_failure + hook 트리거
- [LifecycleHook](../concerns/lifecycle-hook.md) — 상태 전이 반응
- [Evaluator](../concerns/evaluator.md) — Progressive Evaluation Pipeline
- [Agent](../concerns/agent-workspace.md) — 대화형 에이전트 (HITL 질문 구성)
- [이슈 파이프라인](./03-issue-pipeline.md) — 실패가 발생하는 실행 흐름
- [Data Model](../concerns/data-model.md) — HitlReason, EscalationAction enum
