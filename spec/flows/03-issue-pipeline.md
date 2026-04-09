# Flow 3: 이슈 파이프라인 — 컨베이어 벨트

> 이슈가 DataSource의 상태 정의에 따라 자동으로 처리되고, Done이 다음 단계를 트리거한다.

---

## 컨베이어 벨트 흐름

```
belt:analyze 감지 → [analyze handlers] → evaluate → on_done script → belt:implement 부착
                                                                              │
belt:implement 감지 → [implement handlers] → evaluate → on_done script → belt:review 부착
                                                                              │
belt:review 감지 → [review handlers] → evaluate → on_done script → belt:done 부착
```

각 구간은 독립적인 QueueItem. 되돌아가지 않고, 항상 새 아이템으로 다음 구간에 진입.

---

## 단일 구간 상세

```
DataSource.collect(): trigger 조건 매칭 (예: belt:analyze 라벨)
    │
    ▼
  Pending → Ready → Running (자동 전이, concurrency 제한)
    │
    │  ① worktree 생성 (인프라, 또는 retry 시 기존 보존분 재사용)
    │  ② hook.on_enter() 트리거 (workspace의 LifecycleHook)
    │  ③ handlers 순차 실행:
    │       prompt → AgentRuntime.invoke() (worktree 안에서)
    │       script → bash (WORK_ID + WORKTREE 주입)
    │
    ├── 전부 성공 → Completed
    │     │
    │     ▼
    │   evaluate (per-item, concurrency slot 소비):
    │   "이 아이템의 결과가 충분한가?"
    │     ├── Done → hook.on_done() 트리거 → worktree 정리
    │     │           └── hook 실패 → Failed (로그 기록)
    │     └── HITL → HITL 이벤트 생성 → 사람 대기 (worktree 보존)
    │
    └── 실패 (handler 또는 on_enter)
          │
          ▼
        Stagnation 분석 (항상 실행):
          이전 시도와 유사한 패턴인가? (반복, 교대 등)
          패턴 감지 시 → 다른 접근법으로 전환하여 재시도
          │
          ▼
        Escalation (실패 횟수 기반):
          ├── 1회 → 조용히 재시도 (대안 접근법 주입)
          ├── 2회 → 외부 시스템에 알림 + 재시도
          └── 3회 → 사람에게 전달 (lateral report 첨부)
                       └── 사람: done/retry/skip/replan
                       └── timeout → terminal (skip 또는 replan)
        상세: [실패 복구와 HITL](./04-failure-and-hitl.md)
```

---

## on_done script 예시

on_done script는 `belt context`로 필요한 정보를 조회하여 외부 시스템에 결과를 반영한다.

```yaml
on_done:
  - script: |
      CTX=$(belt context $WORK_ID --json)
      ISSUE=$(echo $CTX | jq -r '.source_data.issue.number // .issue.number')
      REPO=$(echo $CTX | jq -r '.source.url')
      TITLE=$(echo $CTX | jq -r '.source_data.issue.title // .issue.title')
      gh pr create --title "$TITLE" --body "Closes #$ISSUE" -R $REPO
      gh issue edit $ISSUE --remove-label "belt:implement" -R $REPO
      gh issue edit $ISSUE --add-label "belt:review" -R $REPO
```

Daemon이 주입하는 환경변수는 `WORK_ID`와 `WORKTREE`뿐. 이슈 번호, 레포 URL 등은 `belt context`로 직접 조회한다.

---

## 피드백 루프

### PR review comment (changes-requested)

```
DataSource.collect()가 changes-requested 감지
  → 새 아이템 생성 → handlers 실행 → 수정 반영
```

### /spec update

```
스펙 변경 → on_spec_active → Cron(gap-detection) 재평가
  → gap 발견 시 새 이슈 생성 → 파이프라인 재진입
```

### 핵심 원칙

**스펙 = 계약**. 계약이 바뀌어야 하면 `/spec update`. 계약 범위 내 작업이면 이슈 등록.

---

## 인프라 오류와 Circuit Breaker

handler 로직 실패(stagnation)와 인프라 오류는 다른 문제이다.

| 구분 | 예시 | 대응 |
|------|------|------|
| **handler 실패** | 컴파일 에러, 테스트 실패 | stagnation 분석 → lateral thinking → escalation |
| **인프라 오류** | GitHub API 장애, 디스크 부족, worktree 깨짐 | circuit breaker → 일시 중단 → 자동 복구 |

### 인프라 오류 발생 시

```
인프라 오류 감지 (GitHub API 5xx, worktree 생성 실패 등)
  │
  ├── dashboard에 오류 상태 표시
  │     "⚠ github:org/repo — GitHub API 503 (3/5 failures)"
  │
  ├── backoff retry (1s → 2s → 4s → ...)
  │
  └── Circuit Breaker:
        closed (정상)
          → 3회 연속 인프라 오류 → open
        open (중단)
          → 해당 source/작업 일시 중단
          → dashboard에 "🔴 circuit open" 표시
          → cooldown 후 half-open
        half-open (시험)
          → 1건 시험 실행
          → 성공 → closed (정상 복귀)
          → 실패 → open (다시 중단)
```

circuit breaker는 source 단위로 동작한다. 한 source의 인프라 오류가 다른 workspace의 작업에 영향을 주지 않는다.

---

## 검증 시나리오

| 시나리오 | 입력 | 기대 최종 phase | 기대 side effect |
|---------|------|----------------|-----------------|
| 정상 완주 | 이슈 + belt:analyze | Done | analyze→implement→review 순서 처리, PR 생성 |
| handler 전부 성공 + evaluate Done | handler 성공 | Done | hook.on_done() 트리거, worktree 정리 |
| handler 성공 + evaluate HITL | handler 성공, LLM 불확실 | HITL | HITL 이벤트 생성, worktree 보존 |
| hook.on_done() 실패 | on_done script exit 1 | Failed | on_fail은 실행하지 않음 (handler 실패가 아니므로) |
| changes-requested | PR 리뷰 코멘트 | 새 아이템 Pending | DataSource.collect()가 감지, 수정 반영 |
| circuit breaker open | 3회 연속 인프라 오류 | 해당 source 일시 중단 | dashboard에 circuit open 표시 |
| circuit breaker 복구 | half-open에서 1건 성공 | closed (정상 복귀) | 해당 source 재개 |

---

### 관련 문서

- [DataSource](../concerns/datasource.md) — 상태 기반 워크플로우 + context 스키마
- [LifecycleHook](../concerns/lifecycle-hook.md) — 상태 전이 반응
- [실패 복구와 HITL](./04-failure-and-hitl.md) — escalation 정책
- [Stagnation Detection](../concerns/stagnation.md) — 실패 패턴 감지
- [Evaluator](../concerns/evaluator.md) — Progressive Evaluation Pipeline
