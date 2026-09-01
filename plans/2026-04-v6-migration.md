# v6 마이그레이션 (2026-04)

이 문서는 작성 시점의 의사결정 기록이다. 현재 동작·규격의 신뢰 소스가 아니다 — 현재 상태는 spec/ 과 코드로 판단한다.

> 원문: `spec/DESIGN.md` (Spec v6 Draft, 2026-04-05 작성)의 "v5 → v6 변경 요약", "v4 → v5 변경 요약", "구현 순서" 절을 그대로 옮겼다.

## v5 → v6 변경 요약

| 항목 | v5 | v6 | 이슈 |
|------|-----|-----|------|
| Lifecycle 반응 | on_done/on_fail yaml script, Executor 직접 실행 | `LifecycleHook` trait, DataSource별 impl, workspace 바인딩 | 신규 |
| Daemon 역할 | yaml script 실행기 | 상태 머신 CPU — hook 트리거만, 실행 책임 없음 | 신규 |
| Daemon 내부 | 단일 daemon.rs | Orchestrator + Advancer·Executor·HitlService 모듈 분리 | #717 |
| Phase 전이 | `item.phase =` 직접 대입 | `QueueItem::transit()` 강제, phase `pub(crate)` | #718 |
| ItemContext | `issue`/`pr` 필드 직접 | `source_data: serde_json::Value` 추가 (OCP) | #719 |
| hitl_terminal_action | `Option<String>` | `Option<EscalationAction>` (타입 안전) | #720 |
| Dependency gate | in-memory queue | DB 조회 기반 (restart-safety) | #721 |
| Evaluate | cron job, workspace 배치 | Daemon tick 정규 단계, Progressive Pipeline (Mechanical→Semantic→Consensus), history-aware 사전 검증 | #722 |
| 실패 대응 | failure_count → 단순 retry | Composite Similarity 패턴 감지 + Lateral Thinking 사고 전환 | #723 |

## v4 → v5 변경 요약

| 항목 | v4 | v5 |
|------|-----|-----|
| 레포 단위 | `repo` | `workspace` (1:1 매핑) |
| Daemon 역할 | 수집 + drain + Task 실행 + escalation | 상태 머신 + yaml 액션 실행기 |
| Task trait | 5개 구현체 | **제거**. prompt/script로 대체 |
| 파이프라인 단계 | `TaskKind` enum (하드코딩) | yaml states (동적 정의) |
| 부수효과 (PR, 라벨) | Task.after_invoke() | on_done script (gh CLI 등) |
| 인프라 (worktree) | Task.before_invoke() | 인프라 레이어, retry 시 보존 |
| 컨텍스트 조회 | Task 내부 | `belt context` CLI |
| 환경변수 | DataSource별 다수 | `WORK_ID` + `WORKTREE` 만 |
| QueuePhase | 5개 | 8개 (+Completed, HITL, Failed) |
| evaluate | Agent가 판단 | cron 기반 + force_trigger 하이브리드, CLI 도구 호출 |
| DataSource trait | 5개 메서드 | collect + get_context 만 |
| Concurrency | InFlightTracker | 2단계 (workspace + global) |

## 구현 순서

```
Phase 1: 코어 재구성
  → workspace 마이그레이션, DataSource trait, QueuePhase 확장
  → 상태 머신 단순화, belt context CLI

Phase 2: handler 실행기
  → AgentRuntime trait, prompt/script 실행기, worktree 인프라
  → Task trait 제거

Phase 3: evaluate + escalation
  → Evaluator (Daemon tick, Progressive Pipeline)
  → escalation 정책, on_done/on_fail, Failed 상태

Phase 4: Agent + slash command
  → /agent, /auto, /spec 통합

Phase 5: TUI + 품질 루프
  → dashboard, gap-detection, spec completion

Phase 6: 내부 품질 강화 (v6 신규)
  → #720 hitl_terminal_action 타입 안전
  → #721 Dependency gate DB 기반
  → #718 Phase 전이 캡슐화 (QueueItem::transit)
  → #717 Daemon 모듈 분리 (Advancer, Executor, HitlService)
  → #722 Evaluator per-item 판정
  → #719 ItemContext source_data 확장
  → #723 Stagnation Detection + Lateral Thinking
        SimilarityJudge trait (Composite Pattern)
        CompositeSimilarity (ExactHash + TokenFingerprint + NCD)
        LateralAnalyzer (내장 페르소나 5종)
```
