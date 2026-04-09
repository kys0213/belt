# Flow 1: 온보딩 — workspace 등록 → DataSource 설정 → Agent 초기화

> 사용자가 workspace를 등록하고, DataSource별 워크플로우를 설정하면 자동화가 시작된다.

---

## 1. Workspace 등록

```bash
belt workspace add --config workspace.yaml
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
        trigger: { label: "belt:analyze" }
        handlers:
          - prompt: "이슈를 분석하고 구현 가능 여부를 판단해줘"
        on_done:
          - script: |
              CTX=$(belt context $WORK_ID --json)
              ISSUE=$(echo $CTX | jq -r '.issue.number')
              REPO=$(echo $CTX | jq -r '.source.url')
              gh issue edit $ISSUE --remove-label "belt:analyze" -R $REPO
              gh issue edit $ISSUE --add-label "belt:implement" -R $REPO

      implement:
        trigger: { label: "belt:implement" }
        handlers:
          - prompt: "이슈를 구현해줘"
        on_done:
          - script: |
              CTX=$(belt context $WORK_ID --json)
              ISSUE=$(echo $CTX | jq -r '.issue.number')
              REPO=$(echo $CTX | jq -r '.source.url')
              TITLE=$(echo $CTX | jq -r '.issue.title')
              gh pr create --title "$TITLE" --body "Closes #$ISSUE" -R $REPO
              gh issue edit $ISSUE --remove-label "belt:implement" -R $REPO
              gh issue edit $ISSUE --add-label "belt:review" -R $REPO

      review:
        trigger: { label: "belt:review" }
        handlers:
          - prompt: "PR을 리뷰하고 품질을 평가해줘"
        on_done:
          - script: |
              CTX=$(belt context $WORK_ID --json)
              ISSUE=$(echo $CTX | jq -r '.issue.number')
              REPO=$(echo $CTX | jq -r '.source.url')
              gh issue edit $ISSUE --remove-label "belt:review" -R $REPO
              gh issue edit $ISSUE --add-label "belt:done" -R $REPO

    escalation:
      1: retry
      2: retry_with_comment
      3: hitl
      terminal: skip          # hitl timeout 시 적용 (skip 또는 replan)

    # 참고: on_done/on_fail은 v6 Phase 1에서 ScriptLifecycleHook 어댑터로 처리됨.
    # Phase 2에서 DataSource별 LifecycleHook impl로 대체 예정.

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
2. workspace 디렉토리 생성 (~/.belt/workspaces/auth-project/)
3. DataSource 인스턴스 생성 + Daemon에 등록
4. AgentRuntime 바인딩 (RuntimeRegistry 구성)
5. per-workspace cron seed (evaluate, gap-detection, knowledge-extract)
6. Agent 워크스페이스 초기화 확인
```

### 에러 시나리오

CLI가 등록 전에 즉시 검증하고, 실패 시 구체적 에러 메시지를 표시한다.

```
belt workspace add --config workspace.yaml

  yaml 파싱 실패:
    → "Error: workspace.yaml:12 — 'handlers' 필드가 필요합니다"

  repo 접근 불가:
    → "Error: https://github.com/org/repo 접근 불가 (401 Unauthorized)"
    → "hint: gh auth status로 인증 상태를 확인하세요"

  workspace 이름 중복:
    → "Error: workspace 'auth-project'가 이미 존재합니다"
    → "hint: belt workspace remove auth-project로 기존 workspace를 삭제하세요"

  DataSource 유형 미지원:
    → "Error: 'jira' DataSource는 아직 지원되지 않습니다 (v7+)"
```

모든 검증은 DB 기록 전에 수행된다. 실패 시 부수효과 없음.

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
belt workspace update <name> --config '<JSON>'
belt workspace config <name>    # 유효 설정 조회
belt workspace remove <name>    # cascade 삭제 (외부 시스템 데이터는 유지)
```

---

## 검증 시나리오

| 시나리오 | 입력 | 기대 상태 | 기대 side effect |
|---------|------|----------|-----------------|
| 정상 등록 | 유효한 workspace.yaml | DB에 workspace 등록됨 | DataSource 인스턴스 생성, cron seed 생성 |
| yaml 파싱 실패 | 잘못된 yaml | 등록 거부 | DB 변경 없음, 구체적 에러 메시지 |
| repo 접근 불가 | 잘못된 URL/인증 | 등록 거부 | DB 변경 없음, 인증 힌트 표시 |
| 이름 중복 | 기존 workspace와 동일 name | 등록 거부 | DB 변경 없음 |
| 미지원 DataSource | `sources.jira` (v6) | 등록 거부 | DB 변경 없음 |
| workspace 삭제 | `belt workspace remove` | DB에서 cascade 삭제 | 외부 시스템(GitHub 이슈 등) 데이터는 유지 |

---

### 관련 문서

- [DataSource](../concerns/datasource.md) — 상태 기반 워크플로우 정의
- [AgentRuntime](../concerns/agent-runtime.md) — RuntimeRegistry 구성
- [Agent](../concerns/agent-workspace.md) — Agent 워크스페이스 초기화
- [Cron 엔진](../concerns/cron-engine.md) — per-workspace cron seed
