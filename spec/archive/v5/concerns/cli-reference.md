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
├── bootstrap                                ← .claude/rules 컨벤션 파일 생성
│   ├── [--workspace <dir>]                  # 워크스페이스 루트 (기본: 현재 디렉토리)
│   ├── [--rules-dir <dir>]                  # 커스텀 rules 디렉토리 경로
│   ├── [--force]                            # 기존 파일 덮어쓰기
│   ├── [--llm]                              # LLM으로 맞춤 컨벤션 생성
│   ├── [--project-name <name>]              # 프로젝트 이름 (--llm 전용)
│   ├── [--language <lang>]                  # 주 언어 (--llm 전용, e.g., Rust, TypeScript)
│   ├── [--framework <fw>]                   # 프레임워크 (--llm 전용, e.g., tokio, Next.js)
│   ├── [--description <desc>]              # 프로젝트 설명 (--llm 전용)
│   └── [--create-pr]                        # 생성된 컨벤션으로 PR 생성 (--llm 전용)
├── auto                                     ← /auto 슬래시 커맨드 플러그인 관리
│   └── plugin
│       ├── install [--project <dir>] [--force]   # /auto 슬래시 커맨드 설치
│       ├── uninstall [--project <dir>]           # /auto 슬래시 커맨드 제거
│       └── status [--project <dir>]              # 플러그인 설치 상태 확인
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

## `belt bootstrap` 상세

워크스페이스에 `.claude/rules` 컨벤션 파일을 생성한다. 정적 템플릿 또는 LLM 기반 맞춤 생성을 지원.

```bash
# 기본 사용 (정적 템플릿)
belt bootstrap

# 특정 디렉토리에 생성
belt bootstrap --workspace /path/to/project

# 기존 파일 덮어쓰기
belt bootstrap --force

# LLM으로 맞춤 컨벤션 생성
belt bootstrap --llm \
  --project-name my-app \
  --language Rust \
  --framework tokio \
  --description "비동기 웹 서버"

# LLM 생성 후 PR까지 자동 생성
belt bootstrap --llm --create-pr
```

| 플래그 | 기본값 | 설명 |
|--------|--------|------|
| `--workspace` | 현재 디렉토리 | 워크스페이스 루트 경로 |
| `--rules-dir` | `<workspace>/.claude/rules` | 커스텀 rules 디렉토리 |
| `--force` | false | 기존 파일 덮어쓰기 |
| `--llm` | false | LLM 기반 맞춤 생성 |
| `--project-name` | — | 프로젝트 이름 (`--llm` 필요) |
| `--language` | — | 주 프로그래밍 언어 (`--llm` 필요) |
| `--framework` | — | 프레임워크/런타임 (`--llm` 필요) |
| `--description` | — | 프로젝트 설명 (`--llm` 필요) |
| `--create-pr` | false | 컨벤션 PR 생성 (`--llm` 필요) |

---

## `belt auto` 상세

`/auto` 슬래시 커맨드 플러그인을 프로젝트의 `.claude/commands/`에 설치, 제거, 상태 확인한다.

```bash
# 플러그인 설치
belt auto plugin install
belt auto plugin install --project /path/to/project
belt auto plugin install --force    # 기존 파일 덮어쓰기

# 플러그인 제거
belt auto plugin uninstall

# 설치 상태 확인
belt auto plugin status
```

| 서브커맨드 | 설명 |
|-----------|------|
| `plugin install` | `/auto` 슬래시 커맨드 파일을 `.claude/commands/`에 설치 |
| `plugin uninstall` | 설치된 `/auto` 슬래시 커맨드 파일 제거 |
| `plugin status` | 플러그인 설치 여부 확인 |

| 플래그 | 적용 대상 | 기본값 | 설명 |
|--------|----------|--------|------|
| `--project` | install, uninstall, status | 현재 디렉토리 | 프로젝트 루트 경로 |
| `--force` | install | false | 기존 파일 덮어쓰기 |

---

### 관련 문서

- [DESIGN-v5](../DESIGN-v5.md) — 전체 아키텍처
- [DataSource](./datasource.md) — context 스키마
- [Agent](./agent-workspace.md) — /agent 세션
