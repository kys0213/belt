use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::Result;

use belt_core::action::Action;
use belt_core::context::HistoryEntry;
use belt_core::escalation::EscalationAction;
use belt_core::phase::QueuePhase;
use belt_core::queue::QueueItem;
use belt_core::runtime::RuntimeRegistry;
use belt_core::source::DataSource;
use belt_core::state_machine;
use belt_core::workspace::{StateConfig, WorkspaceConfig};
use belt_infra::worktree::WorktreeManager;

use crate::concurrency::ConcurrencyTracker;
use crate::executor::{ActionEnv, ActionExecutor};

/// Daemon — 상태 머신 + yaml prompt/script 실행기.
pub struct Daemon {
    config: WorkspaceConfig,
    sources: Vec<Box<dyn DataSource>>,
    executor: ActionExecutor,
    worktree_mgr: Box<dyn WorktreeManager>,
    tracker: ConcurrencyTracker,
    queue: VecDeque<QueueItem>,
    history: Vec<HistoryEntry>,
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
        }
    }

    /// 1단계: DataSource에서 새 아이템을 수집하여 Pending 큐에 추가.
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

    /// 2단계: Pending → Ready → Running 자동 전이.
    pub fn advance(&mut self) -> usize {
        let mut advanced = 0;

        for item in self.queue.iter_mut() {
            if item.phase == QueuePhase::Pending
                && state_machine::transit(QueuePhase::Pending, QueuePhase::Ready).is_ok()
            {
                item.phase = QueuePhase::Ready;
                advanced += 1;
            }
        }

        let ws_id = &self.config.name;
        let ws_concurrency = self
            .config
            .sources
            .values()
            .next()
            .map(|s| s.concurrency)
            .unwrap_or(1);

        for item in self.queue.iter_mut() {
            if item.phase == QueuePhase::Ready
                && self.tracker.can_spawn_in_workspace(ws_id, ws_concurrency)
            {
                item.phase = QueuePhase::Running;
                self.tracker.track(ws_id);
                advanced += 1;
            }
        }

        advanced
    }

    /// 3단계: Running 아이템의 handler를 실행.
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
        if let Err(e) = self.executor.execute_all(&on_enter, &env).await {
            tracing::warn!("on_enter failed for {}: {e}", item.work_id);
        }

        // handler chain
        let handlers: Vec<Action> = state_config.handlers.iter().map(Action::from).collect();
        let result = self.executor.execute_all(&handlers, &env).await;

        match result {
            Ok(Some(r)) if !r.success() => {
                let failure_count = self.count_failures(&item.source_id, &item.state);
                let escalation = self.resolve_escalation(&item.state, failure_count + 1);

                self.record_history(item, "failed", Some(&r.stderr));

                if escalation.should_run_on_fail() {
                    let on_fail: Vec<Action> = state_config.on_fail.iter().map(Action::from).collect();
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
            Ok(_) => {
                item.phase = QueuePhase::Completed;
                self.record_history(item, "completed", None);
                self.tracker.release(&self.config.name.clone());
                self.queue.push_back(item.clone());
                ItemOutcome::Completed(item.clone())
            }
            Err(e) => {
                item.phase = QueuePhase::Failed;
                self.record_history(item, "failed", Some(&e.to_string()));
                self.tracker.release(&self.config.name.clone());

                ItemOutcome::Failed {
                    item: item.clone(),
                    error: e.to_string(),
                    escalation: EscalationAction::Retry,
                }
            }
        }
    }

    /// on_done script 실행. 성공 시 Done, 실패 시 Failed.
    pub async fn execute_on_done(&mut self, item: &mut QueueItem) -> Result<bool> {
        let state_config = self.find_state_config(&item.state).cloned();
        let state_config = match state_config {
            Some(cfg) => cfg,
            None => {
                item.phase = QueuePhase::Done;
                return Ok(true);
            }
        };

        if state_config.on_done.is_empty() {
            item.phase = QueuePhase::Done;
            self.record_history(item, "done", None);
            let worktree = self.worktree_mgr.create_or_reuse(&self.config.name, &item.source_id).await?;
            let _ = self.worktree_mgr.cleanup(&worktree).await;
            return Ok(true);
        }

        let worktree = self.worktree_mgr.create_or_reuse(&self.config.name, &item.source_id).await?;
        let env = ActionEnv::new(&item.work_id, &worktree);
        let on_done: Vec<Action> = state_config.on_done.iter().map(Action::from).collect();
        let result = self.executor.execute_all(&on_done, &env).await?;

        match result {
            Some(r) if !r.success() => {
                item.phase = QueuePhase::Failed;
                self.record_history(item, "failed", Some("on_done script failed"));
                Ok(false)
            }
            _ => {
                item.phase = QueuePhase::Done;
                self.record_history(item, "done", None);
                let _ = self.worktree_mgr.cleanup(&worktree).await;
                Ok(true)
            }
        }
    }

    /// Daemon tick: collect → advance → execute → on_done.
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
                ItemOutcome::Failed { item, error, escalation } => {
                    tracing::warn!("failed: {} (escalation={:?}, error={})", item.work_id, escalation, error);
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

    /// tokio::select! 기반 async event loop.
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

    // --- helpers ---

    fn find_state_config(&self, state: &str) -> Option<&StateConfig> {
        for source in self.config.sources.values() {
            if let Some(cfg) = source.states.get(state) {
                return Some(cfg);
            }
        }
        None
    }

    fn count_failures(&self, _source_id: &str, state: &str) -> u32 {
        self.history
            .iter()
            .filter(|h| h.state == state && h.status == belt_core::context::HistoryStatus::Failed)
            .count() as u32
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

    fn record_history(&mut self, item: &QueueItem, status: &str, error: Option<&str>) {
        let attempt = self.history.iter().filter(|h| h.state == item.state).count() as u32 + 1;
        self.history.push(HistoryEntry {
            state: item.state.clone(),
            status: status.parse().unwrap_or(belt_core::context::HistoryStatus::Failed),
            attempt,
            summary: None,
            error: error.map(|s| s.to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
        });
    }

    // --- queries ---

    pub fn queue_items(&self) -> &VecDeque<QueueItem> {
        &self.queue
    }

    pub fn items_in_phase(&self, phase: QueuePhase) -> Vec<&QueueItem> {
        self.queue.iter().filter(|i| i.phase == phase).collect()
    }

    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    pub fn push_item(&mut self, item: QueueItem) {
        self.queue.push_back(item);
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
sources:
  github:
    url: https://github.com/org/repo
    concurrency: 2
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
