# Flow 5: 모니터링 — 칸반 보드 + 시각화

> 사용자가 다수 workspace x 다수 스펙의 전체 진행 상황을 TUI, CLI, /claw 세션에서 일관되게 확인한다.

---

## 1. 인터페이스 3개 레이어

| 레이어 | 진입 | 특징 |
|--------|------|------|
| TUI Dashboard | `autodev dashboard` | ratatui, 실시간 갱신, 키보드 네비게이션 |
| CLI 출력 | `autodev status --format rich` | 정적 스냅샷, 코어 CLI |
| Claw 세션 | `/claw` → "보드 보여줘" | 자연어, 읽기 전용 조회의 주 인터페이스 |

> **Phase 구분**: `status`, `spec status` 등 코어 CLI는 Phase 1로 직접 구현. `board`, `decisions`, `logs` 등 읽기 전용 조회는 `/claw`가 흡수 (Phase 2, 필요 시 독립 CLI 추가).

---

## 2. TUI Dashboard

### AllWorkspaces 뷰 (기본)

```
┌─ Workspaces ─────┐┌─ Active Items ─────────┐┌─ Runtime ─────────────┐
│ ● auth-project    ││ #42 Completed (eval)   ││ claude/sonnet  12 OK  │
│ ○ backend-tasks   ││ #44 Running            ││ Tokens: 45.2K / 1h   │
│                   ││                        ││ Avg: 4m 32s          │
├───────────────────┤├────────────────────────┤├───────────────────────┤
│ Logs              ││ DataSource             ││ Scripts (1h)          │
│ 14:30 done #42    ││ github ● connected     ││ on_done     8 ok      │
│ 14:25 eval ..     ││   scan: 30s ago        ││ on_fail     1 ok      │
│ 14:20 skip        ││                        ││ evaluate   12 ok      │
│ 14:15 ⚠ fail #39 ││                        ││ ⚠ failed    1        │
└───────────────────┘└────────────────────────┘└───────────────────────┘
```

### PerWorkspace 뷰 (Tab 전환)

```
┌─ Board ──────────────────────────────┐┌─ Active ──────────────┐
│ auth-v2  ████████░░ 60% (3/5)        ││ #44 Running           │
│   ✅ #42 JWT middleware               ││   claude/sonnet        │
│   ✅ #43 Token API                    ││   3m elapsed           │
│   🔄 #44 Session adapter (running)   ││                        │
│   ⏳ #45 Error handling (dep: #44)   ││ #42 Completed (eval)  │
│   ⚠ #39 Auth refactor (failed)      ││   evaluate 대기        │
│   ⏳ #46 Missing tests               ││                        │
├──────────────────────────────────────┤├────────────────────────┤
│ Orphan: 0 | HITL: 1 pending         ││ Logs                   │
│ Kanban: 0P | 0Re | 1Ru | 1C | 2D   ││ ...                    │
└──────────────────────────────────────┘└────────────────────────┘
```

> **Kanban 약어**: P=Pending, Re=Ready, Ru=Running, C=Completed, D=Done, H=HITL, S=Skipped, F=Failed

### 전이 타임라인 (ItemDetail 오버레이, Enter)

```
┌─ #42 JWT middleware ─────────────────────────┐
│ Phase: Done | Runtime: claude/sonnet         │
│                                               │
│ Timeline:                                     │
│  14:00 ○ Pending  ← github.collect()         │
│  14:00 ○ Ready    ← auto                     │
│  14:01 ○ Running                              │
│         ├ worktree: /tmp/autodev/auth-42      │
│         └ handler: claude/sonnet (1.2K, 6m)  │
│  14:07 ○ Completed ← handlers 성공           │
│  14:07 ○ evaluate  → Done                    │
│  14:07 ○ on_done script (exit 0, 2s)         │
│  14:07 ● Done                                │
│         └ worktree 정리                       │
└───────────────────────────────────────────────┘
```

### 실패 아이템 타임라인

```
┌─ #39 Auth refactor ──────────────────────────┐
│ Phase: Failed | Runtime: claude/sonnet       │
│                                               │
│ Timeline:                                     │
│  13:00 ○ Pending  ← github.collect()         │
│  13:00 ○ Running                              │
│         └ handler: claude/sonnet (2.1K, 8m)  │
│  13:08 ○ Completed                            │
│  13:08 ○ evaluate  → Done                    │
│  13:08 ✗ on_done script (exit 1, 0.5s)       │
│         └ error: gh pr create rate limited    │
│  13:08 ● Failed                              │
│         └ worktree 보존: /tmp/autodev/auth-39 │
│                                               │
│ Actions: [r] retry-script  [s] skip          │
└───────────────────────────────────────────────┘
```

### 키보드

| 키 | 동작 |
|----|------|
| j/k, ↑/↓ | 아이템 이동 |
| ←/→ | workspace 전환 |
| Tab | AllWorkspaces ↔ PerWorkspace |
| Enter | 상세 / 전이 타임라인 |
| h | HITL 오버레이 |
| s | Spec 상세 |
| d | 판단 이력 |
| R | 새로고침 |

---

## 3. CLI 출력

### `--format` 옵션 (CLI 공통)

| 값 | 용도 |
|---|------|
| `text` | 기본 텍스트 (기존 호환) |
| `json` | 구조화된 JSON (Claw 파싱용) |
| `rich` | 색상 + 박스 + 진행률 바 (터미널용) |

모든 CLI 서브커맨드(status, board, spec list, spec status, queue list 등)에 적용.

### `autodev status --format rich`

```
● autodev daemon (uptime 2h 15m)

Workspaces:
  auth-project  ● active   queue: 1P 1R 1C 2D 1F   specs: 2/3
  backend-tasks ● active   queue: 0P 0R 0C 5D       specs: 1/1 ✓

Runtime: claude/sonnet (45.2K tokens/1h)
HITL: 1 pending ⚠
Failed: 1 ⚠
Next evaluate: 25s
```

### `autodev spec status <id> --format rich`

```
auth-v2  Auth Module v2
Status: Active | Runtime: claude/sonnet
Progress: ████████░░░░ 60% (3/5)

Issues:
  ✅ #42 JWT middleware       Done        6m   1.2K tokens
  ✅ #43 Token API            Done        8m   1.8K tokens
  🔄 #44 Session adapter      Running     3m   ...
  ⚠ #39 Auth refactor        Failed      —    on_done script 실패
  ⏳ #45 Error handling        Pending     dep:#44
  ⏳ #46 Missing tests         Pending

Acceptance Criteria:
  ✅ POST /auth/login → JWT 반환 (200)
  ✅ 만료 토큰 → 401 반환
  ⬜ POST /auth/refresh → 새 토큰 반환
  ⬜ cargo test -p auth 전체 통과

Dependencies:
  #45 depends on #44 (shared: src/auth/session.rs)
```

---

## 4. TUI 추가 패널

```
┌─ Runtime ──────────────────┐  ┌─ DataSource ────────────────┐
│ claude/sonnet  12 runs  OK │  │ github  ● connected         │
│ claude/opus     2 runs  OK │  │   last scan: 30s ago        │
│ Tokens: 45.2K in / 12.1K  │  │                             │
│ Avg duration: 4m 32s      │  │                             │
└────────────────────────────┘  └─────────────────────────────┘

┌─ Scripts (1h) ───────────────────┐
│ on_done         8 ok             │
│ on_fail         1 ok             │
│ on_enter        3 ok             │
│ evaluate       12 ok  1 hitl     │
│ ⚠ on_done       1 failed        │
└──────────────────────────────────┘
```

---

## 5. HITL 알림

HITL 이벤트가 생성되면 사용자에게 다음 경로로 알린다:

| 경로 | 방법 |
|------|------|
| TUI Dashboard | HITL 카운터 실시간 갱신 (`HITL: 1 pending ⚠`) |
| CLI | `autodev status`에 경고 표시, `autodev hitl list`로 조회 |
| /claw 세션 | 진입 시 HITL 대기 목록 자동 표시 |
| on_fail script | escalation=hitl 시 실행 — GitHub 코멘트 등으로 외부 알림 가능 |

별도 push 알림(Slack, email)은 on_fail/on_done script에서 직접 구현한다 (webhook 호출 등).

---

## 6. 데이터 요구사항

```sql
-- 아이템별 전이 이벤트 (append-only)
transition_events (
    id          TEXT PRIMARY KEY,
    work_id     TEXT NOT NULL,
    source_id   TEXT NOT NULL,       -- 계보 추적
    event_type  TEXT NOT NULL,       -- phase_enter, handler, evaluate, on_done, on_fail, on_enter
    phase       TEXT,                -- Pending, Ready, Running, Completed, Done, HITL, Failed, Skipped
    detail      TEXT,                -- script exit code, prompt result, error message
    created_at  TEXT NOT NULL
)

-- token 사용량 (AgentRuntime 실행마다 기록)
token_usage (
    id          TEXT PRIMARY KEY,
    work_id     TEXT NOT NULL,
    workspace   TEXT NOT NULL,
    runtime     TEXT NOT NULL,       -- claude, gemini, codex
    model       TEXT,                -- sonnet, opus, haiku
    input_tokens  INTEGER,
    output_tokens INTEGER,
    duration_ms   INTEGER,
    created_at  TEXT NOT NULL
)
```

---

### 관련 문서

- [DESIGN-v5](../DESIGN-v5.md) — QueuePhase 상태 머신
- [스펙 생명주기](./02-spec-lifecycle.md) — 스펙 진행률
- [실패 복구와 HITL](./04-failure-and-hitl.md) — HITL 오버레이
- [CLI 레퍼런스](../concerns/cli-reference.md) — 전체 커맨드 트리
