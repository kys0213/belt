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

    // Retry continues in a derived Pending item.
    assert_eq!(daemon.items_in_phase(QueuePhase::Pending).len(), 1);
}

/// Second failure of the lineage escalates to RetryWithComment.
#[tokio::test]
async fn second_failure_escalation_beyond_retry() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    // Two consecutive failures.
    let mut daemon = setup_daemon(&tmp, source, vec![1, 1]);

    // First failure -> Retry (failure_count=1).
    daemon.collect().await.unwrap();
    daemon.advance();
    let outcomes = daemon.execute_running().await;
    match &outcomes[0] {
        ItemOutcome::Failed { escalation, .. } => {
            assert_eq!(*escalation, EscalationAction::Retry);
        }
        other => panic!("expected Failed with Retry, got {other:?}"),
    }

    // The derived item is now in Pending. Advance and execute again.
    daemon.advance();
    let outcomes = daemon.execute_running().await;

    match &outcomes[0] {
        ItemOutcome::Failed { escalation, .. } => {
            assert_eq!(*escalation, EscalationAction::RetryWithComment);
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// The third failure of the lineage escalates to HITL.
///
/// The item should end up in the Hitl phase with RetryMaxExceeded reason.
#[tokio::test]
async fn repeated_failures_escalate_to_hitl() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));

    let mut daemon = setup_daemon(&tmp, source, vec![1, 1, 1]);

    // First failure -> Retry, second -> RetryWithComment.
    daemon.collect().await.unwrap();
    daemon.advance();
    daemon.execute_running().await;
    daemon.advance();
    daemon.execute_running().await;

    // Third failure -> Hitl.
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
    item.set_phase_unchecked(QueuePhase::Running);
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
    assert_eq!(item.phase(), QueuePhase::Done);
    assert_eq!(item.hitl_respondent.as_deref(), Some("human"));
}

/// HITL item can be resolved with Retry action (goes back to Pending).
#[tokio::test]
async fn hitl_respond_retry() {
    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let mut daemon = setup_daemon(&tmp, source, vec![]);

    let mut item = test_item("github:org/repo#1", "analyze");
    item.set_phase_unchecked(QueuePhase::Running);
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
    assert_eq!(item.phase(), QueuePhase::Pending);
}

/// HITL item can be resolved with Skip action.
#[tokio::test]
async fn hitl_respond_skip() {
    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let mut daemon = setup_daemon(&tmp, source, vec![]);

    let mut item = test_item("github:org/repo#1", "analyze");
    item.set_phase_unchecked(QueuePhase::Running);
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
    assert_eq!(item.phase(), QueuePhase::Skipped);
}

/// Failed items have worktree_preserved flag set.
#[tokio::test]
async fn failed_items_preserve_worktree() {
    let tmp = TempDir::new().unwrap();
    let source = MockDataSource::new("github");
    let mut daemon = setup_daemon(&tmp, source, vec![]);

    let mut item = test_item("github:org/repo#1", "analyze");
    item.set_phase_unchecked(QueuePhase::Running);
    item.updated_at = chrono::Utc::now().to_rfc3339();
    daemon.push_item(item);

    daemon
        .mark_failed("github:org/repo#1:analyze", "test failure".to_string())
        .unwrap();

    let item = daemon.get_item("github:org/repo#1:analyze").unwrap();
    assert_eq!(item.phase(), QueuePhase::Failed);
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
    item.set_phase_unchecked(QueuePhase::Running);
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
    assert_eq!(item.phase(), QueuePhase::Pending);
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
// ---------------------------------------------------------------------------
// DB-owned escalation results: derivation, lineage failure count, hooks
// ---------------------------------------------------------------------------

mod store_results {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use belt_core::escalation::EscalationPolicy;
    use belt_core::hitl::HitlId;
    use belt_core::hitl::HitlStatus;
    use belt_core::lifecycle::{HookContext, LifecycleHook};
    use belt_core::transition::{Actor, TransitionReason, TransitionRequest};
    use belt_daemon::cron::{CronContext, CronHandler, LogCleanupJob};
    use belt_infra::db::{DeriveKind, DeriveOutcome, DeriveRequest, NewItem};
    use belt_infra::worktree::WorktreeManager;

    use super::*;

    const ORIGIN: &str = "github:org/repo#1:analyze";

    /// `(from, to, reason)` of every phase entry, oldest first.
    fn phase_enters(daemon: &Daemon, work_id: &str) -> Vec<(String, String, String)> {
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
                    e.reason.unwrap(),
                )
            })
            .collect()
    }

    fn enter(from: &str, to: &str, reason: &str) -> (String, String, String) {
        (from.to_string(), to.to_string(), reason.to_string())
    }

    fn derived(n: u32) -> String {
        format!("{ORIGIN}:{n}")
    }

    fn daemon_over(
        tmp: &TempDir,
        exit_codes: Vec<i32>,
        config: WorkspaceConfig,
        db: Database,
        with_item: bool,
    ) -> Daemon {
        let mut source = MockDataSource::new("github");
        if with_item {
            source.add_item(test_item("github:org/repo#1", "analyze"));
        }
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(MockRuntime::new("mock", exit_codes)));
        Daemon::new(
            config,
            vec![Box::new(source)],
            Arc::new(registry),
            Box::new(MockWorktreeManager::new(tmp.path().join("worktrees"))),
            4,
            db,
        )
    }

    fn failing_daemon(tmp: &TempDir, exit_codes: Vec<i32>, config: WorkspaceConfig) -> Daemon {
        daemon_over(
            tmp,
            exit_codes,
            config,
            Database::open_in_memory().unwrap(),
            true,
        )
    }

    /// Hook that records each call together with the stored phase at call time.
    struct RecordingHook {
        db: Arc<Database>,
        calls: Mutex<Vec<String>>,
        worktrees: Mutex<Vec<(String, PathBuf)>>,
        /// When set, on_enter moves the stored row to Skipped behind the
        /// daemon's back, so the result transition conflicts.
        skip_on_enter: bool,
    }

    impl RecordingHook {
        fn new(db: Arc<Database>) -> Arc<Self> {
            Self::build(db, false)
        }

        fn skipping_on_enter(db: Arc<Database>) -> Arc<Self> {
            Self::build(db, true)
        }

        fn build(db: Arc<Database>, skip_on_enter: bool) -> Arc<Self> {
            Arc::new(Self {
                db,
                calls: Mutex::new(Vec::new()),
                worktrees: Mutex::new(Vec::new()),
                skip_on_enter,
            })
        }

        fn stored_phase(&self, work_id: &str) -> String {
            self.db
                .get_item(work_id)
                .unwrap()
                .phase()
                .as_str()
                .to_string()
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn worktree_of(&self, work_id: &str) -> PathBuf {
            self.worktrees
                .lock()
                .unwrap()
                .iter()
                .find(|(w, _)| w == work_id)
                .map(|(_, p)| p.clone())
                .unwrap_or_else(|| panic!("{work_id} never entered Running"))
        }
    }

    #[async_trait]
    impl LifecycleHook for RecordingHook {
        async fn on_enter(&self, ctx: &HookContext) -> anyhow::Result<()> {
            self.worktrees
                .lock()
                .unwrap()
                .push((ctx.work_id.clone(), ctx.worktree.clone()));
            if self.skip_on_enter {
                self.db.update_phase(&ctx.work_id, QueuePhase::Skipped)?;
            }
            Ok(())
        }

        async fn on_done(&self, _ctx: &HookContext) -> anyhow::Result<()> {
            Ok(())
        }

        async fn on_fail(&self, ctx: &HookContext) -> anyhow::Result<()> {
            let phase = self.stored_phase(&ctx.work_id);
            self.calls
                .lock()
                .unwrap()
                .push(format!("on_fail {} {phase}", ctx.work_id));
            Ok(())
        }

        async fn on_escalation(
            &self,
            ctx: &HookContext,
            action: EscalationAction,
        ) -> anyhow::Result<()> {
            let phase = self.stored_phase(&ctx.work_id);
            self.calls
                .lock()
                .unwrap()
                .push(format!("on_escalation {} {action} {phase}", ctx.work_id));
            Ok(())
        }
    }

    async fn run_once(daemon: &mut Daemon) -> ItemOutcome {
        daemon.advance();
        let mut outcomes = daemon.execute_running().await;
        assert_eq!(outcomes.len(), 1, "exactly one item runs per step");
        outcomes.remove(0)
    }

    fn escalation_of(outcome: &ItemOutcome) -> EscalationAction {
        match outcome {
            ItemOutcome::Failed { escalation, .. } => *escalation,
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retry_ends_the_origin_and_derives_a_pending_item() {
        let tmp = TempDir::new().unwrap();
        let daemon = failing_daemon(&tmp, vec![1, 0], test_workspace_config());
        let hook = RecordingHook::new(Arc::clone(daemon.database()));
        let mut daemon = daemon.with_hook(hook.clone());

        daemon.collect().await.unwrap();
        let outcome = run_once(&mut daemon).await;
        assert_eq!(escalation_of(&outcome), EscalationAction::Retry);

        assert_eq!(
            phase_enters(&daemon, ORIGIN),
            vec![
                enter("pending", "ready", "advance"),
                enter("ready", "running", "advance"),
                enter("running", "skipped", "derived"),
            ],
            "the origin ends as Skipped (derived), it is not reused"
        );
        let next = daemon.db().get_item(&derived(2)).unwrap();
        assert_eq!(next.phase(), QueuePhase::Pending);
        assert_eq!(next.derived_from.as_deref(), Some(ORIGIN));
        assert_eq!(next.lineage_root, ORIGIN);
        assert!(daemon.get_item(ORIGIN).is_none());
        assert_eq!(
            daemon.get_item(&derived(2)).map(|i| i.phase()),
            Some(QueuePhase::Pending)
        );

        // The derived item runs in the worktree handed over from the origin.
        run_once(&mut daemon).await;
        assert_eq!(hook.worktree_of(&derived(2)), hook.worktree_of(ORIGIN));
    }

    #[tokio::test]
    async fn on_escalation_runs_after_the_result_is_committed() {
        let tmp = TempDir::new().unwrap();
        let daemon = failing_daemon(&tmp, vec![1, 1, 1], test_workspace_config());
        let hook = RecordingHook::new(Arc::clone(daemon.database()));
        let mut daemon = daemon.with_hook(hook.clone());

        daemon.collect().await.unwrap();
        run_once(&mut daemon).await;
        run_once(&mut daemon).await;
        run_once(&mut daemon).await;

        assert_eq!(
            hook.calls(),
            vec![
                // retry: no on_fail
                format!("on_escalation {ORIGIN} retry skipped"),
                format!("on_escalation {} retry_with_comment skipped", derived(2)),
                format!("on_fail {} skipped", derived(2)),
                format!("on_escalation {} hitl hitl", derived(3)),
                format!("on_fail {} hitl", derived(3)),
            ]
        );
    }

    #[tokio::test]
    async fn conflicting_result_transition_calls_no_hook() {
        let tmp = TempDir::new().unwrap();
        let daemon = failing_daemon(&tmp, vec![1], test_workspace_config());
        let hook = RecordingHook::skipping_on_enter(Arc::clone(daemon.database()));
        let mut daemon = daemon.with_hook(hook.clone());

        daemon.collect().await.unwrap();
        let outcome = run_once(&mut daemon).await;

        assert!(
            matches!(
                outcome,
                ItemOutcome::Conflicted {
                    current: QueuePhase::Skipped,
                    ..
                }
            ),
            "got {outcome:?}"
        );
        assert!(hook.calls().is_empty(), "no hook after a conflict");
        assert!(matches!(
            daemon.db().get_item(&derived(2)),
            Err(belt_core::error::BeltError::ItemNotFound(_))
        ));
    }

    #[tokio::test]
    async fn lineage_failures_climb_the_ladder_to_one_open_hitl_request() {
        let tmp = TempDir::new().unwrap();
        let mut daemon = failing_daemon(&tmp, vec![1, 1, 1], test_workspace_config());

        daemon.collect().await.unwrap();
        let ladder: Vec<EscalationAction> = [
            run_once(&mut daemon).await,
            run_once(&mut daemon).await,
            run_once(&mut daemon).await,
        ]
        .iter()
        .map(escalation_of)
        .collect();
        assert_eq!(
            ladder,
            vec![
                EscalationAction::Retry,
                EscalationAction::RetryWithComment,
                EscalationAction::Hitl,
            ]
        );

        for done in [ORIGIN.to_string(), derived(2)] {
            assert_eq!(
                daemon.db().get_item(&done).unwrap().phase(),
                QueuePhase::Skipped
            );
        }
        let last = derived(3);
        assert_eq!(
            daemon.db().get_item(&last).unwrap().phase(),
            QueuePhase::Hitl
        );
        assert_eq!(daemon.db().failure_count(&last).unwrap(), 3);

        let hitl_entries: Vec<_> = [ORIGIN.to_string(), derived(2), last.clone()]
            .iter()
            .flat_map(|w| daemon.db().transitions_of(w).unwrap())
            .filter(|e| e.to_phase.as_deref() == Some("hitl"))
            .collect();
        assert_eq!(hitl_entries.len(), 1, "one Hitl entry in the lineage");
        let request = daemon
            .db()
            .hitl_request(&HitlId::new(format!("hitl-{}", hitl_entries[0].seq)))
            .unwrap()
            .expect("the request is opened with the transition");
        assert_eq!(request.status, HitlStatus::Open);
        assert_eq!(request.work_id, last);
        assert_eq!(request.reason, Some(HitlReason::RetryMaxExceeded));
        assert_eq!(daemon.items_in_phase(QueuePhase::Hitl).len(), 1);
    }

    #[tokio::test]
    async fn failure_count_survives_a_restart() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("belt.db");
        let path = path.to_str().unwrap();

        {
            let mut daemon = daemon_over(
                &tmp,
                vec![1],
                test_workspace_config(),
                Database::open(path).unwrap(),
                true,
            );
            daemon.collect().await.unwrap();
            let outcome = run_once(&mut daemon).await;
            assert_eq!(escalation_of(&outcome), EscalationAction::Retry);
        }

        let mut daemon = daemon_over(
            &tmp,
            vec![1],
            test_workspace_config(),
            Database::open(path).unwrap(),
            false,
        );
        daemon.restore_from_store().unwrap();
        let outcome = run_once(&mut daemon).await;
        assert_eq!(
            escalation_of(&outcome),
            EscalationAction::RetryWithComment,
            "the second failure of the lineage is level 2 after a restart"
        );
    }

    #[tokio::test]
    async fn failures_past_the_highest_level_reuse_it() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_workspace_config();
        config.sources.get_mut("github").unwrap().escalation =
            EscalationPolicy::new(BTreeMap::from([
                (1, EscalationAction::Retry),
                (2, EscalationAction::RetryWithComment),
            ]));
        let mut daemon = failing_daemon(&tmp, vec![1, 1, 1], config);

        daemon.collect().await.unwrap();
        run_once(&mut daemon).await;
        run_once(&mut daemon).await;
        let outcome = run_once(&mut daemon).await;

        assert_eq!(escalation_of(&outcome), EscalationAction::RetryWithComment);
        assert_eq!(
            daemon.db().get_item(&derived(4)).unwrap().phase(),
            QueuePhase::Pending
        );
    }

    #[tokio::test]
    async fn skip_escalation_moves_running_to_skipped() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_workspace_config();
        config.sources.get_mut("github").unwrap().escalation =
            EscalationPolicy::new(BTreeMap::from([(1, EscalationAction::Skip)]));
        let mut daemon = failing_daemon(&tmp, vec![1], config);

        daemon.collect().await.unwrap();
        let outcome = run_once(&mut daemon).await;

        assert_eq!(escalation_of(&outcome), EscalationAction::Skip);
        assert_eq!(
            phase_enters(&daemon, ORIGIN),
            vec![
                enter("pending", "ready", "advance"),
                enter("ready", "running", "advance"),
                enter("running", "skipped", "escalation:skip"),
            ]
        );
        assert!(matches!(
            daemon.db().get_item(&derived(2)),
            Err(belt_core::error::BeltError::ItemNotFound(_))
        ));
    }

    #[tokio::test]
    async fn unreadable_failure_history_surfaces_instead_of_falling_back() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("belt.db");
        let path = path.to_str().unwrap();
        let mut daemon = daemon_over(
            &tmp,
            vec![1, 1],
            test_workspace_config(),
            Database::open(path).unwrap(),
            true,
        );

        daemon.collect().await.unwrap();
        run_once(&mut daemon).await;

        // Corrupt the stored attempt history the stagnation check reads.
        let raw = rusqlite::Connection::open(path).unwrap();
        raw.execute("UPDATE history SET attempt = 'not-a-number'", [])
            .unwrap();
        drop(raw);

        let outcome = run_once(&mut daemon).await;
        assert!(
            matches!(outcome, ItemOutcome::StoreError { .. }),
            "got {outcome:?}"
        );
        assert_eq!(
            daemon.db().get_item(&derived(2)).unwrap().phase(),
            QueuePhase::Running,
            "no escalation is committed on a store read error"
        );
        assert!(matches!(
            daemon.db().get_item(&derived(3)),
            Err(belt_core::error::BeltError::ItemNotFound(_))
        ));
    }

    // -----------------------------------------------------------------------
    // log-cleanup follows the worktree owner
    // -----------------------------------------------------------------------

    fn claim(db: &Database, work_id: &str, from: QueuePhase, to: QueuePhase) {
        db.transition(&TransitionRequest {
            work_id: work_id.to_string(),
            expected_from: from,
            to,
            actor: Actor::Daemon,
            reason: TransitionReason::Advance,
            detail: None,
        })
        .unwrap();
    }

    #[test]
    fn log_cleanup_keeps_a_handed_over_worktree_until_its_owner_finishes() {
        let tmp = TempDir::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let worktrees: Arc<dyn WorktreeManager> =
            Arc::new(MockWorktreeManager::new(tmp.path().to_path_buf()));

        db.insert_collected(&NewItem {
            source_id: "github:org/repo#1".to_string(),
            workspace_id: "test-ws".to_string(),
            state: "analyze".to_string(),
            title: None,
            actor: Actor::Daemon,
        })
        .unwrap();
        claim(&db, ORIGIN, QueuePhase::Pending, QueuePhase::Ready);
        claim(&db, ORIGIN, QueuePhase::Ready, QueuePhase::Running);
        worktrees.create_or_reuse(ORIGIN).unwrap();
        let outcome = db
            .derive(&DeriveRequest {
                work_id: ORIGIN.to_string(),
                expected_from: QueuePhase::Running,
                kind: DeriveKind::EscalationRetry,
                actor: Actor::Daemon,
                reason: TransitionReason::Derived,
                detail: None,
            })
            .unwrap();
        assert_eq!(
            outcome,
            DeriveOutcome::Derived {
                work_id: derived(2)
            }
        );

        let job = LogCleanupJob::new(Arc::clone(&db), Arc::clone(&worktrees));
        let past_ttl = CronContext {
            now: chrono::Utc::now() + chrono::Duration::days(30),
        };

        job.execute(&past_ttl).unwrap();
        assert!(
            worktrees.exists(ORIGIN),
            "the Skipped origin no longer owns the handed-over worktree"
        );

        db.transition(&TransitionRequest {
            work_id: derived(2),
            expected_from: QueuePhase::Pending,
            to: QueuePhase::Skipped,
            actor: Actor::Cli,
            reason: TransitionReason::Manual,
            detail: None,
        })
        .unwrap();
        job.execute(&past_ttl).unwrap();
        assert!(
            !worktrees.exists(ORIGIN),
            "the worktree goes once its owner is finished"
        );
    }
}
