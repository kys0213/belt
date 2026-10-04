# workspace.yaml 스키마

> workspace.yaml은 Belt의 단일 설정 파일(Single Source of Truth)이다.
> 각 concern 문서는 자기 영역의 설정을 이 문서에서 참조한다.

---

## 전체 구조

```yaml
# workspace.yaml
name: my-project
concurrency: 2

sources:
  github:
    url: https://github.com/org/repo
    scan_interval_secs: 300
    states:
      analyze:
        trigger: { label: "belt:analyze" }
        handlers:
          - prompt: "이슈를 분석해줘"
        on_done: [{ script: "..." }]
    escalation:
      1: retry
      2: retry_with_comment
      3: hitl
      terminal: skip

runtime:
  default: claude
  claude: { model: sonnet }

evaluate:
  mechanical: ["cargo test", "cargo clippy -- -D warnings"]

stagnation:
  enabled: true
  lateral: { enabled: true }

# 생략 시: origin channel에만 기본 이벤트로 알리고, 외부 응답은 받지 않는다
notifications:
  origin:
    enabled: true
    events: [started, failed, hitl_requested]
    respond:
      allow: []                  # 비면 origin 응답 미수용. 예: ["octocat"]
  channels: []                   # 추가 channel (선택). 항목 형식은 아래 notifications 절 참조
```

> 전체 필드와 기본값은 아래 레퍼런스 테이블 참조. 실제 yaml 예시는 [DataSource](./datasource.md)의 GitHub 워크플로우 참조.

---

## 필드 레퍼런스

### Root

| 필드 | 타입 | 기본값 | 필수 | 설명 | 상세 |
|------|------|--------|------|------|------|
| `name` | String | — | ✅ | workspace 이름 (DB PK) | [Setup](../flows/01-setup.md) |
| `concurrency` | u32 | 1 | — | 이 workspace의 동시 Running 수 | [Daemon](./daemon.md) |

### sources.{type}

| 필드 | 타입 | 기본값 | 필수 | 설명 | 상세 |
|------|------|--------|------|------|------|
| `url` | String | — | ✅ | 외부 시스템 URL | [DataSource](./datasource.md) |
| `scan_interval_secs` | u32 | 300 | — | collect() 주기 (초) | [DataSource](./datasource.md) |

### sources.{type}.states.{state}

| 필드 | 타입 | 기본값 | 필수 | 설명 | 상세 |
|------|------|--------|------|------|------|
| `trigger` | TriggerConfig | — | ✅ | 상태 진입 조건 | [DataSource](./datasource.md) |
| `trigger.label` | String | — | (조건부) | GitHub 라벨 트리거 | [DataSource](./datasource.md) |
| `trigger.changes_requested` | bool | false | — | PR changes_requested 트리거 | [DataSource](./datasource.md) |
| `handlers` | Vec | — | ✅ | 실행할 작업 배열 | [DataSource](./datasource.md) |
| `handlers[].prompt` | String | — | (1) | LLM 프롬프트 (prompt 또는 script 중 하나) | [DataSource](./datasource.md) |
| `handlers[].script` | String | — | (1) | bash 스크립트 (prompt 또는 script 중 하나) | [DataSource](./datasource.md) |
| `handlers[].runtime` | String | runtime.default | — | 이 handler의 LLM | [AgentRuntime](./agent-runtime.md) |
| `handlers[].model` | String | runtime별 기본값 | — | 이 handler의 모델 | [AgentRuntime](./agent-runtime.md) |
| `on_done` | Vec | — | — | Done 판정 후 실행 스크립트 | [LifecycleHook](./lifecycle-hook.md) |
| `on_fail` | Vec | — | — | handler 실패 시 실행 스크립트 | [LifecycleHook](./lifecycle-hook.md) |
| `on_enter` | Vec | — | — | Running 진입 후 실행 스크립트 | [LifecycleHook](./lifecycle-hook.md) |

> (1): `prompt`과 `script` 중 하나는 필수.

### sources.{type}.escalation

| 필드 | 타입 | 기본값 | 필수 | 설명 | 상세 |
|------|------|--------|------|------|------|
| `{N}` | EscalationAction | — | ✅ | N회 실패 시 액션 | [DataSource](./datasource.md) |
| `terminal` | EscalationAction | — | ✅ | HITL timeout 시 액션 | [DataSource](./datasource.md) |

EscalationAction: `retry` | `retry_with_comment` | `hitl` | `skip` | `replan`

### runtime

| 필드 | 타입 | 기본값 | 필수 | 설명 | 상세 |
|------|------|--------|------|------|------|
| `default` | String | "claude" | — | 기본 LLM | [AgentRuntime](./agent-runtime.md) |
| `{runtime_name}.model` | String | runtime별 내장 기본값 | — | 런타임 기본 모델 | [AgentRuntime](./agent-runtime.md) |

### evaluate

| 필드 | 타입 | 기본값 | 필수 | 설명 | 상세 |
|------|------|--------|------|------|------|
| `mechanical` | Vec\<String\> | — | — | MechanicalStage 검증 커맨드 | [Evaluator](./evaluator.md) |

### stagnation — [Stagnation Detection](./stagnation.md) 참조

| 필드 | 타입 | 기본값 | 필수 | 설명 |
|------|------|--------|------|------|
| `enabled` | bool | true | — | 정체 패턴 감지 활성화 |
| `lateral.enabled` | bool | true | — | lateral thinking 활성화 |

> 유사도 threshold(0.9)와 최소 연속 횟수(2)는 현재 고정값이며 yaml로 노출되지 않는다. 상세: [Stagnation Detection](./stagnation.md)

### notifications — [NotificationChannel](./notification.md) 참조

```yaml
notifications:
  origin:
    enabled: true
    events: [started, failed, hitl_requested]
    respond:
      allow: ["octocat"]
  channels:
    - name: team-chat                    # 예시 이름
      type: "<구현이 제공하는 channel type>"   # 자리표시자. 실제 값으로 바꿔야 하며, 지원하지 않는 type은 로드 실패
      events: [hitl_requested]
      respond:
        allow: []
      config: {}                         # channel 구현 전용. 코어는 해석하지 않는다
```

> `type`의 자리표시자를 그대로 두거나 지원하지 않는 값을 쓰면 workspace 설정 로드가 실패한다 (Fail Fast). 현재 `origin` 외에 사용할 수 있는 channel type은 없으므로 `channels`는 비워 둔다.

| 필드 | 타입 | 기본값 | 필수 | 설명 |
|------|------|--------|------|------|
| `notifications` | Object | (생략 가능) | — | 생략하면 origin channel에만 기본 이벤트로 알리고 외부 응답은 받지 않는다 |
| `origin.enabled` | bool | true | — | 출처 시스템(origin channel)으로 알림을 보낼지 |
| `origin.events` | Vec\<Event\> | `[started, failed, hitl_requested]` | — | origin으로 보낼 이벤트 |
| `origin.respond.allow` | Vec\<String\> | `[]` | — | origin에서 HITL 응답을 허용할 응답자. 비면 응답을 받지 않는다 |
| `channels` | Vec | `[]` | — | 추가로 fan-out할 channel 목록 |
| `channels[].name` | String | — | ✅ | workspace 안에서 고유한 이름. 회신 문구의 "via"에 쓰인다 |
| `channels[].type` | String | — | ✅ | channel 구현 종류. 지원하지 않는 값이면 로드 실패 |
| `channels[].events` | Vec\<Event\> | — | ✅ | 이 channel로 보낼 이벤트 |
| `channels[].respond.allow` | Vec\<String\> | `[]` | — | 이 channel에서 응답을 허용할 응답자. 비면 발송 전용 |
| `channels[].config` | Object | `{}` | — | channel 구현 전용 설정 |

Event: `started` | `done` | `failed` | `skipped` | `hitl_requested`

- Dashboard는 이 설정과 무관하게 항상 켜져 있고 응답할 수 있다.
- 확인 요청·`already_handled` 같은 회신은 이벤트 필터와 무관하게 응답을 보낸 channel로 간다.
- `hitl_resolved`는 내부 이벤트라 선택할 수 없다.

---

## Daemon 글로벌 설정 (별도)

Daemon 자체의 설정은 workspace.yaml이 아닌 별도 config에서 관리한다.

| 필드 | 타입 | 기본값 | 설명 | 상세 |
|------|------|--------|------|------|
| `max_concurrent` | u32 | 4 | 전체 workspace 합산 동시 실행 상한 | [Daemon](./daemon.md) |
| `tick` | u32 | 30 | tick 간격 (초, CLI `--tick`으로 지정) | [Daemon](./daemon.md) |

---

### 관련 문서

- [DataSource](./datasource.md) — sources, handlers, escalation 상세
- [AgentRuntime](./agent-runtime.md) — runtime 설정, 모델 결정 우선순위
- [Evaluator](./evaluator.md) — evaluate.mechanical 상세
- [Stagnation Detection](./stagnation.md) — stagnation 설정
- [Daemon](./daemon.md) — concurrency 2단계 제어
- [LifecycleHook](./lifecycle-hook.md) — on_done/on_fail/on_enter hook
- [NotificationChannel](./notification.md) — notifications 동작
- [Setup Flow](../flows/01-setup.md) — workspace 등록 흐름
