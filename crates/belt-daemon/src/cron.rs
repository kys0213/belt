//! Cron engine: periodic job scheduler for the Belt daemon.
//!
//! Provides a simple interval/daily schedule system and an engine that
//! ticks through registered jobs, executing those that are due.

use belt_core::error::BeltError;
use chrono::{DateTime, Utc};
use std::time::Duration;

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

/// Expires unanswered HITL (human-in-the-loop) items after a timeout.
pub struct HitlTimeoutJob;

impl CronHandler for HitlTimeoutJob {
    fn execute(&self, _ctx: &CronContext) -> Result<(), BeltError> {
        // TODO: query pending HITL items, expire those older than threshold
        tracing::info!("HitlTimeoutJob: checking for expired HITL items");
        Ok(())
    }
}

/// Generates a daily summary report.
pub struct DailyReportJob;

impl CronHandler for DailyReportJob {
    fn execute(&self, _ctx: &CronContext) -> Result<(), BeltError> {
        // TODO: aggregate metrics and produce daily report
        tracing::info!("DailyReportJob: generating daily report");
        Ok(())
    }
}

/// Cleans up old logs and worktrees.
pub struct LogCleanupJob;

impl CronHandler for LogCleanupJob {
    fn execute(&self, _ctx: &CronContext) -> Result<(), BeltError> {
        // TODO: delete old log files and stale worktrees
        tracing::info!("LogCleanupJob: cleaning up old logs and worktrees");
        Ok(())
    }
}

/// Classifies completed queue items into Done or HITL.
pub struct EvaluateJob;

impl CronHandler for EvaluateJob {
    fn execute(&self, _ctx: &CronContext) -> Result<(), BeltError> {
        // TODO: scan Completed items, classify into Done or HITL
        tracing::info!("EvaluateJob: evaluating completed items");
        Ok(())
    }
}

/// Create all built-in jobs with their default schedules.
pub fn builtin_jobs() -> Vec<CronJobDef> {
    vec![
        CronJobDef {
            name: "hitl_timeout".to_string(),
            schedule: CronSchedule::Interval(Duration::from_secs(5 * 60)),
            workspace: None,
            enabled: true,
            last_run_at: None,
            handler: Box::new(HitlTimeoutJob),
        },
        CronJobDef {
            name: "daily_report".to_string(),
            schedule: CronSchedule::Daily { hour: 6, min: 0 },
            workspace: None,
            enabled: true,
            last_run_at: None,
            handler: Box::new(DailyReportJob),
        },
        CronJobDef {
            name: "log_cleanup".to_string(),
            schedule: CronSchedule::Daily { hour: 0, min: 0 },
            workspace: None,
            enabled: true,
            last_run_at: None,
            handler: Box::new(LogCleanupJob),
        },
        CronJobDef {
            name: "evaluate".to_string(),
            schedule: CronSchedule::Interval(Duration::from_secs(60)),
            workspace: None,
            enabled: true,
            last_run_at: None,
            handler: Box::new(EvaluateJob),
        },
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn builtin_jobs_are_valid() {
        let jobs = builtin_jobs();
        assert_eq!(jobs.len(), 4);

        let names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
        assert!(names.contains(&"hitl_timeout"));
        assert!(names.contains(&"daily_report"));
        assert!(names.contains(&"log_cleanup"));
        assert!(names.contains(&"evaluate"));
    }
}
