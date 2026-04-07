# Agent — 대화형 에이전트

> `belt agent` = 대화형 + 비대화형 LLM 세션. 자연어로 시스템을 조회하고 조작하는 통합 인터페이스.
>
> 분류(Done or HITL)는 **코어 Evaluator (Daemon tick)**가 담당한다. Agent는 분류기가 아니다.

---

## 코어 evaluate (참고)

분류 로직은 코어에 속한다. Agent와 무관.

**v6 (#722)**: evaluate는 **per-work_id 단위**로 LLM 판정을 실행한다.

```
handler 전부 성공 → Completed
    │
    ▼
Evaluator (Daemon tick에서 Executor보다 먼저 실행):
  Progressive Pipeline:
    Stage 1: Mechanical (cargo test 등, 비용 0)
      → 실패 시 Retry (LLM 안 부름)
    Stage 2: Semantic (LLM 1회, belt agent -p)
      → LLM이 belt context로 컨텍스트 조회 후 판정
    │
    ├── Done → hook.on_done() 트리거
    │     ├── hook 성공 → Done (worktree 정리)
    │     └── hook 실패 → Failed (worktree 보존)
    │
    └── HITL → HITL 이벤트 생성 → 사람 대기 (worktree 보존)
```

- Evaluator는 Daemon tick의 정규 단계. 상세: [Evaluator](./evaluator.md)
- SemanticStage에서 LLM이 `belt queue done/hitl` CLI를 직접 호출하여 상태를 전이한다
- 개별 판정 실패 시 해당 아이템만 Completed에 머물고, 다른 아이템 판정에 영향 없다
- evaluate LLM 호출도 `daemon.max_concurrent` slot을 소비한다

---

## CLI 통합 설계

`belt agent`는 서브커맨드 유무에 따라 동작이 결정된다.

### 사용법

```bash
# 대화형 세션 (서브커맨드 없이 실행)
belt agent                                         # 글로벌 rules 로드, 대화형 세션 시작
belt agent --workspace workspace.yaml              # workspace 지정 대화형 세션

# 비대화형 실행 (Evaluator가 호출)
belt agent -p "프롬프트"                            # 글로벌 rules로 비대화형 실행
belt agent --workspace workspace.yaml -p "프롬프트"  # workspace 지정 비대화형 실행

# 실행 계획
belt agent --plan                                  # 실행 계획만 출력
belt agent --workspace workspace.yaml --plan       # workspace 지정 실행 계획

# 워크스페이스 관리
belt agent init [--force]                          # agent 워크스페이스 초기화
belt agent rules                                   # 규칙 조회
belt agent edit [rule]                             # 규칙 편집

# 플러그인
belt agent plugin [--install-dir]                  # /agent 슬래시 커맨드 설치
belt agent context                                 # 시스템 컨텍스트 수집
```

### --workspace 옵션 동작

| --workspace | -p | 동작 |
|---|---|---|
| 없음 | 없음 | 글로벌 rules → 대화형 세션 |
| 없음 | 있음 | 글로벌 rules → 비대화형 실행 |
| 있음 | 없음 | workspace rules → 대화형 세션 |
| 있음 | 있음 | workspace rules → 비대화형 실행 |

> `--workspace`가 없으면 글로벌 agent 워크스페이스(`~/.belt/agent-workspace/`)의 rules를 로드한다.

---

## 대화형 세션 (/agent)

어디서든 실행 가능한 대화형 인터페이스.

### 진입 경험

```
belt agent 실행 →

Step 1: 상태 수집
  belt status --json
  belt hitl list --json
  belt queue list --phase failed --json

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
  → Bash tool로 belt CLI 호출
```

### 자연어 → CLI 매핑 예시

```
"지금 상황 어때?"      → belt status --format rich
"큐 막힌 거 있어?"     → belt queue list --json → 분석
"HITL 대기 목록"       → belt hitl list --json
"실패한 거 있어?"      → belt queue list --phase failed --json
"cron 일시정지"        → belt cron pause gap-detection
"뭐 하면 좋을까?"     → status + hitl + queue(failed) 종합 → 추천
```

---

## 워크스페이스 구조

```
~/.belt/agent-workspace/
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

Per-workspace 오버라이드: `~/.belt/workspaces/<name>/agent/system/`

---

## Plugin slash command 통합

```
v4 (15개) → v5 (3개):
  /auto   — 데몬 제어 (start/stop/setup/config/dashboard/update)
  /spec   — 스펙 CRUD (add/update/list/status/remove/pause/resume)
  /agent  — 대화 세션 (조회/조작/모니터링을 자연어로, 읽기 전용 CLI 흡수)
```

### 실행 컨텍스트

| Command | 실행 위치 | 설명 |
|---------|----------|------|
| `/auto` | 어디서든 | Daemon 제어, workspace 등록 |
| `/spec` | 레포의 Claude 세션 | 해당 레포의 스펙 CRUD |
| `/agent` | 어디서든 | 대화형 에이전트 (전체 workspace 조회/조작) |

---

## 실행 흐름

```
1. --workspace 옵션에 따라 workspace 결정 (위 "--workspace 옵션 동작" 테이블 참조)
2. RuntimeRegistry 구성 (workspace yaml의 runtime 설정 기반, 없으면 기본 ClaudeRuntime)
3. Rules 로딩 (아래 우선순위)
4. System prompt = built-in agent rules + workspace rules
5. -p 있으면 ActionExecutor로 비대화형 실행, 없으면 대화형 세션 시작
```

### Rules 로딩 우선순위

1. `agent_config.rules_path` — workspace yaml에서 명시적 지정
2. `~/.belt/workspaces/<name>/agent/system/` — per-workspace 오버라이드
3. `~/.belt/agent-workspace/.claude/rules/` — 글로벌 기본값

디렉토리 내 모든 `.md` 파일을 concat하여 system prompt에 주입한다.

### classify-policy.md 로딩 경로 및 해석 (R-CW-007)

`classify-policy.md`는 LLM 에이전트가 큐 아이템을 Done / HITL로 분류할 때
참조하는 자연어 정책 문서다. `.claude/rules/` 하위에 위치하며, system prompt에 주입된다.

#### 로딩 경로

`agent::resolve_rules_dir` 함수가 아래 우선순위로 **디렉토리**를 탐색한다.
첫 번째로 존재하는 디렉토리 안의 **모든 `.md` 파일**이 로드된다.

```
Priority 1: claw_config.rules_path        (workspace YAML 명시)
Priority 2: $BELT_HOME/workspaces/<name>/claw/system/   (per-workspace)
Priority 3: $BELT_HOME/claw-workspace/.claude/rules/    (global, belt claw init)
```

`$BELT_HOME`은 환경변수 `BELT_HOME`이 설정되지 않으면 `~/.belt`로 기본값.

#### 파일 미존재 시 fallback

- 디렉토리 자체가 없는 경우: agent는 built-in Claw rules(대화 턴 제한, 응답 포맷, 에러 핸들링)만으로 실행. 에러 없음.
- 디렉토리는 있지만 `.md` 파일이 없는 경우: 동일하게 built-in rules만 사용.
- `classify-policy.md`만 없고 다른 `.md`가 있는 경우: 다른 정책 파일은 정상 로드, 분류 정책 가이던스만 빠진 채 실행.

#### 구현 위치

- `crates/belt-cli/src/agent.rs` — `resolve_rules_dir`, `load_rules_from_dir`
- `crates/belt-cli/src/claw/mod.rs` — `ClawWorkspace::init`, `default_classify_policy()`

### LLM이 사용 가능한 도구

`belt agent`로 실행된 LLM은 bash tool을 통해 다음 belt CLI를 호출할 수 있다:

| CLI | 용도 | 사용 시점 |
|-----|------|----------|
| `belt context $WORK_ID --json` | 아이템 정보 조회 | evaluate 판단 입력 |
| `belt queue done $WORK_ID` | Done 판정 | evaluate 결과 |
| `belt queue hitl $WORK_ID --reason "..."` | HITL 판정 | evaluate 결과 |
| `belt status --json` | 시스템 상태 조회 | 대화형 세션 |
| `belt hitl list --json` | HITL 목록 조회 | 대화형 세션 |
| `belt queue list --json` | 큐 목록 조회 | 대화형 세션 |

### Evaluator와의 관계

Evaluator의 SemanticStage가 내부적으로 `belt agent -p`를 호출한다. 이때:
- **per-item**: 각 아이템에 대해 개별 프롬프트 발행 (v6 #722)
- LLM이 `belt context $WORK_ID`로 해당 아이템 정보를 조회
- 판단 후 `belt queue done/hitl` CLI를 직접 호출하여 상태 전이
- classify-policy.md의 state별 Done 조건이 판단 기준
- evaluate LLM 호출도 `daemon.max_concurrent` slot을 소비

상세: [Evaluator](./evaluator.md)

---

### 관련 문서

- [DESIGN](../DESIGN.md) — QueuePhase 상태 머신 + evaluate 위치
- [CLI 레퍼런스](./cli-reference.md) — CLI 전체 커맨드 트리
- [Cron 엔진](./cron-engine.md) — 품질 루프 (gap-detection 등)
- [Data Model](./data-model.md) — 컨텍스트 모델 (belt context 출력)
