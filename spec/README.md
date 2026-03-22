# Spec v5 Draft

> **Date**: 2026-03-22
> **Status**: Draft (reviewed)
> **구조**: 설계 개요 + 관심사별 상세 스펙 + 사용자 플로우

## 핵심 변경 (v4 → v5)

- **Daemon = 상태 머신 + 실행기**: yaml에 정의된 prompt/script만 호출하는 단순 실행기로 축소
- **Task trait 제거**: 5개 구현체 → prompt/script로 대체
- **workspace = 1 repo**: 다른 DataSource 지원을 위한 추상화
- **QueuePhase 8개**: Pending/Ready/Running/Completed/Done/HITL/Failed/Skipped
- **worktree는 인프라**: 항상 worktree에서 작업, Done 시 정리
- **환경변수 최소화**: `WORK_ID` + `WORKTREE`만, 나머지는 `autodev context` CLI
- **evaluate = CLI 도구 호출**: LLM이 `autodev queue done/hitl` 실행

## 설계 문서

- **[DESIGN-v5.md](./DESIGN-v5.md)** — 설계 철학 + 전체 구조 개요 (간결)

## 관심사별 상세 스펙 (concerns/)

"이 시스템은 내부적으로 어떻게 동작하지?" — 구현자 대상

| 문서 | 설명 |
|------|------|
| [QueuePhase 상태 머신](./concerns/queue-state-machine.md) | 8개 phase 전이 다이어그램, worktree 생명주기, on_fail 조건 |
| [Daemon](./concerns/daemon.md) | 실행 루프 의사코드, concurrency, graceful shutdown |
| [DataSource](./concerns/datasource.md) | 외부 시스템 추상화 trait + context CLI + 워크플로우 yaml |
| [AgentRuntime](./concerns/agent-runtime.md) | LLM 실행 추상화 trait + Registry |
| [Claw 워크스페이스](./concerns/claw-workspace.md) | 대화형 에이전트 + evaluate + slash command |
| [Cron 엔진](./concerns/cron-engine.md) | 주기 실행 + 품질 루프 + force trigger |
| [CLI 레퍼런스](./concerns/cli-reference.md) | 3-layer SSOT + `autodev context` + 전체 커맨드 트리 |

## 사용자 플로우 (flows/)

"사용자가 X를 하면 어떻게 되지?" — 시나리오 기반, 기획자/사용자 대상

| # | Flow | 설명 |
|---|------|------|
| 01 | [온보딩](./flows/01-setup.md) | workspace 등록 → 컨벤션 부트스트랩 |
| 02 | [스펙 생명주기](./flows/02-spec-lifecycle.md) | 스펙 등록 → 이슈 분해 → 완료 판정 |
| 03 | [이슈 파이프라인](./flows/03-issue-pipeline.md) | handlers 실행 → evaluate → on_done |
| 04 | [실패 복구와 HITL](./flows/04-failure-and-hitl.md) | escalation → on_fail → 사람 개입 |
| 05 | [모니터링](./flows/05-monitoring.md) | TUI + CLI + /claw 시각화 |
