# Claw — 대화형 에이전트

> Claw = `/claw` 세션. 자연어로 시스템을 조회하고 조작하는 대화형 인터페이스.
>
> 분류(Done or HITL)는 **코어 evaluate cron**이 담당한다. Claw는 분류기가 아니다.

---

## 코어 evaluate (참고)

분류 로직은 코어에 속한다. Claw와 무관.

```
handler 전부 성공 → Completed
    │
    ▼
evaluate cron (force_trigger로 즉시 실행 가능):
  autodev agent -p "Completed 아이템의 완료 여부를 판단해줘"
    │
    │  LLM이 autodev context로 컨텍스트 조회 후 CLI 도구로 결정:
    │
    ├── autodev queue done $WORK_ID
    │     → Daemon이 on_done script 실행 (CLI 명령이 상태 전이를 트리거하고, Daemon이 script를 실행)
    │       ├── script 성공 → Done (worktree 정리)
    │       └── script 실패 → Failed (worktree 보존, 로그 기록)
    │
    └── autodev queue hitl $WORK_ID --reason "..."
          → HITL 이벤트 생성 → 사람 대기 (worktree 보존)
```

evaluate cron: `interval 60s + force_trigger on Completed 전이`. LLM이 JSON을 파싱하는 게 아니라, 직접 `autodev queue done/hitl` CLI를 호출하여 상태를 전이한다.

evaluate의 판단 입력: `autodev context $WORK_ID --json` (queue 메타데이터 + 외부 시스템 컨텍스트 + append-only history).

---

## 대화형 에이전트 (/claw 세션)

어디서든 실행 가능한 대화형 인터페이스.

### 진입 경험

```
/claw 실행 →

Step 1: 상태 수집
  autodev status --json
  autodev hitl list --json
  autodev queue list --phase failed --json

Step 2: 요약 표시

  ● daemon running (uptime 2h 15m)

  Workspaces:
    auth-project — queue: 1R 1C 2D | specs: auth-v2 60%

  ⚠ HITL 대기: 1건
    → #44 Session adapter — 3회 실패

  ⚠ Failed: 1건
    → #39 Auth refactor — on_done script 실패

  무엇을 도와드릴까요?

Step 3: 자연어 대화
  → Bash tool로 autodev CLI 호출
```

### 자연어 → CLI 매핑 예시

```
"지금 상황 어때?"      → autodev status --format rich
"큐 막힌 거 있어?"     → autodev queue list --json → 분석
"HITL 대기 목록"       → autodev hitl list --json
"실패한 거 있어?"      → autodev queue list --phase failed --json
"cron 일시정지"        → autodev cron pause gap-detection
"뭐 하면 좋을까?"     → status + hitl + queue(failed) 종합 → 추천
```

---

## 워크스페이스 구조

```
~/.autodev/claw-workspace/
├── CLAUDE.md                         # 판단 원칙
├── .claude/rules/
│   ├── classify-policy.md            # Done vs HITL 분류 기준
│   ├── hitl-policy.md                # HITL 판단 기준
│   └── auto-approve-policy.md        # 자동 승인 기준
├── commands/
└── skills/
    ├── gap-detect/
    └── prioritize/
```

Per-workspace 오버라이드: `~/.autodev/workspaces/<name>/claw/`

---

## Plugin slash command 통합

```
v4 (15개) → v5 (3개):
  /auto   — 데몬 제어 (start/stop/setup/config/dashboard/update)
  /spec   — 스펙 CRUD (add/update/list/status/remove/pause/resume)
  /claw   — 대화 세션 (조회/조작/모니터링을 자연어로, 읽기 전용 CLI 흡수)
```

### 실행 컨텍스트

| Command | 실행 위치 | 설명 |
|---------|----------|------|
| `/auto` | 어디서든 | Daemon 제어, workspace 등록 |
| `/spec` | 레포의 Claude 세션 | 해당 레포의 스펙 CRUD |
| `/claw` | 어디서든 | 대화형 에이전트 (전체 workspace 조회/조작) |

---

### 관련 문서

- [DESIGN-v5](../DESIGN-v5.md) — QueuePhase 상태 머신 + evaluate 위치
- [CLI 레퍼런스](./cli-reference.md) — CLI 전체 커맨드 트리
- [Cron 엔진](./cron-engine.md) — evaluate cron
