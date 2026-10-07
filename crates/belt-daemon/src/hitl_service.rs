//! HitlService — handles HITL (Human-In-The-Loop) escalation logic.
//!
//! Extracted from [`Daemon`] to improve modularity and testability.
//! Provides [`HitlService::handle_escalation`] which routes escalation
//! actions (Retry, Skip, Hitl, Replan) and
//! [`HitlService::build_lateral_hitl_notes`] which assembles lateral
//! thinking history for human reviewers.

use std::collections::VecDeque;
use std::sync::Arc;

use chrono::Utc;

use belt_core::error::BeltError;
use belt_core::escalation::EscalationAction;
use belt_core::lifecycle::{HookContext, LifecycleHook};
use belt_core::phase::QueuePhase;
use belt_core::queue::{HitlReason, QueueItem};
use belt_core::transition::{Actor, TransitionOutcome, TransitionReason, TransitionRequest};
use belt_infra::db::{Database, OpenHitlOutcome, OpenHitlRequest, TransitionEvent};
use belt_infra::worktree::WorktreeManager;

/// Spawn an async hook task if a Tokio runtime is available.
///
/// When called from a synchronous context (e.g. unit tests without a runtime),
/// the task is silently dropped.
fn spawn_hook<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(fut);
    }
}

/// Phase an escalated execution leaves: escalation decides the fate of a failed run.
const ESCALATION_FROM: QueuePhase = QueuePhase::Running;

/// Result of committing an escalation decision to the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationCommit {
    /// The store applied the transition; hook and memory were updated.
    Applied,
    /// The stored phase differs from the expected one. Nothing else ran.
    Conflict { current: QueuePhase },
    /// The transition contract refused the request (busy or invalid action).
    Rejected(TransitionOutcome),
}

impl From<TransitionOutcome> for EscalationCommit {
    fn from(outcome: TransitionOutcome) -> Self {
        match outcome {
            TransitionOutcome::Applied { .. } => EscalationCommit::Applied,
            TransitionOutcome::Conflict { current } => EscalationCommit::Conflict { current },
            refused
            @ (TransitionOutcome::Busy { .. } | TransitionOutcome::InvalidAction { .. }) => {
                EscalationCommit::Rejected(refused)
            }
        }
    }
}

/// Drives HITL escalation logic for the daemon lifecycle.
///
/// Responsibilities:
/// 1. Route escalation actions (Retry, Skip, Hitl/Replan) to appropriate state transitions
/// 2. Build lateral thinking history notes for HITL items
/// 3. Fire lifecycle hooks on escalation
/// 4. Record transition events to the database
pub struct HitlService<'a> {
    queue: &'a mut VecDeque<QueueItem>,
    db: &'a Arc<Database>,
    hook: &'a Arc<dyn LifecycleHook>,
    worktree_mgr: &'a Arc<dyn WorktreeManager>,
}

impl<'a> HitlService<'a> {
    /// Create a new `HitlService` with borrowed daemon state.
    pub fn new(
        queue: &'a mut VecDeque<QueueItem>,
        db: &'a Arc<Database>,
        hook: &'a Arc<dyn LifecycleHook>,
        worktree_mgr: &'a Arc<dyn WorktreeManager>,
    ) -> Self {
        Self {
            queue,
            db,
            hook,
            worktree_mgr,
        }
    }

    /// Handle an escalation action for a queue item that failed while Running.
    ///
    /// The result transition is committed to the store first; the hook and the
    /// in-memory queue follow only when it was applied (spec: the transition
    /// commits before any hook runs). Until the derivation rules replace it,
    /// the transition is the interim mapping of each action:
    /// - `Retry`/`RetryWithComment`: the same row goes Running -> Pending
    /// - `Skip`: Running -> Skipped
    /// - `Hitl`/`Replan`: Running -> Hitl together with an open HITL request
    ///   carrying the lateral thinking notes
    ///
    /// # Errors
    /// `BeltError` when the store fails (I/O, unknown work_id). Nothing is
    /// changed in memory in that case.
    pub fn handle_escalation(
        &mut self,
        item: &mut QueueItem,
        action: EscalationAction,
        lateral_plan: Option<String>,
        hook_ctx: HookContext,
    ) -> Result<EscalationCommit, BeltError> {
        let hitl_notes = match action {
            EscalationAction::Hitl | EscalationAction::Replan => {
                Self::build_lateral_hitl_notes(self.db, &item.work_id, &lateral_plan)
            }
            EscalationAction::Retry
            | EscalationAction::RetryWithComment
            | EscalationAction::Skip => None,
        };
        match self.commit(item, action, hitl_notes.clone())? {
            EscalationCommit::Applied => {}
            refused @ (EscalationCommit::Conflict { .. } | EscalationCommit::Rejected(_)) => {
                return Ok(refused);
            }
        }

        // Lifecycle hook: on_escalation -- fire and forget, log only on failure.
        let hook = Arc::clone(self.hook);
        let esc_action = action;
        spawn_hook(async move {
            if let Err(e) = hook.on_escalation(&hook_ctx, esc_action).await {
                tracing::warn!(
                    work_id = hook_ctx.work_id,
                    "lifecycle hook on_escalation error (ignored): {e}"
                );
            }
        });

        let now = Utc::now().to_rfc3339();
        match action {
            EscalationAction::Retry | EscalationAction::RetryWithComment => {
                let mut retry_item = item.clone();
                retry_item.set_phase_unchecked(QueuePhase::Pending);
                retry_item.updated_at = now;
                // Carry over the preserved worktree path so the retry item
                // can reuse the existing working tree via create_or_reuse_with_previous.
                if item.worktree_preserved {
                    let prev_path = self.worktree_mgr.path(&item.work_id);
                    retry_item.previous_worktree_path =
                        Some(prev_path.to_string_lossy().into_owned());
                    tracing::info!(
                        work_id = %item.work_id,
                        ?prev_path,
                        "storing preserved worktree path for retry item"
                    );
                }
                retry_item.worktree_preserved = false;
                // Inject lateral plan into retry item when stagnation was detected.
                retry_item.lateral_plan = lateral_plan;
                self.queue.push_back(retry_item);
                // The returned item records the failed attempt; the retry lives on in the queue.
                item.set_phase_unchecked(QueuePhase::Failed);
            }
            EscalationAction::Skip => {
                item.set_phase_unchecked(QueuePhase::Skipped);
            }
            EscalationAction::Hitl | EscalationAction::Replan => {
                item.set_phase_unchecked(QueuePhase::Hitl);
                item.hitl_created_at = Some(now);
                item.hitl_reason = Some(HitlReason::RetryMaxExceeded);
                item.hitl_notes = hitl_notes;
                self.queue.push_back(item.clone());
            }
        }
        Ok(EscalationCommit::Applied)
    }

    /// Commit the store transition that realizes `action` for a Running item.
    fn commit(
        &self,
        item: &QueueItem,
        action: EscalationAction,
        hitl_notes: Option<String>,
    ) -> Result<EscalationCommit, BeltError> {
        let to = match action {
            EscalationAction::Retry | EscalationAction::RetryWithComment => QueuePhase::Pending,
            EscalationAction::Skip => QueuePhase::Skipped,
            EscalationAction::Hitl | EscalationAction::Replan => {
                let outcome = self.db.open_hitl(&OpenHitlRequest {
                    work_id: item.work_id.clone(),
                    expected_from: ESCALATION_FROM,
                    reason: HitlReason::RetryMaxExceeded,
                    notes: hitl_notes,
                    actor: Actor::Daemon,
                    transition_reason: TransitionReason::Escalation(action),
                    timeout_at: None,
                    terminal_action: None,
                })?;
                return Ok(match outcome {
                    OpenHitlOutcome::Opened { .. } => EscalationCommit::Applied,
                    OpenHitlOutcome::Rejected(refused) => EscalationCommit::from(refused),
                });
            }
        };
        let outcome = self.db.transition(&TransitionRequest {
            work_id: item.work_id.clone(),
            expected_from: ESCALATION_FROM,
            to,
            actor: Actor::Daemon,
            reason: TransitionReason::Escalation(action),
            detail: Some(format!("escalation: {action}")),
        })?;
        Ok(EscalationCommit::from(outcome))
    }

    /// Build hitl_notes markdown from lateral plan and stagnation events.
    ///
    /// When an item escalates to HITL, this attaches the full lateral thinking
    /// history so that human reviewers can see what automated approaches were
    /// already attempted.
    pub fn build_lateral_hitl_notes(
        db: &Database,
        work_id: &str,
        lateral_plan: &Option<String>,
    ) -> Option<String> {
        // Only produce notes when there is a lateral plan or stagnation events.
        let stagnation_events: Vec<TransitionEvent> = db
            .list_transition_events(work_id)
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.event_type == "stagnation")
            .collect();

        if lateral_plan.is_none() && stagnation_events.is_empty() {
            return None;
        }

        let mut notes = String::from("## Lateral Thinking History\n");

        if let Some(plan) = lateral_plan {
            notes.push_str(&format!("- Current lateral plan: {plan}\n"));
        }

        notes.push_str(&format!(
            "- Stagnation events: {}건\n",
            stagnation_events.len()
        ));

        // Extract pattern/confidence/persona from each stagnation event detail (JSON).
        for ev in &stagnation_events {
            if let Some(detail) = &ev.detail
                && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(detail)
            {
                let pattern = parsed
                    .get("pattern_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let confidence = parsed
                    .get("confidence")
                    .and_then(|v| v.as_f64())
                    .map(|c| format!("{c:.2}"))
                    .unwrap_or_else(|| "N/A".to_string());
                let persona = parsed
                    .get("recommended_persona")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                notes.push_str(&format!(
                    "- Pattern: {pattern} (confidence: {confidence})\n- Persona: {persona}\n",
                ));
            }
        }

        Some(notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::hitl::{HitlId, HitlStatus};
    use belt_core::lifecycle::NoopLifecycleHook;
    use belt_core::queue::testing::test_item;
    use belt_infra::worktree::MockWorktreeManager;
    use tempfile::TempDir;

    fn setup_deps(tmp: &TempDir) -> (Arc<dyn LifecycleHook>, Arc<dyn WorktreeManager>) {
        let hook: Arc<dyn LifecycleHook> = Arc::new(NoopLifecycleHook);
        let worktree_mgr: Arc<dyn WorktreeManager> =
            Arc::new(MockWorktreeManager::new(tmp.path().to_path_buf()));
        (hook, worktree_mgr)
    }

    fn make_hook_ctx(work_id: &str) -> HookContext {
        use belt_core::context::{ItemContext, QueueContext, SourceContext};

        let item = test_item("src:1", "implement");
        HookContext {
            work_id: work_id.to_string(),
            worktree: std::path::PathBuf::from("/tmp/test"),
            item: item.clone(),
            item_context: ItemContext {
                work_id: work_id.to_string(),
                workspace: "test-ws".to_string(),
                queue: QueueContext {
                    phase: "running".to_string(),
                    state: "implement".to_string(),
                    source_id: "src:1".to_string(),
                },
                source: SourceContext {
                    source_type: "mock".to_string(),
                    url: "https://example.com".to_string(),
                    default_branch: None,
                },
                issue: None,
                pr: None,
                history: vec![],
                worktree: None,
                source_data: serde_json::Value::Null,
            },
            failure_count: 0,
        }
    }

    /// A store holding one Running item, the state an escalation starts from.
    fn running_item_in_store() -> (Arc<Database>, QueueItem) {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let mut item = test_item("src:1", "implement");
        item.set_phase_unchecked(QueuePhase::Running);
        db.insert_item(&item).unwrap();
        (db, item)
    }

    fn entered(db: &Database, work_id: &str) -> Vec<(String, String, String)> {
        db.transitions_of(work_id)
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

    #[test]
    fn build_lateral_hitl_notes_returns_none_without_plan_or_events() {
        let db = Database::open_in_memory().unwrap();
        let result = HitlService::build_lateral_hitl_notes(&db, "work:1", &None);
        assert!(result.is_none());
    }

    #[test]
    fn build_lateral_hitl_notes_includes_plan() {
        let db = Database::open_in_memory().unwrap();
        let plan = Some("try a different approach".to_string());
        let result = HitlService::build_lateral_hitl_notes(&db, "work:1", &plan);
        let notes = result.expect("should have notes");
        assert!(notes.contains("## Lateral Thinking History"));
        assert!(notes.contains("try a different approach"));
        assert!(notes.contains("Stagnation events: 0"));
    }

    #[test]
    fn build_lateral_hitl_notes_includes_stagnation_events() {
        let db = Database::open_in_memory().unwrap();
        let ev = TransitionEvent {
            id: "ev-stag-1".to_string(),
            work_id: "work:1".to_string(),
            source_id: "src:1".to_string(),
            event_type: "stagnation".to_string(),
            phase: None,
            from_phase: None,
            detail: Some(
                serde_json::json!({
                    "pattern_type": "spinning",
                    "confidence": 0.95,
                    "reason": "repeated errors",
                    "recommended_persona": "contrarian",
                    "failure_count": 3
                })
                .to_string(),
            ),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        db.insert_transition_event(&ev).unwrap();

        let plan = Some("contrarian approach".to_string());
        let result = HitlService::build_lateral_hitl_notes(&db, "work:1", &plan);
        let notes = result.expect("should have notes");
        assert!(notes.contains("Stagnation events: 1"));
        assert!(notes.contains("Pattern: spinning (confidence: 0.95)"));
        assert!(notes.contains("Persona: contrarian"));
    }

    #[test]
    fn handle_escalation_retry_stores_lateral_plan() {
        let tmp = TempDir::new().unwrap();
        let mut queue = VecDeque::new();
        let (db, mut item) = running_item_in_store();
        let (hook, worktree_mgr) = setup_deps(&tmp);

        let plan = Some("\n\n## Lateral Plan\ntest plan".to_string());
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        let commit = svc
            .handle_escalation(&mut item, EscalationAction::Retry, plan.clone(), hook_ctx)
            .unwrap();

        assert_eq!(commit, EscalationCommit::Applied);
        let retry = svc.queue.back().expect("should have retry item");
        assert_eq!(retry.phase(), QueuePhase::Pending);
        assert_eq!(retry.lateral_plan, plan);
    }

    #[test]
    fn handle_escalation_retry_without_plan_clears_lateral_plan() {
        let tmp = TempDir::new().unwrap();
        let mut queue = VecDeque::new();
        let (db, mut item) = running_item_in_store();
        let (hook, worktree_mgr) = setup_deps(&tmp);

        item.lateral_plan = Some("old plan".to_string());
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        svc.handle_escalation(&mut item, EscalationAction::Retry, None, hook_ctx)
            .unwrap();

        let retry = svc.queue.back().expect("should have retry item");
        assert!(retry.lateral_plan.is_none());
    }

    #[test]
    fn handle_escalation_retry_moves_the_same_row_back_to_pending() {
        let tmp = TempDir::new().unwrap();
        let mut queue = VecDeque::new();
        let (db, mut item) = running_item_in_store();
        let (hook, worktree_mgr) = setup_deps(&tmp);
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        svc.handle_escalation(
            &mut item,
            EscalationAction::RetryWithComment,
            None,
            hook_ctx,
        )
        .unwrap();

        assert_eq!(
            db.get_item(&item.work_id).unwrap().phase(),
            QueuePhase::Pending
        );
        assert_eq!(
            entered(&db, &item.work_id),
            vec![(
                "running".to_string(),
                "pending".to_string(),
                "escalation:retry_with_comment".to_string()
            )]
        );
    }

    #[test]
    fn handle_escalation_hitl_transitions_to_hitl_phase() {
        let tmp = TempDir::new().unwrap();
        let mut queue = VecDeque::new();
        let (db, mut item) = running_item_in_store();
        let (hook, worktree_mgr) = setup_deps(&tmp);

        let plan = Some("some plan".to_string());
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        svc.handle_escalation(&mut item, EscalationAction::Hitl, plan, hook_ctx)
            .unwrap();

        let hitl = svc.queue.back().expect("should have hitl item");
        assert_eq!(hitl.phase(), QueuePhase::Hitl);
    }

    #[test]
    fn handle_escalation_hitl_opens_request_with_notes() {
        let tmp = TempDir::new().unwrap();
        let mut queue = VecDeque::new();
        let (db, mut item) = running_item_in_store();
        let (hook, worktree_mgr) = setup_deps(&tmp);
        let plan = Some("try a different algorithm".to_string());
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        svc.handle_escalation(&mut item, EscalationAction::Hitl, plan, hook_ctx)
            .unwrap();

        assert_eq!(
            db.get_item(&item.work_id).unwrap().phase(),
            QueuePhase::Hitl
        );
        let log = db.transitions_of(&item.work_id).unwrap();
        let entering = log
            .iter()
            .find(|e| e.to_phase.as_deref() == Some("hitl"))
            .expect("running -> hitl must be logged");
        assert_eq!(entering.reason.as_deref(), Some("escalation:hitl"));
        let request = db
            .hitl_request(&HitlId::new(format!("hitl-{}", entering.seq)))
            .unwrap()
            .expect("an open request must exist");
        assert_eq!(request.status, HitlStatus::Open);
        assert_eq!(request.reason, Some(HitlReason::RetryMaxExceeded));
        assert!(
            request
                .notes
                .as_deref()
                .is_some_and(|n| n.contains("try a different algorithm"))
        );
    }

    #[test]
    fn handle_escalation_hitl_attaches_lateral_notes() {
        let tmp = TempDir::new().unwrap();
        let mut queue = VecDeque::new();
        let (db, mut item) = running_item_in_store();
        let (hook, worktree_mgr) = setup_deps(&tmp);

        let plan = Some("try a different algorithm".to_string());
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        svc.handle_escalation(&mut item, EscalationAction::Hitl, plan, hook_ctx)
            .unwrap();

        let hitl = svc.queue.back().expect("should have hitl item");
        let notes = hitl.hitl_notes.as_ref().expect("hitl_notes should be set");
        assert!(notes.contains("## Lateral Thinking History"));
        assert!(notes.contains("try a different algorithm"));
        assert!(notes.contains("Stagnation events: 0"));
    }

    #[test]
    fn handle_escalation_hitl_no_notes_without_plan_or_events() {
        let tmp = TempDir::new().unwrap();
        let mut queue = VecDeque::new();
        let (db, mut item) = running_item_in_store();
        let (hook, worktree_mgr) = setup_deps(&tmp);
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        svc.handle_escalation(&mut item, EscalationAction::Hitl, None, hook_ctx)
            .unwrap();

        let hitl = svc.queue.back().expect("should have hitl item");
        assert_eq!(hitl.phase(), QueuePhase::Hitl);
        assert!(hitl.hitl_notes.is_none());
    }

    #[test]
    fn handle_escalation_skip_transitions_to_skipped() {
        let tmp = TempDir::new().unwrap();
        let mut queue = VecDeque::new();
        let (db, mut item) = running_item_in_store();
        let (hook, worktree_mgr) = setup_deps(&tmp);
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        svc.handle_escalation(&mut item, EscalationAction::Skip, None, hook_ctx)
            .unwrap();

        assert_eq!(item.phase(), QueuePhase::Skipped);
        assert_eq!(
            db.get_item(&item.work_id).unwrap().phase(),
            QueuePhase::Skipped
        );
    }

    #[test]
    fn handle_escalation_conflict_changes_nothing_in_memory() {
        let tmp = TempDir::new().unwrap();
        let mut queue = VecDeque::new();
        let (db, mut item) = running_item_in_store();
        let (hook, worktree_mgr) = setup_deps(&tmp);
        db.update_phase(&item.work_id, QueuePhase::Skipped).unwrap();
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        let commit = svc
            .handle_escalation(&mut item, EscalationAction::Hitl, None, hook_ctx)
            .unwrap();

        assert_eq!(
            commit,
            EscalationCommit::Conflict {
                current: QueuePhase::Skipped
            }
        );
        assert!(svc.queue.is_empty());
        assert_eq!(item.phase(), QueuePhase::Running);
        assert_eq!(
            db.get_item(&item.work_id).unwrap().phase(),
            QueuePhase::Skipped
        );
    }
}
