# DataSource — 외부 시스템 추상화 + 워크플로우 정의

> 외부 시스템(GitHub, Slack, Jira, ...)을 추상화하고, 각 시스템의 언어로 자동화 워크플로우를 정의한다.
> 새 외부 시스템 추가 = 새 DataSource impl + yaml 설정, 코어 변경 0 (OCP).

---

## 역할

```
DataSource가 소유하는 것:
  1. 수집 — 어떤 조건에서 아이템을 감지하는가 (trigger)
  2. 컨텍스트 — 해당 아이템의 외부 시스템 정보를 어떻게 조회하는가 (context)

코어/yaml이 소유하는 것:
  3. 처리 — 감지된 아이템을 어떻게 처리하는가 (handlers: prompt/script)
  4. 전이 — 처리 완료 후 다음에 뭘 트리거하는가 (on_done script)
  5. 실패 반영 — 실패 시 외부 시스템에 어떻게 알리는가 (on_fail script)
  6. 실패 정책 — 실패 시 어떻게 escalation하는가 (escalation)

코어는 DataSource 내부를 모른다. collect() 결과를 큐에 넣고, 상태 전이만 관리.
```

---

## trait 정의

```rust
pub trait DataSource: Send + Sync {
    /// DataSource 이름 (예: "github", "jira")
    fn name(&self) -> &str;

    /// 외부 시스템에서 trigger 조건에 매칭되는 새 아이템 감지
    async fn collect(&mut self, workspace: &WorkspaceConfig) -> Result<Vec<QueueItem>>;

    /// 해당 아이템의 외부 시스템 컨텍스트를 조회
    /// autodev context CLI가 내부적으로 호출
    async fn get_context(&self, item: &QueueItem) -> Result<ItemContext>;
}
```

v4 대비 대폭 축소. `on_phase_enter`, `on_failed`, `on_done`, `before_task`, `after_task` 모두 제거.
- on_done/on_fail → yaml에 정의된 script가 처리 (gh CLI 등 직접 호출)
- worktree 셋업 → 인프라 레이어가 항상 처리
- escalation → yaml의 escalation 정책을 코어가 실행

---

## `autodev context` — 스크립트용 조회 CLI

script가 아이템 정보를 조회하는 유일한 방법. DataSource.get_context()를 내부적으로 호출한다.

```bash
autodev context $WORK_ID --json
```

### 왜 환경변수 대신 CLI인가

Daemon이 `$ISSUE_NUMBER`, `$REPO_URL` 같은 환경변수를 주입하면 DataSource마다 변수가 끝없이 늘어난다 (GitHub: `$ISSUE_NUMBER`, Jira: `$TICKET_KEY`, Slack: `$THREAD_TS`, ...). 대신 `autodev context`로 통일하고, DataSource별 context 스키마를 정의한다.

Daemon이 주입하는 환경변수는 **2개만**:

| 변수 | 설명 |
|------|------|
| `WORK_ID` | 큐 아이템 식별자 |
| `WORKTREE` | worktree 경로 |

### GitHub context 스키마

```json
{
  "work_id": "github:org/repo#42:implement",
  "workspace": "auth-project",
  "queue": {
    "phase": "Running",
    "state": "implement",
    "source_id": "github:org/repo#42"
  },
  "source": {
    "type": "github",
    "url": "https://github.com/org/repo",
    "default_branch": "main"
  },
  "issue": {
    "number": 42,
    "title": "JWT middleware 구현",
    "body": "...",
    "labels": ["autodev:implement"],
    "author": "irene"
  },
  "pr": {
    "number": 87,
    "head_branch": "feat/jwt-middleware",
    "review_comments": [...]
  },
  "history": [
    { "state": "analyze", "status": "done", "attempt": 1, "summary": "구현 가능" },
    { "state": "implement", "status": "failed", "attempt": 1, "error": "compile error" },
    { "state": "implement", "status": "running", "attempt": 2 }
  ],
  "worktree": "/tmp/autodev/auth-project-42"
}
```

### history는 append-only

같은 `source_id`의 모든 이벤트가 시간순으로 축적된다. 실패 횟수는 history에서 계산:

```bash
# on_fail script에서 실패 횟수 조회
FAILURES=$(echo $CTX | jq '[.history[] | select(.status=="failed" and .state=="implement")] | length')
```

별도 `failure_count` 컬럼 없이 history 조회만으로 충분.

### Jira context 스키마 (v6+)

```json
{
  "work_id": "jira:BE-123:analyze",
  "workspace": "backend-tasks",
  "queue": {
    "phase": "Running",
    "state": "analyze",
    "source_id": "jira:BE-123"
  },
  "source": {
    "type": "jira",
    "url": "https://jira.company.com/project/BE"
  },
  "ticket": {
    "key": "BE-123",
    "summary": "...",
    "status": "In Progress",
    "assignee": "irene"
  },
  "history": [...]
}
```

---

## 상태 기반 워크플로우

각 DataSource는 자기 시스템의 상태 표현으로 워크플로우를 정의한다. v5는 GitHub에 집중한다.

### GitHub (라벨 기반)

```yaml
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
```

### 향후 확장 (v6+)

DataSource trait을 구현하면 코어 변경 없이 새 외부 시스템을 추가할 수 있다.

| 시스템 | 상태 표현 | trigger 예시 |
|--------|----------|-------------|
| Jira | 티켓 status | `{ status: "To Analyze" }` |
| Slack | 리액션 | `{ reaction: "robot_face" }` |
| Linear | 라벨/status | `{ label: "autodev" }` |

---

## Handler

handler는 **prompt** 또는 **script** 두 가지 타입. 동일한 통합 액션 타입을 사용한다.

```yaml
handlers:
  - prompt: "이슈를 분석해줘"          # LLM (AgentRuntime.invoke(), worktree 안에서)
  - script: "scripts/lint-check.sh"   # 결정적 (bash, WORK_ID + WORKTREE 주입)
```

- **prompt**: 순수 작업 지시만 담당. 린트/컨벤션은 hooks와 rules가 단계 진입 시 자동 보장
- **script**: `autodev context $WORK_ID --json`으로 필요한 정보를 조회하여 사용

handler 배열은 Running 상태에서 순차 실행. 하나라도 실패 시 on_fail → escalation.

---

## on_done / on_fail / on_enter

모든 lifecycle hook은 **script 배열**로 정의. handler와 동일한 통합 액션 타입.

```yaml
states:
  implement:
    trigger: { label: "autodev:implement" }
    on_enter:                              # Running 진입 시 (선택)
      - script: |
          CTX=$(autodev context $WORK_ID --json)
          echo "시작: $(echo $CTX | jq -r '.issue.title')"
    handlers:
      - prompt: "이슈를 구현해줘"
    on_done:                               # 성공 시
      - script: |
          CTX=$(autodev context $WORK_ID --json)
          # PR 생성, 라벨 전환 등
    on_fail:                               # 실패 시 (escalation 전)
      - script: |
          CTX=$(autodev context $WORK_ID --json)
          ISSUE=$(echo $CTX | jq -r '.issue.number')
          REPO=$(echo $CTX | jq -r '.source.url')
          gh issue comment $ISSUE --body "구현 실패" -R $REPO
```

실행 주체: Daemon이 상태 전이 시점에 직접 실행.
- `on_enter`: Running 진입 후, handler 실행 전. **실패 시 handler를 실행하지 않고 즉시 escalation 정책을 적용**한다 (handler 실패와 동일한 경로). on_enter 실패도 history에 failed 이벤트로 기록되며 failure_count에 포함된다.
- `on_done`: evaluate가 Done 판정 후 (script 실패 시 → Failed 상태)
- `on_fail`: handler 또는 on_enter 실패 시, escalation level에 따라 조건부 실행 (`retry`에서는 실행 안 함)

---

## Escalation 정책

workspace yaml에서 실패 정책을 정의하고, 코어가 실행한다.

Escalation level은 **순차 실행 구간**과 **대안 선택 구간**으로 나뉜다:

- Level 1~3: 순차적으로 적용 (1회 실패 → retry, 2회 → retry_with_comment, 3회 → hitl)
- Level 4: **terminal 분기** — hitl 응답에서 사람이 선택하거나, `terminal` 설정으로 자동 적용

```yaml
escalation:
  1: retry                # 같은 state에서 재시도 (on_fail 실행 안 함)
  2: retry_with_comment   # on_fail script 실행 + 재시도
  3: hitl                 # on_fail script 실행 + HITL 이벤트 생성
  terminal: skip          # hitl에서 사람이 결정하지 않으면 (timeout) 적용되는 최종 액션
                          # 선택지: skip (종료) 또는 replan (스펙 수정 제안)
```

> **설계 의도**: level 4(skip)와 level 5(replan)는 순차적으로 도달할 수 없다. skip은 terminal 상태(Skipped)이므로 이후 실패가 발생하지 않는다. 따라서 skip과 replan은 level 3(hitl) 이후의 **대안적 선택지**로 재설계하였다.
>
> - `terminal: skip` — hitl timeout 시 해당 아이템을 건너뛰고 종료
> - `terminal: replan` — hitl timeout 시 스펙 수정을 제안하는 HITL(replan) 이벤트 생성
>
> 사람이 hitl에 직접 응답하는 경우, done/retry/skip/replan 중 자유롭게 선택할 수 있다.

### on_fail 실행 조건

`retry`만 on_fail script를 실행하지 않는다. 나머지(`retry_with_comment`, `hitl`)는 on_fail script 실행 후 해당 액션을 수행한다.

```
1회 실패 → retry           → 조용히 재시도 (worktree 보존)
2회 실패 → retry_with_comment → 외부 시스템에 실패 알림 + 재시도
3회 실패 → hitl            → 외부 시스템에 알림 + 사람 대기
                              └── 사람 응답: done / retry / skip / replan
                              └── timeout  → terminal 액션 적용 (skip 또는 replan)
```

failure_count는 history의 append-only 이벤트에서 계산. 코어는 `history | filter(state, failed) | count` → escalation 매핑만 알면 된다.

### Retry와 worktree

retry 시 worktree를 보존하여 이전 작업 위에서 재시도한다. 새 아이템이 같은 source_id로 생성되며, worktree 경로가 이전 아이템에서 인계된다.

---

## 아이템 계보 (Lineage)

같은 외부 엔티티에서 파생된 아이템들은 `source_id`로 연결된다.

```
source_id = "github:org/repo#42"

queue_items 테이블:
  work_id              | source_id            | state     | phase
  github:org/repo#42:a | github:org/repo#42   | analyze   | Done
  github:org/repo#42:i | github:org/repo#42   | implement | Running
  github:org/repo#42:r | github:org/repo#42   | review    | Pending
```

`autodev context $WORK_ID`는 source_id 기반으로 같은 엔티티의 이전 단계 이력(`history`)을 포함한다.

---

### 관련 문서

- [DESIGN-v5](../DESIGN-v5.md) — 전체 아키텍처
- [AgentRuntime](./agent-runtime.md) — handler prompt 실행
- [Cron 엔진](./cron-engine.md) — 품질 루프
- [CLI 레퍼런스](./cli-reference.md) — autodev context CLI
