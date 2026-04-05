# QueuePhase 상태 머신

> 큐 아이템의 전체 생명주기를 정의한다.
> 상위 설계는 [DESIGN-v6](../DESIGN-v6.md) 참조.

---

## Phase 정의

| Phase | 설명 |
|-------|------|
| **Pending** | DataSource.collect()가 감지, 큐 대기 |
| **Ready** | 실행 준비 완료 (자동 전이) |
| **Running** | worktree 생성 + handler 실행 중 |
| **Completed** | handler 전부 성공, evaluate 대기 |
| **Done** | evaluate 완료 판정 + on_done script 성공 |
| **HITL** | evaluate가 사람 판단 필요로 분류 |
| **Skipped** | escalation skip 또는 preflight 실패 |
| **Failed** | on_done script 실패, 인프라 오류 등 |

---

## Phase 전이 캡슐화 (v6 #718)

`QueueItem.phase` 필드를 직접 대입하면 `can_transition_to()` 검증을 우회할 수 있다.
v6에서는 모든 전이를 `QueueItem::transit()` 메서드로 강제한다.

```rust
impl QueueItem {
    /// phase 필드는 pub(crate) — belt-core 외부에서 직접 대입 불가
    /// 읽기는 pub getter: fn phase(&self) -> QueuePhase

    pub fn transit(&mut self, to: QueuePhase) -> Result<QueuePhase, BeltError> {
        let from = self.phase;
        if !from.can_transition_to(&to) {
            return Err(BeltError::InvalidTransition { from, to });
        }
        self.phase = to;
        self.updated_at = Utc::now().to_rfc3339();
        Ok(from)
    }
}
```

### 테스트 지원

테스트에서 특정 phase의 아이템을 생성하려면 빌더를 사용한다:

```rust
// 테스트 전용 빌더 (cfg(test) 또는 #[doc(hidden)])
QueueItem::builder()
    .work_id("test:1:analyze")
    .with_phase(QueuePhase::Running)  // 검증 없이 직접 설정
    .build()
```

### DB 로드

`belt-infra/db.rs`의 `from_row()`는 `pub(crate)` 접근 가능하므로 DB에서 로드 시 phase 직접 설정이 가능하다.

---

## 전체 상태 전이

```
                            DataSource.collect()
                                    │
                                    ▼
                    ┌───────────────────────────────┐
                    │           Pending              │
                    │   (큐 대기, 수집됨)              │
                    └───────────────┬───────────────┘
                                    │ 자동 전이
                                    ▼
                    ┌───────────────────────────────┐
                    │            Ready               │
                    │   (실행 준비 완료)               │
                    └───────────────┬───────────────┘
                                    │ 자동 전이 (concurrency 제한)
                                    ▼
                    ┌───────────────────────────────┐
                    │           Running              │
                    │                                │
                    │  ① worktree 생성 (or 재사용)    │
                    │  ② on_enter script             │
                    │  ③ handlers 순차 실행           │
                    │     prompt → LLM (worktree)    │
                    │     script → bash              │
                    └──────┬────────────┬───────────┘
                           │            │
                    전부 성공        handler/on_enter 실패
                           │            │
                           ▼            ▼
          ┌─────────────────┐    ┌─────────────────────────────┐
          │    Completed     │    │  Stagnation Analyzer (항상 실행)│
          │                  │    │                               │
          │  handler 완료    │    │  ① CompositeSimilarity로     │
          │  evaluate 대기   │    │    outputs/errors 유사도 분석 │
          │                  │    │  ② 패턴 감지 시              │
          │  force_trigger   │    │    LateralAnalyzer가         │
          │  ("evaluate")    │    │    내장 페르소나로 대안 분석   │
          └────────┬────────┘    │    → lateral_plan 생성        │
                   │              │                               │
                   │              │  Escalation (failure_count):  │
                   │              │  1: retry                    │
                   │              │     → lateral_plan 주입       │
                   │              │     → 새 아이템 → Pending     │
                   │              │     → worktree 보존          │
                   │              │     → on_fail 실행 안 함      │
                   │              │                               │
                   │              │  2: retry_with_comment        │
                   │              │     → lateral_plan 주입       │
                   │              │     → on_fail script 실행     │
                   │              │     → 새 아이템 → Pending     │
                   │              │     → worktree 보존          │
                   │              │                               │
                   │              │  3: hitl                      │
                   │              │     → lateral_report 첨부     │
                   │              │     → on_fail script 실행     │
                   │              │     → HITL 이벤트 생성 ───────┐│
                   │              │     → worktree 보존          ││
                   │              │                               ││
                   │              │  terminal: skip 또는 replan   ││
                   │              │     (hitl timeout 시 적용)     ││
                   │              │     skip   → Skipped ─────────┼┼──┐
                   │              │     replan → HITL(replan) ────┤│  │
                   │              └───────────────────────────────┘│  │
                   │                                               │  │
                   │  evaluate cron (per-item)                     │  │
                   │  (LLM이 belt queue done/hitl CLI 호출)     │  │
                   │                                               │  │
              ┌────┴────┐                                          │  │
              │         │                                          │  │
          완료 판정   사람 필요                                      │  │
              │         │                                          │  │
              ▼         ▼                                          │  │
    ┌──────────┐    ┌──────────────────────────────────────┐       │  │
    │ on_done  │    │                HITL                   │◄──────┘  │
    │ script   │    │                                      │          │
    │ 실행     │    │  사람 대기 (worktree 보존)             │          │
    └──┬───┬──┘    │                                      │          │
       │   │       │  응답 경로:                            │          │
    성공  실패     │    "done"  → on_done → Done           │          │
       │   │       │    "retry" → 새 아이템 → Pending      │          │
       ▼   ▼       │    "skip"  → Skipped                 │          │
  ┌──────┐┌─────┐  │    "replan"→ 스펙 수정 제안           │          │
  │ Done ││ Fail│  └──────────────────────────────────────┘          │
  │      ││ ed  │                                                    │
  │ wt   ││     │  ┌──────────────────────────────────────┐          │
  │ 정리  ││ wt  │  │              Skipped                 │◄─────────┘
  │      ││ 보존 │  │                                      │
  └──────┘│ 로그 │  │  terminal (worktree 정리)             │
          │ 기록 │  └──────────────────────────────────────┘
          └─────┘
```

---

## Worktree 생명주기

| Phase / 이벤트 | Worktree |
|----------------|----------|
| Running | 생성 (또는 retry 시 기존 보존분 재사용) |
| Completed | 유지 (evaluate 대기) |
| Done | **정리** |
| HITL | 보존 (사람 확인 후 결정) |
| Failed | 보존 (디버깅용) |
| Skipped | 정리 |
| Retry | 보존 (이전 작업 위에서 재시도) |
| Graceful shutdown 롤백 (Running→Pending) | **보존** (재시작 후 재사용) |
| hitl-timeout (HITL 만료) | **정리** |
| log-cleanup cron | 보존된 worktree 중 TTL 초과분 정리 |

**정리 원칙**: worktree는 **Done 또는 Skipped**가 되어야만 정리한다. HITL 만료 시에도 정리하여 좀비 worktree를 방지한다. Shutdown 롤백 시에는 재시작 후 재사용을 위해 보존한다. 나머지 보존분(Failed 등)은 `log-cleanup` cron이 TTL(기본 7일) 기준으로 주기 정리한다.

---

## on_fail 실행 조건

| Escalation | on_fail 실행 | 동작 |
|------------|-------------|------|
| retry | 안 함 | 조용한 재시도 |
| retry_with_comment | 실행 | 외부 알림 + 재시도 |
| hitl | 실행 | 외부 알림 + 사람 대기 |

`retry`만 on_fail을 실행하지 않는다. "조용한 재시도"로 외부 시스템에 노이즈를 주지 않는다.

> `skip`과 `replan`은 hitl의 응답 경로 또는 hitl timeout 시 `terminal` 설정에 의해 적용된다. 독립적인 escalation level이 아니다. 상세는 [DataSource](./datasource.md)의 Escalation 정책 참조.

failure_count는 append-only history에서 계산한다: `history | filter(state, failed) | count`. on_enter 실패도 handler 실패와 동일하게 failure_count에 포함된다.

> **v6 (#723)**: 모든 실패에서 StagnationDetector가 CompositeSimilarity로 유사도 분석을 수행한다. 패턴이 감지되면 LateralAnalyzer가 내장 페르소나(HACKER, ARCHITECT 등)로 대안 접근법을 분석하고, lateral_plan을 생성하여 retry 시 handler prompt에 주입한다. escalation 자체는 기존 failure_count 기반 그대로이되, **모든 retry가 lateral plan으로 강화**된다. 상세: [Stagnation Detection](./stagnation.md)

---

## Evaluate 원칙

### 판단 원칙

1. **의심스러우면 HITL** (safe default) — evaluate가 확신할 수 없으면 Done이 아니라 HITL로 분류한다. 잘못된 Done보다 불필요한 HITL이 낫다.

2. **"충분한가?"만 판단** — "이 handler의 결과물이 다음 단계로 넘어가기에 충분한가?"만 본다. 품질 판단(좋은 코드인가?)은 Cron 품질 루프가 담당한다.

3. **state별 구체 기준은 agent-workspace rules에 위임** — `~/.belt/agent-workspace/.claude/rules/classify-policy.md`에 state별 Done 조건을 정의한다. 코어는 rules를 모르고, `belt agent`가 rules를 참조하여 판단한다.

### Per-Item 판정 (v6 #722)

evaluate는 **per-work_id 단위**로 LLM 판정을 실행한다. 각 Completed 아이템에 대해 개별 프롬프트를 발행하고, 해당 아이템의 context를 포함한다.

```
for item in queue.get(Completed):
    belt_agent_p(workspace,
        "아이템 {work_id}의 완료 여부를 판단해줘.
         belt context {work_id} --json 으로 컨텍스트를 확인하고,
         belt queue done {work_id} 또는 belt queue hitl {work_id} 를 실행해줘")
```

- 개별 판정 실패 시 해당 아이템만 Completed에 머물고, 다른 아이템 판정에 영향 없다
- `batch_size`로 한 tick에서 처리할 최대 아이템 수를 제한한다
- 기존 `eval_failure_counts`는 이미 per-work_id로 관리됨 (설계 의도 일치)

### 실패 원칙

Completed는 **안전한 대기 상태**. evaluate가 실패하든 CLI가 실패하든 Completed에서 멈추고, 다음 기회에 재시도한다.

| 실패 유형 | 동작 | 상태 |
|-----------|------|------|
| evaluate LLM 오류/timeout | Completed 유지, 다음 cron tick에서 재시도 | Completed |
| evaluate 반복 실패 (N회) | HITL로 에스컬레이션 | → HITL |
| CLI 호출 실패 (`belt queue done/hitl`) | Completed 유지 + 에러 로그, 다음 tick 재시도 | Completed |
| on_done script 실패 | Failed 상태 (on_fail은 실행하지 않음 — handler 실패가 아니므로) | → Failed |

---

## 수용 기준

### Phase 전이 캡슐화 (#718)

- [ ] `QueueItem.phase` 필드는 `pub(crate)` 가시성으로, belt-core 외부에서 직접 대입 불가
- [ ] 모든 phase 변경은 `QueueItem::transit(to)` 메서드를 경유한다
- [ ] `transit()` 메서드는 내부에서 `can_transition_to()` 검증 + `updated_at` 갱신을 수행한다
- [ ] 테스트 코드에서도 phase 직접 대입 대신 `transit()` 또는 테스트 헬퍼를 사용한다

### 상태 전이 규칙

- [ ] Pending→Ready 전이는 Daemon tick마다 자동 수행된다
- [ ] Ready→Running 전이는 workspace.concurrency와 daemon.max_concurrent 모두 만족할 때만 수행된다
- [ ] queue_dependencies에 미완료(Done이 아닌) 의존이 있으면 Ready→Running 전이가 블로킹된다
- [ ] `can_transition_to()`가 허용하지 않는 전이를 시도하면 `InvalidTransition` 에러가 반환된다
- [ ] Done, Skipped는 terminal — 이후 전이 불가

### Escalation 정책

- [ ] failure_count=1일 때 `retry`가 적용되면 on_fail을 실행하지 않고 새 아이템으로 재시도한다
- [ ] failure_count=2일 때 `retry_with_comment`가 적용되면 on_fail 실행 후 새 아이템으로 재시도한다
- [ ] failure_count=3일 때 `hitl`이 적용되면 on_fail 실행 후 HITL 이벤트가 생성된다
- [ ] on_enter 실패도 failure_count에 포함된다
- [ ] 모든 실패에서 stagnation 분석이 실행되고, 패턴 감지 시 lateral_plan이 retry에 주입된다

### Evaluate (per-item, #722)

- [ ] evaluate는 per-work_id 단위로 LLM 판정을 실행한다
- [ ] 각 판정에 해당 아이템의 context가 포함된다
- [ ] 개별 판정 실패 시 해당 아이템만 Completed에 머물고, 다른 아이템에 영향 없다
- [ ] evaluate LLM 오류 시 아이템은 Completed에 머무르고, 다음 cron tick에서 재시도된다
- [ ] evaluate 반복 실패(N회)로 HITL 에스컬레이션 시 HitlReason::EvaluateFailure가 기록된다
- [ ] on_done script 실패 시 Failed 전이되고, on_fail은 실행하지 않는다

### Worktree 생명주기

- [ ] Running 진입 시 worktree가 생성된다 (retry 시 기존 worktree 재사용)
- [ ] Done, Skipped 전이 시 worktree가 정리된다
- [ ] HITL, Failed 전이 시 worktree가 보존된다
- [ ] log-cleanup cron이 TTL(7일) 초과 보존 worktree를 정리한다

---

### 관련 문서

- [DESIGN-v6](../DESIGN-v6.md) — 설계 철학
- [Daemon](./daemon.md) — 내부 모듈 구조 + 실행 루프
- [Stagnation Detection](./stagnation.md) — 반복 패턴 감지 + escalation 가속
- [DataSource](./datasource.md) — escalation 정책 + on_fail script
- [Cron 엔진](./cron-engine.md) — evaluate cron + force_trigger
- [실패 복구와 HITL](../flows/04-failure-and-hitl.md) — 실패/HITL 시나리오
- [Data Model](./data-model.md) — 테이블 스키마, 도메인 enum
