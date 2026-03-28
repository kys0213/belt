//! Integration tests for GapDetectionJob.
//!
//! Tests gap detection through the public API: spec coverage analysis,
//! gap reporting, deduplication guards, and spec lifecycle transitions.
//! No external CLI or API calls required -- all tests use in-memory DB
//! and temporary file system fixtures.

use std::sync::Arc;

use belt_core::phase::QueuePhase;
use belt_core::spec::{Spec, SpecStatus};
use belt_daemon::cron::{CronContext, CronHandler, GapAnalysisReport, GapDetectionJob};
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
            "/// Check authorization by validating the bearer token and returning\n",
            "/// the associated role if the token is valid.\n",
            "fn authorization(token: &str, credentials: &HashMap<String, String>) -> Option<String> {\n",
            "    if token.is_empty() {\n",
            "        return None;\n",
            "    }\n",
            "    credentials.get(token).cloned()\n",
            "}\n",
            "\n",
            "/// Middleware that intercepts each request, extracts the bearer token\n",
            "/// from the Authorization header, and validates it before forwarding.\n",
            "fn middleware(request: &Request) -> Result<Response, AuthError> {\n",
            "    let header = request.headers.get(\"Authorization\").ok_or(AuthError::Missing)?;\n",
            "    let token = header.strip_prefix(\"Bearer \").ok_or(AuthError::InvalidScheme)?;\n",
            "    match authorization(token, &request.credentials) {\n",
            "        Some(role) => Ok(Response { status: 200, role }),\n",
            "        None => Err(AuthError::Forbidden),\n",
            "    }\n",
            "}\n",
            "\n",
            "/// Guard that ensures only requests with a valid token can access\n",
            "/// secure routes. Returns 401 for missing tokens, 403 for invalid ones.\n",
            "fn secure(request: &Request) -> Result<(), AuthError> {\n",
            "    let result = middleware(request)?;\n",
            "    if result.status != 200 {\n",
            "        return Err(AuthError::Forbidden);\n",
            "    }\n",
            "    Ok(())\n",
            "}\n",
            "\n",
            "/// Return the list of protected endpoints that require authorization.\n",
            "fn endpoints(config: &Config) -> Vec<Endpoint> {\n",
            "    config.protected_paths.iter().map(|p| {\n",
            "        Endpoint::new(p).with_middleware(authorization_middleware)\n",
            "    }).collect()\n",
            "}\n",
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
///
/// This test verifies three things:
/// 1. `has_open_items_for_source` returns `true` for specs with an open
///    (Pending) queue item — the DB dedupe guard is active.
/// 2. `analyze_gaps()` still detects and reports the gap — the dedupe
///    guard lives in `execute()`, not in analysis.
/// 3. `execute()` completes successfully, skipping issue creation for the
///    duplicate spec.
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
    assert!(
        db.has_open_items_for_source("spec-dup").unwrap(),
        "open Pending queue item must be detected by has_open_items_for_source"
    );

    // Use analyze_gaps() to verify the gap is still detected by pure
    // analysis -- the dedupe guard only suppresses issue creation in
    // execute(), not the analysis itself.
    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    let report = job.analyze_gaps().expect("analyze_gaps should succeed");

    assert_eq!(
        report.gaps.len(),
        1,
        "expected exactly one gap for spec-dup despite open queue item, got: {:?}",
        report.gaps,
    );
    assert_eq!(
        report.gaps[0].spec_id, "spec-dup",
        "gap should reference the correct spec"
    );
    assert!(
        report.gaps[0].coverage_score < 0.5,
        "coverage score {:.2} should be below the default threshold \
         because the workspace code does not cover the spec keywords",
        report.gaps[0].coverage_score,
    );
    assert!(
        !report.gaps[0].missing_items.is_empty(),
        "missing_items should list uncovered keywords from the spec"
    );
    assert!(
        report.covered_spec_ids.is_empty(),
        "no spec should be marked as covered when keywords are absent from the code"
    );

    // Also verify the full execute() path completes without error --
    // it should succeed but skip issue creation due to the dedupe guard.
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
///
/// This test verifies two things:
/// 1. `has_open_items_for_source` returns `false` for specs with only
///    terminal (Done) queue items.
/// 2. `analyze_gaps()` still detects and reports the gap — the terminal
///    item does not suppress gap analysis.
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

    // Verify Done items are not considered "open".
    assert!(
        !db.has_open_items_for_source("spec-done-prev").unwrap(),
        "Done queue items must not be treated as open"
    );

    // Use analyze_gaps() to verify the gap is actually detected despite
    // the terminal queue item.
    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    let report = job.analyze_gaps().expect("analyze_gaps should succeed");

    assert_eq!(
        report.gaps.len(),
        1,
        "expected exactly one gap for spec-done-prev, got: {:?}",
        report.gaps,
    );
    let gap = &report.gaps[0];
    assert_eq!(
        gap.spec_id, "spec-done-prev",
        "gap should reference the correct spec"
    );
    assert_eq!(
        gap.spec_name, "Done Prev Gap",
        "gap should carry the correct spec name"
    );
    assert!(
        gap.coverage_score < 0.5,
        "coverage score {:.2} should be below the default threshold \
         because the workspace code does not cover the spec keywords",
        gap.coverage_score,
    );
    assert!(
        (gap.coverage_score - 0.0).abs() < f64::EPSILON,
        "coverage score should be exactly 0.0 because no spec keywords \
         (authorization, middleware, secure, endpoints) appear in the workspace code, \
         got: {:.2}",
        gap.coverage_score,
    );
    assert!(
        !gap.missing_items.is_empty(),
        "missing_items should list uncovered keywords from the spec"
    );

    // Verify each individual keyword from the spec is reported as missing.
    let joined = gap.missing_items.join(" ").to_lowercase();
    assert!(
        joined.contains("authorization"),
        "missing_items should reference 'authorization', got: {:?}",
        gap.missing_items,
    );
    assert!(
        joined.contains("middleware"),
        "missing_items should reference 'middleware', got: {:?}",
        gap.missing_items,
    );
    assert!(
        joined.contains("secure"),
        "missing_items should reference 'secure', got: {:?}",
        gap.missing_items,
    );
    assert!(
        joined.contains("endpoints"),
        "missing_items should reference 'endpoints', got: {:?}",
        gap.missing_items,
    );

    assert!(
        report.covered_spec_ids.is_empty(),
        "no spec should be marked as covered when keywords are absent from the code"
    );
    assert!(
        !report
            .covered_spec_ids
            .contains(&"spec-done-prev".to_string()),
        "spec-done-prev should not appear in covered_spec_ids"
    );

    // Also verify the full execute() path completes without error.
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

    // Code covers "authorization" and "middleware" with real logic but does
    // NOT cover "token", "session", "role", "access", or "control".
    std::fs::write(
        tmp.path().join("auth.rs"),
        concat!(
            "use std::collections::HashMap;\n",
            "\n",
            "/// Check authorization by verifying the user has the required\n",
            "/// permission for the requested resource.\n",
            "fn authorization(user: &str, permissions: &HashMap<String, Vec<String>>, resource: &str) -> bool {\n",
            "    match permissions.get(user) {\n",
            "        Some(allowed) => allowed.iter().any(|r| r == resource),\n",
            "        None => false,\n",
            "    }\n",
            "}\n",
            "\n",
            "/// Middleware that intercepts each request, extracts the caller\n",
            "/// identity from the header, and delegates to authorization.\n",
            "fn middleware(request: &Request, permissions: &HashMap<String, Vec<String>>) -> Result<Response, AuthError> {\n",
            "    let caller = request.headers.get(\"X-Caller\")\n",
            "        .ok_or(AuthError::MissingCaller)?;\n",
            "    if authorization(caller, permissions, &request.path) {\n",
            "        Ok(Response { status: 200 })\n",
            "    } else {\n",
            "        Err(AuthError::Forbidden)\n",
            "    }\n",
            "}\n",
        ),
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
    let report = job.analyze_gaps().expect("analyze_gaps should succeed");

    assert_eq!(
        report.gaps.len(),
        1,
        "expected exactly one gap for spec-partial at threshold 0.90, got: {:?}",
        report.gaps,
    );
    let gap = &report.gaps[0];
    assert_eq!(gap.spec_id, "spec-partial");
    assert_eq!(
        gap.spec_name, "Partial Auth",
        "gap should carry the correct spec name",
    );

    // The fixture code only covers "authorization" and "middleware" out of
    // 8 spec keywords -- the coverage score must be well below the 0.90
    // threshold regardless of whether keyword or LLM analysis is used.
    assert!(
        gap.coverage_score < 0.90,
        "coverage score {:.2} should be below the 0.90 threshold",
        gap.coverage_score,
    );
    assert!(
        gap.coverage_score < 0.50,
        "coverage score {:.2} should be below 0.50 because only 2 of 8 \
         keywords (authorization, middleware) are present in the fixture code",
        gap.coverage_score,
    );
    assert!(
        gap.coverage_score >= 0.0,
        "coverage score {:.2} must be non-negative",
        gap.coverage_score,
    );

    // Verify the missing items reference uncovered keywords.
    assert!(
        !gap.missing_items.is_empty(),
        "missing_items should list uncovered keywords (token, session, etc.)",
    );
    let joined_missing = gap.missing_items.join(" ").to_lowercase();
    assert!(
        joined_missing.contains("token") || joined_missing.contains("session"),
        "missing_items should include 'token' or 'session', got: {:?}",
        gap.missing_items,
    );
    assert!(
        joined_missing.contains("validation") || joined_missing.contains("role"),
        "missing_items should include 'validation' or 'role', got: {:?}",
        gap.missing_items,
    );
    assert!(
        joined_missing.contains("access") || joined_missing.contains("control"),
        "missing_items should include 'access' or 'control', got: {:?}",
        gap.missing_items,
    );
    // "authorization" and "middleware" should NOT be in missing items.
    assert!(
        !joined_missing.contains("authorization"),
        "authorization is covered and should not be in missing_items, got: {:?}",
        gap.missing_items,
    );
    assert!(
        !joined_missing.contains("middleware"),
        "middleware is covered and should not be in missing_items, got: {:?}",
        gap.missing_items,
    );

    assert!(
        !report
            .covered_spec_ids
            .contains(&"spec-partial".to_string()),
        "spec-partial should not be in covered_spec_ids at threshold 0.90",
    );

    // Also verify the full execute() path completes without error.
    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf())
        .with_coverage_threshold(0.90);
    assert!(
        job.execute(&ctx()).is_ok(),
        "high-threshold gap detection should succeed"
    );
}

/// A very low threshold (near 0) should consider even minimal coverage
/// as sufficient.  The fixture code provides real authentication logic
/// covering the spec keywords (authorization, middleware, token, session,
/// validation) so that the spec is treated as covered at the lenient
/// threshold.
#[test]
fn low_threshold_accepts_minimal_coverage() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    // Real authentication logic covering the spec keywords.
    std::fs::write(
        tmp.path().join("auth.rs"),
        concat!(
            "use std::collections::HashMap;\n",
            "\n",
            "/// Validate a bearer token by checking its signature and expiry.\n",
            "fn token_validation(raw: &str, secret: &[u8]) -> Result<Claims, AuthError> {\n",
            "    let token = raw.strip_prefix(\"Bearer \").ok_or(AuthError::InvalidToken)?;\n",
            "    let claims = decode(token, secret)?;\n",
            "    if claims.is_expired() {\n",
            "        return Err(AuthError::Expired);\n",
            "    }\n",
            "    Ok(claims)\n",
            "}\n",
            "\n",
            "/// Manage a user session after successful authentication.\n",
            "fn session_manager(store: &mut HashMap<String, Session>, claims: &Claims) -> SessionId {\n",
            "    let session = Session::new(claims.sub.clone(), claims.exp);\n",
            "    let id = session.id.clone();\n",
            "    store.insert(id.clone(), session);\n",
            "    id\n",
            "}\n",
            "\n",
            "/// Authorization check: verify the caller has the required role.\n",
            "fn authorization(claims: &Claims, required_role: &str) -> bool {\n",
            "    claims.roles.iter().any(|r| r == required_role)\n",
            "}\n",
            "\n",
            "/// Middleware layer that chains token validation, session lookup,\n",
            "/// and authorization before forwarding to the inner handler.\n",
            "fn middleware(request: &Request, secret: &[u8], sessions: &mut HashMap<String, Session>) -> Response {\n",
            "    let header = match request.headers.get(\"Authorization\") {\n",
            "        Some(h) => h,\n",
            "        None => return Response::unauthorized(\"missing authorization header\"),\n",
            "    };\n",
            "    let claims = match token_validation(header, secret) {\n",
            "        Ok(c) => c,\n",
            "        Err(e) => return Response::unauthorized(&format!(\"auth failed: {}\", e)),\n",
            "    };\n",
            "    let _session_id = session_manager(sessions, &claims);\n",
            "    if !authorization(&claims, \"user\") {\n",
            "        return Response::forbidden(\"insufficient permissions\");\n",
            "    }\n",
            "    Response::ok()\n",
            "}\n",
        ),
    )
    .unwrap();

    let spec = make_active_spec(
        "spec-lenient",
        "Lenient Auth",
        "authorization middleware token session validation",
    );
    db.insert_spec(&spec).unwrap();

    // Use analyze_gaps() first to verify the spec is treated as covered
    // at the lenient threshold (before execute() transitions spec status).
    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf())
        .with_coverage_threshold(0.10);
    let report = job.analyze_gaps().expect("analyze_gaps should succeed");

    assert!(
        report
            .covered_spec_ids
            .contains(&"spec-lenient".to_string()),
        "spec-lenient should be covered at threshold 0.10, gaps: {:?}",
        report.gaps,
    );
    assert!(
        report.gaps.is_empty(),
        "no gaps should be reported at lenient threshold, got: {:?}",
        report.gaps,
    );

    // Also verify the full execute() path completes without error.
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
        concat!(
            "use tracing::{info, warn};\n",
            "\n",
            "/// Initialise structured logging with a tracing subscriber.\n",
            "/// Configures log level filtering and a JSON-formatted output layer.\n",
            "fn logging(level: &str) -> Result<(), Box<dyn std::error::Error>> {\n",
            "    let filter = tracing_subscriber::EnvFilter::try_from_default_env()\n",
            "        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));\n",
            "    tracing_subscriber::fmt()\n",
            "        .with_env_filter(filter)\n",
            "        .with_target(true)\n",
            "        .with_thread_ids(true)\n",
            "        .json()\n",
            "        .init();\n",
            "    info!(level = %level, \"structured logging initialised\");\n",
            "    Ok(())\n",
            "}\n",
            "\n",
            "/// Run periodic monitoring health checks and emit status events.\n",
            "/// Returns the overall health status of all registered services.\n",
            "fn monitoring(services: &[&str]) -> HealthStatus {\n",
            "    let mut all_healthy = true;\n",
            "    for svc in services {\n",
            "        let healthy = check_health(svc);\n",
            "        if healthy {\n",
            "            info!(service = %svc, \"health check passed\");\n",
            "        } else {\n",
            "            warn!(service = %svc, \"health check failed\");\n",
            "            all_healthy = false;\n",
            "        }\n",
            "    }\n",
            "    if all_healthy { HealthStatus::Ok } else { HealthStatus::Degraded }\n",
            "}\n",
            "\n",
            "/// Collect, aggregate, and export runtime metrics.\n",
            "/// Tracks request count, error rate, and latency percentiles.\n",
            "fn metrics(collector: &MetricsCollector) -> MetricsSnapshot {\n",
            "    let snapshot = MetricsSnapshot {\n",
            "        request_count: collector.counter(\"http_requests_total\"),\n",
            "        error_rate: collector.gauge(\"error_rate\"),\n",
            "        p99_latency_ms: collector.histogram_percentile(\"latency_ms\", 0.99),\n",
            "    };\n",
            "    info!(\n",
            "        requests = snapshot.request_count,\n",
            "        errors = %snapshot.error_rate,\n",
            "        p99 = %snapshot.p99_latency_ms,\n",
            "        \"metrics snapshot exported\"\n",
            "    );\n",
            "    snapshot\n",
            "}\n",
        ),
    )
    .unwrap();

    // Spec A: fully covered (keywords: logging, monitoring, metrics).
    let spec_a = make_active_spec("spec-a", "Logging", "logging monitoring metrics");
    db.insert_spec(&spec_a).unwrap();

    // Spec B: not covered at all (keywords: authorization, middleware, token, validation).
    let spec_b = make_active_spec(
        "spec-b",
        "Auth Gap Multi",
        "authorization middleware token validation",
    );
    db.insert_spec(&spec_b).unwrap();

    // Use analyze_gaps() to verify independent per-spec analysis.
    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    let report = job.analyze_gaps().expect("analyze_gaps should succeed");

    // Spec A should be covered -- its keywords all appear in lib.rs.
    assert!(
        report.covered_spec_ids.contains(&"spec-a".to_string()),
        "spec-a (Logging) should be covered, covered_ids: {:?}, gaps: {:?}",
        report.covered_spec_ids,
        report.gaps,
    );

    // Spec B should have a gap -- none of its keywords appear in lib.rs.
    assert_eq!(
        report.gaps.len(),
        1,
        "expected exactly one gap (spec-b), got: {:?}",
        report.gaps,
    );
    let gap_b = &report.gaps[0];
    assert_eq!(
        gap_b.spec_id, "spec-b",
        "the gap should be for spec-b (Auth Gap Multi)",
    );
    assert_eq!(
        gap_b.spec_name, "Auth Gap Multi",
        "gap should carry the correct spec name for spec-b",
    );
    assert!(
        gap_b.coverage_score < 0.5,
        "spec-b coverage score {:.2} should be below threshold since no keywords are covered",
        gap_b.coverage_score,
    );
    assert!(
        (gap_b.coverage_score - 0.0).abs() < f64::EPSILON,
        "spec-b coverage score should be exactly 0.0 because none of the keywords \
         (authorization, middleware, token, validation) appear in lib.rs, got: {:.2}",
        gap_b.coverage_score,
    );
    assert!(
        !gap_b.missing_items.is_empty(),
        "spec-b should have missing items listing uncovered keywords",
    );

    // Verify each individual keyword from spec-b is reported as missing.
    let joined_b = gap_b.missing_items.join(" ").to_lowercase();
    assert!(
        joined_b.contains("authorization"),
        "missing_items should reference 'authorization', got: {:?}",
        gap_b.missing_items,
    );
    assert!(
        joined_b.contains("middleware"),
        "missing_items should reference 'middleware', got: {:?}",
        gap_b.missing_items,
    );
    assert!(
        joined_b.contains("token"),
        "missing_items should reference 'token', got: {:?}",
        gap_b.missing_items,
    );
    assert!(
        joined_b.contains("validation"),
        "missing_items should reference 'validation', got: {:?}",
        gap_b.missing_items,
    );

    // Spec B should NOT appear in covered_spec_ids.
    assert!(
        !report.covered_spec_ids.contains(&"spec-b".to_string()),
        "spec-b should not be in covered_spec_ids when it has a gap",
    );

    // Also verify the full execute() path completes without error.
    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    assert!(
        job.execute(&ctx()).is_ok(),
        "gap detection should succeed analyzing multiple specs"
    );
}

// ---------------------------------------------------------------------------
// Auth Gap Multi: standalone spec-b gap detection
// ---------------------------------------------------------------------------

/// Dedicated test for the "Auth Gap Multi" (spec-b) gap detection scenario.
///
/// When the workspace contains no authorization, middleware, token, or
/// validation code, gap analysis must report spec-b as a gap with a
/// coverage score below the default threshold and all four keywords
/// listed as missing.  This mirrors the gap-detection cron report for
/// spec-b (coverage 0.00, threshold 0.50).
#[test]
fn auth_gap_multi_detected_when_no_auth_code_present() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    // Workspace contains only unrelated code -- no authorization,
    // middleware, token, or validation keywords.
    std::fs::write(
        tmp.path().join("main.rs"),
        concat!(
            "/// Application entry point.\n",
            "fn main() {\n",
            "    println!(\"hello world\");\n",
            "}\n",
        ),
    )
    .unwrap();

    let spec = make_active_spec(
        "spec-b",
        "Auth Gap Multi",
        "authorization middleware token validation",
    );
    db.insert_spec(&spec).unwrap();

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    let report = job.analyze_gaps().expect("analyze_gaps should succeed");

    // Exactly one gap expected for spec-b.
    assert_eq!(
        report.gaps.len(),
        1,
        "expected exactly one gap for spec-b (Auth Gap Multi), got: {:?}",
        report.gaps,
    );

    let gap = &report.gaps[0];
    assert_eq!(gap.spec_id, "spec-b", "gap should reference spec-b");
    assert_eq!(
        gap.spec_name, "Auth Gap Multi",
        "gap should carry the correct spec name",
    );
    assert!(
        gap.coverage_score < 0.5,
        "coverage score {:.2} should be below the 0.50 default threshold \
         because no auth code is present",
        gap.coverage_score,
    );
    assert!(
        (gap.coverage_score - 0.0).abs() < f64::EPSILON,
        "coverage score should be exactly 0.0 because no spec keywords \
         (authorization, middleware, token, validation) appear in the workspace, \
         got: {:.2}",
        gap.coverage_score,
    );
    assert!(
        !gap.missing_items.is_empty(),
        "missing_items should list uncovered keywords",
    );

    // Verify each spec keyword is reported as missing.
    let joined = gap.missing_items.join(" ").to_lowercase();
    assert!(
        joined.contains("authorization"),
        "missing_items should reference 'authorization', got: {:?}",
        gap.missing_items,
    );
    assert!(
        joined.contains("middleware"),
        "missing_items should reference 'middleware', got: {:?}",
        gap.missing_items,
    );
    assert!(
        joined.contains("token"),
        "missing_items should reference 'token', got: {:?}",
        gap.missing_items,
    );
    assert!(
        joined.contains("validation"),
        "missing_items should reference 'validation', got: {:?}",
        gap.missing_items,
    );

    // Spec-b must NOT appear in covered_spec_ids.
    assert!(
        !report.covered_spec_ids.contains(&"spec-b".to_string()),
        "spec-b should not be covered when no auth code is in the workspace",
    );
    assert!(
        report.covered_spec_ids.is_empty(),
        "no spec should be marked as covered when all keywords are absent from the workspace",
    );

    // Full execute() path should also succeed.
    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    assert!(
        job.execute(&ctx()).is_ok(),
        "gap detection execute should succeed for Auth Gap Multi",
    );
}

// ---------------------------------------------------------------------------
// Spec with no extractable keywords
// ---------------------------------------------------------------------------

/// Specs whose content yields no extractable keywords (all words are
/// stop-words or below the minimum keyword length) should be treated
/// as covered -- no gap reported, and the spec appears in
/// `covered_spec_ids`.
#[test]
fn spec_with_no_keywords_treated_as_covered() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    std::fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();

    // Content with only very short/common words that get filtered out.
    let spec = make_active_spec("spec-no-kw", "No Keywords", "a an the is it of to");
    db.insert_spec(&spec).unwrap();

    // Verify via analyze_gaps() that the spec is treated as covered.
    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
    let report = job.analyze_gaps().expect("analyze_gaps should succeed");

    assert!(
        report.gaps.is_empty(),
        "no gaps should be reported for a spec with no extractable keywords, got: {:?}",
        report.gaps,
    );
    assert!(
        report.covered_spec_ids.contains(&"spec-no-kw".to_string()),
        "spec-no-kw should be in covered_spec_ids when all keywords are filtered out, \
         covered_ids: {:?}",
        report.covered_spec_ids,
    );

    // Also verify the full execute() path completes without error.
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
///
/// When keywords only appear in a `.txt` file (not a recognized source
/// extension), they must NOT count as coverage -- a gap should be reported.
#[test]
fn ignores_non_source_files() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    // Put the keywords in a non-source file only.
    std::fs::write(
        tmp.path().join("notes.txt"),
        "authorization middleware token validation",
    )
    .unwrap();
    // Source file has no matching keywords.
    std::fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();

    let spec = make_active_spec(
        "spec-ext",
        "Extension Check",
        "authorization middleware token validation",
    );
    db.insert_spec(&spec).unwrap();

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());

    // Verify via analyze_gaps that the .txt file did NOT satisfy coverage.
    let report: GapAnalysisReport = job.analyze_gaps().expect("analyze_gaps should succeed");
    assert_eq!(
        report.gaps.len(),
        1,
        "expected exactly one gap when keywords only appear in .txt file, got: {:?}",
        report.gaps,
    );
    assert_eq!(report.gaps[0].spec_id, "spec-ext");
    assert!(
        report.gaps[0].coverage_score < 0.5,
        "coverage score {:.2} should be below the default threshold because .txt is not scanned",
        report.gaps[0].coverage_score,
    );
    assert!(
        !report.gaps[0].missing_items.is_empty(),
        "missing_items should list the uncovered keywords",
    );
    assert!(
        report.covered_spec_ids.is_empty(),
        "no spec should be covered when keywords are only in non-source files",
    );
}

/// When keywords appear in a recognized source file (.rs), they should
/// count as coverage even if the same keywords also exist in a .txt file.
#[test]
fn source_file_satisfies_coverage_over_non_source() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    // Keywords in both .txt (should be ignored) and .rs (should be scanned).
    std::fs::write(
        tmp.path().join("notes.txt"),
        "authorization middleware token validation",
    )
    .unwrap();
    // Use realistic function bodies so keyword-based and LLM-based analysis
    // both treat the spec as covered (empty stubs are flagged by LLM).
    std::fs::write(
        tmp.path().join("auth.rs"),
        concat!(
            "/// Checks authorization by verifying the user role against the ACL.\n",
            "fn authorization(user: &User, acl: &[String]) -> bool {\n",
            "    acl.contains(&user.role)\n",
            "}\n",
            "\n",
            "/// Middleware that intercepts requests and validates auth headers.\n",
            "fn middleware(req: &Request) -> Result<Response, Error> {\n",
            "    let header = req.headers.get(\"Authorization\").ok_or(Error::Missing)?;\n",
            "    let t = parse_token(header)?;\n",
            "    if authorization(&t.user, &req.acl) { Ok(Response::ok()) } else { Err(Error::Forbidden) }\n",
            "}\n",
            "\n",
            "/// Parses and decodes a bearer token from the Authorization header.\n",
            "fn token(raw: &str) -> Result<Token, Error> {\n",
            "    let stripped = raw.strip_prefix(\"Bearer \").ok_or(Error::InvalidScheme)?;\n",
            "    decode_jwt(stripped)\n",
            "}\n",
            "\n",
            "/// Validates the token signature, expiration, and issuer claims.\n",
            "fn validation(token: &Token, keys: &KeySet) -> Result<(), Error> {\n",
            "    if token.is_expired() { return Err(Error::Expired); }\n",
            "    keys.verify(&token.signature)?;\n",
            "    Ok(())\n",
            "}\n",
        ),
    )
    .unwrap();

    let spec = make_active_spec(
        "spec-ext-covered",
        "Extension Covered",
        "authorization middleware token validation",
    );
    db.insert_spec(&spec).unwrap();

    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());

    let report: GapAnalysisReport = job.analyze_gaps().expect("analyze_gaps should succeed");
    assert!(
        report.gaps.is_empty(),
        "expected no gaps when keywords are in .rs source file, got: {:?}",
        report.gaps,
    );
    assert!(
        report
            .covered_spec_ids
            .contains(&"spec-ext-covered".to_string()),
        "spec should be marked as covered when keywords appear in recognized source files",
    );
}

// ---------------------------------------------------------------------------
// Threshold boundary: exact threshold values
// ---------------------------------------------------------------------------

/// Coverage threshold of 0.0 means everything is covered.
/// Even when no spec keywords appear in the workspace code, a threshold
/// of 0.0 should treat all specs as covered (no gaps reported).
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

    // Verify via analyze_gaps that threshold 0.0 treats everything as covered.
    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf())
        .with_coverage_threshold(0.0);
    let report = job.analyze_gaps().expect("analyze_gaps should succeed");

    assert!(
        report.gaps.is_empty(),
        "threshold 0.0 should report no gaps regardless of keyword coverage, got: {:?}",
        report.gaps,
    );
    assert!(
        report.covered_spec_ids.contains(&"spec-zero".to_string()),
        "spec-zero should be marked as covered at threshold 0.0, covered_ids: {:?}",
        report.covered_spec_ids,
    );

    // Also verify the full execute() path completes without error.
    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf())
        .with_coverage_threshold(0.0);
    assert!(
        job.execute(&ctx()).is_ok(),
        "threshold 0.0 should succeed (no gaps reported)"
    );
}

/// Coverage threshold of 1.0 means only 100% coverage is acceptable.
/// With partial keyword coverage (only "authorization" out of four keywords),
/// the gap should be detected and the report should list the missing keywords.
#[test]
fn threshold_one_requires_full_coverage() {
    let db = test_db();
    let tmp = tempfile::tempdir().unwrap();

    // Partial coverage: only "authorization" keyword is present with real
    // logic. "middleware", "token", and "session" are absent.
    std::fs::write(
        tmp.path().join("auth.rs"),
        concat!(
            "use std::collections::HashMap;\n",
            "\n",
            "/// Check authorization by verifying the caller has the required\n",
            "/// permission entry in the permissions map.\n",
            "fn authorization(caller: &str, permissions: &HashMap<String, Vec<String>>, resource: &str) -> bool {\n",
            "    match permissions.get(caller) {\n",
            "        Some(allowed) => allowed.iter().any(|r| r == resource),\n",
            "        None => false,\n",
            "    }\n",
            "}\n",
        ),
    )
    .unwrap();

    let spec = make_active_spec(
        "spec-full",
        "Full Threshold",
        "authorization middleware token session",
    );
    db.insert_spec(&spec).unwrap();

    // Verify via analyze_gaps that threshold 1.0 detects the gap.
    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf())
        .with_coverage_threshold(1.0);
    let report = job.analyze_gaps().expect("analyze_gaps should succeed");

    assert_eq!(
        report.gaps.len(),
        1,
        "expected exactly one gap when threshold requires 100% coverage, got: {:?}",
        report.gaps,
    );
    let gap = &report.gaps[0];
    assert_eq!(
        gap.spec_id, "spec-full",
        "the gap should reference spec-full"
    );
    assert!(
        gap.coverage_score < 1.0,
        "coverage score {:.2} should be below 1.0 because only partial keywords are covered",
        gap.coverage_score,
    );
    // "authorization" is covered (1 of 4 keywords), so coverage should be ~0.25, not 0.0.
    assert!(
        gap.coverage_score > 0.0,
        "coverage score should be above 0.0 because 'authorization' keyword IS present \
         in the workspace code, got: {:.2}",
        gap.coverage_score,
    );
    assert!(
        gap.coverage_score < 0.5,
        "coverage score {:.2} should be below 0.5 because only 1 of 4 keywords is covered",
        gap.coverage_score,
    );
    assert!(
        !gap.missing_items.is_empty(),
        "missing_items should list uncovered keywords (middleware, token, session)"
    );
    // Verify the missing items reference the specific uncovered keywords.
    let joined = gap.missing_items.join(" ").to_lowercase();
    assert!(
        joined.contains("middleware") || joined.contains("token"),
        "missing_items should reference 'middleware' or 'token' (uncovered keywords), got: {:?}",
        gap.missing_items,
    );
    assert!(
        joined.contains("session"),
        "missing_items should reference 'session' (uncovered keyword), got: {:?}",
        gap.missing_items,
    );
    assert!(
        !report.covered_spec_ids.contains(&"spec-full".to_string()),
        "spec-full should not be in covered_spec_ids when coverage is partial at threshold 1.0",
    );

    // Also verify the full execute() path completes without error.
    let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf())
        .with_coverage_threshold(1.0);
    assert!(
        job.execute(&ctx()).is_ok(),
        "threshold 1.0 should succeed (gap reported but execution continues)"
    );
}
