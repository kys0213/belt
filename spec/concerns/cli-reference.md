# CLI 레퍼런스 — 3-layer 아키텍처 + 전체 커맨드 트리

> belt CLI는 모든 레이어의 SSOT(Single Source of Truth)이다.

---

## 아키텍처 (3-layer)

```
Layer 1: Slash Command (3개, thin wrapper)
  /auto, /spec, /agent

Layer 2: DataSource + AgentRuntime (OCP 확장점)
  → 외부 시스템 워크플로우 + LLM 실행 추상화

Layer 3: belt CLI (SSOT)
  → DB 조작, 상태 전이, 코어 로직
  → 모든 레이어가 CLI를 호출
```

---

## Slash Command 매핑 (v4 → v5)

| v4 | v5 |
|----|-----|
| /auto, /auto-setup, /auto-config, /auto-dashboard, /update | /auto (서브커맨드) |
| /add-spec, /update-spec, /spec | /spec (서브커맨드) |
| /status, /board, /decisions, /hitl, /repo, /claw, /cron | /agent (자연어) |

---

## belt CLI 전체 참조

### Phase 1: 코어 CLI (v5 초기 구현)

상태 변경, 데몬 제어, CRUD — 직접 CLI로 노출.

```
belt
├── start / stop / restart
├── status [--format text|json|rich]
├── dashboard
├── workspace
│   ├── add / list / show / update / remove / config
├── spec
│   ├── add / list / show / update
│   ├── pause / resume / complete / remove
│   ├── link / unlink
│   ├── status <id> / verify <id>
├── queue
│   ├── list [--phase <phase>] / show / skip
│   ├── done <work_id>                      ← evaluate가 호출: Completed → Done (on_done 실행)
│   ├── hitl <work_id> [--reason <msg>]     ← evaluate가 호출: Completed → HITL
│   ├── retry-script <work_id>              ← Failed 아이템의 on_done script 재실행
│   └── dependency add / remove
├── context <work_id> [--json]               ← script용 정보 조회
├── hitl
│   ├── list / show / respond / timeout
├── cron
│   ├── list / add / update
│   ├── pause / resume / remove / trigger
├── agent                                    ← 서브커맨드 없이 실행 시 대화형 세션 시작
│   ├── [default]                            # 대화형 세션 (글로벌 rules 로드)
│   ├── [-p <prompt>]                        # 비대화형 실행 (evaluate cron이 호출)
│   ├── [--workspace <name>]                 # 대상 workspace 지정
│   ├── [--plan]                             # 실행 계획만 출력
│   ├── [--json]                             # JSON 출력
│   ├── init [--force]                       # agent 워크스페이스 초기화
│   ├── rules                                # 규칙 조회
│   ├── edit [rule]                          # 규칙 편집
│   ├── plugin [--install-dir]               # /agent 슬래시 커맨드 설치
│   └── context                              # 시스템 컨텍스트 수집 (agent injection용)
```

> **v4 대비 변경**: `queue advance` 제거 (Pending→Ready 자동 전이), `context` 서브커맨드 추가, `repo` → `workspace` 리네이밍. `claw` + `agent` → `agent`로 통합.

### Phase 2: /agent 위임 (읽기 전용)

아래 커맨드는 `/agent` 세션에서 자연어로 접근. 별도 CLI 구현은 `/agent`가 안정화된 후 필요 시 추가.

```
# /agent가 내부적으로 호출하는 조회 커맨드 (구현 우선순위 낮음)
├── decisions list / show
├── board [--format text|json|rich]
├── convention
├── worktree list / clean
├── logs / usage / report
```

> `/agent`는 `belt status --json`, `belt queue list --json` 등 Phase 1 CLI의 JSON 출력을 파싱하여 자연어로 표시한다. Phase 2 커맨드도 동일한 패턴으로, `/agent`가 먼저 커버하고 독립 CLI는 수요가 확인되면 추가.

모든 서브커맨드는 `--json` 또는 `--format json` 출력 지원.

---

## `belt context` 상세

script가 아이템 정보를 조회하는 유일한 방법.

```bash
# 기본 사용 (on_done/on_fail script 내에서)
CTX=$(belt context $WORK_ID --json)
ISSUE=$(echo $CTX | jq -r '.issue.number')
REPO=$(echo $CTX | jq -r '.source.url')

# 특정 필드만 조회 (jq 없이)
belt context $WORK_ID --field issue.number    # → 42
belt context $WORK_ID --field source.url      # → https://github.com/org/repo
```

context 스키마는 DataSource별로 다르다. 상세는 [DataSource](./datasource.md) 참조.

---

### 관련 문서

- [DESIGN-v5](../DESIGN-v5.md) — 전체 아키텍처
- [DataSource](./datasource.md) — context 스키마
- [Agent](./agent-workspace.md) — /agent 세션
