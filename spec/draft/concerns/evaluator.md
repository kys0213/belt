# Evaluator — Progressive Evaluation Pipeline

> 실행 전에 판정하고, 필요할 때만 실행한다.
> 비용이 낮은 검증부터 단계적으로 수행하여 불필요한 LLM 호출을 줄인다.
> Ouroboros의 3-stage progressive evaluation을 차용.

---

## 핵심 원칙

```
1. Evaluate before Execute — 실행보다 판정이 먼저
2. Cheapest first — 비용 0 검증 → LLM 1회 → (다중 LLM)
3. History-aware — 이전 기록으로 판정 가능하면 handler 실행 생략
```

---

## Daemon tick에서의 위치

```
loop {
    collect()                    // 수집
    evaluator.evaluate()         // 판정 — 실행보다 먼저
    advancer.advance()           // 전이
    executor.execute()           // 실행
    cron_engine.tick()           // 품질 루프
}
```

Evaluator는 두 가지를 처리한다:

1. **Completed 아이템 판정** — handler가 끝난 아이템을 Done/HITL로 분류
2. **Ready 아이템 사전 검증** — 이전 기록 기반으로 handler 실행 없이 판정 가능한지 확인

```
evaluator.evaluate():
    // 1. Completed 아이템 → 단계적 판정
    for item in queue.get(Completed):
        decision = pipeline.evaluate(item)
        match decision:
            Done → hook.on_done(), transit(Done)
            Hitl → create_hitl_event()
            NeedMoreWork → transit(Ready)  // 재실행 필요

    // 2. Ready 아이템 → 사전 검증 (이력 기반)
    for item in queue.get(Ready):
        if can_judge_from_history(item):
            // handler 실행 없이 판정 (비용 0)
            decision = pipeline.evaluate_from_history(item)
            match decision:
                Done → hook.on_done(), transit(Done)
                Hitl → create_hitl_event()
                Inconclusive → pass  // Advancer가 Running으로 전이
```

---

## Progressive Evaluation Pipeline

비용이 낮은 단계부터 순차 실행. 앞 단계에서 판정되면 뒤 단계를 건너뛴다.

```
┌─ Stage 1: Mechanical (비용 $0) ─────────────────────────────┐
│                                                              │
│  cargo test, cargo clippy, lint 등 결정적 검증               │
│  → 실패 시 즉시 retry (LLM 안 부름)                          │
│  → 성공 시 Stage 2로                                         │
└──────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─ Stage 2: Semantic (LLM 1회) ────────────────────────────────┐
│                                                              │
│  LLM이 "이 결과가 충분한가?" 판정                             │
│  classify-policy.md 기준으로 Done / HITL 분류                │
│  → 확실하면 판정 완료                                        │
│  → 불확실하면 Stage 3로 (Phase 2)                            │
└──────────────────────────────────────────────────────────────┘
                          │
                          ▼ (Phase 2)
┌─ Stage 3: Consensus (다중 LLM) ─────────────────────────────┐
│                                                              │
│  트리거 조건 충족 시에만 실행                                  │
│  다중 모델 투표 또는 역할 기반 토론                            │
│  → 최종 판정                                                 │
└──────────────────────────────────────────────────────────────┘
```

---

## trait 정의

```rust
/// 평가 단계 하나를 추상화.
/// 각 impl이 자기 방식으로 판정하고, 다음 단계로 넘길지 결정.
#[async_trait]
pub trait EvaluationStage: Send + Sync {
    fn name(&self) -> &str;

    /// 판정 수행. 확정이면 Done/Hitl, 불확실하면 Inconclusive.
    async fn evaluate(&self, ctx: &EvalContext, db: &dyn Db) -> Result<EvalDecision>;
}

pub enum EvalDecision {
    Done,                     // 충분 — hook.on_done() 트리거
    Hitl { reason: String },  // 사람 필요 — HITL 이벤트 생성
    Retry,                    // Stage 1 실패 — handler 재실행
    Inconclusive,             // 이 단계에서 판정 불가 — 다음 단계로
}
```

---

## EvaluationPipeline — Stage 컴포지트

```rust
pub struct EvaluationPipeline {
    stages: Vec<Box<dyn EvaluationStage>>,
}

impl EvaluationPipeline {
    /// 등록된 Stage를 순차 실행. 확정 판정이 나오면 중단.
    pub async fn evaluate(&self, ctx: &EvalContext, db: &dyn Db) -> Result<EvalDecision> {
        for stage in &self.stages {
            match stage.evaluate(ctx, db).await? {
                EvalDecision::Inconclusive => continue,  // 다음 Stage로
                decision => return Ok(decision),         // 확정 — 중단
            }
        }
        // 모든 Stage가 Inconclusive → HITL로 에스컬레이션
        Ok(EvalDecision::Hitl { reason: "all stages inconclusive".into() })
    }
}
```

v6에서는 `MechanicalStage` + `SemanticStage`만 등록. Phase 2에서 `ConsensusStage`를 추가하면 코어 변경 0.

---

## Stage 상세

### Stage 1: MechanicalStage (v6)

worktree에서 결정적 검증을 실행한다. LLM 비용 0.

```rust
struct MechanicalStage {
    commands: Vec<String>,  // workspace yaml에서 로드
}

impl EvaluationStage for MechanicalStage {
    async fn evaluate(&self, ctx: &EvalContext, db: &dyn Db) -> Result<EvalDecision> {
        for cmd in &self.commands {
            let result = execute_in_worktree(cmd, &ctx.worktree);
            if result.failed {
                return Ok(EvalDecision::Retry);  // 빌드/테스트 실패 → 재시도
            }
        }
        Ok(EvalDecision::Inconclusive)  // 기계적으로는 통과 → Semantic으로
    }
}
```

workspace yaml에서 검증 커맨드를 정의:

```yaml
evaluate:
  mechanical:
    - "cargo test"
    - "cargo clippy -- -D warnings"
```

### Stage 2: SemanticStage (v6)

LLM이 classify-policy.md 기준으로 Done/HITL 판정. 현재 evaluate 로직과 동일.

```rust
struct SemanticStage {
    runtime: Box<dyn AgentRuntime>,
}

impl EvaluationStage for SemanticStage {
    async fn evaluate(&self, ctx: &EvalContext, db: &dyn Db) -> Result<EvalDecision> {
        // belt agent -p 호출 (기존과 동일)
        // LLM이 belt queue done/hitl CLI를 호출하여 판정
    }
}
```

### Stage 3: ConsensusStage (Phase 2)

다중 LLM 투표. 트리거 조건 충족 시에만 실행.

트리거 조건 (Ouroboros 차용):
- drift score가 임계값 초과
- lateral thinking이 적용된 retry
- uncertainty가 높은 경우

---

## History-aware 사전 검증

Ready 아이템에 대해 이전 기록을 조회하여 handler 실행 없이 판정 가능한지 확인한다.

```
can_judge_from_history(item):
    // 같은 source_id로 이전에 동일한 state를 성공한 기록이 있는가?
    prev = db.query("SELECT * FROM history
        WHERE source_id=? AND state=? AND status='done'
        ORDER BY created_at DESC LIMIT 1", item.source_id, item.state)

    if prev.is_some():
        // 이전 성공 결과와 현재 worktree 상태를 비교
        similarity = judge.score(prev.summary, current_state)
        if similarity >= threshold:
            return true  // 이전 결과로 판정 가능

    return false  // handler 실행 필요
```

---

## Evaluator 모듈 — Daemon 내부

```
Daemon (CPU)
  ├── Evaluator              ← tick 루프에서 실행보다 먼저
  │     └── EvaluationPipeline
  │           ├── MechanicalStage (v6)
  │           ├── SemanticStage (v6)
  │           └── ConsensusStage (Phase 2)
  │
  ├── Advancer
  ├── Executor
  ├── HitlService
  └── CronEngine
```

Evaluator는 cron job이 아닌 **Daemon tick 루프의 정규 단계**이다. 실행(Executor)보다 먼저 동작하여, 판정 가능한 아이템은 handler 실행 없이 처리한다.

---

## 영향 범위

| 변경 | 내용 |
|------|------|
| Daemon tick 순서 | evaluate → advance → execute → cron |
| Evaluator 위치 | cron job → Daemon 모듈 |
| CronEngine | evaluate 제거, 품질 루프(gap-detection 등)만 담당 |
| workspace yaml | `evaluate.mechanical` 섹션 추가 (검증 커맨드) |

---

## 수용 기준

- [ ] Evaluator가 Daemon tick에서 Executor보다 먼저 실행된다
- [ ] EvaluationPipeline이 Stage를 비용 순으로 순차 실행한다
- [ ] MechanicalStage가 worktree에서 결정적 검증을 수행한다 (비용 0)
- [ ] SemanticStage가 LLM으로 Done/HITL 판정한다
- [ ] Stage가 확정 판정을 내리면 후속 Stage를 건너뛴다
- [ ] Ready 아이템에 대해 이전 기록 기반 사전 검증이 동작한다
- [ ] 새 EvaluationStage impl 추가 시 코어 변경 없다 (OCP)

---

### 관련 문서

- [DESIGN-v6](../DESIGN-v6.md) — Daemon tick 순서
- [Daemon](./daemon.md) — 실행 루프
- [Agent Workspace](./agent-workspace.md) — classify-policy.md (SemanticStage 기준)
- [Stagnation Detection](./stagnation.md) — PatternDetector (유사도 판단 재사용)
- [LifecycleHook](./lifecycle-hook.md) — hook.on_done() 트리거
