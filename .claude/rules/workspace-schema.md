---
paths:
  - "**/workspace*.yaml"
  - "**/workspace*.yml"
  - "tests/**/*.yaml"
---

# workspace.yaml 작업 시 주의

> 스키마 정의(필드·타입·기본값·필수 여부)의 단일 소스는 [spec/concerns/workspace-schema.md](../../spec/concerns/workspace-schema.md)다. 필드를 추가·변경할 때는 그 문서를 먼저 갱신한다.

## 자주 위반되는 제약

- `sources.{type}.escalation`은 각 실패 횟수(`{N}`)뿐 아니라 `terminal`도 필수다. `terminal` 없이 `hitl`만 쓰면 무한 HITL이 가능하다.
- 핸들러 실행 시 주입되는 환경변수는 `WORK_ID`, `WORKTREE` 뿐이다. 스크립트가 그 외 변수(`$ISSUE`, `$STATUS` 등)를 기대하도록 작성하지 않는다.
- handler 하나는 `prompt` 또는 `script` 중 하나만 가진다. 혼용하지 않는다.
- `on_done`/`on_fail`/`on_enter`는 side-effect 전용이다. 도메인 로직(조건 분기 등)을 넣지 않는다.

## 체크리스트

- [ ] `name` 필드가 있는가
- [ ] 각 handler에 `prompt` 또는 `script` 중 하나만 있는가
- [ ] 스크립트가 `WORK_ID`, `WORKTREE` 외 환경변수에 의존하지 않는가
- [ ] `escalation`에 `terminal` 정책이 명시됐는가
