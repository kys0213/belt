//! Cron engine: periodic job scheduler for the Belt daemon.
//!
//! Provides a simple interval/daily schedule system and an engine that
//! ticks through registered jobs, executing those that are due.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use belt_core::error::BeltError;
use belt_core::phase::QueuePhase;
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
        }
    }
}

use chrono::Timelike;

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

/// HITL timeout threshold (24 hours).
const HITL_TIMEOUT_HOURS: i64 = 24;

/// Worktree TTL for log cleanup (7 days).
const WORKTREE_TTL_DAYS: i64 = 7;

/// Shared dependencies for built-in cron jobs.
pub struct BuiltinJobDeps {
    /// Database handle for querying and updating queue items.
    pub db: Arc<Database>,
    /// Worktree manager for cleanup operations.
    pub worktree_mgr: Arc<dyn WorktreeManager>,
    /// Belt home directory for evaluator scripts.
    pub belt_home: PathBuf,
    /// Workspace name for evaluate job.
    pub workspace: String,
}

/// Expires unanswered HITL (human-in-the-loop) items after a 24-hour timeout.
///
/// Queries items in the `Hitl` phase, checks their `updated_at` timestamp,
/// and transitions those older than 24 hours to `Failed`.
pub struct HitlTimeoutJob {
    db: Arc<Database>,
    worktree_mgr: Arc<dyn WorktreeManager>,
}

impl CronHandler for HitlTimeoutJob {
    fn execute(&self, ctx: &CronContext) -> Result<(), BeltError> {
        tracing::info!("HitlTimeoutJob: checking for expired HITL items");

        let hitl_items = self.db.list_items(Some(QueuePhase::Hitl), None)?;
        let threshold = ctx.now - chrono::Duration::hours(HITL_TIMEOUT_HOURS);
        let mut expired_count = 0u32;

        for item in &hitl_items {
            let updated = DateTime::parse_from_rfc3339(&item.updated_at)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or(ctx.now);

            if updated < threshold {
                if let Err(e) = self.db.update_phase(&item.work_id, QueuePhase::Failed) {
                    tracing::warn!(
                        work_id = %item.work_id,
                        error = %e,
                        "failed to expire HITL item"
                    );
                    continue;
                }

                // Clean up the associated worktree.
                if let Err(e) = self.worktree_mgr.cleanup(&item.work_id) {
                    tracing::warn!(
                        work_id = %item.work_id,
                        error = %e,
                        "failed to cleanup worktree for expired HITL item"
                    );
                }

                expired_count += 1;
                tracing::info!(
                    work_id = %item.work_id,
                    "HITL item expired after {} hours",
                    HITL_TIMEOUT_HOURS
                );
            }
        }

        tracing::info!(
            total_hitl = hitl_items.len(),
            expired = expired_count,
            "HitlTimeoutJob completed"
        );
        Ok(())
    }
}

/// Generates a daily summary report by aggregating queue item statistics.
///
/// Counts items in each relevant phase (Done, Failed, Hitl, Running, Pending)
/// and logs a summary.
pub struct DailyReportJob {
    db: Arc<Database>,
}

impl CronHandler for DailyReportJob {
    fn execute(&self, _ctx: &CronContext) -> Result<(), BeltError> {
        tracing::info!("DailyReportJob: generating daily report");

        let done_count = self.db.list_items(Some(QueuePhase::Done), None)?.len();
        let failed_count = self.db.list_items(Some(QueuePhase::Failed), None)?.len();
        let hitl_count = self.db.list_items(Some(QueuePhase::Hitl), None)?.len();
        let running_count = self
            .db
            .list_items(Some(QueuePhase::Running), None)?
            .len();
        let pending_count = self
            .db
            .list_items(Some(QueuePhase::Pending), None)?
            .len();
        let completed_count = self
            .db
            .list_items(Some(QueuePhase::Completed), None)?
            .len();

        tracing::info!(
            done = done_count,
            failed = failed_count,
            hitl_waiting = hitl_count,
            running = running_count,
            pending = pending_count,
            completed = completed_count,
            "=== Daily Report ==="
        );

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

/// Evaluates completed queue items by running the evaluate script.
///
/// Scans items in the `Completed` phase, invokes the evaluate script,
/// and transitions items to `Done` (exit code 0) or `Hitl` (non-zero).
pub struct EvaluateJob {
    db: Arc<Database>,
    belt_home: PathBuf,
    workspace: String,
}

impl CronHandler for EvaluateJob {
    fn execute(&self, _ctx: &CronContext) -> Result<(), BeltError> {
        tracing::info!("EvaluateJob: evaluating completed items");

        let completed_items = self.db.list_items(Some(QueuePhase::Completed), None)?;
        if completed_items.is_empty() {
            tracing::debug!("EvaluateJob: no completed items to evaluate");
            return Ok(());
        }

        let evaluator = crate::evaluator::Evaluator::new(&self.workspace);
        let script = evaluator.build_evaluate_script();

        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .env("WORKSPACE", &self.workspace)
            .env("BELT_HOME", self.belt_home.to_string_lossy().as_ref())
            .output()
            .map_err(|e| BeltError::Runtime(format!("failed to run evaluate script: {e}")))?;

        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if exit_code == 0 {
            tracing::info!(
                items = completed_items.len(),
                "evaluate script succeeded"
            );
        } else {
            tracing::warn!(
                exit_code,
                stderr = %stderr,
                stdout = %stdout,
                "evaluate script returned non-zero exit code"
            );
        }

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
            handler: Box::new(HitlTimeoutJob {
                db: Arc::clone(&deps.db),
                worktree_mgr: Arc::clone(&deps.worktree_mgr),
            }),
        },
        CronJobDef {
            name: "daily_report".to_string(),
            schedule: CronSchedule::Daily { hour: 6, min: 0 },
            workspace: None,
            enabled: true,
            last_run_at: None,
            handler: Box::new(DailyReportJob {
                db: Arc::clone(&deps.db),
            }),
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
            handler: Box::new(EvaluateJob {
                db: deps.db,
                belt_home: deps.belt_home,
                workspace: deps.workspace,
            }),
        },
    ]
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
        let worktree_mgr: Arc<dyn WorktreeManager> =
            Arc::new(belt_infra::worktree::MockWorktreeManager::new(
                tmp.path().to_path_buf(),
            ));
        BuiltinJobDeps {
            db,
            worktree_mgr,
            belt_home: tmp.path().to_path_buf(),
            workspace: "test-ws".to_string(),
        }
    }

    #[test]
    fn builtin_jobs_are_valid() {
        let deps = make_test_deps();
        let jobs = builtin_jobs(deps);
        assert_eq!(jobs.len(), 4);

        let names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
        assert!(names.contains(&"hitl_timeout"));
        assert!(names.contains(&"daily_report"));
        assert!(names.contains(&"log_cleanup"));
        assert!(names.contains(&"evaluate"));
    }

    // -- Built-in job logic tests --

    #[test]
    fn hitl_timeout_expires_old_items() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let worktree_mgr: Arc<dyn WorktreeManager> =
            Arc::new(belt_infra::worktree::MockWorktreeManager::new(
                tmp.path().to_path_buf(),
            ));

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

        let job = HitlTimeoutJob {
            db: Arc::clone(&db),
            worktree_mgr,
        };
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

        let job = DailyReportJob {
            db: Arc::clone(&db),
        };
        let ctx = CronContext { now: Utc::now() };
        // Should not error even with items in various states.
        job.execute(&ctx).unwrap();
    }

    #[test]
    fn log_cleanup_removes_old_worktrees() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let worktree_mgr: Arc<dyn WorktreeManager> =
            Arc::new(belt_infra::worktree::MockWorktreeManager::new(
                tmp.path().to_path_buf(),
            ));

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
        let worktree_mgr: Arc<dyn WorktreeManager> =
            Arc::new(belt_infra::worktree::MockWorktreeManager::new(
                tmp.path().to_path_buf(),
            ));

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
    fn evaluate_job_runs_with_no_completed_items() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();

        let job = EvaluateJob {
            db,
            belt_home: tmp.path().to_path_buf(),
            workspace: "test-ws".to_string(),
        };
        let ctx = CronContext { now: Utc::now() };
        // Should return Ok when there are no completed items (early return).
        job.execute(&ctx).unwrap();
    }
}
