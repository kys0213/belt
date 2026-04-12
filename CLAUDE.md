# Belt

Autonomous development conveyor belt — GitHub 이슈를 수집하여 LLM agent로 자동 처리하는 Rust 데몬.

## Language & Toolchain

- Rust edition 2024
- `cargo fmt` (rustfmt.toml 준수)
- `cargo clippy -- -D warnings` 통과 필수

## Workspace Structure

```
crates/
  belt-core/     # 도메인 모델, trait 정의 (phase, queue, runtime, source, escalation, spec)
  belt-infra/    # 외부 시스템 어댑터 (SQLite DB, GitHub gh CLI, Claude/Gemini/Codex runtime, worktree)
  belt-daemon/   # 실행 루프, cron 엔진, concurrency, evaluator, executor
  belt-cli/      # CLI 바이너리 (clap), TUI dashboard (ratatui), claw 세션
```

의존 방향: `belt-core → belt-infra → belt-daemon → belt-cli` (역방향 금지)

## Key Dependencies

| 용도 | 패키지 |
|------|--------|
| 직렬화 | serde, serde_json, serde_yaml |
| 에러 | thiserror (library), anyhow (application) |
| 비동기 | tokio (full), async-trait |
| DB | rusqlite (bundled) |
| CLI | clap (derive) |
| TUI | ratatui, crossterm |
| 로깅 | tracing, tracing-subscriber |

## Tests

단위 테스트는 같은 파일 내 `#[cfg(test)] mod tests`, 통합 테스트는 `crates/belt-daemon/tests/`:

```
crates/belt-daemon/tests/
  daemon_lifecycle.rs    # Daemon 풀 라이프사이클 (collect → advance → execute)
  escalation.rs          # 실패 에스컬레이션, HITL 진입/응답
  cron_integration.rs    # CronEngine tick, pause/resume, DB 동기화
  e2e_real.rs            # Real E2E (GitHub + Claude API, #[ignore])
  e2e_helpers.rs         # E2E 헬퍼 (gh CLI 래퍼, daemon 팩토리)
```

```bash
cargo test                                                    # 단위 + 통합 (E2E 제외)
cargo test -p belt-daemon -- --ignored --test-threads=1       # Real E2E (GitHub + Claude 필요)
```

## Build & Run

```bash
cargo build                          # 전체 빌드
cargo run -- status                  # 시스템 상태 조회
cargo run -- start --config workspace.yaml --tick 30   # 데몬 시작
```

## Release

Release Please가 conventional commits를 분석하여 Release PR을 생성한다.

```bash
gh workflow run release-please.yml   # Release PR 생성/업데이트 (수동)
# Release PR 머지 시 자동으로 태그 + GitHub Release 생성
```

- `feat:` → minor bump, `fix:`/`docs:`/`refactor:` → patch bump
- `BREAKING CHANGE:` footer → major bump
- `test:`, `ci:`, `chore:` → 릴리즈 제외
- Release PR 머지 시 자동으로 태그 생성 → release.yml 트리거 → 5개 플랫폼 바이너리 빌드 + GitHub Release

PR 제목도 conventional commit 형식을 따른다 (squash merge 시 커밋 메시지로 사용됨).

## 프로젝트 맥락

- **종류**: 제품 (CLI 도구 + 데몬 서비스)
- **팀**: 1인 개발
- **주요 독자**: Rust 개발자

## 엔지니어링 가치

- 가독성 > 성능 (프로파일링으로 확인된 병목만 최적화)
- 명시성 > 간결성 (trait 경계·에러 타입·match 브랜치 명시)
- 안정성 > 속도 (의심스러우면 HITL)
- 추상화는 Rule of 3 (3번 반복 전까지 명시적 중복 허용)

## 문서화

- **톤**: 기술적 한다체 (`~한다`, `~된다`)
- **언어**: README=영어, spec/rules=한국어 기반, 기술용어=영어 원어 유지
- **독자**: Rust 개발자 (코드 예시·trait 이름 자유롭게 사용)
- **구조**: 비교/분류는 테이블 우선, 핵심 제약은 blockquote 강조
