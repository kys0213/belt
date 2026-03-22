# Flow 2: 스펙 생명주기 — 등록 → 이슈 분해 → 완료

> 사용자가 디자인 스펙을 등록하면, 이슈가 자동 생성되어 파이프라인에 진입하고, Cron 품질 루프가 스펙 완료까지 감시한다.

---

## Spec Lifecycle

```
Draft ──→ Active ←──→ Paused
              │
              ▼
          Completing
              │
              ▼
          Completed (terminal)

Any ──→ Archived (soft delete)
Archived ──resume──→ Active (복구)
```

| 상태 | 가능한 전이 | CLI |
|------|------------|-----|
| Draft | → Active | `spec add` |
| Active | → Paused, → Completing(자동), → Archived | `spec pause`, `spec remove` |
| Paused | → Active, → Archived | `spec resume`, `spec remove` |
| Completing | → Active(gap 발견), → Completed(HITL 승인) | 자동 |
| Completed | → Archived | `spec remove` |
| Archived | → Active | `spec resume` |

---

## 등록 → 이슈 분해

```
/spec add [file]
  → 필수 섹션 검증 (개요, 요구사항, 아키텍처, 테스트, 수용 기준)
  → DB에 저장 (status: Active)
  → 스펙 분해 → 이슈 자동 생성
  → 각 이슈에 trigger 라벨 (예: autodev:analyze) 부착
  → DataSource.collect()가 감지 → 파이프라인 진입
```

### 스펙 분해 전략

스펙 분해는 **built-in skill**로 제공되며, 커스텀 오버라이드가 가능하다.

```
기본 분해: acceptance criteria 기반
  → 각 수용 기준을 독립 이슈로 분해
  → 이슈 간 의존성 자동 추론 (공유 파일/모듈 기반)

커스텀 분해: per-workspace 또는 per-user skill 오버라이드
  → ~/.autodev/workspaces/<name>/skills/decompose/ 에 커스텀 skill 배치
  → 기본 skill 대신 커스텀 skill 실행
```

분해 결과는 사용자에게 제시 → 확인/수정 후 이슈 생성 (대화형).
잘못 분해된 경우 `/spec update`로 스펙 수정 → gap-detection이 재평가.

---

## 스펙 완료 판정

스펙 완료는 Cron 품질 루프가 담당:

```
gap-detection cron (1시간):
  → 스펙 vs 현재 코드 비교
  → gap 발견 → 새 이슈 생성 → 파이프라인 재진입
  → gap 없음 + 모든 linked issues Done → Completing 상태로 전이

on_spec_completing:
  → TestRunner: spec.test_commands 실행
    → 실패 → 새 이슈 생성 → Active로 복귀
    → 성공 → HITL: 최종 확인 (approve / request-changes)
```

> **Completing 전이 조건**: gap-detection이 gap을 발견하지 못하고, 해당 스펙에 linked된 모든 이슈가 Done 상태일 때 자동으로 Completing 상태로 전이한다.

---

## 다중 스펙 우선순위

스펙 등록 시점에 Claw가 기존 Active 스펙과의 관계를 판단한다:

```
/spec add [file]
  → Claw: 기존 Active 스펙과 충돌/의존성 분석
    → 독립: 병렬 실행 가능 (concurrency 제한 내)
    → 의존: DependencyGuard 등록 → 선행 스펙 완료 후 실행
    → 충돌: 같은 파일/모듈 영향 → HITL 요청 (사용자 판단)
```

판단 기준: 스펙의 대상 모듈/파일 경로, acceptance criteria의 겹침 여부.

> **DependencyGuard**: 스펙 간 의존 관계를 추적하는 메커니즘. 의존 스펙의 이슈가 선행 스펙 완료 전에 실행되지 않도록 큐 스케줄링에서 제어한다. 충돌이 감지되면 (같은 파일/모듈에 영향) HITL 이벤트를 생성하여 사용자에게 판단을 요청한다. 내부적으로는 `autodev queue dependency add/remove` CLI로 관리한다.

---

## /spec 통합 커맨드

```
/spec                → 목록
/spec add [file]     → 등록
/spec update <id>    → 수정
/spec status <id>    → 진행도 상세
/spec remove <id>    → Archived
/spec pause <id>     → 일시정지
/spec resume <id>    → 재개
```

---

### 관련 문서

- [이슈 파이프라인](./03-issue-pipeline.md) — 분해된 이슈의 처리 흐름
- [실패 복구와 HITL](./04-failure-and-hitl.md) — 완료 확인 HITL
- [Cron 엔진](../concerns/cron-engine.md) — gap-detection 품질 루프
