use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;

use belt_core::action::Action;
use belt_core::context::HistoryEntry;
use belt_core::error::BeltError;
use belt_core::escalation::EscalationAction;
use belt_core::phase::QueuePhase;
use belt_core::queue::{HistoryEvent, QueueItem};
use belt_core::runtime::RuntimeRegistry;
use belt_core::source::DataSource;
use belt_core::state_machine;
use belt_core::workspace::{StateConfig, WorkspaceConfig};
use belt_infra::db::Database;
use belt_infra::worktree::WorktreeManager;

use crate::concurrency::ConcurrencyTracker;
use crate::executor::{ActionEnv, ActionExecutor};

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
    executor: ActionExecutor,
    worktree_mgr: Box<dyn WorktreeManager>,
    tracker: ConcurrencyTracker,
    queue: VecDeque<QueueItem>,
    history: Vec<HistoryEntry>,
    db: Option<Database>,
    /// History events with full lineage information for failure tracking.
    history_events: Vec<HistoryEvent>,
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

impl Daemon {
    pub fn new(
        config: WorkspaceConfig,
        sources: Vec<Box<dyn DataSource>>,
        registry: Arc<RuntimeRegistry>,
        worktree_mgr: Box<dyn WorktreeManager>,
        max_concurrent: u32,
    ) -> Self {
        Self {
            config,
            sources,
            executor: ActionExecutor::new(registry),
            worktree_mgr,
            tracker: ConcurrencyTracker::new(max_concurrent),
            queue: VecDeque::new(),
            history: Vec::new(),
            db: None,
            history_events: Vec::new(),
        }
    }

    /// Set the database for persisting token usage records.
    pub fn with_db(mut self, db: Database) -> Self {
        self.db = Some(db);
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

        // Pending -> Ready (uses safe transit)
        for item in self.queue.iter_mut() {
            if item.phase == QueuePhase::Pending
                && state_machine::transit(QueuePhase::Pending, QueuePhase::Ready).is_ok()
                && transit(item, QueuePhase::Ready).is_ok()
            {
                advanced += 1;
            }
        }

        // Ready -> Running (respecting concurrency)
        let ws_id = &self.config.name;
        let ws_concurrency = self.config.concurrency;

        for item in self.queue.iter_mut() {
            if item.phase == QueuePhase::Ready
                && self.tracker.can_spawn_in_workspace(&ws_id, ws_concurrency)
                && transit(item, QueuePhase::Running).is_ok()
            {
                self.tracker.track(&ws_id);
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

    // ---------------------------------------------------------------
    // Phase 3: Execute running items
    // ---------------------------------------------------------------

    /// Execute handlers for all Running items.
    pub async fn execute_running(&mut self) -> Vec<ItemOutcome> {
        let mut outcomes = Vec::new();

        let running_indices: Vec<usize> = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, item)| item.phase == QueuePhase::Running)
            .map(|(i, _)| i)
            .collect();

        for &idx in running_indices.iter().rev() {
            let mut item = self.queue.remove(idx).unwrap();
            let outcome = self.execute_item(&mut item).await;
            outcomes.push(outcome);
        }

        outcomes
    }

    async fn execute_item(&mut self, item: &mut QueueItem) -> ItemOutcome {
        let state_config = self.find_state_config(&item.state);

        let state_config = match state_config {
            Some(cfg) => cfg.clone(),
            None => {
                item.phase = QueuePhase::Skipped;
                return ItemOutcome::Skipped(item.clone());
            }
        };

        let worktree = match self
            .worktree_mgr
            .create_or_reuse(&self.config.name, &item.source_id)
            .await
        {
            Ok(path) => path,
            Err(e) => {
                item.phase = QueuePhase::Failed;
                return ItemOutcome::Failed {
                    item: item.clone(),
                    error: format!("worktree creation failed: {e}"),
                    escalation: EscalationAction::Retry,
                };
            }
        };

        let env = ActionEnv::new(&item.work_id, &worktree);

        // on_enter
        let on_enter: Vec<Action> = state_config.on_enter.iter().map(Action::from).collect();
        match self.executor.execute_all(&on_enter, &env).await {
            Ok(Some(r)) if !r.success() => {
                // 비정상 exit code → escalation 적용
                self.record_history(item, "failed", Some(&r.stderr));
                let failure_count = self.count_failures(&item.source_id, &item.state);
                let escalation = self.resolve_escalation(&item.state, failure_count);
                if escalation.should_run_on_fail() {
                    let on_fail: Vec<Action> =
                        state_config.on_fail.iter().map(Action::from).collect();
                    let _ = self.executor.execute_all(&on_fail, &env).await;
                }
                self.handle_escalation(item, escalation);
                self.tracker.release(&self.config.name.clone());

                return ItemOutcome::Failed {
                    item: item.clone(),
                    error: format!("on_enter failed with exit code {}", r.exit_code),
                    escalation,
                };
            }
            Err(e) => {
                // 실행 자체가 실패한 경우
                self.record_history(item, "failed", Some(&e.to_string()));
                let failure_count = self.count_failures(&item.source_id, &item.state);
                let escalation = self.resolve_escalation(&item.state, failure_count);
                if escalation.should_run_on_fail() {
                    let on_fail: Vec<Action> =
                        state_config.on_fail.iter().map(Action::from).collect();
                    let _ = self.executor.execute_all(&on_fail, &env).await;
                }
                self.handle_escalation(item, escalation);
                self.tracker.release(&self.config.name.clone());

                return ItemOutcome::Failed {
                    item: item.clone(),
                    error: format!("on_enter failed: {e}"),
                    escalation,
                };
            }
            _ => {
                // 성공 → handler 진행
            }
        }

        // handler chain
        let handlers: Vec<Action> = state_config.handlers.iter().map(Action::from).collect();
        let result = self.executor.execute_all(&handlers, &env).await;

        match result {
            Ok(Some(r)) if !r.success() => {
                self.try_record_token_usage(item, &r);

                let failure_count = self.count_failures(&item.source_id, &item.state);
                let escalation = self.resolve_escalation(&item.state, failure_count + 1);

                self.record_history(item, "failed", Some(&r.stderr));
                self.record_history_event(item, "failed", Some(r.stderr.clone()));

                if escalation.should_run_on_fail() {
                    let on_fail: Vec<Action> =
                        state_config.on_fail.iter().map(Action::from).collect();
                    let _ = self.executor.execute_all(&on_fail, &env).await;
                }

                self.handle_escalation(item, escalation);
                self.tracker.release(&self.config.name.clone());

                ItemOutcome::Failed {
                    item: item.clone(),
                    error: r.stderr.clone(),
                    escalation,
                }
            }
            Ok(Some(r)) => {
                self.try_record_token_usage(item, &r);

                let _ = transit(item, QueuePhase::Completed);
                self.record_history(item, "completed", None);
                self.tracker.release(&self.config.name.clone());
                self.queue.push_back(item.clone());
                ItemOutcome::Completed(item.clone())
            }
            Ok(None) => {
                let _ = transit(item, QueuePhase::Completed);
                self.record_history(item, "completed", None);
                self.tracker.release(&self.config.name.clone());
                self.queue.push_back(item.clone());
                ItemOutcome::Completed(item.clone())
            }
            Err(e) => {
                item.phase = QueuePhase::Failed;
                self.record_history(item, "failed", Some(&e.to_string()));
                self.record_history_event(item, "failed", Some(e.to_string()));
                self.tracker.release(&self.config.name.clone());

                ItemOutcome::Failed {
                    item: item.clone(),
                    error: e.to_string(),
                    escalation: EscalationAction::Retry,
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
    pub fn mark_done(&mut self, work_id: &str) -> Result<(), BeltError> {
        let item = self
            .queue
            .iter_mut()
            .find(|it| it.work_id == work_id)
            .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))?;
        transit(item, QueuePhase::Done)
    }

    /// Mark a Completed item as Hitl (human-in-the-loop).
    pub fn mark_hitl(&mut self, work_id: &str, _reason: Option<String>) -> Result<(), BeltError> {
        let item = self
            .queue
            .iter_mut()
            .find(|it| it.work_id == work_id)
            .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))?;
        transit(item, QueuePhase::Hitl)
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

    /// Mark a Running item as Failed and record a HistoryEvent.
    pub fn mark_failed(&mut self, work_id: &str, error: String) -> Result<(), BeltError> {
        let item = self
            .queue
            .iter_mut()
            .find(|it| it.work_id == work_id)
            .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))?;

        let source_id = item.source_id.clone();
        let state = item.state.clone();

        transit(item, QueuePhase::Failed)?;

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
                let _ = self.mark_hitl(work_id, Some("escalation: repeated failures".to_string()));
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
            let worktree = self
                .worktree_mgr
                .create_or_reuse(&self.config.name, &item.source_id)
                .await;
            if let Ok(ref wt) = worktree {
                let _ = self.worktree_mgr.cleanup(wt).await;
            }
            return Ok(true);
        }

        let worktree = self
            .worktree_mgr
            .create_or_reuse(&self.config.name, &item.source_id)
            .await?;
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
                let _ = self.worktree_mgr.cleanup(&worktree).await;
                Ok(true)
            }
        }
    }

    /// Daemon tick: collect -> advance -> execute -> on_done.
    pub async fn tick(&mut self) -> Result<()> {
        let collected = self.collect().await?;
        if collected > 0 {
            tracing::info!("collected {collected} items");
        }

        let advanced = self.advance();
        if advanced > 0 {
            tracing::debug!("advanced {advanced} items");
        }

        let outcomes = self.execute_running().await;
        for outcome in &outcomes {
            match outcome {
                ItemOutcome::Completed(item) => tracing::info!("completed: {}", item.work_id),
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

        let completed: Vec<String> = self
            .items_in_phase(QueuePhase::Completed)
            .iter()
            .map(|i| i.work_id.clone())
            .collect();

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

        Ok(())
    }

    /// tokio::select! based async event loop.
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
                    tracing::info!("received SIGINT, shutting down...");
                    break;
                }
            }
        }

        tracing::info!("belt daemon stopped");
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
        match action {
            EscalationAction::Retry | EscalationAction::RetryWithComment => {
                let mut retry_item = item.clone();
                retry_item.phase = QueuePhase::Pending;
                retry_item.updated_at = chrono::Utc::now().to_rfc3339();
                self.queue.push_back(retry_item);
            }
            EscalationAction::Skip => {
                item.phase = QueuePhase::Skipped;
            }
            EscalationAction::Hitl | EscalationAction::Replan => {
                item.phase = QueuePhase::Hitl;
                self.queue.push_back(item.clone());
            }
        }
    }

    /// Token usage가 있으면 DB에 기록한다. DB가 없거나 기록 실패 시 경고만 출력.
    fn try_record_token_usage(&self, item: &QueueItem, result: &crate::executor::ActionResult) {
        let (Some(usage), Some(runtime_name)) = (&result.token_usage, &result.runtime_name) else {
            return;
        };

        if let Some(ref db) = self.db {
            let model = result.model.as_deref().unwrap_or("unknown");
            if let Err(e) =
                db.record_token_usage(&item.work_id, &self.config.name, runtime_name, model, usage)
            {
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
        let worktree_mgr = MockWorktreeManager::new(tmp.path());

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
                .mark_hitl("s1:analyze", Some("needs review".into()))
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
}
