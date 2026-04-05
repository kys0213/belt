# Flow 4: 실패 복구와 HITL

> handler 실패 시 stagnation 분석 + lateral thinking으로 사고를 전환하여 재시도하고, evaluate가 HITL로 분류하면 사람의 판단을 요청한다.

---

## 실패 경로

```
handler 또는 on_enter 실행 실패
    │
    ▼
Stagnation Analyzer (항상 실행):
  ① ExecutionHistory 구성
     outputs = DB history.summary (별도)
     errors  = DB history.error (별도)
    │
    ▼
  ② StagnationDetector.detect
     CompositeSimilarity (ExactHash + TokenFingerprint + NCD)
     outputs → SPINNING? OSCILLATION?
     errors  → SPINNING? (별도 검사)
     drifts  → NO_DRIFT? DIMINISHING?
    │
    ├── 패턴 없음 ────────────────────── escalation 적용 (lateral 없이)
    │
    └── 패턴 감지 ─┐
                    ▼
  ③ LateralAnalyzer
     패턴 → 페르소나 선택 (이전 시도 제외)
       SPINNING    → HACKER
       OSCILLATION → ARCHITECT
       NO_DRIFT    → RESEARCHER
       DIMINISHING → SIMPLIFIER
       복합        → CONTRARIAN
     belt agent -p → lateral_plan 생성
                    │
                    ▼
  ④ Escalation (failure_count 기반, lateral_plan 포함):
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

### HITL에 lateral report 첨부

hitl에 도달하면 지금까지의 모든 lateral 시도 이력이 `hitl_notes`에 첨부된다:

```
HITL Event:
  work_id: github:org/repo#42:implement
  reason: retry_max_exceeded
  hitl_notes:
    "Stagnation Report:
     pattern: SPINNING (3회 유사 에러 — CompositeSimilarity score: 0.92)

     attempt 1: compile error (Session not found)
     attempt 2: HACKER 제안 → tower-sessions 시도 → 다른 에러 발생
     attempt 3: CONTRARIAN 제안 → trait object 시도 → 컴파일 성공, 테스트 실패

     2회 접근 전환 후에도 미해결. 구조적 문제일 가능성."
```

사람이 lateral report를 참고하여 더 정확한 판단을 내릴 수 있다.

### 응답 경로

```
사용자 응답 (TUI / CLI / /agent 세션)
  → belt hitl respond <id> --choice N
  → 라우팅:
      "done"     → on_done script 실행
                     ├── script 성공 → Done (worktree 정리)
                     └── script 실패 → Failed (worktree 보존, 로그 기록)
      "retry"    → 새 아이템 생성 → Pending (worktree 보존)
      "skip"     → Skipped (worktree 정리)
      "replan"   → replan 처리 (아래 참조)
```

### Replan

```
replan 요청 (사람 응답 또는 hitl timeout)
  → replan_count 증가 (max 3)
  → max 초과 시: Skipped (worktree 정리)
  → max 이내:
      1. HitlReason::SpecModificationProposed 이벤트 생성
      2. Failed 전이 (worktree 보존)
      3. Agent가 실패 컨텍스트 + lateral report를 분석하여 스펙 수정 제안
      4. 사용자가 /spec update로 스펙 수정 → 새 이슈 생성 → 파이프라인 재진입
```

### 타임아웃

```
기본: 24시간
초과 시: hitl-timeout cron (5분 주기)이 감지
  → escalation.terminal 설정에 따라:
      skip   → Skipped (worktree 정리)
      replan → replan 처리 (위 참조)
```

`hitl_terminal_action`은 `EscalationAction` enum 타입 (#720).

---

## Graceful Shutdown

```
SIGINT → on_shutdown:
  1. Running 아이템 완료 대기 (timeout: 30초)
     → timeout 초과: Pending으로 롤백, worktree 보존
  2. Cron engine 정지
```

---

### 관련 문서

- [Stagnation Detection](../concerns/stagnation.md) — Composite Similarity + Lateral Thinking
- [Daemon](../concerns/daemon.md) — Executor.handle_failure 상세
- [DataSource](../concerns/datasource.md) — escalation 정책 + on_fail script
- [Agent](../concerns/agent-workspace.md) — 대화형 에이전트 (HITL 응답 경로 포함)
- [이슈 파이프라인](./03-issue-pipeline.md) — 실패가 발생하는 실행 흐름
- [Data Model](../concerns/data-model.md) — HitlReason, EscalationAction, Persona enum
