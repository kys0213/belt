//! Advancer — drives queue items through the Pending -> Ready -> Running
//! state machine transitions.
//!
//! Extracted from [`Daemon`] to improve testability and separation of
//! concerns.  The [`Advancer`] struct borrows the mutable daemon state
//! that the advance phase needs and returns the number of items that
//! were successfully transitioned.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;

use chrono::Utc;

use belt_core::dependency::{DependencyGuard, SpecDependencyGuard};
use belt_core::phase::QueuePhase;
use belt_core::queue::{HitlReason, QueueItem};
use belt_core::state_machine;
use belt_infra::db::{Database, TransitionEvent};

use crate::concurrency::ConcurrencyTracker;

/// Safely transition a [`QueueItem`] to a new phase.
///
/// Delegates to [`QueueItem::transit`] which validates via
/// [`QueuePhase::can_transition_to`].
/// Returns the previous phase on success for transition event recording.
fn transit(
    item: &mut QueueItem,
    to: QueuePhase,
) -> Result<QueuePhase, belt_core::error::BeltError> {
    item.transit(to)
}

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

/// Drives queue items through the advance phase of the daemon lifecycle.
///
/// Responsibilities:
/// 1. Filter items for advance eligibility (dependency gate, conflict detection)
/// 2. Update `QueueItem.phase` via `transit` / state_machine
/// 3. Emit transition events to the database
/// 4. Handle escalation decisions (HITL entry on spec conflicts)
/// 5. Return updated count for the daemon to log
pub struct Advancer<'a> {
    queue: &'a mut VecDeque<QueueItem>,
    tracker: &'a mut ConcurrencyTracker,
    db: &'a Option<Arc<Database>>,
    ws_name: &'a str,
    ws_concurrency: u32,
    dependency_guard: &'a SpecDependencyGuard,
}

impl<'a> Advancer<'a> {
    /// Create a new `Advancer` with borrowed daemon state.
    pub fn new(
        queue: &'a mut VecDeque<QueueItem>,
        tracker: &'a mut ConcurrencyTracker,
        db: &'a Option<Arc<Database>>,
        ws_name: &'a str,
        ws_concurrency: u32,
        dependency_guard: &'a SpecDependencyGuard,
    ) -> Self {
        Self {
            queue,
            tracker,
            db,
            ws_name,
            ws_concurrency,
            dependency_guard,
        }
    }

    /// Auto-transition Pending -> Ready -> Running (respecting concurrency).
    ///
    /// Returns the number of items that were successfully transitioned.
    pub fn run(&mut self) -> usize {
        let mut advanced = 0;

        // Pending -> Ready (uses safe transit + dependency gate + conflict detection)
        let pending_indices: Vec<usize> = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, item)| item.phase() == QueuePhase::Pending)
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
                record_transition(
                    self.db,
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
                    record_transition(
                        self.db,
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

        // Ready -> Running (respecting concurrency + queue_dependencies)
        let ready_indices: Vec<usize> = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, item)| item.phase() == QueuePhase::Ready)
            .map(|(i, _)| i)
            .collect();

        for idx in ready_indices {
            if !self
                .tracker
                .can_spawn_in_workspace(self.ws_name, self.ws_concurrency)
            {
                break;
            }

            // Queue dependency gate: check if all dependency work_ids are Done.
            if !self.check_queue_dependency_gate(&self.queue[idx].work_id.clone()) {
                tracing::debug!(
                    "queue dependency gate blocked: {} (waiting for dependencies)",
                    self.queue[idx].work_id,
                );
                continue;
            }

            if transit(&mut self.queue[idx], QueuePhase::Running).is_ok() {
                record_transition(
                    self.db,
                    &self.queue[idx].work_id,
                    &self.queue[idx].source_id,
                    QueuePhase::Ready,
                    QueuePhase::Running,
                    "phase_enter",
                    None,
                );
                self.tracker.track(self.ws_name);
                advanced += 1;
            }
        }

        advanced
    }

    /// Advance Pending items to Ready.
    pub fn advance_pending_to_ready(&mut self) {
        for item in self.queue.iter_mut() {
            if item.phase() == QueuePhase::Pending {
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
            .filter(|(_, it)| it.phase() == QueuePhase::Ready)
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
    fn check_dependency_gate(&self, source_id: &str) -> bool {
        let db = match self.db {
            Some(db) => db,
            None => return true,
        };

        let spec = match db.get_spec(source_id) {
            Ok(spec) => spec,
            Err(_) => return true,
        };

        let result = self
            .dependency_guard
            .check_dependencies(&spec, |dep_id| db.get_spec(dep_id).ok());

        if !result.is_ready() {
            tracing::trace!("spec {} blocked by dependencies: {:?}", spec.id, result);
        }

        result.is_ready()
    }

    /// Check whether a queue item's queue_dependencies are all Done.
    fn check_queue_dependency_gate(&self, work_id: &str) -> bool {
        let db = match self.db {
            Some(db) => db,
            None => return true,
        };

        let dep_work_ids = match db.list_queue_dependencies(work_id) {
            Ok(deps) => deps,
            Err(_) => return true,
        };

        if dep_work_ids.is_empty() {
            return true;
        }

        for dep_id in &dep_work_ids {
            let dep_phase = self
                .queue
                .iter()
                .find(|item| item.work_id == *dep_id)
                .map(|item| item.phase());

            match dep_phase {
                Some(QueuePhase::Done) => {}
                Some(phase) => {
                    tracing::trace!(
                        work_id = %work_id,
                        dependency = %dep_id,
                        dependency_phase = %phase.as_str(),
                        "queue dependency not done"
                    );
                    return false;
                }
                None => {
                    // Dependency not found in queue -- gate open.
                }
            }
        }

        true
    }

    /// Check whether a queue item's associated spec has entry_point conflicts.
    fn check_conflict_gate(&self, source_id: &str) -> Option<String> {
        let db = match self.db {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::queue::testing::test_item;

    fn make_queue(items: Vec<QueueItem>) -> VecDeque<QueueItem> {
        items.into_iter().collect()
    }

    #[test]
    fn run_advances_pending_through_ready_to_running() {
        let mut queue = make_queue(vec![test_item("w1", "analyze")]);
        let mut tracker = ConcurrencyTracker::new(4);
        let db: Option<Arc<Database>> = None;
        let dep_guard = SpecDependencyGuard;

        let mut advancer = Advancer::new(&mut queue, &mut tracker, &db, "test-ws", 2, &dep_guard);

        let advanced = advancer.run();
        assert_eq!(advanced, 2); // Pending->Ready + Ready->Running
        assert_eq!(queue[0].phase(), QueuePhase::Running);
    }

    #[test]
    fn run_respects_concurrency_limit() {
        let items = vec![
            test_item("w1", "analyze"),
            test_item("w2", "analyze"),
            test_item("w3", "analyze"),
        ];
        let mut queue = make_queue(items);
        let mut tracker = ConcurrencyTracker::new(4);
        let db: Option<Arc<Database>> = None;
        let dep_guard = SpecDependencyGuard;

        // ws_concurrency = 1, so only one item should reach Running
        let mut advancer = Advancer::new(&mut queue, &mut tracker, &db, "test-ws", 1, &dep_guard);

        let _advanced = advancer.run();

        let running_count = queue
            .iter()
            .filter(|i| i.phase() == QueuePhase::Running)
            .count();
        assert_eq!(running_count, 1);
    }

    #[test]
    fn advance_pending_to_ready_transitions_all() {
        let mut queue = make_queue(vec![
            test_item("w1", "analyze"),
            test_item("w2", "implement"),
        ]);
        let mut tracker = ConcurrencyTracker::new(4);
        let db: Option<Arc<Database>> = None;
        let dep_guard = SpecDependencyGuard;

        let mut advancer = Advancer::new(&mut queue, &mut tracker, &db, "test-ws", 2, &dep_guard);

        advancer.advance_pending_to_ready();

        assert!(queue.iter().all(|i| i.phase() == QueuePhase::Ready));
    }

    #[test]
    fn advance_ready_to_running_respects_ws_limits() {
        let mut queue = make_queue(vec![test_item("w1", "analyze"), test_item("w2", "analyze")]);
        // Pre-advance to Ready
        for item in queue.iter_mut() {
            let _ = transit(item, QueuePhase::Ready);
        }
        // Give them different workspace_ids
        queue[0].workspace_id = "ws-a".to_string();
        queue[1].workspace_id = "ws-b".to_string();

        let mut tracker = ConcurrencyTracker::new(4);
        let db: Option<Arc<Database>> = None;
        let dep_guard = SpecDependencyGuard;

        let mut limits = HashMap::new();
        limits.insert("ws-a".to_string(), 1);
        limits.insert("ws-b".to_string(), 1);

        let mut advancer = Advancer::new(&mut queue, &mut tracker, &db, "test-ws", 2, &dep_guard);

        advancer.advance_ready_to_running(&limits, 1);

        assert!(queue.iter().all(|i| i.phase() == QueuePhase::Running));
    }

    #[test]
    fn run_empty_queue_is_noop() {
        let mut queue: VecDeque<QueueItem> = VecDeque::new();
        let mut tracker = ConcurrencyTracker::new(4);
        let db: Option<Arc<Database>> = None;
        let dep_guard = SpecDependencyGuard;

        let mut advancer = Advancer::new(&mut queue, &mut tracker, &db, "test-ws", 2, &dep_guard);

        let advanced = advancer.run();
        assert_eq!(advanced, 0);
    }
}
