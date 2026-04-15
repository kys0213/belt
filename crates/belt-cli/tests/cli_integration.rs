//! Integration tests for CLI subcommands: context, hitl, cron.
//!
//! Each test sets `BELT_HOME` to a temporary directory, seeds a SQLite
//! database with the required fixtures, and invokes the `belt` binary
//! as a subprocess to verify observable output and exit codes.

use std::path::PathBuf;
use std::process::Command;

use belt_core::phase::QueuePhase;
use belt_core::queue::QueueItem;
use belt_infra::db::Database;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a temporary BELT_HOME with an initialised database.
fn setup_belt_home() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let db_path = tmp.path().join("belt.db");
    let db = Database::open(db_path.to_str().unwrap()).expect("failed to open database");
    (tmp, db)
}

/// Path to the `belt` binary built by `cargo test`.
fn belt_bin() -> PathBuf {
    // `cargo test` places test binaries alongside the main binary.
    let mut path = std::env::current_exe()
        .expect("failed to get current exe")
        .parent()
        .expect("no parent dir")
        .to_path_buf();
    // Integration test binaries live in `deps/`; step up to target/debug.
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("belt")
}

/// Run `belt` with the given args and BELT_HOME override.
fn run_belt(belt_home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(belt_bin())
        .args(args)
        .env("BELT_HOME", belt_home.as_os_str())
        .output()
        .expect("failed to execute belt binary")
}

/// Insert a queue item in HITL phase for testing.
fn insert_hitl_item(db: &Database, work_id: &str) {
    let mut item = QueueItem::new(
        work_id.to_string(),
        "source-1".to_string(),
        "ws-test".to_string(),
        "implement".to_string(),
    );
    item.set_phase_unchecked(QueuePhase::Hitl);
    item.hitl_created_at = Some(chrono::Utc::now().to_rfc3339());
    item.hitl_reason = Some(belt_core::queue::HitlReason::EvaluateFailure);
    db.insert_item(&item).expect("failed to insert HITL item");
}

// ---------------------------------------------------------------------------
// context: --field source_data
// ---------------------------------------------------------------------------

#[test]
fn context_field_source_data_null_fallback() {
    // When no workspace config exists, context falls back to static mode
    // and source_data should be null.  `--field source_data` should report
    // "field not found" because null values are omitted from serialization.
    let (tmp, db) = setup_belt_home();

    let item = QueueItem::new(
        "ctx-1".to_string(),
        "src-1".to_string(),
        "ws-ctx".to_string(),
        "implement".to_string(),
    );
    db.insert_item(&item).expect("insert item");

    let output = run_belt(tmp.path(), &["context", "ctx-1", "--field", "source_data"]);
    // source_data is Null and skipped during serialization, so field lookup fails.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "expected failure for null source_data field, stderr: {stderr}"
    );
    assert!(
        stderr.contains("not found") || stderr.contains("source_data"),
        "stderr should mention field not found: {stderr}"
    );
}

#[test]
fn context_field_queue_phase() {
    // Verify --field can extract nested fields like queue.phase.
    let (tmp, db) = setup_belt_home();

    let item = QueueItem::new(
        "ctx-2".to_string(),
        "src-2".to_string(),
        "ws-ctx".to_string(),
        "implement".to_string(),
    );
    db.insert_item(&item).expect("insert item");

    let output = run_belt(tmp.path(), &["context", "ctx-2", "--field", "queue.phase"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.trim() == "pending",
        "expected 'pending', got: {stdout}"
    );
}

#[test]
fn context_field_work_id() {
    // Verify --field extracts top-level scalar fields.
    let (tmp, db) = setup_belt_home();

    let item = QueueItem::new(
        "ctx-3".to_string(),
        "src-3".to_string(),
        "ws-ctx".to_string(),
        "implement".to_string(),
    );
    db.insert_item(&item).expect("insert item");

    let output = run_belt(tmp.path(), &["context", "ctx-3", "--field", "work_id"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert_eq!(stdout.trim(), "ctx-3");
}

#[test]
fn context_json_output() {
    // `--json` flag should produce valid JSON containing expected fields.
    let (tmp, db) = setup_belt_home();

    let item = QueueItem::new(
        "ctx-4".to_string(),
        "src-4".to_string(),
        "ws-ctx".to_string(),
        "implement".to_string(),
    );
    db.insert_item(&item).expect("insert item");

    let output = run_belt(tmp.path(), &["context", "ctx-4", "--json"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("output should be valid JSON");
    assert_eq!(parsed["work_id"], "ctx-4");
    assert_eq!(parsed["queue"]["phase"], "pending");
    assert_eq!(parsed["queue"]["state"], "implement");
}

// ---------------------------------------------------------------------------
// hitl respond: EscalationAction / HitlRespondAction parsing
// ---------------------------------------------------------------------------

#[test]
fn hitl_respond_valid_action_done() {
    let (tmp, db) = setup_belt_home();
    insert_hitl_item(&db, "hitl-1");

    let output = run_belt(
        tmp.path(),
        &["hitl", "respond", "hitl-1", "--action", "done"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected success for valid action 'done', stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("done"),
        "stdout should mention action: {stdout}"
    );

    // Verify phase changed to Done.
    let updated = db.get_item("hitl-1").expect("item should exist");
    assert_eq!(updated.phase(), QueuePhase::Done);
}

#[test]
fn hitl_respond_valid_action_retry() {
    let (tmp, db) = setup_belt_home();
    insert_hitl_item(&db, "hitl-2");

    let output = run_belt(
        tmp.path(),
        &["hitl", "respond", "hitl-2", "--action", "retry"],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let updated = db.get_item("hitl-2").expect("item should exist");
    assert_eq!(updated.phase(), QueuePhase::Pending);
}

#[test]
fn hitl_respond_valid_action_skip() {
    let (tmp, db) = setup_belt_home();
    insert_hitl_item(&db, "hitl-3");

    let output = run_belt(
        tmp.path(),
        &["hitl", "respond", "hitl-3", "--action", "skip"],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let updated = db.get_item("hitl-3").expect("item should exist");
    assert_eq!(updated.phase(), QueuePhase::Skipped);
}

#[test]
fn hitl_respond_invalid_action_rejected() {
    let (tmp, db) = setup_belt_home();
    insert_hitl_item(&db, "hitl-4");

    let output = run_belt(
        tmp.path(),
        &["hitl", "respond", "hitl-4", "--action", "invalid_action"],
    );
    assert!(
        !output.status.success(),
        "expected failure for invalid action"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid") || stderr.contains("HITL respond action"),
        "stderr should mention invalid action: {stderr}"
    );
}

#[test]
fn hitl_respond_non_hitl_item_rejected() {
    // Responding to an item that is NOT in HITL phase should fail.
    let (tmp, db) = setup_belt_home();

    let item = QueueItem::new(
        "hitl-5".to_string(),
        "source-5".to_string(),
        "ws-test".to_string(),
        "implement".to_string(),
    );
    // Item is in Pending phase (not HITL).
    db.insert_item(&item).expect("insert item");

    let output = run_belt(
        tmp.path(),
        &["hitl", "respond", "hitl-5", "--action", "done"],
    );
    assert!(
        !output.status.success(),
        "expected failure for non-HITL item"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not 'hitl'") || stderr.contains("pending"),
        "stderr should mention phase mismatch: {stderr}"
    );
}

#[test]
fn hitl_timeout_set_valid_escalation_action() {
    // `belt hitl timeout set` parses the --action via EscalationAction::from_str.
    let (tmp, db) = setup_belt_home();
    insert_hitl_item(&db, "hitl-t1");

    let output = run_belt(
        tmp.path(),
        &[
            "hitl",
            "timeout",
            "set",
            "hitl-t1",
            "--duration",
            "3600",
            "--action",
            "skip",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected success for valid escalation action 'skip', stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("skip"),
        "stdout should mention action: {stdout}"
    );
}

#[test]
fn hitl_timeout_set_invalid_escalation_action() {
    // Invalid escalation action strings should be rejected.
    let (tmp, db) = setup_belt_home();
    insert_hitl_item(&db, "hitl-t2");

    let output = run_belt(
        tmp.path(),
        &[
            "hitl",
            "timeout",
            "set",
            "hitl-t2",
            "--duration",
            "3600",
            "--action",
            "not_a_real_action",
        ],
    );
    assert!(
        !output.status.success(),
        "expected failure for invalid escalation action"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid escalation action") || stderr.contains("not_a_real_action"),
        "stderr should mention invalid action: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// cron trigger: last_run_at reset
// ---------------------------------------------------------------------------

#[test]
fn cron_trigger_resets_last_run_at() {
    let (tmp, db) = setup_belt_home();

    // Seed a cron job and set its last_run_at.
    db.add_cron_job("test-job", "*/5 * * * *", "/bin/true", None)
        .expect("add cron job");
    db.update_cron_last_run("test-job")
        .expect("update last_run_at");

    // Verify it has a last_run_at before trigger.
    let job_before = db.get_cron_job("test-job").expect("get job");
    assert!(
        job_before.last_run_at.is_some(),
        "last_run_at should be set before trigger"
    );

    // Run `belt cron trigger test-job` — this resets last_run_at to NULL.
    let output = run_belt(tmp.path(), &["cron", "trigger", "test-job"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Trigger persisted") || stdout.contains("last_run_at reset"),
        "stdout should confirm trigger: {stdout}"
    );

    // Verify last_run_at is now NULL.
    let job_after = db.get_cron_job("test-job").expect("get job after trigger");
    assert!(
        job_after.last_run_at.is_none(),
        "last_run_at should be NULL after trigger"
    );
}

#[test]
fn cron_trigger_nonexistent_job_fails() {
    let (tmp, _db) = setup_belt_home();

    let output = run_belt(tmp.path(), &["cron", "trigger", "no-such-job"]);
    assert!(
        !output.status.success(),
        "expected failure for nonexistent cron job"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no-such-job") || stderr.contains("not found"),
        "stderr should mention the missing job: {stderr}"
    );
}
