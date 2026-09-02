# Belt Spec — Design

> 버전 마이그레이션 배경과 변경 이력은 [plans/2026-04-v6-migration.md](../plans/2026-04-v6-migration.md) 참고.

---

## 목표

Daemon을 **상태 머신 + 상태 전이를 트리거하는 CPU**로 단순화한다.
handler(prompt/script)가 작업을 수행하고, LifecycleHook이 상태 변화에 반응한다.
실패 시 **패턴을 감지하고 사고를 전환**하여 같은 실수를 반복하지 않는다.

```
Daemon이 아는 것       = 큐 상태 머신 + 언제 어떤 hook을 트리거할지
handler가 실행         = yaml에 정의된 prompt/script (작업 자체)
LifecycleHook이 반응   = 상태 전이 시 외부 시스템에 반영 (DataSource별 impl)
evaluate가 판단        = handler 결과가 충분한지, 사람이 봐야 하는지 (Done or HITL)
stagnation이 감지      = 실패 패턴을 분석하고, 사고를 전환하여 다르게 재시도
```

---

## Actor

| Actor | 역할 | 상호작용 |
|-------|------|---------|
| **운영자** | Belt를 설치·설정·모니터링하는 사람 | workspace.yaml 작성, `belt start`, TUI dashboard, HITL 응답 |
| **이슈 작성자** | GitHub에 이슈를 등록하는 개발자/PM | 이슈 등록 + belt 라벨 부착 → Belt가 자동 수집 |
| **Belt Daemon** | 자율 실행 프로세스 | 수집 → 분류 → 전이 → 실행 → 반영 루프 |
| **LLM Agent** | handler prompt를 실행하는 AI (Claude, Gemini, Codex) | Daemon이 subprocess로 호출, worktree 안에서 실행 |
| **GitHub** | 이슈/PR 소스 시스템 | DataSource.collect()가 `gh` CLI로 이슈 조회, on_done script가 PR 생성 |
| **Cron Engine** | 주기 작업 스케줄러 | evaluate, gap-detection, hitl-timeout 등 내부 주기 실행 |
| **Reviewer** | PR을 리뷰하는 사람 또는 Bot | changes_requested → DataSource가 감지 → 파이프라인 재진입 |

---

## 외부 시스템 연동

Belt는 외부 시스템을 trait으로 추상화한다. 코어는 구체적 시스템을 모른다.

| 경계 | 추상화 | 사용 지점 |
|------|--------|----------|
| **이슈 소스** | `DataSource` trait | collect(), get_context() — 읽기 |
| **상태 반응** | `LifecycleHook` trait | on_enter/on_done/on_fail/on_escalation — 쓰기 |
| **LLM 실행** | `AgentRuntime` trait | handler prompt, evaluate, lateral plan |
| **상태 저장** | belt-infra DB 레이어 | 전체 상태 영속화 |
| **코드 격리** | belt-infra worktree 레이어 | worktree 생성/정리 |

> **연동 원칙**: 코어는 외부 시스템의 프로토콜/인증을 모른다. trait impl이 각자의 방식으로 연동한다. 새 외부 시스템 추가 = DataSource + LifecycleHook impl 추가, 코어 변경 0. 구체적인 연동 방식은 각 concern 문서([DataSource](./concerns/datasource.md), [LifecycleHook](./concerns/lifecycle-hook.md), [AgentRuntime](./concerns/agent-runtime.md))에서 정의한다.

---

## 설계 철학

### 1. 컨베이어 벨트

아이템은 한 방향으로 흐른다. 되돌아가지 않는다. 부족하면 Cron이 새 아이템을 만들어서 다시 벨트에 태운다.

### 2. Workspace = 1 Repo

workspace는 하나의 외부 레포와 1:1로 대응한다. v4의 `repo` 개념을 리네이밍. GitHub 외 Jira, Slack 등도 지원하기 위한 추상화 (v5~v6는 GitHub에 집중).

### 3. DataSource가 수집을, LifecycleHook이 반응을 소유

DataSource는 외부 시스템에서 아이템을 읽어오고(수집/컨텍스트), LifecycleHook은 상태 변화에 대해 외부 시스템에 쓴다(반응). 같은 외부 시스템이라도 읽기와 쓰기의 관심사가 분리된다. 상세: [DataSource](./concerns/datasource.md), [LifecycleHook](./concerns/lifecycle-hook.md)

### 4. Daemon = CPU

상태 머신을 틱마다 순회하며 전이를 결정하고, 해당 workspace의 LifecycleHook을 트리거한다. Daemon은 hook이 실제로 무엇을 하는지 모른다 — `Result<()>`만 받을 뿐. 내부는 Advancer·Executor·HitlService·StagnationDetector·Evaluator 모듈로 분리. 상세: [Daemon](./concerns/daemon.md)

### 5. handler는 작업, hook은 반응

handler(prompt/script)는 yaml에 정의된 작업 자체(분석, 구현, 리뷰). LifecycleHook은 상태 전이 시 외부 시스템 반응(PR 생성, 라벨 변경, 코멘트). Daemon은 handler를 실행하고, 전이가 발생하면 hook을 트리거한다.

### 6. 코드 작업은 항상 worktree

handler prompt는 항상 git worktree 안에서 실행. worktree 생성/정리는 인프라 레이어 담당.

### 7. Progressive Evaluation — 판정은 비용 순으로

Daemon tick은 execute 이후 evaluate 순서로 동작한다 (collect→advance→execute→evaluate). evaluate는 방금 execute에서 Completed된 아이템과 이전 tick에서 Completed된 아이템을 함께 판정하며, 여기서 해제한 concurrency slot은 다음 tick의 advance가 사용한다. 판정 자체는 비용이 낮은 검증(Mechanical)부터 단계적으로 수행하여, 낮은 단계로 판정 가능하면 이후 단계(Semantic 등)를 생략한다. 상세: [Evaluator](./concerns/evaluator.md)

### 8. 아이템 계보 (Lineage)

같은 외부 엔티티에서 파생된 아이템은 `source_id`로 연결. 모든 이벤트는 append-only history로 축적.

### 9. 환경변수 최소화

`WORK_ID` + `WORKTREE` 2개만 주입. 나머지는 `belt context $WORK_ID --json`으로 조회. 상세: [DataSource](./concerns/datasource.md)

### 10. Concurrency 제어

workspace.concurrency (workspace yaml 루트) + daemon.max_concurrent 2단계. evaluate LLM 호출도 slot 소비. 상세: [Daemon](./concerns/daemon.md)

### 11. Cron은 품질 루프

파이프라인은 1회성, 품질은 Cron이 지속 감시. gap-detection이 새 이슈 생성 → 파이프라인 재진입. 상세: [Cron 엔진](./concerns/cron-engine.md)

### 12. Phase 전이 캡슐화

`QueueItem.phase` 필드를 직접 대입하지 못하도록 `pub(crate)` + `transit()` 메서드로 캡슐화한다. 모든 전이는 `can_transition_to()` 검증을 경유한다. 상세: [QueuePhase 상태 머신](./concerns/queue-state-machine.md)

### 13. Stagnation Detection + Lateral Thinking — 실패하면 다르게 시도

실패 횟수만으로는 "같은 실수 반복"과 "다른 시도 실패"를 구분할 수 없다. 정체 감지는 동일 출력이 반복되는 패턴(SPINNING)을 감지한다 — 유사도 판정 기준과 임계값은 현재 고정값이며 설정으로 노출되지 않는다. 패턴 감지 시 내장 페르소나(Lateral Thinking)가 접근법을 전환하여 재시도하며, 모든 retry에 lateral plan이 자동 주입되는 것이 기본 동작이다. 상세: [Stagnation Detection](./concerns/stagnation.md)

### Agent는 대화형 에이전트

`belt agent` / `/agent` 세션. 자연어로 큐 조회, HITL 응답, cron 관리. 상세: [Agent](./concerns/agent-workspace.md)

---

## 전체 상태 흐름

```
                         DataSource.collect()
                                │
                                ▼
┌───────────────────────────────────────────────────────────────────┐
│                          Pending                                  │
│  큐 대기. spec dependency gate 확인.                               │
└───────────────────────────┬───────────────────────────────────────┘
                            │ Advancer: spec dep gate (DB)
                            ▼
┌───────────────────────────────────────────────────────────────────┐
│                           Ready                                   │
│  실행 준비 완료. concurrency slot 대기.                            │
└───────────────────────────┬───────────────────────────────────────┘
                            │ Advancer: queue dep gate (DB)
                            │         + conflict gate
                            │         + concurrency (ws + global)
                            ▼
┌───────────────────────────────────────────────────────────────────┐
│                          Running                                  │
│                                                                   │
│  ① worktree 생성 (or retry 시 기존 재사용)                        │
│  ② hook.on_enter() 트리거                                        │
│  ③ handlers 순차 실행                                             │
│     lateral_plan이 있으면 handler prompt에 추가 컨텍스트 주입       │
└────────┬──────────────────────────────────────────┬───────────────┘
         │                                          │
      전부 성공                              handler/on_enter 실패
         │                                          │
         ▼                                          ▼
┌─────────────────┐          ┌──────────────────────────────────────┐
│   Completed      │          │  Stagnation Analyzer (항상 실행)      │
│                  │          │                                      │
│  Evaluator가     │          │  ① 정체 패턴 감지 (동일 출력 반복)   │
│  다음 tick에서   │          │     과거/현재 실패 error 비교        │
│  판정            │          │                                      │
│                  │          │                                      │
│                  │          │                                      │
│  Progressive:    │          │  ② 사고 전환 (패턴 감지 시)          │
│  Mechanical      │          │     페르소나 선택 → lateral_plan     │
│   → Semantic     │          │                                      │
│   → (Consensus)  │          │  ③ Escalation (failure_count 기반)   │
│                  │          │     hook.on_escalation() 트리거      │
│                  │          │     hook.on_fail() 트리거 (retry 제외)│
│                  │          │     retry       → lateral_plan 주입  │
│                  │          │     hitl        → report 첨부 ─────────┐
│                  │          │     terminal    → skip/replan ─────────┤
│                  │          └──────────────────────────────────────┘  │
│                  │                                                    │
│                  │                retry → 새 아이템                   │
│                  │                  │                                 │
│                  │                  │  hook.on_escalation() 트리거    │
│                  │           ┌──────┘                                 │
└───┬─────────┬────┘           │                                        │
    │         │                ▼                                        │
 완료 판정  사람 필요       Pending (lateral_plan 보존)                  │
    │         │                                                        │
    ▼         │                                                        │
 hook         │                                                        │
.on_done()    │                                                        │
    │         │                                                        │
  ┌─┴──┐      │                                                        │
성공  실패    │                                                        │
  │   │       │                                                        │
  ▼   ▼       ▼                                                        │
┌──────┐ ┌───────┐ ┌──────────────────────────────────────────────┐    │
│ Done │ │Failed │ │                   HITL                        │◄───┘
│      │ │       │ │                                              │
│ wt   │ │ wt    │ │  hitl_notes에 lateral_report 포함:          │
│ 정리  │ │ 보존   │ │    패턴, 시도한 페르소나들, 각 분석 결과    │
│      │ │       │ │                                              │
│TERM. │ │       │ │  응답: done / retry / skip / replan          │
└──────┘ └───────┘ │  timeout → terminal (skip / replan)          │
                   └────────────────────────┬─────────────────────┘
                                            │ skip
                                            ▼
                   ┌──────────────────────────────────────────────┐
                   │                   Skipped                    │
                   │  terminal (worktree 정리)                    │
                   └──────────────────────────────────────────────┘
```

### Tick 순서와 Evaluator 위치

```
매 tick:
  ① collect    — DataSource에서 새 아이템 수집 → Pending
  ② advance    — Pending→Ready→Running 전이 (slot 확보)
  ③ execute    — Running 아이템의 handler 실행 + hook 트리거
  ④ evaluate   — Completed 아이템을 Done/HITL로 판정 (concurrency slot 해제)
  ⑤ cron.tick  — 품질 루프 (gap-detection 등)
```

> **slot 해제와 확보는 tick 경계를 넘어 순환한다**: advance는 이전 tick의 evaluate가 해제한 slot으로
> 아이템을 Running에 올린다. execute가 끝낸 아이템은 같은 tick의 evaluate가 판정해 slot을 해제하고,
> 그 slot을 다음 tick의 advance가 다시 사용한다.

### 상태별 소유 모듈

| Phase | 소유 모듈 | 핵심 동작 | Hook 트리거 |
|-------|----------|----------|------------|
| Pending | Advancer | spec dependency gate (DB 조회) | — |
| Ready | Advancer | queue dep gate (DB) + concurrency check | — |
| Running | Executor | worktree + handlers (lateral_plan 주입) | hook.on_enter() |
| Running → 실패 | StagnationDetector + LateralAnalyzer | 유사도 분석 → 사고 전환 → escalation | hook.on_escalation() + hook.on_fail() |
| Completed | Evaluator | Progressive Pipeline: Mechanical → Semantic → (Consensus) | — |
| Done | — | worktree 정리 | hook.on_done() |
| HITL | HitlService | 응답 대기 / timeout / terminal action | — |
| Failed | — | hook.on_done() 실패, 인프라 오류 | — |
| Skipped | — | terminal | — |

---

## Daemon 내부 구조

```
┌─ Daemon (CPU) ────────────────────────────────────────────────────────┐
│                                                                       │
│  loop { collector → advancer → executor → evaluator → cron.tick() }  │
│                                                                       │
│  Daemon이 아는 것: 상태 머신 + 언제 어떤 hook을 트리거할지             │
│  Daemon이 모르는 것: hook이 실제로 무엇을 하는지                       │
│                                                                       │
│  ┌──────────┐  ┌───────────────────────────────────────────────────┐ │
│  │ Advancer │  │ Executor                                          │ │
│  │          │  │                                                   │ │
│  │ 전이     │  │  ① hook.on_enter() 트리거                         │ │
│  │ dep gate │  │  ② handler 실행 (yaml prompt/script)              │ │
│  │ (DB)     │  │  ③ 성공 → transit(Completed)                      │ │
│  │ conflict │  │     → hook.on_done() 트리거                       │ │
│  │ concurr. │  │  ④ 실패 시:                                       │ │
│  │          │  │  ┌─────────────────────────────────────────────┐  │ │
│  │          │  │  │ 정체 패턴 감지                              │  │ │
│  │          │  │  │  동일 출력 반복(SPINNING) 여부를             │  │ │
│  │          │  │  │  해시 완전 일치 기준으로 판정                │  │ │
│  │          │  │  └────────────────┬────────────────────────────┘  │ │
│  │          │  │                   ▼ 패턴 감지 시                   │ │
│  │          │  │  ┌─────────────────────────────────────────────┐  │ │
│  │          │  │  │ LateralAnalyzer                             │  │ │
│  │          │  │  │  패턴 → 페르소나 → lateral_plan 생성        │  │ │
│  │          │  │  └─────────────────────────────────────────────┘  │ │
│  │          │  │                   ▼                               │ │
│  │          │  │  escalation 결정 (failure_count)                   │ │
│  │          │  │     → hook.on_escalation(action) 트리거            │ │
│  │          │  │     → hook.on_fail() 트리거 (retry 제외)           │ │
│  └──────────┘  └───────────────────────────────────────────────────┘ │
│                                                                       │
│  ┌─────────────┐  ┌─────────────┐  ┌──────────────┐                 │
│  │ Evaluator   │  │ HitlService │  │ CronEngine   │                 │
│  │ per-item    │  │ 응답 처리    │  │ tick         │                 │
│  │ Done/HITL   │  │ timeout     │  │ force_trigger│                 │
│  │ 분류        │  │ terminal    │  │ 품질 루프     │                 │
│  └─────────────┘  └─────────────┘  └──────────────┘                 │
└──────────────────────────────┬────────────────────────────────────────┘
                               │ 트리거 (호출만, 실행 책임 없음)
              ┌────────────────┼─────────────────┐
              ▼                ▼                  ▼
┌──────────────────┐  ┌──────────────┐  ┌──────────────┐
│ WorkspaceBinding │  │ AgentRuntime │  │   SQLite DB  │
│                  │  │              │  │              │
│ sources:         │  │  LLM 실행    │  │ queue_items  │
│  [DataSource]    │  │  추상화      │  │ history      │
│   수집/컨텍스트   │  │              │  │ transition_  │
│                  │  │              │  │  events      │
│ hook:            │  │              │  │              │
│  LifecycleHook   │  │              │  │              │
│   on_enter()     │  │              │  │              │
│   on_done()      │  │              │  │              │
│   on_fail()      │  │              │  │              │
│   on_escalation()│  │              │  │              │
│                  │  │              │  │              │
│  Hook impl이     │  │              │  │              │
│  실행 책임 소유   │  │              │  │              │
└──────────────────┘  └──────────────┘  └──────────────┘
```

---

## Stagnation — 정체 감지

정체 감지는 동일 (source_id, state)에서 과거 실패 error와 현재 error를 비교해, 완전 일치 기준(SPINNING)으로 반복 실패 패턴을 판정한다. 새 유사도 알고리즘·패턴 감지 로직은 코어 변경 없이 추가할 수 있는 확장점이다.

상세: [Stagnation Detection](./concerns/stagnation.md)

---

## 관심사 분리

| 레이어 | 책임 | 토큰 |
|--------|------|------|
| Daemon | CPU — 상태 머신 순회 + hook 트리거 + cron 스케줄링 | 0 |
| Advancer | Pending→Ready→Running 전이, dependency gate (DB), conflict 검출 | 0 |
| Executor | handler 실행, escalation 결정, hook 트리거 | handler별 |
| StagnationDetector | 정체 패턴(SPINNING) 감지 | 0 |
| LateralAnalyzer | 내장 페르소나로 대안 접근법 분석, lateral_plan 생성 | 분석 시 |
| HitlService | HITL 응답 처리, timeout 만료, terminal action | 0 |
| Evaluator | Completed → Done/HITL 분류 (per-item, CLI 도구 호출) | 분류 시 |
| 인프라 | worktree 생성/정리, 플랫폼 추상화 (셸, IPC) | 0 |
| DataSource | 수집(collect) + 컨텍스트 조회(context, source_data) — 읽기 | 0 |
| LifecycleHook | 상태 전이 반응 (on_enter/on_done/on_fail/on_escalation) — 쓰기 | 0 |
| AgentRuntime | LLM 실행 추상화 | handler별 |
| Agent | `belt agent` / `/agent` 대화형 에이전트 | 세션 시 |
| Cron | 주기 작업, 품질 루프 | job별 |

---

## OCP 확장점

```
새 외부 시스템     = DataSource + LifecycleHook impl 추가  → 코어 변경 0
새 LLM            = AgentRuntime impl 추가                → 코어 변경 0
새 파이프라인 단계  = workspace yaml 수정                   → 코어 변경 0
새 lifecycle 반응  = LifecycleHook impl 추가/변경          → 코어 변경 0
새 품질 검사       = Cron 등록                             → 코어 변경 0
새 OS/플랫폼      = ShellExecutor impl 추가               → 코어 변경 0
새 유사도 알고리즘  = SimilarityJudge impl 추가            → 코어 변경 0
```

> **자유 스키마 확장점**: `ItemContext.source_data: serde_json::Value`는 DataSource별 자유 스키마 확장을 위한 필드다.
> 각 DataSource는 원본 응답을 소스 종류별 키(예: GitHub는 `issue`) 아래에 담아 소스 간 데이터가 서로 충돌하지 않게 한다. 활용 계획은 [source_data와 stagnation 로드맵](../plans/source-data-and-stagnation-roadmap.md) 참조.

---

## 상세 문서

| 문서 | 설명 |
|------|------|
| [QueuePhase 상태 머신](./concerns/queue-state-machine.md) | 상태 전이, 전이 캡슐화, worktree 생명주기, on_fail 조건 |
| [Daemon](./concerns/daemon.md) | 내부 모듈 구조, 실행 루프, dependency gate (DB), concurrency, graceful shutdown |
| [Evaluator](./concerns/evaluator.md) | Progressive Evaluation Pipeline, Stage trait — 완료 아이템 판정 |
| [Stagnation Detection](./concerns/stagnation.md) | 정체 패턴(SPINNING) 감지, Lateral Thinking 사고 전환 |
| [LifecycleHook](./concerns/lifecycle-hook.md) | 상태 전이 반응 trait, DataSource별 impl, workspace 바인딩, lazy 로딩 |
| [DataSource](./concerns/datasource.md) | trait, context 스키마 (source_data), 워크플로우 yaml, escalation |
| [AgentRuntime](./concerns/agent-runtime.md) | LLM 실행 추상화, RuntimeRegistry |
| [Agent](./concerns/agent-workspace.md) | 대화형 에이전트, per-item evaluate, slash command |
| [Cron 엔진](./concerns/cron-engine.md) | 품질 루프, per-item evaluate, force trigger |
| [CLI 레퍼런스](./concerns/cli-reference.md) | 3-layer SSOT, belt context, 전체 커맨드 |
| [Cross-Platform](./concerns/cross-platform.md) | OS 추상화 (ShellExecutor, DaemonNotifier) |
| [Data Model](./concerns/data-model.md) | SQLite 스키마, 도메인 enum, source_data, stagnation types |
