# Flow 3: 이슈 파이프라인 — 컨베이어 벨트

> 이슈가 DataSource의 상태 정의에 따라 자동으로 처리되고, Done이 다음 단계를 트리거한다.

---

## 컨베이어 벨트 흐름

```
autodev:analyze 감지 → [analyze handlers] → evaluate → on_done script → autodev:implement 부착
                                                                              │
autodev:implement 감지 → [implement handlers] → evaluate → on_done script → autodev:review 부착
                                                                              │
autodev:review 감지 → [review handlers] → evaluate → on_done script → autodev:done 부착
```

각 구간은 독립적인 QueueItem. 되돌아가지 않고, 항상 새 아이템으로 다음 구간에 진입.

---

## 단일 구간 상세

```
DataSource.collect(): trigger 조건 매칭 (예: autodev:analyze 라벨)
    │
    ▼
  Pending → Ready → Running (자동 전이, concurrency 제한)
    │
    │  ① worktree 생성 (인프라, 또는 retry 시 기존 보존분 재사용)
    │  ② on_enter script 실행 (정의된 경우)
    │  ③ handlers 순차 실행:
    │       prompt → AgentRuntime.invoke() (worktree 안에서)
    │       script → bash (WORK_ID + WORKTREE 주입)
    │
    ├── 전부 성공 → Completed
    │     │
    │     ▼
    │   evaluate cron (force_trigger로 즉시 실행 + 주기 폴링 하이브리드):
    │   "완료? 추가 검토?"
    │     ├── Done → on_done script 실행 → worktree 정리
    │     │           └── script 실패 → Failed (로그 기록, 재시도 가능)
    │     └── HITL → HITL 이벤트 생성 → 사람 대기 (worktree 보존)
    │
    └── 실패 (handler 또는 on_enter) → escalation 정책 적용
          ├── retry             → 새 아이템 생성, worktree 보존
          ├── retry_with_comment → on_fail + 새 아이템 생성
          └── hitl              → on_fail + HITL 생성
                                    └── 사람: done/retry/skip/replan
                                    └── timeout → terminal (skip 또는 replan)
```

---

## on_done script 예시

on_done script는 `autodev context`로 필요한 정보를 조회하여 외부 시스템에 결과를 반영한다.

```yaml
on_done:
  - script: |
      CTX=$(autodev context $WORK_ID --json)
      ISSUE=$(echo $CTX | jq -r '.issue.number')
      REPO=$(echo $CTX | jq -r '.source.url')
      TITLE=$(echo $CTX | jq -r '.issue.title')
      gh pr create --title "$TITLE" --body "Closes #$ISSUE" -R $REPO
      gh issue edit $ISSUE --remove-label "autodev:implement" -R $REPO
      gh issue edit $ISSUE --add-label "autodev:review" -R $REPO
```

Daemon이 주입하는 환경변수는 `WORK_ID`와 `WORKTREE`뿐. 이슈 번호, 레포 URL 등은 `autodev context`로 직접 조회한다.

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
- [Cron 엔진](../concerns/cron-engine.md) — evaluate cron + 품질 루프
