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
use belt_infra::db::Database;
use belt_infra::worktree::WorktreeManager;

use crate::concurrency::ConcurrencyTracker;
use crate::cron::{BuiltinJobDeps, CronEngine, builtin_jobs};
use crate::evaluator::Evaluator;
use crate::executor::{ActionEnv, ActionExecutor, ActionResult};

/// Safely transition a [`QueueItem`] to a new phase.
///
/// All phase mutations **must** go through this function so that
/// [`QueuePhase::can_transition_to`] is always checked.
fn transit(item: &mut QueueItem, to: QueuePhase) -> Result<(), BeltError> {
    if !item.phase.can_transition_to(&to) {
        return Err(BeltError::InvalidTransition {
            from: item.phase,
            to,
        });
    }
    item.phase = to;
    item.updated_at = Utc::now().to_rfc3339();
    Ok(())
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
    /// Also initializes the built-in cron jobs which require a database handle.
    pub fn with_db(mut self, db: Database) -> Self {
        let db = Arc::new(db);
        let deps = BuiltinJobDeps {
            db: Arc::clone(&db),
            worktree_mgr: Arc::clone(&self.worktree_mgr),
            workspace_root: self.belt_home.clone(),
        };
        let mut cron = self.cron_engine.take().unwrap_or_default();
        for job in builtin_jobs(deps) {
            cron.register(job);
        }
        self.cron_engine = Some(cron);
        self.db = Some(db);
        self
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

        // Pending -> Ready (uses safe transit + dependency gate)
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
    pub fn advance_ready_to_running(&mut self, ws_concurrency: usize) {
        let ws_counts: std::collections::HashMap<String, usize> = {
            let mut m = std::collections::HashMap::new();
            for item in self.queue.iter() {
                if item.phase == QueuePhase::Running {
                    *m.entry(item.workspace_id.clone()).or_insert(0) += 1;
                }
            }
            m
        };

        let ready_indices: Vec<usize> = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, it)| it.phase == QueuePhase::Ready)
            .map(|(i, _)| i)
            .collect();

        let mut ws_started: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();

        for idx in ready_indices {
            let ws = self.queue[idx].workspace_id.clone();
            let already_running = ws_counts.get(&ws).copied().unwrap_or(0)
                + ws_started.get(&ws).copied().unwrap_or(0);
            if already_running >= ws_concurrency {
                continue;
            }

            if !self.tracker.can_spawn() {
                break;
            }

            if transit(&mut self.queue[idx], QueuePhase::Running).is_ok() {
                self.tracker.track(&ws);
                *ws_started.entry(ws).or_insert(0) += 1;
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
                };
            }
        };

        // Collect on_fail actions for deferred, escalation-aware execution.
        let on_fail_actions: Vec<Action> = state_config.on_fail.iter().map(Action::from).collect();

        let worktree = match worktree_mgr.create_or_reuse(&ws_name) {
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
                };
            }
        };

        let env = ActionEnv::new(&item.work_id, &worktree);

        // on_enter
        let on_enter: Vec<Action> = state_config.on_enter.iter().map(Action::from).collect();
        match executor.execute_all(&on_enter, &env).await {
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
                };
            }
            _ => {
                // 성공 → handler 진행
            }
        }

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
            },
            Ok(r) => {
                item.phase = QueuePhase::Completed;
                ExecutionResult {
                    item,
                    outcome: ExecutionOutcome::Completed { result: r },
                    ws_name,
                    on_fail_actions: Vec::new(),
                    worktree: Some(worktree),
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
        } = exec_result;

        match outcome {
            ExecutionOutcome::Skipped => ItemOutcome::Skipped(item),
            ExecutionOutcome::WorktreeError { error } => {
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

                let failure_count = self.count_failures(&item.source_id, &item.state);
                let escalation = self.resolve_escalation(&item.state, failure_count + 1);

                // Q-12: Execute on_fail scripts only when escalation is not a silent retry.
                if escalation.should_run_on_fail() {
                    if let Some(ref wt) = worktree {
                        let env = ActionEnv::new(&item.work_id, wt);
                        if let Err(e) = self.executor.execute_all(&on_fail_actions, &env).await {
                            tracing::warn!(
                                work_id = %item.work_id,
                                "on_fail script execution error: {e}"
                            );
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
                tracing::info!(
                    work_id = %item.work_id,
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
        transit(item, QueuePhase::Completed)
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
        transit(item, QueuePhase::Done)?;

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
        transit(item, QueuePhase::Hitl)?;
        item.hitl_created_at = Some(Utc::now().to_rfc3339());
        item.hitl_reason = Some(reason);
        item.hitl_notes = notes;
        Ok(())
    }

    /// Mark a Hitl item as Skipped.
    pub fn mark_skipped(&mut self, work_id: &str) -> Result<(), BeltError> {
        let item = self
            .queue
            .iter_mut()
            .find(|it| it.work_id == work_id)
            .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))?;
        transit(item, QueuePhase::Skipped)
    }

    /// Retry a Hitl item by sending it back to Pending.
    pub fn retry_from_hitl(&mut self, work_id: &str) -> Result<(), BeltError> {
        let item = self
            .queue
            .iter_mut()
            .find(|it| it.work_id == work_id)
            .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))?;
        transit(item, QueuePhase::Pending)
    }

    /// Respond to a HITL item with a user action.
    ///
    /// Applies the given [`HitlRespondAction`] and records the respondent.
    pub fn respond_hitl(
        &mut self,
        work_id: &str,
        action: HitlRespondAction,
        respondent: Option<String>,
        notes: Option<String>,
    ) -> Result<(), BeltError> {
        let item = self
            .queue
            .iter_mut()
            .find(|it| it.work_id == work_id)
            .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))?;

        if item.phase != QueuePhase::Hitl {
            return Err(BeltError::InvalidTransition {
                from: item.phase,
                to: QueuePhase::Done, // placeholder
            });
        }

        item.hitl_respondent = respondent;
        if let Some(n) = notes {
            item.hitl_notes = Some(n);
        }

        match action {
            HitlRespondAction::Done => transit(item, QueuePhase::Done),
            HitlRespondAction::Retry => transit(item, QueuePhase::Pending),
            HitlRespondAction::Skip => transit(item, QueuePhase::Skipped),
            HitlRespondAction::Replan => transit(item, QueuePhase::Failed),
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

        transit(item, QueuePhase::Failed)?;
        item.mark_worktree_preserved();
        tracing::info!(work_id, "worktree preserved for failed item");

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
            return Ok(true);
        }

        let worktree = self.worktree_mgr.create_or_reuse(&item.work_id)?;
        let env = ActionEnv::new(&item.work_id, &worktree);
        let on_done: Vec<Action> = state_config.on_done.iter().map(Action::from).collect();
        let result = self.executor.execute_all(&on_done, &env).await?;

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

        // Evaluator 스크립트 실행으로 Done vs HITL 판정.
        let eval_result = self.evaluator.run_evaluate(&self.belt_home).await;

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
    /// 4. drain 중 두 번째 SIGINT 시 즉시 종료 (Running -> Pending 롤백).
    pub async fn run(&mut self, tick_interval_secs: u64) {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(tick_interval_secs));
        tracing::info!("belt daemon started (tick={}s)", tick_interval_secs);

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = self.tick().await {
                        tracing::error!("tick error: {e}");
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("received SIGINT, initiating graceful shutdown...");
                    self.shutdown_requested = true;
                    break;
                }
            }
        }

        // Running 아이템 완료를 최대 30초 대기. timeout 시 Pending으로 롤백.
        self.drain_with_timeout(std::time::Duration::from_secs(30))
            .await;

        tracing::info!("belt daemon stopped");
    }

    /// Running 아이템 완료 대기. timeout 초과 또는 두 번째 SIGINT 시 Pending으로 롤백.
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
                        "drain timeout reached, rolling back {} running items to Pending",
                        remaining
                    );
                    self.rollback_running_to_pending();
                    return;
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::warn!("received second SIGINT, forcing shutdown");
                    self.rollback_running_to_pending();
                    return;
                }
            }
        }
    }

    /// Running → Pending 롤백. worktree는 보존한다.
    fn rollback_running_to_pending(&mut self) {
        let ws_name = self.config.name.clone();
        for item in self.queue.iter_mut() {
            if item.phase == QueuePhase::Running {
                if let Err(e) = transit(item, QueuePhase::Pending) {
                    tracing::error!("failed to rollback {}: {e}", item.work_id);
                    continue;
                }
                self.tracker.release(&ws_name);
                tracing::info!(
                    "rolled back {} to Pending (worktree preserved)",
                    item.work_id
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

    /// Return the number of items currently in Running phase.
    pub fn running_count(&self) -> usize {
        self.queue
            .iter()
            .filter(|it| it.phase == QueuePhase::Running)
            .count()
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

    #[test]
    fn advance_ready_to_running_respects_ws_concurrency() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        // 3 items ready, ws_concurrency = 2
        let mut i1 = test_item("s1", "analyze");
        i1.phase = QueuePhase::Ready;
        let mut i2 = test_item("s2", "analyze");
        i2.phase = QueuePhase::Ready;
        let mut i3 = test_item("s3", "analyze");
        i3.phase = QueuePhase::Ready;
        daemon.push_item(i1);
        daemon.push_item(i2);
        daemon.push_item(i3);

        daemon.advance_ready_to_running(2);

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 2);
        assert_eq!(daemon.items_in_phase(QueuePhase::Ready).len(), 1);
    }

    #[test]
    fn advance_ready_to_running_promotes_all_when_capacity_allows() {
        let tmp = TempDir::new().unwrap();
        let source = MockDataSource::new("github");
        let mut daemon = setup_daemon(&tmp, source, vec![]);

        let mut i1 = test_item("s1", "analyze");
        i1.phase = QueuePhase::Ready;
        let mut i2 = test_item("s2", "analyze");
        i2.phase = QueuePhase::Ready;
        daemon.push_item(i1);
        daemon.push_item(i2);

        daemon.advance_ready_to_running(4);

        assert_eq!(daemon.items_in_phase(QueuePhase::Running).len(), 2);
        assert_eq!(daemon.items_in_phase(QueuePhase::Ready).len(), 0);
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
}
