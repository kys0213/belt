---
paths:
  - "**/workspace*.yaml"
  - "**/workspace*.yml"
  - "tests/**/*.yaml"
---

# workspace.yaml 스키마 컨벤션

> workspace.yaml은 Belt의 단일 설정 파일(Single Source of Truth)이다. Daemon은 이 파일에 정의된 것만 실행한다.

## 원칙

1. **필수 필드**: `name`은 반드시 포함한다. `sources`(기본 빈 맵)와 `concurrency`(기본 1)는 생략 가능하나 명시를 권장한다.
2. **prompt/script 분리**: handler 하나는 `prompt` 또는 `script` 중 하나만 가진다. 혼용하지 않는다.
3. **환경변수 주입은 2개만**: 핸들러 실행 시 주입되는 환경변수는 `WORK_ID`와 `WORKTREE`뿐이다. 추가 변수를 기대하는 스크립트를 작성하지 않는다.
4. **on_done/on_fail은 side-effect 전용**: hook은 외부 시스템 반영(GitHub 댓글, 라벨 변경)을 담당한다. 도메인 로직을 넣지 않는다.

## DO

```yaml
# 필수 필드를 모두 포함한 최소 구성
name: my-project
concurrency: 2

sources:
  github:
    url: https://github.com/org/repo
    states:
      analyze:
        trigger:
          label: "belt:analyze"
        handlers:
          - prompt: "이슈를 분석하고 구현 계획을 작성해줘"
        on_done:
          - script: |
              gh issue comment "$ISSUE" --body "분석 완료: $WORK_ID" -R org/repo
        on_fail:
          - script: |
              gh issue comment "$ISSUE" --body "분석 실패: $WORK_ID" -R org/repo
    escalation:
      1: retry
      2: hitl

runtime:
  default: claude
  claude:
    model: claude-sonnet-4-20250514
```

```yaml
# prompt와 script를 별도 handler로 분리
handlers:
  - prompt: "구현 계획을 작성해줘"
  - script: "cargo test"
  - script: "cargo clippy -- -D warnings"
```

## DON'T

```yaml
# prompt와 script를 한 handler에 혼용하지 않는다
handlers:
  - prompt: "구현해줘"
    script: "cargo test"   # 나쁨

# WORK_ID, WORKTREE 외 환경변수를 기대하지 않는다
handlers:
  - script: "deploy.sh $DEPLOY_ENV"   # 나쁨 — DEPLOY_ENV는 주입되지 않음

# on_done에 도메인 로직을 넣지 않는다
on_done:
  - script: |
      if [ "$STATUS" = "success" ]; then   # 나쁨 — STATUS는 없음
        gh pr merge ...
      fi

# escalation에 terminal 없이 hitl만 쓰면 무한 HITL 가능
escalation:
  1: hitl   # 나쁨 — terminal 정책 없음
```

## 체크리스트

- [ ] `name` 필드가 있는가 (`sources`, `concurrency`는 명시 권장)
- [ ] 각 handler에 `prompt` 또는 `script` 중 하나만 있는가
- [ ] 스크립트가 `WORK_ID`, `WORKTREE` 외 환경변수에 의존하지 않는가
- [ ] `on_done`/`on_fail`이 외부 시스템 반영(GitHub 댓글 등)만 담당하는가
- [ ] `escalation`에 최종 처리 정책(`hitl` 또는 `skip`)이 명시됐는가
