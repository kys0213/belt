//! Scenario 6: a v1 database (written before schema versioning) is backed
//! up and migrated to v2 when the daemon first opens it.
//!
//! The migration is not in the spec; these assertions pin the current code
//! behavior (belt-infra/src/db_migrations.rs) as the contract.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::harness::{E2eEnv, wait_until};
use crate::support::{Workspace, phase_enters};

/// The v1 schema, frozen as `V1_SCHEMA` in belt-infra/src/db_migrations.rs.
const V1_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS queue_items (
        work_id          TEXT PRIMARY KEY,
        source_id        TEXT NOT NULL,
        workspace_id     TEXT NOT NULL,
        state            TEXT NOT NULL,
        phase            TEXT NOT NULL,
        title            TEXT,
        created_at       TEXT NOT NULL,
        updated_at       TEXT NOT NULL,
        hitl_created_at  TEXT,
        hitl_respondent  TEXT,
        hitl_notes       TEXT,
        hitl_reason          TEXT,
        hitl_timeout_at      TEXT,
        hitl_terminal_action TEXT,
        replan_count         INTEGER NOT NULL DEFAULT 0,
        worktree_preserved   INTEGER NOT NULL DEFAULT 0,
        previous_worktree_path TEXT
    );
    CREATE TABLE IF NOT EXISTS history (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        work_id    TEXT NOT NULL,
        source_id  TEXT NOT NULL,
        state      TEXT NOT NULL,
        status     TEXT NOT NULL,
        attempt    INTEGER NOT NULL,
        summary    TEXT,
        error      TEXT,
        created_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS workspaces (
        name        TEXT PRIMARY KEY,
        config_path TEXT NOT NULL,
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS cron_jobs (
        name        TEXT PRIMARY KEY,
        schedule    TEXT NOT NULL,
        script      TEXT NOT NULL DEFAULT '',
        workspace   TEXT,
        enabled     INTEGER NOT NULL DEFAULT 1,
        last_run_at TEXT,
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL DEFAULT ''
    );
    CREATE TABLE IF NOT EXISTS knowledge_base (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        workspace  TEXT NOT NULL,
        source_ref TEXT NOT NULL,
        category   TEXT NOT NULL,
        content    TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS token_usage (
        id                 INTEGER PRIMARY KEY AUTOINCREMENT,
        work_id            TEXT NOT NULL,
        workspace          TEXT NOT NULL,
        runtime            TEXT NOT NULL,
        model              TEXT NOT NULL,
        input_tokens       INTEGER NOT NULL,
        output_tokens      INTEGER NOT NULL,
        cache_read_tokens  INTEGER,
        cache_write_tokens INTEGER,
        duration_ms        INTEGER,
        created_at         TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS transition_events (
        id         TEXT PRIMARY KEY,
        work_id    TEXT NOT NULL,
        source_id  TEXT NOT NULL,
        event_type TEXT NOT NULL,
        phase      TEXT,
        from_phase TEXT,
        detail     TEXT,
        created_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS queue_dependencies (
        work_id    TEXT NOT NULL,
        depends_on TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (work_id, depends_on)
    );
";

/// Workspace of the legacy rows; differs from the daemon's, so the daemon
/// never executes them.
const LEGACY_WS: &str = "legacy-ws";
const PENDING: &str = "github:old/repo#1:analyze";
const EVAL_HITL: &str = "github:old/repo#2:analyze";
const SPEC_HITL: &str = "github:old/repo#3:analyze";
const ODD_TERMINAL_HITL: &str = "github:old/repo#4:analyze";
const RUNNING: &str = "github:old/repo#5:analyze";

/// Write a v1 database (`user_version` 0, v1 tables) with legacy rows.
fn write_v1_db(path: &Path, extra_sql: &str) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(V1_SCHEMA).unwrap();
    let item = |work_id: &str, phase: &str, hitl: Option<(&str, &str, Option<&str>)>| {
        let source_id = work_id.rsplit_once(':').unwrap().0;
        let (created, reason, terminal) = match hitl {
            Some((c, r, t)) => (Some(c), Some(r), t),
            None => (None, None, None),
        };
        conn.execute(
            "INSERT INTO queue_items (work_id, source_id, workspace_id, state, phase, title,
                 created_at, updated_at, hitl_created_at, hitl_reason, hitl_notes,
                 hitl_timeout_at, hitl_terminal_action)
             VALUES (?1, ?2, ?3, 'analyze', ?4, 'legacy', '2026-01-01T00:00:00Z',
                 '2026-01-01T00:00:00Z', ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                work_id,
                source_id,
                LEGACY_WS,
                phase,
                created,
                reason,
                hitl.map(|_| "old notes"),
                hitl.map(|_| "2027-01-01T00:00:00Z"),
                terminal,
            ],
        )
        .unwrap();
    };
    item(PENDING, "pending", None);
    item(
        EVAL_HITL,
        "hitl",
        Some(("2026-01-02T00:00:00Z", "evaluate_failure", Some("skip"))),
    );
    item(
        SPEC_HITL,
        "hitl",
        Some(("2026-01-03T00:00:00Z", "spec_completion_review", None)),
    );
    item(
        ODD_TERMINAL_HITL,
        "hitl",
        Some(("2026-01-04T00:00:00Z", "timeout", Some("escalate"))),
    );
    item(RUNNING, "running", None);
    // Inserted out of order: the copy must follow (created_at, id).
    conn.execute_batch(&format!(
        "INSERT INTO transition_events VALUES ('e2', '{PENDING}', 'github:old/repo#1', 'phase_enter', 'pending', NULL, 'second', '2026-01-01T00:00:02Z');
         INSERT INTO transition_events VALUES ('e1', '{PENDING}', 'github:old/repo#1', 'phase_enter', 'pending', NULL, 'first', '2026-01-01T00:00:01Z');
         {extra_sql}"
    ))
    .unwrap();
}

fn pragma(conn: &rusqlite::Connection, name: &str) -> Value {
    conn.query_row(&format!("PRAGMA {name}"), [], |r| {
        r.get::<_, rusqlite::types::Value>(0)
    })
    .map(|v| match v {
        rusqlite::types::Value::Integer(i) => Value::from(i),
        rusqlite::types::Value::Text(t) => Value::from(t),
        other => panic!("unexpected PRAGMA {name} value {other:?}"),
    })
    .unwrap()
}

fn table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |r| r.get(0),
    )
    .unwrap()
}

pub fn v1_database_is_backed_up_and_migrated_on_daemon_start() {
    let env = E2eEnv::new();
    let db_path = env.belt_home.join("belt.db");
    let backup = env.belt_home.join("belt.db.bak-v1");
    write_v1_db(&db_path, "");
    // The daemon is the first process to open the database.
    let config = Workspace::new("true").install(&env);
    let mut daemon = env.start_daemon(&config, 1);
    wait_until(
        Duration::from_secs(15),
        "daemon to migrate and start",
        &mut daemon,
        || (backup.exists() && env.daemon_log().contains("belt daemon started")).then_some(()),
    );

    // 1. The backup is the untouched v1 database.
    {
        let bak = rusqlite::Connection::open(&backup).unwrap();
        assert_eq!(pragma(&bak, "user_version"), 0);
        assert!(!table_exists(&bak, "transition_log"));
        assert!(!table_exists(&bak, "hitl_requests"));
        let phase: String = bak
            .query_row(
                "SELECT phase FROM queue_items WHERE work_id = ?1",
                [SPEC_HITL],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(phase, "hitl");
    }

    // 2. v2 schema in WAL mode.
    let db = env.db();
    assert_eq!(pragma(&db, "user_version"), 2);
    assert_eq!(pragma(&db, "journal_mode"), "wal");

    // 3. Every legacy item starts its own lineage.
    let items = env.queue_items();
    assert_eq!(items.len(), 5, "{items:#?}");
    for item in &items {
        assert_eq!(item["lineage_root"], item["work_id"], "{item}");
        assert!(item.get("derived_from").is_none(), "{item}");
    }

    // 4. transition_events -> transition_log in (created_at, id) order.
    let log = env.transitions(PENDING);
    let legacy: Vec<&Value> = log.iter().filter(|t| t["actor"] == "legacy").collect();
    let details: Vec<&str> = legacy.iter().filter_map(|t| t["detail"].as_str()).collect();
    assert_eq!(details, ["first", "second"], "{log:#?}");
    let ids: Vec<String> = db
        .prepare("SELECT legacy_event_id FROM transition_log WHERE work_id = ?1 AND actor = 'legacy' ORDER BY seq")
        .unwrap()
        .query_map([PENDING], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(ids, ["e1", "e2"]);

    // 5. An evaluate_failure Hitl item keeps Hitl with one open request.
    assert_eq!(env.phase_of(EVAL_HITL).as_deref(), Some("hitl"));
    let open = match env.belt_json(&["hitl", "list", "--format", "json"]) {
        Value::Array(rows) => rows,
        other => panic!("{other}"),
    };
    let eval: Vec<&Value> = open.iter().filter(|r| r["work_id"] == EVAL_HITL).collect();
    assert_eq!(eval.len(), 1, "{open:#?}");
    assert_eq!(eval[0]["hitl_id"], "hitl-legacy-1", "{}", eval[0]);
    assert_eq!(eval[0]["status"], "open");
    assert_eq!(eval[0]["reason"], "evaluate_failure");
    assert_eq!(eval[0]["notes"], "old notes");
    assert_eq!(eval[0]["terminal_action"], "skip");
    assert!(
        eval[0]["timeout_at"]
            .as_str()
            .is_some_and(|t| t.starts_with("2027-01-01"))
    );

    // 6. A spec-lifecycle Hitl item is closed as Skipped, without a request.
    assert_eq!(env.phase_of(SPEC_HITL).as_deref(), Some("skipped"));
    assert!(!open.iter().any(|r| r["work_id"] == SPEC_HITL));
    let spec_log = env.transitions(SPEC_HITL);
    let closed = *phase_enters(&spec_log).last().unwrap();
    assert_eq!(closed["from_phase"], "hitl", "{closed}");
    assert_eq!(closed["to_phase"], "skipped", "{closed}");
    assert_eq!(closed["actor"], "legacy", "{closed}");
    // The row carries the legacy reason in `detail`; `reason` is `legacy`.
    assert_eq!(closed["reason"], "legacy", "{closed}");
    assert_eq!(closed["detail"], "spec_completion_review", "{closed}");
    let (reason, notes): (String, String) = db
        .query_row(
            "SELECT hitl_reason, hitl_notes FROM queue_items WHERE work_id = ?1",
            [SPEC_HITL],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(reason, "manual_escalation");
    assert!(
        notes.contains("[legacy reason: spec_completion_review]"),
        "{notes}"
    );

    // 7. A terminal action other than skip/replan is dropped into the notes.
    let odd: Vec<&Value> = open
        .iter()
        .filter(|r| r["work_id"] == ODD_TERMINAL_HITL)
        .collect();
    assert_eq!(odd.len(), 1, "{open:#?}");
    assert!(odd[0]["terminal_action"].is_null(), "{}", odd[0]);
    assert!(
        odd[0]["notes"]
            .as_str()
            .is_some_and(|n| n.contains("[legacy terminal: escalate]")),
        "{}",
        odd[0]
    );

    // 8. A legacy Running row is rolled back to Pending on start.
    assert_eq!(env.phase_of(RUNNING).as_deref(), Some("pending"));
    let run_log = env.transitions(RUNNING);
    let rollback = *phase_enters(&run_log).last().unwrap();
    assert_eq!(rollback["from_phase"], "running", "{rollback}");
    assert_eq!(rollback["to_phase"], "pending", "{rollback}");
    assert_eq!(rollback["actor"], "daemon", "{rollback}");
    assert_eq!(rollback["reason"], "rollback", "{rollback}");

    // Untouched pending item.
    assert_eq!(env.phase_of(PENDING).as_deref(), Some("pending"));

    // Reopening changes nothing and makes no second backup.
    assert!(
        daemon.stop(Duration::from_secs(10)),
        "daemon ignored SIGINT"
    );
    drop(daemon);
    assert_eq!(env.queue_items().len(), 5);
    assert!(!env.belt_home.join("belt.db.bak-v1.2").exists());
    assert_eq!(pragma(&env.db(), "user_version"), 2);
}

/// A v1 database with a phase no v1 binary wrote is refused before any
/// backup or change.
pub fn v1_database_with_unknown_phase_is_refused_without_backup() {
    let env = E2eEnv::new();
    let db_path = env.belt_home.join("belt.db");
    write_v1_db(
        &db_path,
        "INSERT INTO queue_items (work_id, source_id, workspace_id, state, phase, created_at, updated_at)
         VALUES ('weird-1', 'github:old/repo#9', 'legacy-ws', 'analyze', 'weird', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');",
    );
    let out = env.belt(&["queue", "list", "--format", "json"]);
    assert!(
        !out.status.success(),
        "migration of an unknown phase succeeded"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("migration v2: queue item weird-1 has unknown phase 'weird'"),
        "{stderr}"
    );
    assert!(!env.belt_home.join("belt.db.bak-v1").exists());
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    assert_eq!(pragma(&conn, "user_version"), 0);
    assert!(!table_exists(&conn, "transition_log"));
}

/// An existing `.bak-v1` is never overwritten: the backup goes to the next
/// free name, `.bak-v1.2`.
pub fn v1_backup_with_taken_name_goes_to_next_free_name() {
    let env = E2eEnv::new();
    let db_path = env.belt_home.join("belt.db");
    let taken = env.belt_home.join("belt.db.bak-v1");
    let next = env.belt_home.join("belt.db.bak-v1.2");
    const EARLIER_BACKUP: &str = "an earlier backup, not a database";
    std::fs::write(&taken, EARLIER_BACKUP).unwrap();
    write_v1_db(&db_path, "");

    // Any command that opens the database migrates it.
    let items = env.queue_items();
    assert_eq!(items.len(), 5, "{items:#?}");

    assert_eq!(std::fs::read_to_string(&taken).unwrap(), EARLIER_BACKUP);
    assert!(next.exists(), "no .bak-v1.2 next to the taken .bak-v1");
    let bak = rusqlite::Connection::open(&next).unwrap();
    assert_eq!(pragma(&bak, "user_version"), 0);
    assert!(!table_exists(&bak, "transition_log"));
    assert!(table_exists(&bak, "transition_events"));
    assert!(!env.belt_home.join("belt.db.bak-v1.3").exists());
    assert_eq!(pragma(&env.db(), "user_version"), 2);
}
