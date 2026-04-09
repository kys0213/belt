# workspace.yaml 스키마

> workspace.yaml은 Belt의 단일 설정 파일(Single Source of Truth)이다.
> 각 concern 문서는 자기 영역의 설정을 이 문서에서 참조한다.

---

## 전체 구조

```yaml
# workspace.yaml
name: my-project                       # workspace 이름 (필수, 유니크)
concurrency: 2                         # 동시 Running 아이템 수 (기본 1)

sources:                               # DataSource 설정 (필수)
  github:
    url: https://github.com/org/repo
    scan_interval_secs: 300            # 수집 주기 초 (기본 300)

    states:                            # 워크플로우 상태 정의 (필수)
      analyze:
        trigger: { label: "belt:analyze" }
        handlers:
          - prompt: "이슈를 분석해줘"
        on_done:
          - script: |
              # evaluate Done 판정 후 실행
        on_fail:
          - script: |
              # handler 실패 시 실행
        on_enter:
          - script: |
              # Running 진입 후, handler 실행 전

    escalation:                        # 실패 정책 (필수)
      1: retry
      2: retry_with_comment
      3: hitl
      terminal: skip

runtime:                               # LLM 런타임 설정
  default: claude
  claude:
    model: sonnet
  gemini:
    model: pro

evaluate:                              # Evaluator 설정
  mechanical:
    - "cargo test"
    - "cargo clippy -- -D warnings"

stagnation:                            # Stagnation Detection 설정
  enabled: true
  spinning_threshold: 3
  oscillation_cycles: 2
  similarity_threshold: 0.8
  no_drift_epsilon: 0.01
  no_drift_iterations: 3
  diminishing_threshold: 0.01
  confidence_threshold: 0.5
  similarity:
    - judge: exact_hash
      weight: 0.5
    - judge: token_fingerprint
      weight: 0.3
    - judge: ncd
      weight: 0.2
  lateral:
    enabled: true
    max_attempts: 3
```

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

### stagnation

| 필드 | 타입 | 기본값 | 필수 | 설명 | 상세 |
|------|------|--------|------|------|------|
| `enabled` | bool | true | — | 정체 패턴 감지 활성화 | [Stagnation](./stagnation.md) |
| `spinning_threshold` | u32 | 3 | — | SPINNING 감지용 최소 연속 유사 출력 수 | [Stagnation](./stagnation.md) |
| `oscillation_cycles` | u32 | 2 | — | OSCILLATION 감지용 최소 교대 사이클 수 (출력 2N개) | [Stagnation](./stagnation.md) |
| `similarity_threshold` | f64 | 0.8 | — | 유사 판정 기준 (0.0~1.0) | [Stagnation](./stagnation.md) |
| `no_drift_epsilon` | f64 | 0.01 | — | drift 변화 임계값 | [Stagnation](./stagnation.md) |
| `no_drift_iterations` | u32 | 3 | — | drift 정체 판정 반복 수 | [Stagnation](./stagnation.md) |
| `diminishing_threshold` | f64 | 0.01 | — | 개선폭 임계값 | [Stagnation](./stagnation.md) |
| `confidence_threshold` | f64 | 0.5 | — | 탐지 유효 최소 confidence (0.0~1.0) | [Stagnation](./stagnation.md) |

### stagnation.similarity

| 필드 | 타입 | 기본값 | 필수 | 설명 | 상세 |
|------|------|--------|------|------|------|
| `judge` | String | — | ✅ | 판정 알고리즘 (`exact_hash`, `token_fingerprint`, `ncd`) | [Stagnation](./stagnation.md) |
| `weight` | f64 | — | ✅ | 가중치 (합산 1.0) | [Stagnation](./stagnation.md) |

> 기본 프리셋: exact_hash(0.5) + token_fingerprint(0.3) + ncd(0.2)

### stagnation.lateral

| 필드 | 타입 | 기본값 | 필수 | 설명 | 상세 |
|------|------|--------|------|------|------|
| `enabled` | bool | true | — | lateral thinking 활성화 | [Stagnation](./stagnation.md) |
| `max_attempts` | u32 | 3 | — | 페르소나 최대 시도 횟수 | [Stagnation](./stagnation.md) |

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
- [Stagnation Detection](./stagnation.md) — stagnation 전체 설정, Rust 구조체
- [Daemon](./daemon.md) — concurrency 2단계 제어
- [LifecycleHook](./lifecycle-hook.md) — on_done/on_fail/on_enter hook
- [Setup Flow](../flows/01-setup.md) — workspace 등록 흐름
