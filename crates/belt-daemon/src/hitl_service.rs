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

use belt_core::escalation::EscalationAction;
use belt_core::lifecycle::{HookContext, LifecycleHook};
use belt_core::phase::QueuePhase;
use belt_core::queue::{HitlReason, QueueItem};
use belt_infra::db::{Database, TransitionEvent};
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

/// Drives HITL escalation logic for the daemon lifecycle.
///
/// Responsibilities:
/// 1. Route escalation actions (Retry, Skip, Hitl/Replan) to appropriate state transitions
/// 2. Build lateral thinking history notes for HITL items
/// 3. Fire lifecycle hooks on escalation
/// 4. Record transition events to the database
pub struct HitlService<'a> {
    queue: &'a mut VecDeque<QueueItem>,
    db: &'a Option<Arc<Database>>,
    hook: &'a Arc<dyn LifecycleHook>,
    worktree_mgr: &'a Arc<dyn WorktreeManager>,
}

impl<'a> HitlService<'a> {
    /// Create a new `HitlService` with borrowed daemon state.
    pub fn new(
        queue: &'a mut VecDeque<QueueItem>,
        db: &'a Option<Arc<Database>>,
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

    /// Handle an escalation action for a queue item.
    ///
    /// Routes the action to the appropriate state transition:
    /// - `Retry`/`RetryWithComment`: clone item back to Pending with lateral plan
    /// - `Skip`: transition to Skipped
    /// - `Hitl`/`Replan`: transition to Hitl with lateral thinking notes
    pub fn handle_escalation(
        &mut self,
        item: &mut QueueItem,
        action: EscalationAction,
        lateral_plan: Option<String>,
        hook_ctx: HookContext,
    ) {
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
            }
            EscalationAction::Skip => {
                item.set_phase_unchecked(QueuePhase::Skipped);
            }
            EscalationAction::Hitl | EscalationAction::Replan => {
                item.set_phase_unchecked(QueuePhase::Hitl);
                item.hitl_created_at = Some(now);
                item.hitl_reason = Some(HitlReason::RetryMaxExceeded);
                item.hitl_notes =
                    Self::build_lateral_hitl_notes(self.db, &item.work_id, &lateral_plan);
                self.queue.push_back(item.clone());
            }
        }
    }

    /// Build hitl_notes markdown from lateral plan and stagnation events.
    ///
    /// When an item escalates to HITL, this attaches the full lateral thinking
    /// history so that human reviewers can see what automated approaches were
    /// already attempted.
    pub fn build_lateral_hitl_notes(
        db: &Option<Arc<Database>>,
        work_id: &str,
        lateral_plan: &Option<String>,
    ) -> Option<String> {
        // Only produce notes when there is a lateral plan or stagnation events.
        let stagnation_events: Vec<TransitionEvent> = db
            .as_ref()
            .and_then(|db| db.list_transition_events(work_id).ok())
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
                    phase: "failed".to_string(),
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

    #[test]
    fn build_lateral_hitl_notes_returns_none_without_plan_or_events() {
        let db: Option<Arc<Database>> = None;
        let result = HitlService::build_lateral_hitl_notes(&db, "work:1", &None);
        assert!(result.is_none());
    }

    #[test]
    fn build_lateral_hitl_notes_includes_plan() {
        let db: Option<Arc<Database>> = None;
        let plan = Some("try a different approach".to_string());
        let result = HitlService::build_lateral_hitl_notes(&db, "work:1", &plan);
        let notes = result.expect("should have notes");
        assert!(notes.contains("## Lateral Thinking History"));
        assert!(notes.contains("try a different approach"));
        assert!(notes.contains("Stagnation events: 0"));
    }

    #[test]
    fn build_lateral_hitl_notes_includes_stagnation_events() {
        let db_inner = Database::open_in_memory().unwrap();
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
        db_inner.insert_transition_event(&ev).unwrap();

        let db: Option<Arc<Database>> = Some(Arc::new(db_inner));
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
        let db: Option<Arc<Database>> = None;
        let (hook, worktree_mgr) = setup_deps(&tmp);

        let mut item = test_item("src:1", "implement");
        item.set_phase_unchecked(QueuePhase::Failed);
        let plan = Some("\n\n## Lateral Plan\ntest plan".to_string());
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        svc.handle_escalation(&mut item, EscalationAction::Retry, plan.clone(), hook_ctx);

        let retry = svc.queue.back().expect("should have retry item");
        assert_eq!(retry.phase(), QueuePhase::Pending);
        assert_eq!(retry.lateral_plan, plan);
    }

    #[test]
    fn handle_escalation_retry_without_plan_clears_lateral_plan() {
        let tmp = TempDir::new().unwrap();
        let mut queue = VecDeque::new();
        let db: Option<Arc<Database>> = None;
        let (hook, worktree_mgr) = setup_deps(&tmp);

        let mut item = test_item("src:1", "implement");
        item.set_phase_unchecked(QueuePhase::Failed);
        item.lateral_plan = Some("old plan".to_string());
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        svc.handle_escalation(&mut item, EscalationAction::Retry, None, hook_ctx);

        let retry = svc.queue.back().expect("should have retry item");
        assert!(retry.lateral_plan.is_none());
    }

    #[test]
    fn handle_escalation_hitl_transitions_to_hitl_phase() {
        let tmp = TempDir::new().unwrap();
        let mut queue = VecDeque::new();
        let db: Option<Arc<Database>> = None;
        let (hook, worktree_mgr) = setup_deps(&tmp);

        let mut item = test_item("src:1", "implement");
        item.set_phase_unchecked(QueuePhase::Failed);
        let plan = Some("some plan".to_string());
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        svc.handle_escalation(&mut item, EscalationAction::Hitl, plan, hook_ctx);

        let hitl = svc.queue.back().expect("should have hitl item");
        assert_eq!(hitl.phase(), QueuePhase::Hitl);
    }

    #[test]
    fn handle_escalation_hitl_attaches_lateral_notes() {
        let tmp = TempDir::new().unwrap();
        let mut queue = VecDeque::new();
        let db: Option<Arc<Database>> = None;
        let (hook, worktree_mgr) = setup_deps(&tmp);

        let mut item = test_item("src:1", "implement");
        item.set_phase_unchecked(QueuePhase::Failed);
        let plan = Some("try a different algorithm".to_string());
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        svc.handle_escalation(&mut item, EscalationAction::Hitl, plan, hook_ctx);

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
        let db: Option<Arc<Database>> = None;
        let (hook, worktree_mgr) = setup_deps(&tmp);

        let mut item = test_item("src:1", "implement");
        item.set_phase_unchecked(QueuePhase::Failed);
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        svc.handle_escalation(&mut item, EscalationAction::Hitl, None, hook_ctx);

        let hitl = svc.queue.back().expect("should have hitl item");
        assert_eq!(hitl.phase(), QueuePhase::Hitl);
        assert!(hitl.hitl_notes.is_none());
    }

    #[test]
    fn handle_escalation_skip_transitions_to_skipped() {
        let tmp = TempDir::new().unwrap();
        let mut queue = VecDeque::new();
        let db: Option<Arc<Database>> = None;
        let (hook, worktree_mgr) = setup_deps(&tmp);

        let mut item = test_item("src:1", "implement");
        item.set_phase_unchecked(QueuePhase::Failed);
        let hook_ctx = make_hook_ctx(&item.work_id);

        let mut svc = HitlService::new(&mut queue, &db, &hook, &worktree_mgr);
        svc.handle_escalation(&mut item, EscalationAction::Skip, None, hook_ctx);

        assert_eq!(item.phase(), QueuePhase::Skipped);
    }
}
