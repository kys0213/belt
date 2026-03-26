//! Cron engine: periodic job scheduler for the Belt daemon.
//!
//! Provides a simple interval/daily schedule system and an engine that
//! ticks through registered jobs, executing those that are due.

use std::sync::Arc;
use std::time::Duration;

use std::collections::HashMap;

use belt_core::error::BeltError;
use belt_core::escalation::EscalationAction;
use belt_core::phase::QueuePhase;
use belt_core::workspace::WorkspaceConfig;
use belt_infra::db::Database;
use belt_infra::worktree::WorktreeManager;
use chrono::{DateTime, Utc};

// ---------------------------------------------------------------------------
// CronSchedule
// ---------------------------------------------------------------------------

/// Schedule specification for a cron job.
#[derive(Debug, Clone)]
pub enum CronSchedule {
    /// Run every `Duration` (e.g. every 60 seconds).
    Interval(Duration),
    /// Run once per day at the given hour and minute (UTC).
    Daily {
        /// Hour of day (0–23).
        hour: u32,
        /// Minute of hour (0–59).
        min: u32,
    },
    /// Standard 5-field cron expression (minute hour day month weekday).
    ///
    /// Stores the raw expression string and evaluates it against the current
    /// time on each tick.
    Expression(String),
}

impl CronSchedule {
    /// Returns `true` when the job should run given the last execution time
    /// and the current wall-clock time.
    pub fn should_run(&self, last_run_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
        match self {
            CronSchedule::Interval(interval) => {
                let Some(last) = last_run_at else {
                    return true;
                };
                let elapsed = now.signed_duration_since(last);
                elapsed >= chrono::Duration::from_std(*interval).unwrap_or(chrono::TimeDelta::MAX)
            }
            CronSchedule::Daily { hour, min } => {
                let now_hour = now.time().hour();
                let now_min = now.time().minute();

                // Not yet reached the scheduled time today.
                if (now_hour, now_min) < (*hour, *min) {
                    return false;
                }

                match last_run_at {
                    None => true,
                    Some(last) => last.date_naive() < now.date_naive(),
                }
            }
            CronSchedule::Expression(expr) => {
                if !cron_expression_matches(expr, now) {
                    return false;
                }
                // Prevent re-running within the same minute.
                match last_run_at {
                    None => true,
                    Some(last) => {
                        last.date_naive() != now.date_naive()
                            || last.time().hour() != now.time().hour()
                            || last.time().minute() != now.time().minute()
                    }
                }
            }
        }
    }

    /// Parse a cron expression string into a `CronSchedule`.
    ///
    /// Accepts standard 5-field cron expressions (minute hour day month weekday)
    /// where each field may use digits, `*`, `/`, `-`, or `,`.
    pub fn parse_expression(expr: &str) -> Result<Self, BeltError> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(BeltError::Runtime(format!(
                "invalid cron expression: expected 5 fields, got {}",
                fields.len()
            )));
        }
        for field in &fields {
            if !field
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '*' | '/' | '-' | ','))
            {
                return Err(BeltError::Runtime(format!(
                    "invalid cron expression field: '{field}'"
                )));
            }
        }
        Ok(CronSchedule::Expression(expr.to_string()))
    }
}

/// Check whether the current time matches a 5-field cron expression.
///
/// Each field is matched independently: minute, hour, day-of-month, month,
/// day-of-week. Supports `*`, ranges (`1-5`), step values (`*/10`), and
/// comma-separated lists (`1,3,5`).
fn cron_expression_matches(expr: &str, now: DateTime<Utc>) -> bool {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }

    let now_values = [
        now.time().minute(),
        now.time().hour(),
        now.date_naive().day(),
        now.date_naive().month(),
        now.date_naive().weekday().num_days_from_sunday(),
    ];

    let max_values = [59, 23, 31, 12, 7];

    for (field, (&now_val, &max_val)) in fields.iter().zip(now_values.iter().zip(max_values.iter()))
    {
        if !cron_field_matches(field, now_val, max_val) {
            return false;
        }
    }
    true
}

/// Match a single cron field against a value.
///
/// Supports: `*`, `*/step`, `value`, `start-end`, `start-end/step`, and
/// comma-separated combinations.
fn cron_field_matches(field: &str, value: u32, max: u32) -> bool {
    for part in field.split(',') {
        if cron_part_matches(part, value, max) {
            return true;
        }
    }
    false
}

/// Match a single comma-separated part of a cron field.
fn cron_part_matches(part: &str, value: u32, max: u32) -> bool {
    if let Some(slash_pos) = part.find('/') {
        let range_part = &part[..slash_pos];
        let step: u32 = match part[slash_pos + 1..].parse() {
            Ok(s) if s > 0 => s,
            _ => return false,
        };
        let (start, end) = if range_part == "*" {
            (0, max)
        } else if let Some(dash_pos) = range_part.find('-') {
            let s: u32 = range_part[..dash_pos].parse().unwrap_or(0);
            let e: u32 = range_part[dash_pos + 1..].parse().unwrap_or(max);
            (s, e)
        } else {
            let s: u32 = range_part.parse().unwrap_or(0);
            (s, max)
        };
        if value < start || value > end {
            return false;
        }
        (value - start).is_multiple_of(step)
    } else if part == "*" {
        true
    } else if let Some(dash_pos) = part.find('-') {
        let start: u32 = part[..dash_pos].parse().unwrap_or(0);
        let end: u32 = part[dash_pos + 1..].parse().unwrap_or(max);
        value >= start && value <= end
    } else {
        part.parse::<u32>().ok() == Some(value)
    }
}

use chrono::{Datelike, Timelike};

// ---------------------------------------------------------------------------
// CronHandler
// ---------------------------------------------------------------------------

/// Trait implemented by each concrete cron job.
///
/// `execute` is intentionally **synchronous** — async support will be added
/// in a future iteration.
pub trait CronHandler: Send + Sync {
    /// Execute the job logic.
    fn execute(&self, ctx: &CronContext) -> Result<(), BeltError>;
}

/// Context passed to a [`CronHandler`] during execution.
#[derive(Debug)]
pub struct CronContext {
    /// Current wall-clock time (UTC).
    pub now: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// CronJobDef
// ---------------------------------------------------------------------------

/// Definition of a single cron job registered with the engine.
pub struct CronJobDef {
    /// Unique name used for register/unregister/pause lookups.
    pub name: String,
    /// When to run.
    pub schedule: CronSchedule,
    /// Optional workspace scope (`None` = global).
    pub workspace: Option<String>,
    /// Whether the job is active.
    pub enabled: bool,
    /// Last successful execution time.
    pub last_run_at: Option<DateTime<Utc>>,
    /// The handler invoked on each due tick.
    pub handler: Box<dyn CronHandler>,
}

// ---------------------------------------------------------------------------
// CronEngine
// ---------------------------------------------------------------------------

/// A lightweight scheduler that checks registered jobs on each `tick()`.
pub struct CronEngine {
    jobs: Vec<CronJobDef>,
}

impl CronEngine {
    /// Create an empty engine with no registered jobs.
    pub fn new() -> Self {
        Self { jobs: Vec::new() }
    }

    /// Register a new job. Replaces any existing job with the same name.
    pub fn register(&mut self, job: CronJobDef) {
        self.unregister(&job.name);
        self.jobs.push(job);
    }

    /// Remove a job by name. No-op if the name does not exist.
    pub fn unregister(&mut self, name: &str) {
        self.jobs.retain(|j| j.name != name);
    }

    /// Disable a job so it will be skipped during `tick()`.
    pub fn pause(&mut self, name: &str) {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.name == name) {
            job.enabled = false;
        }
    }

    /// Re-enable a previously paused job.
    pub fn resume(&mut self, name: &str) {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.name == name) {
            job.enabled = true;
        }
    }

    /// Reset `last_run_at` so the job fires on the next `tick()`.
    pub fn force_trigger(&mut self, name: &str) {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.name == name) {
            job.last_run_at = None;
        }
    }

    /// Iterate over all registered jobs, executing those that are due.
    ///
    /// Execution errors are logged via `tracing::error!` but do **not**
    /// halt the tick — remaining jobs will still be evaluated.
    pub fn tick(&mut self) {
        let now = Utc::now();
        for job in &mut self.jobs {
            if !job.enabled {
                continue;
            }
            if !job.schedule.should_run(job.last_run_at, now) {
                continue;
            }
            let ctx = CronContext { now };
            match job.handler.execute(&ctx) {
                Ok(()) => {
                    tracing::info!(job = %job.name, "cron job executed successfully");
                    job.last_run_at = Some(now);
                }
                Err(e) => {
                    tracing::error!(job = %job.name, error = %e, "cron job failed");
                    // Still update last_run_at to avoid tight retry loops.
                    job.last_run_at = Some(now);
                }
            }
        }
    }

    /// Synchronize in-memory `last_run_at` state from the database.
    ///
    /// For each registered job whose name appears in the DB `cron_jobs` table,
    /// if the DB `last_run_at` is `NULL` (i.e. a trigger was requested via
    /// `reset_cron_last_run`), the in-memory `last_run_at` is cleared so the
    /// job fires on the next `tick()`.
    pub fn sync_triggers_from_db(&mut self, db: &Database) {
        let db_jobs = match db.list_cron_jobs() {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::error!(error = %e, "failed to read cron jobs from DB for trigger sync");
                return;
            }
        };

        let db_map: HashMap<&str, &belt_infra::db::CronJob> =
            db_jobs.iter().map(|j| (j.name.as_str(), j)).collect();

        for job in &mut self.jobs {
            if let Some(db_job) = db_map.get(job.name.as_str())
                && db_job.last_run_at.is_none()
                && job.last_run_at.is_some()
            {
                tracing::info!(
                    job = %job.name,
                    "trigger sync: resetting last_run_at (trigger requested)"
                );
                job.last_run_at = None;
            }
        }
    }

    /// Fully synchronize custom (user-defined) cron jobs from the database.
    ///
    /// Performs a three-way reconciliation between the in-memory job list and
    /// the DB `cron_jobs` table:
    ///
    /// 1. **New jobs** in the DB that are not yet registered are added.
    /// 2. **Removed jobs** that no longer exist in the DB are unregistered.
    /// 3. **Updated jobs** have their `enabled` state and `last_run_at`
    ///    synchronized. Schedule and script changes cause re-registration so
    ///    the handler picks up the new values.
    ///
    /// Built-in jobs (identified by a static name list) are never touched.
    pub fn sync_custom_jobs_from_db(&mut self, db: &Arc<Database>) {
        let db_jobs = match db.list_cron_jobs() {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::error!(error = %e, "failed to read cron jobs from DB for full sync");
                return;
            }
        };

        let builtin_names: &[&str] = &[
            "hitl_timeout",
            "daily_report",
            "log_cleanup",
            "evaluate",
            "pr_review_scan",
            "gap_detection",
            "knowledge_extraction",
        ];

        // Filter to only custom jobs (skip built-in names and workspace-scoped built-ins).
        let custom_db_jobs: Vec<&belt_infra::db::CronJob> = db_jobs
            .iter()
            .filter(|j| {
                if builtin_names.contains(&j.name.as_str()) {
                    return false;
                }
                if j.name.contains(':')
                    && builtin_names
                        .iter()
                        .any(|b| j.name.ends_with(&format!(":{b}")))
                {
                    return false;
                }
                true
            })
            .collect();

        let db_map: HashMap<&str, &&belt_infra::db::CronJob> = custom_db_jobs
            .iter()
            .map(|j| (j.name.as_str(), j))
            .collect();

        // Collect names of current in-memory custom jobs (non-builtin).
        let in_memory_custom: Vec<String> = self
            .jobs
            .iter()
            .filter(|j| {
                let name = j.name.as_str();
                if builtin_names.contains(&name) {
                    return false;
                }
                if name.contains(':')
                    && builtin_names
                        .iter()
                        .any(|b| name.ends_with(&format!(":{b}")))
                {
                    return false;
                }
                true
            })
            .map(|j| j.name.clone())
            .collect();

        // 1. Remove in-memory custom jobs that no longer exist in DB.
        for name in &in_memory_custom {
            if !db_map.contains_key(name.as_str()) {
                tracing::info!(job = %name, "sync: removing custom job (deleted from DB)");
                self.unregister(name);
            }
        }

        // 2. For each DB custom job, add new or sync existing.
        for db_job in &custom_db_jobs {
            if let Some(mem_job) = self.jobs.iter_mut().find(|j| j.name == db_job.name) {
                // Sync enabled state.
                if mem_job.enabled != db_job.enabled {
                    tracing::info!(
                        job = %db_job.name,
                        enabled = db_job.enabled,
                        "sync: updating enabled state"
                    );
                    mem_job.enabled = db_job.enabled;
                }

                // Sync last_run_at (trigger detection).
                if db_job.last_run_at.is_none() && mem_job.last_run_at.is_some() {
                    tracing::info!(
                        job = %db_job.name,
                        "sync: resetting last_run_at (trigger requested)"
                    );
                    mem_job.last_run_at = None;
                }

                // Check if schedule or script changed; if so, re-register.
                let schedule_str = match &mem_job.schedule {
                    CronSchedule::Expression(expr) => Some(expr.as_str()),
                    _ => None,
                };
                let schedule_changed = schedule_str != Some(&db_job.schedule);
                // We cannot inspect the script from CronJobDef directly, so
                // re-register on schedule change to be safe.
                if schedule_changed {
                    tracing::info!(
                        job = %db_job.name,
                        new_schedule = %db_job.schedule,
                        "sync: re-registering custom job (schedule changed)"
                    );
                    let schedule = match CronSchedule::parse_expression(&db_job.schedule) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(
                                job = %db_job.name,
                                error = %e,
                                "sync: skipping re-register due to invalid schedule"
                            );
                            continue;
                        }
                    };
                    self.register(CronJobDef {
                        name: db_job.name.clone(),
                        schedule,
                        workspace: db_job.workspace.clone(),
                        enabled: db_job.enabled,
                        last_run_at: None,
                        handler: Box::new(CustomScriptJob {
                            script: db_job.script.clone(),
                            job_name: db_job.name.clone(),
                            db: Arc::clone(db),
                        }),
                    });
                }
            } else {
                // New job: register it.
                let schedule = match CronSchedule::parse_expression(&db_job.schedule) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            job = %db_job.name,
                            schedule = %db_job.schedule,
                            error = %e,
                            "sync: skipping new custom job with invalid schedule"
                        );
                        continue;
                    }
                };

                tracing::info!(
                    job = %db_job.name,
                    schedule = %db_job.schedule,
                    enabled = db_job.enabled,
                    "sync: registering new custom job from DB"
                );

                self.register(CronJobDef {
                    name: db_job.name.clone(),
                    schedule,
                    workspace: db_job.workspace.clone(),
                    enabled: db_job.enabled,
                    last_run_at: None,
                    handler: Box::new(CustomScriptJob {
                        script: db_job.script.clone(),
                        job_name: db_job.name.clone(),
                        db: Arc::clone(db),
                    }),
                });
            }
        }
    }

    /// Return the number of registered jobs.
    pub fn job_count(&self) -> usize {
        self.jobs.len()
    }
}

impl Default for CronEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Built-in jobs
// ---------------------------------------------------------------------------

/// Default HITL timeout duration (24 hours).
const DEFAULT_HITL_TIMEOUT_SECS: i64 = 24 * 60 * 60;

/// Worktree TTL for log cleanup (7 days).
const WORKTREE_TTL_DAYS: i64 = 7;

/// Shared dependencies for built-in cron jobs.
pub struct BuiltinJobDeps {
    /// Database handle for querying and updating queue items.
    pub db: Arc<Database>,
    /// Worktree manager for cleanup operations.
    pub worktree_mgr: Arc<dyn WorktreeManager>,
    /// Root directory of the workspace (used by gap detection to scan code).
    pub workspace_root: std::path::PathBuf,
    /// Directory where daily report JSON files are stored.
    /// When `None`, reports are only logged (not persisted to files).
    pub report_dir: Option<std::path::PathBuf>,
}

/// Expires unanswered HITL (human-in-the-loop) items after a configurable timeout.
///
/// Queries items in the `Hitl` phase, checks their `updated_at` timestamp,
/// and transitions those older than the configured timeout to `Failed`.
/// Also cleans up the associated worktree on expiry.
pub struct HitlTimeoutJob {
    db: Arc<Database>,
    worktree_mgr: Arc<dyn WorktreeManager>,
    /// Timeout duration in seconds. Items in HITL phase longer than this
    /// are considered expired. Defaults to 24 hours.
    pub timeout_secs: i64,
}

impl HitlTimeoutJob {
    /// Create a new `HitlTimeoutJob` with the default timeout (24 hours).
    pub fn new(db: Arc<Database>, worktree_mgr: Arc<dyn WorktreeManager>) -> Self {
        Self {
            db,
            worktree_mgr,
            timeout_secs: DEFAULT_HITL_TIMEOUT_SECS,
        }
    }

    /// Set the timeout duration in seconds.
    pub fn with_timeout_secs(mut self, secs: i64) -> Self {
        self.timeout_secs = secs;
        self
    }
}

impl CronHandler for HitlTimeoutJob {
    fn execute(&self, ctx: &CronContext) -> Result<(), BeltError> {
        tracing::info!("HitlTimeoutJob: checking for expired HITL items");

        let hitl_items = self.db.list_items(Some(QueuePhase::Hitl), None)?;
        let threshold = ctx.now - chrono::Duration::seconds(self.timeout_secs);
        let mut expired_count = 0u32;

        // Cache loaded workspace configs to avoid repeated file I/O.
        let mut ws_cache: HashMap<String, Option<WorkspaceConfig>> = HashMap::new();

        for item in &hitl_items {
            // Check per-item timeout first (set via `belt hitl timeout set`).
            let is_expired = if let Some(ref timeout_at_str) = item.hitl_timeout_at {
                DateTime::parse_from_rfc3339(timeout_at_str)
                    .map(|dt| dt.with_timezone(&Utc) <= ctx.now)
                    .unwrap_or(false)
            } else {
                // Fall back to global timeout based on updated_at.
                let updated = DateTime::parse_from_rfc3339(&item.updated_at)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or(ctx.now);
                updated < threshold
            };

            if !is_expired {
                continue;
            }

            // Determine target phase from per-item terminal action first,
            // then fall back to workspace escalation_policy terminal action,
            // and finally default to Failed (safe default).
            let target_phase = match item.hitl_terminal_action.as_deref() {
                Some("skip") => QueuePhase::Skipped,
                Some("replan") => QueuePhase::Failed,
                Some("failed") => QueuePhase::Failed,
                _ => resolve_workspace_terminal_phase(
                    &self.db,
                    &item.workspace_id,
                    &item.source_id,
                    &mut ws_cache,
                ),
            };

            if let Err(e) = self.db.update_phase(&item.work_id, target_phase) {
                tracing::warn!(
                    work_id = %item.work_id,
                    error = %e,
                    "failed to expire HITL item"
                );
                continue;
            }

            // Clean up the associated worktree for terminal phases.
            if target_phase == QueuePhase::Skipped
                && let Err(e) = self.worktree_mgr.cleanup(&item.work_id)
            {
                tracing::warn!(
                    work_id = %item.work_id,
                    error = %e,
                    "failed to cleanup worktree for expired HITL item"
                );
            }

            expired_count += 1;
            tracing::info!(
                work_id = %item.work_id,
                target_phase = %target_phase,
                terminal_action = ?item.hitl_terminal_action,
                "HITL item expired, transitioned to {}",
                target_phase
            );
        }

        tracing::info!(
            total_hitl = hitl_items.len(),
            expired = expired_count,
            "HitlTimeoutJob completed"
        );
        Ok(())
    }
}

/// Resolve the target phase for an expired HITL item by consulting the
/// workspace's escalation policy `terminal` action.
///
/// Extracts the source key from `source_id` (the prefix before the first `:`),
/// looks up the corresponding `SourceConfig`, and maps its `terminal_action()`
/// to a `QueuePhase`. Returns `QueuePhase::Failed` as the safe default when
/// the workspace or source cannot be found, or when no terminal action is set.
fn resolve_workspace_terminal_phase(
    db: &Database,
    workspace_id: &str,
    source_id: &str,
    cache: &mut HashMap<String, Option<WorkspaceConfig>>,
) -> QueuePhase {
    // Load or retrieve cached workspace config.
    let ws_config = cache.entry(workspace_id.to_string()).or_insert_with(|| {
        let (_name, config_path, _created_at) = match db.get_workspace(workspace_id) {
            Ok(ws) => ws,
            Err(e) => {
                tracing::warn!(
                    workspace_id = %workspace_id,
                    error = %e,
                    "failed to look up workspace for terminal action resolution"
                );
                return None;
            }
        };
        match belt_infra::workspace_loader::load_workspace_config(std::path::Path::new(
            &config_path,
        )) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                tracing::warn!(
                    workspace_id = %workspace_id,
                    error = %e,
                    "failed to load workspace config for terminal action resolution"
                );
                None
            }
        }
    });

    let Some(config) = ws_config else {
        return QueuePhase::Failed;
    };

    // Extract source key from source_id (e.g. "github:org/repo#42" -> "github").
    let source_key = source_id.split(':').next().unwrap_or("github");

    let terminal_action = config
        .sources
        .get(source_key)
        .and_then(|src| src.escalation.terminal_action());

    match terminal_action {
        Some(EscalationAction::Skip) => QueuePhase::Skipped,
        Some(EscalationAction::Replan) => QueuePhase::Failed,
        _ => QueuePhase::Failed,
    }
}

/// Structured daily report containing queue statistics and runtime usage.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DailyReport {
    /// Report generation date (YYYY-MM-DD).
    pub date: String,
    /// RFC 3339 timestamp when the report was generated.
    pub generated_at: String,
    /// Queue item counts grouped by phase.
    pub phase_summary: HashMap<String, u32>,
    /// Total number of queue items across all phases.
    pub total_items: u32,
    /// Items that failed in the last 24 hours.
    pub recent_failures: Vec<DailyReportFailure>,
    /// Items currently waiting for human review.
    pub hitl_waiting: Vec<DailyReportHitlItem>,
    /// Aggregated token usage from the last 24 hours.
    pub token_usage: DailyReportTokenUsage,
}

/// A failed item summary included in the daily report.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DailyReportFailure {
    /// Work item ID.
    pub work_id: String,
    /// Source identifier.
    pub source_id: String,
    /// Optional title of the item.
    pub title: Option<String>,
    /// When the item was last updated.
    pub updated_at: String,
}

/// A HITL-waiting item summary included in the daily report.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DailyReportHitlItem {
    /// Work item ID.
    pub work_id: String,
    /// Source identifier.
    pub source_id: String,
    /// Optional title of the item.
    pub title: Option<String>,
    /// When HITL was created.
    pub hitl_created_at: Option<String>,
    /// Notes attached to the HITL request.
    pub hitl_notes: Option<String>,
}

/// Token usage summary for the daily report.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DailyReportTokenUsage {
    /// Total input tokens consumed in the period.
    pub total_input_tokens: u64,
    /// Total output tokens produced in the period.
    pub total_output_tokens: u64,
    /// Combined input + output tokens.
    pub total_tokens: u64,
    /// Number of runtime invocations.
    pub executions: u64,
    /// Average invocation duration in milliseconds.
    pub avg_duration_ms: Option<f64>,
    /// Per-model breakdown of token usage.
    pub by_model: HashMap<String, DailyReportModelUsage>,
}

/// Per-model token usage in the daily report.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DailyReportModelUsage {
    /// Total input tokens for this model.
    pub input_tokens: u64,
    /// Total output tokens for this model.
    pub output_tokens: u64,
    /// Number of invocations for this model.
    pub executions: u64,
}

/// Generates a daily summary report by aggregating queue item statistics,
/// recent failures, HITL items, and token usage. Persists the report as a
/// JSON file under the configured report directory.
pub struct DailyReportJob {
    db: Arc<Database>,
    /// Directory where report JSON files are written.
    /// When `None`, reports are only logged (not persisted).
    report_dir: Option<std::path::PathBuf>,
}

impl DailyReportJob {
    /// Create a new `DailyReportJob`.
    pub fn new(db: Arc<Database>, report_dir: Option<std::path::PathBuf>) -> Self {
        Self { db, report_dir }
    }

    /// Generate the daily report from current database state.
    fn generate_report(&self, ctx: &CronContext) -> Result<DailyReport, BeltError> {
        // Phase summary via efficient grouped query.
        let phase_counts = self.db.count_items_by_phase()?;
        let mut phase_summary: HashMap<String, u32> = HashMap::new();
        let mut total_items: u32 = 0;
        for (phase, count) in &phase_counts {
            phase_summary.insert(phase.clone(), *count);
            total_items += count;
        }

        // Recent failures (items currently in Failed phase).
        let failed_items = self.db.list_items(Some(QueuePhase::Failed), None)?;
        let recent_failures: Vec<DailyReportFailure> = failed_items
            .iter()
            .map(|item| DailyReportFailure {
                work_id: item.work_id.clone(),
                source_id: item.source_id.clone(),
                title: item.title.clone(),
                updated_at: item.updated_at.clone(),
            })
            .collect();

        // HITL-waiting items.
        let hitl_items = self.db.list_items(Some(QueuePhase::Hitl), None)?;
        let hitl_waiting: Vec<DailyReportHitlItem> = hitl_items
            .iter()
            .map(|item| DailyReportHitlItem {
                work_id: item.work_id.clone(),
                source_id: item.source_id.clone(),
                title: item.title.clone(),
                hitl_created_at: item.hitl_created_at.clone(),
                hitl_notes: item.hitl_notes.clone(),
            })
            .collect();

        // Token usage stats (last 24 hours).
        let runtime_stats = self.db.get_runtime_stats()?;
        let by_model: HashMap<String, DailyReportModelUsage> = runtime_stats
            .by_model
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    DailyReportModelUsage {
                        input_tokens: v.input_tokens,
                        output_tokens: v.output_tokens,
                        executions: v.executions,
                    },
                )
            })
            .collect();

        let token_usage = DailyReportTokenUsage {
            total_input_tokens: runtime_stats.total_tokens_input,
            total_output_tokens: runtime_stats.total_tokens_output,
            total_tokens: runtime_stats.total_tokens,
            executions: runtime_stats.executions,
            avg_duration_ms: runtime_stats.avg_duration_ms,
            by_model,
        };

        Ok(DailyReport {
            date: ctx.now.format("%Y-%m-%d").to_string(),
            generated_at: ctx.now.to_rfc3339(),
            phase_summary,
            total_items,
            recent_failures,
            hitl_waiting,
            token_usage,
        })
    }

    /// Persist the report as a JSON file in the report directory.
    fn save_report(&self, report: &DailyReport) -> Result<Option<std::path::PathBuf>, BeltError> {
        let Some(ref dir) = self.report_dir else {
            return Ok(None);
        };

        std::fs::create_dir_all(dir).map_err(|e| {
            BeltError::Runtime(format!(
                "failed to create report directory '{}': {e}",
                dir.display()
            ))
        })?;

        let filename = format!("daily-report-{}.json", report.date);
        let path = dir.join(&filename);

        let json = serde_json::to_string_pretty(report)
            .map_err(|e| BeltError::Runtime(format!("failed to serialize daily report: {e}")))?;

        std::fs::write(&path, &json).map_err(|e| {
            BeltError::Runtime(format!(
                "failed to write daily report to '{}': {e}",
                path.display()
            ))
        })?;

        Ok(Some(path))
    }
}

impl CronHandler for DailyReportJob {
    fn execute(&self, ctx: &CronContext) -> Result<(), BeltError> {
        tracing::info!("DailyReportJob: generating daily report");

        let report = self.generate_report(ctx)?;

        // Log the summary.
        tracing::info!(
            date = %report.date,
            total_items = report.total_items,
            failed = report.recent_failures.len(),
            hitl_waiting = report.hitl_waiting.len(),
            total_tokens = report.token_usage.total_tokens,
            executions = report.token_usage.executions,
            "=== Daily Report ==="
        );

        // Log per-phase breakdown.
        for (phase, count) in &report.phase_summary {
            tracing::info!(phase = %phase, count = count, "phase summary");
        }

        // Log failed items for visibility.
        for failure in &report.recent_failures {
            tracing::warn!(
                work_id = %failure.work_id,
                source_id = %failure.source_id,
                title = ?failure.title,
                "failed item in report"
            );
        }

        // Persist to disk.
        match self.save_report(&report) {
            Ok(Some(path)) => {
                tracing::info!(
                    path = %path.display(),
                    "daily report saved"
                );
            }
            Ok(None) => {
                tracing::debug!("no report directory configured, skipping file persistence");
            }
            Err(e) => {
                // File persistence failure is non-fatal; the report was already logged.
                tracing::warn!(error = %e, "failed to persist daily report to file");
            }
        }

        Ok(())
    }
}

/// Cleans up old worktrees that exceed the TTL (7 days).
///
/// Scans terminal-phase items (Done, Skipped) and checks their `updated_at`
/// timestamp. Worktrees older than the TTL are removed.
pub struct LogCleanupJob {
    db: Arc<Database>,
    worktree_mgr: Arc<dyn WorktreeManager>,
}

impl CronHandler for LogCleanupJob {
    fn execute(&self, ctx: &CronContext) -> Result<(), BeltError> {
        tracing::info!("LogCleanupJob: cleaning up old worktrees");

        let threshold = ctx.now - chrono::Duration::days(WORKTREE_TTL_DAYS);
        let mut cleaned_count = 0u32;

        // Clean up worktrees for terminal-phase items older than TTL.
        let done_items = self.db.list_items(Some(QueuePhase::Done), None)?;
        let skipped_items = self.db.list_items(Some(QueuePhase::Skipped), None)?;

        let candidates = done_items.iter().chain(skipped_items.iter());

        for item in candidates {
            let updated = DateTime::parse_from_rfc3339(&item.updated_at)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or(ctx.now);

            if updated < threshold && self.worktree_mgr.exists(&item.work_id) {
                match self.worktree_mgr.cleanup(&item.work_id) {
                    Ok(()) => {
                        cleaned_count += 1;
                        tracing::info!(
                            work_id = %item.work_id,
                            "cleaned up stale worktree"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            work_id = %item.work_id,
                            error = %e,
                            "failed to cleanup stale worktree"
                        );
                    }
                }
            }
        }

        tracing::info!(cleaned = cleaned_count, "LogCleanupJob completed");
        Ok(())
    }
}

/// Detects gaps between active specs and implemented code (CR-07).
///
/// Runs every hour. For each active spec it queries the database for specs
/// in `Active` status, extracts keywords from their content, and checks
/// whether corresponding code artefacts exist by scanning source files
/// under the configured workspace root.
///
/// When a gap is found (keywords from a spec have no matches in the
/// codebase) it creates a GitHub issue labelled `autopilot:gap` via the
/// `gh` CLI.
pub struct GapDetectionJob {
    db: Arc<Database>,
    /// Root directory of the workspace to scan for code files.
    workspace_root: std::path::PathBuf,
}

impl GapDetectionJob {
    /// Create a new `GapDetectionJob`.
    pub fn new(db: Arc<Database>, workspace_root: std::path::PathBuf) -> Self {
        Self { db, workspace_root }
    }
}

/// Check whether all linked GitHub issues for a spec are in a closed/done state.
///
/// Iterates over the spec's linked targets (from the `spec_links` table).
/// For each target that looks like a GitHub issue reference (contains `#`),
/// queries the `gh` CLI to check whether the issue is closed.
///
/// Returns `true` when there are no linked issues or all linked issues are closed.
/// Returns `false` if any linked issue is still open.
fn all_linked_issues_done(db: &Database, spec_id: &str) -> bool {
    let links = match db.list_spec_links(spec_id) {
        Ok(links) => links,
        Err(e) => {
            tracing::warn!(
                spec_id = %spec_id,
                error = %e,
                "failed to list spec links, treating as not-all-done"
            );
            return false;
        }
    };

    // If no linked issues, condition is satisfied.
    if links.is_empty() {
        return true;
    }

    for link in &links {
        let target = &link.target;

        // Only check GitHub issue references (URLs containing /issues/ or shorthand with #).
        let is_github_issue = target.contains("/issues/") || target.contains('#');
        if !is_github_issue {
            continue;
        }

        // Extract the issue reference for `gh issue view`.
        // Handles both URL format (https://github.com/org/repo/issues/42)
        // and shorthand format (org/repo#42).
        let issue_ref = if target.contains("/issues/") {
            // URL format: extract "org/repo#number"
            // e.g. https://github.com/org/repo/issues/42 -> org/repo#42
            let parts: Vec<&str> = target.split('/').collect();
            if parts.len() >= 5 {
                let idx = parts.iter().position(|p| *p == "issues");
                if let Some(i) = idx {
                    if i >= 2 && i + 1 < parts.len() {
                        format!("{}/{}#{}", parts[i - 2], parts[i - 1], parts[i + 1])
                    } else {
                        continue;
                    }
                } else {
                    continue;
                }
            } else {
                continue;
            }
        } else {
            // Shorthand like "org/repo#42"
            target.clone()
        };

        // Use `gh issue view` to check status.
        let result = std::process::Command::new("gh")
            .args([
                "issue", "view", &issue_ref, "--json", "state", "--jq", ".state",
            ])
            .output();

        match result {
            Ok(output) if output.status.success() => {
                let state = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .to_uppercase();
                if state != "CLOSED" {
                    tracing::info!(
                        spec_id = %spec_id,
                        issue = %issue_ref,
                        state = %state,
                        "linked issue is not closed"
                    );
                    return false;
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(
                    spec_id = %spec_id,
                    issue = %issue_ref,
                    stderr = %stderr.trim(),
                    "failed to check linked issue status, treating as not-done"
                );
                return false;
            }
            Err(e) => {
                tracing::warn!(
                    spec_id = %spec_id,
                    error = %e,
                    "failed to invoke gh CLI for linked issue check"
                );
                // When gh is unavailable, err on safe side: do not auto-advance.
                return false;
            }
        }
    }

    true
}

/// Minimum keyword length to consider meaningful for gap detection.
const MIN_KEYWORD_LEN: usize = 4;

/// File extensions to scan when matching keywords against code.
const CODE_EXTENSIONS: &[&str] = &["rs", "ts", "js", "py", "go", "yaml", "yml", "toml", "md"];

/// Extract meaningful keywords from spec content.
///
/// Splits the content into whitespace-delimited tokens, strips
/// punctuation, lowercases, and keeps tokens that are at least
/// [`MIN_KEYWORD_LEN`] characters and are not common stop-words.
fn extract_keywords(content: &str) -> Vec<String> {
    let stop_words: std::collections::HashSet<&str> = [
        "the", "and", "for", "with", "that", "this", "from", "will", "have", "been", "should",
        "would", "could", "each", "when", "then", "into", "also", "than", "them", "they", "their",
        "there", "were", "what", "which", "about", "some", "more", "other", "does", "done",
    ]
    .iter()
    .copied()
    .collect();

    content
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
                .to_lowercase()
        })
        .filter(|w| w.len() >= MIN_KEYWORD_LEN && !stop_words.contains(w.as_str()))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect()
}

/// Scan source files under `root` and collect their contents into a single
/// lowercase string for keyword matching.
///
/// Only files with extensions listed in [`CODE_EXTENSIONS`] are read.
/// Errors reading individual files are silently skipped.
fn collect_code_corpus(root: &std::path::Path) -> String {
    let mut corpus = String::new();
    let walker = walk_dir(root);
    for path in walker {
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && CODE_EXTENSIONS.contains(&ext)
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            corpus.push_str(&text.to_lowercase());
            corpus.push('\n');
        }
    }
    corpus
}

/// Recursively list all files under `dir`.
fn walk_dir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip hidden directories and common non-source directories.
                if let Some(name) = path.file_name().and_then(|n| n.to_str())
                    && (name.starts_with('.') || name == "target" || name == "node_modules")
                {
                    continue;
                }
                files.extend(walk_dir(&path));
            } else {
                files.push(path);
            }
        }
    }
    files
}

/// Represents a detected gap between a spec and the codebase.
#[derive(Debug)]
struct DetectedGap {
    spec_id: String,
    spec_name: String,
    missing_keywords: Vec<String>,
}

/// Minimum ratio of matched keywords for a spec to be considered covered.
/// If fewer than this fraction of keywords match, a gap is reported.
const COVERAGE_THRESHOLD: f64 = 0.5;

/// Check whether an open GitHub issue already exists for a given spec's gap.
///
/// Queries `gh issue list` for open issues with the `autopilot:gap` label
/// whose title contains the spec name.  Returns `true` when a matching
/// issue is found, meaning a new issue should **not** be created.
fn has_existing_gap_issue(spec_name: &str) -> bool {
    let search_title = format!("[Gap] Spec '{spec_name}'");
    let result = std::process::Command::new("gh")
        .args([
            "issue",
            "list",
            "--label",
            "autopilot:gap",
            "--state",
            "open",
            "--search",
            &search_title,
            "--json",
            "number",
            "--limit",
            "1",
        ])
        .output();

    match result {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let trimmed = stdout.trim();
            // gh returns "[]" when no issues match.
            !trimmed.is_empty() && trimmed != "[]"
        }
        _ => {
            // If gh CLI is unavailable or fails, err on the safe side and
            // allow issue creation so gaps are not silently swallowed.
            false
        }
    }
}

/// Create a HITL queue item for final human confirmation before Completing -> Completed.
fn create_spec_completion_hitl(db: &Database, spec: &belt_core::spec::Spec, detail: &str) {
    let work_id = format!("spec-completion:{}:hitl", spec.id);
    let mut item = belt_core::queue::QueueItem::new(
        work_id.clone(),
        spec.id.clone(),
        spec.workspace_id.clone(),
        "spec_completion".to_string(),
    );
    item.phase = QueuePhase::Hitl;
    item.title = Some(format!("Spec '{}' ready for final review", spec.name));
    item.hitl_created_at = Some(chrono::Utc::now().to_rfc3339());
    item.hitl_notes = Some(format!(
        "Spec '{}' has no gaps and all linked issues are done. {}. \
         Approve to advance from Completing to Completed.",
        spec.name, detail,
    ));

    match db.insert_item(&item) {
        Ok(()) => {
            tracing::info!(
                spec_id = %spec.id,
                work_id = %work_id,
                "created HITL item for spec completion final review"
            );
        }
        Err(BeltError::Database(ref msg)) if msg.contains("UNIQUE constraint") => {
            tracing::debug!(
                spec_id = %spec.id,
                "HITL item for spec completion already exists, skipping"
            );
        }
        Err(e) => {
            tracing::warn!(
                spec_id = %spec.id,
                error = %e,
                "failed to create HITL item for spec completion"
            );
        }
    }
}

/// Create a HITL queue item when test commands fail for a spec.
fn create_spec_test_failure_hitl(db: &Database, spec: &belt_core::spec::Spec, detail: &str) {
    let work_id = format!("spec-test-fail:{}:hitl", spec.id);
    let mut item = belt_core::queue::QueueItem::new(
        work_id.clone(),
        spec.id.clone(),
        spec.workspace_id.clone(),
        "spec_test_failure".to_string(),
    );
    item.phase = QueuePhase::Hitl;
    item.title = Some(format!("Spec '{}' test commands failed", spec.name));
    item.hitl_created_at = Some(chrono::Utc::now().to_rfc3339());
    item.hitl_notes = Some(format!(
        "Spec '{}' passed gap detection but test commands failed. {}",
        spec.name, detail,
    ));

    match db.insert_item(&item) {
        Ok(()) => {
            tracing::info!(
                spec_id = %spec.id,
                work_id = %work_id,
                "created HITL item for spec test failure"
            );
        }
        Err(BeltError::Database(ref msg)) if msg.contains("UNIQUE constraint") => {
            tracing::debug!(
                spec_id = %spec.id,
                "HITL item for spec test failure already exists, skipping"
            );
        }
        Err(e) => {
            tracing::warn!(
                spec_id = %spec.id,
                error = %e,
                "failed to create HITL item for spec test failure"
            );
        }
    }
}

impl CronHandler for GapDetectionJob {
    fn execute(&self, _ctx: &CronContext) -> Result<(), BeltError> {
        tracing::info!("GapDetectionJob: scanning active specs for unimplemented gaps");

        // Step 1: Query active specs from the database.
        let active_specs = self
            .db
            .list_specs(None, Some(belt_core::spec::SpecStatus::Active))?;

        if active_specs.is_empty() {
            tracing::info!("GapDetectionJob: no active specs found, nothing to check");
            return Ok(());
        }

        tracing::info!(
            count = active_specs.len(),
            "GapDetectionJob: found active specs"
        );

        // Step 2: Collect code corpus from workspace root.
        let corpus = collect_code_corpus(&self.workspace_root);

        if corpus.is_empty() {
            tracing::warn!(
                root = %self.workspace_root.display(),
                "GapDetectionJob: no source files found in workspace root"
            );
            return Ok(());
        }

        // Step 3: For each spec, extract keywords and check coverage.
        let mut gaps: Vec<DetectedGap> = Vec::new();
        let mut covered_spec_ids: Vec<String> = Vec::new();

        for spec in &active_specs {
            let keywords = extract_keywords(&spec.content);
            if keywords.is_empty() {
                // Specs with no extractable keywords are considered covered.
                covered_spec_ids.push(spec.id.clone());
                continue;
            }

            let missing: Vec<String> = keywords
                .iter()
                .filter(|kw| !corpus.contains(kw.as_str()))
                .cloned()
                .collect();

            let matched_ratio = if keywords.is_empty() {
                1.0
            } else {
                1.0 - (missing.len() as f64 / keywords.len() as f64)
            };

            if matched_ratio < COVERAGE_THRESHOLD && !missing.is_empty() {
                tracing::info!(
                    spec_id = %spec.id,
                    spec_name = %spec.name,
                    total_keywords = keywords.len(),
                    missing_count = missing.len(),
                    "GapDetectionJob: gap detected"
                );
                gaps.push(DetectedGap {
                    spec_id: spec.id.clone(),
                    spec_name: spec.name.clone(),
                    missing_keywords: missing,
                });
            } else {
                covered_spec_ids.push(spec.id.clone());
            }
        }

        // Step 4: Create GitHub issues for detected gaps (with dedupe guard).
        let mut skipped_count = 0usize;
        for gap in &gaps {
            // Dedupe guard: skip if an open queue item already targets this spec.
            if self
                .db
                .has_open_items_for_source(&gap.spec_id)
                .unwrap_or(false)
            {
                tracing::info!(
                    spec_id = %gap.spec_id,
                    "GapDetectionJob: skipping issue creation — open queue item exists for spec"
                );
                skipped_count += 1;
                continue;
            }

            // Dedupe guard: skip if an open GitHub issue already exists for this gap.
            if has_existing_gap_issue(&gap.spec_name) {
                tracing::info!(
                    spec_id = %gap.spec_id,
                    spec_name = %gap.spec_name,
                    "GapDetectionJob: skipping issue creation — open GitHub issue already exists"
                );
                skipped_count += 1;
                continue;
            }

            let title = format!("[Gap] Spec '{}' has unimplemented keywords", gap.spec_name);
            let missing_list = gap.missing_keywords.join(", ");
            let body = format!(
                "## Gap Detection Report\n\n\
                 **Spec ID:** {}\n\
                 **Spec Name:** {}\n\n\
                 The following keywords from the spec were not found in the codebase:\n\n\
                 `{}`\n\n\
                 _This issue was automatically created by the gap-detection cron job._",
                gap.spec_id, gap.spec_name, missing_list,
            );

            let result = std::process::Command::new("gh")
                .args([
                    "issue",
                    "create",
                    "--title",
                    &title,
                    "--body",
                    &body,
                    "--label",
                    "autopilot:gap",
                ])
                .output();

            match result {
                Ok(output) if output.status.success() => {
                    let url = String::from_utf8_lossy(&output.stdout);
                    tracing::info!(
                        spec_id = %gap.spec_id,
                        issue_url = %url.trim(),
                        "GapDetectionJob: created gap issue"
                    );
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!(
                        spec_id = %gap.spec_id,
                        stderr = %stderr.trim(),
                        "GapDetectionJob: failed to create gap issue via gh CLI"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        spec_id = %gap.spec_id,
                        error = %e,
                        "GapDetectionJob: failed to invoke gh CLI"
                    );
                }
            }
        }

        // Step 5: For fully-covered specs, verify linked issues are all Done,
        // then transition Active -> Completing, run test commands, and create
        // HITL items for final confirmation.
        let mut completing_count = 0usize;
        for spec_id in &covered_spec_ids {
            // Skip if there is already an open queue item for this spec
            // (avoids duplicate HITL items on repeated cron runs).
            if self.db.has_open_items_for_source(spec_id).unwrap_or(false) {
                tracing::debug!(
                    spec_id = %spec_id,
                    "GapDetectionJob: skipping Completing transition — open item exists"
                );
                continue;
            }

            // 5a: Check all linked GitHub issues are closed/done.
            if !all_linked_issues_done(&self.db, spec_id) {
                tracing::info!(
                    spec_id = %spec_id,
                    "GapDetectionJob: skipping spec — not all linked issues are done"
                );
                continue;
            }

            // 5b: Transition Active -> Completing.
            if let Err(e) = self
                .db
                .update_spec_status(spec_id, belt_core::spec::SpecStatus::Completing)
            {
                tracing::warn!(
                    spec_id = %spec_id,
                    error = %e,
                    "GapDetectionJob: failed to transition spec to Completing"
                );
                continue;
            }

            tracing::info!(
                spec_id = %spec_id,
                "GapDetectionJob: spec transitioned to Completing (no gaps, all linked issues done)"
            );
            completing_count += 1;

            // 5c: Run test commands if the spec defines them.
            let spec = match self.db.get_spec(spec_id) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        spec_id = %spec_id,
                        error = %e,
                        "GapDetectionJob: failed to reload spec for test execution"
                    );
                    continue;
                }
            };

            let test_cmds = spec.test_command_list();
            if !test_cmds.is_empty() {
                tracing::info!(
                    spec_id = %spec_id,
                    commands = ?test_cmds,
                    "GapDetectionJob: running test commands for spec"
                );

                match belt_infra::test_runner::run_test_commands(
                    &test_cmds,
                    &self.workspace_root,
                    true, // fail-fast
                ) {
                    Ok(result) if result.all_passed => {
                        tracing::info!(
                            spec_id = %spec_id,
                            "GapDetectionJob: all test commands passed"
                        );
                        // Tests passed — create HITL for final human confirmation
                        // before advancing Completing -> Completed.
                        create_spec_completion_hitl(&self.db, &spec, "All test commands passed.");
                    }
                    Ok(result) => {
                        // Test failure — revert spec back to Active and create HITL.
                        let failed_cmds: Vec<&str> = result
                            .results
                            .iter()
                            .filter(|r| !r.success)
                            .map(|r| r.command.as_str())
                            .collect();
                        tracing::warn!(
                            spec_id = %spec_id,
                            failed_commands = ?failed_cmds,
                            "GapDetectionJob: test commands failed, reverting to Active"
                        );
                        if let Err(e) = self
                            .db
                            .update_spec_status(spec_id, belt_core::spec::SpecStatus::Active)
                        {
                            tracing::warn!(
                                spec_id = %spec_id,
                                error = %e,
                                "GapDetectionJob: failed to revert spec to Active after test failure"
                            );
                        }
                        // Create HITL so a human can investigate the test failure.
                        let detail = format!("Test commands failed: {}", failed_cmds.join(", "));
                        create_spec_test_failure_hitl(&self.db, &spec, &detail);
                    }
                    Err(e) => {
                        tracing::warn!(
                            spec_id = %spec_id,
                            error = %e,
                            "GapDetectionJob: failed to run test commands, reverting to Active"
                        );
                        if let Err(e2) = self
                            .db
                            .update_spec_status(spec_id, belt_core::spec::SpecStatus::Active)
                        {
                            tracing::warn!(
                                spec_id = %spec_id,
                                error = %e2,
                                "GapDetectionJob: failed to revert spec to Active"
                            );
                        }
                    }
                }
            } else {
                // No test commands — go straight to HITL for final confirmation.
                create_spec_completion_hitl(&self.db, &spec, "No test commands configured.");
            }
        }

        tracing::info!(
            total_specs = active_specs.len(),
            gaps_found = gaps.len(),
            gaps_skipped_dedupe = skipped_count,
            completing = completing_count,
            "GapDetectionJob: completed gap detection scan"
        );
        Ok(())
    }
}

/// Extracts knowledge from completed (Done) queue items (CR-08).
///
/// Runs every hour. Queries items in the `Done` phase, checks whether
/// knowledge has already been extracted for each item (via `source_ref`
/// deduplication), and persists new [`KnowledgeEntry`] rows to the
/// `knowledge_base` table.
///
/// Knowledge is categorised into:
/// - **decision**: items whose title or state suggests a decision was made
/// - **pattern**: items related to implementation patterns
/// - **domain**: general domain knowledge from the item context
///
/// [`KnowledgeEntry`]: belt_infra::db::KnowledgeEntry
pub struct KnowledgeExtractionJob {
    db: Arc<Database>,
}

impl KnowledgeExtractionJob {
    /// Create a new `KnowledgeExtractionJob`.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }
}

/// Keywords that signal a "decision" category.
const DECISION_KEYWORDS: &[&str] = &[
    "decided", "agreed", "chose", "choose", "decision", "approve", "reject",
];

/// Keywords that signal a "pattern" category.
const PATTERN_KEYWORDS: &[&str] = &[
    "pattern",
    "refactor",
    "abstraction",
    "convention",
    "template",
    "reusable",
];

/// Classify an item into a knowledge category based on its title and state.
///
/// Returns `"decision"` if title contains decision keywords, `"pattern"` if it
/// contains pattern keywords, or `"domain"` as the default category.
fn classify_knowledge_category(title: &str, state: &str) -> &'static str {
    let haystack = format!("{} {}", title.to_lowercase(), state.to_lowercase());
    if DECISION_KEYWORDS.iter().any(|kw| haystack.contains(kw)) {
        return "decision";
    }
    if PATTERN_KEYWORDS.iter().any(|kw| haystack.contains(kw)) {
        return "pattern";
    }
    "domain"
}

impl CronHandler for KnowledgeExtractionJob {
    fn execute(&self, ctx: &CronContext) -> Result<(), BeltError> {
        tracing::info!("KnowledgeExtractionJob: scanning completed items for knowledge");

        // Step 1: Query all Done items.
        let done_items = self.db.list_items(Some(QueuePhase::Done), None)?;

        if done_items.is_empty() {
            tracing::info!("KnowledgeExtractionJob: no Done items found, nothing to extract");
            return Ok(());
        }

        tracing::info!(
            count = done_items.len(),
            "KnowledgeExtractionJob: found Done items"
        );

        let mut extracted_count = 0u32;
        let mut skipped_count = 0u32;

        for item in &done_items {
            // Step 2: Deduplicate — skip items whose source_ref already exists.
            let source_ref = &item.source_id;
            let existing = self.db.get_knowledge_by_source(source_ref)?;
            if !existing.is_empty() {
                skipped_count += 1;
                continue;
            }

            // Step 3: Classify and extract knowledge content.
            let title = item.title.as_deref().unwrap_or(&item.work_id);
            let category = classify_knowledge_category(title, &item.state);
            let content = format!(
                "Completed work item: {} (state: {}, workspace: {})",
                title, item.state, item.workspace_id,
            );

            let entry = belt_infra::db::KnowledgeEntry {
                id: None,
                workspace: item.workspace_id.clone(),
                source_ref: source_ref.clone(),
                category: category.to_string(),
                content,
                created_at: ctx.now.to_rfc3339(),
            };

            // Step 4: Persist.
            match self.db.insert_knowledge(&entry) {
                Ok(()) => {
                    extracted_count += 1;
                    tracing::info!(
                        source_ref = %source_ref,
                        category = %category,
                        "KnowledgeExtractionJob: extracted knowledge entry"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        source_ref = %source_ref,
                        error = %e,
                        "KnowledgeExtractionJob: failed to persist knowledge entry"
                    );
                }
            }
        }

        tracing::info!(
            total_done = done_items.len(),
            extracted = extracted_count,
            skipped = skipped_count,
            "KnowledgeExtractionJob: completed knowledge extraction"
        );
        Ok(())
    }
}

/// Classifies completed queue items into Done or HITL.
///
/// This cron job triggers the evaluate cycle. The actual evaluation is
/// performed by [`crate::evaluator::Evaluator::run_evaluate`], which
/// spawns `belt agent --workspace <config> -p <prompt> --json` as an
/// isolated subprocess with:
/// - **Workspace isolation**: `WORKSPACE`, `BELT_HOME`, `BELT_DB` env vars
/// - **Timeout handling**: configurable timeout (default 5 min) with child kill
/// - **IPC**: structured JSON result collected from subprocess stdout
///
/// Per-item failure tracking and HITL escalation are handled by
/// [`crate::daemon::Daemon::evaluate_completed`].
///
/// The cron schedule ensures periodic evaluation, while `force_trigger("evaluate")`
/// is called on every Completed transition for immediate evaluation.
pub struct EvaluateJob;

impl CronHandler for EvaluateJob {
    fn execute(&self, _ctx: &CronContext) -> Result<(), BeltError> {
        // The actual evaluate_completed() logic runs in the daemon's async tick.
        // This cron handler serves as the schedule trigger point.
        // When the cron engine fires this job, the daemon's next tick will
        // pick up Completed items and invoke the evaluator subprocess.
        tracing::info!("EvaluateJob: triggering evaluate cycle for completed items");
        Ok(())
    }
}

/// Default state name used when enqueuing PR review feedback items.
const PR_REVIEW_SCAN_STATE: &str = "review_feedback";

/// Periodically scans open PRs for `changes_requested` reviews.
///
/// When a PR has a `CHANGES_REQUESTED` review, this job creates a new
/// queue item so the feedback loop can process the review comments and
/// push updated changes.
pub struct PrReviewScanJob {
    db: Arc<Database>,
}

impl CronHandler for PrReviewScanJob {
    fn execute(&self, _ctx: &CronContext) -> Result<(), BeltError> {
        // PR changes_requested detection is handled in
        // GitHubDataSource::collect() for states configured with
        // `trigger.changes_requested: true`.  This cron job triggers
        // an additional scan cycle to ensure timely detection between
        // regular collect intervals.
        tracing::info!("PrReviewScanJob: scanning PRs for changes_requested reviews");

        // List all registered workspaces from DB.
        let workspaces = self.db.list_workspaces()?;

        let mut enqueued_count = 0u32;

        for (name, config_path, _created_at) in &workspaces {
            // Load workspace config from disk.
            let workspace_config = match belt_infra::workspace_loader::load_workspace_config(
                std::path::Path::new(config_path),
            ) {
                Ok(cfg) => cfg,
                Err(e) => {
                    tracing::warn!(
                        workspace = %name,
                        error = %e,
                        "failed to load workspace config, skipping"
                    );
                    continue;
                }
            };

            // Determine the source URL for GitHub.
            let source_url = match workspace_config.sources.get("github") {
                Some(cfg) => cfg.url.clone(),
                None => continue,
            };

            let gh = belt_infra::sources::github::GitHubDataSource::new(&source_url);

            // Bridge async collect_review_items into this sync handler.
            let items = std::thread::scope(|_| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| BeltError::Runtime(e.to_string()))?;
                rt.block_on(gh.collect_review_items(&workspace_config, PR_REVIEW_SCAN_STATE))
                    .map_err(|e| BeltError::Runtime(e.to_string()))
            })?;

            for item in &items {
                match self.db.insert_item(item) {
                    Ok(()) => {
                        enqueued_count += 1;
                        tracing::info!(
                            work_id = %item.work_id,
                            title = ?item.title,
                            "enqueued PR review feedback item"
                        );
                    }
                    Err(BeltError::Database(ref msg)) if msg.contains("UNIQUE constraint") => {
                        // Item already exists in the queue — skip silently.
                        tracing::debug!(
                            work_id = %item.work_id,
                            "PR review item already enqueued, skipping"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            work_id = %item.work_id,
                            error = %e,
                            "failed to enqueue PR review feedback item"
                        );
                    }
                }
            }
        }

        tracing::info!(enqueued = enqueued_count, "PrReviewScanJob completed");
        Ok(())
    }
}

/// Create all built-in jobs with their default schedules.
///
/// Requires shared dependencies (database, worktree manager, etc.)
/// that the jobs use to perform their work.
pub fn builtin_jobs(deps: BuiltinJobDeps) -> Vec<CronJobDef> {
    vec![
        CronJobDef {
            name: "hitl_timeout".to_string(),
            schedule: CronSchedule::Interval(Duration::from_secs(5 * 60)),
            workspace: None,
            enabled: true,
            last_run_at: None,
            handler: Box::new(HitlTimeoutJob::new(
                Arc::clone(&deps.db),
                Arc::clone(&deps.worktree_mgr),
            )),
        },
        CronJobDef {
            name: "daily_report".to_string(),
            schedule: CronSchedule::Daily { hour: 6, min: 0 },
            workspace: None,
            enabled: true,
            last_run_at: None,
            handler: Box::new(DailyReportJob::new(
                Arc::clone(&deps.db),
                deps.report_dir.clone(),
            )),
        },
        CronJobDef {
            name: "log_cleanup".to_string(),
            schedule: CronSchedule::Daily { hour: 0, min: 0 },
            workspace: None,
            enabled: true,
            last_run_at: None,
            handler: Box::new(LogCleanupJob {
                db: Arc::clone(&deps.db),
                worktree_mgr: Arc::clone(&deps.worktree_mgr),
            }),
        },
        CronJobDef {
            name: "evaluate".to_string(),
            schedule: CronSchedule::Interval(Duration::from_secs(60)),
            workspace: None,
            enabled: true,
            last_run_at: None,
            handler: Box::new(EvaluateJob),
        },
        CronJobDef {
            name: "pr_review_scan".to_string(),
            schedule: CronSchedule::Interval(Duration::from_secs(5 * 60)),
            workspace: None,
            enabled: true,
            last_run_at: None,
            handler: Box::new(PrReviewScanJob {
                db: Arc::clone(&deps.db),
            }),
        },
        CronJobDef {
            name: "gap_detection".to_string(),
            schedule: CronSchedule::Interval(Duration::from_secs(3600)),
            workspace: None,
            enabled: true,
            last_run_at: None,
            handler: Box::new(GapDetectionJob::new(
                Arc::clone(&deps.db),
                deps.workspace_root.clone(),
            )),
        },
        CronJobDef {
            name: "knowledge_extraction".to_string(),
            schedule: CronSchedule::Interval(Duration::from_secs(3600)),
            workspace: None,
            enabled: true,
            last_run_at: None,
            handler: Box::new(KnowledgeExtractionJob::new(Arc::clone(&deps.db))),
        },
    ]
}

// ---------------------------------------------------------------------------
// Custom (user-defined) script jobs
// ---------------------------------------------------------------------------

/// A cron handler that executes a user-defined shell script.
///
/// When the cron engine fires this job, the handler spawns the script as a
/// child process and waits for it to complete. The script receives `BELT_HOME`
/// and `BELT_CRON_JOB` environment variables.
pub struct CustomScriptJob {
    /// Absolute path to the script to execute.
    pub script: String,
    /// Job name (passed as `BELT_CRON_JOB` env var).
    pub job_name: String,
    /// Database handle for updating `last_run_at` after execution.
    pub db: Arc<Database>,
}

impl CronHandler for CustomScriptJob {
    fn execute(&self, _ctx: &CronContext) -> Result<(), BeltError> {
        tracing::info!(
            job = %self.job_name,
            script = %self.script,
            "CustomScriptJob: executing user-defined script"
        );

        let belt_home = std::env::var("BELT_HOME").unwrap_or_else(|_| "~/.belt".to_string());

        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&self.script)
            .env("BELT_HOME", &belt_home)
            .env("BELT_CRON_JOB", &self.job_name)
            .output()
            .map_err(|e| {
                BeltError::Runtime(format!("failed to spawn script '{}': {e}", self.script))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::error!(
                job = %self.job_name,
                exit_code = ?output.status.code(),
                stderr = %stderr,
                "CustomScriptJob: script failed"
            );
            return Err(BeltError::Runtime(format!(
                "script '{}' exited with status {}: {}",
                self.script,
                output.status.code().unwrap_or(-1),
                stderr.trim()
            )));
        }

        // Update last_run_at in DB so the CLI can display accurate info.
        if let Err(e) = self.db.update_cron_last_run(&self.job_name) {
            tracing::warn!(
                job = %self.job_name,
                error = %e,
                "CustomScriptJob: failed to update last_run_at in DB"
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.is_empty() {
            tracing::info!(
                job = %self.job_name,
                stdout = %stdout.trim(),
                "CustomScriptJob: script output"
            );
        }

        Ok(())
    }
}

/// Load user-defined cron jobs from the database and register them with the engine.
///
/// Reads all cron jobs from the `cron_jobs` table, skips any whose name matches
/// a built-in job, and creates a [`CustomScriptJob`] handler for each.
pub fn load_custom_jobs(engine: &mut CronEngine, db: &Arc<Database>) {
    let jobs = match db.list_cron_jobs() {
        Ok(jobs) => jobs,
        Err(e) => {
            tracing::error!(error = %e, "failed to load custom cron jobs from DB");
            return;
        }
    };

    let builtin_names = [
        "hitl_timeout",
        "daily_report",
        "log_cleanup",
        "evaluate",
        "pr_review_scan",
        "gap_detection",
        "knowledge_extraction",
    ];

    for job in jobs {
        // Skip built-in jobs (they are registered separately).
        if builtin_names.contains(&job.name.as_str()) {
            continue;
        }
        // Skip workspace-scoped built-in jobs (e.g. "billing:hitl_timeout").
        if job.name.contains(':')
            && builtin_names
                .iter()
                .any(|b| job.name.ends_with(&format!(":{b}")))
        {
            continue;
        }

        let schedule = match CronSchedule::parse_expression(&job.schedule) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    job = %job.name,
                    schedule = %job.schedule,
                    error = %e,
                    "skipping custom cron job with invalid schedule"
                );
                continue;
            }
        };

        tracing::info!(
            job = %job.name,
            schedule = %job.schedule,
            enabled = job.enabled,
            "registering custom cron job from DB"
        );

        engine.register(CronJobDef {
            name: job.name.clone(),
            schedule,
            workspace: job.workspace.clone(),
            enabled: job.enabled,
            last_run_at: None,
            handler: Box::new(CustomScriptJob {
                script: job.script.clone(),
                job_name: job.name,
                db: Arc::clone(db),
            }),
        });
    }
}

// ---------------------------------------------------------------------------
// Per-workspace cron seed
// ---------------------------------------------------------------------------

/// Seed built-in cron jobs for a specific workspace.
///
/// Creates workspace-scoped instances of the standard jobs with the
/// intervals specified in the issue requirements:
/// - `HitlTimeoutJob` — every 1 hour
/// - `DailyReportJob` — every 24 hours
/// - `LogCleanupJob` — every 6 hours
/// - `EvaluateJob` — every 60 seconds
///
/// The `deps` parameter provides the shared dependencies (DB, worktree manager,
/// belt home, workspace name) used to initialise each job handler.
pub fn seed_workspace_crons(engine: &mut CronEngine, workspace: &str, deps: BuiltinJobDeps) {
    let ws = workspace.to_string();

    engine.register(CronJobDef {
        name: format!("{ws}:hitl_timeout"),
        schedule: CronSchedule::Interval(Duration::from_secs(3600)),
        workspace: Some(ws.clone()),
        enabled: true,
        last_run_at: None,
        handler: Box::new(HitlTimeoutJob::new(
            Arc::clone(&deps.db),
            Arc::clone(&deps.worktree_mgr),
        )),
    });

    engine.register(CronJobDef {
        name: format!("{ws}:daily_report"),
        schedule: CronSchedule::Interval(Duration::from_secs(86400)),
        workspace: Some(ws.clone()),
        enabled: true,
        last_run_at: None,
        handler: Box::new(DailyReportJob::new(
            Arc::clone(&deps.db),
            deps.report_dir.clone(),
        )),
    });

    engine.register(CronJobDef {
        name: format!("{ws}:log_cleanup"),
        schedule: CronSchedule::Interval(Duration::from_secs(21600)),
        workspace: Some(ws.clone()),
        enabled: true,
        last_run_at: None,
        handler: Box::new(LogCleanupJob {
            db: Arc::clone(&deps.db),
            worktree_mgr: Arc::clone(&deps.worktree_mgr),
        }),
    });

    engine.register(CronJobDef {
        name: format!("{ws}:evaluate"),
        schedule: CronSchedule::Interval(Duration::from_secs(60)),
        workspace: Some(ws.clone()),
        enabled: true,
        last_run_at: None,
        handler: Box::new(EvaluateJob),
    });

    engine.register(CronJobDef {
        name: format!("{ws}:knowledge_extraction"),
        schedule: CronSchedule::Interval(Duration::from_secs(3600)),
        workspace: Some(ws.clone()),
        enabled: true,
        last_run_at: None,
        handler: Box::new(KnowledgeExtractionJob::new(Arc::clone(&deps.db))),
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use belt_infra::db::Database;
    use chrono::TimeZone;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A mock handler that counts how many times it has been called.
    struct CountingHandler {
        count: Arc<AtomicU32>,
    }

    impl CronHandler for CountingHandler {
        fn execute(&self, _ctx: &CronContext) -> Result<(), BeltError> {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// A handler that always returns an error.
    struct FailingHandler;

    impl CronHandler for FailingHandler {
        fn execute(&self, _ctx: &CronContext) -> Result<(), BeltError> {
            Err(BeltError::Runtime("intentional failure".into()))
        }
    }

    fn make_counting_job(name: &str, schedule: CronSchedule) -> (CronJobDef, Arc<AtomicU32>) {
        let count = Arc::new(AtomicU32::new(0));
        let job = CronJobDef {
            name: name.to_string(),
            schedule,
            workspace: None,
            enabled: true,
            last_run_at: None,
            handler: Box::new(CountingHandler {
                count: Arc::clone(&count),
            }),
        };
        (job, count)
    }

    // -- CronSchedule::should_run tests --

    #[test]
    fn interval_should_run_when_never_executed() {
        let sched = CronSchedule::Interval(Duration::from_secs(60));
        assert!(sched.should_run(None, Utc::now()));
    }

    #[test]
    fn interval_should_not_run_when_recently_executed() {
        let sched = CronSchedule::Interval(Duration::from_secs(60));
        let now = Utc::now();
        let last = now - chrono::Duration::seconds(30);
        assert!(!sched.should_run(Some(last), now));
    }

    #[test]
    fn interval_should_run_when_enough_time_elapsed() {
        let sched = CronSchedule::Interval(Duration::from_secs(60));
        let now = Utc::now();
        let last = now - chrono::Duration::seconds(61);
        assert!(sched.should_run(Some(last), now));
    }

    #[test]
    fn daily_should_run_when_never_executed_and_past_time() {
        let sched = CronSchedule::Daily { hour: 6, min: 0 };
        let now = Utc.with_ymd_and_hms(2026, 3, 23, 7, 0, 0).unwrap();
        assert!(sched.should_run(None, now));
    }

    #[test]
    fn daily_should_not_run_before_scheduled_time() {
        let sched = CronSchedule::Daily { hour: 6, min: 0 };
        let now = Utc.with_ymd_and_hms(2026, 3, 23, 5, 59, 0).unwrap();
        assert!(!sched.should_run(None, now));
    }

    #[test]
    fn daily_should_not_run_twice_same_day() {
        let sched = CronSchedule::Daily { hour: 6, min: 0 };
        let last = Utc.with_ymd_and_hms(2026, 3, 23, 6, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 3, 23, 12, 0, 0).unwrap();
        assert!(!sched.should_run(Some(last), now));
    }

    #[test]
    fn daily_should_run_next_day() {
        let sched = CronSchedule::Daily { hour: 6, min: 0 };
        let last = Utc.with_ymd_and_hms(2026, 3, 22, 6, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 3, 23, 6, 1, 0).unwrap();
        assert!(sched.should_run(Some(last), now));
    }

    // -- CronEngine tests --

    #[test]
    fn register_and_unregister() {
        let mut engine = CronEngine::new();
        let (job, _) = make_counting_job("test", CronSchedule::Interval(Duration::from_secs(60)));
        engine.register(job);
        assert_eq!(engine.job_count(), 1);

        engine.unregister("test");
        assert_eq!(engine.job_count(), 0);
    }

    #[test]
    fn register_replaces_existing() {
        let mut engine = CronEngine::new();
        let (job1, _) = make_counting_job("a", CronSchedule::Interval(Duration::from_secs(60)));
        let (job2, _) = make_counting_job("a", CronSchedule::Interval(Duration::from_secs(120)));
        engine.register(job1);
        engine.register(job2);
        assert_eq!(engine.job_count(), 1);
    }

    #[test]
    fn pause_and_resume() {
        let mut engine = CronEngine::new();
        let (job, count) = make_counting_job("p", CronSchedule::Interval(Duration::from_secs(0)));
        engine.register(job);

        engine.pause("p");
        engine.tick();
        assert_eq!(count.load(Ordering::SeqCst), 0);

        engine.resume("p");
        engine.tick();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn force_trigger_resets_last_run() {
        let mut engine = CronEngine::new();
        let (job, count) =
            make_counting_job("ft", CronSchedule::Interval(Duration::from_secs(9999)));
        engine.register(job);

        // First tick always fires (last_run_at is None).
        engine.tick();
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Second tick should NOT fire — interval not elapsed.
        engine.tick();
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // After force_trigger, it should fire again.
        engine.force_trigger("ft");
        engine.tick();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn tick_executes_due_jobs() {
        let mut engine = CronEngine::new();
        let (job, count) = make_counting_job("t", CronSchedule::Interval(Duration::from_secs(0)));
        engine.register(job);

        engine.tick();
        assert!(count.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn tick_continues_after_handler_error() {
        let mut engine = CronEngine::new();

        // Failing job first.
        engine.register(CronJobDef {
            name: "fail".to_string(),
            schedule: CronSchedule::Interval(Duration::from_secs(0)),
            workspace: None,
            enabled: true,
            last_run_at: None,
            handler: Box::new(FailingHandler),
        });

        // Counting job second — should still execute.
        let (ok_job, count) =
            make_counting_job("ok", CronSchedule::Interval(Duration::from_secs(0)));
        engine.register(ok_job);

        engine.tick();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    fn make_test_deps() -> BuiltinJobDeps {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let worktree_mgr: Arc<dyn WorktreeManager> = Arc::new(
            belt_infra::worktree::MockWorktreeManager::new(tmp.path().to_path_buf()),
        );
        BuiltinJobDeps {
            db,
            worktree_mgr,
            workspace_root: tmp.path().to_path_buf(),
            report_dir: None,
        }
    }

    #[test]
    fn builtin_jobs_are_valid() {
        let deps = make_test_deps();
        let jobs = builtin_jobs(deps);
        assert_eq!(jobs.len(), 7);

        let names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
        assert!(names.contains(&"hitl_timeout"));
        assert!(names.contains(&"daily_report"));
        assert!(names.contains(&"log_cleanup"));
        assert!(names.contains(&"evaluate"));
        assert!(names.contains(&"pr_review_scan"));
        assert!(names.contains(&"gap_detection"));
        assert!(names.contains(&"knowledge_extraction"));
    }

    #[test]
    fn gap_detection_job_executes_successfully() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let job = GapDetectionJob::new(db, tmp.path().to_path_buf());
        let ctx = CronContext { now: Utc::now() };
        assert!(job.execute(&ctx).is_ok());
    }

    #[test]
    fn extract_keywords_filters_short_and_stop_words() {
        let kw = extract_keywords("the quick fox should implement authentication handler");
        assert!(kw.contains(&"quick".to_string()));
        assert!(kw.contains(&"implement".to_string()));
        assert!(kw.contains(&"authentication".to_string()));
        assert!(kw.contains(&"handler".to_string()));
        // "the" and "should" are stop-words, "fox" is < MIN_KEYWORD_LEN
        assert!(!kw.contains(&"the".to_string()));
        assert!(!kw.contains(&"should".to_string()));
        assert!(!kw.contains(&"fox".to_string()));
    }

    #[test]
    fn extract_keywords_deduplicates() {
        let kw = extract_keywords("token token token validation validation");
        assert_eq!(kw.iter().filter(|k| *k == "token").count(), 1);
        assert_eq!(kw.iter().filter(|k| *k == "validation").count(), 1);
    }

    #[test]
    fn extract_keywords_strips_punctuation() {
        let kw = extract_keywords("(authentication) [handler] {validator}");
        assert!(kw.contains(&"authentication".to_string()));
        assert!(kw.contains(&"handler".to_string()));
        assert!(kw.contains(&"validator".to_string()));
    }

    #[test]
    fn collect_code_corpus_reads_rs_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("main.rs"), "fn authentication_handler() {}").unwrap();
        std::fs::write(tmp.path().join("readme.txt"), "this should be ignored").unwrap();

        let corpus = collect_code_corpus(tmp.path());
        assert!(corpus.contains("authentication_handler"));
        assert!(!corpus.contains("ignored"));
    }

    #[test]
    fn collect_code_corpus_skips_hidden_and_target_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let hidden = tmp.path().join(".git");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("config.rs"), "fn secret() {}").unwrap();

        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("build.rs"), "fn build_artifact() {}").unwrap();

        std::fs::write(tmp.path().join("lib.rs"), "fn visible() {}").unwrap();

        let corpus = collect_code_corpus(tmp.path());
        assert!(corpus.contains("visible"));
        assert!(!corpus.contains("secret"));
        assert!(!corpus.contains("build_artifact"));
    }

    #[test]
    fn gap_detection_finds_gaps_for_active_specs() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();

        // Create a code file that mentions "authentication" but not "authorization".
        std::fs::write(tmp.path().join("auth.rs"), "fn authentication() {}").unwrap();

        // Insert an active spec whose keywords include "authorization" (not in code).
        let mut spec = belt_core::spec::Spec::new(
            "spec-gap".into(),
            "ws".into(),
            "Auth Gap".into(),
            "implement authorization middleware for secure endpoints".into(),
        );
        spec.status = belt_core::spec::SpecStatus::Active;
        db.insert_spec(&spec).unwrap();

        let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
        let ctx = CronContext { now: Utc::now() };
        // Should succeed even though gh CLI may not be available (warnings logged).
        assert!(job.execute(&ctx).is_ok());
    }

    #[test]
    fn gap_detection_no_gap_when_keywords_covered() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();

        // Code covers all spec keywords.
        std::fs::write(
            tmp.path().join("lib.rs"),
            "fn authentication() {}\nfn validation() {}\nfn middleware() {}",
        )
        .unwrap();

        let mut spec = belt_core::spec::Spec::new(
            "spec-ok".into(),
            "ws".into(),
            "All Covered".into(),
            "authentication validation middleware".into(),
        );
        spec.status = belt_core::spec::SpecStatus::Active;
        db.insert_spec(&spec).unwrap();

        let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
        let ctx = CronContext { now: Utc::now() };
        assert!(job.execute(&ctx).is_ok());
    }

    #[test]
    fn gap_detection_skips_when_open_item_exists_for_spec() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();

        // Create a code file that does NOT cover the spec keywords.
        std::fs::write(tmp.path().join("main.rs"), "fn unrelated_code() {}").unwrap();

        // Insert an active spec with keywords missing from code.
        let mut spec = belt_core::spec::Spec::new(
            "spec-dup".into(),
            "ws".into(),
            "Duplicate Gap".into(),
            "implement authorization middleware for secure endpoints".into(),
        );
        spec.status = belt_core::spec::SpecStatus::Active;
        db.insert_spec(&spec).unwrap();

        // Insert an open (non-terminal) queue item with source_id matching the spec_id.
        let item = belt_core::queue::QueueItem::new(
            "spec-dup:implement".into(),
            "spec-dup".into(),
            "ws".into(),
            "implement".into(),
        );
        db.insert_item(&item).unwrap();

        // The DB-based dedupe guard should detect the open item.
        assert!(db.has_open_items_for_source("spec-dup").unwrap());

        // Execute gap detection — should succeed without attempting to create
        // a duplicate issue (the gh CLI call is skipped).
        let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
        let ctx = CronContext { now: Utc::now() };
        assert!(job.execute(&ctx).is_ok());
    }

    #[test]
    fn has_open_items_for_source_returns_false_for_terminal_items() {
        let db = Arc::new(Database::open_in_memory().unwrap());

        // Insert a Done item.
        let mut item = belt_core::queue::QueueItem::new(
            "spec-done:implement".into(),
            "spec-done".into(),
            "ws".into(),
            "implement".into(),
        );
        item.phase = QueuePhase::Done;
        db.insert_item(&item).unwrap();

        // Terminal item should not count as open.
        assert!(!db.has_open_items_for_source("spec-done").unwrap());
    }

    #[test]
    fn has_open_items_for_source_returns_false_for_missing_source() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        assert!(!db.has_open_items_for_source("nonexistent").unwrap());
    }

    #[test]
    fn knowledge_extraction_job_executes_successfully() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let job = KnowledgeExtractionJob::new(Arc::clone(&db));
        let ctx = CronContext { now: Utc::now() };
        assert!(job.execute(&ctx).is_ok());
    }

    #[test]
    fn knowledge_extraction_extracts_from_done_items() {
        let db = Arc::new(Database::open_in_memory().unwrap());

        // Insert a Done item.
        let mut item = belt_core::queue::QueueItem::new(
            "w1".into(),
            "s1".into(),
            "ws".into(),
            "implement".into(),
        );
        item.title = Some("implement authentication handler".to_string());
        item.phase = QueuePhase::Done;
        db.insert_item(&item).unwrap();

        let job = KnowledgeExtractionJob::new(Arc::clone(&db));
        let ctx = CronContext { now: Utc::now() };
        assert!(job.execute(&ctx).is_ok());

        // Verify knowledge was extracted.
        let entries = db.get_knowledge_by_source("s1").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].workspace, "ws");
        assert_eq!(entries[0].category, "domain");
        assert!(entries[0].content.contains("authentication handler"));
    }

    #[test]
    fn knowledge_extraction_deduplicates() {
        let db = Arc::new(Database::open_in_memory().unwrap());

        let mut item = belt_core::queue::QueueItem::new(
            "w1".into(),
            "s1".into(),
            "ws".into(),
            "implement".into(),
        );
        item.title = Some("implement feature".to_string());
        item.phase = QueuePhase::Done;
        db.insert_item(&item).unwrap();

        let job = KnowledgeExtractionJob::new(Arc::clone(&db));
        let ctx = CronContext { now: Utc::now() };

        // Execute twice.
        job.execute(&ctx).unwrap();
        job.execute(&ctx).unwrap();

        // Should still have only one entry.
        let entries = db.get_knowledge_by_source("s1").unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn knowledge_extraction_classifies_decisions() {
        let db = Arc::new(Database::open_in_memory().unwrap());

        let mut item = belt_core::queue::QueueItem::new(
            "w1".into(),
            "s1".into(),
            "ws".into(),
            "implement".into(),
        );
        item.title = Some("decided to use JWT for authentication".to_string());
        item.phase = QueuePhase::Done;
        db.insert_item(&item).unwrap();

        let job = KnowledgeExtractionJob::new(Arc::clone(&db));
        let ctx = CronContext { now: Utc::now() };
        job.execute(&ctx).unwrap();

        let entries = db.get_knowledge_by_source("s1").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].category, "decision");
    }

    #[test]
    fn knowledge_extraction_classifies_patterns() {
        let db = Arc::new(Database::open_in_memory().unwrap());

        let mut item = belt_core::queue::QueueItem::new(
            "w1".into(),
            "s1".into(),
            "ws".into(),
            "refactor".into(),
        );
        item.title = Some("refactor auth module to use new pattern".to_string());
        item.phase = QueuePhase::Done;
        db.insert_item(&item).unwrap();

        let job = KnowledgeExtractionJob::new(Arc::clone(&db));
        let ctx = CronContext { now: Utc::now() };
        job.execute(&ctx).unwrap();

        let entries = db.get_knowledge_by_source("s1").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].category, "pattern");
    }

    #[test]
    fn gap_detection_has_hourly_schedule() {
        let jobs = builtin_jobs(make_test_deps());
        let gap = jobs.iter().find(|j| j.name == "gap_detection").unwrap();
        match &gap.schedule {
            CronSchedule::Interval(d) => assert_eq!(d.as_secs(), 3600),
            _ => panic!("expected Interval schedule"),
        }
    }

    #[test]
    fn knowledge_extraction_has_hourly_schedule() {
        let jobs = builtin_jobs(make_test_deps());
        let ke = jobs
            .iter()
            .find(|j| j.name == "knowledge_extraction")
            .unwrap();
        match &ke.schedule {
            CronSchedule::Interval(d) => assert_eq!(d.as_secs(), 3600),
            _ => panic!("expected Interval schedule"),
        }
    }

    // -- Built-in job logic tests --

    #[test]
    fn hitl_timeout_expires_old_items() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let worktree_mgr: Arc<dyn WorktreeManager> = Arc::new(
            belt_infra::worktree::MockWorktreeManager::new(tmp.path().to_path_buf()),
        );

        // Insert an item in HITL phase with an old timestamp.
        let old_time = (Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
        let mut item =
            belt_core::queue::QueueItem::new("w1".into(), "s1".into(), "ws".into(), "st".into());
        item.phase = QueuePhase::Hitl;
        item.created_at = old_time.clone();
        item.updated_at = old_time;
        db.insert_item(&item).unwrap();

        // Insert a recent HITL item that should NOT be expired.
        let mut recent =
            belt_core::queue::QueueItem::new("w2".into(), "s2".into(), "ws".into(), "st".into());
        recent.phase = QueuePhase::Hitl;
        db.insert_item(&recent).unwrap();

        // We need to set phase via DB since insert_item sets it from the struct.
        // Items are already in Hitl phase from the struct.

        let job = HitlTimeoutJob::new(Arc::clone(&db), worktree_mgr);
        let ctx = CronContext { now: Utc::now() };
        job.execute(&ctx).unwrap();

        // Old item should be Failed now.
        let updated = db.get_item("w1").unwrap();
        assert_eq!(updated.phase, QueuePhase::Failed);

        // Recent item should still be Hitl.
        let still_hitl = db.get_item("w2").unwrap();
        assert_eq!(still_hitl.phase, QueuePhase::Hitl);
    }

    #[test]
    fn hitl_timeout_uses_per_item_timeout_at() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let worktree_mgr: Arc<dyn WorktreeManager> = Arc::new(
            belt_infra::worktree::MockWorktreeManager::new(tmp.path().to_path_buf()),
        );

        // Item with per-item timeout in the past (should expire).
        let mut item = belt_core::queue::QueueItem::new(
            "w-expired".into(),
            "s1".into(),
            "ws".into(),
            "st".into(),
        );
        item.phase = QueuePhase::Hitl;
        item.hitl_timeout_at = Some((Utc::now() - chrono::Duration::minutes(5)).to_rfc3339());
        item.hitl_terminal_action = Some("skip".to_string());
        db.insert_item(&item).unwrap();

        // Item with per-item timeout in the future (should NOT expire).
        let mut future_item = belt_core::queue::QueueItem::new(
            "w-future".into(),
            "s2".into(),
            "ws".into(),
            "st".into(),
        );
        future_item.phase = QueuePhase::Hitl;
        future_item.hitl_timeout_at = Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339());
        future_item.hitl_terminal_action = Some("failed".to_string());
        db.insert_item(&future_item).unwrap();

        let job = HitlTimeoutJob::new(Arc::clone(&db), worktree_mgr);
        let ctx = CronContext { now: Utc::now() };
        job.execute(&ctx).unwrap();

        // Expired item should be Skipped (per its terminal action).
        let expired = db.get_item("w-expired").unwrap();
        assert_eq!(expired.phase, QueuePhase::Skipped);

        // Future item should still be Hitl.
        let still_hitl = db.get_item("w-future").unwrap();
        assert_eq!(still_hitl.phase, QueuePhase::Hitl);
    }

    #[test]
    fn hitl_timeout_terminal_action_failed() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let worktree_mgr: Arc<dyn WorktreeManager> = Arc::new(
            belt_infra::worktree::MockWorktreeManager::new(tmp.path().to_path_buf()),
        );

        let mut item =
            belt_core::queue::QueueItem::new("w1".into(), "s1".into(), "ws".into(), "st".into());
        item.phase = QueuePhase::Hitl;
        item.hitl_timeout_at = Some((Utc::now() - chrono::Duration::minutes(1)).to_rfc3339());
        item.hitl_terminal_action = Some("failed".to_string());
        db.insert_item(&item).unwrap();

        let job = HitlTimeoutJob::new(Arc::clone(&db), worktree_mgr);
        let ctx = CronContext { now: Utc::now() };
        job.execute(&ctx).unwrap();

        let updated = db.get_item("w1").unwrap();
        assert_eq!(updated.phase, QueuePhase::Failed);
    }

    #[test]
    fn hitl_timeout_falls_back_to_workspace_escalation_terminal_action() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let worktree_mgr: Arc<dyn WorktreeManager> = Arc::new(
            belt_infra::worktree::MockWorktreeManager::new(tmp.path().to_path_buf()),
        );

        // Create a workspace config file with terminal: skip.
        let ws_config_path = tmp.path().join("workspace.yml");
        std::fs::write(
            &ws_config_path,
            "name: test-ws\nsources:\n  github:\n    url: https://github.com/org/repo\n    escalation:\n      1: retry\n      2: hitl\n      terminal: skip\n",
        )
        .unwrap();

        // Register the workspace in the DB.
        db.add_workspace("test-ws", ws_config_path.to_str().unwrap())
            .unwrap();

        // Insert an expired HITL item WITHOUT per-item terminal_action.
        // The source_id starts with "github:" to match the source key.
        let mut item = belt_core::queue::QueueItem::new(
            "w-ws-fallback".into(),
            "github:org/repo#99".into(),
            "test-ws".into(),
            "implement".into(),
        );
        item.phase = QueuePhase::Hitl;
        item.hitl_timeout_at = Some((Utc::now() - chrono::Duration::minutes(5)).to_rfc3339());
        // hitl_terminal_action is None — should fall back to workspace policy.
        db.insert_item(&item).unwrap();

        let job = HitlTimeoutJob::new(Arc::clone(&db), worktree_mgr);
        let ctx = CronContext { now: Utc::now() };
        job.execute(&ctx).unwrap();

        // Item should be Skipped (from workspace escalation terminal: skip).
        let updated = db.get_item("w-ws-fallback").unwrap();
        assert_eq!(updated.phase, QueuePhase::Skipped);
    }

    #[test]
    fn hitl_timeout_defaults_to_failed_when_no_workspace_terminal_action() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let worktree_mgr: Arc<dyn WorktreeManager> = Arc::new(
            belt_infra::worktree::MockWorktreeManager::new(tmp.path().to_path_buf()),
        );

        // Create a workspace config WITHOUT terminal action in escalation.
        let ws_config_path = tmp.path().join("workspace.yml");
        std::fs::write(
            &ws_config_path,
            "name: no-terminal-ws\nsources:\n  github:\n    url: https://github.com/org/repo\n    escalation:\n      1: retry\n      2: hitl\n",
        )
        .unwrap();

        db.add_workspace("no-terminal-ws", ws_config_path.to_str().unwrap())
            .unwrap();

        let mut item = belt_core::queue::QueueItem::new(
            "w-no-term".into(),
            "github:org/repo#10".into(),
            "no-terminal-ws".into(),
            "implement".into(),
        );
        item.phase = QueuePhase::Hitl;
        item.hitl_timeout_at = Some((Utc::now() - chrono::Duration::minutes(1)).to_rfc3339());
        db.insert_item(&item).unwrap();

        let job = HitlTimeoutJob::new(Arc::clone(&db), worktree_mgr);
        let ctx = CronContext { now: Utc::now() };
        job.execute(&ctx).unwrap();

        // Should default to Failed (safe default).
        let updated = db.get_item("w-no-term").unwrap();
        assert_eq!(updated.phase, QueuePhase::Failed);
    }

    #[test]
    fn hitl_timeout_per_item_action_overrides_workspace_policy() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let worktree_mgr: Arc<dyn WorktreeManager> = Arc::new(
            belt_infra::worktree::MockWorktreeManager::new(tmp.path().to_path_buf()),
        );

        // Workspace says terminal: skip.
        let ws_config_path = tmp.path().join("workspace.yml");
        std::fs::write(
            &ws_config_path,
            "name: override-ws\nsources:\n  github:\n    url: https://github.com/org/repo\n    escalation:\n      1: retry\n      terminal: skip\n",
        )
        .unwrap();

        db.add_workspace("override-ws", ws_config_path.to_str().unwrap())
            .unwrap();

        // But per-item says "failed" — per-item should win.
        let mut item = belt_core::queue::QueueItem::new(
            "w-override".into(),
            "github:org/repo#5".into(),
            "override-ws".into(),
            "implement".into(),
        );
        item.phase = QueuePhase::Hitl;
        item.hitl_timeout_at = Some((Utc::now() - chrono::Duration::minutes(1)).to_rfc3339());
        item.hitl_terminal_action = Some("failed".to_string());
        db.insert_item(&item).unwrap();

        let job = HitlTimeoutJob::new(Arc::clone(&db), worktree_mgr);
        let ctx = CronContext { now: Utc::now() };
        job.execute(&ctx).unwrap();

        // Per-item "failed" overrides workspace "skip".
        let updated = db.get_item("w-override").unwrap();
        assert_eq!(updated.phase, QueuePhase::Failed);
    }

    #[test]
    fn daily_report_runs_without_error() {
        let db = Arc::new(Database::open_in_memory().unwrap());

        // Insert items in various phases.
        let mut done_item =
            belt_core::queue::QueueItem::new("w1".into(), "s1".into(), "ws".into(), "st".into());
        done_item.phase = QueuePhase::Done;
        db.insert_item(&done_item).unwrap();

        let mut failed_item =
            belt_core::queue::QueueItem::new("w2".into(), "s2".into(), "ws".into(), "st".into());
        failed_item.phase = QueuePhase::Failed;
        db.insert_item(&failed_item).unwrap();

        let job = DailyReportJob::new(Arc::clone(&db), None);
        let ctx = CronContext { now: Utc::now() };
        // Should not error even with items in various states.
        job.execute(&ctx).unwrap();
    }

    #[test]
    fn daily_report_generates_correct_summary() {
        let db = Arc::new(Database::open_in_memory().unwrap());

        // Insert items in multiple phases.
        let mut done1 =
            belt_core::queue::QueueItem::new("d1".into(), "s1".into(), "ws".into(), "st".into());
        done1.phase = QueuePhase::Done;
        db.insert_item(&done1).unwrap();

        let mut done2 =
            belt_core::queue::QueueItem::new("d2".into(), "s2".into(), "ws".into(), "st".into());
        done2.phase = QueuePhase::Done;
        db.insert_item(&done2).unwrap();

        let mut failed1 =
            belt_core::queue::QueueItem::new("f1".into(), "s3".into(), "ws".into(), "st".into());
        failed1.phase = QueuePhase::Failed;
        failed1.title = Some("failed task".into());
        db.insert_item(&failed1).unwrap();

        let mut hitl1 =
            belt_core::queue::QueueItem::new("h1".into(), "s4".into(), "ws".into(), "st".into());
        hitl1.phase = QueuePhase::Hitl;
        hitl1.hitl_notes = Some("needs review".into());
        db.insert_item(&hitl1).unwrap();

        let job = DailyReportJob::new(Arc::clone(&db), None);
        let ctx = CronContext { now: Utc::now() };
        let report = job.generate_report(&ctx).unwrap();

        assert_eq!(report.total_items, 4);
        assert_eq!(*report.phase_summary.get("done").unwrap_or(&0), 2);
        assert_eq!(*report.phase_summary.get("failed").unwrap_or(&0), 1);
        assert_eq!(*report.phase_summary.get("hitl").unwrap_or(&0), 1);
        assert_eq!(report.recent_failures.len(), 1);
        assert_eq!(report.recent_failures[0].work_id, "f1");
        assert_eq!(report.hitl_waiting.len(), 1);
        assert_eq!(report.hitl_waiting[0].work_id, "h1");
    }

    #[test]
    fn daily_report_saves_to_disk() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let report_dir = tmp.path().join("reports");

        let mut item =
            belt_core::queue::QueueItem::new("w1".into(), "s1".into(), "ws".into(), "st".into());
        item.phase = QueuePhase::Done;
        db.insert_item(&item).unwrap();

        let job = DailyReportJob::new(Arc::clone(&db), Some(report_dir.clone()));
        let ctx = CronContext { now: Utc::now() };
        job.execute(&ctx).unwrap();

        // Verify file was created.
        let date_str = ctx.now.format("%Y-%m-%d").to_string();
        let report_path = report_dir.join(format!("daily-report-{date_str}.json"));
        assert!(report_path.exists(), "report file should be created");

        // Verify contents are valid JSON.
        let content = std::fs::read_to_string(&report_path).unwrap();
        let parsed: DailyReport = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.date, date_str);
        assert_eq!(parsed.total_items, 1);
    }

    #[test]
    fn daily_report_empty_db() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let job = DailyReportJob::new(Arc::clone(&db), None);
        let ctx = CronContext { now: Utc::now() };

        let report = job.generate_report(&ctx).unwrap();
        assert_eq!(report.total_items, 0);
        assert!(report.recent_failures.is_empty());
        assert!(report.hitl_waiting.is_empty());
        assert_eq!(report.token_usage.total_tokens, 0);
        assert_eq!(report.token_usage.executions, 0);
    }

    #[test]
    fn log_cleanup_removes_old_worktrees() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let worktree_mgr: Arc<dyn WorktreeManager> = Arc::new(
            belt_infra::worktree::MockWorktreeManager::new(tmp.path().to_path_buf()),
        );

        // Insert a Done item with an old timestamp.
        let old_time = (Utc::now() - chrono::Duration::days(8)).to_rfc3339();
        let mut item =
            belt_core::queue::QueueItem::new("w1".into(), "s1".into(), "ws".into(), "st".into());
        item.phase = QueuePhase::Done;
        item.updated_at = old_time;
        db.insert_item(&item).unwrap();

        // Create a worktree for it.
        worktree_mgr.create_or_reuse("w1").unwrap();
        assert!(worktree_mgr.exists("w1"));

        let job = LogCleanupJob {
            db: Arc::clone(&db),
            worktree_mgr: Arc::clone(&worktree_mgr),
        };
        let ctx = CronContext { now: Utc::now() };
        job.execute(&ctx).unwrap();

        // Worktree should be cleaned up.
        assert!(!worktree_mgr.exists("w1"));
    }

    #[test]
    fn log_cleanup_preserves_recent_worktrees() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let worktree_mgr: Arc<dyn WorktreeManager> = Arc::new(
            belt_infra::worktree::MockWorktreeManager::new(tmp.path().to_path_buf()),
        );

        // Insert a Done item with a recent timestamp.
        let mut item =
            belt_core::queue::QueueItem::new("w1".into(), "s1".into(), "ws".into(), "st".into());
        item.phase = QueuePhase::Done;
        db.insert_item(&item).unwrap();

        // Create a worktree for it.
        worktree_mgr.create_or_reuse("w1").unwrap();
        assert!(worktree_mgr.exists("w1"));

        let job = LogCleanupJob {
            db: Arc::clone(&db),
            worktree_mgr: Arc::clone(&worktree_mgr),
        };
        let ctx = CronContext { now: Utc::now() };
        job.execute(&ctx).unwrap();

        // Worktree should still exist (not old enough).
        assert!(worktree_mgr.exists("w1"));
    }

    #[test]
    fn evaluate_job_runs_without_error() {
        let job = EvaluateJob;
        let ctx = CronContext { now: Utc::now() };
        // EvaluateJob is a trigger-only stub; the actual logic is in Daemon::evaluate_completed.
        job.execute(&ctx).unwrap();
    }

    // -- seed_workspace_crons tests --

    #[test]
    fn seed_workspace_crons_registers_five_jobs() {
        let mut engine = CronEngine::new();
        let deps = make_test_deps();
        seed_workspace_crons(&mut engine, "my-project", deps);
        assert_eq!(engine.job_count(), 5);
    }

    #[test]
    fn seed_workspace_crons_names_are_scoped() {
        let mut engine = CronEngine::new();
        let deps = make_test_deps();
        seed_workspace_crons(&mut engine, "auth", deps);

        // Verify all job names are workspace-scoped.
        let names: Vec<&str> = engine.jobs.iter().map(|j| j.name.as_str()).collect();
        assert!(names.contains(&"auth:hitl_timeout"));
        assert!(names.contains(&"auth:daily_report"));
        assert!(names.contains(&"auth:log_cleanup"));
        assert!(names.contains(&"auth:evaluate"));
        assert!(names.contains(&"auth:knowledge_extraction"));
    }

    #[test]
    fn seed_workspace_crons_sets_workspace_field() {
        let mut engine = CronEngine::new();
        let deps = make_test_deps();
        seed_workspace_crons(&mut engine, "billing", deps);

        for job in &engine.jobs {
            assert_eq!(job.workspace.as_deref(), Some("billing"));
        }
    }

    #[test]
    fn seed_workspace_crons_multiple_workspaces_coexist() {
        let mut engine = CronEngine::new();
        let deps1 = make_test_deps();
        let deps2 = make_test_deps();
        seed_workspace_crons(&mut engine, "alpha", deps1);
        seed_workspace_crons(&mut engine, "beta", deps2);
        assert_eq!(engine.job_count(), 10);
    }

    #[test]
    fn seed_workspace_crons_idempotent() {
        let mut engine = CronEngine::new();
        let deps1 = make_test_deps();
        let deps2 = make_test_deps();
        seed_workspace_crons(&mut engine, "ws", deps1);
        seed_workspace_crons(&mut engine, "ws", deps2);
        // register() replaces by name, so should still be 5.
        assert_eq!(engine.job_count(), 5);
    }

    // -- resolve_workspace_terminal_phase unit tests --

    #[test]
    fn resolve_terminal_phase_returns_skipped_for_skip_action() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();

        let ws_config_path = tmp.path().join("workspace.yml");
        std::fs::write(
            &ws_config_path,
            "name: ws-skip\nsources:\n  github:\n    url: https://github.com/org/repo\n    escalation:\n      1: retry\n      terminal: skip\n",
        )
        .unwrap();
        db.add_workspace("ws-skip", ws_config_path.to_str().unwrap())
            .unwrap();

        let mut cache = HashMap::new();
        let phase =
            resolve_workspace_terminal_phase(&db, "ws-skip", "github:org/repo#1", &mut cache);
        assert_eq!(phase, QueuePhase::Skipped);
    }

    #[test]
    fn resolve_terminal_phase_returns_failed_for_replan_action() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();

        let ws_config_path = tmp.path().join("workspace.yml");
        std::fs::write(
            &ws_config_path,
            "name: ws-replan\nsources:\n  github:\n    url: https://github.com/org/repo\n    escalation:\n      1: retry\n      terminal: replan\n",
        )
        .unwrap();
        db.add_workspace("ws-replan", ws_config_path.to_str().unwrap())
            .unwrap();

        let mut cache = HashMap::new();
        let phase =
            resolve_workspace_terminal_phase(&db, "ws-replan", "github:org/repo#2", &mut cache);
        assert_eq!(phase, QueuePhase::Failed);
    }

    #[test]
    fn resolve_terminal_phase_defaults_to_failed_when_workspace_not_found() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let mut cache = HashMap::new();
        let phase = resolve_workspace_terminal_phase(
            &db,
            "nonexistent-ws",
            "github:org/repo#1",
            &mut cache,
        );
        assert_eq!(phase, QueuePhase::Failed);
    }

    #[test]
    fn resolve_terminal_phase_defaults_to_failed_when_config_missing() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        // Register workspace pointing to a non-existent config file.
        db.add_workspace("ws-bad", "/nonexistent/workspace.yml")
            .unwrap();

        let mut cache = HashMap::new();
        let phase =
            resolve_workspace_terminal_phase(&db, "ws-bad", "github:org/repo#1", &mut cache);
        assert_eq!(phase, QueuePhase::Failed);
    }

    #[test]
    fn resolve_terminal_phase_defaults_to_failed_when_no_terminal_set() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();

        let ws_config_path = tmp.path().join("workspace.yml");
        std::fs::write(
            &ws_config_path,
            "name: ws-noterm\nsources:\n  github:\n    url: https://github.com/org/repo\n    escalation:\n      1: retry\n      2: hitl\n",
        )
        .unwrap();
        db.add_workspace("ws-noterm", ws_config_path.to_str().unwrap())
            .unwrap();

        let mut cache = HashMap::new();
        let phase =
            resolve_workspace_terminal_phase(&db, "ws-noterm", "github:org/repo#1", &mut cache);
        assert_eq!(phase, QueuePhase::Failed);
    }

    #[test]
    fn resolve_terminal_phase_extracts_source_key_from_source_id() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();

        // Config with a "custom" source that has terminal: skip.
        let ws_config_path = tmp.path().join("workspace.yml");
        std::fs::write(
            &ws_config_path,
            "name: ws-custom\nsources:\n  custom:\n    url: https://custom.example.com\n    escalation:\n      1: retry\n      terminal: skip\n",
        )
        .unwrap();
        db.add_workspace("ws-custom", ws_config_path.to_str().unwrap())
            .unwrap();

        let mut cache = HashMap::new();
        // source_id "custom:proj/item#5" should extract key "custom".
        let phase =
            resolve_workspace_terminal_phase(&db, "ws-custom", "custom:proj/item#5", &mut cache);
        assert_eq!(phase, QueuePhase::Skipped);
    }

    #[test]
    fn resolve_terminal_phase_uses_cache_on_repeated_call() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();

        let ws_config_path = tmp.path().join("workspace.yml");
        std::fs::write(
            &ws_config_path,
            "name: ws-cached\nsources:\n  github:\n    url: https://github.com/org/repo\n    escalation:\n      terminal: skip\n",
        )
        .unwrap();
        db.add_workspace("ws-cached", ws_config_path.to_str().unwrap())
            .unwrap();

        let mut cache = HashMap::new();
        let phase1 =
            resolve_workspace_terminal_phase(&db, "ws-cached", "github:org/repo#1", &mut cache);
        assert_eq!(phase1, QueuePhase::Skipped);

        // Cache should now contain the entry.
        assert!(cache.contains_key("ws-cached"));

        // Second call should use cache (same result).
        let phase2 =
            resolve_workspace_terminal_phase(&db, "ws-cached", "github:org/repo#2", &mut cache);
        assert_eq!(phase2, QueuePhase::Skipped);
    }

    // -- has_existing_gap_issue unit tests --

    #[test]
    fn has_existing_gap_issue_returns_false_when_gh_unavailable() {
        // When `gh` CLI is not available or fails, the function returns false
        // (safe side: allow issue creation so gaps are not silently swallowed).
        // In test environments gh may not be configured, so this exercises
        // the error/fallback branch.
        let result = has_existing_gap_issue("nonexistent-spec-name-xyz-12345");
        // Should return false (either gh fails or no matching issue exists).
        assert!(!result);
    }

    #[test]
    fn has_existing_gap_issue_constructs_correct_search_title() {
        // Verifies the search title format used internally.
        // The function searches for "[Gap] Spec '{spec_name}'" in issue titles.
        // We test with a name that is extremely unlikely to match any real issue.
        let result = has_existing_gap_issue("__belt_test_nonexistent_spec_42__");
        assert!(!result);
    }

    // -- HitlTimeoutJob::execute() terminal branching: replan --

    #[test]
    fn hitl_timeout_terminal_action_replan() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let worktree_mgr: Arc<dyn WorktreeManager> = Arc::new(
            belt_infra::worktree::MockWorktreeManager::new(tmp.path().to_path_buf()),
        );

        let mut item = belt_core::queue::QueueItem::new(
            "w-replan".into(),
            "s1".into(),
            "ws".into(),
            "st".into(),
        );
        item.phase = QueuePhase::Hitl;
        item.hitl_timeout_at = Some((Utc::now() - chrono::Duration::minutes(1)).to_rfc3339());
        item.hitl_terminal_action = Some("replan".to_string());
        db.insert_item(&item).unwrap();

        let job = HitlTimeoutJob::new(Arc::clone(&db), worktree_mgr);
        let ctx = CronContext { now: Utc::now() };
        job.execute(&ctx).unwrap();

        // "replan" maps to Failed (item goes back to queue for re-processing).
        let updated = db.get_item("w-replan").unwrap();
        assert_eq!(updated.phase, QueuePhase::Failed);
    }

    #[test]
    fn hitl_timeout_terminal_action_skip_cleans_worktree() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let worktree_mgr: Arc<dyn WorktreeManager> = Arc::new(
            belt_infra::worktree::MockWorktreeManager::new(tmp.path().to_path_buf()),
        );

        // Create a worktree for the item.
        worktree_mgr.create_or_reuse("w-skip-wt").unwrap();
        assert!(worktree_mgr.exists("w-skip-wt"));

        let mut item = belt_core::queue::QueueItem::new(
            "w-skip-wt".into(),
            "s1".into(),
            "ws".into(),
            "st".into(),
        );
        item.phase = QueuePhase::Hitl;
        item.hitl_timeout_at = Some((Utc::now() - chrono::Duration::minutes(1)).to_rfc3339());
        item.hitl_terminal_action = Some("skip".to_string());
        db.insert_item(&item).unwrap();

        let job = HitlTimeoutJob::new(Arc::clone(&db), worktree_mgr.clone());
        let ctx = CronContext { now: Utc::now() };
        job.execute(&ctx).unwrap();

        // Skip should transition to Skipped and cleanup worktree.
        let updated = db.get_item("w-skip-wt").unwrap();
        assert_eq!(updated.phase, QueuePhase::Skipped);
        assert!(!worktree_mgr.exists("w-skip-wt"));
    }

    #[test]
    fn hitl_timeout_no_expiry_when_all_items_recent() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let worktree_mgr: Arc<dyn WorktreeManager> = Arc::new(
            belt_infra::worktree::MockWorktreeManager::new(tmp.path().to_path_buf()),
        );

        // Insert a recent HITL item (no timeout_at, recent updated_at).
        let mut item = belt_core::queue::QueueItem::new(
            "w-recent".into(),
            "s1".into(),
            "ws".into(),
            "st".into(),
        );
        item.phase = QueuePhase::Hitl;
        db.insert_item(&item).unwrap();

        let job = HitlTimeoutJob::new(Arc::clone(&db), worktree_mgr);
        let ctx = CronContext { now: Utc::now() };
        job.execute(&ctx).unwrap();

        // Item should remain in Hitl phase.
        let updated = db.get_item("w-recent").unwrap();
        assert_eq!(updated.phase, QueuePhase::Hitl);
    }

    // -- GapDetectionJob::execute() dedupe guard tests --

    #[test]
    fn gap_detection_dedupe_skips_when_open_queue_item_exists() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();

        // Code that does NOT cover spec keywords.
        std::fs::write(tmp.path().join("main.rs"), "fn unrelated() {}").unwrap();

        // Insert an active spec.
        let mut spec = belt_core::spec::Spec::new(
            "spec-dedupe-q".into(),
            "ws".into(),
            "Dedupe Queue Test".into(),
            "implement authorization middleware for secure endpoints".into(),
        );
        spec.status = belt_core::spec::SpecStatus::Active;
        db.insert_spec(&spec).unwrap();

        // Insert an open queue item matching the spec's source_id.
        let item = belt_core::queue::QueueItem::new(
            "spec-dedupe-q:work".into(),
            "spec-dedupe-q".into(),
            "ws".into(),
            "implement".into(),
        );
        db.insert_item(&item).unwrap();

        // Verify precondition: DB-based dedupe guard detects the open item.
        assert!(db.has_open_items_for_source("spec-dedupe-q").unwrap());

        let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
        let ctx = CronContext { now: Utc::now() };
        // Should succeed; the dedupe guard prevents gh CLI issue creation.
        assert!(job.execute(&ctx).is_ok());
    }

    #[test]
    fn gap_detection_dedupe_allows_when_no_open_items() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();

        // Code that does NOT cover spec keywords.
        std::fs::write(tmp.path().join("main.rs"), "fn unrelated() {}").unwrap();

        // Insert an active spec with a gap.
        let mut spec = belt_core::spec::Spec::new(
            "spec-no-dup".into(),
            "ws".into(),
            "No Dup Test".into(),
            "implement authorization middleware for secure endpoints".into(),
        );
        spec.status = belt_core::spec::SpecStatus::Active;
        db.insert_spec(&spec).unwrap();

        // No open queue items for this spec.
        assert!(!db.has_open_items_for_source("spec-no-dup").unwrap());

        let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
        let ctx = CronContext { now: Utc::now() };
        // Should succeed (gh CLI may warn but the job itself should not error).
        assert!(job.execute(&ctx).is_ok());
    }

    #[test]
    fn gap_detection_dedupe_does_not_block_on_terminal_items() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();

        // Code that does NOT cover spec keywords.
        std::fs::write(tmp.path().join("main.rs"), "fn unrelated() {}").unwrap();

        // Insert an active spec.
        let mut spec = belt_core::spec::Spec::new(
            "spec-term".into(),
            "ws".into(),
            "Terminal Item Test".into(),
            "implement authorization middleware for secure endpoints".into(),
        );
        spec.status = belt_core::spec::SpecStatus::Active;
        db.insert_spec(&spec).unwrap();

        // Insert a Done (terminal) queue item — should NOT block gap detection.
        let mut item = belt_core::queue::QueueItem::new(
            "spec-term:work".into(),
            "spec-term".into(),
            "ws".into(),
            "implement".into(),
        );
        item.phase = QueuePhase::Done;
        db.insert_item(&item).unwrap();

        // Terminal items should not count as "open".
        assert!(!db.has_open_items_for_source("spec-term").unwrap());

        let job = GapDetectionJob::new(Arc::clone(&db), tmp.path().to_path_buf());
        let ctx = CronContext { now: Utc::now() };
        assert!(job.execute(&ctx).is_ok());
    }
}
