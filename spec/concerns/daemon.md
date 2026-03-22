# Daemon — 상태 머신 + 실행기

> Daemon은 yaml에 정의된 prompt/script를 호출하는 단순 실행기.
> GitHub 라벨, PR 생성 같은 도메인 로직을 모른다.

---

## 역할

```
1. 수집: DataSource.collect() → Pending에 넣기
2. 전이: Pending → Ready → Running (자동, concurrency 제한)
3. 실행: yaml에 정의된 prompt/script 호출
4. 완료: handler 성공 → Completed 전이
5. 분류: evaluate cron이 Completed → Done or HITL 판정 (CLI 도구 호출)
6. 반영: on_done/on_fail script 실행
7. 스케줄: Cron engine으로 주기 작업 실행
```

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
- **daemon.max_concurrent**: "머신 리소스 한계" (evaluate cron의 LLM 호출도 slot을 소비)

> **주의**: `concurrency`는 workspace 루트에 위치한다 (`sources.github` 하위가 아님). 하나의 workspace에 여러 source가 있을 수 있으므로, per-source가 아닌 per-workspace 기준으로 제어한다.

Daemon은 `Ready → Running` 전이 시 두 제한을 모두 확인한다.

---

## 실행 루프 (의사코드)

```
loop {
    // 1. 수집
    for source in workspace.sources:
        items = source.collect()
        queue.push(Pending, items)

    // 2. 자동 전이 + 실행 (2단계 concurrency 제한)
    queue.advance_all(Pending → Ready)
    ws_slots = workspace.concurrency - queue.count(Running, workspace)
    global_slots = daemon.max_concurrent - queue.count_all(Running) - active_evaluate_count
    limit = min(ws_slots, global_slots)
    queue.advance(Ready → Running, limit=limit)

    for item in queue.get_new(Running):
        state = lookup_state(item)

        // worktree 생성 (인프라)
        worktree = create_or_reuse_worktree(item)

        // on_enter (실패 시 handler를 실행하지 않고 escalation 적용)
        result = run_actions(state.on_enter, WORK_ID=item.id, WORKTREE=worktree)
        if result.failed:
            failure_count = count_failures(item.source_id, item.state)
            escalation = lookup_escalation(failure_count)
            if escalation != retry:
                run_actions(state.on_fail, WORK_ID=item.id, WORKTREE=worktree)
            apply_escalation(item, escalation)
            continue

        // handlers 순차 실행
        for action in state.handlers:
            result = execute(action, WORK_ID=item.id, WORKTREE=worktree)
            if result.failed:
                failure_count = count_failures(item.source_id, item.state)  // history에서 계산
                escalation = lookup_escalation(failure_count)
                if escalation != retry:
                    run_actions(state.on_fail, WORK_ID=item.id, WORKTREE=worktree)
                apply_escalation(item, escalation)  // retry: worktree 보존
                break
        else:
            // 모든 handler 성공 → Completed
            queue.transit(item, Completed)
            force_trigger("evaluate")

    // 3. cron tick (evaluate, gap-detection 등)
    cron_engine.tick()
}

// evaluate cron (force_trigger 가능):
// LLM이 직접 CLI를 호출하여 상태 전이 (JSON 파싱 불필요)
for item in queue.get(Completed):
    autodev_agent_p(workspace, "Completed 아이템 $WORK_ID 의 완료 여부를 판단하고,
        autodev queue done $WORK_ID 또는 autodev queue hitl $WORK_ID 를 실행해줘")
    // → LLM이 context를 조회하고 판단 후 CLI 실행
    //   autodev queue done $WORK_ID  → on_done script 실행 → Done (worktree 정리)
    //                                  └── script 실패 → Failed (worktree 보존)
    //   autodev queue hitl $WORK_ID  → HITL 이벤트 생성 (worktree 보존)
```

---

## 통합 액션 타입

handler, on_done, on_fail, on_enter 전부 같은 두 가지 타입:

```yaml
- prompt: "..."    # → AgentRuntime.invoke() (LLM, worktree 안에서)
- script: "..."    # → bash 실행 (결정적, WORK_ID + WORKTREE 주입)
```

script 안에서 `autodev context $WORK_ID --json`을 호출하여 필요한 정보를 조회한다.

---

## 환경변수

Daemon이 prompt/script 실행 시 주입하는 환경변수는 **2개만**:

| 변수 | 설명 |
|------|------|
| `WORK_ID` | 큐 아이템 식별자 |
| `WORKTREE` | worktree 경로 |

나머지는 `autodev context $WORK_ID --json`으로 조회. 상세는 [DataSource](./datasource.md) 참조.

---

## Graceful Shutdown

```
SIGINT → on_shutdown:
  1. Running 아이템 완료 대기 (timeout: 30초)
     → timeout 초과: Pending으로 롤백, worktree 보존
       (재시작 후 해당 아이템이 다시 Ready → Running 전이 시 기존 worktree를 재사용)
  2. Cron engine 정지
```

> **worktree 보존 원칙**: shutdown 롤백 시 worktree를 정리하지 않는다. retry와 동일하게, 재시작 후 이전 작업 위에서 이어서 진행할 수 있도록 worktree를 보존한다. 좀비 worktree는 `log-cleanup` cron이 TTL 기반으로 정리한다.

---

### 관련 문서

- [DESIGN-v5](../DESIGN-v5.md) — 설계 철학
- [QueuePhase 상태 머신](./queue-state-machine.md) — 상태 전이 상세
- [DataSource](./datasource.md) — 워크플로우 정의 + context 스키마
- [AgentRuntime](./agent-runtime.md) — LLM 실행 추상화
- [Cron 엔진](./cron-engine.md) — evaluate cron + 품질 루프
