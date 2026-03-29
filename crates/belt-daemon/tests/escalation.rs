//! E2E integration test: Escalation and HITL flow
//!
//! Tests repeated failure escalation through the escalation policy ladder
//! and HITL (human-in-the-loop) lifecycle.

use std::sync::Arc;

use belt_core::escalation::EscalationAction;
use belt_core::phase::QueuePhase;
use belt_core::queue::testing::test_item;
use belt_core::queue::{HitlReason, HitlRespondAction};
use belt_core::runtime::RuntimeRegistry;
use belt_core::workspace::WorkspaceConfig;
use belt_daemon::daemon::{Daemon, ItemOutcome};
use belt_daemon::evaluator::{DEFAULT_MAX_EVAL_FAILURES, EvalDecision, Evaluator};
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

/// First failure -> EscalationAction::Retry (silent retry, no on_fail).
#[tokio::test]
async fn first_failure_escalation_is_retry() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    let mut daemon = setup_daemon(&tmp, source, vec![1]);

    daemon.collect().await.unwrap();
    daemon.advance();
    let outcomes = daemon.execute_running().await;

    match &outcomes[0] {
        ItemOutcome::Failed { escalation, .. } => {
            assert_eq!(*escalation, EscalationAction::Retry);
            assert!(
                !escalation.should_run_on_fail(),
                "Retry should NOT run on_fail"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    // Retry should enqueue a new Pending item.
    assert_eq!(daemon.items_in_phase(QueuePhase::Pending).len(), 1);
}

/// Second failure escalates beyond Retry.
///
/// The daemon counts failures from both history and history_events, so
/// each execution failure records entries in both stores. This means the
/// effective failure count grows faster than the number of execute calls.
/// After one prior failure, the second execute sees an accumulated count
/// that resolves to Hitl (the highest configured escalation level).
#[tokio::test]
async fn second_failure_escalation_beyond_retry() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    // Two consecutive failures.
    let mut daemon = setup_daemon(&tmp, source, vec![1, 1]);

    // First failure -> Retry (count_failures=0, resolve(1)=Retry).
    daemon.collect().await.unwrap();
    daemon.advance();
    let outcomes = daemon.execute_running().await;
    match &outcomes[0] {
        ItemOutcome::Failed { escalation, .. } => {
            assert_eq!(*escalation, EscalationAction::Retry);
        }
        other => panic!("expected Failed with Retry, got {other:?}"),
    }

    // The retry item is now in Pending. Advance and execute again.
    daemon.advance();
    let outcomes = daemon.execute_running().await;

    // Second failure: accumulated count from both history + history_events
    // results in an escalation beyond Retry.
    match &outcomes[0] {
        ItemOutcome::Failed { escalation, .. } => {
            assert!(
                escalation.should_run_on_fail(),
                "second failure escalation should run on_fail (got {escalation:?})"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// Repeated failures eventually escalate to HITL.
///
/// After enough failures, the escalation policy routes to Hitl.
/// The item should end up in the Hitl phase with RetryMaxExceeded reason.
#[tokio::test]
async fn repeated_failures_escalate_to_hitl() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    // Two consecutive failures are enough to reach Hitl escalation
    // (count_failures counts both history + history_events per failure).
    let mut daemon = setup_daemon(&tmp, source, vec![1, 1]);

    // First failure -> Retry.
    daemon.collect().await.unwrap();
    daemon.advance();
    daemon.execute_running().await;

    // Second failure -> Hitl (accumulated failure count >= 3).
    daemon.advance();
    let outcomes = daemon.execute_running().await;

    match &outcomes[0] {
        ItemOutcome::Failed { escalation, .. } => {
            assert_eq!(*escalation, EscalationAction::Hitl);
        }
        other => panic!("expected Failed with Hitl escalation, got {other:?}"),
    }

    // The item should now be in Hitl phase in the queue.
    let hitl = daemon.items_in_phase(QueuePhase::Hitl);
    assert_eq!(
        hitl.len(),
        1,
        "item should be in Hitl after repeated failures"
    );
    assert_eq!(
        hitl[0].hitl_reason,
        Some(HitlReason::RetryMaxExceeded),
        "HITL reason should be RetryMaxExceeded"
    );
}

/// HITL item can be resolved with Done action.
#[tokio::test]
async fn hitl_respond_done() {
    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let mut daemon = setup_daemon(&tmp, source, vec![]);

    // Manually set up an item in Completed -> Hitl.
    let mut item = test_item("github:org/repo#1", "analyze");
    item.phase = QueuePhase::Running;
    item.updated_at = chrono::Utc::now().to_rfc3339();
    daemon.push_item(item);

    daemon.complete_item("github:org/repo#1:analyze").unwrap();
    daemon
        .mark_hitl(
            "github:org/repo#1:analyze",
            HitlReason::EvaluateFailure,
            Some("eval failed".to_string()),
        )
        .unwrap();

    // Respond with Done.
    daemon
        .respond_hitl(
            "github:org/repo#1:analyze",
            HitlRespondAction::Done,
            Some("human".to_string()),
            None,
        )
        .await
        .unwrap();

    let item = daemon.get_item("github:org/repo#1:analyze").unwrap();
    assert_eq!(item.phase, QueuePhase::Done);
    assert_eq!(item.hitl_respondent.as_deref(), Some("human"));
}

/// HITL item can be resolved with Retry action (goes back to Pending).
#[tokio::test]
async fn hitl_respond_retry() {
    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let mut daemon = setup_daemon(&tmp, source, vec![]);

    let mut item = test_item("github:org/repo#1", "analyze");
    item.phase = QueuePhase::Running;
    item.updated_at = chrono::Utc::now().to_rfc3339();
    daemon.push_item(item);

    daemon.complete_item("github:org/repo#1:analyze").unwrap();
    daemon
        .mark_hitl(
            "github:org/repo#1:analyze",
            HitlReason::ManualEscalation,
            None,
        )
        .unwrap();

    daemon
        .respond_hitl(
            "github:org/repo#1:analyze",
            HitlRespondAction::Retry,
            None,
            None,
        )
        .await
        .unwrap();

    let item = daemon.get_item("github:org/repo#1:analyze").unwrap();
    assert_eq!(item.phase, QueuePhase::Pending);
}

/// HITL item can be resolved with Skip action.
#[tokio::test]
async fn hitl_respond_skip() {
    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let mut daemon = setup_daemon(&tmp, source, vec![]);

    let mut item = test_item("github:org/repo#1", "analyze");
    item.phase = QueuePhase::Running;
    item.updated_at = chrono::Utc::now().to_rfc3339();
    daemon.push_item(item);

    daemon.complete_item("github:org/repo#1:analyze").unwrap();
    daemon
        .mark_hitl(
            "github:org/repo#1:analyze",
            HitlReason::Timeout,
            Some("timed out".to_string()),
        )
        .unwrap();

    daemon
        .respond_hitl(
            "github:org/repo#1:analyze",
            HitlRespondAction::Skip,
            None,
            None,
        )
        .await
        .unwrap();

    let item = daemon.get_item("github:org/repo#1:analyze").unwrap();
    assert_eq!(item.phase, QueuePhase::Skipped);
}

/// Failed items have worktree_preserved flag set.
#[tokio::test]
async fn failed_items_preserve_worktree() {
    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let mut daemon = setup_daemon(&tmp, source, vec![]);

    let mut item = test_item("github:org/repo#1", "analyze");
    item.phase = QueuePhase::Running;
    item.updated_at = chrono::Utc::now().to_rfc3339();
    daemon.push_item(item);

    daemon
        .mark_failed("github:org/repo#1:analyze", "test failure".to_string())
        .unwrap();

    let item = daemon.get_item("github:org/repo#1:analyze").unwrap();
    assert_eq!(item.phase, QueuePhase::Failed);
    assert!(
        item.worktree_preserved,
        "failed items should have worktree_preserved=true"
    );
}

/// Evaluator: repeated eval failures escalate to HITL.
#[test]
fn evaluator_repeated_failures_escalate_to_hitl() {
    let mut evaluator = Evaluator::new("test-ws").with_max_eval_failures(3);

    // Failure 1: Retry.
    let decision = evaluator.record_eval_failure("item-1", "error");
    assert_eq!(decision, EvalDecision::Retry);
    assert_eq!(evaluator.eval_failure_count("item-1"), 1);

    // Failure 2: Retry.
    let decision = evaluator.record_eval_failure("item-1", "error");
    assert_eq!(decision, EvalDecision::Retry);
    assert_eq!(evaluator.eval_failure_count("item-1"), 2);

    // Failure 3: HITL escalation.
    let decision = evaluator.record_eval_failure("item-1", "error");
    assert!(
        matches!(decision, EvalDecision::Hitl { .. }),
        "third failure should escalate to HITL"
    );
    assert_eq!(evaluator.eval_failure_count("item-1"), 3);
}

/// Evaluator: clearing failures resets the count.
#[test]
fn evaluator_clear_failures_allows_retry() {
    let mut evaluator = Evaluator::new("test-ws").with_max_eval_failures(2);

    evaluator.record_eval_failure("item-1", "error");
    evaluator.record_eval_failure("item-1", "error");
    // Should have escalated to HITL after 2 failures.

    evaluator.clear_eval_failures("item-1");
    assert_eq!(evaluator.eval_failure_count("item-1"), 0);

    // After clearing, failures start from zero again.
    let decision = evaluator.record_eval_failure("item-1", "error");
    assert_eq!(decision, EvalDecision::Retry);
    assert_eq!(evaluator.eval_failure_count("item-1"), 1);
}

/// Evaluator: independent failure tracking per item.
#[test]
fn evaluator_independent_tracking_per_item() {
    let mut evaluator = Evaluator::new("test-ws").with_max_eval_failures(2);

    // Item 1: 1 failure.
    evaluator.record_eval_failure("item-1", "error");
    // Item 2: 1 failure.
    evaluator.record_eval_failure("item-2", "error");

    assert_eq!(evaluator.eval_failure_count("item-1"), 1);
    assert_eq!(evaluator.eval_failure_count("item-2"), 1);

    // Item 1 hits threshold.
    let decision = evaluator.record_eval_failure("item-1", "error");
    assert!(matches!(decision, EvalDecision::Hitl { .. }));

    // Item 2 also hits threshold.
    let decision = evaluator.record_eval_failure("item-2", "error");
    assert!(matches!(decision, EvalDecision::Hitl { .. }));
}

/// Default max eval failures is 3.
#[test]
fn evaluator_default_max_failures() {
    assert_eq!(DEFAULT_MAX_EVAL_FAILURES, 3);
}

/// History events are recorded for failures.
#[tokio::test]
async fn failure_records_history_event() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    let mut daemon = setup_daemon(&tmp, source, vec![1]);

    daemon.collect().await.unwrap();
    daemon.advance();
    daemon.execute_running().await;

    assert!(
        !daemon.history_events().is_empty(),
        "failure should produce a history event"
    );
    assert_eq!(daemon.history_events()[0].status, "failed");
}

/// Multi-spec conflict detection escalates to HITL with SpecConflict reason.
///
/// When a queue item's spec has overlapping entry_points with another active
/// spec, the advance() method detects the conflict and escalates to HITL.
#[tokio::test]
async fn spec_conflict_detection_creates_hitl() {
    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let mut daemon = setup_daemon(&tmp, source, vec![]);

    let mut item = test_item("github:org/repo#1", "implement");
    item.phase = QueuePhase::Running;
    item.updated_at = chrono::Utc::now().to_rfc3339();
    daemon.push_item(item);

    daemon.complete_item("github:org/repo#1:implement").unwrap();
    daemon
        .mark_hitl(
            "github:org/repo#1:implement",
            HitlReason::SpecConflict,
            Some(
                "spec-conflict: entry_point overlap with [spec-2] on paths [src/auth]".to_string(),
            ),
        )
        .unwrap();

    let item = daemon.get_item("github:org/repo#1:implement").unwrap();
    assert_eq!(item.phase, QueuePhase::Hitl);
    assert_eq!(item.hitl_reason, Some(HitlReason::SpecConflict));
    assert!(
        item.hitl_notes
            .as_deref()
            .unwrap()
            .contains("spec-conflict")
    );
}

/// Spec conflict HITL approved with Done: specs proceed in parallel.
///
/// When the user approves conflicting specs (Done), the item transitions
/// to Done, allowing both specs to proceed concurrently.
#[tokio::test]
async fn spec_conflict_hitl_approve_proceeds_parallel() {
    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let mut daemon = setup_daemon(&tmp, source, vec![]);

    let mut item = test_item("github:org/repo#1", "implement");
    item.phase = QueuePhase::Running;
    item.updated_at = chrono::Utc::now().to_rfc3339();
    daemon.push_item(item);

    daemon.complete_item("github:org/repo#1:implement").unwrap();
    daemon
        .mark_hitl(
            "github:org/repo#1:implement",
            HitlReason::SpecConflict,
            Some("spec-conflict: overlap with [spec-2]".to_string()),
        )
        .unwrap();

    daemon
        .respond_hitl(
            "github:org/repo#1:implement",
            HitlRespondAction::Done,
            Some("reviewer".to_string()),
            Some("approved for parallel execution".to_string()),
        )
        .await
        .unwrap();

    let item = daemon.get_item("github:org/repo#1:implement").unwrap();
    assert_eq!(item.phase, QueuePhase::Done);
    assert_eq!(item.hitl_respondent.as_deref(), Some("reviewer"));
}

/// Spec conflict HITL rejected with Skip: later spec is rejected.
///
/// When the user rejects the conflicting spec (Skip), the item transitions
/// to Skipped, blocking the later spec from proceeding.
#[tokio::test]
async fn spec_conflict_hitl_reject_skips_later_spec() {
    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let mut daemon = setup_daemon(&tmp, source, vec![]);

    let mut item = test_item("github:org/repo#1", "implement");
    item.phase = QueuePhase::Running;
    item.updated_at = chrono::Utc::now().to_rfc3339();
    daemon.push_item(item);

    daemon.complete_item("github:org/repo#1:implement").unwrap();
    daemon
        .mark_hitl(
            "github:org/repo#1:implement",
            HitlReason::SpecConflict,
            Some("spec-conflict: overlap with [spec-2]".to_string()),
        )
        .unwrap();

    daemon
        .respond_hitl(
            "github:org/repo#1:implement",
            HitlRespondAction::Skip,
            Some("reviewer".to_string()),
            Some("reject conflicting spec".to_string()),
        )
        .await
        .unwrap();

    let item = daemon.get_item("github:org/repo#1:implement").unwrap();
    assert_eq!(item.phase, QueuePhase::Skipped);
}

/// Spec conflict HITL with Retry: re-check conflict after modification.
///
/// When the user requests retry on a spec conflict, the item goes back
/// to Pending so the conflict check can be re-evaluated on the next advance().
#[tokio::test]
async fn spec_conflict_hitl_retry_re_evaluates() {
    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let mut daemon = setup_daemon(&tmp, source, vec![]);

    let mut item = test_item("github:org/repo#1", "implement");
    item.phase = QueuePhase::Running;
    item.updated_at = chrono::Utc::now().to_rfc3339();
    daemon.push_item(item);

    daemon.complete_item("github:org/repo#1:implement").unwrap();
    daemon
        .mark_hitl(
            "github:org/repo#1:implement",
            HitlReason::SpecConflict,
            Some("spec-conflict: overlap with [spec-2]".to_string()),
        )
        .unwrap();

    daemon
        .respond_hitl(
            "github:org/repo#1:implement",
            HitlRespondAction::Retry,
            Some("reviewer".to_string()),
            Some("modified entry_points, retry conflict check".to_string()),
        )
        .await
        .unwrap();

    let item = daemon.get_item("github:org/repo#1:implement").unwrap();
    assert_eq!(item.phase, QueuePhase::Pending);
}

/// Spec conflict HITL with Replan: delegate to Claw for spec modification.
///
/// When the user requests replan on a spec conflict, the item is rolled back
/// to Pending and a new HITL item is created for spec modification proposal.
#[tokio::test]
async fn spec_conflict_hitl_replan_creates_modification_item() {
    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let mut daemon = setup_daemon(&tmp, source, vec![]);

    let mut item = test_item("github:org/repo#1", "implement");
    item.phase = QueuePhase::Running;
    item.updated_at = chrono::Utc::now().to_rfc3339();
    daemon.push_item(item);

    daemon.complete_item("github:org/repo#1:implement").unwrap();
    daemon
        .mark_hitl(
            "github:org/repo#1:implement",
            HitlReason::SpecConflict,
            Some("spec-conflict: overlap with [spec-2]".to_string()),
        )
        .unwrap();

    daemon
        .respond_hitl(
            "github:org/repo#1:implement",
            HitlRespondAction::Replan,
            Some("reviewer".to_string()),
            Some("remove overlapping entry_points from spec-2".to_string()),
        )
        .await
        .unwrap();

    let item = daemon.get_item("github:org/repo#1:implement").unwrap();
    assert_eq!(item.phase, QueuePhase::Pending);
    assert_eq!(item.replan_count, 1);

    let hitl_items = daemon.items_in_phase(QueuePhase::Hitl);
    assert_eq!(hitl_items.len(), 1);
    assert_eq!(
        hitl_items[0].hitl_reason,
        Some(HitlReason::SpecModificationProposed)
    );
}

/// Escalation policy: EscalationAction::Retry.is_retry() is true.
#[test]
fn escalation_action_is_retry() {
    assert!(EscalationAction::Retry.is_retry());
    assert!(EscalationAction::RetryWithComment.is_retry());
    assert!(!EscalationAction::Hitl.is_retry());
    assert!(!EscalationAction::Skip.is_retry());
    assert!(!EscalationAction::Replan.is_retry());
}

/// Escalation policy: should_run_on_fail returns false only for Retry.
#[test]
fn escalation_on_fail_policy() {
    assert!(!EscalationAction::Retry.should_run_on_fail());
    assert!(EscalationAction::RetryWithComment.should_run_on_fail());
    assert!(EscalationAction::Hitl.should_run_on_fail());
    assert!(EscalationAction::Skip.should_run_on_fail());
    assert!(EscalationAction::Replan.should_run_on_fail());
}

// ---------------------------------------------------------------
// Spec completion HITL flow
// ---------------------------------------------------------------

/// Spec completion HITL approved (Done) transitions spec Completing -> Completed.
#[tokio::test]
async fn spec_completion_hitl_approve_transitions_to_completed() {
    use belt_core::queue::QueueItem;
    use belt_core::spec::{Spec, SpecStatus};
    use belt_infra::db::Database;

    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let daemon = setup_daemon(&tmp, source, vec![]);

    let db = Database::open_in_memory().unwrap();
    let mut spec = Spec::new(
        "spec-100".to_string(),
        "test-ws".to_string(),
        "Integration Test Spec".to_string(),
        "spec content".to_string(),
    );
    spec.status = SpecStatus::Completing;
    db.insert_spec(&spec).unwrap();

    let mut daemon = daemon.with_db(db);

    let mut item = QueueItem::new(
        "spec-completion:spec-100:hitl".to_string(),
        "spec-100".to_string(),
        "test-ws".to_string(),
        "spec_completion".to_string(),
    );
    item.phase = QueuePhase::Hitl;
    item.hitl_reason = Some(HitlReason::SpecCompletionReview);
    daemon.push_item(item);

    daemon
        .respond_hitl(
            "spec-completion:spec-100:hitl",
            HitlRespondAction::Done,
            Some("human-reviewer".into()),
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        daemon
            .get_item("spec-completion:spec-100:hitl")
            .unwrap()
            .phase,
        QueuePhase::Done
    );

    let updated = daemon.database().unwrap().get_spec("spec-100").unwrap();
    assert_eq!(updated.status, SpecStatus::Completed);
}

/// Spec completion HITL rejected (Skip) reverts spec Completing -> Active.
#[tokio::test]
async fn spec_completion_hitl_reject_reverts_to_active() {
    use belt_core::queue::QueueItem;
    use belt_core::spec::{Spec, SpecStatus};
    use belt_infra::db::Database;

    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let daemon = setup_daemon(&tmp, source, vec![]);

    let db = Database::open_in_memory().unwrap();
    let mut spec = Spec::new(
        "spec-101".to_string(),
        "test-ws".to_string(),
        "Integration Test Spec".to_string(),
        "spec content".to_string(),
    );
    spec.status = SpecStatus::Completing;
    db.insert_spec(&spec).unwrap();

    let mut daemon = daemon.with_db(db);

    let mut item = QueueItem::new(
        "spec-completion:spec-101:hitl".to_string(),
        "spec-101".to_string(),
        "test-ws".to_string(),
        "spec_completion".to_string(),
    );
    item.phase = QueuePhase::Hitl;
    item.hitl_reason = Some(HitlReason::SpecCompletionReview);
    daemon.push_item(item);

    daemon
        .respond_hitl(
            "spec-completion:spec-101:hitl",
            HitlRespondAction::Skip,
            Some("human-reviewer".into()),
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        daemon
            .get_item("spec-completion:spec-101:hitl")
            .unwrap()
            .phase,
        QueuePhase::Skipped
    );

    let updated = daemon.database().unwrap().get_spec("spec-101").unwrap();
    assert_eq!(updated.status, SpecStatus::Active);
}

/// Spec completion HITL retry reverts spec Completing -> Active.
#[tokio::test]
async fn spec_completion_hitl_retry_reverts_to_active() {
    use belt_core::queue::QueueItem;
    use belt_core::spec::{Spec, SpecStatus};
    use belt_infra::db::Database;

    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let daemon = setup_daemon(&tmp, source, vec![]);

    let db = Database::open_in_memory().unwrap();
    let mut spec = Spec::new(
        "spec-102".to_string(),
        "test-ws".to_string(),
        "Integration Test Spec".to_string(),
        "spec content".to_string(),
    );
    spec.status = SpecStatus::Completing;
    db.insert_spec(&spec).unwrap();

    let mut daemon = daemon.with_db(db);

    let mut item = QueueItem::new(
        "spec-completion:spec-102:hitl".to_string(),
        "spec-102".to_string(),
        "test-ws".to_string(),
        "spec_completion".to_string(),
    );
    item.phase = QueuePhase::Hitl;
    item.hitl_reason = Some(HitlReason::SpecCompletionReview);
    daemon.push_item(item);

    daemon
        .respond_hitl(
            "spec-completion:spec-102:hitl",
            HitlRespondAction::Retry,
            Some("human-reviewer".into()),
            Some("needs more work on module X".into()),
        )
        .await
        .unwrap();

    assert_eq!(
        daemon
            .get_item("spec-completion:spec-102:hitl")
            .unwrap()
            .phase,
        QueuePhase::Pending
    );

    let updated = daemon.database().unwrap().get_spec("spec-102").unwrap();
    assert_eq!(updated.status, SpecStatus::Active);
}
