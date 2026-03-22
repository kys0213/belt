# CLI 레퍼런스 — 3-layer 아키텍처 + 전체 커맨드 트리

> autodev CLI는 모든 레이어의 SSOT(Single Source of Truth)이다.

---

## 아키텍처 (3-layer)

```
Layer 1: Slash Command (3개, thin wrapper)
  /auto, /spec, /claw

Layer 2: DataSource + AgentRuntime (OCP 확장점)
  → 외부 시스템 워크플로우 + LLM 실행 추상화

Layer 3: autodev CLI (SSOT)
  → DB 조작, 상태 전이, 코어 로직
  → 모든 레이어가 CLI를 호출
```

---

## Slash Command 매핑 (v4 → v5)

| v4 | v5 |
|----|-----|
| /auto, /auto-setup, /auto-config, /auto-dashboard, /update | /auto (서브커맨드) |
| /add-spec, /update-spec, /spec | /spec (서브커맨드) |
| /status, /board, /decisions, /hitl, /repo, /claw, /cron | /claw (자연어) |

---

## autodev CLI 전체 참조

### Phase 1: 코어 CLI (v5 초기 구현)

상태 변경, 데몬 제어, CRUD — 직접 CLI로 노출.

```
autodev
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
├── context <work_id> [--json]               ← NEW: script용 정보 조회
├── hitl
│   ├── list / show / respond / timeout
├── cron
│   ├── list / add / update
│   ├── pause / resume / remove / trigger
├── claw
│   ├── init / rules / edit
├── agent [--workspace <name>] [-p <prompt>]
│         # Claw 워크스페이스의 rules를 로드한 LLM 세션을 실행.
│         # evaluate cron이 내부적으로 호출하여 Completed 아이템을 분류.
│         # --workspace: 대상 workspace 지정 (해당 workspace의 context 접근 가능)
│         # -p: 프롬프트 전달 (비대화형 실행, 결과 출력 후 종료)
│         # 인자 없이 실행 시 대화형 세션 시작 (/claw와 동일)
```

> **v4 대비 변경**: `queue advance` 제거 (Pending→Ready 자동 전이), `context` 서브커맨드 추가, `repo` → `workspace` 리네이밍.

### Phase 2: /claw 위임 (읽기 전용)

아래 커맨드는 `/claw` 세션에서 자연어로 접근. 별도 CLI 구현은 `/claw`가 안정화된 후 필요 시 추가.

```
# /claw가 내부적으로 호출하는 조회 커맨드 (구현 우선순위 낮음)
├── decisions list / show
├── board [--format text|json|rich]
├── convention
├── worktree list / clean
├── logs / usage / report
```

> `/claw`는 `autodev status --json`, `autodev queue list --json` 등 Phase 1 CLI의 JSON 출력을 파싱하여 자연어로 표시한다. Phase 2 커맨드도 동일한 패턴으로, `/claw`가 먼저 커버하고 독립 CLI는 수요가 확인되면 추가.

모든 서브커맨드는 `--json` 또는 `--format json` 출력 지원.

---

## `autodev context` 상세

script가 아이템 정보를 조회하는 유일한 방법.

```bash
# 기본 사용 (on_done/on_fail script 내에서)
CTX=$(autodev context $WORK_ID --json)
ISSUE=$(echo $CTX | jq -r '.issue.number')
REPO=$(echo $CTX | jq -r '.source.url')

# 특정 필드만 조회 (jq 없이)
autodev context $WORK_ID --field issue.number    # → 42
autodev context $WORK_ID --field source.url      # → https://github.com/org/repo
```

context 스키마는 DataSource별로 다르다. 상세는 [DataSource](./datasource.md) 참조.

---

### 관련 문서

- [DESIGN-v5](../DESIGN-v5.md) — 전체 아키텍처
- [DataSource](./datasource.md) — context 스키마
- [Claw](./claw-workspace.md) — /claw 세션
