use std::collections::HashMap;

/// 2단계 concurrency 제어.
///
/// Level 1: workspace별 제한 (workspace.concurrency)
/// Level 2: 전역 제한 (daemon.max_concurrent)
pub struct ConcurrencyTracker {
    per_workspace: HashMap<String, usize>,
    total: usize,
    max_total: usize,
}

impl ConcurrencyTracker {
    pub fn new(max_total: u32) -> Self {
        Self {
            per_workspace: HashMap::new(),
            total: 0,
            max_total: max_total as usize,
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
}
