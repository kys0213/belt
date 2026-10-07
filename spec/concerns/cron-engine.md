# Cron 엔진 — 주기 실행

> 주기적으로 실행되는 작업을 관리한다.
> 주기 작업(인프라 유지, 지식 추출, 사용자 정의)을 실행한다.

---

## 두 가지 역할

```
1. 인프라 유지 — hitl-timeout, log-cleanup, daily-report (결정적)
2. 지식 추출 — knowledge-extract (LLM 사용)

※ evaluate는 Daemon tick 루프의 정규 단계로 이동. 상세: [Evaluator](./evaluator.md)
```

---

## 기본 Cron Jobs

### 인프라 (Global, 결정적, 토큰 0)

| Job | 주기 | 동작 |
|-----|------|------|
| hitl-timeout | 5분 | 기한이 지난 open HITL 요청을 만료 경합에 올림 (후처리는 daemon tick) |
| daily-report | 매일 06시 | 일간 리포트 |
| log-cleanup | 매일 00시 | 보존 worktree(TTL 초과)와 daemon 로그 정리. 전이 이력·HITL 기록은 지우지 않는다 |

### 지식 추출 (Per-workspace, LLM 사용)

| Job | 주기 | 동작 |
|-----|------|------|
| knowledge-extract | 1시간 | merged PR 지식 추출 |

### 사용자 정의 (예시)

| Job | 주기 | 동작 |
|-----|------|------|
| qa-test | 30분 | 테스트 실행, 실패 시 이슈 생성 |
| security-scan | 2시간 | 보안 취약점 스캔 |

---

## hitl-timeout과 첫 응답 승리

hitl-timeout은 기한이 지난 open HITL 요청을 **만료(expire) 시도**로 HitlService에 올린다. 사람의 응답과 같은 경합에 참여하고, 먼저 확정된 쪽이 이긴다.

```mermaid
sequenceDiagram
    participant C as hitl-timeout cron
    participant U as CLI TUI 외부 channel
    participant DB as SQLite HITL 요청
    participant D as daemon tick

    par 응답과 만료의 경합
        U->>DB: 응답 확정 시도
    and
        C->>DB: 만료 시도
    end
    DB-->>U: 승리 또는 already_handled
    DB-->>C: 승리 또는 already_handled
    Note over DB: 먼저 확정된 쪽만 이긴다. 요청은 resolved 또는 expired
    D->>DB: 후처리 대상 조회 resolved 또는 expired 이면서 미완료
    D->>DB: terminal action 후처리 후 결과 전이
```

- 만료도 응답과 마찬가지로 한 번만 확정된다. 응답이 먼저면 만료 시도는 `already_handled`로 끝나고 아이템에 영향이 없다.
- cron은 phase를 직접 바꾸지 않는다. 만료된 요청의 terminal action 적용(worktree 정리 포함)과 결과 전이는 daemon tick의 후처리가 한다. 상세: [Daemon](./daemon.md#hitl-해결-후처리)
- 만료 시점부터 후처리가 끝날 때까지 아이템은 처리 중이고, 다른 경로의 전이는 `busy`다. 상세: [QueuePhase 상태 머신](./queue-state-machine.md#처리-중-잠금)
- 경합 규칙의 전체는 [Notification](./notification.md#첫-응답-승리)을 따른다.

---

## Force Trigger

force_trigger는 cron job을 다음 tick에서 우선 실행하도록 스케줄링한다.

```
force_trigger(job_name):
  job.last_run_at = NULL → 다음 tick에서 즉시 실행
```

- 동기적으로 실행하지 않는다. cron의 `last_run_at`을 리셋할 뿐.
- knowledge-extract나 사용자 정의 job에 사용.

> **evaluate는 cron job이 아니라 Daemon tick의 정규 단계다**. Completed 아이템은 Evaluator가 다음 tick에서 Progressive Pipeline으로 판정한다. 상세: [Evaluator](./evaluator.md)

---

## Daemon 주입 환경변수 (Cron 전용)

Cron 스크립트에는 workspace 정보가 필요하므로 추가 변수를 주입한다.

| 변수 | 예시 |
|------|------|
| `WORKSPACE` | `auth-project` |
| `BELT_HOME` | `~/.belt` |
| `BELT_DB` | `~/.belt/belt.db` |

> **참고**: handler/on_done/on_fail script에는 `WORK_ID` + `WORKTREE`만 주입된다. Cron은 아이템 단위가 아니라 workspace 단위로 실행되므로 다른 환경변수 세트를 사용한다.

---

## Built-in vs Custom

| | Built-in | Custom |
|---|---|---|
| 생성 | workspace 등록 시 자동 | `belt cron add` |
| 제거 | 불가 (pause/resume) | 자유 |
| Guard | 내장 | 사용자 정의 |

---

## 수용 기준

- [ ] hitl-timeout은 HITL 요청을 만료 경합에 올리고, 응답과 만료 중 먼저 확정된 쪽만 이긴다
- [ ] 응답이 먼저 확정되면 만료 시도는 `already_handled`로 끝나고 아이템에 영향이 없다
- [ ] hitl-timeout은 phase를 직접 바꾸지 않고, 후처리와 결과 전이는 daemon tick이 수행한다
- [ ] 만료 후 후처리 중에는 외부 전이가 `busy`다

---

### 관련 문서

- [DESIGN](../DESIGN.md) — evaluate 아키텍처
- [DataSource](./datasource.md) — belt context 스키마
- [Daemon](./daemon.md) — HITL 후처리
- [QueuePhase 상태 머신](./queue-state-machine.md) — 처리 중 잠금
- [Agent](./agent-workspace.md) — evaluate와 Agent의 관계
- [Stagnation Detection](./stagnation.md) — handler 실패 시 패턴 감지 (cron이 아닌 동기 실행)
