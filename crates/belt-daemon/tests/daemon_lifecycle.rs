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
/// they transition to Done via execute_on_done and are removed, or on failure
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

/// Collect deduplicates items by work_id.
#[tokio::test]
async fn collect_deduplicates_by_work_id() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));
    source.add_item(test_item("github:org/repo#1", "analyze")); // duplicate

    let mut daemon = setup_daemon(&tmp, source, vec![]);

    let collected = daemon.collect().await.unwrap();
    // DataSource returns 2 items but daemon deduplicates by work_id.
    assert_eq!(collected, 2, "DataSource reports 2 items collected");
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
        daemon.get_item("github:org/repo#1:analyze").unwrap().phase,
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
    )
    .with_db(db);

    daemon.collect().await.unwrap();
    daemon.advance();
    let outcomes = daemon.execute_running().await;
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(outcomes[0], ItemOutcome::Completed(_)));

    // Verify token_usage was persisted in the database.
    let db = daemon.db().expect("daemon should have a database");
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
    )
    .with_db(db);

    daemon.collect().await.unwrap();
    daemon.advance();
    let outcomes = daemon.execute_running().await;
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(outcomes[0], ItemOutcome::Failed { .. }));

    // Even on failure, token_usage should be recorded.
    let db = daemon.db().expect("daemon should have a database");
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
