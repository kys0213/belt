---
paths:
  - "CHANGELOG.md"
  - ".github/**"
---

# Commit Convention (Release Please 연동)

Conventional Commits 형식을 **반드시** 준수한다. Release Please가 커밋 메시지를 파싱하여 자동으로 버전을 결정한다.

## 형식

```
<type>(<scope>): <description>

[optional body]

[optional footer(s)]
```

## type → 버전 영향

| type | 버전 영향 | 용도 |
|------|----------|------|
| `feat` | **minor** bump (0.1.0 → 0.2.0) | 새 기능 추가 |
| `fix` | **patch** bump (0.1.0 → 0.1.1) | 버그 수정 |
| `docs` | patch bump | 문서 변경 |
| `refactor` | patch bump | 기능 변경 없는 코드 개선 |
| `test` | 릴리즈 제외 | 테스트 추가/수정 |
| `ci` | 릴리즈 제외 | CI/CD 변경 |
| `chore` | 릴리즈 제외 | 기타 (의존성 업데이트 등) |

## scope (권장)

crate 이름 또는 모듈: `core`, `infra`, `daemon`, `cli`, `tui`, `spec`

```
feat(cli): add belt bootstrap --llm flag
fix(daemon): prevent duplicate cron trigger on SIGUSR1
```

## Breaking Change → major bump

footer에 `BREAKING CHANGE:` 를 포함하면 major bump (0.x → 1.0.0 또는 1.x → 2.0.0).

```
feat(core)!: rename QueuePhase::Hitl to HumanInTheLoop

BREAKING CHANGE: QueuePhase enum variant renamed
```

## 규칙
- PR 제목도 conventional commit 형식을 따른다 (squash merge 시 PR 제목이 커밋 메시지가 됨)
- 한국어/영어 혼용 가능, description은 영어 권장
- `test:`, `ci:`, `chore:` 는 CHANGELOG에 포함되지 않음
