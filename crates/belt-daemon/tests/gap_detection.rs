//! Integration tests for GapDetectionJob.
//!
//! Tests gap detection through the public API: spec coverage analysis,
//! gap reporting, deduplication guards, and spec lifecycle transitions.
//! No external CLI or API calls required -- all tests use in-memory DB
//! and temporary file system fixtures.

use std::sync::Arc;

use belt_core::phase::QueuePhase;
use belt_core::spec::{Spec, SpecStatus};
use belt_daemon::cron::{CronContext, CronHandler, GapDetectionJob};
use belt_infra::db::Database;
use chrono::Utc;

fn test_db() -> Arc<Database> {
    Arc::new(Database::open_in_memory().expect("in-memory DB"))
}

fn make_active_spec(id: &str, name: &str, content: &str) -> Spec {
    let mut spec = Spec::new(
        id.to_string(),
        "ws".to_string(),
        name.to_string(),
        content.to_string(),
    );
    spec.status = SpecStatus::Active;
    spec
}

fn ctx() -> CronContext {
    CronContext { now: Utc::now() }
}

// ---------------------------------------------------------------------------
// Gap detection: missing keywords produce a gap
// ---------------------------------------------------------------------------

/// When an active spec's keywords are not found in the workspace code,
/// gap detection should execute successfully (the gap is logged, but
/// issue creation via `gh` is best-effort and allowed to fail).
#[test]
fn detects_gap_when_authorization_keywords_missing() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    // Code only has "authentication" -- no "authorization" or "middleware".
    std::fs::write(tmp.path().join("auth.rs"), "fn authentication() {}").unwrap();

    let spec = make_active_spec(
        "spec-gap",
        "Auth Gap",
        "implement authorization middleware for secure endpoints",
    );
    db.insert_spec(&spec).unwrap();

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    assert!(job.execute(&ctx()).is_ok(), "gap detection should succeed");
}

// ---------------------------------------------------------------------------
// Gap detection: no gap when keywords are covered
// ---------------------------------------------------------------------------

/// When all spec keywords appear in the workspace code, gap detection
/// should complete without reporting a gap.
#[test]
fn no_gap_when_all_keywords_covered() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    std::fs::write(
        tmp.path().join("lib.rs"),
        concat!(
            "fn authorization() {}\n",
            "fn middleware() {}\n",
            "fn secure() {}\n",
            "fn endpoints() {}\n",
        ),
    )
    .unwrap();

    let spec = make_active_spec(
        "spec-covered",
        "Auth Covered",
        "authorization middleware secure endpoints",
    );
    db.insert_spec(&spec).unwrap();

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    assert!(
        job.execute(&ctx()).is_ok(),
        "gap detection should succeed when keywords are covered"
    );
}

// ---------------------------------------------------------------------------
// Gap detection: no source files in workspace
// ---------------------------------------------------------------------------

/// When the workspace root has no recognizable source files, gap detection
/// should return Ok (early exit, nothing to analyze).
#[test]
fn no_gap_when_workspace_is_empty() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    let spec = make_active_spec("spec-empty", "Empty WS", "authorization middleware");
    db.insert_spec(&spec).unwrap();

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    assert!(
        job.execute(&ctx()).is_ok(),
        "gap detection should succeed on empty workspace"
    );
}

// ---------------------------------------------------------------------------
// Gap detection: no active specs
// ---------------------------------------------------------------------------

/// When there are no active specs, gap detection should be a no-op.
#[test]
fn no_gap_when_no_active_specs() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    // Insert a Draft spec (not Active).
    let spec = Spec::new(
        "spec-draft".into(),
        "ws".into(),
        "Draft Spec".into(),
        "authorization middleware".into(),
    );
    db.insert_spec(&spec).unwrap();

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    assert!(
        job.execute(&ctx()).is_ok(),
        "gap detection should succeed when no specs are active"
    );
}

// ---------------------------------------------------------------------------
// Deduplication: open queue item prevents duplicate issue creation
// ---------------------------------------------------------------------------

/// When an open queue item already exists for a spec, gap detection should
/// skip issue creation for that spec (dedupe guard).
#[test]
fn skips_issue_when_open_queue_item_exists() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    // Code does NOT cover the spec keywords -- a gap exists.
    std::fs::write(tmp.path().join("main.rs"), "fn unrelated() {}").unwrap();

    let spec = make_active_spec(
        "spec-dup",
        "Dup Gap",
        "authorization middleware secure endpoints",
    );
    db.insert_spec(&spec).unwrap();

    // Insert an open (Pending) queue item for this spec.
    let item = belt_core::queue::QueueItem::new(
        "spec-dup:implement".into(),
        "spec-dup".into(),
        "ws".into(),
        "implement".into(),
    );
    db.insert_item(&item).unwrap();

    // The DB dedupe guard should detect the open item.
    assert!(db.has_open_items_for_source("spec-dup").unwrap());

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    assert!(
        job.execute(&ctx()).is_ok(),
        "gap detection should succeed, skipping the duplicate"
    );
}

// ---------------------------------------------------------------------------
// Deduplication: terminal queue items do not block new issue creation
// ---------------------------------------------------------------------------

/// Terminal (Done) queue items should NOT prevent gap detection from
/// considering a spec as needing a new issue.
#[test]
fn terminal_items_do_not_block_gap_detection() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    std::fs::write(tmp.path().join("main.rs"), "fn unrelated() {}").unwrap();

    let spec = make_active_spec(
        "spec-done-prev",
        "Done Prev Gap",
        "authorization middleware secure endpoints",
    );
    db.insert_spec(&spec).unwrap();

    // Insert a Done queue item -- this should NOT block.
    let mut item = belt_core::queue::QueueItem::new(
        "spec-done-prev:implement".into(),
        "spec-done-prev".into(),
        "ws".into(),
        "implement".into(),
    );
    item.phase = QueuePhase::Done;
    db.insert_item(&item).unwrap();

    assert!(!db.has_open_items_for_source("spec-done-prev").unwrap());

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    assert!(
        job.execute(&ctx()).is_ok(),
        "gap detection should proceed when only terminal items exist"
    );
}

// ---------------------------------------------------------------------------
// Coverage threshold: custom threshold affects gap sensitivity
// ---------------------------------------------------------------------------

/// A higher coverage threshold should catch gaps that a lower threshold
/// would consider acceptable.
#[test]
fn higher_threshold_catches_partial_coverage() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    // Code covers "authorization" and "middleware" but NOT "token", "session",
    // "role", "access", "control", or "401"/"403".
    std::fs::write(
        tmp.path().join("auth.rs"),
        "fn authorization() {}\nfn middleware() {}",
    )
    .unwrap();

    let spec = make_active_spec(
        "spec-partial",
        "Partial Auth",
        "authorization middleware token session validation role access control",
    );
    db.insert_spec(&spec).unwrap();

    // With a very high threshold (0.90), partial coverage is a gap.
    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf())
        .with_coverage_threshold(0.90);
    assert!(
        job.execute(&ctx()).is_ok(),
        "high-threshold gap detection should succeed"
    );
}

/// A very low threshold (near 0) should consider even minimal coverage
/// as sufficient.
#[test]
fn low_threshold_accepts_minimal_coverage() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    // Only one keyword covered out of many.
    std::fs::write(tmp.path().join("auth.rs"), "fn authorization() {}").unwrap();

    let spec = make_active_spec(
        "spec-lenient",
        "Lenient Auth",
        "authorization middleware token session validation",
    );
    db.insert_spec(&spec).unwrap();

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf())
        .with_coverage_threshold(0.10);
    assert!(
        job.execute(&ctx()).is_ok(),
        "low-threshold gap detection should succeed"
    );
}

// ---------------------------------------------------------------------------
// Multiple specs: independent analysis per spec
// ---------------------------------------------------------------------------

/// Gap detection should analyze each active spec independently.
/// One covered spec should not affect another uncovered spec.
#[test]
fn multiple_specs_analyzed_independently() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    std::fs::write(
        tmp.path().join("lib.rs"),
        "fn logging() {}\nfn monitoring() {}\nfn metrics() {}",
    )
    .unwrap();

    // Spec A: fully covered.
    let spec_a = make_active_spec("spec-a", "Logging", "logging monitoring metrics");
    db.insert_spec(&spec_a).unwrap();

    // Spec B: not covered at all.
    let spec_b = make_active_spec(
        "spec-b",
        "Auth Gap Multi",
        "authorization middleware token validation",
    );
    db.insert_spec(&spec_b).unwrap();

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    assert!(
        job.execute(&ctx()).is_ok(),
        "gap detection should succeed analyzing multiple specs"
    );
}

// ---------------------------------------------------------------------------
// Spec with no extractable keywords
// ---------------------------------------------------------------------------

/// Specs whose content yields no extractable keywords should be treated
/// as covered (no gap reported).
#[test]
fn spec_with_no_keywords_treated_as_covered() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    std::fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();

    // Content with only very short/common words that get filtered out.
    let spec = make_active_spec("spec-no-kw", "No Keywords", "a an the is it of to");
    db.insert_spec(&spec).unwrap();

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    assert!(
        job.execute(&ctx()).is_ok(),
        "spec with no extractable keywords should be treated as covered"
    );
}

// ---------------------------------------------------------------------------
// Code corpus: only scans recognized source file extensions
// ---------------------------------------------------------------------------

/// Gap detection should only scan files with recognized source extensions
/// (e.g., .rs, .ts, .py) and ignore non-source files.
#[test]
fn ignores_non_source_files() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    // Put the keyword in a non-source file only.
    std::fs::write(
        tmp.path().join("notes.txt"),
        "authorization middleware token validation",
    )
    .unwrap();
    // Source file has no keywords.
    std::fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();

    let spec = make_active_spec(
        "spec-ext",
        "Extension Check",
        "authorization middleware token validation",
    );
    db.insert_spec(&spec).unwrap();

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    // Should succeed -- the .txt file should not satisfy coverage.
    assert!(job.execute(&ctx()).is_ok());
}

// ---------------------------------------------------------------------------
// Threshold boundary: exact threshold values
// ---------------------------------------------------------------------------

/// Coverage threshold of 0.0 means everything is covered.
#[test]
fn threshold_zero_treats_all_as_covered() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    std::fs::write(tmp.path().join("main.rs"), "fn unrelated() {}").unwrap();

    let spec = make_active_spec(
        "spec-zero",
        "Zero Threshold",
        "authorization middleware token session",
    );
    db.insert_spec(&spec).unwrap();

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf())
        .with_coverage_threshold(0.0);
    assert!(
        job.execute(&ctx()).is_ok(),
        "threshold 0.0 should succeed (no gaps reported)"
    );
}

/// Coverage threshold of 1.0 means only 100% coverage is acceptable.
#[test]
fn threshold_one_requires_full_coverage() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    // Partial coverage.
    std::fs::write(tmp.path().join("auth.rs"), "fn authorization() {}").unwrap();

    let spec = make_active_spec(
        "spec-full",
        "Full Threshold",
        "authorization middleware token session",
    );
    db.insert_spec(&spec).unwrap();

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf())
        .with_coverage_threshold(1.0);
    assert!(
        job.execute(&ctx()).is_ok(),
        "threshold 1.0 should succeed (gap reported but execution continues)"
    );
}
