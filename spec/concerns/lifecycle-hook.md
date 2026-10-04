# LifecycleHook — 출처 상태 반영 추상화

> Daemon이 상태를 전이할 때, 해당 workspace의 LifecycleHook이 **출처 시스템의 상태**를 반영한다 (라벨 추가·제거 등).
> handler(prompt/script)는 "무엇을 실행할지", hook은 "상태가 바뀌었을 때 출처 시스템에 무엇을 반영할지".
> 사람에게 보내는 메시지(코멘트, 진행 알림, HITL 요청)와 사람의 응답 수신은 hook이 아니라 [NotificationChannel](./notification.md)이 맡는다.
> 새 DataSource 유형 추가 = LifecycleHook 구현 추가, 코어 변경 0 (OCP).

---

## 설계 배경

hook이 없던 시절에는 on_done/on_fail/on_enter가 yaml script로만 정의되어 Daemon(Executor)이 직접 subprocess로 실행했다. 이 구조에는 다음 문제가 있었다.

1. **handler와 hook이 같은 곳(yaml)에 혼재** — 성격이 다른 관심사가 구분되지 않음
2. **DataSource별 반응을 정의할 수 없음** — GitHub/Jira 모두 bash script로만 표현
3. **on_escalation이 없음** — escalation 발생 시 외부 시스템 반응 경로가 없음
4. **Daemon이 도메인을 침범** — Executor가 `gh issue comment` 같은 script를 직접 실행

handler(yaml 정의)와 hook(DataSource 유형별 구현)을 분리해 이 문제들을 해소했다. 이후 hook에 섞여 있던 "사람에게 말하기"(코멘트 작성)와 "응답 듣기"는 NotificationChannel로 분리했다.

| 이유 | 내용 |
|------|------|
| 응답 수신 | hook은 쓰기 전용이다. 사람의 응답을 받는 반대 방향 책임이 들어오면 hook의 의미가 무너진다 |
| 다중 발송 | hook은 workspace당 하나만 선택된다. 같은 이벤트를 여러 곳에 보내려면 채널 목록이 필요하다 |
| 실패 의미 | hook 실패(on_enter/on_done)는 상태 전이에 영향을 준다. 알림 실패는 phase에 영향을 주면 안 된다 |

---

## 설계 — handler, hook, channel의 분리

| | handler | hook | channel |
|---|---------|------|---------|
| **역할** | 작업 자체 (분석, 구현, 리뷰) | 출처 시스템의 상태 반영 | 사람에게 알림·HITL 요청, 응답 수신 |
| **정의** | workspace yaml (prompt/script) | DataSource 유형별 구현 + workspace 설정 | workspace yaml `notifications` |
| **실행 주체** | Daemon Executor | Daemon이 트리거, hook 구현이 실행 | Daemon이 발송·polling |
| **예시** | "이슈를 구현해줘" | `belt:needs-human` 라벨 추가·제거 | 시작 코멘트, 실패 알림, HITL 요청 |
| **실패 영향** | handler 실패 → escalation | callback에 따라 다름 (아래 표) | phase에 영향 없음 |

```mermaid
flowchart LR
    subgraph Core["Daemon (코어)"]
        EX[Executor]
        PP["HITL 후처리"]
    end
    HK["LifecycleHook (출처 상태 반영)"]
    CH["NotificationChannel (알림·응답)"]
    SRC["출처 시스템 (GitHub 등)"]
    HUMAN["사람"]

    EX -- "on_enter / on_done / on_fail / on_escalation" --> HK
    PP -- "on_done / on_hitl_resolved" --> HK
    HK -- "라벨 추가·제거" --> SRC
    EX -- "channel event" --> CH
    PP -- "channel event" --> CH
    CH -- "메시지" --> HUMAN
    HUMAN -- "응답" --> CH
```

### Daemon = CPU 비유

```mermaid
flowchart TD
    T["tick"] --> S["큐 스캔 + 상태 전이 결정"]
    S --> H["handler 실행 (yaml prompt/script)"]
    H --> X{"전이 발생"}
    X --> HK["workspace에 바인딩된 LifecycleHook callback 호출"]
    X --> CH["channel event 발송"]
    HK --> R1["출처 시스템에 맞게 상태 반영"]
    CH --> R2["설정된 channel로 알림"]
```

### 전이 지점별 callback과 channel event

전이 지점과 hook callback, channel event의 대응은 [NotificationChannel](./notification.md#전이-지점-매핑)의 매핑 표가 단일 기준이다.

---

## DataSource, LifecycleHook, NotificationChannel 관계

```mermaid
flowchart LR
    DS["DataSource<br/>수집 + 컨텍스트 (읽기)"]
    HK["LifecycleHook<br/>출처 상태 반영 (쓰기)"]
    NC["NotificationChannel<br/>알림 + 응답 (사람과의 대화)"]
    G1[GitHubSource] --- G2[GitHubLifecycleHook]
    G1 --- G3["GitHub origin channel"]
    DS -.- G1
    HK -.- G2
    NC -.- G3
```

| 시스템 | DataSource | LifecycleHook | origin channel |
|--------|-----------|---------------|----------------|
| GitHub | 구현됨 | 구현됨 | 구현됨 |
| Jira | 미구현 | 미구현 | 미구현 |

> Slack 짝은 어느 계층에도 구현되어 있지 않다. 새 외부 시스템은 DataSource, LifecycleHook, origin channel을 각각 추가한다.

분리 이유:
- **단일 책임**: DataSource는 읽기(수집/조회), Hook은 출처 상태 쓰기, Channel은 사람과의 소통
- **독립 테스트**: hook만 대체해 상태 전이를 테스트할 수 있다
- **조합 가능**: 같은 DataSource에 다른 hook·channel 구성을 조합할 수 있다

---

## workspace 바인딩과 hook 선택

LifecycleHook은 workspace마다 인스턴스가 생성된다. 같은 GitHub DataSource라도 workspace별로 다른 hook 동작이 가능하다. 사용자가 hook 유형을 직접 지정하지 않는다. workspace의 DataSource 유형이 hook 유형을 결정한다.

```mermaid
flowchart TD
    A["hook 트리거 시점"] --> B["workspace 설정 조회 (변경 시 다음 트리거에 반영)"]
    B --> C{"DataSource 유형에<br/>전용 hook 있음?"}
    C -- "있음 (github)" --> D[GitHubLifecycleHook]
    C -- "없음" --> E{"yaml에 on_enter / on_done / on_fail script 있음?"}
    E -- 있음 --> F[ScriptLifecycleHook]
    E -- 없음 --> G[NoopLifecycleHook]
```

- Daemon 재시작 없이 `belt workspace add`로 등록한 workspace는 다음 tick부터 동작한다.
- yaml을 수정하면 다음 hook 트리거에서 최신 설정이 반영된다.

### 내장 hook의 동작

| hook | 동작 |
|------|------|
| GitHubLifecycleHook | `on_escalation(hitl/replan)` 시 이슈에 라벨(기본 `belt:needs-human`) 추가. `on_hitl_resolved` 시 그 라벨 제거. 그 밖의 callback은 사람 대상 메시지를 보내지 않는다 |
| ScriptLifecycleHook | 기존 yaml `on_enter`/`on_done`/`on_fail` script를 실행하는 호환 어댑터. 전용 hook이 없는 DataSource에 쓰인다. `on_escalation`은 무동작이다 |
| NoopLifecycleHook | 아무 것도 하지 않는다 |

> GitHub 이슈 코멘트(시작·실패·escalation·HITL 요청)는 hook이 아니라 GitHub origin channel이 작성한다. 라벨 추가·제거는 hook에 남는다.

> github source에는 `GitHubLifecycleHook`이 항상 우선 적용된다. 이 hook은 PR 생성이나 라벨 전환(`belt:implement` → `belt:review`)을 하지 않는다. 이 작업은 workspace yaml의 `on_done` script에서 구현한다. daemon은 evaluate 성공 후 hook과 무관하게 해당 state의 `on_done` script를 실행한다.

`jira` 등 다른 source는 전용 hook이 없어 `ScriptLifecycleHook` 또는 `NoopLifecycleHook`으로 동작한다. 새 DataSource 유형은 hook 구현을 추가하는 것으로 연결하며 코어를 바꾸지 않는다.

---

## callback과 에러 처리 정책

| callback | 호출 시점 | 실패 시 | 이유 |
|----------|----------|---------|------|
| `on_enter` | Running 진입 후, handler 실행 전 | handler 건너뛰고 escalation 경로 | 전제 조건 미충족 — handler 실행 의미 없음 |
| `on_done` | evaluate가 Done 판정 후. HITL done 후처리에서도 호출 | Failed 상태로 전이 (HITL done 후처리에서는 Hitl→Failed) | 외부 반영 실패 — 수동 확인 필요 |
| `on_fail` | handler 또는 on_enter 실패 시 (retry 제외) | 이력 기록, 상태 전이에 영향 없음 | 이미 실패 경로 — 2차 실패로 흐름 중단하지 않음 |
| `on_escalation` | escalation 결정 후 (모든 액션) | 이력 기록, escalation 진행 | 알림 실패가 escalation 자체를 막으면 안 됨 |
| `on_hitl_resolved` | daemon의 HITL 해결 후처리에서, 결과 전이 전 | 이력 기록, 후처리 진행 (비치명) | 해결 사실의 외부 반영 실패가 해결 자체를 되돌리면 안 됨 |

```
원칙:
  - on_enter/on_done 실패 → 상태 전이에 영향 (치명적)
  - on_fail/on_escalation/on_hitl_resolved 실패 → 이력만 기록 (비치명적)
  - 모든 hook 실패는 전이 이력에 hook 오류로 기록
```

> GitHub 시작 코멘트는 channel `started` 이벤트로 옮겨졌다. 코멘트 작성 실패는 handler 실행을 막지 않고 dashboard에 표시된다. `on_enter` 실패가 handler를 건너뛰게 하는 것은 hook이 상태 반영에 실패한 경우(ScriptLifecycleHook의 사용자 script 등)에 한정된다.

### on_hitl_resolved

HITL 판정이 확정된 뒤 daemon 후처리가 호출한다. GitHub 구현은 `belt:needs-human` 라벨을 제거한다. 어느 경로(GitHub 코멘트, CLI, TUI, timeout)로 해결됐든 같다. 후처리는 at-least-once이므로 `on_hitl_resolved`는 두 번 호출될 수 있고, 구현은 멱등이어야 한다(이미 없는 라벨 제거는 성공으로 본다).

```mermaid
sequenceDiagram
    participant D as daemon 후처리
    participant HK as LifecycleHook
    participant CH as NotificationChannel
    Note over D: HITL 판정이 확정된 요청 발견
    alt done
        D->>HK: on_done
        alt 실패
            HK-->>D: 오류
            D->>HK: on_hitl_resolved (비치명)
            D->>D: Hitl → Failed
            D->>CH: failed
        else 성공
            D->>HK: on_hitl_resolved (비치명)
            D->>D: Hitl → Done
            D->>CH: done
        end
    else skip
        D->>HK: on_hitl_resolved (비치명)
        D->>D: Hitl → Skipped
    else retry / replan (상한 이내)
        D->>HK: on_hitl_resolved (비치명)
        D->>D: Hitl → Pending
    else replan (상한 초과)
        D->>HK: on_hitl_resolved (비치명)
        D->>D: Hitl → Failed
    else expired (timeout)
        D->>HK: on_hitl_resolved (비치명)
        D->>D: Hitl → Skipped (terminal skip) / Pending (terminal replan) / Failed (replan 상한 초과 또는 terminal 해석 불가 시 기본값; 사람의 replan 응답도 상한 초과면 Failed)
    end
```

### on_escalation과 on_fail 호출 순서

escalation 발생 시 두 callback이 순차 호출된다.

```mermaid
flowchart TD
    F["handler 실패"] --> E["① on_escalation (모든 액션)"]
    E --> Q{"retry인가?"}
    Q -- "아니오 (retry_with_comment, hitl)" --> OF["② on_fail"]
    Q -- 예 --> N["on_fail 생략 (조용한 재시도)"]
    OF --> T["③ 상태 전이"]
    N --> T
```

> on_escalation은 escalation 유형(retry/hitl/...)을 받아 유형별로 반응할 수 있다. on_fail은 "실패했다"는 사실만 전달한다.

---

## 기존 yaml on_done/on_fail/on_enter와의 호환

`ScriptLifecycleHook`은 기존 workspace yaml의 `on_enter`/`on_done`/`on_fail` script를 그대로 실행한다. `handlers`만 필수이고 이 세 필드는 비워도 된다. 전용 hook이 있는 DataSource에서도 daemon은 evaluate 성공 후 state의 `on_done` script를 실행한다. PR 생성 같은 출처 작업은 이 script가 맡는다.

GitHub 전용 hook의 코멘트 기본값(완료 시 코멘트는 기본 off, 실패 시 기본 on)은 코멘트 작성이 channel로 이동하면서 channel 이벤트 선택([workspace.yaml 스키마](./workspace-schema.md)의 `notifications`)으로 대체된다.

---

## 영향 범위

| 문서 | 변경 내용 |
|------|----------|
| [notification.md](./notification.md) | 사람 대상 메시지·응답 수신·전이 매핑 표 |
| [datasource.md](./datasource.md) | origin channel 짝, 응답은 channel 책임 |
| [daemon.md](./daemon.md) | HITL 해결 후처리에서 hook 호출 |
| [DESIGN](../DESIGN.md) | OCP 확장점에 LifecycleHook, NotificationChannel |

---

## 수용 기준

- [ ] Daemon Executor는 상태 전이 시 hook callback을 트리거한다
- [ ] hook callback의 구체적 동작은 DataSource 유형별 구현이 결정한다
- [ ] LifecycleHook은 workspace별로 인스턴스가 생성된다
- [ ] `ScriptLifecycleHook`이 기존 yaml script와 호환을 유지한다
- [ ] on_escalation이 escalation 발생 시 항상 호출되고, on_fail은 retry를 제외하고 호출된다
- [ ] on_escalation → on_fail 순서로 호출된다
- [ ] on_enter/on_done 실패 시 상태 전이에 영향을 준다 (escalation / Failed)
- [ ] on_fail/on_escalation 실패 시 이력만 기록하고 흐름을 중단하지 않는다
- [ ] HITL done 후처리에서 on_done이 실패하면 Hitl→Failed로 전이한다
- [ ] `on_hitl_resolved`는 daemon 후처리에서 모든 해결 결과(done 성공, done의 on_done 실패, skip, retry, replan, 만료)마다 결과 전이 직전에 호출되고, 실패해도 후처리가 결과 전이까지 진행한다
- [ ] GitHub는 HITL 해결 후 `belt:needs-human` 라벨을 제거한다 (GitHub 코멘트, CLI, TUI, timeout 어느 경로든 같다)
- [ ] 내장 hook 구현(GitHubLifecycleHook, NoopLifecycleHook)은 사람 대상 메시지를 보내지 않는다. 사람 대상 메시지는 channel이 보낸다 (사용자 정의 script를 실행하는 ScriptLifecycleHook은 제외)
- [ ] 시작 코멘트 작성 실패가 handler 실행을 막지 않는다
- [ ] 모든 hook 실패는 전이 이력에 기록된다
- [ ] 새 DataSource 유형 추가 시 코어 변경 없이 hook 구현만 추가하면 된다
- [ ] handler(prompt/script), hook(출처 상태 반영), channel(알림·응답)이 명확히 분리된다

---

## 미결

없음

---

### 관련 문서

- [DESIGN](../DESIGN.md) — 설계 철학 (Daemon = Orchestrator)
- [NotificationChannel](./notification.md) — 알림·HITL 응답 채널
- [DataSource](./datasource.md) — 수집/컨텍스트 추상화
- [Daemon](./daemon.md) — 실행 루프, Executor, HITL 후처리
- [QueuePhase 상태 머신](./queue-state-machine.md) — 상태 전이 규칙
