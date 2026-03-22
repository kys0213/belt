# Flow 4: 실패 복구와 HITL

> handler 실행 실패 시 escalation 정책에 따라 복구하고, evaluate가 HITL로 분류하면 사람의 판단을 요청한다.

---

## 실패 경로

```
handler 또는 on_enter 실행 실패
    │
    ▼
escalation 정책 적용 (failure_count 기반, history에서 계산):
    │
    ├── retry             → 조용히 재시도 (on_fail 실행 안 함, worktree 보존)
    ├── retry_with_comment → on_fail script 실행 + 재시도 (worktree 보존)
    └── hitl              → on_fail script 실행 + HITL 이벤트 생성 (worktree 보존)
                              └── 사람 응답: done / retry / skip / replan
                              └── timeout → terminal 액션 적용 (설정에 따라 skip 또는 replan)
```

`retry`만 on_fail을 실행하지 않는다. "조용한 재시도"로 외부 시스템에 노이즈를 주지 않는다.

> **skip/replan**: 독립적인 escalation level이 아니라 hitl의 응답 경로 또는 hitl timeout 시의 `terminal` 설정이다. skip은 terminal 상태(Skipped)이므로, 이후 실패가 발생할 수 없어 순차적 escalation level로 정의할 수 없다.

---

## Escalation 정책 (workspace yaml 소유)

```yaml
sources:
  github:
    escalation:
      1: retry
      2: retry_with_comment
      3: hitl
      terminal: skip          # hitl timeout 시 적용 (skip 또는 replan)
```

DataSource마다 다른 정책이 가능한 구조. v5는 GitHub 정책만 구현.

### on_fail script 예시

```yaml
on_fail:
  - script: |
      CTX=$(autodev context $WORK_ID --json)
      ISSUE=$(echo $CTX | jq -r '.issue.number')
      REPO=$(echo $CTX | jq -r '.source.url')
      FAILURES=$(echo $CTX | jq '[.history[] | select(.status=="failed")] | length')
      gh issue comment $ISSUE --body "실패 (시도 횟수: $FAILURES)" -R $REPO
```

failure_count는 별도 컬럼이 아니라 history의 append-only 이벤트에서 계산.

---

## HITL (Human-in-the-Loop)

### 생성 경로

| 경로 | 트리거 |
|------|--------|
| Escalation | handler 또는 on_enter 실패 → escalation 정책이 HITL 결정 |
| evaluate | handler 성공 → evaluate가 "사람이 봐야 한다" 판단 |
| 스펙 완료 | 모든 linked issues Done → 최종 확인 요청 |
| 충돌 | DependencyGuard가 스펙 충돌 감지 |

> **DependencyGuard**: 다중 스펙 등록 시 Claw가 기존 Active 스펙과의 충돌/의존성을 분석하여 등록하는 가드. 같은 파일/모듈에 영향을 주는 스펙이 동시에 진행될 때 HITL 이벤트를 생성한다. 상세는 [스펙 생명주기](./02-spec-lifecycle.md)의 "다중 스펙 우선순위" 참조.

### 응답 경로

```
사용자 응답 (TUI / CLI / /claw 세션)
  → autodev hitl respond <id> --choice N
  → 라우팅:
      "done"     → on_done script 실행
                     ├── script 성공 → Done (worktree 정리)
                     └── script 실패 → Failed (worktree 보존, 로그 기록)
      "retry"    → 새 아이템 생성 → Pending (worktree 보존)
      "skip"     → Skipped (worktree 정리)
      "replan"   → Claw에게 스펙 수정 제안 위임
```

### 타임아웃

```
기본: 24시간
초과 시: hitl-timeout cron이 처리
  → 설정에 따라: remind / skip / pause_spec
```

---

## Graceful Shutdown

```
SIGINT → on_shutdown:
  1. Running 아이템 완료 대기 (timeout: 30초)
     → timeout 초과: Pending으로 롤백, worktree 보존
       (재시작 후 기존 worktree를 재사용하여 이어서 진행)
  2. Cron engine 정지
```

---

### 관련 문서

- [DataSource](../concerns/datasource.md) — escalation 정책 + on_fail script
- [Claw](../concerns/claw-workspace.md) — 대화형 에이전트 (HITL 응답 경로 포함)
- [이슈 파이프라인](./03-issue-pipeline.md) — 실패가 발생하는 실행 흐름
