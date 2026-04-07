# Daemon — State Machine CPU

> Daemon은 상태 머신을 틱마다 순회하며 전이를 결정하고, hook을 트리거하는 CPU.
> handler(prompt/script)를 실행하고, 상태 전이 시 workspace의 LifecycleHook을 트리거한다.
> GitHub 라벨, PR 생성 같은 도메인 로직을 모른다 — hook.on_*()의 Result만 받을 뿐.
> 내부는 Advancer·Executor·HitlService 모듈로 분리. 실패 시 StagnationDetector + LateralAnalyzer가 사고를 전환하여 재시도한다.

---

## 역할

```
1. 수집: DataSource.collect() → Pending에 넣기
2. 전이: Pending → Ready → Running (자동, concurrency 제한)
3. 트리거: Running 진입 시 hook.on_enter() 트리거
4. 실행: yaml에 정의된 handler(prompt/script) 실행
5. 완료: handler 성공 → Completed 전이
6. 분류: evaluate가 Completed → Done or HITL 판정 (per-item)
7. 반응: 상태 전이 시 hook.on_done/on_fail/on_escalation 트리거
8. 스케줄: Cron engine으로 주기 작업 실행

Daemon이 아는 것: 상태 머신 + 언제 어떤 hook을 트리거할지
Daemon이 모르는 것: hook이 실제로 무엇을 하는지 (Result만 받음)
```

---

## 내부 모듈 구조 (#717)

Daemon은 상태 머신을 순회하며 전이를 결정하고 hook을 트리거하는 CPU이다.

```
Daemon (CPU)
  loop {
    collector.collect()
    evaluator.evaluate()     // 판정 — 실행보다 먼저
    advancer.advance()
    executor.execute()       // handler 실행 + hook 트리거
    cron_engine.tick()
  }
```

| 모듈 | 책임 | 소유하는 상태 |
|------|------|-------------|
| **Advancer** | Pending→Ready→Running 전이, dependency gate (DB), conflict 검출 | queue, ConcurrencyTracker |
| **Executor** | handler 실행 + hook 트리거, 실패 시 stagnation 분석 + lateral plan + escalation | ActionExecutor, StagnationDetector, LateralAnalyzer |
| **Evaluator** | Completed → Done/HITL 분류 (per-item, 이미 분리됨) | eval_failure_counts |
| **HitlService** | HITL 응답 처리, timeout 만료, terminal action 적용 | — (DB 직접 조회) |
| **CronEngine** | cron tick, force_trigger (이미 분리됨) | CronJob 목록 |

### Executor 내부 구조

```
Executor
  │
  ├── ActionExecutor          handler(prompt/script) 실행
  │
  ├── hook: &dyn LifecycleHook   상태 전이 시 트리거 (실행 책임은 Hook impl)
  │     ├── on_enter()
  │     ├── on_done()
  │     ├── on_fail()
  │     └── on_escalation()
  │
  ├── StagnationDetector      실패 시 패턴 탐지
  │     └── judge: Box<dyn SimilarityJudge>
  │           └── CompositeSimilarity
  │                 ├── ExactHash        (w: 0.5)
  │                 ├── TokenFingerprint (w: 0.3)
  │                 └── NCD              (w: 0.2)
  │
  └── LateralAnalyzer         패턴 감지 시 사고 전환
        └── personas/          (include_str! 내장)
              hacker.md, architect.md, researcher.md,
              simplifier.md, contrarian.md
```

### 모듈 간 의존

```
Daemon
  ├── Advancer (queue, db, dependency_guard)
  ├── Executor (action_executor, stagnation_detector, lateral_analyzer)
  ├── Evaluator (workspace_config)
  ├── HitlService (db)
  └── CronEngine (db)
```

- 모듈 간 의존은 trait 또는 함수 파라미터로만 전달 (순환 참조 금지)
- 각 모듈은 독립적으로 단위 테스트 가능
- StagnationDetector는 `Box<dyn SimilarityJudge>` 하나만 의존 (Composite 또는 단일)

---

## Concurrency 제어

두 레벨로 동시 실행을 제어한다:

```yaml
# workspace.yaml — workspace 루트 레벨에 정의
concurrency: 2                    # 이 workspace에서 동시 Running 아이템 수

# daemon 글로벌 설정 (별도 config) — 전체 workspace 합산 상한
max_concurrent: 4
```

- **workspace.concurrency**: workspace yaml 루트에 정의. "이 프로젝트에 동시에 몇 개까지 돌릴까". 모든 source의 아이템 합산 기준.
- **daemon.max_concurrent**: "머신 리소스 한계" (Evaluator의 LLM 호출도 slot을 소비)

> **LateralAnalyzer의 LLM 호출**: handler 실패 경로에서 `belt agent -p`를 호출하여 lateral_plan을 생성한다. 이 호출은 해당 아이템이 이미 점유한 Running slot 안에서 수행되므로 추가 slot을 소비하지 않는다.

Advancer는 `Ready → Running` 전이 시 두 제한을 모두 확인한다.

---

## 실행 루프 (의사코드)

```
loop {
    // 1. 수집
    for binding in workspace_bindings:
        for source in binding.sources:
            items = source.collect()
            queue.push(Pending, items)

    // 2. 판정 (Evaluator) — 실행보다 먼저
    //    Completed 아이템을 비용 순으로 판정: Mechanical → Semantic → (Consensus)
    //    Ready 아이템 중 이전 기록으로 판정 가능한 것은 handler 실행 없이 판정
    evaluator.evaluate()

    // 3. 자동 전이 (Advancer)
    advancer.advance_pending_to_ready()         // spec dep gate (DB)
    advancer.advance_ready_to_running(limit)    // queue dep gate (DB) + concurrency

    // 4. 실행 (Executor)
    for item in queue.get_new(Running):
        binding = lookup_workspace_binding(item)
        hook = binding.hook                     // 이 workspace의 LifecycleHook
        state = lookup_state(item)
        worktree = create_or_reuse_worktree(item)
        ctx = build_hook_context(item, worktree)

        // on_enter hook 트리거 (실패 시 handler 건너뛰고 실패 경로)
        result = hook.on_enter(&ctx)
        if result.failed:
            executor.handle_failure(item, hook)
            continue

        // handlers 순차 실행 (lateral_plan 있으면 prompt에 주입)
        for action in state.handlers:
            result = executor.execute(action, WORK_ID=item.id, WORKTREE=worktree,
                                      lateral_plan=item.lateral_plan)
            if result.failed:
                executor.handle_failure(item, hook)
                break
        else:
            item.transit(Completed)

    // 5. cron tick (품질 루프: gap-detection, knowledge-extract 등)
    cron_engine.tick()
}
```

### Executor.handle_failure — Stagnation + Lateral + Hook 트리거

```
fn handle_failure(item, hook):
    // ① Stagnation Detection (항상 실행)
    //    각 PatternDetector가 DB에서 자기 관심사 데이터를 직접 조회
    detections = stagnation_detector.detect(item.source_id, item.state, db)
    active = detections.filter(|d| d.detected && d.confidence >= threshold)

    // ② Lateral Plan 생성 (패턴 감지 시)
    lateral_plan = None
    if active.is_not_empty() && lateral_config.enabled:
        tried = db.get_tried_personas(item.source_id, item.state)
        persona = select_persona(active[0].pattern, tried)
        if persona.is_some():
            lateral_plan = lateral_analyzer.analyze(
                detection=active[0],
                persona=persona,
                workspace=item.workspace_id,
            )

    // ③ transition_events에 stagnation 기록
    record_stagnation_event(item, detections, lateral_plan)

    // ④ Escalation 결정 (failure_count 기반)
    failure_count = count_failures(item.source_id, item.state)
    escalation = lookup_escalation(failure_count)

    // ⑤ Hook 트리거 — Daemon은 트리거만, 실행 책임은 Hook impl
    ctx = build_hook_context(item, worktree)
    hook.on_escalation(&ctx, escalation)

    if escalation.should_run_on_fail():
        hook.on_fail(&ctx)

    // ⑥ 상태 전이
    match escalation:
        retry | retry_with_comment:
            new_item = create_retry_item(item, lateral_plan)
            // worktree 보존

        hitl:
            lateral_report = build_lateral_report(item.source_id, item.state)
            create_hitl_event(item, reason, hitl_notes=lateral_report)
            // worktree 보존
```

### lateral_plan이 handler에 주입되는 방식

retry로 생성된 새 아이템이 다시 Running에 진입하면, `lateral_plan`이 handler prompt에 추가 컨텍스트로 주입된다:

```
원래 prompt: "이슈를 구현해줘"

주입 후:
  "이슈를 구현해줘

   ⚠ Stagnation Analysis (attempt 2/3)
   Pattern: SPINNING | Persona: HACKER

   실패 원인: 이전 2회 시도에서 동일한 컴파일 에러 반복
   대안 접근법: tower-sessions crate 활용
   실행 계획: 1. Cargo.toml 수정  2. 타입 교체  3. middleware 등록
   주의: 이전과 동일한 접근은 같은 실패를 반복합니다"
```

---

## Dependency Gate (#721)

### Spec Dependency Gate

`check_dependency_gate()` — Pending→Ready 전이 시 확인. 스펙 간 의존 관계 확인.

### Queue Dependency Gate

`check_queue_dependency_gate()` — Ready→Running 전이 시 확인.

dependency phase 확인은 **DB 조회 기반**:

```
1. DB에서 dependency work_id 목록 조회
2. 각 dependency의 phase를 DB에서 조회
3. 판정:
   - Done → gate open
   - DB에 없음 → gate open (orphan 허용)
   - 그 외 → gate blocked
```

### Conflict Gate

`check_conflict_gate()` — entry_point 겹침 감지. DB 기반.

---

## Handler와 Hook의 분리

### Handler — yaml에 정의된 작업

```yaml
handlers:
  - prompt: "..."    # → AgentRuntime.invoke() (LLM, worktree 안에서)
  - script: "..."    # → bash 실행 (결정적, WORK_ID + WORKTREE 주입)
```

handler는 Daemon Executor가 직접 실행한다. 작업의 핵심 로직.

### Hook — LifecycleHook trait impl

| hook | 트리거 시점 | 실행 책임 |
|------|-----------|----------|
| `on_enter` | Running 진입 후, handler 실행 전 | Hook impl |
| `on_done` | evaluate Done 판정 후 | Hook impl |
| `on_fail` | 실패 시 (retry 제외) | Hook impl |
| `on_escalation` | escalation 결정 후 | Hook impl |

Daemon은 hook을 트리거만 한다. hook이 실제로 무엇을 하는지 모른다.
상세: [LifecycleHook](./lifecycle-hook.md)

---

## 환경변수

| 변수 | 설명 |
|------|------|
| `WORK_ID` | 큐 아이템 식별자 |
| `WORKTREE` | worktree 경로 |

나머지는 `belt context $WORK_ID --json`으로 조회.

---

## Graceful Shutdown

```
SIGINT → on_shutdown:
  1. Running 아이템 완료 대기 (timeout: 30초)
     → timeout 초과: Pending으로 롤백, worktree 보존
  2. Cron engine 정지
```

---

## 수용 기준

### Daemon = CPU

- [ ] Daemon은 상태 머신 순회 + hook 트리거만 담당한다
- [ ] 상태 전이 시 workspace의 LifecycleHook.on_*()을 트리거한다
- [ ] hook의 실행 결과(Result)만 받고, 구체적 동작을 모른다

### 내부 모듈 구조 (#717)

- [ ] phase 전이는 Advancer, handler 실행+hook 트리거+stagnation+lateral은 Executor, HITL은 HitlService
- [ ] 각 모듈은 독립적으로 단위 테스트 가능하다
- [ ] 모듈 간 의존은 trait 또는 함수 파라미터로만 전달 (순환 참조 금지)

### Stagnation + Lateral 통합 (#723)

- [ ] handler/on_enter 실패 시 StagnationDetector가 항상 실행된다
- [ ] CompositeSimilarity로 outputs/errors를 별도 검사한다
- [ ] 패턴 감지 시 LateralAnalyzer가 내장 페르소나로 lateral_plan을 생성한다
- [ ] lateral_plan이 retry 시 handler prompt에 추가 컨텍스트로 주입된다
- [ ] hitl 도달 시 모든 lateral 시도 이력이 hitl_notes에 첨부된다
- [ ] stagnation 이벤트가 transition_events에 기록된다

### Dependency Gate (#721)

- [ ] queue dependency의 phase 확인은 DB 조회 기준이다
- [ ] 재시작 후에도 dependency gate가 정확히 동작한다
- [ ] dependency가 Failed/Hitl이면 blocked, DB에 없으면 open

### Concurrency

- [ ] workspace.concurrency + daemon.max_concurrent 2단계 제한
- [ ] evaluate LLM 호출도 concurrency slot 소비

### Graceful Shutdown

- [ ] SIGINT → 30초 대기 → Pending 롤백 + worktree 보존

### 환경변수

- [ ] handler/script에 WORK_ID, WORKTREE 2개만 주입

---

### 관련 문서

- [DESIGN](../DESIGN.md) — 전체 상태 흐름 + 설계 철학
- [LifecycleHook](./lifecycle-hook.md) — 상태 전이 반응 trait
- [QueuePhase 상태 머신](./queue-state-machine.md) — 상태 전이 상세
- [Stagnation Detection](./stagnation.md) — Composite Similarity + Lateral Thinking
- [DataSource](./datasource.md) — 수집/컨텍스트 추상화
- [AgentRuntime](./agent-runtime.md) — LLM 실행 추상화
- [Cron 엔진](./cron-engine.md) — 품질 루프 (gap-detection 등)
