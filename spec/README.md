# Spec v6 Draft

> **Date**: 2026-04-04
> **Status**: Draft
> **구조**: 설계 개요 + 관심사별 상세 스펙 + 사용자 플로우

## 핵심 변경 (v5 → v6)

- **LifecycleHook 분리**: on_done/on_fail/on_enter를 yaml script에서 `LifecycleHook` trait으로 분리. handler(작업)와 hook(반응) 관심사 분리. DataSource별 impl, workspace별 바인딩, lazy 로딩
- **Daemon = CPU**: yaml script 실행기 → 상태 머신 CPU. hook 트리거만, 실행 책임은 Hook impl이 소유
- **Stagnation Detection**: 실패 횟수가 아니라 실패 패턴(SPINNING, OSCILLATION, NO_DRIFT, DIMINISHING_RETURNS)을 감지
- **Daemon 모듈 분리**: 단일 daemon.rs → Advancer + Executor + HitlService + StagnationDetector 모듈
- **Phase 전이 캡슐화**: `item.phase` 직접 대입 금지, `QueueItem::transit()` 메서드 강제
- **ItemContext 확장**: `source_data: serde_json::Value` 추가 — 새 DataSource 추가 시 코어 변경 0
- **hitl_terminal_action 타입 안전**: `Option<String>` → `Option<EscalationAction>`
- **Dependency Gate DB 기반**: in-memory → DB 조회, 재시작 시 순서 보장
- **Evaluator per-item 판정**: workspace 배치 → per-work_id 개별 LLM 판정

## 설계 문서

- **[DESIGN-v6.md](./DESIGN.md)** — 설계 철학 + 전체 구조 개요 (간결)

## 관심사별 상세 스펙 (concerns/)

"이 시스템은 내부적으로 어떻게 동작하지?" — 구현자 대상

| 문서 | 설명 |
|------|------|
| [QueuePhase 상태 머신](./concerns/queue-state-machine.md) | 8개 phase 전이, **전이 캡슐화**, worktree 생명주기, on_fail 조건 |
| [Daemon](./concerns/daemon.md) | **내부 모듈 구조**, 실행 루프, **DB dependency gate**, concurrency, graceful shutdown |
| [Evaluator](./concerns/evaluator.md) | **v6 신규** — Progressive Evaluation Pipeline, Evaluate before Execute |
| [Stagnation Detection](./concerns/stagnation.md) | **v6 신규** — 4가지 정체 패턴, 해시 기반 탐지 |
| [LifecycleHook](./concerns/lifecycle-hook.md) | **v6 신규** — 상태 전이 반응 trait, handler/hook 분리, lazy 로딩 |
| [DataSource](./concerns/datasource.md) | 외부 시스템 추상화 trait + **source_data** + 워크플로우 yaml |
| [AgentRuntime](./concerns/agent-runtime.md) | LLM 실행 추상화 trait + Registry |
| [Agent 워크스페이스](./concerns/agent-workspace.md) | 대화형 에이전트 + **per-item evaluate** + slash command |
| [Cron 엔진](./concerns/cron-engine.md) | 주기 실행 + 품질 루프 (evaluate는 Daemon tick으로 이동) |
| [CLI 레퍼런스](./concerns/cli-reference.md) | 3-layer SSOT + `belt context` + 전체 커맨드 트리 |
| [Cross-Platform](./concerns/cross-platform.md) | OS 추상화 (ShellExecutor, DaemonNotifier) |
| [Distribution](./concerns/distribution.md) | 배포 전략 |
| [Data Model](./concerns/data-model.md) | SQLite 스키마, **StagnationPattern enum**, **EscalationAction FromStr**, **source_data** |

## 사용자 플로우 (flows/)

"사용자가 X를 하면 어떻게 되지?" — 시나리오 기반, 기획자/사용자 대상

| # | Flow | 설명 |
|---|------|------|
| 01 | [온보딩](./flows/01-setup.md) | workspace 등록 → 컨벤션 부트스트랩 |
| 02 | [스펙 생명주기](./flows/02-spec-lifecycle.md) | 스펙 등록 → 이슈 분해 → 완료 판정 |
| 03 | [이슈 파이프라인](./flows/03-issue-pipeline.md) | handlers 실행 → **stagnation detection** → evaluate → hook.on_done |
| 04 | [실패 복구와 HITL](./flows/04-failure-and-hitl.md) | **stagnation + lateral thinking** → escalation → hook 트리거 → 사람 개입 |
| 05 | [모니터링](./flows/05-monitoring.md) | TUI + CLI + /agent 시각화 + **stagnation 표시** |

## 이슈 매핑

| 이슈 | 주요 반영 문서 |
|------|-------------|
| 신규 LifecycleHook 분리 | lifecycle-hook.md, datasource.md, daemon.md, DESIGN |
| #723 Stagnation/Oscillation 탐지 | stagnation.md, daemon.md, data-model.md, flow-04, DESIGN |
| #717 Daemon 내부 모듈 분리 | daemon.md, DESIGN |
| #718 Phase 전이 캡슐화 | queue-state-machine.md, data-model.md, DESIGN |
| #719 ItemContext source_data | data-model.md, datasource.md |
| #720 hitl_terminal_action 타입 | data-model.md, flow-04 |
| #721 Dependency Gate DB 기반 | daemon.md |
| #722 Evaluator per-item 판정 | cron-engine.md, agent-workspace.md, daemon.md |
