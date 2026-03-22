---
paths:
  - "crates/belt-infra/**/*.rs"
---

# Infrastructure 레이어 컨벤션 (외부 시스템 어댑터)

> 이 레이어는 DB, API, CLI 등 외부 시스템과의 경계다. core가 이 레이어의 구현 세부사항에 의존하면 안 된다.

## trait으로 추상화하고 구현은 이 레이어에 둬라
- core가 의존하는 것은 trait (인터페이스)이다.
- 구체 구현(SQLite, GitHub CLI, Git 명령)은 infra 내부에 캡슐화한다.
- DataSource impl, AgentRuntime impl 모두 이 레이어에 위치한다.

## 외부 응답은 즉시 도메인 타입으로 변환하라
- JSON, raw string 등 외부 포맷을 상위 레이어까지 전파하지 마라.
- 파싱/변환 책임은 이 레이어에서 완결한다.

## 에러는 도메인 에러로 매핑하라
- 라이브러리 에러(rusqlite::Error 등)를 그대로 상위에 노출하지 마라.
- BeltError 또는 커스텀 에러로 변환하여 상위 레이어가 구현 세부사항을 알 필요 없게 한다.

## worktree 관리
- worktree 생성/정리는 이 레이어의 인프라 책임이다.
- Done/Skipped → 정리, HITL/Failed/retry → 보존 규칙을 준수한다.
