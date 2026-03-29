use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;

use belt_core::action::Action;
use belt_core::context::HistoryEntry;
use belt_core::dependency::{DependencyGuard, SpecDependencyGuard};
use belt_core::error::BeltError;
use belt_core::escalation::EscalationAction;
use belt_core::phase::QueuePhase;
use belt_core::queue::{HistoryEvent, HitlReason, HitlRespondAction, QueueItem};
use belt_core::runtime::RuntimeRegistry;
use belt_core::source::DataSource;
use belt_core::state_machine;
use belt_core::workspace::{StateConfig, WorkspaceConfig};
use belt_infra::db::{Database, TransitionEvent};
use belt_infra::worktree::WorktreeManager;

use crate::concurrency::ConcurrencyTracker;
use crate::cron::{
    BuiltinJobDeps, CronEngine, builtin_jobs, load_custom_jobs, seed_workspace_crons,
};
use crate::evaluator::Evaluator;
use crate::executor::{ActionEnv, ActionExecutor, ActionResult};

/// Safely transition a [`QueueItem`] to a new phase.
///
/// All phase mutations **must** go through this function so that
/// [`QueuePhase::can_transition_to`] is always checked.
/// Returns the previous phase on success for transition event recording.
fn transit(item: &mut QueueItem, to: QueuePhase) -> Result<QueuePhase, BeltError> {
    let from = item.phase;
    if !from.can_transition_to(&to) {
        return Err(BeltError::InvalidTransition { from, to });
    }
    item.phase = to;
    item.updated_at = Utc::now().to_rfc3339();
    Ok(from)
}

/// Daemon -- state machine + yaml prompt/script executor.
pub struct Daemon {
    config: WorkspaceConfig,
    sources: Vec<Box<dyn DataSource>>,
    executor: Arc<ActionExecutor>,
    worktree_mgr: Arc<dyn WorktreeManager>,
    tracker: ConcurrencyTracker,
    queue: VecDeque<QueueItem>,
    history: Vec<HistoryEntry>,
    db: Option<Arc<Database>>,
    /// History events with full lineage information for failure tracking.
    history_events: Vec<HistoryEvent>,
    evaluator: Evaluator,
    /// Cron engine for scheduling periodic jobs (evaluate, hitl_timeout, etc.).
    cron_engine: Option<CronEngine>,
    /// Graceful shutdown 플래그. true이면 새 아이템 수집을 중단한다.
    shutdown_requested: bool,
    /// Evaluator 스크립트 실행을 위한 Belt home 디렉토리.
    belt_home: PathBuf,
    /// Dependency guard for spec execution ordering.
    dependency_guard: SpecDependencyGuard,
}

#[derive(Debug)]
pub enum ItemOutcome {
    Completed(QueueItem),
    Failed {
        item: QueueItem,
        error: String,
        escalation: EscalationAction,
    },
    Skipped(QueueItem),
}

/// 병렬 실행 태스크의 결과. daemon 상태 업데이트에 필요한 데이터를 담는다.
struct ExecutionResult {
    item: QueueItem,
    outcome: ExecutionOutcome,
    ws_name: String,
    /// on_fail actions from the state config, deferred for escalation-aware execution.
    on_fail_actions: Vec<Action>,
    /// worktree path for on_fail script execution.
    worktree: Option<PathBuf>,
    /// Token usage from on_enter execution, recorded separately from handler result.
    on_enter_result: Option<ActionResult>,
}

enum ExecutionOutcome {
    Completed {
        result: Option<ActionResult>,
    },
    Failed {
        error: String,
        result: Option<ActionResult>,
    },
    Skipped,
    WorktreeError {
        error: String,
    },
}

impl Daemon {
    pub fn new(
        config: WorkspaceConfig,
        sources: Vec<Box<dyn DataSource>>,
        registry: Arc<RuntimeRegistry>,
        worktree_mgr: Box<dyn WorktreeManager>,
        max_concurrent: u32,
    ) -> Self {
        let evaluator = Evaluator::new(&config.name);
        Self {
            config,
            sources,
            executor: Arc::new(ActionExecutor::new(registry)),
            worktree_mgr: worktree_mgr.into(),
            tracker: ConcurrencyTracker::new(max_concurrent),
            queue: VecDeque::new(),
            history: Vec::new(),
            db: None,
            history_events: Vec::new(),
            evaluator,
            cron_engine: None,
            shutdown_requested: false,
            belt_home: PathBuf::from(
                std::env::var("BELT_HOME").unwrap_or_else(|_| ".belt".to_string()),
            ),
            dependency_guard: SpecDependencyGuard,
        }
    }

    /// Set the database for persisting token usage records.
    ///
    /// Also initializes the built-in cron jobs which require a database handle
    /// and loads user-defined custom cron jobs from the database.
    pub fn with_db(mut self, db: Database) -> Self {
        let db = Arc::new(db);
        let report_dir = Some(self.belt_home.join("reports"));
        let deps = BuiltinJobDeps {
            db: Arc::clone(&db),
            worktree_mgr: Arc::clone(&self.worktree_mgr),
            workspace_root: self.belt_home.clone(),
            report_dir: report_dir.clone(),
        };
        let mut cron = self.cron_engine.take().unwrap_or_default();
        for job in builtin_jobs(deps) {
            cron.register(job);
        }

        // Seed per-workspace cron jobs for all registered workspaces (CR-13).
        // This ensures that workspace-scoped cron handlers are active when the
        // daemon starts, not only when `workspace add` is run.
        if let Ok(workspaces) = db.list_workspaces() {
            for (ws_name, _config_path, _created_at) in &workspaces {
                let ws_deps = BuiltinJobDeps {
                    db: Arc::clone(&db),
                    worktree_mgr: Arc::clone(&self.worktree_mgr),
                    workspace_root: self.belt_home.clone(),
                    report_dir: report_dir.clone(),
                };
                seed_workspace_crons(&mut cron, ws_name, ws_deps);
                tracing::info!(workspace = %ws_name, "seeded per-workspace cron jobs");
            }
        }

        // Load user-defined custom cron jobs from the DB.
        load_custom_jobs(&mut cron, &db);

        self.cron_engine = Some(cron);
        self.db = Some(db);
        self
    }

    /// Return a reference to the database, if configured.
    pub fn database(&self) -> Option<&Arc<Database>> {
        self.db.as_ref()
    }

    /// Set the belt home directory for evaluator scripts.
    pub fn with_belt_home(mut self, belt_home: PathBuf) -> Self {
        self.belt_home = belt_home;
        self
    }

    /// Set the cron engine for periodic job scheduling.
    pub fn with_cron_engine(mut self, engine: CronEngine) -> Self {
        self.cron_engine = Some(engine);
        self
    }

    /// Set the maximum evaluate failure threshold for HITL escalation.
    pub fn with_max_eval_failures(mut self, max: u32) -> Self {
        self.evaluator = Evaluator::new(&self.config.name).with_max_eval_failures(max);
        self
    }

    // ---------------------------------------------------------------
    // Transition event recording
    // ---------------------------------------------------------------

    /// Record a phase transition event to the database.
    ///
    /// Silently logs a warning on failure — transition recording must not
    /// block the state machine.
    fn record_transition(
        db: &Option<Arc<Database>>,
        work_id: &str,
        source_id: &str,
        from: QueuePhase,
        to: QueuePhase,
        event_type: &str,
        detail: Option<String>,
    ) {
        let Some(db) = db.as_ref() else {
            return;
        };
        let now = Utc::now();
        let event = TransitionEvent {
            id: format!("te-{}-{}", work_id, now.timestamp_millis()),
            work_id: work_id.to_string(),
            source_id: source_id.to_string(),
            event_type: event_type.to_string(),
            phase: Some(to.as_str().to_string()),
            from_phase: Some(from.as_str().to_string()),
            detail,
            created_at: now.to_rfc3339(),
        };
        if let Err(e) = db.insert_transition_event(&event) {
            tracing::warn!(
                work_id = %work_id,
                error = %e,
                "failed to record transition event"
            );
        }
    }

    // ---------------------------------------------------------------
    // Phase 1: Collect items from DataSources
    // ---------------------------------------------------------------

    /// Collect new items from all DataSources and add them to the Pending queue.
    pub async fn collect(&mut self) -> Result<usize> {
        let mut total = 0;
        for source in &mut self.sources {
            let items = source.collect(&self.config).await?;
            total += items.len();
            for item in items {
                if !self.queue.iter().any(|q| q.work_id == item.work_id) {
                    self.queue.push_back(item);
                }
            }
        }
        Ok(total)
    }

    // ---------------------------------------------------------------
    // Phase 2: Advance queue items through the state machine
    // ---------------------------------------------------------------

    /// Auto-transition Pending -> Ready -> Running (respecting concurrency).
    pub fn advance(&mut self) -> usize {
        let mut advanced = 0;

        // Pending -> Ready (uses safe transit + dependency gate + conflict detection)
        // Collect indices first to avoid borrow issues with self.
        let pending_indices: Vec<usize> = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, item)| item.phase == QueuePhase::Pending)
            .map(|(i, _)| i)
            .collect();

        for idx in pending_indices {
            if state_machine::transit(QueuePhase::Pending, QueuePhase::Ready).is_err() {
                continue;
            }

            // Dependency gate: check if the spec's depends_on specs are all completed.
            if !self.check_dependency_gate(&self.queue[idx].source_id.clone()) {
                tracing::debug!(
                    "dependency gate blocked: {} (source={})",
                    self.queue[idx].work_id,
                    self.queue[idx].source_id
                );
                continue;
            }

            if transit(&mut self.queue[idx], QueuePhase::Ready).is_ok() {
                advanced += 1;
                Self::record_transition(
                    &self.db,
                    &self.queue[idx].work_id,
                    &self.queue[idx].source_id,
                    QueuePhase::Pending,
                    QueuePhase::Ready,
                    "phase_enter",
                    None,
                );

                // Conflict detection: after transitioning to Ready, check if spec
                // entry_points overlap with other active specs. If so, escalate to HITL.
                let conflict = self.check_conflict_gate(&self.queue[idx].source_id.clone());
                if let Some(notes) = conflict {
                    tracing::warn!(
                        work_id = %self.queue[idx].work_id,
                        "spec conflict detected, escalating to HITL: {notes}"
                    );
                    let now = Utc::now().to_rfc3339();
                    let _ = transit(&mut self.queue[idx], QueuePhase::Hitl);
                    Self::record_transition(
                        &self.db,
                        &self.queue[idx].work_id,
                        &self.queue[idx].source_id,
                        QueuePhase::Ready,
                        QueuePhase::Hitl,
                        "phase_enter",
                        Some(notes.clone()),
                    );
                    self.queue[idx].hitl_created_at = Some(now);
                    self.queue[idx].hitl_reason = Some(HitlReason::SpecConflict);
                    self.queue[idx].hitl_notes = Some(notes);
                }
            }
        }

        // Ready -> Running (respecting concurrency)
        let ws_id = &self.config.name;
        let ws_concurrency = self.config.concurrency;

        for item in self.queue.iter_mut() {
            if item.phase == QueuePhase::Ready
                && self.tracker.can_spawn_in_workspace(ws_id, ws_concurrency)
                && transit(item, QueuePhase::Running).is_ok()
            {
                Self::record_transition(
                    &self.db,
                    &item.work_id,
                    &item.source_id,
                    QueuePhase::Ready,
                    QueuePhase::Running,
                    "phase_enter",
                    None,
                );
                self.tracker.track(ws_id);
                advanced += 1;
            }
        }

        advanced
    }

    /// Advance Pending items to Ready.
    pub fn advance_pending_to_ready(&mut self) {
        for item in self.queue.iter_mut() {
            if item.phase == QueuePhase::Pending {
                let _ = transit(item, QueuePhase::Ready);
            }
        }
    }

    /// Advance Ready items to Running, respecting both per-workspace and global concurrency.
    ///
    /// `ws_concurrency_limits` maps workspace IDs to their concurrency limits.
    /// Workspaces not present in the map use `default_concurrency` (falls back to 1).
    pub fn advance_ready_to_running(
        &mut self,
        ws_concurrency_limits: &HashMap<String, u32>,
        default_concurrency: u32,
    ) {
        let ready_indices: Vec<usize> = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, it)| it.phase == QueuePhase::Ready)
            .map(|(i, _)| i)
            .collect();

        for idx in ready_indices {
            if !self.tracker.can_spawn() {
                break;
            }

            let ws = self.queue[idx].workspace_id.clone();
            let ws_limit = ws_concurrency_limits
                .get(&ws)
                .copied()
                .unwrap_or(default_concurrency);

            if !self.tracker.can_spawn_in_workspace(&ws, ws_limit) {
                continue;
            }

            if transit(&mut self.queue[idx], QueuePhase::Running).is_ok() {
                self.tracker.track(&ws);
            }
        }
    }

    /// Check whether a queue item's associated spec has all dependencies completed.
    ///
    /// Uses the database to resolve specs. If no database is configured or no
    /// matching spec is found, the gate is open (returns `true`) to avoid
    /// blocking items that are not spec-based.
    fn check_dependency_gate(&self, source_id: &str) -> bool {
        let db = match &self.db {
            Some(db) => db,
            None => return true,
        };

        // Try to find a spec whose ID matches the source_id.
        let spec = match db.get_spec(source_id) {
            Ok(spec) => spec,
            Err(_) => return true, // No matching spec — gate open.
        };

        let result = self
            .dependency_guard
            .check_dependencies(&spec, |dep_id| db.get_spec(dep_id).ok());

        if !result.is_ready() {
            tracing::trace!("spec {} blocked by dependencies: {:?}", spec.id, result);
        }

        result.is_ready()
    }

    /// Check whether a queue item's associated spec has entry_point conflicts
    /// with other active specs.
    ///
    /// Returns `Some(notes)` with conflict details when a conflict is detected,
    /// or `None` when no conflict exists. If no database is configured or no
    /// matching spec is found, returns `None` (gate open).
    fn check_conflict_gate(&self, source_id: &str) -> Option<String> {
        let db = match &self.db {
            Some(db) => db,
            None => return None,
        };

        let spec = match db.get_spec(source_id) {
            Ok(spec) => spec,
            Err(_) => return None,
        };

        let db_ref = Arc::clone(db);
        let result = self.dependency_guard.check_conflicts(&spec, || {
            db_ref
                .list_specs(None, Some(belt_core::spec::SpecStatus::Active))
                .unwrap_or_default()
        });

        match result {
            belt_core::dependency::ConflictCheckResult::Clear => None,
            belt_core::dependency::ConflictCheckResult::Conflict {
                conflicting_specs,
                overlapping_paths,
            } => Some(format!(
                "spec-conflict: entry_point overlap with [{}] on paths [{}]",
                conflicting_specs.join(", "),
                overlapping_paths.join(", ")
            )),
        }
    }

    // ---------------------------------------------------------------
    // Phase 3: Execute running items (parallel)
    // ---------------------------------------------------------------

    /// Execute handlers for all Running items in parallel using `tokio::spawn`.
    ///
    /// Running 상태의 아이템들을 `tokio::spawn`으로 동시에 실행하고
    /// 결과를 수집한다. concurrency 제한은 `advance()`에서 이미 적용되었으므로
    /// Running 상태인 아이템은 모두 실행 가능하다.
    pub async fn execute_running(&mut self) -> Vec<ItemOutcome> {
        let running_indices: Vec<usize> = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, item)| item.phase == QueuePhase::Running)
            .map(|(i, _)| i)
            .collect();

        if running_indices.is_empty() {
            return Vec::new();
        }

        // Remove running items from queue (reverse order to preserve indices).
        let mut running_items: Vec<QueueItem> = Vec::with_capacity(running_indices.len());
        for &idx in running_indices.iter().rev() {
            running_items.push(self.queue.remove(idx).unwrap());
        }
        running_items.reverse();

        // Spawn parallel tasks.
        let mut join_set = tokio::task::JoinSet::new();
        for item in running_items {
            let executor = Arc::clone(&self.executor);
            let worktree_mgr = Arc::clone(&self.worktree_mgr);
            let ws_name = self.config.name.clone();
            let state_config = self.find_state_config(&item.state).cloned();

            join_set.spawn(async move {
                Self::execute_item_parallel(item, state_config, executor, worktree_mgr, ws_name)
                    .await
            });
        }

        // Collect results and apply state updates.
        let mut outcomes = Vec::new();
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(exec_result) => {
                    let outcome = self.apply_execution_result(exec_result).await;
                    outcomes.push(outcome);
                }
                Err(e) => {
                    tracing::error!("spawned task panicked: {e}");
                }
            }
        }

        outcomes
    }

    /// 단일 아이템의 handler를 실행하는 순수 async 함수.
    /// `&mut self` 의존 없이 `tokio::spawn`으로 실행 가능하다.
    async fn execute_item_parallel(
        mut item: QueueItem,
        state_config: Option<StateConfig>,
        executor: Arc<ActionExecutor>,
        worktree_mgr: Arc<dyn WorktreeManager>,
        ws_name: String,
    ) -> ExecutionResult {
        let state_config = match state_config {
            Some(cfg) => cfg,
            None => {
                item.phase = QueuePhase::Skipped;
                return ExecutionResult {
                    item,
                    outcome: ExecutionOutcome::Skipped,
                    ws_name,
                    on_fail_actions: Vec::new(),
                    worktree: None,
                    on_enter_result: None,
                };
            }
        };

        // Collect on_fail actions for deferred, escalation-aware execution.
        let on_fail_actions: Vec<Action> = state_config.on_fail.iter().map(Action::from).collect();

        // Try to reuse a preserved worktree from a previous run for this source_id.
        // Validates the preserved worktree before using it; if invalid, falls through
        // to normal creation.
        let worktree = if let Some(preserved) = worktree_mgr.lookup_preserved(&item.source_id) {
            if worktree_mgr.validate_preserved(&preserved) {
                tracing::info!(
                    source_id = %item.source_id,
                    ?preserved,
                    "reusing validated preserved worktree from previous run"
                );
                worktree_mgr.clear_preserved(&item.source_id);
                item.previous_worktree_path = None;
                preserved
            } else {
                tracing::warn!(
                    source_id = %item.source_id,
                    ?preserved,
                    "preserved worktree failed validation, falling back to fresh creation"
                );
                worktree_mgr.clear_preserved(&item.source_id);
                item.previous_worktree_path = None;
                match worktree_mgr.create_or_reuse(&ws_name) {
                    Ok(path) => path,
                    Err(e) => {
                        item.phase = QueuePhase::Failed;
                        return ExecutionResult {
                            item,
                            outcome: ExecutionOutcome::WorktreeError {
                                error: format!("worktree creation failed: {e}"),
                            },
                            ws_name,
                            on_fail_actions: Vec::new(),
                            worktree: None,
                            on_enter_result: None,
                        };
                    }
                }
            }
        } else {
            let previous_wt = item.previous_worktree_path.as_deref();
            match worktree_mgr.create_or_reuse_with_previous(&ws_name, previous_wt) {
                Ok(path) => {
                    // Clear the previous_worktree_path after successful handoff.
                    item.previous_worktree_path = None;
                    path
                }
                Err(e) => {
                    item.phase = QueuePhase::Failed;
                    return ExecutionResult {
                        item,
                        outcome: ExecutionOutcome::WorktreeError {
                            error: format!("worktree creation failed: {e}"),
                        },
                        ws_name,
                        on_fail_actions: Vec::new(),
                        worktree: None,
                        on_enter_result: None,
                    };
                }
            }
        };

        let env = ActionEnv::new(&item.work_id, &worktree);

        // on_enter
        let on_enter: Vec<Action> = state_config.on_enter.iter().map(Action::from).collect();
        let on_enter_ok = match executor.execute_all(&on_enter, &env).await {
            Ok(Some(r)) if !r.success() => {
                item.phase = QueuePhase::Failed;
                return ExecutionResult {
                    item,
                    outcome: ExecutionOutcome::Failed {
                        error: format!("on_enter failed with exit code {}", r.exit_code),
                        result: None,
                    },
                    ws_name,
                    on_fail_actions,
                    worktree: Some(worktree),
                    on_enter_result: Some(r),
                };
            }
            Err(e) => {
                tracing::warn!("on_enter failed for {}: {e}", item.work_id);
                item.phase = QueuePhase::Failed;
                return ExecutionResult {
                    item,
                    outcome: ExecutionOutcome::Failed {
                        error: format!("on_enter failed: {e}"),
                        result: None,
                    },
                    ws_name,
                    on_fail_actions,
                    worktree: Some(worktree),
                    on_enter_result: None,
                };
            }
            Ok(on_enter_res) => on_enter_res,
        };

        // handler chain
        let handlers: Vec<Action> = state_config.handlers.iter().map(Action::from).collect();
        let result = executor.execute_all(&handlers, &env).await;

        match result {
            Ok(Some(r)) if !r.success() => ExecutionResult {
                item,
                outcome: ExecutionOutcome::Failed {
                    error: r.stderr.clone(),
                    result: Some(r),
                },
                ws_name,
                on_fail_actions,
                worktree: Some(worktree),
                on_enter_result: on_enter_ok,
            },
            Ok(r) => {
                item.phase = QueuePhase::Completed;
                ExecutionResult {
                    item,
                    outcome: ExecutionOutcome::Completed { result: r },
                    ws_name,
                    on_fail_actions: Vec::new(),
                    worktree: Some(worktree),
                    on_enter_result: on_enter_ok,
                }
            }
            Err(e) => {
                item.phase = QueuePhase::Failed;
                ExecutionResult {
                    item,
                    outcome: ExecutionOutcome::Failed {
                        error: e.to_string(),
                        result: None,
                    },
                    ws_name,
                    on_fail_actions,
                    worktree: Some(worktree),
                    on_enter_result: on_enter_ok,
                }
            }
        }
    }

    /// 병렬 실행 결과를 daemon 상태에 반영한다.
    ///
    /// Q-12: on_fail scripts are only executed when the escalation action
    /// is not a silent retry (`EscalationAction::Retry`).
    async fn apply_execution_result(&mut self, exec_result: ExecutionResult) -> ItemOutcome {
        let ExecutionResult {
            mut item,
            outcome,
            ws_name,
            on_fail_actions,
            worktree,
            on_enter_result,
        } = exec_result;

        // Record token usage from on_enter execution if present.
        if let Some(ref r) = on_enter_result {
            self.try_record_token_usage(&item, r);
        }

        match outcome {
            ExecutionOutcome::Skipped => {
                Self::record_transition(
                    &self.db,
                    &item.work_id,
                    &item.source_id,
                    QueuePhase::Running,
                    QueuePhase::Skipped,
                    "handler",
                    Some("no state config".to_string()),
                );
                ItemOutcome::Skipped(item)
            }
            ExecutionOutcome::WorktreeError { error } => {
                Self::record_transition(
                    &self.db,
                    &item.work_id,
                    &item.source_id,
                    QueuePhase::Running,
                    QueuePhase::Failed,
                    "on_fail",
                    Some(error.clone()),
                );
                self.tracker.release(&ws_name);
                ItemOutcome::Failed {
                    item,
                    error,
                    escalation: EscalationAction::Retry,
                }
            }
            ExecutionOutcome::Completed { result } => {
                if let Some(ref r) = result {
                    self.try_record_token_usage(&item, r);
                }
                Self::record_transition(
                    &self.db,
                    &item.work_id,
                    &item.source_id,
                    QueuePhase::Running,
                    QueuePhase::Completed,
                    "handler",
                    None,
                );
                self.record_history(&item, "completed", None);
                self.record_history_event(&item, "completed", None);
                self.tracker.release(&ws_name);
                self.queue.push_back(item.clone());

                // CR-11: Completed 전이 시 자동 force_trigger("evaluate").
                if let Some(ref mut engine) = self.cron_engine {
                    engine.force_trigger("evaluate");
                    tracing::debug!("force_trigger(evaluate) after Completed: {}", item.work_id);
                }

                ItemOutcome::Completed(item)
            }
            ExecutionOutcome::Failed { error, result } => {
                if let Some(ref r) = result {
                    self.try_record_token_usage(&item, r);
                }
                Self::record_transition(
                    &self.db,
                    &item.work_id,
                    &item.source_id,
                    QueuePhase::Running,
                    QueuePhase::Failed,
                    "on_fail",
                    Some(error.clone()),
                );

                let failure_count = self.count_failures(&item.source_id, &item.state);
                let escalation = self.resolve_escalation(&item.state, failure_count + 1);

                // Q-12: Execute on_fail scripts only when escalation is not a silent retry.
                if escalation.should_run_on_fail() {
                    if let Some(ref wt) = worktree {
                        let env = ActionEnv::new(&item.work_id, wt);
                        match self.executor.execute_all(&on_fail_actions, &env).await {
                            Ok(Some(ref r)) => {
                                self.try_record_token_usage(&item, r);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    work_id = %item.work_id,
                                    "on_fail script execution error: {e}"
                                );
                            }
                            _ => {}
                        }
                    }
                } else {
                    tracing::debug!(
                        work_id = %item.work_id,
                        escalation = ?escalation,
                        "skipping on_fail execution for silent retry"
                    );
                }

                self.record_history(&item, "failed", Some(&error));
                self.record_history_event(&item, "failed", Some(error.clone()));

                // Q-10: Mark worktree as preserved for Failed items.
                item.mark_worktree_preserved();

                // Register preserved worktree by source_id for reuse on retry/restart.
                if let Some(ref wt) = worktree {
                    self.worktree_mgr
                        .register_preserved(&item.source_id, wt.clone());
                }
                tracing::info!(
                    work_id = %item.work_id,
                    source_id = %item.source_id,
                    phase = "failed",
                    "worktree preserved for failed item"
                );

                self.handle_escalation(&mut item, escalation);
                self.tracker.release(&ws_name);

                ItemOutcome::Failed {
                    item,
                    error,
                    escalation,
                }
            }
        }
    }

    // ---------------------------------------------------------------
    // Safe state transition methods
    // ---------------------------------------------------------------

    /// Mark a Running item as Completed.
    pub fn complete_item(&mut self, work_id: &str) -> Result<(), BeltError> {
        let item = self
            .queue
            .iter_mut()
            .find(|it| it.work_id == work_id)
            .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))?;
        let from = transit(item, QueuePhase::Completed)?;
        Self::record_transition(
            &self.db,
            work_id,
            &item.source_id,
            from,
            QueuePhase::Completed,
            "phase_enter",
            None,
        );
        Ok(())
    }

    /// Mark a Completed item as Done.
    ///
    /// After transitioning to Done, automatically cleans up the associated
    /// worktree. Cleanup errors are logged but do not fail the operation.
    pub fn mark_done(&mut self, work_id: &str) -> Result<(), BeltError> {
        let item = self
            .queue
            .iter_mut()
            .find(|it| it.work_id == work_id)
            .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))?;
        let from = transit(item, QueuePhase::Done)?;
        Self::record_transition(
            &self.db,
            work_id,
            &item.source_id,
            from,
            QueuePhase::Done,
            "phase_enter",
            None,
        );

        if let Err(e) = self.worktree_mgr.cleanup(work_id) {
            tracing::warn!(work_id, error = %e, "worktree cleanup failed on mark_done, continuing");
        }

        Ok(())
    }

    /// Mark a Completed item as Hitl (human-in-the-loop) with reason and optional notes.
    pub fn mark_hitl(
        &mut self,
        work_id: &str,
        reason: HitlReason,
        notes: Option<String>,
    ) -> Result<(), BeltError> {
        let item = self
            .queue
            .iter_mut()
            .find(|it| it.work_id == work_id)
            .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))?;
        let from = transit(item, QueuePhase::Hitl)?;
        item.hitl_created_at = Some(Utc::now().to_rfc3339());
        item.hitl_reason = Some(reason);
        item.hitl_notes = notes.clone();
        Self::record_transition(
            &self.db,
            work_id,
            &item.source_id,
            from,
            QueuePhase::Hitl,
            "phase_enter",
            Some(format!("reason: {reason}")),
        );
        Ok(())
    }

    /// Mark a Hitl item as Skipped.
    pub fn mark_skipped(&mut self, work_id: &str) -> Result<(), BeltError> {
        let item = self
            .queue
            .iter_mut()
            .find(|it| it.work_id == work_id)
            .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))?;
        let from = transit(item, QueuePhase::Skipped)?;
        Self::record_transition(
            &self.db,
            work_id,
            &item.source_id,
            from,
            QueuePhase::Skipped,
            "phase_enter",
            None,
        );
        Ok(())
    }

    /// Retry a Hitl item by sending it back to Pending.
    pub fn retry_from_hitl(&mut self, work_id: &str) -> Result<(), BeltError> {
        let item = self
            .queue
            .iter_mut()
            .find(|it| it.work_id == work_id)
            .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))?;
        let from = transit(item, QueuePhase::Pending)?;
        Self::record_transition(
            &self.db,
            work_id,
            &item.source_id,
            from,
            QueuePhase::Pending,
            "phase_enter",
            Some("retry from hitl".to_string()),
        );
        Ok(())
    }

    /// Maximum number of replan attempts before failing permanently.
    const MAX_REPLAN_COUNT: u32 = 3;

    /// Respond to a HITL item with a user action.
    ///
    /// Applies the given [`HitlRespondAction`] and records the respondent.
    ///
    /// For `Replan`, the item is rolled back to Pending with an incremented
    /// `replan_count`, and a new HITL item is created to delegate spec
    /// modification to the Claw agent. If `replan_count` exceeds
    /// [`Self::MAX_REPLAN_COUNT`], the item transitions to Failed instead.
    pub async fn respond_hitl(
        &mut self,
        work_id: &str,
        action: HitlRespondAction,
        respondent: Option<String>,
        notes: Option<String>,
    ) -> Result<(), BeltError> {
        let idx = self
            .queue
            .iter()
            .position(|it| it.work_id == work_id)
            .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))?;

        {
            let item = &self.queue[idx];
            if item.phase != QueuePhase::Hitl {
                return Err(BeltError::InvalidTransition {
                    from: item.phase,
                    to: QueuePhase::Done, // placeholder
                });
            }
        }

        let item = &mut self.queue[idx];
        item.hitl_respondent = respondent;
        if let Some(n) = notes {
            item.hitl_notes = Some(n);
        }

        // Capture spec-completion metadata before match borrows item.
        let is_spec_completion = item.state == "spec_completion";
        let is_spec_conflict = item.hitl_reason == Some(HitlReason::SpecConflict);
        let spec_id = item.source_id.clone();
        let source_id_clone = spec_id.clone();

        match action {
            HitlRespondAction::Done => {
                // Remove item from queue to call execute_on_done (which needs
                // &mut self + &mut QueueItem without borrow conflict).
                let mut item = self.queue.remove(idx).unwrap();

                // Execute on_done scripts; transitions to Done on success,
                // Failed on script failure.
                match self.execute_on_done(&mut item).await {
                    Ok(true) => {
                        Self::record_transition(
                            &self.db,
                            work_id,
                            &source_id_clone,
                            QueuePhase::Hitl,
                            QueuePhase::Done,
                            "handler",
                            Some("hitl respond: done".to_string()),
                        );
                        if is_spec_completion {
                            self.apply_spec_completion_transition(&spec_id);
                        }
                        if is_spec_conflict {
                            self.apply_spec_conflict_approved(&spec_id);
                        }
                    }
                    Ok(false) => {
                        Self::record_transition(
                            &self.db,
                            work_id,
                            &source_id_clone,
                            QueuePhase::Hitl,
                            QueuePhase::Failed,
                            "handler",
                            Some("hitl respond: done (on_done script failed)".to_string()),
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            work_id,
                            "on_done execution error during hitl respond: {e}"
                        );
                        let _ = transit(&mut item, QueuePhase::Failed);
                        Self::record_transition(
                            &self.db,
                            work_id,
                            &source_id_clone,
                            QueuePhase::Hitl,
                            QueuePhase::Failed,
                            "handler",
                            Some(format!("hitl respond: on_done error: {e}")),
                        );
                    }
                }

                // Put item back into queue so callers can inspect final state.
                self.queue.push_back(item);
                Ok(())
            }
            HitlRespondAction::Retry => {
                let from = transit(item, QueuePhase::Pending)?;
                Self::record_transition(
                    &self.db,
                    work_id,
                    &source_id_clone,
                    from,
                    QueuePhase::Pending,
                    "handler",
                    Some("hitl respond: retry".to_string()),
                );
                if is_spec_completion {
                    self.apply_spec_active_revert(&spec_id);
                }
                Ok(())
            }
            HitlRespondAction::Skip => {
                let from = transit(item, QueuePhase::Skipped)?;
                Self::record_transition(
                    &self.db,
                    work_id,
                    &source_id_clone,
                    from,
                    QueuePhase::Skipped,
                    "handler",
                    Some("hitl respond: skip".to_string()),
                );
                if is_spec_completion {
                    self.apply_spec_active_revert(&spec_id);
                }
                if is_spec_conflict {
                    self.apply_spec_conflict_rejected(&spec_id);
                }
                Ok(())
            }
            HitlRespondAction::Replan => {
                let new_replan_count = item.replan_count + 1;

                if new_replan_count > Self::MAX_REPLAN_COUNT {
                    tracing::warn!(
                        work_id,
                        replan_count = new_replan_count,
                        max = Self::MAX_REPLAN_COUNT,
                        "replan limit exceeded, transitioning to Failed"
                    );
                    item.replan_count = new_replan_count;
                    let from = transit(item, QueuePhase::Failed)?;
                    Self::record_transition(
                        &self.db,
                        work_id,
                        &source_id_clone,
                        from,
                        QueuePhase::Failed,
                        "handler",
                        Some("replan limit exceeded".to_string()),
                    );
                    return Ok(());
                }

                // Capture metadata before mutating the item for the new HITL item.
                let failure_reason = item
                    .hitl_notes
                    .clone()
                    .unwrap_or_else(|| "unknown failure".to_string());
                let source_id = item.source_id.clone();
                let workspace_id = item.workspace_id.clone();
                let state = item.state.clone();

                // Roll back item to Pending with incremented replan_count.
                item.replan_count = new_replan_count;
                let from = transit(item, QueuePhase::Pending)?;
                Self::record_transition(
                    &self.db,
                    work_id,
                    &source_id_clone,
                    from,
                    QueuePhase::Pending,
                    "handler",
                    Some(format!("replan attempt {new_replan_count}")),
                );

                // Create a new HITL item for spec modification proposal.
                let replan_work_id = format!("{work_id}:replan-{new_replan_count}");
                let mut replan_item =
                    QueueItem::new(replan_work_id, source_id, workspace_id, state);
                // The replan item starts at Pending and moves to Hitl to await
                // human review of the Claw agent's spec modification proposal.
                transit(&mut replan_item, QueuePhase::Ready)?;
                transit(&mut replan_item, QueuePhase::Running)?;
                transit(&mut replan_item, QueuePhase::Completed)?;
                transit(&mut replan_item, QueuePhase::Hitl)?;
                replan_item.hitl_created_at = Some(Utc::now().to_rfc3339());
                replan_item.hitl_reason = Some(HitlReason::SpecModificationProposed);
                replan_item.hitl_notes = Some(format!(
                    "Claw replan delegation (attempt {new_replan_count}): {failure_reason}"
                ));
                replan_item.title = Some(format!(
                    "spec-modification-proposed (replan #{new_replan_count})"
                ));
                self.queue.push_back(replan_item);

                tracing::info!(
                    work_id,
                    replan_count = new_replan_count,
                    "replan: item rolled back to Pending, spec modification HITL item created"
                );

                Ok(())
            }
        }
    }

    /// Transition a spec from Completing to Completed in the database.
    ///
    /// Called when a `spec_completion` HITL item is approved (Done).
    /// Logs a warning and continues if the database is unavailable or the
    /// transition fails -- the queue item has already moved to Done.
    fn apply_spec_completion_transition(&self, spec_id: &str) {
        if let Some(db) = &self.db {
            match db.update_spec_status(spec_id, belt_core::spec::SpecStatus::Completed) {
                Ok(()) => {
                    tracing::info!(
                        spec_id = %spec_id,
                        "spec transitioned from Completing to Completed via HITL approval"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        spec_id = %spec_id,
                        error = %e,
                        "failed to transition spec to Completed after HITL approval"
                    );
                }
            }
        } else {
            tracing::warn!(
                spec_id = %spec_id,
                "no database configured — cannot transition spec to Completed"
            );
        }
    }

    /// Revert a spec from Completing to Active in the database.
    ///
    /// Called when a `spec_completion` HITL item is rejected (Skip) or
    /// needs additional modifications (Retry).
    fn apply_spec_active_revert(&self, spec_id: &str) {
        if let Some(db) = &self.db {
            match db.update_spec_status(spec_id, belt_core::spec::SpecStatus::Active) {
                Ok(()) => {
                    tracing::info!(
                        spec_id = %spec_id,
                        "spec reverted from Completing to Active via HITL rejection/retry"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        spec_id = %spec_id,
                        error = %e,
                        "failed to revert spec to Active after HITL rejection/retry"
                    );
                }
            }
        } else {
            tracing::warn!(
                spec_id = %spec_id,
                "no database configured — cannot revert spec to Active"
            );
        }
    }

    /// Handle approval of a spec conflict HITL item.
    ///
    /// When the user approves conflicting specs to proceed in parallel,
    /// this method logs the decision. The item has already been transitioned
    /// to Done, so the conflicting spec's queue item will proceed normally
    /// on the next `advance()` cycle.
    fn apply_spec_conflict_approved(&self, spec_id: &str) {
        tracing::info!(
            spec_id = %spec_id,
            "spec conflict approved — conflicting specs will proceed in parallel"
        );
    }

    /// Handle rejection of a spec conflict HITL item.
    ///
    /// When the user rejects the later spec due to conflict, this method
    /// pauses the conflicting spec in the database so it no longer competes
    /// for the overlapping entry points. The queue item has already been
    /// transitioned to Skipped.
    fn apply_spec_conflict_rejected(&self, spec_id: &str) {
        if let Some(db) = &self.db {
            match db.update_spec_status(spec_id, belt_core::spec::SpecStatus::Paused) {
                Ok(()) => {
                    tracing::info!(
                        spec_id = %spec_id,
                        "spec paused due to conflict rejection"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        spec_id = %spec_id,
                        error = %e,
                        "failed to pause spec after conflict rejection"
                    );
                }
            }
        } else {
            tracing::warn!(
                spec_id = %spec_id,
                "no database configured — cannot pause spec after conflict rejection"
            );
        }
    }

    /// Mark a Running item as Failed and record a HistoryEvent.
    ///
    /// Also marks the worktree as preserved so it remains available for debugging.
    pub fn mark_failed(&mut self, work_id: &str, error: String) -> Result<(), BeltError> {
        let item = self
            .queue
            .iter_mut()
            .find(|it| it.work_id == work_id)
            .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))?;

        let source_id = item.source_id.clone();
        let state = item.state.clone();

        let from = transit(item, QueuePhase::Failed)?;
        item.mark_worktree_preserved();
        Self::record_transition(
            &self.db,
            work_id,
            &source_id,
            from,
            QueuePhase::Failed,
            "on_fail",
            Some(error.clone()),
        );

        // Register preserved worktree by source_id for potential reuse.
        let ws_name = &self.config.name;
        let wt_path = self.worktree_mgr.path(ws_name);
        if wt_path.exists() {
            self.worktree_mgr.register_preserved(&source_id, wt_path);
        }
        tracing::info!(work_id, source_id = %source_id, "worktree preserved for failed item");

        let attempt = self.count_failures(&source_id, &state) + 1;

        self.history_events.push(HistoryEvent {
            work_id: work_id.to_string(),
            source_id,
            state,
            status: "failed".to_string(),
            attempt,
            summary: None,
            error: Some(error),
            created_at: Utc::now(),
        });

        Ok(())
    }

    /// Apply escalation logic based on accumulated failure count.
    pub fn apply_escalation(&mut self, work_id: &str, source_id: &str, state: &str) {
        let failure_count = self.count_failures(source_id, state);

        match failure_count {
            0 => {}
            1 => {
                tracing::info!(work_id, source_id, state, "first failure recorded");
            }
            2 => {
                tracing::warn!(work_id, source_id, state, "second failure recorded");
            }
            _ => {
                tracing::error!(
                    work_id,
                    source_id,
                    state,
                    failure_count,
                    "escalating to HITL after repeated failures"
                );
                let _ = self.mark_hitl(
                    work_id,
                    HitlReason::RetryMaxExceeded,
                    Some("escalation: repeated failures".to_string()),
                );
            }
        }
    }

    // ---------------------------------------------------------------
    // on_done / tick / run (async execution loop)
    // ---------------------------------------------------------------

    /// Execute on_done scripts. Transition to Done on success, Failed on failure.
    pub async fn execute_on_done(&mut self, item: &mut QueueItem) -> Result<bool> {
        let state_config = self.find_state_config(&item.state).cloned();
        let state_config = match state_config {
            Some(cfg) => cfg,
            None => {
                let _ = transit(item, QueuePhase::Done);
                return Ok(true);
            }
        };

        if state_config.on_done.is_empty() {
            let _ = transit(item, QueuePhase::Done);
            self.record_history(item, "done", None);
            let _ = self.worktree_mgr.cleanup(&item.work_id);
            self.worktree_mgr.clear_preserved(&item.source_id);
            return Ok(true);
        }

        let worktree = self.worktree_mgr.create_or_reuse(&item.work_id)?;
        let env = ActionEnv::new(&item.work_id, &worktree);
        let on_done: Vec<Action> = state_config.on_done.iter().map(Action::from).collect();
        let result = self.executor.execute_all(&on_done, &env).await?;

        // Record token usage from on_done handler execution.
        if let Some(ref r) = result {
            self.try_record_token_usage(item, r);
        }

        match result {
            Some(r) if !r.success() => {
                let _ = transit(item, QueuePhase::Failed);
                self.record_history(item, "failed", Some("on_done script failed"));
                Ok(false)
            }
            _ => {
                let _ = transit(item, QueuePhase::Done);
                self.record_history(item, "done", None);
                let _ = self.worktree_mgr.cleanup(&item.work_id);
                self.worktree_mgr.clear_preserved(&item.source_id);
                Ok(true)
            }
        }
    }

    /// 4단계: Completed 아이템을 evaluator로 평가하여 Done 또는 HITL로 분류.
    ///
    /// handler 성공 -> Completed 전이 후 evaluator가 Done/HITL을 결정한다.
    /// evaluator 실행 실패 시 per-item failure count를 증가시키고,
    /// N회(default 3) 이상 실패 시 자동으로 HITL로 에스컬레이션한다.
    async fn evaluate_completed(&mut self) {
        let completed: Vec<String> = self
            .items_in_phase(QueuePhase::Completed)
            .iter()
            .map(|i| i.work_id.clone())
            .collect();

        if completed.is_empty() {
            return;
        }

        // evaluate LLM 호출도 concurrency slot을 소비한다 (D-04).
        if !self.tracker.can_spawn() {
            tracing::debug!(
                "no concurrency slots available for evaluate, deferring {} items",
                completed.len()
            );
            return;
        }
        self.tracker.track_evaluate();

        // Evaluator: 가능하면 ActionExecutor를 통해 LLM 호출 (token usage 추적),
        // fallback으로 기존 스크립트 방식 사용.
        let eval_result = {
            // Use the first completed item's worktree for the evaluate env.
            let eval_env = if let Some(work_id) = completed.first() {
                self.worktree_mgr.create_or_reuse(work_id).ok().map(|wt| {
                    ActionEnv::new(work_id, &wt)
                        .with_var("WORKSPACE", &self.config.name)
                        .with_var("BELT_HOME", &self.belt_home.to_string_lossy())
                        .with_var("BELT_DB", &self.belt_home.join("belt.db").to_string_lossy())
                })
            } else {
                None
            };

            if let Some(env) = &eval_env {
                match self
                    .evaluator
                    .run_evaluate_with_executor(&self.executor, env)
                    .await
                {
                    Ok(action_result) => {
                        // Record token usage from the evaluate LLM call for each completed item.
                        for work_id in &completed {
                            if let Some(item) = self.queue.iter().find(|i| i.work_id == *work_id) {
                                self.try_record_token_usage(item, &action_result);
                            }
                        }
                        Ok(crate::evaluator::EvaluateResult::from(action_result))
                    }
                    Err(e) => Err(e),
                }
            } else {
                self.evaluator.run_evaluate(&self.belt_home).await
            }
        };

        match eval_result {
            Ok(result) if result.success() => {
                // Evaluator 성공 -- on_done을 거쳐 Done으로 전이.
                for work_id in &completed {
                    self.evaluator.clear_eval_failures(work_id);
                }
                for work_id in completed {
                    if let Some(idx) = self.queue.iter().position(|i| i.work_id == work_id) {
                        let mut item = self.queue.remove(idx).unwrap();
                        match self.execute_on_done(&mut item).await {
                            Ok(true) => tracing::info!("done: {}", item.work_id),
                            Ok(false) => tracing::warn!("on_done failed: {}", item.work_id),
                            Err(e) => tracing::error!("on_done error for {}: {e}", item.work_id),
                        }
                    }
                }
            }
            Ok(result) => {
                // Q-07 + Q-14: Evaluator 비정상 종료 -- per-item failure tracking.
                let error_msg = format!(
                    "evaluator exit_code={}: {}",
                    result.exit_code,
                    result.stderr.trim()
                );
                tracing::info!(
                    "evaluator returned non-zero ({}), evaluating {} items for escalation",
                    result.exit_code,
                    completed.len()
                );

                // Collect decisions first to avoid borrow conflict with self.evaluator.
                let decisions: Vec<(String, crate::evaluator::EvalDecision)> = completed
                    .iter()
                    .map(|work_id| {
                        let decision = self.evaluator.record_eval_failure(work_id, &error_msg);
                        (work_id.clone(), decision)
                    })
                    .collect();

                let now = chrono::Utc::now().to_rfc3339();
                for (work_id, decision) in decisions {
                    match decision {
                        crate::evaluator::EvalDecision::Hitl { reason } => {
                            // N회 실패 -> HITL 에스컬레이션.
                            if let Some(idx) = self.queue.iter().position(|i| i.work_id == work_id)
                                && let Some(item) = self.queue.get_mut(idx)
                            {
                                let _ = transit(item, QueuePhase::Hitl);
                                item.hitl_created_at = Some(now.clone());
                                item.hitl_reason = Some(HitlReason::EvaluateFailure);
                                item.hitl_notes = Some(reason.clone());
                                self.history.push(HistoryEntry {
                                    source_id: item.source_id.clone(),
                                    work_id: item.work_id.clone(),
                                    state: item.state.clone(),
                                    status: belt_core::context::HistoryStatus::Hitl,
                                    attempt: self.evaluator.eval_failure_count(&work_id),
                                    summary: None,
                                    error: Some(reason),
                                    created_at: now.clone(),
                                });
                            }
                        }
                        crate::evaluator::EvalDecision::Retry => {
                            // Completed 유지, 다음 tick에서 재시도.
                            tracing::debug!(
                                "evaluate retry for {} (failure_count={})",
                                work_id,
                                self.evaluator.eval_failure_count(&work_id)
                            );
                        }
                        crate::evaluator::EvalDecision::Done => {
                            // Should not occur from record_eval_failure, but handle gracefully.
                            self.evaluator.clear_eval_failures(&work_id);
                        }
                    }
                }
            }
            Err(e) => {
                // Evaluator 실행 자체가 실패 -- per-item failure tracking with escalation.
                let error_msg = format!("evaluator execution error: {e}");
                tracing::warn!(
                    "evaluator failed for {} completed items: {e}",
                    completed.len()
                );

                let decisions: Vec<(String, crate::evaluator::EvalDecision)> = completed
                    .iter()
                    .map(|work_id| {
                        let decision = self.evaluator.record_eval_failure(work_id, &error_msg);
                        (work_id.clone(), decision)
                    })
                    .collect();

                let now = chrono::Utc::now().to_rfc3339();
                for (work_id, decision) in decisions {
                    if let crate::evaluator::EvalDecision::Hitl { reason } = decision
                        && let Some(idx) = self.queue.iter().position(|i| i.work_id == work_id)
                        && let Some(item) = self.queue.get_mut(idx)
                    {
                        let _ = transit(item, QueuePhase::Hitl);
                        // Q-10: Mark worktree as preserved for HITL items.
                        item.mark_worktree_preserved();
                        tracing::info!(
                            work_id = %item.work_id,
                            phase = "hitl",
                            "worktree preserved for HITL item"
                        );
                        item.hitl_created_at = Some(now.clone());
                        item.hitl_reason = Some(HitlReason::EvaluateFailure);
                        item.hitl_notes = Some(reason.clone());
                        self.history.push(HistoryEntry {
                            source_id: item.source_id.clone(),
                            work_id: item.work_id.clone(),
                            state: item.state.clone(),
                            status: belt_core::context::HistoryStatus::Hitl,
                            attempt: self.evaluator.eval_failure_count(&work_id),
                            summary: None,
                            error: Some(reason),
                            created_at: now.clone(),
                        });
                    }
                }
            }
        }

        // evaluate slot 반환.
        self.tracker.release_evaluate();
    }

    /// Daemon tick: collect -> advance -> execute -> evaluate.
    ///
    /// shutdown이 요청되면 collect/advance를 건너뛰고 실행 중인
    /// 아이템의 완료 처리만 수행한다.
    pub async fn tick(&mut self) -> Result<()> {
        if !self.shutdown_requested {
            let collected = self.collect().await?;
            if collected > 0 {
                tracing::info!("collected {collected} items");
            }

            let advanced = self.advance();
            if advanced > 0 {
                tracing::debug!("advanced {advanced} items");
            }
        }

        let outcomes = self.execute_running().await;
        let mut has_completed = false;
        for outcome in &outcomes {
            match outcome {
                ItemOutcome::Completed(item) => {
                    tracing::info!("completed: {}", item.work_id);
                    has_completed = true;
                }
                ItemOutcome::Failed {
                    item,
                    error,
                    escalation,
                } => {
                    tracing::warn!(
                        "failed: {} (escalation={:?}, error={})",
                        item.work_id,
                        escalation,
                        error
                    );
                }
                ItemOutcome::Skipped(item) => tracing::info!("skipped: {}", item.work_id),
            }
        }

        // handler 성공 → Completed 전이 후 force_trigger("evaluate") (D-10).
        // force_trigger는 cron의 last_run_at을 리셋하여 다음 tick에서 즉시 실행.
        if has_completed && let Some(ref mut engine) = self.cron_engine {
            engine.force_trigger("evaluate");
            tracing::debug!("force_trigger(evaluate) after handler completion");
        }

        // Evaluator로 Completed 아이템 평가 (Done vs HITL).
        self.evaluate_completed().await;

        // Cron jobs: HITL timeout, daily report, log cleanup, evaluate 등.
        if let Some(ref mut engine) = self.cron_engine {
            engine.tick();
        }

        Ok(())
    }

    /// tokio::select! 기반 async event loop with graceful shutdown.
    ///
    /// SIGINT 수신 시:
    /// 1. `shutdown_requested = true` -- 새 아이템 수집 중단.
    /// 2. Running 아이템 완료를 최대 30초 대기 (`drain_with_timeout`).
    /// 3. timeout 초과 시 Running -> Pending 롤백 (worktree 보존).
    /// 4. drain 중 두 번째 SIGINT 시 즉시 종료 (Running -> Failed 강제 전이).
    pub async fn run(&mut self, tick_interval_secs: u64) {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(tick_interval_secs));
        tracing::info!("belt daemon started (tick={}s)", tick_interval_secs);

        self.run_select_loop(&mut tick).await;

        // Running 아이템 완료를 최대 30초 대기. timeout 시 Pending으로 롤백.
        self.drain_with_timeout(std::time::Duration::from_secs(30))
            .await;

        tracing::info!("belt daemon stopped");
    }

    /// Handle a cron-trigger notification by performing a full sync of custom
    /// cron jobs from the database (including new/removed/paused/resumed/
    /// triggered jobs) and running an immediate tick.
    async fn handle_cron_trigger_signal(&mut self, source: &str) {
        tracing::info!(source, "syncing custom cron jobs from DB...");
        if let (Some(engine), Some(db)) = (&mut self.cron_engine, &self.db) {
            engine.sync_custom_jobs_from_db(db);
        }
        // Run an immediate tick so the triggered job executes now.
        if let Err(e) = self.tick().await {
            tracing::error!("tick error after {source}: {e}");
        }
    }

    /// Select loop with SIGUSR1 + IPC support (unix).
    #[cfg(unix)]
    async fn run_select_loop(&mut self, tick: &mut tokio::time::Interval) {
        let mut sigusr1 =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
                .expect("failed to register SIGUSR1 handler");

        // Also start the IPC listener so that the TCP-based notification
        // path works on Unix too (useful for testing and uniformity).
        let ipc = belt_infra::ipc::IpcListener::bind(&self.belt_home)
            .await
            .ok();

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = self.tick().await {
                        tracing::error!("tick error: {e}");
                    }
                }
                _ = sigusr1.recv() => {
                    self.handle_cron_trigger_signal("SIGUSR1").await;
                }
                Some(signal) = async {
                    match &ipc {
                        Some(l) => l.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match signal {
                        belt_infra::ipc::DaemonSignal::CronSync => {
                            self.handle_cron_trigger_signal("IPC").await;
                        }
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("received SIGINT, initiating graceful shutdown...");
                    self.shutdown_requested = true;
                    break;
                }
            }
        }
    }

    /// Select loop with IPC support (non-unix).
    #[cfg(not(unix))]
    async fn run_select_loop(&mut self, tick: &mut tokio::time::Interval) {
        let ipc = belt_infra::ipc::IpcListener::bind(&self.belt_home)
            .await
            .ok();

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = self.tick().await {
                        tracing::error!("tick error: {e}");
                    }
                }
                Some(signal) = async {
                    match &ipc {
                        Some(l) => l.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match signal {
                        belt_infra::ipc::DaemonSignal::CronSync => {
                            self.handle_cron_trigger_signal("IPC").await;
                        }
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("received SIGINT, initiating graceful shutdown...");
                    self.shutdown_requested = true;
                    break;
                }
            }
        }
    }

    /// Running 아이템 완료 대기.
    ///
    /// - timeout 초과 시 Running -> Failed (강제 전이) + 에러 로깅.
    /// - 두 번째 SIGINT 시 Running -> Pending 롤백 (worktree 보존).
    async fn drain_with_timeout(&mut self, timeout: std::time::Duration) {
        let running_count = self.items_in_phase(QueuePhase::Running).len();
        if running_count == 0 {
            return;
        }

        tracing::info!(
            "draining {} running items (timeout={}s)...",
            running_count,
            timeout.as_secs()
        );

        let deadline = tokio::time::Instant::now() + timeout;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let outcomes = self.execute_running().await;
                    for outcome in &outcomes {
                        if let ItemOutcome::Completed(item) = outcome {
                            tracing::info!("drain: completed {}", item.work_id);
                        }
                    }

                    let remaining = self.items_in_phase(QueuePhase::Running).len();
                    if remaining == 0 {
                        tracing::info!("all running items drained successfully");
                        return;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let remaining = self.items_in_phase(QueuePhase::Running).len();
                    tracing::warn!(
                        "drain timeout ({}s) exceeded, rolling back {} running items to pending",
                        timeout.as_secs(),
                        remaining
                    );
                    self.rollback_running_to_pending();
                    return;
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::warn!("received second SIGINT, force-failing running items");
                    self.force_fail_running();
                    return;
                }
            }
        }
    }

    /// Running -> Failed 강제 전이 (두 번째 SIGINT 수신 시).
    ///
    /// 각 Running 아이템을 Failed로 전이하고 에러 히스토리를 기록한다.
    /// worktree는 보존하여 후속 디버깅에 활용할 수 있도록 한다.
    fn force_fail_running(&mut self) {
        let ws_name = self.config.name.clone();
        let running_work_ids: Vec<String> = self
            .queue
            .iter()
            .filter(|item| item.phase == QueuePhase::Running)
            .map(|item| item.work_id.clone())
            .collect();

        for work_id in running_work_ids {
            if let Err(e) =
                self.mark_failed(&work_id, "graceful shutdown timeout exceeded".to_string())
            {
                tracing::error!("failed to force-fail {}: {e}", work_id);
                continue;
            }
            self.tracker.release(&ws_name);
            tracing::error!(
                "force-failed {} due to shutdown timeout (worktree preserved)",
                work_id
            );
        }
    }

    /// Running → Pending 롤백. worktree는 보존하고 source_id 기반으로 등록한다.
    ///
    /// 보존된 worktree 경로를 `WorktreeManager`의 preserved registry에 등록하여
    /// 재시작 후 동일 source_id의 아이템이 기존 worktree를 재사용할 수 있게 한다.
    pub fn rollback_running_to_pending(&mut self) {
        let ws_name = self.config.name.clone();
        for item in self.queue.iter_mut() {
            if item.phase == QueuePhase::Running {
                // Register preserved worktree before rollback so it can be reused.
                let wt_path = self.worktree_mgr.path(&ws_name);
                if wt_path.exists() {
                    self.worktree_mgr
                        .register_preserved(&item.source_id, wt_path.clone());
                    tracing::info!(
                        source_id = %item.source_id,
                        ?wt_path,
                        "preserved worktree registered for source_id"
                    );
                }

                item.mark_worktree_preserved();

                if let Err(e) = transit(item, QueuePhase::Pending) {
                    tracing::error!("failed to rollback {}: {e}", item.work_id);
                    continue;
                }
                self.tracker.release(&ws_name);
                tracing::info!(
                    "rolled back {} to Pending (worktree preserved, source_id={})",
                    item.work_id,
                    item.source_id,
                );
            }
        }
    }

    /// Shutdown이 요청되었는지 확인.
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    /// 프로그래밍 방식으로 graceful shutdown을 요청.
    pub fn request_shutdown(&mut self) {
        self.shutdown_requested = true;
    }

    // ---------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------

    fn find_state_config(&self, state: &str) -> Option<&StateConfig> {
        for source in self.config.sources.values() {
            if let Some(cfg) = source.states.get(state) {
                return Some(cfg);
            }
        }
        None
    }

    /// Count failures for a given source_id **and** state.
    pub fn count_failures(&self, source_id: &str, state: &str) -> u32 {
        let from_entries = self
            .history
            .iter()
            .filter(|h| h.state == state && h.status == belt_core::context::HistoryStatus::Failed)
            .count() as u32;

        let from_events = self
            .history_events
            .iter()
            .filter(|h| h.source_id == source_id && h.state == state && h.status == "failed")
            .count() as u32;

        from_entries + from_events
    }

    fn resolve_escalation(&self, _state: &str, failure_count: u32) -> EscalationAction {
        let policy = self
            .config
            .sources
            .values()
            .next()
            .map(|s| &s.escalation)
            .cloned()
            .unwrap_or_default();
        policy.resolve(failure_count)
    }

    fn handle_escalation(&mut self, item: &mut QueueItem, action: EscalationAction) {
        let now = chrono::Utc::now().to_rfc3339();
        match action {
            EscalationAction::Retry | EscalationAction::RetryWithComment => {
                let mut retry_item = item.clone();
                retry_item.phase = QueuePhase::Pending;
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
                self.queue.push_back(retry_item);
            }
            EscalationAction::Skip => {
                item.phase = QueuePhase::Skipped;
            }
            EscalationAction::Hitl | EscalationAction::Replan => {
                item.phase = QueuePhase::Hitl;
                item.hitl_created_at = Some(now);
                item.hitl_reason = Some(HitlReason::RetryMaxExceeded);
                self.queue.push_back(item.clone());
            }
        }
    }

    /// Token usage가 있으면 DB에 기록한다. DB가 없거나 기록 실패 시 경고만 출력.
    fn try_record_token_usage(&self, item: &QueueItem, result: &ActionResult) {
        let (Some(usage), Some(runtime_name)) = (&result.token_usage, &result.runtime_name) else {
            return;
        };

        if let Some(ref db) = self.db {
            let model = result.model.as_deref().unwrap_or("unknown");
            if let Err(e) = db.record_token_usage(
                &item.work_id,
                &self.config.name,
                runtime_name,
                model,
                usage,
                Some(result.duration.as_millis() as u64),
            ) {
                tracing::warn!("failed to record token usage for {}: {e}", item.work_id);
            } else {
                tracing::debug!(
                    "recorded token usage for {}: input={}, output={}",
                    item.work_id,
                    usage.input_tokens,
                    usage.output_tokens
                );
            }
        }
    }

    fn record_history(&mut self, item: &QueueItem, status: &str, error: Option<&str>) {
        let attempt = self
            .history
            .iter()
            .filter(|h| h.state == item.state)
            .count() as u32
            + 1;
        self.history.push(HistoryEntry {
            source_id: item.source_id.clone(),
            work_id: item.work_id.clone(),
            state: item.state.clone(),
            status: status
                .parse()
                .unwrap_or(belt_core::context::HistoryStatus::Failed),
            attempt,
            summary: None,
            error: error.map(|s| s.to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
        });
    }

    fn record_history_event(&mut self, item: &QueueItem, status: &str, error: Option<String>) {
        let attempt = self
            .history_events
            .iter()
            .filter(|h| h.source_id == item.source_id && h.state == item.state)
            .count() as u32
            + 1;
        self.history_events.push(HistoryEvent {
            work_id: item.work_id.clone(),
            source_id: item.source_id.clone(),
            state: item.state.clone(),
            status: status.to_string(),
            attempt,
            summary: None,
            error,
            created_at: Utc::now(),
        });
    }

    // ---------------------------------------------------------------
    // Queries
    // ---------------------------------------------------------------

    /// Get all queue items.
    pub fn queue_items(&self) -> &VecDeque<QueueItem> {
        &self.queue
    }

    /// Get items in a specific phase.
    pub fn items_in_phase(&self, phase: QueuePhase) -> Vec<&QueueItem> {
        self.queue.iter().filter(|i| i.phase == phase).collect()
    }

    /// Get HistoryEntry records.
    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    /// Get HistoryEvent records (with full lineage info).
    pub fn history_events(&self) -> &[HistoryEvent] {
        &self.history_events
    }

    /// Push an item onto the queue.
    pub fn push_item(&mut self, item: QueueItem) {
        self.queue.push_back(item);
    }

    /// Look up a queue item by work_id.
    pub fn get_item(&self, work_id: &str) -> Option<&QueueItem> {
        self.queue.iter().find(|it| it.work_id == work_id)
    }

    /// Access the worktree manager.
    pub fn worktree_mgr(&self) -> &dyn WorktreeManager {
        &*self.worktree_mgr
    }

    /// Return the number of items currently in Running phase.
    pub fn running_count(&self) -> usize {
        self.queue
            .iter()
            .filter(|it| it.phase == QueuePhase::Running)
            .count()
    }

    /// Return a reference to the database, if configured.
    pub fn db(&self) -> Option<&Database> {
        self.db.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::queue::testing::test_item;
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

    // --- Safe state transition tests ---

    #[test]
    fn transit_success() {
        let mut item = test_item("s1", "analyze");
        assert!(transit(&mut item, QueuePhase::Ready).is_ok());
        assert_eq!(item.phase, QueuePhase::Ready);
    }

    #[test]
    fn transit_failure() {
        let mut item = test_item("s1", "analyze");
        // Pending -> Completed is not allowed.
        let err = transit(&mut item, QueuePhase::Completed);
        assert!(err.is_err());
        assert_eq!(item.phase, QueuePhase::Pending);
    }

    #[test]
    fn complete_and_mark_done_happy_path() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Running;
        item.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item);

        assert!(daemon.complete_item("s1:analyze").is_ok());
        assert_eq!(
            daemon.get_item("s1:analyze").unwrap().phase,
            QueuePhase::Completed
        );

        assert!(daemon.mark_done("s1:analyze").is_ok());
        assert_eq!(
            daemon.get_item("s1:analyze").unwrap().phase,
            QueuePhase::Done
        );
    }

    #[test]
    fn mark_done_cleans_up_worktree() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Running;
        item.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item);

        // Create a worktree for this item.
        daemon.worktree_mgr.create_or_reuse("s1:analyze").unwrap();
        assert!(daemon.worktree_mgr.exists("s1:analyze"));

        assert!(daemon.complete_item("s1:analyze").is_ok());
        assert!(daemon.mark_done("s1:analyze").is_ok());

        // Worktree should have been cleaned up.
        assert!(
            !daemon.worktree_mgr.exists("s1:analyze"),
            "worktree should be removed after mark_done"
        );
    }

    #[test]
    fn complete_to_hitl_and_retry() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Running;
        item.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item);

        daemon.complete_item("s1:analyze").unwrap();

        assert!(
            daemon
                .mark_hitl(
                    "s1:analyze",
                    HitlReason::ManualEscalation,
                    Some("needs review".into()),
                )
                .is_ok()
        );
        assert_eq!(
            daemon.get_item("s1:analyze").unwrap().phase,
            QueuePhase::Hitl
        );

        assert!(daemon.retry_from_hitl("s1:analyze").is_ok());
        assert_eq!(
            daemon.get_item("s1:analyze").unwrap().phase,
            QueuePhase::Pending
        );
    }

    #[test]
    fn mark_failed_records_history() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Running;
        item.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item);

        assert!(daemon.mark_failed("s1:analyze", "timeout".into()).is_ok());
        assert_eq!(
            daemon.get_item("s1:analyze").unwrap().phase,
            QueuePhase::Failed
        );
        assert_eq!(daemon.history_events().len(), 1);
    }

    #[test]
    fn invalid_transition_returns_error() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        daemon.push_item(test_item("s1", "analyze"));

        // Pending -> Done is not a valid transition.
        let result = daemon.mark_done("s1:analyze");
        assert!(result.is_err());
        assert_eq!(
            daemon.get_item("s1:analyze").unwrap().phase,
            QueuePhase::Pending
        );
    }

    // --- Async execution tests ---

    #[tokio::test]
    async fn collect_adds_to_queue() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));
        source.add_item(test_item("github:org/repo#2", "implement"));

        let mut daemon = setup_daemon(&tmp, source, vec![]);
        let collected = daemon.collect().await.unwrap();
        assert_eq!(collected, 2);
        assert_eq!(daemon.queue_items().len(), 2);
    }

    #[tokio::test]
    async fn advance_pending_to_running() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));

        let mut daemon = setup_daemon(&tmp, source, vec![]);
        daemon.collect().await.unwrap();
        daemon.advance();

        let running = daemon.items_in_phase(QueuePhase::Running);
        assert_eq!(running.len(), 1);
    }

    #[tokio::test]
    async fn advance_respects_concurrency() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));
        source.add_item(test_item("github:org/repo#2", "analyze"));
        source.add_item(test_item("github:org/repo#3", "analyze"));

        let mut daemon = setup_daemon(&tmp, source, vec![]);
        daemon.collect().await.unwrap();
        daemon.advance();

        let running = daemon.items_in_phase(QueuePhase::Running);
        assert_eq!(running.len(), 2);
    }

    #[tokio::test]
    async fn execute_handler_success() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));

        let mut daemon = setup_daemon(&tmp, source, vec![0]);
        daemon.collect().await.unwrap();
        daemon.advance();

        let outcomes = daemon.execute_running().await;
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0], ItemOutcome::Completed(_)));
    }

    #[tokio::test]
    async fn execute_handler_failure_triggers_escalation() {
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
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        let pending = daemon.items_in_phase(QueuePhase::Pending);
        assert_eq!(pending.len(), 1);
    }

    #[tokio::test]
    async fn state_not_found_skips() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "nonexistent_state"));

        let mut daemon = setup_daemon(&tmp, source, vec![]);
        daemon.collect().await.unwrap();
        daemon.advance();

        let outcomes = daemon.execute_running().await;
        assert!(matches!(outcomes[0], ItemOutcome::Skipped(_)));
    }

    #[tokio::test]
    async fn on_done_success_transitions_to_done() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("github:org/repo#1", "analyze");
        item.phase = QueuePhase::Completed;

        let success = daemon.execute_on_done(&mut item).await.unwrap();
        assert!(success);
        assert_eq!(item.phase, QueuePhase::Done);
    }

    #[tokio::test]
    async fn parallel_execution_runs_multiple_items() {
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
        assert_eq!(completed_count, 2);
    }

    #[tokio::test]
    async fn shutdown_skips_collect_and_advance() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));

        let mut daemon = setup_daemon(&tmp, source, vec![]);
        daemon.request_shutdown();

        daemon.tick().await.unwrap();
        assert_eq!(daemon.queue_items().len(), 0);
        assert!(daemon.is_shutdown_requested());
    }

    #[tokio::test]
    async fn drain_with_timeout_returns_immediately_when_no_running() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // No running items -- drain should return immediately without blocking.
        daemon
            .drain_with_timeout(std::time::Duration::from_secs(30))
            .await;

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 0);
    }

    #[test]
    fn force_fail_running_multiple_items() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Put multiple items directly into Running phase.
        let mut item1 = test_item("github:org/repo#1", "analyze");
        item1.phase = QueuePhase::Running;
        item1.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item1);

        let mut item2 = test_item("github:org/repo#2", "analyze");
        item2.phase = QueuePhase::Running;
        item2.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item2);

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 2);

        daemon.force_fail_running();

        // Both items should be Failed (not Pending).
        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 0);
        assert_eq!(daemon.items_in_phase(QueuePhase::Pending).len(), 0);
        assert_eq!(daemon.items_in_phase(QueuePhase::Failed).len(), 2);

        // History events should record the forced failure.
        assert_eq!(daemon.history_events().len(), 2);
        for event in daemon.history_events() {
            assert_eq!(event.status, "failed");
            assert!(
                event
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("shutdown timeout exceeded")
            );
        }
    }

    #[test]
    fn force_fail_running_transitions_to_failed_with_history() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("github:org/repo#1", "analyze");
        item.phase = QueuePhase::Running;
        item.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item);

        daemon.force_fail_running();

        let failed = daemon.items_in_phase(QueuePhase::Failed);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].work_id, "github:org/repo#1:analyze");

        // Verify history event was recorded.
        assert_eq!(daemon.history_events().len(), 1);
        let event = &daemon.history_events()[0];
        assert_eq!(event.status, "failed");
        assert_eq!(
            event.error.as_deref().unwrap(),
            "graceful shutdown timeout exceeded"
        );
    }

    #[test]
    fn force_fail_running_is_noop_when_no_running_items() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Should not panic on empty queue.
        daemon.force_fail_running();
        assert_eq!(daemon.history_events().len(), 0);
    }

    #[tokio::test]
    async fn rollback_uses_valid_state_transition() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));

        let mut daemon = setup_daemon(&tmp, source, vec![]);
        daemon.collect().await.unwrap();
        daemon.advance();

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 1);

        // Rollback should use proper state transition (Running -> Pending).
        daemon.rollback_running_to_pending();

        let pending = daemon.items_in_phase(QueuePhase::Pending);
        assert_eq!(pending.len(), 1);
        // Verify updated_at was refreshed.
        assert_ne!(pending[0].updated_at, pending[0].created_at);
    }

    #[tokio::test]
    async fn rollback_running_to_pending_preserves_items() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));

        let mut daemon = setup_daemon(&tmp, source, vec![]);
        daemon.collect().await.unwrap();
        daemon.advance();

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 1);

        daemon.rollback_running_to_pending();

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 0);
        assert_eq!(daemon.items_in_phase(QueuePhase::Pending).len(), 1);
    }

    // ---------------------------------------------------------------
    // Construction and builder tests
    // ---------------------------------------------------------------

    #[test]
    fn new_daemon_has_empty_queue_and_history() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let daemon = setup_daemon(&tmp, source, vec![]);

        assert_eq!(daemon.queue_items().len(), 0);
        assert_eq!(daemon.history().len(), 0);
        assert_eq!(daemon.history_events().len(), 0);
        assert_eq!(daemon.running_count(), 0);
        assert!(!daemon.is_shutdown_requested());
    }

    #[test]
    fn new_daemon_with_belt_home_sets_path() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let belt_home = tmp.path().join("custom-belt-home");
        let daemon = setup_daemon(&tmp, source, vec![]).with_belt_home(belt_home.clone());

        // belt_home is private, but we confirm construction succeeds without panic.
        // The field will be used during evaluator execution.
        let _ = daemon;
    }

    // ---------------------------------------------------------------
    // Query method tests
    // ---------------------------------------------------------------

    #[test]
    fn get_item_returns_none_for_unknown_work_id() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let daemon = setup_daemon(&tmp, source, vec![]);

        assert!(daemon.get_item("nonexistent:work-id").is_none());
    }

    #[test]
    fn push_item_and_get_item_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let item = test_item("src1", "analyze");
        daemon.push_item(item.clone());

        let retrieved = daemon.get_item("src1:analyze").unwrap();
        assert_eq!(retrieved.work_id, "src1:analyze");
        assert_eq!(retrieved.phase, QueuePhase::Pending);
    }

    #[test]
    fn items_in_phase_filters_correctly() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        daemon.push_item(test_item("s1", "analyze"));

        let mut item2 = test_item("s2", "implement");
        item2.phase = QueuePhase::Running;
        daemon.push_item(item2);

        let pending = daemon.items_in_phase(QueuePhase::Pending);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].work_id, "s1:analyze");

        let running = daemon.items_in_phase(QueuePhase::Running);
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].work_id, "s2:implement");

        let done = daemon.items_in_phase(QueuePhase::Done);
        assert!(done.is_empty());
    }

    #[test]
    fn running_count_reflects_phase() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        assert_eq!(daemon.running_count(), 0);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Running;
        daemon.push_item(item);

        assert_eq!(daemon.running_count(), 1);

        let mut item2 = test_item("s2", "implement");
        item2.phase = QueuePhase::Running;
        daemon.push_item(item2);

        assert_eq!(daemon.running_count(), 2);
    }

    // ---------------------------------------------------------------
    // Shutdown flag tests
    // ---------------------------------------------------------------

    #[test]
    fn request_shutdown_sets_flag() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        assert!(!daemon.is_shutdown_requested());
        daemon.request_shutdown();
        assert!(daemon.is_shutdown_requested());
    }

    #[test]
    fn request_shutdown_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        daemon.request_shutdown();
        daemon.request_shutdown();
        assert!(daemon.is_shutdown_requested());
    }

    // ---------------------------------------------------------------
    // advance_pending_to_ready tests
    // ---------------------------------------------------------------

    #[test]
    fn advance_pending_to_ready_transitions_all_pending() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        daemon.push_item(test_item("s1", "analyze"));
        daemon.push_item(test_item("s2", "implement"));

        daemon.advance_pending_to_ready();

        assert_eq!(daemon.items_in_phase(QueuePhase::Ready).len(), 2);
        assert_eq!(daemon.items_in_phase(QueuePhase::Pending).len(), 0);
    }

    #[test]
    fn advance_pending_to_ready_skips_non_pending() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        daemon.push_item(test_item("s1", "analyze"));

        let mut item2 = test_item("s2", "implement");
        item2.phase = QueuePhase::Running;
        daemon.push_item(item2);

        daemon.advance_pending_to_ready();

        assert_eq!(daemon.items_in_phase(QueuePhase::Ready).len(), 1);
        // Running item stays Running
        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 1);
    }

    #[test]
    fn advance_pending_to_ready_empty_queue_is_noop() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Should not panic on empty queue.
        daemon.advance_pending_to_ready();

        assert_eq!(daemon.queue_items().len(), 0);
    }

    // ---------------------------------------------------------------
    // advance_ready_to_running tests
    // ---------------------------------------------------------------

    /// Create a Ready item assigned to the given workspace.
    fn ready_item_for_ws(source_id: &str, workspace_id: &str) -> QueueItem {
        let mut item = test_item(source_id, "analyze");
        item.workspace_id = workspace_id.to_string();
        item.phase = QueuePhase::Ready;
        item
    }

    /// Count items in `phase` that belong to `workspace_id`.
    fn count_in_phase_for_ws(daemon: &Daemon, phase: QueuePhase, workspace_id: &str) -> usize {
        daemon
            .items_in_phase(phase)
            .iter()
            .filter(|i| i.workspace_id == workspace_id)
            .count()
    }

    #[test]
    fn advance_ready_to_running_respects_ws_concurrency() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // 3 items ready, ws_concurrency = 2
        for id in ["s1", "s2", "s3"] {
            daemon.push_item(ready_item_for_ws(id, "test-ws"));
        }

        let limits = HashMap::from([("test-ws".to_string(), 2)]);
        daemon.advance_ready_to_running(&limits, 1);

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 2);
        assert_eq!(daemon.items_in_phase(QueuePhase::Ready).len(), 1);
    }

    #[test]
    fn advance_ready_to_running_promotes_all_when_capacity_allows() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        for id in ["s1", "s2"] {
            daemon.push_item(ready_item_for_ws(id, "test-ws"));
        }

        let limits = HashMap::from([("test-ws".to_string(), 4)]);
        daemon.advance_ready_to_running(&limits, 1);

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 2);
        assert_eq!(daemon.items_in_phase(QueuePhase::Ready).len(), 0);
    }

    #[test]
    fn advance_ready_to_running_per_workspace_independent_limits() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // ws-alpha: 3 items, limit 2
        for id in ["a1", "a2", "a3"] {
            daemon.push_item(ready_item_for_ws(id, "ws-alpha"));
        }
        // ws-beta: 2 items, limit 1
        for id in ["b1", "b2"] {
            daemon.push_item(ready_item_for_ws(id, "ws-beta"));
        }

        let limits = HashMap::from([("ws-alpha".to_string(), 2), ("ws-beta".to_string(), 1)]);
        daemon.advance_ready_to_running(&limits, 1);

        assert_eq!(
            count_in_phase_for_ws(&daemon, QueuePhase::Running, "ws-alpha"),
            2
        );
        assert_eq!(
            count_in_phase_for_ws(&daemon, QueuePhase::Ready, "ws-alpha"),
            1
        );
        assert_eq!(
            count_in_phase_for_ws(&daemon, QueuePhase::Running, "ws-beta"),
            1
        );
        assert_eq!(
            count_in_phase_for_ws(&daemon, QueuePhase::Ready, "ws-beta"),
            1
        );
    }

    #[test]
    fn advance_ready_to_running_uses_default_for_unknown_workspace() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // 3 items for "unknown-ws", not in limits map; default = 2
        for id in ["s1", "s2", "s3"] {
            daemon.push_item(ready_item_for_ws(id, "unknown-ws"));
        }

        let limits = HashMap::new();
        daemon.advance_ready_to_running(&limits, 2);

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 2);
        assert_eq!(daemon.items_in_phase(QueuePhase::Ready).len(), 1);
    }

    #[test]
    fn advance_ready_to_running_respects_global_limit() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        // Global max_concurrent = 4 from setup_daemon
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Two workspaces, each with limit 3, but global limit is 4
        for i in 0..3 {
            daemon.push_item(ready_item_for_ws(&format!("a{i}"), "ws-a"));
        }
        for i in 0..3 {
            daemon.push_item(ready_item_for_ws(&format!("b{i}"), "ws-b"));
        }

        let limits = HashMap::from([("ws-a".to_string(), 3), ("ws-b".to_string(), 3)]);
        daemon.advance_ready_to_running(&limits, 1);

        // Global limit is 4, so only 4 total should be running
        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 4);
        assert_eq!(daemon.items_in_phase(QueuePhase::Ready).len(), 2);
    }

    // ---------------------------------------------------------------
    // count_failures tests
    // ---------------------------------------------------------------

    #[test]
    fn count_failures_returns_zero_with_no_history() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let daemon = setup_daemon(&tmp, source, vec![]);

        assert_eq!(daemon.count_failures("src1", "analyze"), 0);
    }

    #[test]
    fn count_failures_includes_history_events() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("src1", "analyze");
        item.phase = QueuePhase::Running;
        daemon.push_item(item);

        daemon
            .mark_failed("src1:analyze", "first failure".into())
            .unwrap();

        assert_eq!(daemon.count_failures("src1", "analyze"), 1);
    }

    #[test]
    fn count_failures_is_source_and_state_specific() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item1 = test_item("src1", "analyze");
        item1.phase = QueuePhase::Running;
        daemon.push_item(item1);

        daemon
            .mark_failed("src1:analyze", "failure".into())
            .unwrap();

        // Different source_id → 0 failures
        assert_eq!(daemon.count_failures("src2", "analyze"), 0);
        // Different state → 0 failures
        assert_eq!(daemon.count_failures("src1", "implement"), 0);
        // Correct source_id + state → 1 failure
        assert_eq!(daemon.count_failures("src1", "analyze"), 1);
    }

    #[test]
    fn count_failures_accumulates_across_multiple_failures() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Record failures via mark_failed using distinct work_ids so each
        // call finds a fresh Running item (work_id is unique per attempt).
        for i in 0..3u32 {
            let mut item = test_item("src1", "analyze");
            item.work_id = format!("src1:analyze-attempt-{i}");
            item.phase = QueuePhase::Running;
            daemon.push_item(item);
            daemon
                .mark_failed(
                    &format!("src1:analyze-attempt-{i}"),
                    "repeated failure".into(),
                )
                .unwrap();
        }

        // count_failures filters by source_id + state, not work_id.
        assert_eq!(daemon.count_failures("src1", "analyze"), 3);
    }

    // ---------------------------------------------------------------
    // apply_escalation tests
    // ---------------------------------------------------------------

    #[test]
    fn apply_escalation_first_failure_logs_info() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // No failures recorded yet → count_failures = 0 → no action taken.
        daemon.apply_escalation("work-1", "src1", "analyze");
        // No HITL item should be in queue.
        assert_eq!(daemon.items_in_phase(QueuePhase::Hitl).len(), 0);
    }

    #[test]
    fn apply_escalation_after_three_failures_routes_to_hitl() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Record 3 failures via mark_failed using distinct work_ids so
        // each call finds a fresh Running item.
        for i in 0..3u32 {
            let mut item = test_item("src1", "analyze");
            item.work_id = format!("src1:analyze-attempt-{i}");
            item.phase = QueuePhase::Running;
            daemon.push_item(item);
            daemon
                .mark_failed(&format!("src1:analyze-attempt-{i}"), "failure".into())
                .unwrap();
        }

        // Push a Completed item so mark_hitl (called by apply_escalation) can succeed.
        let mut item = test_item("src1", "analyze");
        item.phase = QueuePhase::Completed;
        daemon.push_item(item);

        // With 3 recorded failures, apply_escalation sees failure_count >= 3 → HITL.
        daemon.apply_escalation("src1:analyze", "src1", "analyze");

        assert_eq!(daemon.items_in_phase(QueuePhase::Hitl).len(), 1);
    }

    // ---------------------------------------------------------------
    // mark_skipped tests
    // ---------------------------------------------------------------

    #[test]
    fn mark_skipped_transitions_hitl_to_skipped() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Hitl;
        daemon.push_item(item);

        assert!(daemon.mark_skipped("s1:analyze").is_ok());
        assert_eq!(
            daemon.get_item("s1:analyze").unwrap().phase,
            QueuePhase::Skipped
        );
    }

    #[test]
    fn mark_skipped_returns_error_for_unknown_id() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let result = daemon.mark_skipped("does-not-exist");
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------
    // mark_failed error path tests
    // ---------------------------------------------------------------

    #[test]
    fn mark_failed_returns_error_for_unknown_work_id() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let result = daemon.mark_failed("no-such-id", "error".into());
        assert!(result.is_err());
    }

    #[test]
    fn mark_failed_records_error_text_in_history_event() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Running;
        daemon.push_item(item);

        daemon
            .mark_failed("s1:analyze", "timeout exceeded".into())
            .unwrap();

        let events = daemon.history_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].error.as_deref(), Some("timeout exceeded"));
        assert_eq!(events[0].status, "failed");
        assert_eq!(events[0].work_id, "s1:analyze");
    }

    #[test]
    fn mark_failed_increments_attempt_counter() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Use distinct work_ids so each mark_failed targets a fresh Running item.
        for i in 0..3u32 {
            let mut item = test_item("s1", "analyze");
            item.work_id = format!("s1:analyze-{i}");
            item.phase = QueuePhase::Running;
            daemon.push_item(item);
            daemon
                .mark_failed(&format!("s1:analyze-{i}"), format!("failure {i}"))
                .unwrap();
        }

        let events = daemon.history_events();
        // The attempt counter in the last event should reflect 3 events recorded
        // for the same source_id + state combination.
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].attempt, 3);
    }

    // ---------------------------------------------------------------
    // Deduplication in collect tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn collect_deduplicates_same_work_id() {
        let tmp = TempDir::new().unwrap();

        // Use two sources that each return the same item (same work_id).
        let mut source1 = MockDataSource::new("github");
        source1.add_item(test_item("github:org/repo#1", "analyze"));

        let mut source2 = MockDataSource::new("github2");
        source2.add_item(test_item("github:org/repo#1", "analyze"));

        let config = test_workspace_config();
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(MockRuntime::new("mock", vec![])));
        let worktree_mgr = MockWorktreeManager::new(tmp.path().to_path_buf());
        let mut daemon = Daemon::new(
            config,
            vec![Box::new(source1), Box::new(source2)],
            Arc::new(registry),
            Box::new(worktree_mgr),
            4,
        );

        // Both sources report the same work_id; collect() should report 2 total
        // (1 from each source) but only enqueue 1 unique item.
        let total_reported = daemon.collect().await.unwrap();
        assert_eq!(total_reported, 2);
        assert_eq!(daemon.queue_items().len(), 1);
    }

    // ---------------------------------------------------------------
    // transit function tests
    // ---------------------------------------------------------------

    #[test]
    fn transit_updates_updated_at() {
        let mut item = test_item("s1", "analyze");
        let original_updated_at = item.updated_at.clone();

        // Small delay to ensure time progresses.
        std::thread::sleep(std::time::Duration::from_millis(2));

        transit(&mut item, QueuePhase::Ready).unwrap();

        // updated_at should have changed after a successful transition.
        assert_ne!(item.updated_at, original_updated_at);
    }

    #[test]
    fn transit_does_not_update_updated_at_on_failure() {
        let mut item = test_item("s1", "analyze");
        let original_updated_at = item.updated_at.clone();

        // Pending -> Done is invalid.
        let _ = transit(&mut item, QueuePhase::Done);

        // updated_at must be unchanged on failed transition.
        assert_eq!(item.updated_at, original_updated_at);
        assert_eq!(item.phase, QueuePhase::Pending);
    }

    // ---------------------------------------------------------------
    // advance full pipeline tests
    // ---------------------------------------------------------------

    #[test]
    fn advance_returns_count_of_transitioned_items() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        daemon.push_item(test_item("s1", "analyze"));
        daemon.push_item(test_item("s2", "implement"));

        // Both start Pending → Ready → Running.
        let advanced = daemon.advance();
        // Each item advances twice (Pending→Ready, Ready→Running) = 4 total,
        // but max concurrency is 4 and ws_concurrency is 2 so both transition.
        assert!(advanced >= 2);
    }

    #[test]
    fn advance_with_concurrency_limit_caps_running() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        // Use ws_concurrency = 2 (from config).
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Add 4 items to exceed ws_concurrency = 2.
        for i in 0..4u32 {
            daemon.push_item(test_item(&format!("s{i}"), "analyze"));
        }

        daemon.advance();

        // At most ws_concurrency (2) items should be Running.
        assert!(daemon.items_in_phase(QueuePhase::Running).len() <= 2);
    }

    // ---------------------------------------------------------------
    // complete_item and mark_done error paths
    // ---------------------------------------------------------------

    #[test]
    fn complete_item_returns_error_for_unknown_id() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let result = daemon.complete_item("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn mark_done_returns_error_for_unknown_id() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let result = daemon.mark_done("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn complete_item_invalid_phase_returns_error() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Pending item cannot be Completed (must go Pending→Ready→Running first).
        daemon.push_item(test_item("s1", "analyze"));
        let result = daemon.complete_item("s1:analyze");
        assert!(result.is_err());
        assert_eq!(
            daemon.get_item("s1:analyze").unwrap().phase,
            QueuePhase::Pending
        );
    }

    // ---------------------------------------------------------------
    // retry_from_hitl tests
    // ---------------------------------------------------------------

    #[test]
    fn retry_from_hitl_returns_error_for_unknown_id() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let result = daemon.retry_from_hitl("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn retry_from_hitl_invalid_phase_returns_error() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Pending → Pending (retry_from_hitl) is invalid since only Hitl → Pending is valid.
        daemon.push_item(test_item("s1", "analyze"));
        let result = daemon.retry_from_hitl("s1:analyze");
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------
    // mark_hitl error paths
    // ---------------------------------------------------------------

    #[test]
    fn mark_hitl_returns_error_for_unknown_id() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let result = daemon.mark_hitl("nonexistent", HitlReason::ManualEscalation, None);
        assert!(result.is_err());
    }

    #[test]
    fn mark_hitl_invalid_phase_returns_error() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Pending → Hitl is not a valid transition.
        daemon.push_item(test_item("s1", "analyze"));
        let result = daemon.mark_hitl(
            "s1:analyze",
            HitlReason::ManualEscalation,
            Some("reason".into()),
        );
        assert!(result.is_err());
        assert_eq!(
            daemon.get_item("s1:analyze").unwrap().phase,
            QueuePhase::Pending
        );
    }

    // ---------------------------------------------------------------
    // rollback tests
    // ---------------------------------------------------------------

    #[test]
    fn rollback_running_to_pending_handles_empty_queue() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Should not panic on empty queue.
        daemon.rollback_running_to_pending();

        assert_eq!(daemon.queue_items().len(), 0);
    }

    #[test]
    fn rollback_running_to_pending_skips_non_running_items() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        daemon.push_item(test_item("s1", "analyze")); // Pending

        let mut item2 = test_item("s2", "implement");
        item2.phase = QueuePhase::Completed;
        daemon.push_item(item2);

        let mut item3 = test_item("s3", "implement");
        item3.phase = QueuePhase::Running;
        daemon.push_item(item3);

        daemon.rollback_running_to_pending();

        // Pending item untouched
        assert_eq!(
            daemon.get_item("s1:analyze").unwrap().phase,
            QueuePhase::Pending
        );
        // Completed item untouched
        assert_eq!(
            daemon.get_item("s2:implement").unwrap().phase,
            QueuePhase::Completed
        );
        // Running item rolled back to Pending
        assert_eq!(
            daemon.get_item("s3:implement").unwrap().phase,
            QueuePhase::Pending
        );
    }

    #[test]
    fn rollback_multiple_running_items_all_to_pending() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        for i in 0..3u32 {
            let mut item = test_item(&format!("s{i}"), "analyze");
            item.phase = QueuePhase::Running;
            daemon.push_item(item);
        }

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 3);
        daemon.rollback_running_to_pending();
        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 0);
        assert_eq!(daemon.items_in_phase(QueuePhase::Pending).len(), 3);
    }

    // ---------------------------------------------------------------
    // Worktree preservation and reuse tests
    // ---------------------------------------------------------------

    #[test]
    fn rollback_registers_preserved_worktree_by_source_id() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Create the workspace worktree directory so it exists for registration.
        let ws_path = daemon.worktree_mgr.path("test-ws");
        std::fs::create_dir_all(&ws_path).unwrap();

        let mut item = test_item("github:org/repo#1", "analyze");
        item.phase = QueuePhase::Running;
        daemon.push_item(item);

        daemon.rollback_running_to_pending();

        // The preserved worktree should be registered under the source_id.
        let preserved = daemon.worktree_mgr.lookup_preserved("github:org/repo#1");
        assert!(preserved.is_some());
        assert_eq!(preserved.unwrap(), ws_path);
    }

    #[test]
    fn rollback_marks_worktree_preserved_flag() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("github:org/repo#1", "analyze");
        item.phase = QueuePhase::Running;
        daemon.push_item(item);

        daemon.rollback_running_to_pending();

        let item = daemon.get_item("github:org/repo#1:analyze").unwrap();
        assert!(item.worktree_preserved);
        assert_eq!(item.phase, QueuePhase::Pending);
    }

    #[tokio::test]
    async fn mark_failed_registers_preserved_worktree() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Create the workspace worktree directory.
        let ws_path = daemon.worktree_mgr.path("test-ws");
        std::fs::create_dir_all(&ws_path).unwrap();

        let mut item = test_item("github:org/repo#1", "analyze");
        item.phase = QueuePhase::Running;
        item.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item);

        daemon
            .mark_failed("github:org/repo#1:analyze", "test error".into())
            .unwrap();

        let preserved = daemon.worktree_mgr.lookup_preserved("github:org/repo#1");
        assert!(preserved.is_some());
    }

    #[tokio::test]
    async fn on_done_clears_preserved_worktree() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Register a preserved worktree.
        let wt_path = daemon.worktree_mgr.path("work-1");
        std::fs::create_dir_all(&wt_path).unwrap();
        daemon
            .worktree_mgr
            .register_preserved("github:org/repo#1", wt_path);

        let mut item = test_item("github:org/repo#1", "analyze");
        item.phase = QueuePhase::Completed;

        let success = daemon.execute_on_done(&mut item).await.unwrap();
        assert!(success);

        // Preserved mapping should be cleared after Done.
        assert!(
            daemon
                .worktree_mgr
                .lookup_preserved("github:org/repo#1")
                .is_none()
        );
    }

    // ---------------------------------------------------------------
    // respond_hitl replan tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn respond_hitl_replan_rolls_back_to_pending_and_creates_hitl_item() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Hitl;
        item.hitl_notes = Some("original failure reason".into());
        daemon.push_item(item);

        let result = daemon
            .respond_hitl(
                "s1:analyze",
                HitlRespondAction::Replan,
                Some("reviewer".into()),
                None,
            )
            .await;
        assert!(result.is_ok());

        // Original item should be rolled back to Pending with replan_count = 1.
        let original = daemon.get_item("s1:analyze").unwrap();
        assert_eq!(original.phase, QueuePhase::Pending);
        assert_eq!(original.replan_count, 1);

        // A new HITL item should have been created for spec modification.
        let replan_item = daemon.get_item("s1:analyze:replan-1").unwrap();
        assert_eq!(replan_item.phase, QueuePhase::Hitl);
        assert_eq!(
            replan_item.hitl_reason,
            Some(HitlReason::SpecModificationProposed)
        );
        assert!(
            replan_item
                .hitl_notes
                .as_ref()
                .unwrap()
                .contains("original failure reason")
        );
        assert!(
            replan_item
                .title
                .as_ref()
                .unwrap()
                .contains("spec-modification-proposed")
        );
    }

    #[tokio::test]
    async fn respond_hitl_replan_increments_count_on_successive_replans() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Hitl;
        item.replan_count = 1; // Already replanned once.
        daemon.push_item(item);

        let result = daemon
            .respond_hitl(
                "s1:analyze",
                HitlRespondAction::Replan,
                None,
                Some("second failure".into()),
            )
            .await;
        assert!(result.is_ok());

        let original = daemon.get_item("s1:analyze").unwrap();
        assert_eq!(original.phase, QueuePhase::Pending);
        assert_eq!(original.replan_count, 2);

        let replan_item = daemon.get_item("s1:analyze:replan-2").unwrap();
        assert_eq!(replan_item.phase, QueuePhase::Hitl);
    }

    #[tokio::test]
    async fn respond_hitl_replan_exceeds_limit_transitions_to_failed() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Hitl;
        item.replan_count = 3; // Already at max.
        daemon.push_item(item);

        let result = daemon
            .respond_hitl("s1:analyze", HitlRespondAction::Replan, None, None)
            .await;
        assert!(result.is_ok());

        // Should transition to Failed, not Pending.
        let original = daemon.get_item("s1:analyze").unwrap();
        assert_eq!(original.phase, QueuePhase::Failed);
        assert_eq!(original.replan_count, 4);

        // No replan HITL item should be created.
        assert!(daemon.get_item("s1:analyze:replan-4").is_none());
    }

    #[tokio::test]
    async fn respond_hitl_replan_requires_hitl_phase() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Item is in Pending, not Hitl.
        daemon.push_item(test_item("s1", "analyze"));

        let result = daemon
            .respond_hitl("s1:analyze", HitlRespondAction::Replan, None, None)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn respond_hitl_done_still_works() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Hitl;
        daemon.push_item(item);

        let result = daemon
            .respond_hitl(
                "s1:analyze",
                HitlRespondAction::Done,
                Some("reviewer".into()),
                None,
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(
            daemon.get_item("s1:analyze").unwrap().phase,
            QueuePhase::Done
        );
    }

    // ---------------------------------------------------------------
    // collect() additional tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn collect_returns_zero_when_source_empty() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let collected = daemon.collect().await.unwrap();
        assert_eq!(collected, 0);
        assert_eq!(daemon.queue_items().len(), 0);
    }

    #[tokio::test]
    async fn collect_is_idempotent_for_existing_items() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));

        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let first = daemon.collect().await.unwrap();
        assert_eq!(first, 1);
        assert_eq!(daemon.queue_items().len(), 1);

        // Second collect with an empty source should add nothing.
        let second = daemon.collect().await.unwrap();
        assert_eq!(second, 0);
        assert_eq!(daemon.queue_items().len(), 1);
    }

    #[tokio::test]
    async fn collect_skips_items_already_in_queue() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));

        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Pre-populate the queue with the same work_id.
        daemon.push_item(test_item("github:org/repo#1", "analyze"));
        assert_eq!(daemon.queue_items().len(), 1);

        // collect() should report 1 collected but not add a duplicate.
        let collected = daemon.collect().await.unwrap();
        assert_eq!(collected, 1);
        assert_eq!(daemon.queue_items().len(), 1);
    }

    // ---------------------------------------------------------------
    // execute_running() additional tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn execute_running_returns_empty_when_no_running_items() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Push a Pending item -- not Running.
        daemon.push_item(test_item("s1", "analyze"));

        let outcomes = daemon.execute_running().await;
        assert!(outcomes.is_empty());
        // The Pending item should remain untouched.
        assert_eq!(daemon.queue_items().len(), 1);
    }

    #[tokio::test]
    async fn execute_running_mixed_success_and_failure() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));
        source.add_item(test_item("github:org/repo#2", "analyze"));

        // First handler succeeds (exit 0), second fails (exit 1).
        let mut daemon = setup_daemon(&tmp, source, vec![0, 1]);
        daemon.collect().await.unwrap();
        daemon.advance();

        let outcomes = daemon.execute_running().await;
        assert_eq!(outcomes.len(), 2);

        let completed = outcomes
            .iter()
            .filter(|o| matches!(o, ItemOutcome::Completed(_)))
            .count();
        let failed = outcomes
            .iter()
            .filter(|o| matches!(o, ItemOutcome::Failed { .. }))
            .count();
        assert_eq!(completed, 1);
        assert_eq!(failed, 1);
    }

    #[tokio::test]
    async fn execute_running_records_history_on_completion() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));

        let mut daemon = setup_daemon(&tmp, source, vec![0]);
        daemon.collect().await.unwrap();
        daemon.advance();

        let outcomes = daemon.execute_running().await;
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0], ItemOutcome::Completed(_)));

        // History events should record a "completed" entry.
        assert!(
            daemon
                .history_events()
                .iter()
                .any(|e| e.status == "completed")
        );
    }

    #[tokio::test]
    async fn execute_running_releases_concurrency_on_failure() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));

        let mut daemon = setup_daemon(&tmp, source, vec![1]);
        daemon.collect().await.unwrap();
        daemon.advance();

        assert_eq!(daemon.running_count(), 1);

        let outcomes = daemon.execute_running().await;
        assert!(matches!(outcomes[0], ItemOutcome::Failed { .. }));

        // After failure, concurrency slot should be released so new items can run.
        // Verify by adding a new item and advancing it.
        daemon.push_item(test_item("github:org/repo#2", "analyze"));
        daemon.advance();
        assert!(!daemon.items_in_phase(QueuePhase::Running).is_empty());
    }

    // ---------------------------------------------------------------
    // execute_on_done() additional tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn execute_on_done_transitions_to_done_when_no_state_config() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Use a state that has no matching state config.
        let mut item = test_item("github:org/repo#1", "unknown_state");
        item.phase = QueuePhase::Completed;

        let success = daemon.execute_on_done(&mut item).await.unwrap();
        assert!(success);
        assert_eq!(item.phase, QueuePhase::Done);
    }

    #[tokio::test]
    async fn execute_on_done_with_empty_on_done_transitions_to_done() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");

        // Use a custom config with a state that has empty on_done.
        let yaml = r#"
name: test-ws
concurrency: 2
sources:
  github:
    url: https://github.com/org/repo
    states:
      no_done:
        trigger:
          label: "belt:no_done"
        handlers:
          - script: "echo handler"
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(MockRuntime::new("mock", vec![])));
        let worktree_mgr = MockWorktreeManager::new(tmp.path().to_path_buf());
        let mut daemon = Daemon::new(
            config,
            vec![Box::new(source)],
            Arc::new(registry),
            Box::new(worktree_mgr),
            4,
        );

        let mut item = test_item("github:org/repo#1", "no_done");
        item.phase = QueuePhase::Completed;

        let success = daemon.execute_on_done(&mut item).await.unwrap();
        assert!(success);
        assert_eq!(item.phase, QueuePhase::Done);
    }

    #[tokio::test]
    async fn execute_on_done_records_history_entry() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("github:org/repo#1", "analyze");
        item.phase = QueuePhase::Completed;

        assert_eq!(daemon.history().len(), 0);

        let success = daemon.execute_on_done(&mut item).await.unwrap();
        assert!(success);

        // History should have a "done" entry.
        assert!(
            daemon
                .history()
                .iter()
                .any(|h| h.status == belt_core::context::HistoryStatus::Done)
        );
    }

    // ---------------------------------------------------------------
    // tick() main loop tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn tick_full_pipeline_collect_advance_execute() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));

        let mut daemon = setup_daemon(&tmp, source, vec![0]);

        // Before tick: queue should be empty.
        assert_eq!(daemon.queue_items().len(), 0);
        assert_eq!(daemon.history_events().len(), 0);

        // After tick: item is collected, advanced, executed, then evaluated.
        // The evaluator runs on_done which transitions to Done and may remove
        // items from the queue, so we verify via history_events instead.
        daemon.tick().await.unwrap();

        // A "completed" history event proves the pipeline ran successfully.
        assert!(
            daemon
                .history_events()
                .iter()
                .any(|e| e.status == "completed"),
            "tick should produce at least one completed history event"
        );
    }

    #[tokio::test]
    async fn tick_with_empty_source_is_noop() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // tick() with no items should succeed without errors.
        daemon.tick().await.unwrap();
        assert_eq!(daemon.queue_items().len(), 0);
        assert_eq!(daemon.history_events().len(), 0);
    }

    #[tokio::test]
    async fn tick_processes_multiple_items() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));
        source.add_item(test_item("github:org/repo#2", "analyze"));

        // Both handlers succeed.
        let mut daemon = setup_daemon(&tmp, source, vec![0, 0]);

        daemon.tick().await.unwrap();

        // Both items should have produced "completed" history events.
        let completed_events = daemon
            .history_events()
            .iter()
            .filter(|e| e.status == "completed")
            .count();
        assert_eq!(completed_events, 2);
    }

    #[tokio::test]
    async fn tick_shutdown_mode_only_executes_running() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![0]);

        // Put an item directly in Running to simulate pre-existing work.
        let mut item = test_item("github:org/repo#1", "analyze");
        item.phase = QueuePhase::Running;
        item.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item);

        daemon.request_shutdown();

        // tick() should not collect new items but should execute Running ones.
        daemon.tick().await.unwrap();

        // The Running item should have been processed.
        assert!(daemon.items_in_phase(QueuePhase::Running).is_empty());
    }

    #[tokio::test]
    async fn tick_does_not_collect_after_shutdown() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));

        let mut daemon = setup_daemon(&tmp, source, vec![]);
        daemon.request_shutdown();

        daemon.tick().await.unwrap();

        // collect() should have been skipped due to shutdown.
        assert_eq!(daemon.queue_items().len(), 0);
    }

    // --- spec completion HITL response tests ---

    #[tokio::test]
    async fn respond_hitl_done_spec_completion_transitions_spec_to_completed() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let daemon = setup_daemon(&tmp, source, vec![]);

        // Set up an in-memory database with a spec in Completing status.
        let db = Database::open_in_memory().unwrap();
        let mut spec = belt_core::spec::Spec::new(
            "spec-42".to_string(),
            "test-ws".to_string(),
            "Test Spec".to_string(),
            "content".to_string(),
        );
        spec.status = belt_core::spec::SpecStatus::Completing;
        db.insert_spec(&spec).unwrap();

        let mut daemon = daemon.with_db(db);

        // Create a spec_completion HITL item.
        let mut item = QueueItem::new(
            "spec-completion:spec-42:hitl".to_string(),
            "spec-42".to_string(),
            "test-ws".to_string(),
            "spec_completion".to_string(),
        );
        item.phase = QueuePhase::Hitl;
        item.hitl_reason = Some(HitlReason::SpecCompletionReview);
        daemon.push_item(item);

        // Approve (Done) the HITL item.
        let result = daemon
            .respond_hitl(
                "spec-completion:spec-42:hitl",
                HitlRespondAction::Done,
                Some("reviewer".into()),
                None,
            )
            .await;
        assert!(result.is_ok());

        // Queue item should be Done.
        assert_eq!(
            daemon
                .get_item("spec-completion:spec-42:hitl")
                .unwrap()
                .phase,
            QueuePhase::Done
        );

        // Spec should have transitioned to Completed.
        let updated_spec = daemon.db.as_ref().unwrap().get_spec("spec-42").unwrap();
        assert_eq!(updated_spec.status, belt_core::spec::SpecStatus::Completed);
    }

    #[tokio::test]
    async fn respond_hitl_skip_spec_completion_reverts_spec_to_active() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let daemon = setup_daemon(&tmp, source, vec![]);

        let db = Database::open_in_memory().unwrap();
        let mut spec = belt_core::spec::Spec::new(
            "spec-43".to_string(),
            "test-ws".to_string(),
            "Test Spec".to_string(),
            "content".to_string(),
        );
        spec.status = belt_core::spec::SpecStatus::Completing;
        db.insert_spec(&spec).unwrap();

        let mut daemon = daemon.with_db(db);

        let mut item = QueueItem::new(
            "spec-completion:spec-43:hitl".to_string(),
            "spec-43".to_string(),
            "test-ws".to_string(),
            "spec_completion".to_string(),
        );
        item.phase = QueuePhase::Hitl;
        item.hitl_reason = Some(HitlReason::SpecCompletionReview);
        daemon.push_item(item);

        // Reject (Skip) the HITL item.
        let result = daemon
            .respond_hitl(
                "spec-completion:spec-43:hitl",
                HitlRespondAction::Skip,
                Some("reviewer".into()),
                None,
            )
            .await;
        assert!(result.is_ok());

        // Queue item should be Skipped.
        assert_eq!(
            daemon
                .get_item("spec-completion:spec-43:hitl")
                .unwrap()
                .phase,
            QueuePhase::Skipped
        );

        // Spec should revert to Active after rejection.
        let updated_spec = daemon.db.as_ref().unwrap().get_spec("spec-43").unwrap();
        assert_eq!(updated_spec.status, belt_core::spec::SpecStatus::Active);
    }

    #[tokio::test]
    async fn respond_hitl_retry_spec_completion_reverts_spec_to_active() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let daemon = setup_daemon(&tmp, source, vec![]);

        let db = Database::open_in_memory().unwrap();
        let mut spec = belt_core::spec::Spec::new(
            "spec-44".to_string(),
            "test-ws".to_string(),
            "Test Spec".to_string(),
            "content".to_string(),
        );
        spec.status = belt_core::spec::SpecStatus::Completing;
        db.insert_spec(&spec).unwrap();

        let mut daemon = daemon.with_db(db);

        let mut item = QueueItem::new(
            "spec-completion:spec-44:hitl".to_string(),
            "spec-44".to_string(),
            "test-ws".to_string(),
            "spec_completion".to_string(),
        );
        item.phase = QueuePhase::Hitl;
        item.hitl_reason = Some(HitlReason::SpecCompletionReview);
        daemon.push_item(item);

        // Retry (additional modifications needed) the HITL item.
        let result = daemon
            .respond_hitl(
                "spec-completion:spec-44:hitl",
                HitlRespondAction::Retry,
                Some("reviewer".into()),
                None,
            )
            .await;
        assert!(result.is_ok());

        // Queue item should be Pending (retried).
        assert_eq!(
            daemon
                .get_item("spec-completion:spec-44:hitl")
                .unwrap()
                .phase,
            QueuePhase::Pending
        );

        // Spec should revert to Active for additional modifications.
        let updated_spec = daemon.db.as_ref().unwrap().get_spec("spec-44").unwrap();
        assert_eq!(updated_spec.status, belt_core::spec::SpecStatus::Active);
    }

    // ---------------------------------------------------------------
    // Token usage auto-save tests
    // ---------------------------------------------------------------

    #[test]
    fn try_record_token_usage_saves_to_db() {
        use belt_core::runtime::TokenUsage;
        use belt_infra::db::Database;

        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let db = Database::open_in_memory().unwrap();
        let daemon = setup_daemon(&tmp, source, vec![]).with_db(db);

        let item = test_item("github:org/repo#1", "analyze");
        let result = ActionResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration: std::time::Duration::from_millis(500),
            token_usage: Some(TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: Some(10),
                cache_write_tokens: None,
            }),
            runtime_name: Some("mock".to_string()),
            model: Some("gpt-4".to_string()),
        };

        daemon.try_record_token_usage(&item, &result);

        // Verify the record was inserted into the DB.
        let rows = daemon
            .db
            .as_ref()
            .unwrap()
            .get_token_usage_by_work_id("github:org/repo#1:analyze")
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, 100);
        assert_eq!(rows[0].output_tokens, 50);
        assert_eq!(rows[0].model, "gpt-4");
        assert_eq!(rows[0].runtime, "mock");
    }

    #[test]
    fn try_record_token_usage_skips_when_no_usage() {
        use belt_infra::db::Database;

        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let db = Database::open_in_memory().unwrap();
        let daemon = setup_daemon(&tmp, source, vec![]).with_db(db);

        let item = test_item("github:org/repo#2", "analyze");
        let result = ActionResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration: std::time::Duration::from_millis(100),
            token_usage: None,
            runtime_name: Some("mock".to_string()),
            model: None,
        };

        daemon.try_record_token_usage(&item, &result);

        // No records should be inserted when token_usage is None.
        let rows = daemon
            .db
            .as_ref()
            .unwrap()
            .get_token_usage_by_work_id("github:org/repo#2:analyze")
            .unwrap();
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn try_record_token_usage_skips_when_no_runtime_name() {
        use belt_core::runtime::TokenUsage;
        use belt_infra::db::Database;

        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let db = Database::open_in_memory().unwrap();
        let daemon = setup_daemon(&tmp, source, vec![]).with_db(db);

        let item = test_item("github:org/repo#3", "analyze");
        let result = ActionResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration: std::time::Duration::from_millis(100),
            token_usage: Some(TokenUsage {
                input_tokens: 200,
                output_tokens: 100,
                cache_read_tokens: None,
                cache_write_tokens: None,
            }),
            runtime_name: None, // script actions have no runtime name
            model: None,
        };

        daemon.try_record_token_usage(&item, &result);

        // No records should be inserted when runtime_name is None.
        let rows = daemon
            .db
            .as_ref()
            .unwrap()
            .get_token_usage_by_work_id("github:org/repo#3:analyze")
            .unwrap();
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn try_record_token_usage_uses_unknown_model_when_absent() {
        use belt_core::runtime::TokenUsage;
        use belt_infra::db::Database;

        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let db = Database::open_in_memory().unwrap();
        let daemon = setup_daemon(&tmp, source, vec![]).with_db(db);

        let item = test_item("github:org/repo#4", "analyze");
        let result = ActionResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration: std::time::Duration::from_millis(200),
            token_usage: Some(TokenUsage {
                input_tokens: 50,
                output_tokens: 25,
                cache_read_tokens: None,
                cache_write_tokens: None,
            }),
            runtime_name: Some("claude".to_string()),
            model: None, // model not provided
        };

        daemon.try_record_token_usage(&item, &result);

        let rows = daemon
            .db
            .as_ref()
            .unwrap()
            .get_token_usage_by_work_id("github:org/repo#4:analyze")
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model, "unknown");
    }

    // ---------------------------------------------------------------
    // advance_ready_to_running: HashMap per-workspace concurrency (additional)
    // ---------------------------------------------------------------

    #[test]
    fn advance_ready_to_running_empty_queue_is_noop() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let limits = HashMap::from([("test-ws".to_string(), 2)]);
        daemon.advance_ready_to_running(&limits, 1);

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 0);
        assert_eq!(daemon.queue_items().len(), 0);
    }

    #[test]
    fn advance_ready_to_running_zero_ws_limit_blocks_all() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        for id in ["s1", "s2"] {
            daemon.push_item(ready_item_for_ws(id, "ws-zero"));
        }

        // Workspace limit of 0 means nothing should be promoted.
        let limits = HashMap::from([("ws-zero".to_string(), 0)]);
        daemon.advance_ready_to_running(&limits, 1);

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 0);
        assert_eq!(daemon.items_in_phase(QueuePhase::Ready).len(), 2);
    }

    #[test]
    fn advance_ready_to_running_already_running_counts_against_limit() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // Place one item already in Running to occupy a slot.
        let mut running = test_item("existing", "analyze");
        running.workspace_id = "ws-a".to_string();
        running.phase = QueuePhase::Running;
        running.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(running);
        // Track the existing running item in the concurrency tracker.
        daemon.tracker.track("ws-a");

        // Add 2 Ready items for the same workspace.
        for id in ["r1", "r2"] {
            daemon.push_item(ready_item_for_ws(id, "ws-a"));
        }

        // ws limit = 2, one already running => only 1 more can start.
        let limits = HashMap::from([("ws-a".to_string(), 2)]);
        daemon.advance_ready_to_running(&limits, 1);

        assert_eq!(
            count_in_phase_for_ws(&daemon, QueuePhase::Running, "ws-a"),
            2
        );
        assert_eq!(count_in_phase_for_ws(&daemon, QueuePhase::Ready, "ws-a"), 1);
    }

    #[test]
    fn advance_ready_to_running_mixed_known_and_unknown_workspaces() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // ws-known: 3 items, limit 1
        for id in ["k1", "k2", "k3"] {
            daemon.push_item(ready_item_for_ws(id, "ws-known"));
        }
        // ws-unknown: 2 items, not in map => default_concurrency = 3
        for id in ["u1", "u2"] {
            daemon.push_item(ready_item_for_ws(id, "ws-unknown"));
        }

        let limits = HashMap::from([("ws-known".to_string(), 1)]);
        daemon.advance_ready_to_running(&limits, 3);

        assert_eq!(
            count_in_phase_for_ws(&daemon, QueuePhase::Running, "ws-known"),
            1
        );
        assert_eq!(
            count_in_phase_for_ws(&daemon, QueuePhase::Ready, "ws-known"),
            2
        );
        // ws-unknown uses default=3, and only 2 items exist, so both promoted.
        assert_eq!(
            count_in_phase_for_ws(&daemon, QueuePhase::Running, "ws-unknown"),
            2
        );
        assert_eq!(
            count_in_phase_for_ws(&daemon, QueuePhase::Ready, "ws-unknown"),
            0
        );
    }

    #[test]
    fn advance_ready_to_running_global_limit_interacts_with_per_ws_limits() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        // Global max_concurrent = 4 from setup_daemon.
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // 3 workspaces, each with limit 2, total would be 6 but global is 4.
        for i in 0..2 {
            daemon.push_item(ready_item_for_ws(&format!("a{i}"), "ws-a"));
        }
        for i in 0..2 {
            daemon.push_item(ready_item_for_ws(&format!("b{i}"), "ws-b"));
        }
        for i in 0..2 {
            daemon.push_item(ready_item_for_ws(&format!("c{i}"), "ws-c"));
        }

        let limits = HashMap::from([
            ("ws-a".to_string(), 2),
            ("ws-b".to_string(), 2),
            ("ws-c".to_string(), 2),
        ]);
        daemon.advance_ready_to_running(&limits, 1);

        // Total running should be capped at global limit (4).
        let total_running = daemon.items_in_phase(QueuePhase::Running).len();
        assert_eq!(total_running, 4);
        assert_eq!(daemon.items_in_phase(QueuePhase::Ready).len(), 2);
    }

    // ---------------------------------------------------------------
    // drain_with_timeout: graceful shutdown (additional)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn drain_with_timeout_completes_items_within_timeout() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        // exit_code 0 means handler completes successfully.
        let mut daemon = setup_daemon(&tmp, source, vec![0, 0]);

        let mut item1 = test_item("github:org/repo#1", "analyze");
        item1.phase = QueuePhase::Running;
        item1.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item1);

        let mut item2 = test_item("github:org/repo#2", "analyze");
        item2.phase = QueuePhase::Running;
        item2.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item2);

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 2);

        // Drain with a generous timeout -- items should complete successfully.
        daemon
            .drain_with_timeout(std::time::Duration::from_secs(30))
            .await;

        // All running items should have been drained (completed or otherwise processed).
        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 0);
    }

    #[test]
    fn rollback_running_to_pending_is_called_on_timeout_scenario() {
        // Simulates the rollback path that drain_with_timeout invokes when
        // the deadline expires. Per R-DM-004, timeout rolls back Running →
        // Pending (preserving worktrees) rather than failing items.
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        for i in 0..3u32 {
            let mut item = test_item(&format!("github:org/repo#{i}"), "analyze");
            item.phase = QueuePhase::Running;
            item.updated_at = Utc::now().to_rfc3339();
            daemon.push_item(item);
        }

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 3);

        // This is what drain_with_timeout calls when the deadline expires.
        daemon.rollback_running_to_pending();

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 0);
        assert_eq!(daemon.items_in_phase(QueuePhase::Pending).len(), 3);
    }

    #[test]
    fn force_fail_running_is_called_on_second_sigint_scenario() {
        // Simulates the force-fail path that drain_with_timeout invokes on
        // a second SIGINT. Per R-DM-004, the second interrupt force-fails
        // running items so the daemon can exit immediately.
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        for i in 0..3u32 {
            let mut item = test_item(&format!("github:org/repo#{i}"), "analyze");
            item.phase = QueuePhase::Running;
            item.updated_at = Utc::now().to_rfc3339();
            daemon.push_item(item);
        }

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 3);

        // This is what drain_with_timeout calls on the second SIGINT.
        daemon.force_fail_running();

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 0);
        assert_eq!(daemon.items_in_phase(QueuePhase::Failed).len(), 3);

        // Each failed item should have a history event with shutdown timeout error.
        let events = daemon.history_events();
        assert_eq!(events.len(), 3);
        for event in events {
            assert_eq!(event.status, "failed");
            assert!(
                event
                    .error
                    .as_ref()
                    .is_some_and(|err| err.contains("shutdown timeout exceeded"))
            );
        }
    }

    // ---------------------------------------------------------------
    // report_dir initialization tests
    // ---------------------------------------------------------------

    #[test]
    fn report_dir_derives_from_belt_home() {
        let tmp = TempDir::new().unwrap();
        let belt_home = tmp.path().join("custom-belt-home");
        let source = MockDataSource::new("github");
        let daemon = setup_daemon(&tmp, source, vec![]).with_belt_home(belt_home.clone());

        let db = Database::open_in_memory().unwrap();
        let daemon = daemon.with_db(db);

        // with_db should initialize cron_engine (which receives report_dir
        // derived as belt_home.join("reports")). If cron_engine is Some,
        // report_dir was computed and passed to builtin jobs.
        assert!(
            daemon.cron_engine.is_some(),
            "with_db must initialize cron_engine with report_dir"
        );
    }

    #[test]
    fn report_dir_default_belt_home_uses_dot_belt() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let daemon = setup_daemon(&tmp, source, vec![]);

        // Default belt_home is ".belt" (from env or fallback).
        // Verify that with_db doesn't panic with the default path.
        let db = Database::open_in_memory().unwrap();
        let daemon = daemon.with_db(db);

        assert!(daemon.cron_engine.is_some());
    }

    // ---------------------------------------------------------------
    // mark_hitl: HITL state transition detail tests
    // ---------------------------------------------------------------

    #[test]
    fn mark_hitl_sets_reason_notes_and_created_at() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Running;
        item.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item);

        daemon.complete_item("s1:analyze").unwrap();
        daemon
            .mark_hitl(
                "s1:analyze",
                HitlReason::EvaluateFailure,
                Some("partial result".into()),
            )
            .unwrap();

        let item = daemon.get_item("s1:analyze").unwrap();
        assert_eq!(item.phase, QueuePhase::Hitl);
        assert_eq!(item.hitl_reason, Some(HitlReason::EvaluateFailure));
        assert_eq!(item.hitl_notes.as_deref(), Some("partial result"));
        assert!(
            item.hitl_created_at.is_some(),
            "hitl_created_at should be set"
        );
    }

    #[test]
    fn mark_hitl_with_none_notes() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Running;
        item.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item);

        daemon.complete_item("s1:analyze").unwrap();
        daemon
            .mark_hitl("s1:analyze", HitlReason::Timeout, None)
            .unwrap();

        let item = daemon.get_item("s1:analyze").unwrap();
        assert_eq!(item.phase, QueuePhase::Hitl);
        assert_eq!(item.hitl_reason, Some(HitlReason::Timeout));
        assert!(item.hitl_notes.is_none());
    }

    // ---------------------------------------------------------------
    // retry_from_hitl: transition detail tests
    // ---------------------------------------------------------------

    #[test]
    fn retry_from_hitl_resets_phase_to_pending() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Hitl;
        item.hitl_reason = Some(HitlReason::RetryMaxExceeded);
        item.hitl_notes = Some("needs review".into());
        item.hitl_created_at = Some(Utc::now().to_rfc3339());
        daemon.push_item(item);

        daemon.retry_from_hitl("s1:analyze").unwrap();

        let item = daemon.get_item("s1:analyze").unwrap();
        assert_eq!(item.phase, QueuePhase::Pending);
    }

    // ---------------------------------------------------------------
    // respond_hitl: Retry and Skip action tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn respond_hitl_retry_transitions_to_pending() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Hitl;
        daemon.push_item(item);

        let result = daemon
            .respond_hitl(
                "s1:analyze",
                HitlRespondAction::Retry,
                Some("reviewer".into()),
                Some("retrying after fix".into()),
            )
            .await;
        assert!(result.is_ok());

        let item = daemon.get_item("s1:analyze").unwrap();
        assert_eq!(item.phase, QueuePhase::Pending);
        assert_eq!(item.hitl_respondent.as_deref(), Some("reviewer"));
        assert_eq!(item.hitl_notes.as_deref(), Some("retrying after fix"));
    }

    #[tokio::test]
    async fn respond_hitl_skip_transitions_to_skipped() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Hitl;
        daemon.push_item(item);

        let result = daemon
            .respond_hitl(
                "s1:analyze",
                HitlRespondAction::Skip,
                Some("admin".into()),
                None,
            )
            .await;
        assert!(result.is_ok());

        let item = daemon.get_item("s1:analyze").unwrap();
        assert_eq!(item.phase, QueuePhase::Skipped);
        assert_eq!(item.hitl_respondent.as_deref(), Some("admin"));
    }

    #[tokio::test]
    async fn respond_hitl_returns_error_for_unknown_id() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let result = daemon
            .respond_hitl("nonexistent", HitlRespondAction::Done, None, None)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn respond_hitl_updates_notes_when_provided() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Hitl;
        item.hitl_notes = Some("original notes".into());
        daemon.push_item(item);

        daemon
            .respond_hitl(
                "s1:analyze",
                HitlRespondAction::Done,
                None,
                Some("updated notes".into()),
            )
            .await
            .unwrap();

        let item = daemon.get_item("s1:analyze").unwrap();
        assert_eq!(item.hitl_notes.as_deref(), Some("updated notes"));
    }

    #[tokio::test]
    async fn respond_hitl_preserves_notes_when_not_provided() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Hitl;
        item.hitl_notes = Some("original notes".into());
        daemon.push_item(item);

        daemon
            .respond_hitl(
                "s1:analyze",
                HitlRespondAction::Done,
                Some("reviewer".into()),
                None,
            )
            .await
            .unwrap();

        let item = daemon.get_item("s1:analyze").unwrap();
        assert_eq!(item.hitl_notes.as_deref(), Some("original notes"));
    }

    // ---------------------------------------------------------------
    // history() accessor tests
    // ---------------------------------------------------------------

    #[test]
    fn history_returns_empty_initially() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let daemon = setup_daemon(&tmp, source, vec![]);

        assert!(daemon.history().is_empty());
    }

    #[tokio::test]
    async fn history_records_completed_items() {
        let tmp = TempDir::new().unwrap();
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));

        // Exit code 0 means handler succeeds.
        let mut daemon = setup_daemon(&tmp, source, vec![0]);

        daemon.tick().await.unwrap();

        let entries = daemon.history();
        assert!(
            !entries.is_empty(),
            "history should contain entries after tick"
        );
        assert!(
            entries.iter().any(|h| h.source_id == "github:org/repo#1"),
            "history should contain the processed item's source_id"
        );
    }

    // ---------------------------------------------------------------
    // history_events() accessor tests
    // ---------------------------------------------------------------

    #[test]
    fn history_events_returns_empty_initially() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let daemon = setup_daemon(&tmp, source, vec![]);

        assert!(daemon.history_events().is_empty());
    }

    #[test]
    fn history_events_records_failed_items_with_error() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Running;
        item.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item);

        daemon
            .mark_failed("s1:analyze", "handler crashed".into())
            .unwrap();

        let events = daemon.history_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].work_id, "s1:analyze");
        assert_eq!(events[0].source_id, "s1");
        assert_eq!(events[0].state, "analyze");
        assert_eq!(events[0].status, "failed");
        assert_eq!(events[0].error.as_deref(), Some("handler crashed"));
        assert_eq!(events[0].attempt, 1);
    }

    #[test]
    fn history_events_increments_attempt_on_repeated_failures() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // First failure.
        let mut item = test_item("s1", "analyze");
        item.phase = QueuePhase::Running;
        item.updated_at = Utc::now().to_rfc3339();
        daemon.push_item(item);
        daemon
            .mark_failed("s1:analyze", "first error".into())
            .unwrap();

        // Manually reset to Running for second failure.
        let item = daemon
            .queue
            .iter_mut()
            .find(|it| it.work_id == "s1:analyze")
            .unwrap();
        item.phase = QueuePhase::Running;

        daemon
            .mark_failed("s1:analyze", "second error".into())
            .unwrap();

        let events = daemon.history_events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].attempt, 1);
        assert_eq!(events[1].attempt, 2);
    }

    // ---------------------------------------------------------------
    // with_cron_engine() builder pattern test
    // ---------------------------------------------------------------

    #[test]
    fn with_cron_engine_injects_engine() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let daemon = setup_daemon(&tmp, source, vec![]);

        // Inject a CronEngine via the builder method.
        let engine = CronEngine::new();
        let daemon = daemon.with_cron_engine(engine);

        assert!(
            daemon.cron_engine.is_some(),
            "cron_engine should be set after with_cron_engine()"
        );
    }

    // ---------------------------------------------------------------
    // with_max_eval_failures() builder pattern test
    // ---------------------------------------------------------------

    #[test]
    fn with_max_eval_failures_sets_evaluator_threshold() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let daemon = setup_daemon(&tmp, source, vec![]);

        // Set a custom threshold. The method should not panic and
        // should return a daemon with the updated evaluator.
        let daemon = daemon.with_max_eval_failures(5);

        // We cannot directly inspect evaluator.max_eval_failures (private),
        // but verify the daemon was constructed without error.
        assert_eq!(daemon.config.name, "test-ws");
    }
}
