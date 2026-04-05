# Data Model

> 관련 문서: [DESIGN-v6](../DESIGN-v6.md), [QueuePhase 상태 머신](./queue-state-machine.md), [DataSource](./datasource.md), [Cron 엔진](./cron-engine.md), [Stagnation](./stagnation.md)

Belt의 모든 상태는 SQLite 단일 파일(`~/.belt/belt.db`)에 저장된다. 이 문서는 테이블 스키마, 도메인 모델, 직렬화 규칙을 한 곳에 정의한다.

---

## 테이블 개요

| 테이블 | 역할 | PK |
|--------|------|----|
| `queue_items` | 컨베이어 벨트 위의 작업 단위 | `work_id` |
| `history` | 작업 시도 기록 (append-only) | `id` (auto) |
| `transition_events` | phase 전이 이벤트 로그 | `id` |
| `queue_dependencies` | 아이템 간 실행 순서 제약 | `(work_id, depends_on)` |
| `specs` | 스펙 정의 및 라이프사이클 | `id` |
| `spec_links` | 스펙 ↔ 외부 리소스 연결 | `id` |
| `workspaces` | 등록된 워크스페이스 메타 | `name` |
| `cron_jobs` | 예약 작업 정의 | `name` |
| `token_usage` | LLM 토큰 사용량 추적 | `id` (auto) |
| `knowledge_base` | PR에서 추출한 지식 | `id` (auto) |

---

## 테이블 스키마

### queue_items

컨베이어 벨트의 작업 단위. 하나의 아이템은 하나의 워크플로우 상태(analyze, implement 등)에 대응한다.

```sql
CREATE TABLE queue_items (
    work_id              TEXT PRIMARY KEY,       -- '{source_id}:{state}'
    source_id            TEXT NOT NULL,           -- 'github:org/repo#42'
    workspace_id         TEXT NOT NULL,           -- workspace 이름
    state                TEXT NOT NULL,           -- 워크플로우 상태 ('analyze', 'implement' 등)
    phase                TEXT NOT NULL,           -- QueuePhase enum (lowercase)
    title                TEXT,                    -- 표시용 제목
    created_at           TEXT NOT NULL,           -- RFC3339
    updated_at           TEXT NOT NULL,           -- RFC3339

    -- HITL 메타데이터 (phase = 'hitl' 일 때 유효)
    hitl_created_at      TEXT,                    -- HITL 진입 시각
    hitl_respondent      TEXT,                    -- 응답한 사용자
    hitl_notes           TEXT,                    -- 사용자 메모
    hitl_reason          TEXT,                    -- HitlReason enum (snake_case)
    hitl_timeout_at      TEXT,                    -- 만료 시각 (RFC3339)
    hitl_terminal_action TEXT,                    -- EscalationAction enum (snake_case) ← v6: Option<String> → Option<EscalationAction>

    -- 추적 필드
    replan_count         INTEGER NOT NULL DEFAULT 0,  -- 재계획 횟수 (max 3)
    worktree_preserved   INTEGER NOT NULL DEFAULT 0   -- 1 = worktree 보존됨
);
```

**v6 변경 (#720)**: `hitl_terminal_action`은 `EscalationAction` enum 값만 허용한다. DB에는 snake_case 문자열로 저장, 로드 시 `FromStr`로 파싱한다. 유효하지 않은 값은 파싱 에러.

**인메모리 전용 필드** (DB에 저장하지 않음):
- `previous_worktree_path: Option<String>` — retry 시 이전 아이템의 worktree 경로를 전달하기 위한 transient 필드

### history

작업 시도 기록. append-only로만 쓰고, 읽기 전용으로 조회한다. `failure_count`는 이 테이블에서 계산한다. **Stagnation detection도 이 테이블의 summary/error를 입력으로 사용한다.**

```sql
CREATE TABLE history (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    work_id    TEXT NOT NULL,
    source_id  TEXT NOT NULL,
    state      TEXT NOT NULL,           -- 워크플로우 상태
    status     TEXT NOT NULL,           -- 'running' | 'done' | 'failed' | 'skipped' | 'hitl'
    attempt    INTEGER NOT NULL,        -- 시도 번호 (1-based)
    summary    TEXT,                    -- 결과 요약
    error      TEXT,                    -- 에러 메시지 (실패 시)
    created_at TEXT NOT NULL            -- RFC3339
);
```

**파생 쿼리**:
- `failure_count`: `SELECT COUNT(*) FROM history WHERE source_id = ? AND state = ? AND status = 'failed'`
- `max_attempt`: `SELECT MAX(attempt) FROM history WHERE source_id = ? AND state = ?`
- **stagnation 입력**: `SELECT summary, error FROM history WHERE source_id = ? AND state = ? ORDER BY attempt DESC LIMIT ?`

### transition_events

phase 전이 이벤트. history와 달리 phase 변경에 초점을 맞춘 상세 로그.

```sql
CREATE TABLE transition_events (
    id         TEXT PRIMARY KEY,        -- UUID
    work_id    TEXT NOT NULL,
    source_id  TEXT NOT NULL,
    event_type TEXT NOT NULL,           -- 'phase_enter' | 'handler' | 'evaluate' | 'on_done' | 'on_fail' | 'stagnation'
    phase      TEXT,                    -- 진입한 phase
    from_phase TEXT,                    -- 이전 phase
    detail     TEXT,                    -- 사람이 읽을 수 있는 설명
    created_at TEXT NOT NULL            -- RFC3339
);
```

**v6 변경 (#723)**: `event_type`에 `'stagnation'` 추가. `detail`에 탐지 패턴, confidence, evidence, 가속 결과를 JSON으로 기록한다.

### queue_dependencies

아이템 간 실행 순서 제약. `depends_on` 아이템이 Done이 아니면 `work_id` 아이템은 Ready→Running 전이가 블로킹된다.

**v6 변경 (#721)**: dependency phase 확인은 **DB 조회 기반**이다 (in-memory queue가 아님). 재시작 후에도 정확히 동작한다.

```sql
CREATE TABLE queue_dependencies (
    work_id    TEXT NOT NULL,
    depends_on TEXT NOT NULL,
    created_at TEXT NOT NULL,           -- RFC3339
    PRIMARY KEY (work_id, depends_on)
);
```

### specs

스펙 정의. 6-status 라이프사이클을 따른다.

```sql
CREATE TABLE specs (
    id                TEXT PRIMARY KEY,     -- UUID
    workspace_id      TEXT NOT NULL,
    name              TEXT NOT NULL,
    status            TEXT NOT NULL,         -- SpecStatus enum (lowercase)
    content           TEXT NOT NULL,         -- 마크다운 본문
    priority          INTEGER,              -- 낮을수록 높은 우선순위
    labels            TEXT,                 -- 쉼표 구분 레이블
    depends_on        TEXT,                 -- 쉼표 구분 의존 spec ID
    entry_point       TEXT,                 -- 쉼표 구분 파일/모듈 경로
    decomposed_issues TEXT,                 -- 쉼표 구분 GitHub 이슈 번호
    test_commands     TEXT,                 -- 쉼표 구분 검증 명령어
    created_at        TEXT NOT NULL,         -- RFC3339
    updated_at        TEXT NOT NULL          -- RFC3339
);
```

### spec_links

스펙과 외부 리소스(이슈 URL, PR 등) 간 연결.

```sql
CREATE TABLE spec_links (
    id         TEXT PRIMARY KEY,
    spec_id    TEXT NOT NULL,
    target     TEXT NOT NULL,           -- URL 또는 'owner/repo#123'
    created_at TEXT NOT NULL,
    UNIQUE(spec_id, target)
);
```

### workspaces

등록된 워크스페이스. yaml 파일 경로를 참조한다.

```sql
CREATE TABLE workspaces (
    name        TEXT PRIMARY KEY,
    config_path TEXT NOT NULL,          -- workspace.yaml 절대 경로
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
```

### cron_jobs

예약 작업. built-in(evaluate, hitl-timeout 등)과 사용자 정의 모두 저장.

```sql
CREATE TABLE cron_jobs (
    name        TEXT PRIMARY KEY,
    schedule    TEXT NOT NULL,           -- cron 표현식 ('*/5 * * * *')
    script      TEXT NOT NULL DEFAULT '',
    workspace   TEXT,                    -- NULL = 글로벌
    enabled     INTEGER NOT NULL DEFAULT 1,
    last_run_at TEXT,                    -- RFC3339, force_trigger 시 NULL로 리셋
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL DEFAULT ''
);
```

### token_usage

LLM 호출 토큰 사용량. `belt status`와 TUI Dashboard에서 집계 표시.

```sql
CREATE TABLE token_usage (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    work_id            TEXT NOT NULL,
    workspace          TEXT NOT NULL,
    runtime            TEXT NOT NULL,       -- 'claude' | 'gemini' | 'codex'
    model              TEXT NOT NULL,       -- 'opus' | 'sonnet' | 'haiku' 등
    input_tokens       INTEGER NOT NULL,
    output_tokens      INTEGER NOT NULL,
    cache_read_tokens  INTEGER,
    cache_write_tokens INTEGER,
    duration_ms        INTEGER,
    created_at         TEXT NOT NULL        -- RFC3339
);
```

### knowledge_base

merged PR에서 추출한 지식. knowledge-extract cron이 저장.

```sql
CREATE TABLE knowledge_base (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace  TEXT NOT NULL,
    source_ref TEXT NOT NULL,           -- 'PR #42'
    category   TEXT NOT NULL,           -- 'decision' | 'pattern' | 'domain' | 'review_feedback'
    content    TEXT NOT NULL,
    created_at TEXT NOT NULL
);
```

---

## 도메인 Enum

### QueuePhase

8개 상태. 직렬화 시 lowercase.

| Variant | 직렬화 | Terminal | 설명 |
|---------|--------|----------|------|
| `Pending` | `"pending"` | No | DataSource가 감지, 큐 대기 |
| `Ready` | `"ready"` | No | 실행 준비 완료 (자동 전이) |
| `Running` | `"running"` | No | worktree 생성 + handler 실행 중 |
| `Completed` | `"completed"` | No | handler 성공, evaluate 대기 |
| `Done` | `"done"` | **Yes** | evaluate 완료 + on_done 성공 |
| `Hitl` | `"hitl"` | No | 사람 판단 필요 |
| `Failed` | `"failed"` | No | on_done 실패 또는 인프라 오류 |
| `Skipped` | `"skipped"` | **Yes** | escalation skip 또는 preflight 실패 |

**v6 (#718)**: `phase` 필드는 `pub(crate)` 가시성. 외부에서 직접 대입 불가, 반드시 `QueueItem::transit()` 경유. 상세: [QueuePhase 상태 머신](./queue-state-machine.md)

### SpecStatus

6개 상태. 직렬화 시 lowercase.

| Variant | 직렬화 | 설명 |
|---------|--------|------|
| `Draft` | `"draft"` | 초기 상태 |
| `Active` | `"active"` | 활성 (이슈 생성/처리 진행) |
| `Paused` | `"paused"` | 일시 중단 |
| `Completing` | `"completing"` | 모든 이슈 Done + gap 없음, HITL 대기 |
| `Completed` | `"completed"` | 최종 완료 |
| `Archived` | `"archived"` | 소프트 삭제 |

### HitlReason

HITL 생성 경로. 직렬화 시 snake_case.

| Variant | 직렬화 | 설명 |
|---------|--------|------|
| `EvaluateFailure` | `"evaluate_failure"` | evaluate 반복 실패 |
| `RetryMaxExceeded` | `"retry_max_exceeded"` | 재시도 횟수 초과 |
| `Timeout` | `"timeout"` | 실행 타임아웃 |
| `ManualEscalation` | `"manual_escalation"` | 사용자 수동 요청 |
| `SpecConflict` | `"spec_conflict"` | 스펙 파일 겹침 |
| `SpecCompletionReview` | `"spec_completion_review"` | 스펙 완료 최종 확인 |
| `SpecModificationProposed` | `"spec_modification_proposed"` | Agent 수정 제안 |
| `StagnationDetected` | `"stagnation_detected"` | **v6** 반복 패턴 감지 + lateral thinking 사고 전환 |

### EscalationAction

failure_count별 대응. 직렬화 시 snake_case.

| Variant | 직렬화 | on_fail 실행 | 설명 |
|---------|--------|:------------:|------|
| `Retry` | `"retry"` | **No** | 조용한 재시도 |
| `RetryWithComment` | `"retry_with_comment"` | Yes | on_fail + 재시도 |
| `Hitl` | `"hitl"` | Yes | on_fail + HITL 생성 |
| `Skip` | `"skip"` | Yes | on_fail + Skipped |
| `Replan` | `"replan"` | Yes | on_fail + HITL(replan) |

**v6 (#720)**: `EscalationAction`은 `FromStr` + `Display` impl을 가진다. `hitl_terminal_action` 필드 타입으로도 사용된다.

```rust
impl FromStr for EscalationAction {
    type Err = BeltError;
    fn from_str(s: &str) -> Result<Self, Self::Err> { /* snake_case 파싱 */ }
}
```

### StagnationPattern (v6 신규)

정체 패턴 유형. 직렬화 시 snake_case.

| Variant | 직렬화 | 설명 |
|---------|--------|------|
| `Spinning` | `"spinning"` | 동일/유사 출력 반복 (A→A→A) |
| `Oscillation` | `"oscillation"` | 교대 반복 (A→B→A→B) |
| `NoDrift` | `"no_drift"` | 진행 점수 정체 |
| `DiminishingReturns` | `"diminishing_returns"` | 개선폭 감소 |

SPINNING/OSCILLATION은 `CompositeSimilarity`로 유사도 판단, NO_DRIFT/DIMINISHING은 drift score 수치 비교.

### Persona (v6 신규)

Lateral Thinking 사고 전환 페르소나. belt-core에 `include_str!`로 내장. 직렬화 시 snake_case.

| Variant | 직렬화 | 패턴 친화도 | 전략 |
|---------|--------|-----------|------|
| `Hacker` | `"hacker"` | SPINNING | 제약 우회, 워크어라운드 |
| `Architect` | `"architect"` | OSCILLATION | 구조 재설계, 관점 전환 |
| `Researcher` | `"researcher"` | NO_DRIFT | 정보 수집, 체계적 디버깅 |
| `Simplifier` | `"simplifier"` | DIMINISHING | 복잡도 축소, 가정 제거 |
| `Contrarian` | `"contrarian"` | 복합/기타 | 가정 뒤집기, 문제 역전 |

상세: [Stagnation Detection](./stagnation.md)

### HistoryStatus

history 테이블의 status 컬럼. 직렬화 시 lowercase.

| Variant | 직렬화 |
|---------|--------|
| `Running` | `"running"` |
| `Done` | `"done"` |
| `Failed` | `"failed"` |
| `Skipped` | `"skipped"` |
| `Hitl` | `"hitl"` |

---

## 액션 타입

handler와 lifecycle hook은 서로 다른 타입을 사용한다.

### HandlerConfig (yaml 설정)

handler 배열에서 사용. prompt + script 모두 가능.

```yaml
handlers:
  - prompt: "이슈를 분석하세요"
    runtime: claude            # optional
    model: sonnet              # optional
  - script: "cargo test"
```

### ScriptAction (yaml 설정)

lifecycle hook(`on_enter`, `on_done`, `on_fail`)에서 사용. **script만 허용**.

```yaml
on_done:
  - script: "gh pr create ..."
on_fail:
  - script: "gh issue comment ..."
```

### Action (런타임 추상화)

코어의 실행 단위. `HandlerConfig`와 `ScriptAction` 모두 `Action`으로 변환되어 실행된다.

```
HandlerConfig::Prompt  → Action::Prompt { text, runtime, model }
HandlerConfig::Script  → Action::Script { command }
ScriptAction           → Action::Script { command }
```

---

## Workspace yaml 설정 모델

### WorkspaceConfig

```yaml
name: my-project
concurrency: 2                    # workspace 동시 Running 수 (default: 1)

sources:
  github:
    url: "https://github.com/org/repo"
    scan_interval_secs: 300       # default: 300
    states:
      analyze:
        trigger: { label: "belt:analyze" }
        handlers:
          - prompt: "이슈를 분석하세요"
        on_done:
          - script: "gh issue edit ... --add-label belt:implement"
      # ... 추가 states
    escalation:
      1: retry
      2: retry_with_comment
      3: hitl
      terminal: skip              # HITL 만료 시 ('skip' | 'replan')

# v6 신규: stagnation 탐지 + lateral thinking 설정
stagnation:
  enabled: true                    # default: true
  spinning_threshold: 3            # default: 3
  oscillation_cycles: 2            # default: 2
  similarity_threshold: 0.8        # composite score 유사 판정 (default: 0.8)
  no_drift_epsilon: 0.01           # default: 0.01
  no_drift_iterations: 3           # default: 3
  diminishing_threshold: 0.01      # default: 0.01
  confidence_threshold: 0.5        # default: 0.5

  similarity:                      # CompositeSimilarity (Composite Pattern)
    - judge: exact_hash            # 기본 프리셋
      weight: 0.5
    - judge: token_fingerprint
      weight: 0.3
    - judge: ncd
      weight: 0.2

  lateral:
    enabled: true                  # default: true
    max_attempts: 3                # 페르소나 최대 시도 (default: 3)

runtime:
  default: claude
  claude:
    model: sonnet
  gemini:
    model: pro
```

### TriggerConfig

| 필드 | 타입 | 설명 |
|------|------|------|
| `label` | `Option<String>` | 라벨 매칭 트리거 |
| `changes_requested` | `bool` | PR CHANGES_REQUESTED 트리거 (default: false) |

---

## 컨텍스트 모델 (belt context 출력)

`belt context $WORK_ID --json`이 반환하는 구조. script가 정보를 조회하는 유일한 방법.

**v6 변경 (#719)**: `source_data` 필드 추가. DataSource별 자유 스키마. 기존 `issue`/`pr` 필드는 호환성을 위해 유지.

```json
{
  "work_id": "github:org/repo#42:implement",
  "workspace": "my-project",
  "queue": {
    "phase": "running",
    "state": "implement",
    "source_id": "github:org/repo#42"
  },
  "source": {
    "type": "github",
    "url": "https://github.com/org/repo",
    "default_branch": "main"
  },
  "source_data": {
    "issue": {
      "number": 42,
      "title": "...",
      "body": "...",
      "labels": ["belt:implement"],
      "author": "user",
      "state": "open"
    },
    "pr": {
      "number": 43,
      "title": "...",
      "state": "open",
      "draft": false,
      "head_branch": "belt/42-implement",
      "base_branch": "main",
      "reviews": [
        { "reviewer": "user", "state": "APPROVED" }
      ]
    }
  },
  "issue": {
    "number": 42,
    "title": "...",
    "body": "...",
    "labels": ["belt:implement"],
    "author": "user",
    "state": "open"
  },
  "pr": {
    "number": 43,
    "title": "...",
    "state": "open",
    "draft": false,
    "head_branch": "belt/42-implement",
    "base_branch": "main",
    "reviews": [
      { "reviewer": "user", "state": "APPROVED" }
    ]
  },
  "history": [
    {
      "work_id": "github:org/repo#42:analyze",
      "state": "analyze",
      "status": "done",
      "attempt": 1,
      "created_at": "2026-03-25T10:00:00Z"
    }
  ],
  "worktree": "/tmp/belt/worktrees/42-implement"
}
```

### source_data 마이그레이션 전략 (#719)

| Phase | 상태 | 설명 |
|-------|------|------|
| **1 (v6)** | `source_data` + `issue`/`pr` 양쪽 채움 | 하위 호환. 기존 script 수정 불필요 |
| **2 (v7+)** | `issue`/`pr` deprecated | script가 `source_data` 경로로 전환 |
| **3 (v8+)** | `issue`/`pr` 제거 | `source_data`만 사용 |

script에서의 접근:
```bash
# v6 (양쪽 모두 가능)
belt context $WORK_ID --json | jq '.issue.number'
belt context $WORK_ID --json | jq '.source_data.issue.number'

# v7+ (source_data 권장)
belt context $WORK_ID --json | jq '.source_data.issue.number'

# Jira DataSource (v7+)
belt context $WORK_ID --json | jq '.source_data.ticket.key'
```

---

## 타임스탬프 규칙

- 모든 `created_at`, `updated_at`: **RFC3339 문자열** (`"2026-03-27T12:30:45Z"`)
- 생성 시: `Utc::now().to_rfc3339()`
- 파싱 시: `DateTime::parse_from_rfc3339()` → `DateTime<Utc>`
- SQLite에 TEXT로 저장 (네이티브 datetime 미사용)

## 직렬화 규칙

| 대상 | serde 설정 |
|------|-----------|
| enum variant | `#[serde(rename_all = "lowercase")]` 또는 `"snake_case"` |
| Optional 필드 | `#[serde(skip_serializing_if = "Option::is_none")]` |
| bool default false | `#[serde(default, skip_serializing_if = "std::ops::Not::not")]` |
| Vec default empty | `#[serde(default, skip_serializing_if = "Vec::is_empty")]` |
| u32 default 0 | `#[serde(default, skip_serializing_if = "is_zero")]` |

---

## 테이블 관계

```
queue_items.work_id ──< history.work_id
queue_items.work_id ──< transition_events.work_id
queue_items.work_id ──< token_usage.work_id
queue_items.work_id ──< queue_dependencies.work_id
queue_items.source_id ─── (같은 외부 엔티티를 공유하는 아이템들을 연결)

specs.id ──< spec_links.spec_id

workspaces.name ──< queue_items.workspace_id
workspaces.name ──< specs.workspace_id
workspaces.name ──< cron_jobs.workspace
workspaces.name ──< token_usage.workspace
workspaces.name ──< knowledge_base.workspace
```

> 참고: FK 제약은 SQLite에서 명시적으로 선언하지 않는다. 애플리케이션 레이어에서 정합성을 보장한다.
