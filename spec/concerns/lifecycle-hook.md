# LifecycleHook — 상태 전이 반응 추상화

> Daemon이 상태를 전이할 때, 해당 workspace의 LifecycleHook이 외부 시스템에 반응한다.
> handler(prompt/script)는 "무엇을 실행할지", hook은 "상태가 바뀌었을 때 어떻게 반응할지".
> 새 DataSource 유형 추가 = LifecycleHook impl 추가, 코어 변경 0 (OCP).

---

## 설계 배경

`LifecycleHook` trait이 없던 시절에는 on_done/on_fail/on_enter가 yaml `Vec<ScriptAction>`으로만 정의되어 Daemon(Executor)이 이를 직접 subprocess로 실행했다. 이 구조에는 다음 문제가 있었다.

1. **handler와 hook이 같은 곳(yaml)에 혼재** — 성격이 다른 관심사가 구분되지 않음
2. **DataSource별 반응을 정의할 수 없음** — GitHub/Jira/Slack 모두 bash script로만 표현
3. **on_escalation이 없음** — escalation 발생 시 외부 시스템 반응 경로가 없음
4. **Daemon이 도메인을 침범** — Executor가 `gh issue comment` 같은 script를 직접 실행

아래 설계로 handler(yaml 정의)와 hook(trait impl)을 분리해 이 문제들을 해소했다.

---

## 설계 — handler와 hook 분리

### 핵심 구분

| | handler | hook |
|---|---------|------|
| **역할** | 작업 자체 (분석, 구현, 리뷰) | 상태 변화에 대한 외부 시스템 반응 |
| **정의** | workspace yaml (prompt/script) | LifecycleHook trait impl |
| **실행 주체** | Daemon Executor | Daemon이 트리거, Hook impl이 실행 |
| **소유** | workspace yaml | DataSource 유형 + workspace 설정 |
| **예시** | "이슈를 구현해줘" | PR 생성, 라벨 변경, 코멘트 작성 |

### Daemon = CPU 비유

```
Daemon (CPU) — tick마다:
  │
  ├── 큐 스캔 → 아이템 상태 확인
  │
  ├── 상태 전이 결정 (Advancer)
  │
  ├── handler 실행 (Executor) — yaml prompt/script
  │
  └── 전이 발생 시 hook 트리거
        → workspace에 바인딩된 LifecycleHook.on_*() 호출
        → Hook impl이 자기 시스템에 맞게 반응
```

---

## trait 정의

```rust
/// 상태 전이 시 외부 시스템에 반응하는 lifecycle hook.
///
/// DataSource 유형별로 impl을 제공하고, workspace별로 인스턴스가 생성된다.
/// Daemon은 상태 전이 시점에 해당 hook을 트리거할 뿐, 구체적 반응을 모른다.
#[async_trait]
pub trait LifecycleHook: Send + Sync {
    /// Running 진입 후, handler 실행 전.
    /// 실패 시 handler를 건너뛰고 escalation 경로로 진입.
    async fn on_enter(&self, ctx: &HookContext) -> Result<()>;

    /// evaluate가 Done 판정 후 호출.
    /// 실패 시 Failed 상태로 전이.
    async fn on_done(&self, ctx: &HookContext) -> Result<()>;

    /// handler 또는 on_enter 실패 시 호출.
    /// escalation level에 따라 조건부 (retry에서는 호출 안 함).
    async fn on_fail(&self, ctx: &HookContext) -> Result<()>;

    /// escalation 결정 후 호출.
    /// DataSource가 자기 시스템에 맞는 반응 수행.
    async fn on_escalation(&self, ctx: &HookContext, action: EscalationAction) -> Result<()>;
}

/// Hook에 전달되는 컨텍스트.
/// belt context CLI와 동일한 정보를 구조체로 제공.
pub struct HookContext {
    pub work_id: String,
    pub worktree: PathBuf,
    pub item: QueueItem,
    pub item_context: ItemContext,     // DataSource.get_context() 결과
    pub failure_count: u32,
}
```

---

## DataSource와 LifecycleHook 관계

```
DataSource (trait)          LifecycleHook (trait)
  │                           │
  ├── collect()               ├── on_enter()
  ├── get_context()           ├── on_done()
  │                           ├── on_fail()
  │                           └── on_escalation()
  │                           
  │  수집 + 컨텍스트            상태 전이 반응
  │  (읽기)                    (쓰기/반영)

GitHubSource ◄──────────────► GitHubLifecycleHook
JiraSource   ◄──────────────► JiraLifecycleHook
SlackSource  ◄──────────────► SlackLifecycleHook
```

분리 이유:
- **단일 책임**: DataSource는 읽기(수집/조회), Hook은 쓰기(반영)
- **독립 테스트**: Hook만 mock하여 상태 전이 테스트 가능
- **조합 가능**: 같은 DataSource에 다른 Hook 전략을 조합할 수 있음

---

## workspace 바인딩

LifecycleHook은 workspace마다 인스턴스가 생성된다. 같은 GitHub DataSource라도 workspace별로 다른 hook 동작이 가능하다.

```rust
/// workspace 등록 시 생성되는 바인딩.
struct WorkspaceBinding {
    config: WorkspaceConfig,
    sources: Vec<Box<dyn DataSource>>,
    hook: Box<dyn LifecycleHook>,       // 이 workspace의 lifecycle
}
```

### hook 동작은 어디서 정의하는가

workspace yaml에서 hook 설정을 정의하고, DataSource 유형에 맞는 Hook impl이 설정을 해석한다.

```yaml
# workspace.yaml
name: auth-project
sources:
  github:
    url: https://github.com/org/repo
    states:
      implement:
        trigger: { label: "belt:implement" }
        handlers:
          - prompt: "이슈를 구현해줘"    # handler — 작업

# hook 동작은 DataSource 유형(github)이 결정한다.
# GitHubLifecycleHook의 현재 동작 (모두 `gh` CLI 기반):
#   on_enter     → 이슈에 "작업 시작" 코멘트
#   on_done      → 이슈에 "완료" 코멘트 (comment_on_done, 기본 false → 기본은 무동작)
#   on_fail      → 이슈에 실패 코멘트 (comment_on_fail, 기본 true)
#   on_escalation(hitl/replan) → 이슈에 라벨 추가(기본 belt:needs-human) + 코멘트
#   on_escalation(retry_with_comment) → 코멘트만
#   on_escalation(retry) → 무동작 (조용한 재시도)
# PR 생성이나 라벨 전환(belt:implement → belt:review)은 이 hook이 아니라
# workspace yaml의 on_done script(ScriptLifecycleHook 경로)에서 별도로 구현해야 한다.
```

### hook 동작 커스터마이징 (현재 미구현)

`GitHubLifecycleHook`은 `comment_on_done`(기본 false), `comment_on_fail`(기본 true), `hitl_label`(기본 `belt:needs-human`) 필드를 갖는 `GitHubHookConfig`로 동작을 바꿀 수 있도록 설계되어 있다. 하지만 `SourceConfig`(workspace yaml의 `sources.{type}` 스키마)에는 `hooks` 필드가 없고, hook 로딩 경로(`hook_cache.rs`)도 `HookParams::new(source_type, url)`로 기본값만 생성한다 — yaml로 이 값들을 오버라이드하는 경로는 아직 없다. 커스터마이징하려면 `GitHubHookConfig`/`HookParams` 생성 코드를 직접 바꿔야 한다.

### Hook impl 동적 로딩

Hook impl은 Daemon 시작 시 일괄 생성하지 않는다. hook 트리거 시점에 DB에서 workspace 정보를 조회하고, 필요한 Hook impl을 동적으로 로드한다.

```
hook 트리거 시점 (on_enter, on_done, on_fail, on_escalation):
  1. DB에서 workspace 조회 (config_path), updated_at으로 캐시 유효성 확인
  2. 캐시 미스 시 yaml 파싱 → sources 키에서 DataSource 유형 식별
  3. DataSource 유형별 전용 Hook을 우선 시도:
       github → GitHubLifecycleHook
       그 외(jira 등) → 아직 전용 impl 없음, 다음 단계로 폴백
  4. 전용 Hook이 없고 yaml에 on_enter/on_done/on_fail script가 존재하면
       → ScriptLifecycleHook 어댑터 사용
  5. 둘 다 없으면 NoopLifecycleHook
  6. hook.on_*() 실행
```

동적 로딩의 이점:
- **실행 중 workspace 추가**: Daemon 재시작 없이 `belt workspace add`로 등록하면 다음 tick부터 동작
- **설정 변경 즉시 반영**: yaml 수정 시 다음 hook 트리거에서 최신 설정 반영
- **메모리 절약**: 사용하지 않는 workspace의 Hook impl을 메모리에 유지하지 않음

사용자가 직접 Hook 유형을 지정할 필요 없다 — DB에 기록된 workspace의 DataSource 유형이 곧 Hook 유형을 결정한다.

---

## Hook 에러 처리 정책

| hook | 실패 시 | 이유 |
|------|---------|------|
| `on_enter()` | handler 건너뛰고 escalation 경로 | 전제 조건 미충족 — handler 실행 의미 없음 |
| `on_done()` | Failed 상태로 전이 | 외부 반영 실패 — 수동 확인 필요 |
| `on_fail()` | 로그 기록, 상태 전이에 영향 없음 | 이미 실패 경로 — 2차 실패로 흐름 중단하지 않음 |
| `on_escalation()` | 로그 기록, escalation 진행 | 알림 실패가 escalation 자체를 막으면 안 됨 |

```
원칙:
  - on_enter/on_done 실패 → 상태 전이에 영향 (치명적)
  - on_fail/on_escalation 실패 → 로그만 기록 (비치명적)
  - 모든 hook 실패는 transition_events에 event_type='hook_error'로 기록
```

### on_escalation과 on_fail 호출 순서

escalation 발생 시 두 hook이 순차 호출된다:

```
executor.handle_failure(item, hook):
    escalation = lookup_escalation(failure_count)

    // ① on_escalation — 항상 호출 (모든 escalation 액션에 대해)
    hook.on_escalation(&ctx, escalation)

    // ② on_fail — 조건부 호출 (retry 제외)
    if escalation.should_run_on_fail():  // retry_with_comment, hitl
        hook.on_fail(&ctx)

    // ③ 상태 전이
    match escalation: ...
```

> on_escalation은 escalation 유형(retry/hitl/...)을 `action` 파라미터로 받아 유형별 반응이 가능하다.
> on_fail은 "실패했다"는 사실만 전달한다. retry에서는 "조용한 재시도"이므로 호출하지 않는다.

---

## Daemon 실행 루프 변경

```
loop {
    // 1. 수집
    for binding in workspaces:
        for source in binding.sources:
            items = source.collect()
            queue.push(Pending, items)

    // 2. 판정 (Evaluator) — 실행보다 먼저
    evaluator.evaluate()

    // 3. 전이 (Advancer) — 변경 없음
    advancer.advance_pending_to_ready()
    advancer.advance_ready_to_running(limit)

    // 4. 실행 (Executor)
    for item in queue.get_new(Running):
        binding = lookup_workspace_binding(item)
        hook = binding.hook

        // on_enter hook
        result = hook.on_enter(&ctx)
        if result.failed:
            executor.handle_failure(item, hook)
            continue

        // handlers 순차 실행 — yaml prompt/script (변경 없음)
        for action in state.handlers:
            result = executor.execute(action, ...)
            if result.failed:
                executor.handle_failure(item, hook)
                break
        else:
            item.transit(Completed)

    // 5. cron tick — 변경 없음
    cron_engine.tick()
}

fn handle_failure(item, hook):
    // stagnation detection + lateral plan (변경 없음)
    ...

    // escalation 결정 (failure_count 기반, 변경 없음)
    escalation = lookup_escalation(failure_count)

    // hook 트리거
    hook.on_escalation(&ctx, escalation)

    // on_fail 조건부 호출 (retry 제외)
    if escalation.should_run_on_fail():
        hook.on_fail(&ctx)

    match escalation:
        retry → create_retry_item(item, lateral_plan)
        retry_with_comment → create_retry_item(item, lateral_plan)
        hitl → create_hitl_event(item, ...)
```

---

## 기존 yaml on_done/on_fail/on_enter와의 호환

### ScriptLifecycleHook — 호환 어댑터

`ScriptLifecycleHook` — 기존 yaml script를 LifecycleHook trait으로 감싸는 어댑터. DataSource별 전용 Hook이 없을 때(또는 해당 source_type이 아직 지원되지 않을 때) 폴백으로 쓰인다.

```rust
/// 기존 yaml script 기반 hook을 LifecycleHook trait으로 감싸는 어댑터.
/// 기존 workspace yaml과의 호환성을 유지한다.
struct ScriptLifecycleHook {
    state_configs: HashMap<String, StateConfig>,
}

#[async_trait]
impl LifecycleHook for ScriptLifecycleHook {
    async fn on_done(&self, ctx: &HookContext) -> Result<()> {
        let state = &self.state_configs[&ctx.item.state];
        for script in &state.on_done {
            execute_script(&script.script, &ctx.work_id, &ctx.worktree)?;
        }
        Ok(())
    }

    async fn on_escalation(&self, _ctx: &HookContext, _action: EscalationAction) -> Result<()> {
        Ok(()) // 기존 yaml에는 on_escalation 개념 없음 — no-op
    }
    // ...
}
```

### DataSource별 전용 Hook — GitHubLifecycleHook

`crates/belt-infra/src/hooks/github.rs`에 구현되어 있고 `hooks::create_hook()`을 통해 GitHub source에 대해 항상 우선 선택된다(`ScriptLifecycleHook`보다 먼저 시도됨). `GitHubHookConfig`는 코드 상수 기본값(`comment_on_done: false`, `comment_on_fail: true`, `hitl_label: "belt:needs-human"`)으로만 생성되고, yaml에서 오버라이드하는 경로는 없다.

```rust
pub struct GitHubLifecycleHook {
    config: GitHubHookConfig,
    shell: Arc<dyn ShellExecutor>,
}

impl LifecycleHook for GitHubLifecycleHook {
    async fn on_enter(&self, ctx: &HookContext) -> Result<()> {
        // 이슈에 "작업 시작" 코멘트 (gh issue comment)
    }
    async fn on_done(&self, ctx: &HookContext) -> Result<()> {
        // comment_on_done이 true일 때만 "완료" 코멘트
    }
    async fn on_fail(&self, ctx: &HookContext) -> Result<()> {
        // comment_on_fail이 true일 때 실패 코멘트
    }
    async fn on_escalation(&self, ctx: &HookContext, action: EscalationAction) -> Result<()> {
        match action {
            EscalationAction::Hitl | EscalationAction::Replan => {
                // hitl_label 추가 + escalation 코멘트
            }
            EscalationAction::RetryWithComment => {
                // 재시도 코멘트만
            }
            EscalationAction::Retry => {} // 조용한 재시도 — 무동작
            EscalationAction::Skip => {
                // skip 코멘트
            }
        }
        Ok(())
    }
}
```

`jira` 등 다른 source_type은 아직 전용 Hook impl이 없다 — `hooks::create_hook()`이 "unsupported" 에러를 반환하고 `ScriptLifecycleHook` 또는 `NoopLifecycleHook`으로 폴백한다. 새 DataSource 유형을 추가하려면 `LifecycleHook`을 구현하고 `hooks::create_hook()`의 매치암에 등록하면 된다(코어 변경 없음, OCP).

---

## StateConfig (현재 구조)

`on_enter`/`on_done`/`on_fail`은 `#[serde(default)]`로 남아 있어 기존 yaml과 호환되며, `ScriptLifecycleHook` 어댑터가 이를 소비한다. `handlers`만 필수로 채우면 되고, 이 세 필드는 비워도 된다.

```rust
pub struct StateConfig {
    pub trigger: TriggerConfig,
    pub handlers: Vec<HandlerConfig>,
    #[serde(default)]
    pub on_enter: Vec<ScriptAction>,
    #[serde(default)]
    pub on_done: Vec<ScriptAction>,
    #[serde(default)]
    pub on_fail: Vec<ScriptAction>,
}
```

이 세 필드를 완전히 제거하고 hook 설정 전부를 DataSource별 Hook impl로 옮기는 것은 아직 하지 않은 변경이다 — 필드를 제거하면 `ScriptLifecycleHook` 경로 자체가 성립하지 않으므로, 제거하려면 모든 지원 source_type에 전용 Hook impl이 먼저 갖춰져야 한다.

---

## 동적 로딩과 메모리

Hook impl은 트리거 시점에 생성되고, 실행 후 캐시된다. Daemon은 workspace 메타정보(DB)만 상시 유지하고, 전체 yaml을 미리 파싱해 메모리에 올려두지 않는다.

```
Daemon 시작 → DB에서 workspace 목록만 조회
hook 트리거 시 → DB(config_path, updated_at) → 캐시 확인 → 미스 시 yaml 파싱 → Hook impl 생성 → 캐시에 저장 → 실행
```

workspace가 늘어나도 Daemon의 메모리 부담이 선형 증가하지 않는다. 자주 트리거되는 workspace의 Hook impl은 LRU 캐시(`hook_cache.rs`)로 재사용하고, yaml 변경 시(`updated_at` 비교) 캐시를 무효화한다.

---

## 영향 범위

| 문서/모듈 | 변경 내용 |
|-----------|----------|
| `belt-core/source.rs` | DataSource trait 변경 없음 |
| `belt-core/` (신규) | `LifecycleHook` trait + `HookContext` 정의 |
| `belt-daemon/daemon.rs` | Executor가 hook.on_*() 트리거로 변경 |
| `datasource.md` | hook 분리 반영, on_done/on_fail yaml 정의 → trait 위임 |
| `daemon.md` | Executor 실행 루프에서 hook 트리거 반영 |
| `DESIGN.md` | OCP 확장점에 LifecycleHook 추가 |

---

## 수용 기준

- [ ] `LifecycleHook` trait이 belt-core에 정의된다
- [ ] Daemon Executor는 상태 전이 시 hook.on_*()을 트리거한다
- [ ] hook.on_*()의 구체적 동작은 DataSource 유형별 impl이 결정한다
- [ ] LifecycleHook은 workspace별로 인스턴스가 생성된다
- [ ] `ScriptLifecycleHook` 어댑터가 기존 yaml script와 호환을 유지한다
- [ ] on_escalation이 escalation 발생 시 항상 호출되고, on_fail은 retry를 제외하고 호출된다
- [ ] on_escalation → on_fail 순서로 호출된다
- [ ] on_enter/on_done 실패 시 상태 전이에 영향을 준다 (escalation / Failed)
- [ ] on_fail/on_escalation 실패 시 로그만 기록하고 흐름을 중단하지 않는다
- [ ] 모든 hook 실패는 transition_events에 기록된다
- [ ] 새 DataSource 유형 추가 시 코어 변경 없이 Hook impl만 추가하면 된다
- [ ] handler(prompt/script)와 hook(lifecycle 반응)이 명확히 분리된다

---

### 관련 문서

- [DESIGN](../DESIGN.md) — 설계 철학 (Daemon = Orchestrator)
- [DataSource](./datasource.md) — 수집/컨텍스트 추상화
- [Daemon](./daemon.md) — 실행 루프, Executor 모듈
- [QueuePhase 상태 머신](./queue-state-machine.md) — 상태 전이 규칙
