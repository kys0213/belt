//! E2E integration test: Daemon lifecycle
//!
//! Tests the full Daemon flow: create -> collect -> advance -> execute -> complete
//! using Mock DataSource and Mock AgentRuntime.

use std::sync::Arc;

use belt_core::escalation::EscalationAction;
use belt_core::phase::QueuePhase;
use belt_core::queue::testing::test_item;
use belt_core::runtime::{RuntimeRegistry, TokenUsage};
use belt_core::workspace::WorkspaceConfig;
use belt_daemon::daemon::{Daemon, ItemOutcome};
use belt_infra::db::Database;
use belt_infra::runtimes::mock::MockRuntime;
use belt_infra::sources::mock::MockDataSource;
use belt_infra::worktree::MockWorktreeManager;
use tempfile::TempDir;

fn test_workspace_config() -> WorkspaceConfig {
    let yaml = r#"
name: test-ws
concurrency: 2
sources:
  github:
    url: https://github.com/org/repo
    states:
      analyze:
        trigger:
          label: "belt:analyze"
        handlers:
          - prompt: "analyze this issue"
        on_done:
          - script: "echo done"
      implement:
        trigger:
          label: "belt:implement"
        handlers:
          - prompt: "implement this"
          - script: "echo test"
        on_done:
          - script: "echo created PR"
        on_fail:
          - script: "echo failed"
    escalation:
      1: retry
      2: retry_with_comment
      3: hitl
      terminal: skip
"#;
    serde_yaml::from_str(yaml).unwrap()
}

fn setup_daemon(tmp: &TempDir, source: MockDataSource, exit_codes: Vec<i32>) -> Daemon {
    let config = test_workspace_config();
    let mut registry = RuntimeRegistry::new("mock".to_string());
    registry.register(Arc::new(MockRuntime::new("mock", exit_codes)));
    let worktree_mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

    Daemon::new(
        config,
        vec![Box::new(source)],
        Arc::new(registry),
        Box::new(worktree_mgr),
        4,
        Database::open_in_memory().unwrap(),
    )
}

/// Full lifecycle: collect -> advance -> execute -> complete.
/// A single item flows from Pending to Completed after successful handler execution.
#[tokio::test]
async fn full_lifecycle_single_item() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    let mut daemon = setup_daemon(&tmp, source, vec![0]);

    // Phase 1: Collect
    let collected = daemon.collect().await.unwrap();
    assert_eq!(collected, 1);
    assert_eq!(daemon.queue_items().len(), 1);
    assert_eq!(
        daemon.items_in_phase(QueuePhase::Pending).len(),
        1,
        "item should start in Pending"
    );

    // Phase 2: Advance (Pending -> Ready -> Running)
    let advanced = daemon.advance();
    assert!(advanced >= 1, "at least one item should advance");
    assert_eq!(
        daemon.items_in_phase(QueuePhase::Running).len(),
        1,
        "item should be in Running after advance"
    );

    // Phase 3: Execute (Running -> Completed)
    let outcomes = daemon.execute_running().await;
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(outcomes[0], ItemOutcome::Completed(_)),
        "outcome should be Completed"
    );

    // The item should now be in the Completed phase in the queue.
    let completed = daemon.items_in_phase(QueuePhase::Completed);
    assert_eq!(completed.len(), 1, "item should be Completed in queue");
}

/// Full lifecycle with multiple items respecting concurrency.
#[tokio::test]
async fn full_lifecycle_multiple_items_respects_concurrency() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));
    source.add_item(test_item("github:org/repo#2", "analyze"));
    source.add_item(test_item("github:org/repo#3", "analyze"));

    // Workspace concurrency is 2, so only 2 items should run simultaneously.
    let mut daemon = setup_daemon(&tmp, source, vec![0, 0, 0]);

    daemon.collect().await.unwrap();
    daemon.advance();

    let running = daemon.items_in_phase(QueuePhase::Running).len();
    let ready = daemon.items_in_phase(QueuePhase::Ready).len();
    assert_eq!(running, 2, "only 2 items should be Running (concurrency=2)");
    assert_eq!(ready, 1, "1 item should remain Ready");

    // Execute the 2 running items.
    let outcomes = daemon.execute_running().await;
    assert_eq!(outcomes.len(), 2);

    // Advance again to pick up the remaining item.
    daemon.advance();
    let running = daemon.items_in_phase(QueuePhase::Running).len();
    assert_eq!(running, 1, "remaining item should now be Running");

    let outcomes = daemon.execute_running().await;
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(outcomes[0], ItemOutcome::Completed(_)));
}

/// Full tick cycle: collect + advance + execute in one call.
///
/// After tick(), items go through collect -> advance -> execute -> evaluate.
/// The evaluate step may remove Completed items from the queue (on success
/// they transition to Done after on_done and are removed, or on failure
/// they remain in Completed for retry). Either way, the queue state reflects
/// the completed lifecycle.
#[tokio::test]
async fn tick_runs_full_cycle() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    let mut daemon = setup_daemon(&tmp, source, vec![0]);

    // Before tick: no items.
    assert_eq!(daemon.queue_items().len(), 0);

    daemon.tick().await.unwrap();

    // After a full tick, the item was collected, advanced, executed, and evaluated.
    // The evaluator may have completed the item (removing it from queue) or
    // the item stays in Completed (eval failure retry). Either is valid.
    // We verify the item is no longer in Pending or Running.
    let pending = daemon.items_in_phase(QueuePhase::Pending).len();
    let running = daemon.items_in_phase(QueuePhase::Running).len();
    assert_eq!(pending, 0, "no items should be Pending after tick");
    assert_eq!(running, 0, "no items should be Running after tick");
}

/// Handler failure produces a Failed outcome with escalation action.
#[tokio::test]
async fn handler_failure_triggers_escalation() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    // MockRuntime returns exit_code 1 -> failure
    let mut daemon = setup_daemon(&tmp, source, vec![1]);

    daemon.collect().await.unwrap();
    daemon.advance();
    let outcomes = daemon.execute_running().await;

    assert_eq!(outcomes.len(), 1);
    match &outcomes[0] {
        ItemOutcome::Failed { escalation, .. } => {
            // First failure -> EscalationAction::Retry per the escalation policy.
            assert_eq!(*escalation, EscalationAction::Retry);
        }
        other => panic!("expected Failed outcome, got {other:?}"),
    }

    // Retry escalation should create a new Pending item.
    let pending = daemon.items_in_phase(QueuePhase::Pending);
    assert_eq!(pending.len(), 1, "retry should enqueue a new Pending item");
}

/// Item with an unknown state (no StateConfig) is Skipped during execution.
#[tokio::test]
async fn unknown_state_skips_item() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "nonexistent_state"));

    let mut daemon = setup_daemon(&tmp, source, vec![]);

    daemon.collect().await.unwrap();
    daemon.advance();
    let outcomes = daemon.execute_running().await;

    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(outcomes[0], ItemOutcome::Skipped(_)),
        "item with unknown state should be Skipped"
    );
}

/// Collect deduplicates through the store, not by the source's work_id.
#[tokio::test]
async fn collect_deduplicates_by_work_id() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));
    source.add_item(test_item("github:org/repo#1", "analyze")); // duplicate

    let mut daemon = setup_daemon(&tmp, source, vec![]);

    let collected = daemon.collect().await.unwrap();
    // The source offers 2 items; the store accepts only the first.
    assert_eq!(collected, 1, "only the inserted item is counted");
    assert_eq!(
        daemon.queue_items().len(),
        1,
        "queue should deduplicate by work_id"
    );
}

/// Shutdown flag prevents collect and advance during tick.
#[tokio::test]
async fn shutdown_prevents_collect_and_advance() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    let mut daemon = setup_daemon(&tmp, source, vec![]);
    daemon.request_shutdown();

    daemon.tick().await.unwrap();

    assert!(daemon.is_shutdown_requested());
    assert_eq!(
        daemon.queue_items().len(),
        0,
        "no items should be collected after shutdown"
    );
}

/// Shutdown rollback followed by re-advance reuses the preserved worktree.
///
/// Simulates: collect -> advance -> Running -> shutdown rollback -> Pending
/// -> re-advance -> Running -> execute. The preserved worktree should be
/// reused (validated) instead of creating a fresh one.
#[tokio::test]
async fn shutdown_rollback_reuses_preserved_worktree() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    let mut daemon = setup_daemon(&tmp, source, vec![0]); // exit code 0 = success

    // Phase 1: collect -> advance to Running.
    daemon.collect().await.unwrap();
    daemon.advance();
    assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 1);

    // Create the workspace worktree directory so rollback can register it.
    let ws_path = daemon.worktree_mgr().path("test-ws");
    std::fs::create_dir_all(&ws_path).unwrap();
    // Write a marker file to verify the same directory is reused.
    std::fs::write(ws_path.join("marker.txt"), "preserved-content").unwrap();

    // Phase 2: Simulate graceful shutdown rollback.
    daemon.rollback_running_to_pending();

    assert_eq!(daemon.items_in_phase(QueuePhase::Pending).len(), 1);
    assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 0);

    // Verify worktree was registered for the source_id.
    let preserved = daemon.worktree_mgr().lookup_preserved("github:org/repo#1");
    assert!(
        preserved.is_some(),
        "preserved worktree should be registered after rollback"
    );

    // Verify the item has worktree_preserved flag set.
    let item = daemon.get_item("github:org/repo#1:analyze").unwrap();
    assert!(item.worktree_preserved);

    // Phase 3: Re-advance to Running and execute.
    daemon.advance();
    assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 1);

    let outcomes = daemon.execute_running().await;
    assert_eq!(outcomes.len(), 1);

    // The preserved worktree mapping should have been cleared after handoff.
    let preserved_after = daemon.worktree_mgr().lookup_preserved("github:org/repo#1");
    assert!(
        preserved_after.is_none(),
        "preserved worktree mapping should be cleared after execution"
    );
}

/// Multiple ticks process items through the full lifecycle.
#[tokio::test]
async fn multiple_ticks_process_items() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));
    source.add_item(test_item("github:org/repo#2", "analyze"));

    let mut daemon = setup_daemon(&tmp, source, vec![0, 0]);

    // First tick: collect + advance + execute + evaluate.
    daemon.tick().await.unwrap();

    // After tick, no items should be in Pending or Running.
    let pending = daemon.items_in_phase(QueuePhase::Pending).len();
    let running = daemon.items_in_phase(QueuePhase::Running).len();
    assert_eq!(pending, 0, "no items should be Pending after first tick");
    assert_eq!(running, 0, "no items should be Running after first tick");

    // Second tick should be a no-op (nothing to collect/execute).
    daemon.tick().await.unwrap();
    assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 0);
}

/// Completed item can be marked Done via mark_done.
#[tokio::test]
async fn complete_then_mark_done() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    let mut daemon = setup_daemon(&tmp, source, vec![0]);

    daemon.collect().await.unwrap();
    daemon.advance();
    daemon.execute_running().await;

    // Item is now Completed; manually mark it Done.
    let result = daemon.mark_done("github:org/repo#1:analyze");
    assert!(result.is_ok());
    assert_eq!(
        daemon
            .get_item("github:org/repo#1:analyze")
            .unwrap()
            .phase(),
        QueuePhase::Done
    );
}

/// Parallel execution of multiple Running items.
#[tokio::test]
async fn parallel_execution() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));
    source.add_item(test_item("github:org/repo#2", "analyze"));

    let mut daemon = setup_daemon(&tmp, source, vec![0, 0]);

    daemon.collect().await.unwrap();
    daemon.advance();

    let outcomes = daemon.execute_running().await;
    assert_eq!(outcomes.len(), 2);

    let completed_count = outcomes
        .iter()
        .filter(|o| matches!(o, ItemOutcome::Completed(_)))
        .count();
    assert_eq!(
        completed_count, 2,
        "both items should complete successfully"
    );
}

/// After execute_running, token_usage from RuntimeResponse should be
/// automatically persisted to the database.
#[tokio::test]
async fn execute_running_saves_token_usage_to_db() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    let config = test_workspace_config();
    let mock = MockRuntime::new("mock", vec![0]).with_token_usages(vec![TokenUsage {
        input_tokens: 500,
        output_tokens: 200,
        cache_read_tokens: Some(50),
        cache_write_tokens: None,
    }]);
    let mut registry = RuntimeRegistry::new("mock".to_string());
    registry.register(Arc::new(mock));
    let worktree_mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

    let db = Database::open_in_memory().unwrap();
    let mut daemon = Daemon::new(
        config,
        vec![Box::new(source)],
        Arc::new(registry),
        Box::new(worktree_mgr),
        4,
        db,
    );

    daemon.collect().await.unwrap();
    daemon.advance();
    let outcomes = daemon.execute_running().await;
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(outcomes[0], ItemOutcome::Completed(_)));

    // Verify token_usage was persisted in the database.
    let db = daemon.db();
    let rows = db
        .get_token_usage_by_work_id("github:org/repo#1:analyze")
        .unwrap();
    assert_eq!(rows.len(), 1, "one token_usage row should be recorded");
    assert_eq!(rows[0].input_tokens, 500);
    assert_eq!(rows[0].output_tokens, 200);
    assert_eq!(rows[0].cache_read_tokens, Some(50));
    assert!(rows[0].cache_write_tokens.is_none());
    assert_eq!(rows[0].runtime, "mock");
    assert_eq!(rows[0].workspace, "test-ws");
}

/// Token usage from a failed handler execution should also be saved.
#[tokio::test]
async fn execute_running_saves_token_usage_on_failure() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    let config = test_workspace_config();
    let mock = MockRuntime::new("mock", vec![1]).with_token_usages(vec![TokenUsage {
        input_tokens: 300,
        output_tokens: 100,
        cache_read_tokens: None,
        cache_write_tokens: None,
    }]);
    let mut registry = RuntimeRegistry::new("mock".to_string());
    registry.register(Arc::new(mock));
    let worktree_mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

    let db = Database::open_in_memory().unwrap();
    let mut daemon = Daemon::new(
        config,
        vec![Box::new(source)],
        Arc::new(registry),
        Box::new(worktree_mgr),
        4,
        db,
    );

    daemon.collect().await.unwrap();
    daemon.advance();
    let outcomes = daemon.execute_running().await;
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(outcomes[0], ItemOutcome::Failed { .. }));

    // Even on failure, token_usage should be recorded.
    let db = daemon.db();
    let rows = db
        .get_token_usage_by_work_id("github:org/repo#1:analyze")
        .unwrap();
    assert!(
        !rows.is_empty(),
        "token_usage should be recorded even on failure"
    );
    assert_eq!(rows[0].input_tokens, 300);
    assert_eq!(rows[0].output_tokens, 100);
}

/// 이름이 "evaluate"인 cron job을 심고(last_run_at=now, 1시간 간격 — 스스로는 발화하지 않음),
/// Daemon 진행 중에 그 job이 force_trigger 로 발화되는지 센다.
fn evaluate_named_probe() -> (
    belt_daemon::cron::CronEngine,
    Arc<std::sync::atomic::AtomicU32>,
) {
    use belt_daemon::cron::{CronContext, CronEngine, CronHandler, CronJobDef, CronSchedule};
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Probe(Arc<AtomicU32>);
    impl CronHandler for Probe {
        fn execute(&self, _ctx: &CronContext) -> Result<(), belt_core::error::BeltError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let count = Arc::new(AtomicU32::new(0));
    let mut engine = CronEngine::new();
    engine.register(CronJobDef {
        name: "evaluate".to_string(),
        schedule: CronSchedule::Interval(std::time::Duration::from_secs(3600)),
        workspace: None,
        enabled: true,
        last_run_at: Some(chrono::Utc::now()),
        handler: Box::new(Probe(Arc::clone(&count))),
    });
    (engine, count)
}

/// Completed 전이가 cron "evaluate" 를 force_trigger 하지 않는다 (execute_running 경로).
/// 평가는 tick 의 정규 단계로만 돈다.
#[tokio::test]
async fn execute_running_does_not_force_trigger_evaluate_cron() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));
    let (engine, count) = evaluate_named_probe();
    let mut daemon = setup_daemon(&tmp, source, vec![0]).with_cron_engine(engine);

    daemon.collect().await.unwrap();
    daemon.advance();
    daemon.execute_running().await;
    assert_eq!(daemon.items_in_phase(QueuePhase::Completed).len(), 1);

    daemon.tick().await.unwrap();

    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// 한 tick 안에서 Completed 가 되어도 cron "evaluate" 는 발화하지 않고,
/// Completed 아이템은 같은 tick 의 정규 평가 단계에서 처리된다.
#[tokio::test]
async fn tick_evaluates_completed_without_cron_trigger() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));
    let (engine, count) = evaluate_named_probe();
    // 핸들러 1회 + 평가 1회
    let mut daemon = setup_daemon(&tmp, source, vec![0, 0]).with_cron_engine(engine);

    daemon.tick().await.unwrap();

    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(daemon.items_in_phase(QueuePhase::Completed).len(), 0);
}

// ---------------------------------------------------------------------------
// DB-owned claim transitions (Pending -> Ready -> Running)
// ---------------------------------------------------------------------------

mod claim {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    use belt_core::lifecycle::{HookContext, LifecycleHook};
    use belt_core::transition::{Actor, TransitionOutcome, TransitionReason, TransitionRequest};

    use super::*;

    struct CountingHook {
        on_enter: AtomicU32,
    }

    #[async_trait::async_trait]
    impl LifecycleHook for CountingHook {
        async fn on_enter(&self, _ctx: &HookContext) -> anyhow::Result<()> {
            self.on_enter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn on_done(&self, _ctx: &HookContext) -> anyhow::Result<()> {
            Ok(())
        }
        async fn on_fail(&self, _ctx: &HookContext) -> anyhow::Result<()> {
            Ok(())
        }
        async fn on_escalation(
            &self,
            _ctx: &HookContext,
            _action: EscalationAction,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn daemon_with_db(tmp: &TempDir, source: MockDataSource, db: Database) -> Daemon {
        let config = test_workspace_config();
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(MockRuntime::new("mock", vec![0])));
        let worktree_mgr = MockWorktreeManager::new(tmp.path().to_path_buf());
        Daemon::new(
            config,
            vec![Box::new(source)],
            Arc::new(registry),
            Box::new(worktree_mgr),
            4,
            db,
        )
    }

    #[tokio::test]
    async fn advance_records_claim_in_transition_log() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));
        let mut daemon = setup_daemon(&tmp, source, vec![0]);

        daemon.collect().await.unwrap();
        daemon.advance();

        let work_id = daemon.queue_items()[0].work_id.clone();
        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 1);
        let log = daemon.db().transitions_of(&work_id).unwrap();
        let enters: Vec<_> = log
            .iter()
            .filter(|e| e.kind == "phase_enter")
            .map(|e| {
                (
                    e.from_phase.as_deref().unwrap(),
                    e.to_phase.as_deref().unwrap(),
                    e.actor.as_str(),
                )
            })
            .collect();
        assert_eq!(
            enters,
            vec![
                ("pending", "ready", "daemon"),
                ("ready", "running", "daemon")
            ]
        );
        let stored = daemon.db().get_item(&work_id).unwrap();
        assert_eq!(stored.phase(), QueuePhase::Running);
    }

    #[tokio::test]
    async fn claim_conflict_skips_handler_and_hooks() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("belt.db");
        let db_path = db_path.to_str().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));
        let hook = Arc::new(CountingHook {
            on_enter: AtomicU32::new(0),
        });
        let mut daemon = daemon_with_db(&tmp, source, Database::open(db_path).unwrap())
            .with_hook(Arc::clone(&hook) as Arc<dyn LifecycleHook>);

        daemon.collect().await.unwrap();
        daemon.advance_pending_to_ready();
        let work_id = daemon.queue_items()[0].work_id.clone();

        // Another process skips the item right before the claim.
        let other = Database::open(db_path).unwrap();
        let outcome = other
            .transition(&TransitionRequest {
                work_id: work_id.clone(),
                expected_from: QueuePhase::Ready,
                to: QueuePhase::Skipped,
                actor: Actor::Cli,
                reason: TransitionReason::Manual,
                detail: None,
            })
            .unwrap();
        assert!(matches!(outcome, TransitionOutcome::Applied { .. }));

        daemon.advance_ready_to_running(&HashMap::new(), 1);
        let outcomes = daemon.execute_running().await;

        assert!(outcomes.is_empty(), "no handler may run: {outcomes:?}");
        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 0);
        assert!(
            daemon.get_item(&work_id).is_none(),
            "a copy whose row is Skipped leaves the queue"
        );
        assert_eq!(hook.on_enter.load(Ordering::SeqCst), 0);
        let log = other.transitions_of(&work_id).unwrap();
        assert!(
            log.iter()
                .any(|e| e.kind == "transition_conflict" && e.actor == "daemon"),
            "daemon conflict must be logged: {log:?}"
        );
        assert_eq!(
            other.get_item(&work_id).unwrap().phase(),
            QueuePhase::Skipped
        );
    }
}

// ---------------------------------------------------------------------------
// DB-owned result transitions (Running -> Completed and conflicts)
// ---------------------------------------------------------------------------

mod results {
    use std::sync::atomic::{AtomicU32, Ordering};

    use belt_core::lifecycle::{HookContext, LifecycleHook};

    use super::*;

    /// Counts the reactions a discarded execution must not trigger.
    #[derive(Default)]
    struct ReactionCounter {
        on_done: AtomicU32,
        on_fail: AtomicU32,
        on_escalation: AtomicU32,
    }

    #[async_trait::async_trait]
    impl LifecycleHook for ReactionCounter {
        async fn on_enter(&self, _ctx: &HookContext) -> anyhow::Result<()> {
            Ok(())
        }
        async fn on_done(&self, _ctx: &HookContext) -> anyhow::Result<()> {
            self.on_done.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn on_fail(&self, _ctx: &HookContext) -> anyhow::Result<()> {
            self.on_fail.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn on_escalation(
            &self,
            _ctx: &HookContext,
            _action: EscalationAction,
        ) -> anyhow::Result<()> {
            self.on_escalation.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn daemon_with(
        tmp: &TempDir,
        source: MockDataSource,
        runtime: MockRuntime,
        hook: Arc<dyn LifecycleHook>,
    ) -> Daemon {
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(runtime));
        Daemon::new(
            test_workspace_config(),
            vec![Box::new(source)],
            Arc::new(registry),
            Box::new(MockWorktreeManager::new(tmp.path().to_path_buf())),
            4,
            Database::open_in_memory().unwrap(),
        )
        .with_hook(hook)
    }

    /// `(from, to, actor, reason)` of every phase entry, oldest first.
    fn phase_enters(daemon: &Daemon, work_id: &str) -> Vec<(String, String, String, String)> {
        daemon
            .db()
            .transitions_of(work_id)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "phase_enter")
            .map(|e| {
                (
                    e.from_phase.unwrap(),
                    e.to_phase.unwrap(),
                    e.actor,
                    e.reason.unwrap(),
                )
            })
            .collect()
    }

    fn enter(from: &str, to: &str, reason: &str) -> (String, String, String, String) {
        (
            from.to_string(),
            to.to_string(),
            "daemon".to_string(),
            reason.to_string(),
        )
    }

    fn one_item_source() -> MockDataSource {
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));
        source
    }

    #[tokio::test]
    async fn handler_success_is_committed_to_the_store() {
        let tmp = TempDir::new().unwrap();
        let hook = Arc::new(ReactionCounter::default());
        let mut daemon = daemon_with(
            &tmp,
            one_item_source(),
            MockRuntime::new("mock", vec![0]),
            hook,
        );

        daemon.collect().await.unwrap();
        daemon.advance();
        let outcomes = daemon.execute_running().await;

        assert!(matches!(outcomes[0], ItemOutcome::Completed(_)));
        let work_id = daemon.queue_items()[0].work_id.clone();
        assert_eq!(
            phase_enters(&daemon, &work_id),
            vec![
                enter("pending", "ready", "advance"),
                enter("ready", "running", "advance"),
                enter("running", "completed", "advance"),
            ],
            "every phase change leaves exactly one entry"
        );
        let stored = daemon.db().get_item(&work_id).unwrap();
        assert_eq!(stored.phase(), QueuePhase::Completed);
        assert_eq!(daemon.queue_items()[0].phase(), stored.phase());
    }

    #[tokio::test]
    async fn running_conflict_on_failure_discards_the_execution() {
        let tmp = TempDir::new().unwrap();
        let hook = Arc::new(ReactionCounter::default());
        let runtime = MockRuntime::new("mock", vec![1]).with_token_usages(vec![TokenUsage {
            input_tokens: 300,
            output_tokens: 100,
            cache_read_tokens: None,
            cache_write_tokens: None,
        }]);
        let mut daemon = daemon_with(
            &tmp,
            one_item_source(),
            runtime,
            Arc::clone(&hook) as Arc<dyn LifecycleHook>,
        );

        daemon.collect().await.unwrap();
        daemon.advance();
        let work_id = daemon.queue_items()[0].work_id.clone();
        // A manual database change lands while the handler is about to report.
        daemon
            .db()
            .update_phase(&work_id, QueuePhase::Skipped)
            .unwrap();

        let outcomes = daemon.execute_running().await;

        assert!(
            matches!(
                outcomes[0],
                ItemOutcome::Conflicted {
                    current: QueuePhase::Skipped,
                    ..
                }
            ),
            "got {outcomes:?}"
        );
        assert_eq!(hook.on_fail.load(Ordering::SeqCst), 0);
        assert_eq!(hook.on_escalation.load(Ordering::SeqCst), 0);
        assert!(
            daemon.history_events().is_empty(),
            "a discarded execution leaves no attempt history"
        );
        assert!(
            !daemon
                .db()
                .get_token_usage_by_work_id(&work_id)
                .unwrap()
                .is_empty(),
            "the cost of a discarded execution is still recorded"
        );
        let log = daemon.db().transitions_of(&work_id).unwrap();
        assert!(
            log.iter()
                .any(|e| e.kind == "transition_conflict" && e.actor == "daemon"),
            "daemon conflict must be logged: {log:?}"
        );
        assert_eq!(
            daemon.db().get_item(&work_id).unwrap().phase(),
            QueuePhase::Skipped
        );
        assert_eq!(daemon.running_count(), 0);
        assert!(daemon.queue_items().is_empty());
    }

    #[tokio::test]
    async fn running_conflict_on_success_discards_the_execution() {
        let tmp = TempDir::new().unwrap();
        let hook = Arc::new(ReactionCounter::default());
        let mut daemon = daemon_with(
            &tmp,
            one_item_source(),
            MockRuntime::new("mock", vec![0]),
            Arc::clone(&hook) as Arc<dyn LifecycleHook>,
        );

        daemon.collect().await.unwrap();
        daemon.advance();
        let work_id = daemon.queue_items()[0].work_id.clone();
        daemon
            .db()
            .update_phase(&work_id, QueuePhase::Skipped)
            .unwrap();

        let outcomes = daemon.execute_running().await;

        assert!(matches!(outcomes[0], ItemOutcome::Conflicted { .. }));
        assert!(daemon.history_events().is_empty());
        assert_eq!(daemon.items_in_phase(QueuePhase::Completed).len(), 0);
        assert_eq!(
            daemon.db().get_item(&work_id).unwrap().phase(),
            QueuePhase::Skipped
        );
    }
}

// ---------------------------------------------------------------------------
// Store-owned collection, restart restore and tick observation
// ---------------------------------------------------------------------------

mod store_owned {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use belt_core::context::ItemContext;
    use belt_core::queue::QueueItem;
    use belt_core::source::DataSource;
    use belt_core::transition::{Actor, TransitionOutcome, TransitionReason, TransitionRequest};

    use super::*;

    /// A source whose pending items can be refilled between collects.
    #[derive(Clone, Default)]
    struct SharedSource {
        items: Arc<Mutex<Vec<QueueItem>>>,
    }

    impl SharedSource {
        fn offer(&self, source_id: &str, state: &str) {
            self.items.lock().unwrap().push(test_item(source_id, state));
        }
    }

    #[async_trait]
    impl DataSource for SharedSource {
        fn name(&self) -> &str {
            "github"
        }

        async fn collect(&mut self, _ws: &WorkspaceConfig) -> anyhow::Result<Vec<QueueItem>> {
            Ok(std::mem::take(&mut *self.items.lock().unwrap()))
        }

        async fn get_context(&self, item: &QueueItem) -> anyhow::Result<ItemContext> {
            Ok(MockDataSource::default_context(item))
        }
    }

    fn daemon_over(tmp: &TempDir, source: SharedSource, db: Database) -> Daemon {
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(MockRuntime::new("mock", vec![0, 0, 0, 0])));
        Daemon::new(
            test_workspace_config(),
            vec![Box::new(source)],
            Arc::new(registry),
            Box::new(MockWorktreeManager::new(tmp.path().to_path_buf())),
            4,
            db,
        )
    }

    fn db_path(tmp: &TempDir) -> String {
        tmp.path().join("belt.db").to_str().unwrap().to_string()
    }

    fn move_item(db: &Database, work_id: &str, from: QueuePhase, to: QueuePhase, actor: Actor) {
        let outcome = db
            .transition(&TransitionRequest {
                work_id: work_id.to_string(),
                expected_from: from,
                to,
                actor,
                reason: TransitionReason::Manual,
                detail: None,
            })
            .unwrap();
        assert!(
            matches!(outcome, TransitionOutcome::Applied { .. }),
            "{work_id} {from:?}->{to:?}: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn collect_dedupes_by_store_while_an_item_is_open() {
        let tmp = TempDir::new().unwrap();
        let source = SharedSource::default();
        let mut daemon = daemon_over(&tmp, source.clone(), Database::open_in_memory().unwrap());

        source.offer("github:org/repo#1", "analyze");
        source.offer("github:org/repo#1", "analyze");
        assert_eq!(daemon.collect().await.unwrap(), 1);

        source.offer("github:org/repo#1", "analyze");
        assert_eq!(
            daemon.collect().await.unwrap(),
            0,
            "open item blocks recollect"
        );

        assert_eq!(daemon.queue_items().len(), 1);
        assert_eq!(daemon.database().list_items(None, None).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn failed_item_blocks_recollect_until_skipped_then_a_new_lineage_is_issued() {
        let tmp = TempDir::new().unwrap();
        let source = SharedSource::default();
        let mut daemon = daemon_over(&tmp, source.clone(), Database::open_in_memory().unwrap());
        let id = "github:org/repo#1:analyze";

        source.offer("github:org/repo#1", "analyze");
        daemon.collect().await.unwrap();
        let db = Arc::clone(daemon.database());
        move_item(
            &db,
            id,
            QueuePhase::Pending,
            QueuePhase::Ready,
            Actor::Daemon,
        );
        move_item(
            &db,
            id,
            QueuePhase::Ready,
            QueuePhase::Running,
            Actor::Daemon,
        );
        move_item(
            &db,
            id,
            QueuePhase::Running,
            QueuePhase::Failed,
            Actor::Daemon,
        );

        source.offer("github:org/repo#1", "analyze");
        assert_eq!(daemon.collect().await.unwrap(), 0, "Failed still blocks");

        move_item(&db, id, QueuePhase::Failed, QueuePhase::Skipped, Actor::Cli);
        source.offer("github:org/repo#1", "analyze");
        assert_eq!(daemon.collect().await.unwrap(), 1);

        let fresh = daemon.get_item("github:org/repo#1:analyze:2").unwrap();
        assert_eq!(fresh.phase(), QueuePhase::Pending);
        assert_eq!(
            db.get_item("github:org/repo#1:analyze:2").unwrap().phase(),
            QueuePhase::Pending
        );
    }

    #[tokio::test]
    async fn new_daemon_rolls_back_running_and_restores_open_items() {
        let tmp = TempDir::new().unwrap();
        let path = db_path(&tmp);
        let running_id = "github:org/repo#1:analyze";
        let pending_id = "github:org/repo#2:analyze";
        let skipped_id = "github:org/repo#3:analyze";

        {
            let source = SharedSource::default();
            let mut a = daemon_over(&tmp, source.clone(), Database::open(&path).unwrap());
            for n in 1..=3 {
                source.offer(&format!("github:org/repo#{n}"), "analyze");
            }
            a.collect().await.unwrap();
            let db = Arc::clone(a.database());
            move_item(
                &db,
                skipped_id,
                QueuePhase::Pending,
                QueuePhase::Skipped,
                Actor::Cli,
            );
            move_item(
                &db,
                running_id,
                QueuePhase::Pending,
                QueuePhase::Ready,
                Actor::Daemon,
            );
            move_item(
                &db,
                running_id,
                QueuePhase::Ready,
                QueuePhase::Running,
                Actor::Daemon,
            );
        }

        let mut b = daemon_over(
            &tmp,
            SharedSource::default(),
            Database::open(&path).unwrap(),
        );
        assert_eq!(b.queue_items().len(), 0, "nothing is loaded before restore");
        b.restore_from_store().unwrap();

        assert_eq!(b.queue_items().len(), 2, "Skipped is not restored");
        assert_eq!(b.get_item(running_id).unwrap().phase(), QueuePhase::Pending);
        assert_eq!(b.get_item(pending_id).unwrap().phase(), QueuePhase::Pending);
        assert!(b.get_item(skipped_id).is_none());
        assert_eq!(
            b.database().get_item(running_id).unwrap().phase(),
            QueuePhase::Pending
        );

        let log = b.database().transitions_of(running_id).unwrap();
        let rollback = log.last().unwrap();
        assert_eq!(rollback.from_phase.as_deref(), Some("running"));
        assert_eq!(rollback.to_phase.as_deref(), Some("pending"));
        assert_eq!(rollback.reason.as_deref(), Some("rollback"));
        assert_eq!(rollback.actor, "daemon");

        // The restored copy keeps working: a tick advances it.
        b.tick().await.unwrap();
        assert_ne!(
            b.database().get_item(running_id).unwrap().phase(),
            QueuePhase::Pending
        );
    }

    #[tokio::test]
    async fn tick_drops_items_another_connection_skipped() {
        let tmp = TempDir::new().unwrap();
        let path = db_path(&tmp);
        let source = SharedSource::default();
        let mut daemon = daemon_over(&tmp, source.clone(), Database::open(&path).unwrap());
        let id = "github:org/repo#1:analyze";

        source.offer("github:org/repo#1", "analyze");
        daemon.collect().await.unwrap();
        daemon.restore_from_store().unwrap();
        assert!(daemon.get_item(id).is_some());

        let cli = Database::open(&path).unwrap();
        move_item(
            &cli,
            id,
            QueuePhase::Pending,
            QueuePhase::Skipped,
            Actor::Cli,
        );

        daemon.tick().await.unwrap();
        assert!(daemon.get_item(id).is_none(), "skipped item left the copy");
        assert_eq!(cli.get_item(id).unwrap().phase(), QueuePhase::Skipped);
    }

    #[tokio::test]
    async fn tick_follows_phase_changed_by_another_connection() {
        let tmp = TempDir::new().unwrap();
        let path = db_path(&tmp);
        let source = SharedSource::default();
        let mut daemon = daemon_over(&tmp, source.clone(), Database::open(&path).unwrap());
        let id = "github:org/repo#1:analyze";

        source.offer("github:org/repo#1", "analyze");
        daemon.collect().await.unwrap();
        daemon.restore_from_store().unwrap();
        daemon.request_shutdown(); // observe only: no advance

        let cli = Database::open(&path).unwrap();
        move_item(&cli, id, QueuePhase::Pending, QueuePhase::Ready, Actor::Cli);

        daemon.tick().await.unwrap();
        assert_eq!(daemon.get_item(id).unwrap().phase(), QueuePhase::Ready);
    }

    #[tokio::test]
    async fn commit_conflict_on_a_finished_row_drops_the_copy() {
        let tmp = TempDir::new().unwrap();
        let path = db_path(&tmp);
        let source = SharedSource::default();
        let mut daemon = daemon_over(&tmp, source.clone(), Database::open(&path).unwrap());
        let id = "github:org/repo#1:analyze";

        source.offer("github:org/repo#1", "analyze");
        daemon.collect().await.unwrap();
        let db = Arc::clone(daemon.database());
        for (from, to) in [
            (QueuePhase::Pending, QueuePhase::Ready),
            (QueuePhase::Ready, QueuePhase::Running),
            (QueuePhase::Running, QueuePhase::Completed),
        ] {
            move_item(&db, id, from, to, Actor::Daemon);
        }
        daemon.restore_from_store().unwrap();
        assert_eq!(daemon.get_item(id).unwrap().phase(), QueuePhase::Completed);

        let cli = Database::open(&path).unwrap();
        move_item(
            &cli,
            id,
            QueuePhase::Completed,
            QueuePhase::Done,
            Actor::Cli,
        );

        assert!(daemon.mark_done(id).is_err());
        assert!(daemon.get_item(id).is_none(), "finished row: no copy kept");
        assert_eq!(cli.get_item(id).unwrap().phase(), QueuePhase::Done);
    }

    #[tokio::test]
    async fn rollback_conflict_on_a_finished_row_drops_the_copy() {
        let tmp = TempDir::new().unwrap();
        let path = db_path(&tmp);
        let source = SharedSource::default();
        let mut daemon = daemon_over(&tmp, source.clone(), Database::open(&path).unwrap());
        let id = "github:org/repo#1:analyze";

        source.offer("github:org/repo#1", "analyze");
        daemon.collect().await.unwrap();
        daemon.advance();
        assert_eq!(daemon.get_item(id).unwrap().phase(), QueuePhase::Running);

        let other = Database::open(&path).unwrap();
        move_item(
            &other,
            id,
            QueuePhase::Running,
            QueuePhase::Skipped,
            Actor::Daemon,
        );

        daemon.rollback_running_to_pending();
        assert!(daemon.get_item(id).is_none(), "finished row: no copy kept");
        assert_eq!(other.get_item(id).unwrap().phase(), QueuePhase::Skipped);
    }

    #[tokio::test]
    async fn tick_sees_changes_again_after_an_observation_error() {
        let tmp = TempDir::new().unwrap();
        let path = db_path(&tmp);
        let mut daemon = daemon_over(
            &tmp,
            SharedSource::default(),
            Database::open(&path).unwrap(),
        );
        daemon.restore_from_store().unwrap();
        daemon.request_shutdown(); // observe only: no collect, no advance

        let cli = Database::open(&path).unwrap();
        let created = cli
            .insert_collected(&belt_infra::db::NewItem {
                source_id: "github:org/repo#9".to_string(),
                workspace_id: "test-ws".to_string(),
                state: "analyze".to_string(),
                title: None,
                actor: Actor::Cli,
            })
            .unwrap();
        let belt_infra::db::CollectOutcome::Inserted { work_id } = created else {
            panic!("expected a new item, got {created:?}");
        };

        // The row cannot be read, as under a locked or damaged store.
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute(
            "UPDATE queue_items SET phase = 'bogus' WHERE work_id = ?1",
            [&work_id],
        )
        .unwrap();
        assert!(daemon.tick().await.is_err());
        assert!(daemon.get_item(&work_id).is_none());

        raw.execute(
            "UPDATE queue_items SET phase = 'pending' WHERE work_id = ?1",
            [&work_id],
        )
        .unwrap();
        daemon.tick().await.unwrap();
        assert_eq!(
            daemon.get_item(&work_id).map(|i| i.phase()),
            Some(QueuePhase::Pending),
            "the change that failed to read is observed on the next tick"
        );
    }

    #[tokio::test]
    async fn a_hitl_response_from_another_process_is_applied_on_the_next_tick() {
        let tmp = TempDir::new().unwrap();
        let path = db_path(&tmp);
        let source = SharedSource::default();
        let mut daemon = daemon_over(&tmp, source.clone(), Database::open(&path).unwrap());
        let id = "github:org/repo#1:analyze";

        source.offer("github:org/repo#1", "analyze");
        daemon.collect().await.unwrap();
        let db = Arc::clone(daemon.database());
        move_item(
            &db,
            id,
            QueuePhase::Pending,
            QueuePhase::Ready,
            Actor::Daemon,
        );
        move_item(
            &db,
            id,
            QueuePhase::Ready,
            QueuePhase::Running,
            Actor::Daemon,
        );
        move_item(
            &db,
            id,
            QueuePhase::Running,
            QueuePhase::Completed,
            Actor::Daemon,
        );
        let outcome = db
            .open_hitl(&belt_infra::db::OpenHitlRequest {
                work_id: id.to_string(),
                expected_from: QueuePhase::Completed,
                reason: belt_core::queue::HitlReason::EvaluateFailure,
                notes: None,
                actor: Actor::Daemon,
                transition_reason: TransitionReason::Advance,
                timeout_at: None,
                terminal_action: None,
            })
            .unwrap();
        assert!(matches!(
            outcome,
            belt_infra::db::OpenHitlOutcome::Opened { .. }
        ));
        daemon.restore_from_store().unwrap();

        // `belt hitl respond` in another process; the item stays Hitl until
        // the daemon post-processes the confirmed response.
        let cli = belt_daemon::hitl::HitlService::new(Arc::new(Database::open(&path).unwrap()));
        let outcome = cli
            .respond(&belt_daemon::hitl::HitlResponse {
                target: belt_infra::db::HitlTarget::Item(id.to_string()),
                action: belt_core::hitl::HitlAction::Skip,
                by: "bob".to_string(),
                via: "cli".to_string(),
                path: belt_core::hitl::ConfirmPath::Direct,
                notes: None,
            })
            .unwrap();
        assert!(matches!(
            outcome,
            belt_core::hitl::RespondOutcome::Won { .. }
        ));
        assert_eq!(db.get_item(id).unwrap().phase(), QueuePhase::Hitl);

        daemon.tick().await.unwrap();

        assert_eq!(db.get_item(id).unwrap().phase(), QueuePhase::Skipped);
        assert!(daemon.get_item(id).is_none(), "the copy follows the store");
        let last = db.transitions_of(id).unwrap().pop().unwrap();
        assert_eq!(last.reason.as_deref(), Some("post_processing:skip"));
        assert_eq!(last.actor, "daemon");
    }

    #[tokio::test]
    async fn run_fails_when_the_store_cannot_be_read() {
        let tmp = TempDir::new().unwrap();
        let path = db_path(&tmp);
        let db = Database::open(&path).unwrap();
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("DROP TABLE queue_items;")
            .unwrap();

        let mut daemon = daemon_over(&tmp, SharedSource::default(), db);
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), daemon.run(3600))
            .await
            .expect("run must fail at start, not loop");
        assert!(result.is_err());
    }
}
