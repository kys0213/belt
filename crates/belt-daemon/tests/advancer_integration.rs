//! Integration tests for the Advancer module.
//!
//! Tests phase transitions (Pending -> Ready -> Running), queue dependency
//! gates, and transition event recording
//! using an in-memory SQLite database.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use belt_core::phase::QueuePhase;
use belt_core::queue::QueueItem;
use belt_core::queue::testing::test_item;
use belt_daemon::advancer::Advancer;
use belt_daemon::concurrency::ConcurrencyTracker;
use belt_infra::db::Database;

/// Helper: create a VecDeque from a Vec of QueueItems.
fn make_queue(items: Vec<QueueItem>) -> VecDeque<QueueItem> {
    items.into_iter().collect()
}

/// Helper: create an in-memory DB wrapped in Arc and Option.
fn setup_db() -> (Arc<Database>, Option<Arc<Database>>) {
    let db = Arc::new(Database::open_in_memory().expect("in-memory DB"));
    let db_opt = Some(Arc::clone(&db));
    (db, db_opt)
}

/// Helper: insert a QueueItem into the DB.
fn insert_item_to_db(db: &Database, item: &QueueItem) {
    db.insert_item(item).expect("insert_item");
}

// ---------------------------------------------------------------------------
// Advance cycle: Pending -> Ready -> Running with DB + transition_events
// ---------------------------------------------------------------------------

/// Full advance cycle: Pending -> Ready -> Running with DB.
///
/// Inserts a Pending item, runs the Advancer, and verifies:
/// - The item reaches Running phase
/// - At least one transition event is recorded in the DB
///
/// Note: transition event IDs use `te-{work_id}-{timestamp_millis}`, so
/// two transitions within the same millisecond may collide and only
/// the first is persisted. We verify at least one event is recorded.
#[test]
fn advance_cycle_records_transition_events() {
    let (db, db_opt) = setup_db();
    let item = test_item("src-1", "analyze");
    insert_item_to_db(&db, &item);

    let mut queue = make_queue(vec![item]);
    let mut tracker = ConcurrencyTracker::new(4);

    let mut advancer = Advancer::new(&mut queue, &mut tracker, &db_opt, "test-ws", 2);
    let advanced = advancer.run();

    // Pending -> Ready (1) + Ready -> Running (1) = 2
    assert_eq!(advanced, 2);
    assert_eq!(queue[0].phase(), QueuePhase::Running);

    // Verify at least one transition event was recorded.
    let work_id = &queue[0].work_id;
    let events = db.transitions_of(work_id).expect("transitions_of");
    assert_eq!(events.len(), 2, "one log row per transition");
    // The first recorded event should be Pending -> Ready.
    assert_eq!(events[0].from_phase.as_deref(), Some("pending"));
    assert_eq!(events[0].to_phase.as_deref(), Some("ready"));
    assert_eq!(events[0].kind, "phase_enter");
    assert_eq!(events[0].actor, "daemon");
}

/// Multiple items advance through the full cycle; each records events.
#[test]
fn advance_multiple_items_records_events_per_item() {
    let (db, db_opt) = setup_db();
    let item1 = test_item("src-1", "analyze");
    let item2 = test_item("src-2", "implement");
    insert_item_to_db(&db, &item1);
    insert_item_to_db(&db, &item2);

    let mut queue = make_queue(vec![item1, item2]);
    let mut tracker = ConcurrencyTracker::new(4);

    let mut advancer = Advancer::new(&mut queue, &mut tracker, &db_opt, "test-ws", 4);
    let advanced = advancer.run();

    // Both items: Pending->Ready + Ready->Running = 4 transitions
    assert_eq!(advanced, 4);

    for item in queue.iter() {
        assert_eq!(item.phase(), QueuePhase::Running);
        let events = db.transitions_of(&item.work_id).unwrap();
        assert_eq!(events.len(), 2, "each item logs both claim transitions");
    }
}

// ---------------------------------------------------------------------------
// Queue dependency gate: queue_dependencies blocks Ready -> Running
// ---------------------------------------------------------------------------

/// Queue dependency gate blocks Ready->Running when dependency is not Done.
#[test]
fn queue_dependency_gate_blocks_when_dep_not_done() {
    let (db, db_opt) = setup_db();

    // Insert dependency item in Ready phase (not Done).
    let mut dep_item = test_item("dep-src", "analyze");
    dep_item.work_id = "dep-work".to_string();
    insert_item_to_db(&db, &dep_item);
    db.update_phase("dep-work", QueuePhase::Ready).unwrap();

    // Insert the item that depends on dep-work.
    let mut item = test_item("my-src", "implement");
    item.work_id = "my-work".to_string();
    insert_item_to_db(&db, &item);
    db.add_queue_dependency("my-work", "dep-work").unwrap();

    // Both items are in the in-memory queue. dep_item is at Ready, item is Pending.
    let _ = dep_item.transit(QueuePhase::Ready);
    let mut queue = make_queue(vec![item, dep_item]);
    let mut tracker = ConcurrencyTracker::new(4);

    let mut advancer = Advancer::new(&mut queue, &mut tracker, &db_opt, "test-ws", 4);
    let _advanced = advancer.run();

    // item (index 0) should advance Pending->Ready (1), but NOT Ready->Running
    // because its dependency (dep-work) is not Done.
    // dep_item (index 1) is already Ready, so it advances Ready->Running (1).
    let my_item = queue.iter().find(|i| i.work_id == "my-work").unwrap();
    assert_eq!(
        my_item.phase(),
        QueuePhase::Ready,
        "item should be blocked at Ready by queue dependency"
    );

    let dep = queue.iter().find(|i| i.work_id == "dep-work").unwrap();
    assert_eq!(
        dep.phase(),
        QueuePhase::Running,
        "dependency item should advance to Running"
    );
}

/// Queue dependency gate passes when dependency is Done.
#[test]
fn queue_dependency_gate_passes_when_dep_done() {
    let (db, db_opt) = setup_db();

    // Insert dependency item and mark it Done.
    let mut dep_item = test_item("dep-src", "analyze");
    dep_item.work_id = "dep-work".to_string();
    insert_item_to_db(&db, &dep_item);
    db.update_phase("dep-work", QueuePhase::Ready).unwrap();
    db.update_phase("dep-work", QueuePhase::Running).unwrap();
    db.update_phase("dep-work", QueuePhase::Done).unwrap();

    // Insert the item that depends on dep-work.
    let mut item = test_item("my-src", "implement");
    item.work_id = "my-work".to_string();
    insert_item_to_db(&db, &item);
    db.add_queue_dependency("my-work", "dep-work").unwrap();

    // Only the current item is in the queue (dep is Done, no longer in queue).
    let mut queue = make_queue(vec![item]);
    let mut tracker = ConcurrencyTracker::new(4);

    let mut advancer = Advancer::new(&mut queue, &mut tracker, &db_opt, "test-ws", 2);
    let advanced = advancer.run();

    // Item should advance Pending->Ready + Ready->Running = 2.
    assert_eq!(advanced, 2);
    assert_eq!(queue[0].phase(), QueuePhase::Running);
}

/// Queue dependency gate passes when dependency is Done in the in-memory queue.
#[test]
fn queue_dependency_gate_passes_when_dep_done_in_memory() {
    let (db, db_opt) = setup_db();

    // Insert dependency item Done in memory queue.
    let mut dep_item = test_item("dep-src", "analyze");
    dep_item.work_id = "dep-work".to_string();
    dep_item.set_phase_unchecked(QueuePhase::Done);
    insert_item_to_db(&db, &dep_item);

    // Insert the item that depends on dep-work.
    let mut item = test_item("my-src", "implement");
    item.work_id = "my-work".to_string();
    insert_item_to_db(&db, &item);
    db.add_queue_dependency("my-work", "dep-work").unwrap();

    let mut queue = make_queue(vec![item, dep_item]);
    let mut tracker = ConcurrencyTracker::new(4);

    let mut advancer = Advancer::new(&mut queue, &mut tracker, &db_opt, "test-ws", 4);
    let _advanced = advancer.run();

    let my_item = queue.iter().find(|i| i.work_id == "my-work").unwrap();
    assert_eq!(
        my_item.phase(),
        QueuePhase::Running,
        "item should advance to Running when dep is Done in memory"
    );
}

// ---------------------------------------------------------------------------
// Concurrency: advance respects workspace concurrency limits
// ---------------------------------------------------------------------------

/// Workspace concurrency limit prevents more items from reaching Running.
#[test]
fn advance_respects_ws_concurrency_with_db() {
    let (db, db_opt) = setup_db();

    let items = vec![
        test_item("src-1", "analyze"),
        test_item("src-2", "analyze"),
        test_item("src-3", "analyze"),
    ];
    for item in &items {
        insert_item_to_db(&db, item);
    }

    let mut queue = make_queue(items);
    let mut tracker = ConcurrencyTracker::new(4);

    // ws_concurrency = 1: only 1 item can be Running at a time.
    let mut advancer = Advancer::new(&mut queue, &mut tracker, &db_opt, "test-ws", 1);
    advancer.run();

    let running_count = queue
        .iter()
        .filter(|i| i.phase() == QueuePhase::Running)
        .count();
    let ready_count = queue
        .iter()
        .filter(|i| i.phase() == QueuePhase::Ready)
        .count();

    assert_eq!(running_count, 1, "only 1 item should be Running");
    assert_eq!(ready_count, 2, "2 items should remain Ready");
}

/// advance_ready_to_running respects per-workspace limits with DB.
#[test]
fn advance_ready_to_running_per_ws_limits_with_db() {
    let (db, db_opt) = setup_db();

    let mut item1 = test_item("src-1", "analyze");
    let mut item2 = test_item("src-2", "analyze");
    let _ = item1.transit(QueuePhase::Ready);
    let _ = item2.transit(QueuePhase::Ready);
    item1.workspace_id = "ws-a".to_string();
    item2.workspace_id = "ws-b".to_string();
    insert_item_to_db(&db, &item1);
    insert_item_to_db(&db, &item2);

    let mut queue = make_queue(vec![item1, item2]);
    let mut tracker = ConcurrencyTracker::new(4);

    let mut limits = HashMap::new();
    limits.insert("ws-a".to_string(), 1);
    limits.insert("ws-b".to_string(), 1);

    let mut advancer = Advancer::new(&mut queue, &mut tracker, &db_opt, "test-ws", 2);
    advancer.advance_ready_to_running(&limits, 1);

    assert!(
        queue.iter().all(|i| i.phase() == QueuePhase::Running),
        "both items should reach Running (different workspaces)"
    );
}

/// Global concurrency limit caps total Running across workspaces.
#[test]
fn advance_ready_to_running_respects_global_limit() {
    let (db, db_opt) = setup_db();

    let mut item1 = test_item("src-1", "analyze");
    let mut item2 = test_item("src-2", "analyze");
    let _ = item1.transit(QueuePhase::Ready);
    let _ = item2.transit(QueuePhase::Ready);
    item1.workspace_id = "ws-a".to_string();
    item2.workspace_id = "ws-b".to_string();
    insert_item_to_db(&db, &item1);
    insert_item_to_db(&db, &item2);

    let mut queue = make_queue(vec![item1, item2]);
    // Global max = 1: only 1 item can run globally.
    let mut tracker = ConcurrencyTracker::new(1);

    let mut limits = HashMap::new();
    limits.insert("ws-a".to_string(), 2);
    limits.insert("ws-b".to_string(), 2);

    let mut advancer = Advancer::new(&mut queue, &mut tracker, &db_opt, "test-ws", 2);
    advancer.advance_ready_to_running(&limits, 2);

    let running_count = queue
        .iter()
        .filter(|i| i.phase() == QueuePhase::Running)
        .count();
    assert_eq!(
        running_count, 1,
        "global concurrency limit should cap Running to 1"
    );
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

/// Empty queue with DB produces no transitions or events.
#[test]
fn empty_queue_with_db_is_noop() {
    let (_db, db_opt) = setup_db();

    let mut queue: VecDeque<QueueItem> = VecDeque::new();
    let mut tracker = ConcurrencyTracker::new(4);

    let mut advancer = Advancer::new(&mut queue, &mut tracker, &db_opt, "test-ws", 2);
    let advanced = advancer.run();

    assert_eq!(advanced, 0);
    assert!(queue.is_empty());
}

/// Mixed phases: only Pending items advance; Ready items that already exist
/// also advance to Running.
#[test]
fn mixed_phases_advance_correctly() {
    let (db, db_opt) = setup_db();

    let pending_item = test_item("src-1", "analyze");
    let mut ready_item = test_item("src-2", "implement");
    let _ = ready_item.transit(QueuePhase::Ready);

    insert_item_to_db(&db, &pending_item);
    insert_item_to_db(&db, &ready_item);

    let mut queue = make_queue(vec![pending_item, ready_item]);
    let mut tracker = ConcurrencyTracker::new(4);

    let mut advancer = Advancer::new(&mut queue, &mut tracker, &db_opt, "test-ws", 4);
    let advanced = advancer.run();

    // pending_item: Pending->Ready (1) + Ready->Running (1) = 2
    // ready_item: already Ready, Ready->Running (1) = 1
    assert_eq!(advanced, 3);
    assert!(
        queue.iter().all(|i| i.phase() == QueuePhase::Running),
        "all items should be Running"
    );
}
