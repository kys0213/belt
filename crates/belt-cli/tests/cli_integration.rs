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
    item.set_phase_unchecked(QueuePhase::Running);
    db.insert_item(&item).expect("failed to insert HITL item");
    let outcome = db
        .open_hitl(&belt_infra::db::OpenHitlRequest {
            work_id: work_id.to_string(),
            expected_from: QueuePhase::Running,
            reason: belt_core::queue::HitlReason::EvaluateFailure,
            notes: None,
            actor: belt_core::transition::Actor::Daemon,
            transition_reason: belt_core::transition::TransitionReason::Escalation(
                belt_core::escalation::EscalationAction::Hitl,
            ),
            timeout_at: None,
            terminal_action: None,
        })
        .expect("failed to open HITL request");
    assert!(matches!(
        outcome,
        belt_infra::db::OpenHitlOutcome::Opened { .. }
    ));
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

#[test]
fn context_json_exposes_derived_from_only_for_derived_items() {
    let (tmp, db) = setup_belt_home();

    let origin = QueueItem::new(
        "ctx-origin".to_string(),
        "src-d".to_string(),
        "ws-ctx".to_string(),
        "implement".to_string(),
    );
    db.insert_item(&origin).expect("insert origin");

    let mut derived = QueueItem::new(
        "ctx-derived".to_string(),
        "src-d".to_string(),
        "ws-ctx".to_string(),
        "implement".to_string(),
    );
    derived.derived_from = Some("ctx-origin".to_string());
    db.insert_item(&derived).expect("insert derived");

    let run = |work_id: &str| -> serde_json::Value {
        let output = run_belt(tmp.path(), &["context", work_id, "--json"]);
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("valid JSON")
    };

    let derived_json = run("ctx-derived");
    assert_eq!(derived_json["queue"]["derived_from"], "ctx-origin");

    let origin_json = run("ctx-origin");
    assert!(
        origin_json["queue"]
            .as_object()
            .expect("queue object")
            .get("derived_from")
            .is_none(),
        "derived_from key must be omitted for non-derived items"
    );
}

// ---------------------------------------------------------------------------
// hitl respond / list / show / timeout: HITL requests are the only source
// ---------------------------------------------------------------------------

/// Seed a Completed item and open a request the way the daemon does:
/// reason, notes, deadline and terminal action all live on the request.
fn open_daemon_style_hitl(db: &Database) -> (String, String) {
    let id = seed_item(db, "daemon", QueuePhase::Completed);
    let opened = db
        .open_hitl(&OpenHitlRequest {
            work_id: id.clone(),
            expected_from: QueuePhase::Completed,
            reason: belt_core::queue::HitlReason::RetryMaxExceeded,
            notes: Some("needs a human look".to_string()),
            actor: Actor::Daemon,
            transition_reason: TransitionReason::Manual,
            timeout_at: Some("2099-01-01T00:00:00+00:00".to_string()),
            terminal_action: Some(belt_core::escalation::EscalationAction::Replan),
        })
        .expect("open hitl");
    let OpenHitlOutcome::Opened { hitl_id, .. } = opened else {
        panic!("expected an opened request, got {opened:?}");
    };
    (id, hitl_id.as_str().to_string())
}

#[test]
fn hitl_respond_wins_and_holds_the_item_in_hitl() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Hitl);

    let out = run_belt(
        tmp.path(),
        &[
            "hitl",
            "respond",
            &id,
            "--action",
            "done",
            "--respondent",
            "irene",
            "--json",
        ],
    );
    assert!(out.status.success(), "{out:?}");
    let v = stdout_json(&out);
    assert_eq!(v["success"], true);
    assert_eq!(v["action"], "done");
    assert_eq!(v["by"], "irene");
    assert_eq!(v["via"], "cli");

    // Confirmed, not applied: the item leaves Hitl only by daemon post-processing.
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Hitl);
    assert_eq!(db.pending_post_processing().unwrap().len(), 1);
}

#[test]
fn hitl_respond_retry_wins_with_the_confirmed_action() {
    let (tmp, db) = setup_belt_home();
    let (id, _) = open_daemon_style_hitl(&db);

    let out = run_belt(
        tmp.path(),
        &[
            "hitl",
            "respond",
            &id,
            "--action",
            "retry",
            "--respondent",
            "irene",
            "--json",
        ],
    );

    assert!(out.status.success(), "{out:?}");
    let v = stdout_json(&out);
    assert_eq!(v["success"], true);
    assert_eq!(v["action"], "retry");
    assert_eq!(v["by"], "irene");
    assert_eq!(v["via"], "cli");
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Hitl);
    assert_eq!(db.pending_post_processing().unwrap().len(), 1);
}

#[test]
fn hitl_respond_by_hitl_id_wins() {
    let (tmp, db) = setup_belt_home();
    let (id, hitl_id) = open_daemon_style_hitl(&db);

    let out = run_belt(
        tmp.path(),
        &[
            "hitl",
            "respond",
            "--hitl-id",
            &hitl_id,
            "--action",
            "skip",
            "--json",
        ],
    );
    assert!(out.status.success(), "{out:?}");
    let v = stdout_json(&out);
    assert_eq!(v["hitl_id"], hitl_id.as_str());
    assert_eq!(v["work_id"], id.as_str());
    assert_eq!(db.pending_post_processing().unwrap().len(), 1);
}

#[test]
fn hitl_respond_after_confirmation_is_already_handled_and_recorded() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Hitl);
    let first = run_belt(
        tmp.path(),
        &[
            "hitl",
            "respond",
            &id,
            "--action",
            "skip",
            "--respondent",
            "alice",
        ],
    );
    assert!(first.status.success(), "{first:?}");

    let second = run_belt(
        tmp.path(),
        &[
            "hitl",
            "respond",
            &id,
            "--action",
            "done",
            "--respondent",
            "bob",
            "--json",
        ],
    );
    assert_eq!(second.status.code(), Some(EXIT_REFUSED));
    let v = stdout_json(&second);
    assert_eq!(v["success"], false);
    assert_eq!(v["reason"], "already_handled");
    assert_eq!(v["by"], "alice");
    assert_eq!(v["via"], "cli");
    assert_eq!(v["action"], "skip");
    assert!(v["at"].is_string());

    let log = db.transitions_of(&id).unwrap();
    assert!(
        log.iter()
            .any(|e| e.kind == belt_infra::db::transition_kind::HITL_RESPONSE_REJECTED),
        "rejection must be in the history: {log:?}"
    );
    assert_eq!(db.pending_post_processing().unwrap().len(), 1);
}

#[test]
fn hitl_respond_unknown_targets_are_not_found() {
    let (tmp, db) = setup_belt_home();
    // An item without any request is not a HITL target either.
    let pending = seed_item(&db, "1", QueuePhase::Pending);

    for args in [
        vec![
            "hitl",
            "respond",
            "no-such-item",
            "--action",
            "done",
            "--json",
        ],
        vec![
            "hitl",
            "respond",
            pending.as_str(),
            "--action",
            "done",
            "--json",
        ],
        vec![
            "hitl",
            "respond",
            "--hitl-id",
            "no-such-hitl",
            "--action",
            "done",
            "--json",
        ],
    ] {
        let out = run_belt(tmp.path(), &args);
        assert_eq!(out.status.code(), Some(EXIT_REFUSED), "{args:?}");
        assert_eq!(stdout_json(&out)["reason"], "not_found", "{args:?}");
    }
}

#[test]
fn hitl_respond_unknown_action_is_invalid_action() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Hitl);

    let out = run_belt(
        tmp.path(),
        &["hitl", "respond", &id, "--action", "bogus", "--json"],
    );
    assert_eq!(out.status.code(), Some(EXIT_REFUSED));
    assert_eq!(stdout_json(&out)["reason"], "invalid_action");
    assert!(db.pending_post_processing().unwrap().is_empty());
}

#[test]
fn hitl_list_and_show_read_the_request_a_daemon_opened() {
    let (tmp, db) = setup_belt_home();
    let (id, hitl_id) = open_daemon_style_hitl(&db);

    let list = run_belt(tmp.path(), &["hitl", "list", "--format", "json"]);
    assert!(list.status.success(), "{list:?}");
    let rows = stdout_json(&list);
    let row = &rows.as_array().expect("array")[0];
    assert_eq!(row["work_id"], id.as_str());
    assert_eq!(row["hitl_id"], hitl_id.as_str());
    assert_eq!(row["reason"], "retry_max_exceeded");
    assert_eq!(row["notes"], "needs a human look");
    assert_eq!(row["timeout_at"], "2099-01-01T00:00:00+00:00");
    assert_eq!(row["terminal_action"], "replan");

    let text = run_belt(tmp.path(), &["hitl", "list"]);
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(
        text.contains(&id) && text.contains("retry_max_exceeded"),
        "{text}"
    );

    let show = run_belt(tmp.path(), &["hitl", "show", &id, "--format", "json"]);
    assert!(show.status.success(), "{show:?}");
    let v = stdout_json(&show);
    assert_eq!(v["hitl_request"]["hitl_id"], hitl_id.as_str());
    assert_eq!(v["hitl_request"]["status"], "open");
    assert_eq!(v["hitl_request"]["reason"], "retry_max_exceeded");
    assert_eq!(v["hitl_request"]["notes"], "needs a human look");
    assert_eq!(v["hitl_request"]["timeout_at"], "2099-01-01T00:00:00+00:00");
    assert_eq!(v["hitl_request"]["terminal_action"], "replan");
    assert_eq!(v["recommended"]["action"], "skip");
}

#[test]
fn hitl_show_reports_the_confirmed_response() {
    let (tmp, db) = setup_belt_home();
    let (id, _) = open_daemon_style_hitl(&db);
    assert!(
        run_belt(
            tmp.path(),
            &[
                "hitl",
                "respond",
                &id,
                "--action",
                "retry",
                "--respondent",
                "irene"
            ],
        )
        .status
        .success()
    );

    let show = run_belt(tmp.path(), &["hitl", "show", &id, "--format", "json"]);
    let v = stdout_json(&show);
    assert_eq!(v["hitl_request"]["status"], "resolved");
    assert_eq!(v["hitl_request"]["processing"], "awaiting_post_processing");
    assert_eq!(v["hitl_request"]["resolution"]["action"], "retry");
    assert_eq!(v["hitl_request"]["resolution"]["by"], "irene");
    assert_eq!(v["hitl_request"]["resolution"]["via"], "cli");
}

#[test]
fn hitl_timeout_set_rejects_terminal_actions_other_than_skip_and_replan() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Hitl);

    for action in ["failed", "retry", "hitl", "retry_with_comment", "nope"] {
        let out = run_belt(
            tmp.path(),
            &[
                "hitl",
                "timeout",
                "set",
                &id,
                "--duration",
                "3600",
                "--action",
                action,
            ],
        );
        assert!(!out.status.success(), "{action} must be refused");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("invalid value"), "{action}: {stderr}");
    }
}

#[test]
fn hitl_timeout_set_stores_the_deadline_and_action_on_the_open_request() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Hitl);

    for action in ["skip", "replan"] {
        let before = chrono::Utc::now();
        let out = run_belt(
            tmp.path(),
            &[
                "hitl",
                "timeout",
                "set",
                &id,
                "--duration",
                "3600",
                "--action",
                action,
            ],
        );
        assert!(out.status.success(), "{action}: {out:?}");
        let after = chrono::Utc::now();

        let ls = run_belt(tmp.path(), &["hitl", "timeout", "ls", "--json"]);
        let v = stdout_json(&ls);
        let entry = &v.as_array().expect("array")[0];
        assert_eq!(entry["work_id"], id);
        assert_eq!(entry["action"], action);
        let timeout_at =
            chrono::DateTime::parse_from_rfc3339(entry["timeout_at"].as_str().expect("timeout_at"))
                .unwrap()
                .with_timezone(&chrono::Utc);
        assert!(timeout_at >= before + chrono::Duration::seconds(3600));
        assert!(timeout_at <= after + chrono::Duration::seconds(3600));
    }
}

#[test]
fn hitl_timeout_set_without_action_clears_the_stored_action() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Hitl);
    let set = |extra: &[&str]| {
        let mut args = vec!["hitl", "timeout", "set", id.as_str(), "--duration", "60"];
        args.extend_from_slice(extra);
        let out = run_belt(tmp.path(), &args);
        assert!(out.status.success(), "{out:?}");
    };
    set(&["--action", "replan"]);
    set(&[]);

    let ls = run_belt(tmp.path(), &["hitl", "timeout", "ls", "--json"]);
    let v = stdout_json(&ls);
    assert!(v.as_array().expect("array")[0]["action"].is_null());
}

#[test]
fn hitl_timeout_set_without_open_request_is_not_found() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Pending);

    let out = run_belt(
        tmp.path(),
        &[
            "hitl",
            "timeout",
            "set",
            &id,
            "--duration",
            "60",
            "--action",
            "skip",
        ],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no open HITL request"), "{stderr}");
}

#[test]
fn hitl_timeout_ls_lists_open_requests_with_a_deadline() {
    let (tmp, db) = setup_belt_home();
    let (id, _) = open_daemon_style_hitl(&db);
    seed_item(&db, "no-deadline", QueuePhase::Hitl);

    let out = run_belt(tmp.path(), &["hitl", "timeout", "ls", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let v = stdout_json(&out);
    let rows = v.as_array().expect("array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["work_id"], id.as_str());
    assert_eq!(rows[0]["action"], "replan");
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

// ---------------------------------------------------------------------------
// belt spec (removed)
// ---------------------------------------------------------------------------

#[test]
fn spec_subcommand_does_not_exist() {
    let (tmp, _db) = setup_belt_home();
    let output = run_belt(tmp.path(), &["spec", "list"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unrecognized subcommand"),
        "expected clap rejection, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// status: no spec section in any output format
// ---------------------------------------------------------------------------

/// Seed rows an older Belt left behind: a `<ws>:evaluate` cron job and a
/// `specs` table with one row (the current schema no longer creates it).
fn seed_legacy_spec_and_evaluate(tmp: &TempDir, db: &Database) {
    db.add_cron_job("ws-test:evaluate", "0 */6 * * *", "", Some("ws-test"))
        .expect("add legacy evaluate cron job");

    let conn = rusqlite::Connection::open(tmp.path().join("belt.db")).expect("open raw db");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS specs (
            id                TEXT PRIMARY KEY,
            workspace_id      TEXT NOT NULL,
            name              TEXT NOT NULL,
            status            TEXT NOT NULL,
            content           TEXT NOT NULL,
            priority          INTEGER,
            labels            TEXT,
            depends_on        TEXT,
            entry_point       TEXT,
            decomposed_issues TEXT,
            test_commands     TEXT,
            created_at        TEXT NOT NULL,
            updated_at        TEXT NOT NULL
        );
        INSERT INTO specs (id, workspace_id, name, status, content, created_at, updated_at)
        VALUES ('legacy-spec-1', 'ws-test', 'legacy', 'active', 'legacy content',
                '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');",
    )
    .expect("seed legacy specs table");
}

/// Run `belt status --format <format>` against a database seeded with a HITL
/// item plus legacy spec/evaluate rows, and return stdout.
fn status_stdout(format: &str) -> String {
    let (tmp, db) = setup_belt_home();
    insert_hitl_item(&db, "work-status-1");
    seed_legacy_spec_and_evaluate(&tmp, &db);
    drop(db);

    let output = run_belt(tmp.path(), &["status", "--format", format]);
    assert!(
        output.status.success(),
        "belt status --format {format} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Assert that status printed the seeded HITL item. The text format only
/// renders counts, so it is matched on those instead of names.
fn assert_status_shows_seed(stdout: &str, format: &str) {
    let needles: &[&str] = if format == "text" {
        &["Total items: 1", "HITL: 1"]
    } else {
        &["ws-test", "work-status-1"]
    };
    for needle in needles {
        assert!(
            stdout.contains(needle),
            "`belt status --format {format}` should mention {needle:?}:\n{stdout}"
        );
    }
}

/// Assert that the rendered status carries no spec-related key or label.
fn assert_no_spec_section(stdout: &str, format: &str) {
    let lowered = stdout.to_lowercase();
    for needle in ["spec", "next evaluate", "next_evaluate", "linked issue"] {
        assert!(
            !lowered.contains(needle),
            "`belt status --format {format}` must not mention {needle:?}:\n{stdout}"
        );
    }
}

#[test]
fn status_json_has_no_spec_section() {
    let stdout = status_stdout("json");
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("status json output should parse");
    assert!(value.is_object(), "status json should be an object");
    assert_status_shows_seed(&stdout, "json");
    assert_no_spec_section(&stdout, "json");
}

#[test]
fn status_text_has_no_spec_section() {
    let stdout = status_stdout("text");
    assert_status_shows_seed(&stdout, "text");
    assert_no_spec_section(&stdout, "text");
}

#[test]
fn status_rich_has_no_spec_section() {
    let stdout = status_stdout("rich");
    assert_status_shows_seed(&stdout, "rich");
    assert_no_spec_section(&stdout, "rich");
}

// ---------------------------------------------------------------------------
// queue done / skip / hitl / show: transition contract
// ---------------------------------------------------------------------------

use belt_core::transition::{Actor, TransitionOutcome, TransitionReason, TransitionRequest};
use belt_infra::db::{
    CollectOutcome, DeriveKind, DeriveOutcome, DeriveRequest, NewItem, OpenHitlOutcome,
    OpenHitlRequest,
};

const WORKSPACE_YAML: &str = r#"
name: queue-ws
sources:
  github:
    url: "https://github.com/test/repo"
    escalation:
      1: retry
      2: retry_with_comment
      3: hitl
      terminal: skip
    scan_interval_secs: 300
    states:
      implement:
        trigger: {}
        prompt: "implement"
"#;

/// Workspace whose `implement` state runs `on_done_script` when finished.
fn workspace_yaml_with_on_done(on_done_script: &str) -> String {
    format!("{WORKSPACE_YAML}        on_done:\n          - script: \"{on_done_script}\"\n")
}

/// Register a workspace so `queue done` can load the item's state config.
fn register_workspace(tmp: &TempDir, db: &Database) {
    let config_path = tmp.path().join("workspace.yaml");
    std::fs::write(&config_path, WORKSPACE_YAML).expect("write workspace yaml");
    db.add_workspace("ws-queue", config_path.to_str().unwrap())
        .expect("add workspace");
}

/// Register a workspace whose `implement` state has an `on_done` script.
fn register_workspace_with_on_done(tmp: &TempDir, db: &Database, script: &str) {
    let config_path = tmp.path().join("workspace.yaml");
    std::fs::write(&config_path, workspace_yaml_with_on_done(script))
        .expect("write workspace yaml");
    db.add_workspace("ws-queue", config_path.to_str().unwrap())
        .expect("add workspace");
}

/// A throwaway repository to run `belt` in: `queue done` with on_done
/// scripts creates a worktree of the current directory's repository.
fn scratch_repo() -> TempDir {
    let repo = TempDir::new().expect("repo dir");
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .status()
            .expect("run vcs");
        assert!(status.success(), "{args:?}");
    };
    run(&["init", "-q"]);
    run(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.com",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "init",
    ]);
    repo
}

fn run_belt_in(
    cwd: &std::path::Path,
    belt_home: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    Command::new(belt_bin())
        .args(args)
        .env("BELT_HOME", belt_home.as_os_str())
        .current_dir(cwd)
        .output()
        .expect("failed to execute belt binary")
}

fn daemon_move(db: &Database, work_id: &str, from: QueuePhase, to: QueuePhase) {
    let outcome = db
        .transition(&TransitionRequest {
            work_id: work_id.to_string(),
            expected_from: from,
            to,
            actor: Actor::Daemon,
            reason: TransitionReason::Manual,
            detail: None,
        })
        .expect("transition should not error");
    assert!(
        matches!(outcome, TransitionOutcome::Applied { .. }),
        "{from:?} -> {to:?} should apply, got {outcome:?}"
    );
}

/// Create an item through the contract and walk it to `phase`.
/// A Hitl item gets an open HITL request.
fn seed_item(db: &Database, source: &str, phase: QueuePhase) -> String {
    use QueuePhase::*;
    let created = db
        .insert_collected(&NewItem {
            source_id: format!("github:org/repo#{source}"),
            workspace_id: "ws-queue".to_string(),
            state: "implement".to_string(),
            title: None,
            actor: Actor::Daemon,
        })
        .expect("collect");
    let CollectOutcome::Inserted { work_id } = created else {
        panic!("expected a new item, got {created:?}");
    };
    let path: &[QueuePhase] = match phase {
        Pending => &[],
        Ready => &[Ready],
        Running => &[Ready, Running],
        Completed | Hitl => &[Ready, Running, Completed],
        Done => &[Ready, Running, Completed, Done],
        Failed => &[Ready, Running, Failed],
        Skipped => &[Skipped],
    };
    let mut from = Pending;
    for next in path {
        daemon_move(db, &work_id, from, *next);
        from = *next;
    }
    if phase == Hitl {
        let opened = db
            .open_hitl(&OpenHitlRequest {
                work_id: work_id.clone(),
                expected_from: Completed,
                reason: belt_core::queue::HitlReason::EvaluateFailure,
                notes: None,
                actor: Actor::Daemon,
                transition_reason: TransitionReason::Manual,
                timeout_at: None,
                terminal_action: None,
            })
            .expect("open hitl");
        assert!(matches!(opened, OpenHitlOutcome::Opened { .. }));
    }
    work_id
}

fn stdout_json(output: &std::process::Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout is not json ({e}): {stdout}"))
}

#[test]
fn queue_skip_pending_is_applied() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Pending);

    let out = run_belt(tmp.path(), &["queue", "skip", &id, "--json"]);
    assert!(out.status.success(), "{out:?}");
    let v = stdout_json(&out);
    assert_eq!(v["success"], true);
    assert_eq!(v["result"], "applied");
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Skipped);
}

#[test]
fn queue_skip_failed_is_applied() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Failed);

    let out = run_belt(tmp.path(), &["queue", "skip", &id, "--json"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Skipped);
}

#[test]
fn queue_skip_running_without_daemon_is_canceled_directly() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Running);

    let out = run_belt(tmp.path(), &["queue", "skip", &id, "--json"]);
    assert!(out.status.success(), "{out:?}");
    let v = stdout_json(&out);
    assert_eq!(v["success"], true);
    assert_eq!(v["result"], "canceled_directly");
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Skipped);
    assert!(db.open_cancel_request(&id).unwrap().is_none());
}

/// A `sleep` in its own process group, as handlers are spawned.
#[cfg(unix)]
struct SleepHandler(std::process::Child);

#[cfg(unix)]
impl SleepHandler {
    fn spawn() -> Self {
        use std::os::unix::process::CommandExt;
        let child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn sleep");
        Self(child)
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    /// Whether the process exits soon; reaps it.
    fn exits(&mut self) -> bool {
        for _ in 0..50 {
            if self.0.try_wait().expect("try_wait").is_some() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    }
}

#[cfg(unix)]
impl Drop for SleepHandler {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
#[test]
fn queue_skip_running_without_daemon_stops_the_recorded_handler() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Running);
    let mut handler = SleepHandler::spawn();
    assert!(db.set_handler_process(&id, handler.pid()).unwrap());

    let out = run_belt(tmp.path(), &["queue", "skip", &id, "--json"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout_json(&out)["result"], "canceled_directly");
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Skipped);
    assert!(handler.exits(), "the handler process should be stopped");
}

#[test]
fn queue_skip_running_with_a_stale_daemon_pid_file_is_canceled_directly() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Running);
    // No process holds this pid, so there is no daemon to answer.
    std::fs::write(tmp.path().join("daemon.pid"), "4194304").unwrap();

    let out = run_belt(tmp.path(), &["queue", "skip", &id, "--json"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout_json(&out)["result"], "canceled_directly");
}

/// What the daemon double does with the cancel request it finds.
#[derive(Clone, Copy)]
enum DaemonReaction {
    /// Accepts it and never closes it.
    AcceptOnly,
    /// Accepts it, moves the item to Skipped, closes it `canceled`.
    Cancel,
    /// The handler finished first: the item moves on and the request closes
    /// `too_late`.
    TooLate,
}

/// Stands in for a running daemon: the pid file names a live process (this
/// test), no IPC port is published, and a thread plays the daemon's side of
/// the cancel request on its own database connection.
struct DaemonDouble(Option<std::thread::JoinHandle<()>>);

impl DaemonDouble {
    fn start(belt_home: &std::path::Path, reaction: DaemonReaction) -> Self {
        use belt_infra::db::CancelResult;
        std::fs::write(belt_home.join("daemon.pid"), std::process::id().to_string()).unwrap();
        let db_path = belt_home.join("belt.db");
        Self(Some(std::thread::spawn(move || {
            let db = Database::open(db_path.to_str().unwrap()).expect("open daemon-side db");
            let daemon = Actor::Daemon;
            let open = (0..500)
                .find_map(|_| {
                    let found = db.open_cancel_requests().unwrap().into_iter().next();
                    if found.is_none() {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    found
                })
                .expect("the CLI should record a cancel request");
            match reaction {
                DaemonReaction::AcceptOnly => {
                    db.accept_cancel(open.id, &daemon).unwrap();
                }
                DaemonReaction::Cancel => {
                    db.accept_cancel(open.id, &daemon).unwrap();
                    daemon_move(&db, &open.work_id, QueuePhase::Running, QueuePhase::Skipped);
                    db.close_cancel(open.id, CancelResult::Canceled, &daemon)
                        .unwrap();
                }
                DaemonReaction::TooLate => {
                    daemon_move(
                        &db,
                        &open.work_id,
                        QueuePhase::Running,
                        QueuePhase::Completed,
                    );
                    db.close_cancel(open.id, CancelResult::TooLate, &daemon)
                        .unwrap();
                }
            }
        })))
    }

    fn finish(mut self) {
        self.0
            .take()
            .unwrap()
            .join()
            .expect("daemon double panicked");
    }
}

#[test]
fn queue_skip_running_closed_by_the_daemon_is_canceled() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Running);
    let daemon = DaemonDouble::start(tmp.path(), DaemonReaction::Cancel);

    let out = run_belt(tmp.path(), &["queue", "skip", &id, "--json"]);
    daemon.finish();

    assert!(out.status.success(), "{out:?}");
    let v = stdout_json(&out);
    assert_eq!(v["success"], true);
    assert_eq!(v["result"], "canceled");
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Skipped);
}

#[test]
fn queue_skip_running_too_late_is_refused_with_the_current_phase() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Running);
    let daemon = DaemonDouble::start(tmp.path(), DaemonReaction::TooLate);

    let out = run_belt(tmp.path(), &["queue", "skip", &id, "--json"]);
    daemon.finish();

    assert!(!out.status.success(), "{out:?}");
    let v = stdout_json(&out);
    assert_eq!(v["success"], false);
    assert_eq!(v["reason"], "too_late");
    assert_eq!(v["current"], "completed");
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Completed);
}

#[test]
fn queue_skip_running_accepted_but_not_closed_exits_zero_as_accepted() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Running);
    let daemon = DaemonDouble::start(tmp.path(), DaemonReaction::AcceptOnly);

    // The CLI waits out its limit (10s) before reporting the accepted request.
    let out = run_belt(tmp.path(), &["queue", "skip", &id, "--json"]);
    daemon.finish();

    assert!(out.status.success(), "{out:?}");
    let v = stdout_json(&out);
    assert_eq!(v["success"], true);
    assert_eq!(v["result"], "accepted");
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Running);
}

#[test]
fn queue_skip_hitl_awaiting_post_processing_is_busy_and_records_no_request() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Hitl);
    assert!(
        run_belt(tmp.path(), &["queue", "skip", &id])
            .status
            .success()
    );
    assert_eq!(db.pending_post_processing().unwrap().len(), 1);

    let out = run_belt(tmp.path(), &["queue", "skip", &id, "--json"]);
    assert!(!out.status.success());
    assert_eq!(stdout_json(&out)["reason"], "busy");
    assert!(db.open_cancel_request(&id).unwrap().is_none());
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Hitl);
}

#[test]
fn queue_skip_done_item_is_invalid_action() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Done);

    let out = run_belt(tmp.path(), &["queue", "skip", &id, "--json"]);
    assert!(!out.status.success());
    assert_eq!(stdout_json(&out)["reason"], "invalid_action");
}

#[test]
fn queue_done_done_item_is_invalid_action() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Done);

    let out = run_belt(tmp.path(), &["queue", "done", &id, "--json"]);
    assert!(!out.status.success());
    assert_eq!(stdout_json(&out)["reason"], "invalid_action");
}

#[test]
fn queue_done_failed_item_is_invalid_action() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Failed);

    let out = run_belt(tmp.path(), &["queue", "done", &id, "--json"]);
    assert!(!out.status.success());
    assert_eq!(stdout_json(&out)["reason"], "invalid_action");
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Failed);
}

#[test]
fn queue_done_running_item_is_busy() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Running);

    let out = run_belt(tmp.path(), &["queue", "done", &id, "--json"]);
    assert!(!out.status.success());
    assert_eq!(stdout_json(&out)["reason"], "busy");
}

#[test]
fn queue_done_unknown_item_is_not_found() {
    let (tmp, _db) = setup_belt_home();

    let out = run_belt(tmp.path(), &["queue", "done", "no-such-item", "--json"]);
    assert!(!out.status.success());
    assert_eq!(stdout_json(&out)["reason"], "not_found");
}

#[test]
fn queue_done_completed_without_on_done_is_applied() {
    let (tmp, db) = setup_belt_home();
    register_workspace(&tmp, &db);
    let id = seed_item(&db, "1", QueuePhase::Completed);

    let out = run_belt(tmp.path(), &["queue", "done", &id, "--json"]);
    assert!(out.status.success(), "{out:?}");
    let v = stdout_json(&out);
    assert_eq!(v["result"], "applied");
    assert_eq!(v["phase"], "done");
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Done);
}

#[test]
fn queue_done_with_succeeding_on_done_is_done() {
    let (tmp, db) = setup_belt_home();
    register_workspace_with_on_done(&tmp, &db, "exit 0");
    let id = seed_item(&db, "1", QueuePhase::Completed);
    let repo = scratch_repo();

    let out = run_belt_in(repo.path(), tmp.path(), &["queue", "done", &id, "--json"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let v = stdout_json(&out);
    assert_eq!(v["success"], true);
    assert_eq!(v["result"], "applied");
    assert_eq!(v["phase"], "done");
    assert_eq!(v["scripts_run"], true);
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Done);
}

#[test]
fn queue_done_with_failing_on_done_is_failed_and_refused() {
    let (tmp, db) = setup_belt_home();
    register_workspace_with_on_done(&tmp, &db, "exit 3");
    let id = seed_item(&db, "1", QueuePhase::Completed);
    let repo = scratch_repo();

    let out = run_belt_in(repo.path(), tmp.path(), &["queue", "done", &id, "--json"]);
    assert_eq!(out.status.code(), Some(EXIT_REFUSED), "{out:?}");
    let v = stdout_json(&out);
    assert_eq!(v["success"], false);
    assert_eq!(v["reason"], "on_done_failed");
    assert_eq!(v["phase"], "failed");
    assert_eq!(v["scripts_run"], true);
    assert_eq!(v["exit_code"], 3);
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Failed);
}

#[test]
fn queue_hitl_completed_opens_request() {
    let (tmp, db) = setup_belt_home();
    register_workspace(&tmp, &db);
    let id = seed_item(&db, "1", QueuePhase::Completed);

    let out = run_belt(
        tmp.path(),
        &["queue", "hitl", &id, "--reason", "needs a look", "--json"],
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout_json(&out)["result"], "applied");
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Hitl);

    // A request is open now: a second one is invalid_action.
    let again = run_belt(tmp.path(), &["queue", "hitl", &id, "--json"]);
    assert!(!again.status.success());
    assert_eq!(stdout_json(&again)["reason"], "invalid_action");
}

#[test]
fn queue_hitl_request_expires_like_a_daemon_opened_one() {
    let (tmp, db) = setup_belt_home();
    register_workspace(&tmp, &db);
    let id = seed_item(&db, "1", QueuePhase::Completed);

    let out = run_belt(tmp.path(), &["queue", "hitl", &id, "--json"]);
    assert!(out.status.success(), "{out:?}");

    let request = db.open_hitl_requests().unwrap().remove(0);
    assert_eq!(request.work_id, id);
    assert_eq!(
        request.terminal_action,
        Some(belt_core::escalation::EscalationAction::Skip),
        "the workspace terminal action"
    );
    let timeout_at = chrono::DateTime::parse_from_rfc3339(&request.timeout_at.unwrap()).unwrap();
    let hours = (timeout_at.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_hours();
    assert!((23..=24).contains(&hours), "timeout in {hours}h");
}

#[test]
fn queue_hitl_without_a_readable_workspace_is_an_error() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Completed);

    let out = run_belt(tmp.path(), &["queue", "hitl", &id, "--json"]);

    assert!(!out.status.success(), "{out:?}");
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Completed);
    assert!(db.open_hitl_requests().unwrap().is_empty());
}

#[test]
fn queue_hitl_running_item_is_busy() {
    let (tmp, db) = setup_belt_home();
    register_workspace(&tmp, &db);
    let id = seed_item(&db, "1", QueuePhase::Running);

    let out = run_belt(tmp.path(), &["queue", "hitl", &id, "--json"]);
    assert!(!out.status.success());
    assert_eq!(stdout_json(&out)["reason"], "busy");
}

#[test]
fn queue_skip_open_hitl_wins_response_race() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Hitl);

    let out = run_belt(tmp.path(), &["queue", "skip", &id, "--json"]);
    assert!(out.status.success(), "{out:?}");
    let v = stdout_json(&out);
    assert_eq!(v["success"], true);
    assert_eq!(v["result"], "hitl_response");
    assert_eq!(v["action"], "skip");

    // Confirmed, not applied: the item leaves Hitl only by daemon post-processing.
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Hitl);
    assert_eq!(db.pending_post_processing().unwrap().len(), 1);
}

#[test]
fn queue_done_open_hitl_wins_response_race() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Hitl);

    let out = run_belt(tmp.path(), &["queue", "done", &id, "--json"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout_json(&out)["action"], "done");

    // Confirmed, not applied: the item leaves Hitl only by daemon post-processing.
    assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Hitl);
    assert_eq!(db.pending_post_processing().unwrap().len(), 1);
}

/// Exit code of a refused request; mirrors `EXIT_REFUSED` in the binary.
const EXIT_REFUSED: i32 = 1;

#[test]
fn refusals_exit_with_the_refused_code() {
    let (tmp, db) = setup_belt_home();
    let running = seed_item(&db, "1", QueuePhase::Running);
    let done = seed_item(&db, "2", QueuePhase::Done);

    let busy = run_belt(tmp.path(), &["queue", "done", &running, "--json"]);
    assert_eq!(stdout_json(&busy)["reason"], "busy");
    assert_eq!(busy.status.code(), Some(EXIT_REFUSED));

    let invalid = run_belt(tmp.path(), &["queue", "skip", &done, "--json"]);
    assert_eq!(stdout_json(&invalid)["reason"], "invalid_action");
    assert_eq!(invalid.status.code(), Some(EXIT_REFUSED));

    let missing = run_belt(tmp.path(), &["queue", "skip", "no-such-item", "--json"]);
    assert_eq!(stdout_json(&missing)["reason"], "not_found");
    assert_eq!(missing.status.code(), Some(EXIT_REFUSED));
}

#[test]
fn queue_command_on_hitl_awaiting_post_processing_is_busy() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Hitl);
    assert!(
        run_belt(tmp.path(), &["queue", "skip", &id])
            .status
            .success()
    );

    let out = run_belt(tmp.path(), &["queue", "done", &id, "--json"]);
    assert!(!out.status.success());
    assert_eq!(stdout_json(&out)["reason"], "busy");
}

#[test]
fn queue_retry_script_command_is_removed() {
    let (tmp, _db) = setup_belt_home();
    let out = run_belt(tmp.path(), &["queue", "retry-script", "some-id"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unrecognized subcommand"),
        "stderr should be a clap error: {stderr}"
    );
}

#[test]
fn queue_show_json_has_transition_history_with_rejections() {
    let (tmp, db) = setup_belt_home();
    let id = seed_item(&db, "1", QueuePhase::Running);
    // A refused CLI request is recorded in the history.
    assert!(
        !run_belt(tmp.path(), &["queue", "done", &id])
            .status
            .success()
    );

    let out = run_belt(tmp.path(), &["queue", "show", &id, "--json"]);
    assert!(out.status.success(), "{out:?}");
    let v = stdout_json(&out);
    assert_eq!(v["work_id"], id.as_str());
    assert_eq!(v["processing"], "handler");
    let history = v["transitions"].as_array().expect("transitions array");
    let kinds: Vec<&str> = history
        .iter()
        .map(|t| t["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds.first(), Some(&"item_created"));
    assert_eq!(kinds.last(), Some(&"transition_rejected"));
    let seqs: Vec<u64> = history.iter().map(|t| t["seq"].as_u64().unwrap()).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "oldest first: {seqs:?}"
    );
    let last = history.last().unwrap();
    assert_eq!(last["actor"], "cli");
    assert_eq!(last["to_phase"], "done");
}

#[test]
fn queue_show_includes_derived_origin() {
    let (tmp, db) = setup_belt_home();
    let origin = seed_item(&db, "1", QueuePhase::Failed);
    let derived = db
        .derive(&DeriveRequest {
            work_id: origin.clone(),
            expected_from: QueuePhase::Failed,
            kind: DeriveKind::Replan,
            actor: Actor::Daemon,
            reason: TransitionReason::Derived,
            detail: None,
        })
        .expect("derive");
    let DeriveOutcome::Derived { work_id: derived } = derived else {
        panic!("expected a derived item");
    };

    let out = run_belt(tmp.path(), &["queue", "show", &derived, "--json"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout_json(&out)["derived_from"], origin.as_str());

    let text = run_belt(tmp.path(), &["queue", "show", &derived]);
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(
        stdout.contains(&origin),
        "text output names the origin: {stdout}"
    );
    assert!(
        stdout.contains("item_created"),
        "text output shows history: {stdout}"
    );
}
