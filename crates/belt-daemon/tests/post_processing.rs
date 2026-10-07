//! HITL post-processing: the daemon alone applies a confirmed (or expired)
//! HITL request. Each action ends in its result phase with the right
//! worktree, history and `on_hitl_resolved` call; post-processing is
//! at-least-once and gives up to Failed after a bounded number of failed
//! result transitions.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use belt_core::escalation::EscalationAction;
use belt_core::hitl::{ConfirmPath, HitlAction, HitlId};
use belt_core::lifecycle::{HookContext, LifecycleHook};
use belt_core::phase::QueuePhase;
use belt_core::queue::{HitlReason, QueueItem};
use belt_core::runtime::RuntimeRegistry;
use belt_core::transition::{Actor, TransitionOutcome, TransitionReason, TransitionRequest};
use belt_core::workspace::WorkspaceConfig;
use belt_daemon::daemon::Daemon;
use belt_daemon::hitl::{HitlResponse, HitlService};
use belt_daemon::post_processing::{POST_PROCESSING_FAILURE_LIMIT, REPLAN_LIMIT};
use belt_infra::db::{
    CollectOutcome, Database, HistoryEvent, HitlTarget, NewItem, OpenHitlOutcome, OpenHitlRequest,
    TransitionLogEntry, transition_kind,
};
use belt_infra::runtimes::mock::MockRuntime;
use belt_infra::sources::mock::MockDataSource;
use belt_infra::worktree::MockWorktreeManager;
use tempfile::TempDir;

const SOURCE: &str = "github:org/repo#1";
const STATE: &str = "analyze";

fn config(on_done: &str) -> WorkspaceConfig {
    let yaml = format!(
        r#"
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
          - script: "{on_done}"
    escalation:
      1: retry
      2: retry_with_comment
      3: hitl
      terminal: skip
"#
    );
    serde_yaml::from_str(&yaml).unwrap()
}

fn daemon_over(tmp: &TempDir, on_done: &str, db: Database) -> Daemon {
    let mut registry = RuntimeRegistry::new("mock".to_string());
    registry.register(Arc::new(MockRuntime::new("mock", vec![])));
    Daemon::new(
        config(on_done),
        vec![Box::new(MockDataSource::new("github"))],
        Arc::new(registry),
        Box::new(MockWorktreeManager::new(tmp.path().join("worktrees"))),
        4,
        db,
    )
}

fn daemon(tmp: &TempDir, on_done: &str) -> Daemon {
    daemon_over(tmp, on_done, Database::open_in_memory().unwrap())
}

/// One `on_hitl_resolved` call: the item, the verdict, and the stored phase
/// at call time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolved {
    work_id: String,
    action: HitlAction,
    stored_phase: QueuePhase,
}

struct ResolvedHook {
    db: Arc<Database>,
    calls: Mutex<Vec<Resolved>>,
    fail: bool,
}

impl ResolvedHook {
    fn new(db: Arc<Database>, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            db,
            calls: Mutex::new(Vec::new()),
            fail,
        })
    }

    fn calls(&self) -> Vec<Resolved> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl LifecycleHook for ResolvedHook {
    async fn on_enter(&self, _ctx: &HookContext) -> anyhow::Result<()> {
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
    async fn on_hitl_resolved(&self, ctx: &HookContext, action: HitlAction) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(Resolved {
            work_id: ctx.work_id.clone(),
            action,
            stored_phase: self.db.get_item(&ctx.work_id)?.phase(),
        });
        if self.fail {
            anyhow::bail!("label api down");
        }
        Ok(())
    }
}

fn with_hook(daemon: Daemon, fail: bool) -> (Daemon, Arc<ResolvedHook>) {
    let hook = ResolvedHook::new(Arc::clone(daemon.database()), fail);
    (daemon.with_hook(hook.clone()), hook)
}

fn move_item(db: &Database, work_id: &str, from: QueuePhase, to: QueuePhase) {
    let outcome = db
        .transition(&TransitionRequest {
            work_id: work_id.to_string(),
            expected_from: from,
            to,
            actor: Actor::Daemon,
            reason: TransitionReason::Advance,
            detail: None,
        })
        .unwrap();
    assert!(
        matches!(outcome, TransitionOutcome::Applied { .. }),
        "{from:?} -> {to:?}: {outcome:?}"
    );
}

/// A Running item of `SOURCE` that failed into Hitl, with its worktree on
/// disk. Returns `(work_id, hitl_id)`.
fn hitl_item(daemon: &mut Daemon, terminal: Option<EscalationAction>) -> (String, HitlId) {
    let db = Arc::clone(daemon.database());
    let CollectOutcome::Inserted { work_id } = db
        .insert_collected(&NewItem {
            source_id: SOURCE.to_string(),
            workspace_id: "test-ws".to_string(),
            state: STATE.to_string(),
            title: None,
            actor: Actor::Daemon,
        })
        .unwrap()
    else {
        panic!("expected a new item");
    };
    move_item(&db, &work_id, QueuePhase::Pending, QueuePhase::Ready);
    move_item(&db, &work_id, QueuePhase::Ready, QueuePhase::Running);
    let OpenHitlOutcome::Opened { hitl_id, .. } = daemon
        .hitl()
        .open(&OpenHitlRequest {
            work_id: work_id.clone(),
            expected_from: QueuePhase::Running,
            reason: HitlReason::RetryMaxExceeded,
            notes: Some("handler kept failing".to_string()),
            actor: Actor::Daemon,
            transition_reason: TransitionReason::Escalation(EscalationAction::Hitl),
            timeout_at: None,
            terminal_action: terminal,
        })
        .unwrap()
    else {
        panic!("expected an open request");
    };
    daemon.worktree_mgr().create_or_reuse(&work_id).unwrap();
    daemon.restore_from_store().unwrap();
    (work_id, hitl_id)
}

fn respond(service: &HitlService, hitl_id: &HitlId, action: HitlAction, notes: Option<&str>) {
    let outcome = service
        .respond(&HitlResponse {
            target: HitlTarget::Id(hitl_id.clone()),
            action,
            by: "alice".to_string(),
            via: "cli".to_string(),
            path: ConfirmPath::Direct,
            notes: notes.map(str::to_string),
        })
        .unwrap();
    assert!(
        matches!(outcome, belt_core::hitl::RespondOutcome::Won { .. }),
        "{outcome:?}"
    );
}

fn stored_phase(daemon: &Daemon, work_id: &str) -> QueuePhase {
    daemon.db().get_item(work_id).unwrap().phase()
}

fn log_of(daemon: &Daemon, work_id: &str, kind: &str) -> Vec<TransitionLogEntry> {
    daemon
        .db()
        .transitions_of(work_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .collect()
}

/// The last phase entry of `work_id`: `(from, to, reason)`.
fn last_enter(daemon: &Daemon, work_id: &str) -> (String, String, String) {
    let entry = log_of(daemon, work_id, transition_kind::PHASE_ENTER)
        .pop()
        .expect("a phase entry");
    (
        entry.from_phase.unwrap(),
        entry.to_phase.unwrap(),
        entry.reason.unwrap(),
    )
}

fn processed(daemon: &Daemon, hitl_id: &HitlId) -> bool {
    daemon
        .db()
        .hitl_request(hitl_id)
        .unwrap()
        .unwrap()
        .post_processed_at
        .is_some()
}

fn failed_attempt(db: &Database, work_id: &str) {
    db.append_history(&HistoryEvent {
        work_id: work_id.to_string(),
        source_id: SOURCE.to_string(),
        state: STATE.to_string(),
        status: "failed".to_string(),
        attempt: 1,
        summary: None,
        error: Some("compile error in foo.rs".to_string()),
        created_at: chrono::Utc::now().to_rfc3339(),
    })
    .unwrap();
}

/// Another open item of the same `(source_id, state)`: deriving a replan
/// item fails while it exists.
fn block_derivation(db: &Database) -> String {
    let blocker = format!("{SOURCE}:{STATE}:9");
    db.insert_item(&QueueItem::new(
        blocker.clone(),
        SOURCE.to_string(),
        "other-ws".to_string(),
        STATE.to_string(),
    ))
    .unwrap();
    blocker
}

fn derived_items(daemon: &Daemon, origin: &str) -> Vec<QueueItem> {
    daemon
        .db()
        .list_items(None, None)
        .unwrap()
        .into_iter()
        .filter(|i| i.derived_from.as_deref() == Some(origin))
        .collect()
}

// ---- done ------------------------------------------------------------------

#[tokio::test]
async fn done_runs_on_done_then_ends_done_and_cleans_the_worktree() {
    let tmp = TempDir::new().unwrap();
    let (mut daemon, hook) = with_hook(daemon(&tmp, "echo done"), false);
    let (work_id, hitl_id) = hitl_item(&mut daemon, None);
    respond(daemon.hitl(), &hitl_id, HitlAction::Done, None);

    assert_eq!(daemon.run_post_processing().await.unwrap(), 1);

    assert_eq!(stored_phase(&daemon, &work_id), QueuePhase::Done);
    assert_eq!(
        last_enter(&daemon, &work_id),
        (
            "hitl".to_string(),
            "done".to_string(),
            "post_processing:done".to_string()
        )
    );
    assert!(processed(&daemon, &hitl_id));
    assert!(!daemon.worktree_mgr().exists(&work_id));
    assert_eq!(
        hook.calls(),
        vec![Resolved {
            work_id: work_id.clone(),
            action: HitlAction::Done,
            stored_phase: QueuePhase::Hitl,
        }],
        "on_hitl_resolved runs once, before the result transition"
    );
    assert!(daemon.get_item(&work_id).is_none(), "Done leaves the queue");
}

#[tokio::test]
async fn done_with_a_failing_on_done_ends_failed_and_keeps_the_worktree() {
    let tmp = TempDir::new().unwrap();
    let (mut daemon, hook) = with_hook(daemon(&tmp, "exit 1"), false);
    let (work_id, hitl_id) = hitl_item(&mut daemon, None);
    respond(daemon.hitl(), &hitl_id, HitlAction::Done, None);

    daemon.run_post_processing().await.unwrap();

    assert_eq!(stored_phase(&daemon, &work_id), QueuePhase::Failed);
    let (from, to, reason) = last_enter(&daemon, &work_id);
    assert_eq!(
        (from.as_str(), to.as_str(), reason.as_str()),
        ("hitl", "failed", "post_processing:done")
    );
    assert!(processed(&daemon, &hitl_id));
    assert!(daemon.worktree_mgr().exists(&work_id));
    assert_eq!(
        hook.calls().len(),
        1,
        "on_hitl_resolved runs on this path too"
    );
    assert_eq!(hook.calls()[0].stored_phase, QueuePhase::Hitl);
    assert_eq!(
        daemon.get_item(&work_id).unwrap().phase(),
        QueuePhase::Failed
    );
}

// ---- retry -----------------------------------------------------------------

#[tokio::test]
async fn retry_returns_the_same_item_to_pending_with_the_instruction_and_a_reset() {
    let tmp = TempDir::new().unwrap();
    let (mut daemon, hook) = with_hook(daemon(&tmp, "echo done"), false);
    let (work_id, hitl_id) = hitl_item(&mut daemon, None);
    for _ in 0..3 {
        failed_attempt(daemon.db(), &work_id);
    }
    assert_eq!(daemon.db().failure_count(&work_id).unwrap(), 3);
    respond(
        daemon.hitl(),
        &hitl_id,
        HitlAction::Retry,
        Some("use the builder API instead"),
    );

    daemon.run_post_processing().await.unwrap();

    assert_eq!(stored_phase(&daemon, &work_id), QueuePhase::Pending);
    assert_eq!(
        last_enter(&daemon, &work_id).2,
        "post_processing:retry".to_string()
    );
    assert_eq!(
        daemon.db().failure_count(&work_id).unwrap(),
        0,
        "HITL retry is a reset point"
    );
    assert!(derived_items(&daemon, &work_id).is_empty(), "no derivation");
    assert!(daemon.worktree_mgr().exists(&work_id), "worktree kept");
    let copy = daemon.get_item(&work_id).unwrap();
    assert_eq!(copy.phase(), QueuePhase::Pending);
    assert!(
        copy.lateral_plan
            .as_deref()
            .is_some_and(|p| p.contains("use the builder API instead")),
        "the instruction is injected: {:?}",
        copy.lateral_plan
    );
    assert_eq!(hook.calls().len(), 1);
    assert_eq!(hook.calls()[0].action, HitlAction::Retry);
    assert_eq!(hook.calls()[0].stored_phase, QueuePhase::Hitl);
}

// ---- skip ------------------------------------------------------------------

#[tokio::test]
async fn skip_cleans_the_worktree_and_ends_skipped() {
    let tmp = TempDir::new().unwrap();
    let (mut daemon, hook) = with_hook(daemon(&tmp, "echo done"), false);
    let (work_id, hitl_id) = hitl_item(&mut daemon, None);
    respond(daemon.hitl(), &hitl_id, HitlAction::Skip, None);

    daemon.run_post_processing().await.unwrap();

    assert_eq!(stored_phase(&daemon, &work_id), QueuePhase::Skipped);
    assert_eq!(
        last_enter(&daemon, &work_id).2,
        "post_processing:skip".to_string()
    );
    assert!(!daemon.worktree_mgr().exists(&work_id));
    assert!(processed(&daemon, &hitl_id));
    assert_eq!(hook.calls().len(), 1);
    assert_eq!(hook.calls()[0].stored_phase, QueuePhase::Hitl);
    assert!(daemon.get_item(&work_id).is_none());
}

// ---- replan ----------------------------------------------------------------

#[tokio::test]
async fn replan_within_the_limit_derives_a_pending_item_with_the_failure_context() {
    let tmp = TempDir::new().unwrap();
    let (mut daemon, hook) = with_hook(daemon(&tmp, "echo done"), false);
    let (work_id, hitl_id) = hitl_item(&mut daemon, None);
    failed_attempt(daemon.db(), &work_id);
    respond(
        daemon.hitl(),
        &hitl_id,
        HitlAction::Replan,
        Some("split the change in two"),
    );

    daemon.run_post_processing().await.unwrap();

    assert_eq!(stored_phase(&daemon, &work_id), QueuePhase::Skipped);
    assert_eq!(
        last_enter(&daemon, &work_id).2,
        "post_processing:replan".to_string()
    );
    let derived = derived_items(&daemon, &work_id);
    assert_eq!(derived.len(), 1);
    let derived = &derived[0];
    assert_eq!(derived.phase(), QueuePhase::Pending);
    assert_eq!(derived.state, STATE);
    assert_eq!(derived.replan_count, 1, "one replan in the lineage");
    assert_eq!(daemon.db().failure_count(&derived.work_id).unwrap(), 0);
    assert_eq!(
        daemon.db().worktree_key(&derived.work_id).unwrap(),
        derived.work_id,
        "a new worktree"
    );
    assert!(!daemon.worktree_mgr().exists(&work_id), "origin cleaned");
    assert!(processed(&daemon, &hitl_id));
    let copy = daemon
        .get_item(&derived.work_id)
        .expect("derived is queued");
    let plan = copy.lateral_plan.as_deref().unwrap_or_default();
    assert!(plan.contains("split the change in two"), "{plan}");
    assert!(plan.contains("handler kept failing"), "{plan}");
    assert!(plan.contains("compile error in foo.rs"), "{plan}");
    assert!(daemon.get_item(&work_id).is_none());
    assert_eq!(hook.calls().len(), 1);
    assert_eq!(hook.calls()[0].action, HitlAction::Replan);
    assert_eq!(hook.calls()[0].stored_phase, QueuePhase::Hitl);
}

#[tokio::test]
async fn replan_after_three_lineage_replans_ends_failed_and_keeps_the_worktree() {
    let tmp = TempDir::new().unwrap();
    let (mut daemon, hook) = with_hook(daemon(&tmp, "echo done"), false);
    let (work_id, hitl_id) = hitl_item(&mut daemon, None);
    for _ in 0..REPLAN_LIMIT {
        daemon.db().increment_replan_count(&work_id).unwrap();
    }
    respond(daemon.hitl(), &hitl_id, HitlAction::Replan, None);

    daemon.run_post_processing().await.unwrap();

    assert_eq!(stored_phase(&daemon, &work_id), QueuePhase::Failed);
    assert_eq!(
        last_enter(&daemon, &work_id).2,
        "post_processing:replan".to_string()
    );
    assert!(derived_items(&daemon, &work_id).is_empty());
    assert!(daemon.worktree_mgr().exists(&work_id));
    assert!(processed(&daemon, &hitl_id));
    assert_eq!(hook.calls().len(), 1);
    assert_eq!(hook.calls()[0].stored_phase, QueuePhase::Hitl);
}

#[tokio::test]
async fn the_replan_limit_counts_the_whole_lineage() {
    let tmp = TempDir::new().unwrap();
    let mut daemon = daemon(&tmp, "echo done");
    let (mut current, mut hitl_id) = hitl_item(&mut daemon, None);
    let db = Arc::clone(daemon.database());

    for n in 1..=REPLAN_LIMIT {
        respond(daemon.hitl(), &hitl_id, HitlAction::Replan, None);
        daemon.run_post_processing().await.unwrap();
        let derived = derived_items(&daemon, &current);
        assert_eq!(derived.len(), 1, "replan #{n} derives");
        assert_eq!(derived[0].replan_count, n);
        current = derived[0].work_id.clone();
        move_item(&db, &current, QueuePhase::Pending, QueuePhase::Ready);
        move_item(&db, &current, QueuePhase::Ready, QueuePhase::Running);
        let OpenHitlOutcome::Opened { hitl_id: next, .. } = daemon
            .hitl()
            .open(&OpenHitlRequest {
                work_id: current.clone(),
                expected_from: QueuePhase::Running,
                reason: HitlReason::RetryMaxExceeded,
                notes: None,
                actor: Actor::Daemon,
                transition_reason: TransitionReason::Escalation(EscalationAction::Hitl),
                timeout_at: None,
                terminal_action: None,
            })
            .unwrap()
        else {
            panic!("expected an open request");
        };
        hitl_id = next;
    }

    respond(daemon.hitl(), &hitl_id, HitlAction::Replan, None);
    daemon.run_post_processing().await.unwrap();
    assert_eq!(stored_phase(&daemon, &current), QueuePhase::Failed);
    assert!(derived_items(&daemon, &current).is_empty());
}

// ---- expired ---------------------------------------------------------------

#[tokio::test]
async fn expired_with_terminal_skip_ends_skipped() {
    let tmp = TempDir::new().unwrap();
    let (mut daemon, hook) = with_hook(daemon(&tmp, "echo done"), false);
    let (work_id, hitl_id) = hitl_item(&mut daemon, Some(EscalationAction::Skip));
    daemon
        .hitl()
        .expire(&hitl_id, EscalationAction::Skip)
        .unwrap();

    daemon.run_post_processing().await.unwrap();

    assert_eq!(stored_phase(&daemon, &work_id), QueuePhase::Skipped);
    assert!(!daemon.worktree_mgr().exists(&work_id));
    assert!(processed(&daemon, &hitl_id));
    assert_eq!(hook.calls()[0].action, HitlAction::Skip);
}

#[tokio::test]
async fn expired_with_terminal_replan_derives_like_a_replan_response() {
    let tmp = TempDir::new().unwrap();
    let (mut daemon, hook) = with_hook(daemon(&tmp, "echo done"), false);
    let (work_id, hitl_id) = hitl_item(&mut daemon, Some(EscalationAction::Replan));
    daemon
        .hitl()
        .expire(&hitl_id, EscalationAction::Replan)
        .unwrap();

    daemon.run_post_processing().await.unwrap();

    assert_eq!(stored_phase(&daemon, &work_id), QueuePhase::Skipped);
    assert_eq!(derived_items(&daemon, &work_id).len(), 1);
    assert!(processed(&daemon, &hitl_id));
    assert_eq!(hook.calls()[0].action, HitlAction::Replan);
}

#[tokio::test]
async fn expired_with_terminal_replan_over_the_limit_ends_failed() {
    let tmp = TempDir::new().unwrap();
    let mut daemon = daemon(&tmp, "echo done");
    let (work_id, hitl_id) = hitl_item(&mut daemon, Some(EscalationAction::Replan));
    for _ in 0..REPLAN_LIMIT {
        daemon.db().increment_replan_count(&work_id).unwrap();
    }
    daemon
        .hitl()
        .expire(&hitl_id, EscalationAction::Replan)
        .unwrap();

    daemon.run_post_processing().await.unwrap();

    assert_eq!(stored_phase(&daemon, &work_id), QueuePhase::Failed);
    assert!(derived_items(&daemon, &work_id).is_empty());
    assert!(daemon.worktree_mgr().exists(&work_id));
}

// ---- failure handling ------------------------------------------------------

#[tokio::test]
async fn a_failing_on_hitl_resolved_is_logged_as_hook_and_the_transition_proceeds() {
    let tmp = TempDir::new().unwrap();
    let (mut daemon, hook) = with_hook(daemon(&tmp, "echo done"), true);
    let (work_id, hitl_id) = hitl_item(&mut daemon, None);
    respond(daemon.hitl(), &hitl_id, HitlAction::Skip, None);

    daemon.run_post_processing().await.unwrap();

    assert_eq!(stored_phase(&daemon, &work_id), QueuePhase::Skipped);
    assert_eq!(hook.calls().len(), 1);
    let hooks = log_of(&daemon, &work_id, transition_kind::HOOK);
    assert_eq!(hooks.len(), 1, "{hooks:?}");
    assert_eq!(hooks[0].reason.as_deref(), Some("on_hitl_resolved"));
    assert!(
        hooks[0]
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("label api down")),
        "{:?}",
        hooks[0].detail
    );
    assert_eq!(hooks[0].actor, "daemon");
}

#[tokio::test]
async fn a_failing_worktree_cleanup_is_a_post_processing_error_and_the_transition_proceeds() {
    let tmp = TempDir::new().unwrap();
    let mut daemon = daemon(&tmp, "echo done");
    let (work_id, hitl_id) = hitl_item(&mut daemon, None);
    // A plain file where the worktree directory should be: removing it fails.
    let path = daemon.worktree_mgr().path(&work_id);
    std::fs::remove_dir_all(&path).unwrap();
    std::fs::write(&path, "not a directory").unwrap();
    respond(daemon.hitl(), &hitl_id, HitlAction::Skip, None);

    daemon.run_post_processing().await.unwrap();

    assert_eq!(stored_phase(&daemon, &work_id), QueuePhase::Skipped);
    let errors = log_of(&daemon, &work_id, transition_kind::POST_PROCESSING_ERROR);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].reason.as_deref(), Some("worktree_cleanup"));
}

#[tokio::test]
async fn a_failing_result_transition_is_retried_and_gives_up_to_failed_after_the_limit() {
    let tmp = TempDir::new().unwrap();
    let mut daemon = daemon(&tmp, "echo done");
    let (work_id, hitl_id) = hitl_item(&mut daemon, None);
    block_derivation(daemon.db());
    respond(daemon.hitl(), &hitl_id, HitlAction::Replan, None);

    for attempt in 1..POST_PROCESSING_FAILURE_LIMIT {
        daemon.run_post_processing().await.unwrap();
        assert_eq!(
            stored_phase(&daemon, &work_id),
            QueuePhase::Hitl,
            "still retrying after attempt {attempt}"
        );
        assert!(!processed(&daemon, &hitl_id));
        assert_eq!(
            daemon
                .db()
                .hitl_request(&hitl_id)
                .unwrap()
                .unwrap()
                .post_processing_failures,
            attempt
        );
    }

    daemon.run_post_processing().await.unwrap();

    assert_eq!(stored_phase(&daemon, &work_id), QueuePhase::Failed);
    assert!(processed(&daemon, &hitl_id));
    assert!(derived_items(&daemon, &work_id).is_empty());
    let failed = log_of(&daemon, &work_id, transition_kind::POST_PROCESSING_FAILED);
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(daemon.worktree_mgr().exists(&work_id), "Failed keeps it");
    assert_eq!(
        daemon.get_item(&work_id).unwrap().phase(),
        QueuePhase::Failed
    );

    // Nothing is left to process.
    assert_eq!(daemon.run_post_processing().await.unwrap(), 0);
}

// ---- at-least-once ---------------------------------------------------------

#[tokio::test]
async fn an_unfinished_post_processing_runs_again_after_a_restart_and_derives_once() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("belt.db");
    let path = path.to_str().unwrap();

    let (work_id, hitl_id, blocker, first_calls) = {
        let (mut daemon, hook) = with_hook(
            daemon_over(&tmp, "echo done", Database::open(path).unwrap()),
            false,
        );
        let (work_id, hitl_id) = hitl_item(&mut daemon, None);
        let blocker = block_derivation(daemon.db());
        respond(daemon.hitl(), &hitl_id, HitlAction::Replan, None);
        // The hook ran, then the result transition failed: the daemon stops
        // here as if it crashed before marking the request done.
        daemon.run_post_processing().await.unwrap();
        assert!(!processed(&daemon, &hitl_id));
        (work_id, hitl_id, blocker, hook.calls().len())
    };
    assert_eq!(first_calls, 1);

    Database::open(path)
        .unwrap()
        .update_phase(&blocker, QueuePhase::Done)
        .unwrap();

    let (mut daemon, hook) = with_hook(
        daemon_over(&tmp, "echo done", Database::open(path).unwrap()),
        false,
    );
    daemon.restore_from_store().unwrap();
    daemon.run_post_processing().await.unwrap();
    daemon.run_post_processing().await.unwrap();

    assert_eq!(stored_phase(&daemon, &work_id), QueuePhase::Skipped);
    assert!(processed(&daemon, &hitl_id));
    assert_eq!(
        derived_items(&daemon, &work_id).len(),
        1,
        "the derivation happens once"
    );
    assert_eq!(hook.calls().len(), 1, "on_hitl_resolved runs again once");
}

// ---- the daemon picks up responses from other processes -------------------

#[tokio::test]
async fn a_response_from_another_connection_is_applied_on_the_next_tick() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("belt.db");
    let path = path.to_str().unwrap();
    let mut daemon = daemon_over(&tmp, "echo done", Database::open(path).unwrap());
    let (work_id, _) = hitl_item(&mut daemon, None);
    daemon.tick().await.unwrap();
    assert_eq!(daemon.get_item(&work_id).unwrap().phase(), QueuePhase::Hitl);

    // `belt hitl respond` in another process.
    let cli = HitlService::new(Arc::new(Database::open(path).unwrap()));
    let outcome = cli
        .respond(&HitlResponse {
            target: HitlTarget::Item(work_id.clone()),
            action: HitlAction::Skip,
            by: "bob".to_string(),
            via: "cli".to_string(),
            path: ConfirmPath::Direct,
            notes: None,
        })
        .unwrap();
    assert!(matches!(
        outcome,
        belt_core::hitl::RespondOutcome::Won { .. }
    ));
    assert_eq!(
        stored_phase(&daemon, &work_id),
        QueuePhase::Hitl,
        "the response alone does not leave Hitl"
    );

    daemon.tick().await.unwrap();

    assert_eq!(stored_phase(&daemon, &work_id), QueuePhase::Skipped);
    assert!(
        daemon.get_item(&work_id).is_none(),
        "the in-memory copy follows the store"
    );
}
