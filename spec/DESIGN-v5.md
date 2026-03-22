# DESIGN v5

> **Date**: 2026-03-22
> **Status**: Draft (reviewed)
> **기준**: v4 운영 피드백 + 열린 이슈 + 설계 논의 반영

---

## 목표

Daemon을 **상태 머신 + 정의된 시점에 prompt/script를 호출하는 실행기**로 단순화한다.
DataSource가 자기 시스템의 언어로 워크플로우를 정의하되, 코어는 큐만 돌린다.

```
Daemon이 아는 것   = 큐 상태 머신 + yaml에 정의된 prompt/script 실행
DataSource가 정의  = 수집 조건(trigger), 처리(handlers), 결과 반영(on_done/on_fail script)
evaluate가 판단    = handler 결과가 충분한지, 사람이 봐야 하는지 (Done or HITL)
```

---

## 설계 철학

### 1. 컨베이어 벨트

아이템은 한 방향으로 흐른다. 되돌아가지 않는다. 부족하면 Cron이 새 아이템을 만들어서 다시 벨트에 태운다.

### 2. Workspace = 1 Repo

workspace는 하나의 외부 레포와 1:1로 대응한다. v4의 `repo` 개념을 리네이밍. GitHub 외 Jira, Slack 등도 지원하기 위한 추상화 (v5는 GitHub에 집중).

### 3. DataSource가 워크플로우를 소유

각 DataSource는 자기 시스템의 상태 표현으로 워크플로우를 정의한다. 코어는 DataSource 내부를 모른다. 상세: [DataSource](./concerns/datasource.md)

### 4. Daemon = 상태 머신 + 실행기

수집 → 전이 → 실행 → 분류 → 반영 → 스케줄. GitHub 라벨이 뭔지 모르고 yaml대로 실행할 뿐. 상세: [Daemon](./concerns/daemon.md)

### 5. 코드 작업은 항상 worktree

handler prompt는 항상 git worktree 안에서 실행. worktree 생성/정리는 인프라 레이어 담당. on_done script가 외부 시스템 반영 (gh CLI 등).

### 6. 코어가 출구에서 분류

evaluate가 Completed 아이템을 Done or HITL로 분류. LLM이 `autodev queue done/hitl` CLI를 직접 호출하여 상태 전이 (JSON 파싱 불필요).

### 7. 아이템 계보 (Lineage)

같은 외부 엔티티에서 파생된 아이템은 `source_id`로 연결. 모든 이벤트는 append-only history로 축적.

### 8. 환경변수 최소화

`WORK_ID` + `WORKTREE` 2개만 주입. 나머지는 `autodev context $WORK_ID --json`으로 조회. 상세: [DataSource](./concerns/datasource.md)

### 9. Concurrency 제어

workspace.concurrency (workspace yaml 루트) + daemon.max_concurrent 2단계. evaluate LLM 호출도 slot 소비. 상세: [Daemon](./concerns/daemon.md)

### 10. Cron은 품질 루프

파이프라인은 1회성, 품질은 Cron이 지속 감시. gap-detection이 새 이슈 생성 → 파이프라인 재진입. 상세: [Cron 엔진](./concerns/cron-engine.md)

### Claw는 대화형 에이전트

`/claw` 세션. 자연어로 큐 조회, HITL 응답, cron 관리. 상세: [Claw](./concerns/claw-workspace.md)

---

## QueuePhase

```
Pending → Ready → Running → Completed → Done | HITL | Failed | Skipped
```

| Phase | 설명 |
|-------|------|
| **Pending** | DataSource.collect()가 감지, 큐 대기 |
| **Ready** | 실행 준비 완료 (자동 전이) |
| **Running** | worktree 생성 + handler 실행 중 |
| **Completed** | handler 전부 성공, evaluate 대기 |
| **Done** | evaluate 완료 판정 + on_done script 성공 |
| **HITL** | evaluate가 사람 판단 필요로 분류 |
| **Skipped** | escalation skip 또는 preflight 실패 |
| **Failed** | on_done script 실패, 인프라 오류 등 |

상태 전이 다이어그램, worktree 생명주기, on_fail 실행 조건: [QueuePhase 상태 머신](./concerns/queue-state-machine.md)

---

## 전체 구조

```
┌──────────────────────────────────────────────────────────────┐
│  사용자                                                       │
│                                                               │
│  /auto          /spec          /claw          dashboard       │
└───┬──────────────┬──────────────┬──────────────┬──────────────┘
    │              │              │              │
    ▼              ▼              ▼              ▼
┌──────────────────────────────────────────────────────────────┐
│  autodev CLI (SSOT)                                          │
│                                                               │
│  autodev context $WORK_ID --json  ← script가 정보 조회       │
└──────────────────────────┬───────────────────────────────────┘
                           │
                           ▼
┌──────────────────────────────────────────────────────────────┐
│  Daemon (상태 머신 + 실행기)                                   │
│                                                               │
│  수집 → 전이 → 실행 → 완료 → 분류 → 반영 → 스케줄            │
└──────────────────────────┬───────────────────────────────────┘
                           │
              ┌────────────┼────────────┐
              ▼            ▼            ▼
┌──────────────┐  ┌──────────────┐  ┌──────────────┐
│  DataSource  │  │ AgentRuntime │  │  Cron Engine │
│              │  │              │  │              │
│  수집        │  │  LLM 실행    │  │  주기 작업    │
│  컨텍스트    │  │  추상화      │  │  품질 루프    │
│  조회        │  │              │  │              │
└──────────────┘  └──────────────┘  └──────────────┘
```

---

## 관심사 분리

| 레이어 | 책임 | 토큰 |
|--------|------|------|
| Daemon | 상태 머신 + yaml prompt/script 실행 + cron 스케줄링 | 0 |
| 인프라 | worktree 생성/정리 | 0 |
| DataSource | 수집(collect) + 컨텍스트 조회(context) | 0 |
| AgentRuntime | LLM 실행 추상화 | handler별 |
| evaluate | 완료/추가검토 분류 (Done or HITL), CLI 도구 호출 | 분류 시 |
| on_done/on_fail script | 외부 시스템에 결과 반영 | 0 |
| Claw | `/claw` 대화형 에이전트 | 세션 시 |
| Cron | 주기 작업, 품질 루프 | job별 |

---

## OCP 확장점

```
새 외부 시스템     = DataSource impl 추가      → 코어 변경 0
새 LLM            = AgentRuntime impl 추가    → 코어 변경 0
새 파이프라인 단계  = workspace yaml 수정       → 코어 변경 0
새 품질 검사       = Cron 등록                 → 코어 변경 0
```

---

## 상세 문서

| 문서 | 설명 |
|------|------|
| [QueuePhase 상태 머신](./concerns/queue-state-machine.md) | 상태 전이 다이어그램, worktree 생명주기, on_fail 조건 |
| [Daemon](./concerns/daemon.md) | 실행 루프 의사코드, concurrency, graceful shutdown |
| [DataSource](./concerns/datasource.md) | trait, context 스키마, 워크플로우 yaml, escalation |
| [AgentRuntime](./concerns/agent-runtime.md) | LLM 실행 추상화, RuntimeRegistry |
| [Claw](./concerns/claw-workspace.md) | 대화형 에이전트, evaluate, slash command |
| [Cron 엔진](./concerns/cron-engine.md) | 품질 루프, force trigger, cron 관리 |
| [CLI 레퍼런스](./concerns/cli-reference.md) | 3-layer SSOT, autodev context, 전체 커맨드 |

---

## v4 → v5 변경 요약

| 항목 | v4 | v5 |
|------|-----|-----|
| 레포 단위 | `repo` | `workspace` (1:1 매핑) |
| Daemon 역할 | 수집 + drain + Task 실행 + escalation | 상태 머신 + yaml 액션 실행기 |
| Task trait | 5개 구현체 | **제거**. prompt/script로 대체 |
| 파이프라인 단계 | `TaskKind` enum (하드코딩) | yaml states (동적 정의) |
| 부수효과 (PR, 라벨) | Task.after_invoke() | on_done script (gh CLI 등) |
| 인프라 (worktree) | Task.before_invoke() | 인프라 레이어, retry 시 보존 |
| 컨텍스트 조회 | Task 내부 | `autodev context` CLI |
| 환경변수 | DataSource별 다수 | `WORK_ID` + `WORKTREE` 만 |
| QueuePhase | 5개 | 8개 (+Completed, HITL, Failed) |
| Pending → Ready | CLAW feature flag | **항상 자동** |
| evaluate | Claw가 판단 | cron 기반 + force_trigger 하이브리드, CLI 도구 호출 |
| DataSource trait | 5개 메서드 | collect + get_context 만 |
| Concurrency | InFlightTracker | 2단계 (workspace + global) |

---

## v4 미구현 항목 처리

| v4 미구현 | v5 처리 |
|-----------|---------|
| C1. Daemon drain 제거 | DataSource 도입, Pending→Ready 자동 전이 |
| C2. Notifier 연결 | on_done/on_fail script에서 직접 처리 |
| C3. Force trigger | cron-engine force_trigger |
| C4. escalation 정책 | workspace yaml escalation (3단계 순차 + terminal 분기) |
| H1. Spec completion | gap-detection cron + HITL 최종 확인 |
| H2. 섹션 검증 | /spec add 대화형 검증 |
| H3. spec prioritize | /claw 세션 자연어 |
| H4. hitl timeout | hitl-timeout cron |
| H5. claw edit | /claw 세션 자연어 |
| M1~M3 | Cron 품질 루프 + worktree 보존 정책 |

---

## 구현 순서

```
Phase 1: 코어 재구성
  → workspace 마이그레이션, DataSource trait, QueuePhase 확장
  → 상태 머신 단순화, autodev context CLI

Phase 2: handler 실행기
  → AgentRuntime trait, prompt/script 실행기, worktree 인프라
  → Task trait 제거

Phase 3: evaluate + escalation
  → evaluate cron (CLI 도구 호출), force_trigger
  → escalation 정책, on_done/on_fail, Failed 상태

Phase 4: Claw + slash command
  → /claw, /auto, /spec 통합

Phase 5: TUI + 품질 루프
  → dashboard, gap-detection, spec completion
```
