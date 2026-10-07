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

use belt_core::phase::QueuePhase;
use belt_core::queue::QueueItem;
use belt_core::state_machine;
use belt_core::transition::{Actor, TransitionOutcome, TransitionReason, TransitionRequest};
use belt_infra::db::Database;

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

/// Drives queue items through the advance phase of the daemon lifecycle.
///
/// Responsibilities:
/// 1. Filter items for advance eligibility (queue dependency gate)
/// 2. Update `QueueItem.phase` via `transit` / state_machine
/// 3. Emit transition events to the database
/// 4. Return updated count for the daemon to log
pub struct Advancer<'a> {
    queue: &'a mut VecDeque<QueueItem>,
    tracker: &'a mut ConcurrencyTracker,
    db: &'a Option<Arc<Database>>,
    ws_name: &'a str,
    ws_concurrency: u32,
    /// Items whose claim lost to a writer that finished them (Done or
    /// Skipped). They leave the queue once the loop that holds indices ends.
    finished_by_others: Vec<String>,
}

impl<'a> Advancer<'a> {
    /// Create a new `Advancer` with borrowed daemon state.
    pub fn new(
        queue: &'a mut VecDeque<QueueItem>,
        tracker: &'a mut ConcurrencyTracker,
        db: &'a Option<Arc<Database>>,
        ws_name: &'a str,
        ws_concurrency: u32,
    ) -> Self {
        Self {
            queue,
            tracker,
            db,
            ws_name,
            ws_concurrency,
            finished_by_others: Vec::new(),
        }
    }

    /// Remove the copies whose claim found a finished row.
    ///
    /// Deferred to the end of a run because the loops address the queue by
    /// index.
    fn drop_finished_by_others(&mut self) {
        let finished = std::mem::take(&mut self.finished_by_others);
        self.queue.retain(|item| !finished.contains(&item.work_id));
    }

    /// Auto-transition Pending -> Ready -> Running (respecting concurrency).
    ///
    /// Every transition goes through [`Database::transition`]. An item whose
    /// stored phase differs (another process moved it) is not advanced and
    /// its in-memory phase follows the stored one.
    ///
    /// Returns the number of items that were successfully transitioned.
    ///
    /// # Panics
    /// When no database is configured. `Daemon::tick` rejects that case with
    /// an error before reaching here.
    pub fn run(&mut self) -> usize {
        self.require_db();
        let mut advanced = 0;

        let pending_indices: Vec<usize> = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, item)| item.phase() == QueuePhase::Pending)
            .map(|(i, _)| i)
            .collect();

        for idx in pending_indices {
            if self.claim(idx, QueuePhase::Ready) {
                advanced += 1;
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

            if self.claim(idx, QueuePhase::Running) {
                self.tracker.track(self.ws_name);
                advanced += 1;
            }
        }

        self.drop_finished_by_others();
        advanced
    }

    /// Advance Pending items to Ready.
    ///
    /// # Panics
    /// When no database is configured.
    pub fn advance_pending_to_ready(&mut self) {
        self.require_db();
        let pending_indices: Vec<usize> = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, item)| item.phase() == QueuePhase::Pending)
            .map(|(i, _)| i)
            .collect();

        for idx in pending_indices {
            self.claim(idx, QueuePhase::Ready);
        }
        self.drop_finished_by_others();
    }

    fn require_db(&self) -> &Database {
        self.db
            .as_deref()
            .expect("Advancer requires a database: queue state is owned by SQLite")
    }

    /// Move the item at `idx` to `to` through the store and mirror the result.
    ///
    /// Returns `true` only when the store applied the transition. On a
    /// conflict the stored phase wins: the in-memory phase follows it and the
    /// caller must not start any work for the item.
    fn claim(&mut self, idx: usize, to: QueuePhase) -> bool {
        let db = self.require_db();
        let item = &self.queue[idx];
        let from = item.phase();
        if state_machine::transit(from, to).is_err() {
            tracing::error!(work_id = %item.work_id, ?from, ?to, "undefined transition skipped");
            return false;
        }
        let request = TransitionRequest {
            work_id: item.work_id.clone(),
            expected_from: from,
            to,
            actor: Actor::Daemon,
            reason: TransitionReason::Advance,
            detail: None,
        };
        match db.transition(&request) {
            Ok(TransitionOutcome::Applied { .. }) => {
                if let Err(e) = transit(&mut self.queue[idx], to) {
                    tracing::error!(
                        work_id = %request.work_id,
                        "in-memory transit after applied claim failed: {e}"
                    );
                    return false;
                }
                true
            }
            // An `InvalidAction` naming another phase means the row moved on
            // (a Hitl row refuses every claim this way): follow it like a conflict.
            Ok(TransitionOutcome::Conflict { current })
            | Ok(TransitionOutcome::InvalidAction { current })
                if current != from =>
            {
                tracing::info!(
                    work_id = %request.work_id,
                    expected = ?from,
                    current = ?current,
                    "claim lost to another writer; following stored phase"
                );
                self.queue[idx].set_phase_unchecked(current);
                if matches!(current, QueuePhase::Done | QueuePhase::Skipped) {
                    self.finished_by_others.push(request.work_id.clone());
                }
                false
            }
            Ok(
                outcome @ (TransitionOutcome::Conflict { .. }
                | TransitionOutcome::Busy { .. }
                | TransitionOutcome::InvalidAction { .. }),
            ) => {
                tracing::error!(work_id = %request.work_id, ?outcome, "claim transition rejected");
                false
            }
            Err(e) => {
                tracing::error!(work_id = %request.work_id, "claim transition failed: {e}");
                false
            }
        }
    }

    /// Advance Ready items to Running, respecting both per-workspace and global concurrency.
    ///
    /// `ws_concurrency_limits` maps workspace IDs to their concurrency limits.
    /// Workspaces not present in the map use `default_concurrency` (falls back to 1).
    ///
    /// # Panics
    /// When no database is configured.
    pub fn advance_ready_to_running(
        &mut self,
        ws_concurrency_limits: &HashMap<String, u32>,
        default_concurrency: u32,
    ) {
        self.require_db();
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

            if self.claim(idx, QueuePhase::Running) {
                self.tracker.track(&ws);
            }
        }
        self.drop_finished_by_others();
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
                    // Dependency not found in in-memory queue — fall back to DB
                    // to handle system restart scenarios where the dependency
                    // was completed in a previous session.
                    match db.get_item(dep_id) {
                        Ok(item) if item.phase() == QueuePhase::Done => {}
                        Ok(item) => {
                            tracing::trace!(
                                work_id = %work_id,
                                dependency = %dep_id,
                                dependency_phase = %item.phase().as_str(),
                                "queue dependency not done (DB lookup)"
                            );
                            return false;
                        }
                        Err(belt_core::error::BeltError::ItemNotFound(_)) => {
                            // Not in DB either — gate open (original behavior).
                        }
                        Err(err) => {
                            // DB error — gate open for stability (safe default).
                            tracing::warn!(
                                work_id = %work_id,
                                dependency = %dep_id,
                                error = %err,
                                "DB lookup failed for dependency; keeping gate open"
                            );
                        }
                    }
                }
            }
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::queue::testing::test_item;

    fn make_queue(items: Vec<QueueItem>) -> VecDeque<QueueItem> {
        items.into_iter().collect()
    }

    /// In-memory store holding a row for every queued item.
    fn db_with(queue: &VecDeque<QueueItem>) -> Option<Arc<Database>> {
        let db = Database::open_in_memory().expect("in-memory DB");
        for item in queue {
            db.insert_item(item).expect("insert_item");
        }
        Some(Arc::new(db))
    }

    #[test]
    fn run_advances_pending_through_ready_to_running() {
        let mut queue = make_queue(vec![test_item("w1", "analyze")]);
        let mut tracker = ConcurrencyTracker::new(4);
        let db = db_with(&queue);

        let mut advancer = Advancer::new(&mut queue, &mut tracker, &db, "test-ws", 2);

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
        let db = db_with(&queue);

        // ws_concurrency = 1, so only one item should reach Running
        let mut advancer = Advancer::new(&mut queue, &mut tracker, &db, "test-ws", 1);

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
        let db = db_with(&queue);

        let mut advancer = Advancer::new(&mut queue, &mut tracker, &db, "test-ws", 2);

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
        let db = db_with(&queue);

        let mut limits = HashMap::new();
        limits.insert("ws-a".to_string(), 1);
        limits.insert("ws-b".to_string(), 1);

        let mut advancer = Advancer::new(&mut queue, &mut tracker, &db, "test-ws", 2);

        advancer.advance_ready_to_running(&limits, 1);

        assert!(queue.iter().all(|i| i.phase() == QueuePhase::Running));
    }

    #[test]
    fn run_empty_queue_is_noop() {
        let mut queue: VecDeque<QueueItem> = VecDeque::new();
        let mut tracker = ConcurrencyTracker::new(4);
        let db = db_with(&queue);

        let mut advancer = Advancer::new(&mut queue, &mut tracker, &db, "test-ws", 2);

        let advanced = advancer.run();
        assert_eq!(advanced, 0);
    }

    #[test]
    fn dependency_gate_db_fallback_done_passes() {
        // When a dependency is not in the in-memory queue but exists in DB
        // as Done, the gate should pass.
        let db = Database::open_in_memory().expect("in-memory DB");
        let db = Arc::new(db);

        // Insert dependency item into DB and mark it Done.
        let mut dep_item = test_item("dep-src", "analyze");
        dep_item.work_id = "dep-work-id".to_string();
        db.insert_item(&dep_item).unwrap();
        db.update_phase("dep-work-id", QueuePhase::Ready).unwrap();
        db.update_phase("dep-work-id", QueuePhase::Running).unwrap();
        db.update_phase("dep-work-id", QueuePhase::Done).unwrap();

        // Insert the item that depends on it.
        let mut item = test_item("my-src", "implement");
        item.work_id = "my-work-id".to_string();
        db.insert_item(&item).unwrap();
        db.add_queue_dependency("my-work-id", "dep-work-id")
            .unwrap();

        // In-memory queue has only the current item, NOT the dependency.
        let mut queue = make_queue(vec![item]);
        let mut tracker = ConcurrencyTracker::new(4);
        let db_opt: Option<Arc<Database>> = Some(Arc::clone(&db));

        let advancer = Advancer::new(&mut queue, &mut tracker, &db_opt, "test-ws", 2);

        assert!(advancer.check_queue_dependency_gate("my-work-id"));
    }

    #[test]
    fn dependency_gate_db_fallback_not_done_blocks() {
        // When a dependency is not in the in-memory queue and exists in DB
        // but is NOT Done, the gate should block.
        let db = Database::open_in_memory().expect("in-memory DB");
        let db = Arc::new(db);

        // Insert dependency item into DB and leave it at Ready (not Done).
        let mut dep_item = test_item("dep-src", "analyze");
        dep_item.work_id = "dep-work-id".to_string();
        db.insert_item(&dep_item).unwrap();
        db.update_phase("dep-work-id", QueuePhase::Ready).unwrap();

        // Insert the item that depends on it.
        let mut item = test_item("my-src", "implement");
        item.work_id = "my-work-id".to_string();
        db.insert_item(&item).unwrap();
        db.add_queue_dependency("my-work-id", "dep-work-id")
            .unwrap();

        // In-memory queue has only the current item, NOT the dependency.
        let mut queue = make_queue(vec![item]);
        let mut tracker = ConcurrencyTracker::new(4);
        let db_opt: Option<Arc<Database>> = Some(Arc::clone(&db));

        let advancer = Advancer::new(&mut queue, &mut tracker, &db_opt, "test-ws", 2);

        assert!(!advancer.check_queue_dependency_gate("my-work-id"));
    }

    #[test]
    fn dependency_gate_db_fallback_not_found_passes() {
        // When a dependency is neither in the in-memory queue nor in DB,
        // the gate should pass (original behavior).
        let db = Database::open_in_memory().expect("in-memory DB");
        let db = Arc::new(db);

        // Insert the item that depends on a non-existent dependency.
        let mut item = test_item("my-src", "implement");
        item.work_id = "my-work-id".to_string();
        db.insert_item(&item).unwrap();
        db.add_queue_dependency("my-work-id", "nonexistent-dep")
            .unwrap();

        let mut queue = make_queue(vec![item]);
        let mut tracker = ConcurrencyTracker::new(4);
        let db_opt: Option<Arc<Database>> = Some(Arc::clone(&db));

        let advancer = Advancer::new(&mut queue, &mut tracker, &db_opt, "test-ws", 2);

        assert!(advancer.check_queue_dependency_gate("my-work-id"));
    }
}
