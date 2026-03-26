use std::collections::HashMap;

/// 2단계 concurrency 제어.
///
/// Level 1: workspace별 제한 (workspace.concurrency)
/// Level 2: 전역 제한 (daemon.max_concurrent)
///
/// evaluate LLM 호출도 concurrency slot을 소비한다.
/// `active_evaluates` 카운트가 `total`에 포함되어 전역 제한을 공유한다.
pub struct ConcurrencyTracker {
    per_workspace: HashMap<String, usize>,
    total: usize,
    max_total: usize,
    /// 현재 실행 중인 evaluate LLM 호출 수. `total`에 포함된다.
    active_evaluates: usize,
}

impl ConcurrencyTracker {
    pub fn new(max_total: u32) -> Self {
        Self {
            per_workspace: HashMap::new(),
            total: 0,
            max_total: max_total as usize,
            active_evaluates: 0,
        }
    }

    pub fn can_spawn(&self) -> bool {
        self.total < self.max_total
    }

    pub fn can_spawn_in_workspace(&self, workspace_id: &str, workspace_limit: u32) -> bool {
        if !self.can_spawn() {
            return false;
        }
        let current = self.per_workspace.get(workspace_id).copied().unwrap_or(0);
        current < workspace_limit as usize
    }

    /// Returns the number of additional items that can be spawned in this workspace,
    /// respecting both per-workspace and global limits.
    pub fn available_slots(&self, workspace_id: &str, workspace_limit: u32) -> usize {
        if !self.can_spawn() {
            return 0;
        }
        let ws_current = self.per_workspace.get(workspace_id).copied().unwrap_or(0);
        let ws_available = (workspace_limit as usize).saturating_sub(ws_current);
        let global_available = self.max_total.saturating_sub(self.total);
        ws_available.min(global_available)
    }

    pub fn track(&mut self, workspace_id: &str) {
        *self
            .per_workspace
            .entry(workspace_id.to_string())
            .or_insert(0) += 1;
        self.total += 1;
    }

    pub fn release(&mut self, workspace_id: &str) {
        if let Some(count) = self.per_workspace.get_mut(workspace_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_workspace.remove(workspace_id);
            }
        }
        self.total = self.total.saturating_sub(1);
    }

    /// Track an evaluate LLM call. Consumes a global concurrency slot.
    pub fn track_evaluate(&mut self) {
        self.active_evaluates += 1;
        self.total += 1;
    }

    /// Release an evaluate LLM call slot.
    pub fn release_evaluate(&mut self) {
        self.active_evaluates = self.active_evaluates.saturating_sub(1);
        self.total = self.total.saturating_sub(1);
    }

    /// Number of active evaluate LLM calls.
    pub fn active_evaluate_count(&self) -> usize {
        self.active_evaluates
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn workspace_count(&self, workspace_id: &str) -> usize {
        self.per_workspace.get(workspace_id).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_track_release() {
        let mut tracker = ConcurrencyTracker::new(4);
        assert!(tracker.can_spawn());
        tracker.track("ws1");
        assert_eq!(tracker.total(), 1);
        tracker.release("ws1");
        assert_eq!(tracker.total(), 0);
    }

    #[test]
    fn global_limit() {
        let mut tracker = ConcurrencyTracker::new(2);
        tracker.track("ws1");
        tracker.track("ws2");
        assert!(!tracker.can_spawn());
        tracker.release("ws1");
        assert!(tracker.can_spawn());
    }

    #[test]
    fn workspace_limit() {
        let mut tracker = ConcurrencyTracker::new(10);
        tracker.track("ws1");
        tracker.track("ws1");
        assert!(!tracker.can_spawn_in_workspace("ws1", 2));
        assert!(tracker.can_spawn_in_workspace("ws2", 2));
    }

    #[test]
    fn available_slots_min_of_ws_and_global() {
        let mut tracker = ConcurrencyTracker::new(3);
        assert_eq!(tracker.available_slots("ws1", 5), 3);
        tracker.track("ws1");
        assert_eq!(tracker.available_slots("ws1", 5), 2);
        tracker.track("ws2");
        assert_eq!(tracker.available_slots("ws1", 5), 1);
    }

    #[test]
    fn release_saturating() {
        let mut tracker = ConcurrencyTracker::new(4);
        tracker.release("ws1");
        assert_eq!(tracker.total(), 0);
    }

    #[test]
    fn evaluate_consumes_global_slot() {
        let mut tracker = ConcurrencyTracker::new(2);
        tracker.track_evaluate();
        assert_eq!(tracker.total(), 1);
        assert_eq!(tracker.active_evaluate_count(), 1);
        assert!(tracker.can_spawn());

        tracker.track_evaluate();
        assert_eq!(tracker.total(), 2);
        assert!(!tracker.can_spawn());
    }

    #[test]
    fn evaluate_and_handler_share_slots() {
        let mut tracker = ConcurrencyTracker::new(3);
        tracker.track("ws1");
        tracker.track_evaluate();
        assert_eq!(tracker.total(), 2);
        assert!(tracker.can_spawn());

        tracker.track("ws2");
        assert!(!tracker.can_spawn());

        tracker.release_evaluate();
        assert!(tracker.can_spawn());
        assert_eq!(tracker.active_evaluate_count(), 0);
    }

    #[test]
    fn release_evaluate_saturating() {
        let mut tracker = ConcurrencyTracker::new(4);
        tracker.release_evaluate();
        assert_eq!(tracker.active_evaluate_count(), 0);
        assert_eq!(tracker.total(), 0);
    }
}
