# QueuePhase 상태 머신

> 큐 아이템의 전체 생명주기를 정의한다.
> 상위 설계는 [DESIGN-v5](../DESIGN-v5.md) 참조.

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
          │    Completed     │    │     Escalation 정책 적용      │
          │                  │    │     (history 기반 count)      │
          │  handler 완료    │    │                               │
          │  evaluate 대기   │    │  1: retry                    │
          │                  │    │     → 새 아이템 → Pending     │
          │  force_trigger   │    │     → worktree 보존          │
          │  ("evaluate")    │    │     → on_fail 실행 안 함      │
          └────────┬────────┘    │                               │
                   │              │  2: retry_with_comment        │
                   │              │     → on_fail script 실행     │
                   │              │     → 새 아이템 → Pending     │
                   │              │     → worktree 보존          │
                   │              │                               │
                   │              │  3: hitl                      │
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
                   │  evaluate cron                                │  │
                   │  (LLM이 autodev queue done/hitl CLI 호출)     │  │
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

---

## Evaluate 원칙

### 판단 원칙

1. **의심스러우면 HITL** (safe default) — evaluate가 확신할 수 없으면 Done이 아니라 HITL로 분류한다. 잘못된 Done보다 불필요한 HITL이 낫다.

2. **"충분한가?"만 판단** — "이 handler의 결과물이 다음 단계로 넘어가기에 충분한가?"만 본다. 품질 판단(좋은 코드인가?)은 Cron 품질 루프가 담당한다.

3. **state별 구체 기준은 claw-workspace rules에 위임** — `~/.autodev/claw-workspace/.claude/rules/classify-policy.md`에 state별 Done 조건을 정의한다 (Claw 워크스페이스의 rules 파일, [Claw 워크스페이스](./claw-workspace.md) 참조). 코어는 rules를 모르고, `autodev agent`가 rules를 참조하여 판단한다.

### 실패 원칙

Completed는 **안전한 대기 상태**. evaluate가 실패하든 CLI가 실패하든 Completed에서 멈추고, 다음 기회에 재시도한다.

| 실패 유형 | 동작 | 상태 |
|-----------|------|------|
| evaluate LLM 오류/timeout | Completed 유지, 다음 cron tick에서 재시도 | Completed |
| evaluate 반복 실패 (N회) | HITL로 에스컬레이션 | → HITL |
| CLI 호출 실패 (`autodev queue done/hitl`) | Completed 유지 + 에러 로그, 다음 tick 재시도 | Completed |
| on_done script 실패 | Failed 상태 (on_fail은 실행하지 않음 — handler 실패가 아니므로) | → Failed |

---

### 관련 문서

- [DESIGN-v5](../DESIGN-v5.md) — 설계 철학
- [DataSource](./datasource.md) — escalation 정책 + on_fail script
- [Cron 엔진](./cron-engine.md) — evaluate cron + force_trigger
- [실패 복구와 HITL](../flows/04-failure-and-hitl.md) — 실패/HITL 시나리오
