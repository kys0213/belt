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
        Stagnation Analyzer (항상 실행):
          CompositeSimilarity로 outputs/errors 유사도 분석
          패턴 감지 시 → LateralAnalyzer가 페르소나로 대안 분석
          → lateral_plan 생성
          │
          ▼
        Escalation (failure_count 기반, lateral_plan 포함):
          ├── retry             → lateral_plan 주입, 재시도, worktree 보존
          ├── retry_with_comment → lateral_plan 주입, on_fail + 재시도
          └── hitl              → lateral_report 첨부, on_fail + HITL 생성
                                    └── 사람: done/retry/skip/replan
                                    └── timeout → terminal (skip 또는 replan)
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

### 관련 문서

- [DataSource](../concerns/datasource.md) — 상태 기반 워크플로우 + context 스키마
- [실패 복구와 HITL](./04-failure-and-hitl.md) — escalation 정책
- [Stagnation Detection](../concerns/stagnation.md) — 실패 패턴 감지
- [Cron 엔진](../concerns/cron-engine.md) — evaluate cron + 품질 루프
