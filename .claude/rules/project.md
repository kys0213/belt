# Belt Project Rules

## Language & Stack
- Rust (edition 2024)
- SQLite via rusqlite for persistence
- tokio for async runtime
- clap for CLI argument parsing
- serde/serde_json for serialization

## Architecture
- Workspace crate layout: `crates/` 하위에 기능별 crate 분리
- `belt-core` → `belt-infra` → `belt-daemon` → `belt-cli` 방향으로만 의존
- core는 infra를 모른다. trait만 정의하고 구현은 infra에 둔다

## Code Conventions
- `cargo fmt` 적용 (rustfmt.toml 준수)
- `cargo clippy -- -D warnings` 통과 필수
- Public API에는 doc comment 작성
- Error handling: `thiserror` for library errors, `anyhow` for CLI/application errors
- 테스트: 단위 테스트는 같은 파일 내 `#[cfg(test)] mod tests`, 통합 테스트는 `tests/`

## Design Principles (from spec)
- Daemon은 도메인 로직을 모른다 — yaml에 정의된 prompt/script만 실행
- DataSource/AgentRuntime은 trait으로 추상화 — OCP 준수
- 환경변수는 WORK_ID + WORKTREE 2개만 주입
- 아이템은 한 방향으로만 흐른다 — 되돌아가지 않음
- 의심스러우면 HITL (safe default)

## Commit Convention
- Conventional Commits: `feat:`, `fix:`, `refactor:`, `docs:`, `ci:`, `test:`, `chore:`
- 한국어/영어 혼용 가능, 커밋 메시지 본문은 영어 권장

## Dependencies
- 새 dependency 추가 시 필요성 설명 필수
- 가능하면 기존 dependency로 해결
- `features`를 최소한으로 활성화
