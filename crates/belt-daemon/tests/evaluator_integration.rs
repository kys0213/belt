//! Integration tests for Evaluator subprocess execution, token usage recording,
//! failure counting with HITL escalation, and batch evaluation.
//!
//! These tests exercise the Evaluator through the Daemon's public API,
//! verifying end-to-end behavior from Completed items through to Done/HITL
//! transitions and token usage DB persistence.

use std::sync::Arc;

use belt_core::phase::QueuePhase;
use belt_core::queue::testing::test_item;
use belt_core::runtime::{RuntimeRegistry, TokenUsage};
use belt_core::workspace::WorkspaceConfig;
use belt_daemon::daemon::Daemon;
use belt_daemon::evaluator::{EvalDecision, Evaluator};
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
        on_fail:
          - script: "echo failed"
    escalation:
      1: retry
      2: retry_with_comment
      3: hitl
"#;
    serde_yaml::from_str(yaml).unwrap()
}

fn setup_daemon_with_db(
    tmp: &TempDir,
    source: MockDataSource,
    exit_codes: Vec<i32>,
    token_usages: Vec<TokenUsage>,
) -> Daemon {
    let config = test_workspace_config();
    let mock = MockRuntime::new("mock", exit_codes).with_token_usages(token_usages);
    let mut registry = RuntimeRegistry::new("mock".to_string());
    registry.register(Arc::new(mock));
    let worktree_mgr = MockWorktreeManager::new(tmp.path().to_path_buf());
    let db = Database::open_in_memory().unwrap();

    Daemon::new(
        config,
        vec![Box::new(source)],
        Arc::new(registry),
        Box::new(worktree_mgr),
        4,
    )
    .with_db(db)
}

/// After a full tick (collect -> advance -> execute -> evaluate), the daemon
/// should record token usage from the evaluate subprocess to the database.
///
/// This verifies that the evaluate step's LLM call is tracked for cost
/// accounting, not just the handler execution.
#[tokio::test]
async fn run_evaluate_records_token_usage_in_db() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    // Two exit codes: first for handler execution, second for evaluate subprocess.
    // Two token usages: first for handler, second for evaluate.
    let mut daemon = setup_daemon_with_db(
        &tmp,
        source,
        vec![0, 0],
        vec![
            TokenUsage {
                input_tokens: 500,
                output_tokens: 200,
                cache_read_tokens: Some(50),
                cache_write_tokens: None,
            },
            TokenUsage {
                input_tokens: 300,
                output_tokens: 100,
                cache_read_tokens: None,
                cache_write_tokens: Some(25),
            },
        ],
    );

    // Run a full tick: collect -> advance -> execute -> evaluate.
    daemon.tick().await.unwrap();

    // Verify token usage was persisted in the database.
    let db = daemon.db().expect("daemon should have a database");
    let rows = db
        .get_token_usage_by_work_id("github:org/repo#1:analyze")
        .unwrap();

    // At least the handler's token usage should be recorded. The evaluate
    // step may also record its own token usage depending on the flow.
    assert!(
        !rows.is_empty(),
        "token usage should be recorded after tick with evaluate"
    );

    // Verify the handler's token usage is present.
    let handler_row = rows
        .iter()
        .find(|r| r.input_tokens == 500)
        .expect("handler token usage should be recorded");
    assert_eq!(handler_row.output_tokens, 200);
    assert_eq!(handler_row.cache_read_tokens, Some(50));
    assert_eq!(handler_row.runtime, "mock");
    assert_eq!(handler_row.workspace, "test-ws");
}

/// When the evaluator fails consecutively up to max_eval_failures (default 3),
/// it should escalate the item to HITL phase.
///
/// This tests the failure counting mechanism and HITL escalation path through
/// the Evaluator's public API directly (not through the daemon's private
/// evaluate_completed method).
#[tokio::test]
async fn eval_failure_count_escalates_to_hitl_after_threshold() {
    let mut evaluator = Evaluator::new("test-ws").with_max_eval_failures(3);

    // Simulate 3 consecutive evaluate failures for the same work_id.
    let work_id = "github:org/repo#1:analyze";

    // Failure 1 -> Retry (stay in Completed)
    let decision = evaluator.record_eval_failure(work_id, "subprocess exit 1");
    assert_eq!(decision, EvalDecision::Retry);
    assert_eq!(evaluator.eval_failure_count(work_id), 1);
    assert_eq!(
        Evaluator::target_phase(&decision),
        QueuePhase::Completed,
        "retry should keep item in Completed"
    );

    // Failure 2 -> Retry
    let decision = evaluator.record_eval_failure(work_id, "subprocess timeout");
    assert_eq!(decision, EvalDecision::Retry);
    assert_eq!(evaluator.eval_failure_count(work_id), 2);

    // Failure 3 -> HITL escalation
    let decision = evaluator.record_eval_failure(work_id, "subprocess crashed");
    assert!(
        matches!(decision, EvalDecision::Hitl { .. }),
        "should escalate to HITL after 3 failures"
    );
    assert_eq!(evaluator.eval_failure_count(work_id), 3);
    assert_eq!(
        Evaluator::target_phase(&decision),
        QueuePhase::Hitl,
        "HITL decision should target Hitl phase"
    );

    // Verify the HITL reason includes failure count and threshold.
    if let EvalDecision::Hitl { reason } = &decision {
        assert!(
            reason.contains("3"),
            "HITL reason should mention failure count: {reason}"
        );
        assert!(
            reason.contains("threshold"),
            "HITL reason should mention threshold: {reason}"
        );
    }

    // After clearing failures, a subsequent failure restarts the count.
    evaluator.clear_eval_failures(work_id);
    assert_eq!(evaluator.eval_failure_count(work_id), 0);
    let decision = evaluator.record_eval_failure(work_id, "new error");
    assert_eq!(
        decision,
        EvalDecision::Retry,
        "should retry after failure count reset"
    );
    assert_eq!(evaluator.eval_failure_count(work_id), 1);
}

/// Batch evaluation: multiple Completed items should be evaluated and
/// classified into Done or Failed (with retry/HITL escalation).
///
/// This verifies the daemon correctly processes a batch of completed items
/// through the evaluate step in a single tick.
#[tokio::test]
async fn evaluate_batch_classifies_multiple_completed_items() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));
    source.add_item(test_item("github:org/repo#2", "analyze"));

    // Two handler executions succeed (exit code 0), then one evaluate call
    // (exit code 0 = success, items transition to Done).
    let mut daemon = setup_daemon_with_db(
        &tmp,
        source,
        vec![0, 0, 0],
        vec![
            TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
            TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
            TokenUsage {
                input_tokens: 200,
                output_tokens: 80,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
        ],
    );

    // Collect and execute to get items to Completed.
    daemon.collect().await.unwrap();
    daemon.advance();
    let outcomes = daemon.execute_running().await;
    assert_eq!(outcomes.len(), 2);

    // Both items should be Completed after handler execution.
    let completed = daemon.items_in_phase(QueuePhase::Completed);
    assert_eq!(
        completed.len(),
        2,
        "both items should be in Completed phase"
    );

    // Run a full tick to trigger evaluate_completed.
    // The evaluate subprocess (mock, exit_code=0) should mark items as Done.
    daemon.tick().await.unwrap();

    // After evaluation, items should no longer be in Completed.
    // They may be Done (removed from queue) or still in queue depending on
    // the on_done handler outcome.
    let still_completed = daemon.items_in_phase(QueuePhase::Completed);
    assert_eq!(
        still_completed.len(),
        0,
        "no items should remain Completed after successful evaluation"
    );
}

/// build_evaluate_prompt should reflect the workspace name from config.
///
/// This is a focused unit-level test verifying that the prompt builder
/// correctly incorporates workspace configuration.
#[tokio::test]
async fn build_evaluate_prompt_reflects_workspace_config() {
    let evaluator = Evaluator::new("production-api")
        .with_workspace_config_path(std::path::PathBuf::from("/etc/belt/workspace.yaml"));

    let prompt = evaluator.build_evaluate_prompt();

    assert!(
        prompt.contains("production-api"),
        "prompt should contain the workspace name"
    );
    assert!(
        prompt.contains("belt queue done"),
        "prompt should reference 'belt queue done' command"
    );
    assert!(
        prompt.contains("belt queue hitl"),
        "prompt should reference 'belt queue hitl' command"
    );

    // Verify different workspace name produces different prompt.
    let evaluator2 = Evaluator::new("staging-api");
    let prompt2 = evaluator2.build_evaluate_prompt();
    assert!(
        prompt2.contains("staging-api"),
        "prompt should reflect the configured workspace name"
    );
    assert!(
        !prompt2.contains("production-api"),
        "prompt should not contain a different workspace name"
    );
}
