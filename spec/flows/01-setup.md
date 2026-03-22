# Flow 1: 온보딩 — workspace 등록 → DataSource 설정 → Claw 초기화

> 사용자가 workspace를 등록하고, DataSource별 워크플로우를 설정하면 자동화가 시작된다.

---

## 1. Workspace 등록

```bash
autodev workspace add --config workspace.yaml
```

### workspace.yaml

```yaml
name: "auth-project"
concurrency: 2                    # workspace 레벨: 동시 Running 아이템 수

sources:
  github:
    url: https://github.com/org/repo
    scan_interval_secs: 300

    states:
      analyze:
        trigger: { label: "autodev:analyze" }
        handlers:
          - prompt: "이슈를 분석하고 구현 가능 여부를 판단해줘"
        on_done:
          - script: |
              CTX=$(autodev context $WORK_ID --json)
              ISSUE=$(echo $CTX | jq -r '.issue.number')
              REPO=$(echo $CTX | jq -r '.source.url')
              gh issue edit $ISSUE --remove-label "autodev:analyze" -R $REPO
              gh issue edit $ISSUE --add-label "autodev:implement" -R $REPO

      implement:
        trigger: { label: "autodev:implement" }
        handlers:
          - prompt: "이슈를 구현해줘"
        on_done:
          - script: |
              CTX=$(autodev context $WORK_ID --json)
              ISSUE=$(echo $CTX | jq -r '.issue.number')
              REPO=$(echo $CTX | jq -r '.source.url')
              TITLE=$(echo $CTX | jq -r '.issue.title')
              gh pr create --title "$TITLE" --body "Closes #$ISSUE" -R $REPO
              gh issue edit $ISSUE --remove-label "autodev:implement" -R $REPO
              gh issue edit $ISSUE --add-label "autodev:review" -R $REPO

      review:
        trigger: { label: "autodev:review" }
        handlers:
          - prompt: "PR을 리뷰하고 품질을 평가해줘"
        on_done:
          - script: |
              CTX=$(autodev context $WORK_ID --json)
              ISSUE=$(echo $CTX | jq -r '.issue.number')
              REPO=$(echo $CTX | jq -r '.source.url')
              gh issue edit $ISSUE --remove-label "autodev:review" -R $REPO
              gh issue edit $ISSUE --add-label "autodev:done" -R $REPO

    escalation:
      1: retry
      2: retry_with_comment
      3: hitl
      terminal: skip          # hitl timeout 시 적용 (skip 또는 replan)

runtime:
  default: claude
  claude:
    model: sonnet
```

### workspace = 1 repo

workspace는 하나의 외부 레포와 1:1로 대응한다. GitHub 기준으로는 1 workspace = 1 GitHub repo. 다른 DataSource 타입도 해당 시스템에서 "레포"에 해당하는 단위와 1:1 매핑.

### 기대 동작

```
1. DB에 workspace 등록
2. workspace 디렉토리 생성 (~/.autodev/workspaces/auth-project/)
3. DataSource 인스턴스 생성 + Daemon에 등록
4. AgentRuntime 바인딩 (RuntimeRegistry 구성)
5. per-workspace cron seed (evaluate, gap-detection, knowledge-extract)
6. Claw 워크스페이스 초기화 확인
```

---

## 2. 컨벤션 부트스트랩

레포의 `.claude/rules/`가 비어있다면 기술 스택 기반 컨벤션 자동 생성.

```
1. .claude/rules/ 존재 확인 → 있으면 skip
2. 기술 스택 추출 → 카테고리별 컨벤션 제안 (대화형)
3. 사용자 승인 → PR로 커밋
```

---

## 3. Workspace 관리

```bash
autodev workspace update <name> --config '<JSON>'
autodev workspace config <name>    # 유효 설정 조회
autodev workspace remove <name>    # cascade 삭제 (외부 시스템 데이터는 유지)
```

---

### 관련 문서

- [DataSource](../concerns/datasource.md) — 상태 기반 워크플로우 정의
- [AgentRuntime](../concerns/agent-runtime.md) — RuntimeRegistry 구성
- [Claw](../concerns/claw-workspace.md) — Claw 워크스페이스 초기화
- [Cron 엔진](../concerns/cron-engine.md) — per-workspace cron seed
