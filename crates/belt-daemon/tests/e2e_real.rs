//! Real E2E tests for Belt.
//!
//! These tests interact with **real** GitHub (kys0213/belt) and Claude API.
//! They are gated with `#[ignore]` so they never run in normal `cargo test`.
//!
//! Run manually:
//! ```bash
//! cargo test -p belt-daemon -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! Prerequisites:
//! - `gh auth status` must succeed
//! - `claude --version` must succeed
//! - GitHub labels `e2e-test:analyze` and `e2e-test:implement` must exist on kys0213/belt

mod e2e_helpers;

use belt_core::phase::QueuePhase;
use e2e_helpers::*;
use tempfile::TempDir;

// ─── Test 1: Setup and Collect ───────────────────────────────────
//
// Creates a real GitHub issue with `e2e-test:analyze` label,
// then verifies GitHubDataSource.collect() picks it up as a Pending item.
// No Claude API calls.

#[tokio::test]
#[ignore]
async fn e2e_setup_and_collect() {
    assert_prerequisites();

    let number = create_test_issue("[E2E] setup_and_collect test", "e2e-test:analyze");
    let _guard = TestIssueGuard { number };

    // Give GitHub a moment to index the label.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let tmp = TempDir::new().unwrap();
    let mut daemon = setup_real_daemon(&tmp);

    // Collect should pick up our test issue.
    let collected = daemon.collect().await.unwrap();
    assert!(collected >= 1, "expected at least 1 item collected, got {collected}");

    // Find our item by source_id pattern.
    let our_item = daemon
        .queue_items()
        .iter()
        .find(|it| it.work_id.contains(&format!("#{number}")))
        .expect("our test issue should be in the queue");

    assert_eq!(our_item.phase, QueuePhase::Pending);
    assert_eq!(our_item.state, "analyze");
}

// ─── Test 2: Full Pipeline with Real Claude ──────────────────────
//
// Creates a real issue, runs collect → advance → execute with real Claude.
// Verifies the item reaches Completed and transition events are recorded in DB.
// 1 Claude API call.

#[tokio::test]
#[ignore]
async fn e2e_full_pipeline_analyze() {
    assert_prerequisites();

    let number = create_test_issue("[E2E] full_pipeline_analyze test", "e2e-test:analyze");
    let _guard = TestIssueGuard { number };

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let tmp = TempDir::new().unwrap();
    let mut daemon = setup_real_daemon(&tmp);

    // Collect
    let collected = daemon.collect().await.unwrap();
    assert!(collected >= 1, "should collect at least 1 item");

    // Advance: Pending → Ready → Running
    daemon.advance();
    let running = daemon.items_in_phase(QueuePhase::Running);
    assert!(
        !running.is_empty(),
        "at least 1 item should be Running after advance"
    );

    // Execute with real Claude (timeout: 2 min)
    let outcomes = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        daemon.execute_running(),
    )
    .await
    .expect("execute timed out after 120s");

    assert!(!outcomes.is_empty(), "should have execution outcomes");

    // Item should be Completed (handler succeeded).
    let completed = daemon.items_in_phase(QueuePhase::Completed);
    assert!(
        !completed.is_empty(),
        "at least 1 item should be Completed after execution"
    );

    // Verify transition events in DB.
    let db = open_db(&db_path(&tmp));
    let work_id = &completed[0].work_id;
    let events = db.list_transition_events(work_id).unwrap();
    assert!(
        !events.is_empty(),
        "transition events should be recorded in DB"
    );

    // Verify at least one event has event_type containing phase transition.
    let has_phase_event = events.iter().any(|e| e.event_type == "phase_enter");
    assert!(
        has_phase_event,
        "should have at least one phase_enter event, got: {events:?}"
    );
}

// ─── Test 3: Handler Failure and Escalation ──────────────────────
//
// Uses MockRuntime (exit_code=1) with real GitHubDataSource to test
// the failure → escalation → retry → HITL path.
// No Claude API calls.

#[tokio::test]
#[ignore]
async fn e2e_handler_failure_escalation() {
    assert_prerequisites();

    let number = create_test_issue(
        "[E2E] handler_failure_escalation test",
        "e2e-test:analyze",
    );
    let _guard = TestIssueGuard { number };

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let tmp = TempDir::new().unwrap();
    // MockRuntime: all calls fail (exit_code=1).
    let mut daemon = setup_mock_runtime_daemon(&tmp, vec![1, 1, 1, 1]);

    // Step 1: Collect the issue.
    let collected = daemon.collect().await.unwrap();
    assert!(collected >= 1, "should collect at least 1 item");

    // Step 2: Advance (Pending → Ready → Running) and execute (fail).
    daemon.advance();
    let outcomes = daemon.execute_running().await;
    assert!(!outcomes.is_empty(), "should have execution outcomes");

    // First failure → escalation policy level 1: Retry → new Pending item.
    let pending = daemon.items_in_phase(QueuePhase::Pending);
    assert!(
        !pending.is_empty(),
        "retry escalation should create a new Pending item, queue: {:?}",
        daemon
            .queue_items()
            .iter()
            .map(|i| format!("{}:{}", i.work_id, i.phase))
            .collect::<Vec<_>>()
    );

    // Step 3: Advance + execute again (second failure → hitl).
    daemon.advance();
    let outcomes = daemon.execute_running().await;
    assert!(!outcomes.is_empty(), "should have second round outcomes");

    let hitl = daemon.items_in_phase(QueuePhase::Hitl);
    assert!(
        !hitl.is_empty(),
        "second failure should escalate to HITL, queue: {:?}",
        daemon
            .queue_items()
            .iter()
            .map(|i| format!("{}:{}", i.work_id, i.phase))
            .collect::<Vec<_>>()
    );
}

// ─── Test 4: HITL Respond ────────────────────────────────────────
//
// Manually inserts an item, marks it HITL, then responds with Done.
// Validates the phase transition path.
// No external calls.

#[tokio::test]
#[ignore]
async fn e2e_hitl_respond() {
    assert_prerequisites();

    let number = create_test_issue("[E2E] hitl_respond test", "e2e-test:analyze");
    let _guard = TestIssueGuard { number };

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let tmp = TempDir::new().unwrap();
    // Use mock runtime that succeeds, so we can manually force HITL.
    let mut daemon = setup_mock_runtime_daemon(&tmp, vec![0]);

    // Collect and advance to get an item through the pipeline.
    daemon.collect().await.unwrap();
    daemon.advance();
    daemon.execute_running().await;

    // Find our completed item.
    let completed = daemon.items_in_phase(QueuePhase::Completed);
    assert!(!completed.is_empty(), "should have a Completed item");
    let work_id = completed[0].work_id.clone();

    // Manually mark it as HITL (simulating evaluate failure or manual escalation).
    let hitl_result = daemon.mark_hitl(
        &work_id,
        belt_core::queue::HitlReason::ManualEscalation,
        None,
    );
    assert!(hitl_result.is_ok(), "mark_hitl should succeed");
    assert_eq!(
        daemon.get_item(&work_id).unwrap().phase,
        QueuePhase::Hitl
    );

    // Respond with Done.
    let done_result = daemon.mark_done(&work_id);
    assert!(done_result.is_ok(), "mark_done should succeed");
    assert_eq!(
        daemon.get_item(&work_id).unwrap().phase,
        QueuePhase::Done
    );
}

// ─── Test 5: Token Usage Tracking ────────────────────────────────
//
// Runs one full pipeline with real Claude and verifies that token usage
// is recorded in the database.
// 1 Claude API call.

#[tokio::test]
#[ignore]
async fn e2e_token_usage_tracking() {
    assert_prerequisites();

    let number = create_test_issue("[E2E] token_usage_tracking test", "e2e-test:analyze");
    let _guard = TestIssueGuard { number };

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let tmp = TempDir::new().unwrap();
    let mut daemon = setup_real_daemon(&tmp);

    // Run one full tick (collect → advance → execute → evaluate).
    let tick_result = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        daemon.tick(),
    )
    .await
    .expect("tick timed out after 120s");

    assert!(tick_result.is_ok(), "tick should succeed: {tick_result:?}");

    // Verify token usage was recorded in DB.
    let db = open_db(&db_path(&tmp));
    let stats = db.get_runtime_stats().unwrap();

    assert!(
        stats.total_tokens > 0,
        "total_tokens should be > 0 after real Claude execution, got: {}",
        stats.total_tokens
    );
    assert!(
        stats.executions > 0,
        "executions count should be > 0, got: {}",
        stats.executions
    );
}
