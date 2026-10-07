# Belt Spec

> **구조**: 설계 개요 + 관심사별 상세 스펙 + 사용자 플로우

## 설계 문서

- **[DESIGN.md](./DESIGN.md)** — 설계 철학 + 전체 구조 개요 (간결)

## 관심사별 상세 스펙 (concerns/)

"이 시스템은 내부적으로 어떻게 동작하지?" — 구현자 대상

| 문서 | 설명 |
|------|------|
| [workspace.yaml 스키마](./concerns/workspace-schema.md) | workspace.yaml 전체 구조 (SSOT) — sources/runtime/evaluate/stagnation 설정, 각 concern이 참조 |
| [QueuePhase 상태 머신](./concerns/queue-state-machine.md) | 8개 phase 전이, **전이 캡슐화**, worktree 생명주기, on_fail 조건 |
| [Daemon](./concerns/daemon.md) | **내부 모듈 구조**, 실행 루프, **DB dependency gate**, concurrency, graceful shutdown |
| [Evaluator](./concerns/evaluator.md) | Progressive Evaluation Pipeline, 완료 아이템 판정 |
| [Stagnation Detection](./concerns/stagnation.md) | 정체 패턴(SPINNING, OSCILLATION) 감지, Lateral Thinking 사고 전환 |
| [LifecycleHook](./concerns/lifecycle-hook.md) | 상태 전이 반응 trait, handler/hook 분리, lazy 로딩 |
| [DataSource](./concerns/datasource.md) | 외부 시스템 추상화 trait + **source_data** + 워크플로우 yaml |
| [AgentRuntime](./concerns/agent-runtime.md) | LLM 실행 추상화 trait + Registry |
| [Agent 워크스페이스](./concerns/agent-workspace.md) | 대화형 에이전트 + **per-item evaluate** + slash command |
| [Cron 엔진](./concerns/cron-engine.md) | 주기 작업 (evaluate는 Daemon tick으로 이동) |
| [CLI 레퍼런스](./concerns/cli-reference.md) | 3-layer SSOT + `belt context` + 전체 커맨드 트리 |
| [Cross-Platform](./concerns/cross-platform.md) | OS 추상화 (ShellExecutor, DaemonNotifier) |
| [Distribution](./concerns/distribution.md) | 배포 전략 |
| [Data Model](./concerns/data-model.md) | 전이 이력, HITL 요청, 취소 요청, 파생 아이템, 도메인 어휘 |

## 사용자 플로우 (flows/)

"사용자가 X를 하면 어떻게 되지?" — 시나리오 기반, 기획자/사용자 대상

| # | Flow | 설명 |
|---|------|------|
| 01 | [온보딩](./flows/01-setup.md) | workspace 등록 → 컨벤션 부트스트랩 |
| 03 | [이슈 파이프라인](./flows/03-issue-pipeline.md) | handlers 실행 → **stagnation detection** → evaluate → hook.on_done |
| 04 | [실패 복구와 HITL](./flows/04-failure-and-hitl.md) | **stagnation + lateral thinking** → escalation → hook 트리거 → 사람 개입 |
| 05 | [모니터링](./flows/05-monitoring.md) | TUI + CLI + /agent 시각화 + **stagnation 표시** |
