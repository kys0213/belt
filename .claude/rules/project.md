# Belt Project Rules

## Code Conventions
- Public API에는 doc comment 작성
- Error handling: `thiserror` for library errors, `anyhow` for CLI/application errors

## Design Principles
- Daemon은 도메인 로직을 모른다 — yaml에 정의된 prompt/script만 실행
- DataSource/AgentRuntime은 trait으로 추상화 — OCP 준수
- 환경변수는 WORK_ID + WORKTREE 2개만 주입
- 아이템은 한 방향으로만 흐른다 — 되돌아가지 않음
- 의심스러우면 HITL (safe default)

## Dependencies
- 새 dependency 추가 시 필요성 설명 필수
- 가능하면 기존 dependency로 해결
- `features`를 최소한으로 활성화
