//! E2E integration test: CronEngine tick and job execution.
//!
//! Tests CronEngine scheduling, job registration, pause/resume,
//! force trigger, and interaction with the daemon.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use belt_core::error::BeltError;
use belt_daemon::cron::{CronContext, CronEngine, CronHandler, CronJobDef, CronSchedule};
use chrono::{TimeZone, Utc};

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

/// A handler that always fails.
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

/// tick() executes due jobs.
#[test]
fn tick_executes_due_jobs() {
    let mut engine = CronEngine::new();
    let (job, count) = make_counting_job("test", CronSchedule::Interval(Duration::from_secs(0)));
    engine.register(job);

    engine.tick();
    assert!(
        count.load(Ordering::SeqCst) >= 1,
        "job should have executed"
    );
}

/// tick() skips disabled (paused) jobs.
#[test]
fn tick_skips_paused_jobs() {
    let mut engine = CronEngine::new();
    let (job, count) = make_counting_job("p", CronSchedule::Interval(Duration::from_secs(0)));
    engine.register(job);

    engine.pause("p");
    engine.tick();
    assert_eq!(count.load(Ordering::SeqCst), 0, "paused job should not run");
}

/// resume() re-enables a paused job.
#[test]
fn resume_reenables_paused_job() {
    let mut engine = CronEngine::new();
    let (job, count) = make_counting_job("r", CronSchedule::Interval(Duration::from_secs(0)));
    engine.register(job);

    engine.pause("r");
    engine.tick();
    assert_eq!(count.load(Ordering::SeqCst), 0);

    engine.resume("r");
    engine.tick();
    assert_eq!(count.load(Ordering::SeqCst), 1, "resumed job should run");
}

/// register() replaces existing job with the same name.
#[test]
fn register_replaces_existing_job() {
    let mut engine = CronEngine::new();
    let (job1, _) = make_counting_job("a", CronSchedule::Interval(Duration::from_secs(60)));
    let (job2, count2) = make_counting_job("a", CronSchedule::Interval(Duration::from_secs(0)));
    engine.register(job1);
    engine.register(job2);

    assert_eq!(engine.job_count(), 1, "duplicate name should replace");
    engine.tick();
    assert_eq!(
        count2.load(Ordering::SeqCst),
        1,
        "replacement job should run"
    );
}

/// unregister() removes a job by name.
#[test]
fn unregister_removes_job() {
    let mut engine = CronEngine::new();
    let (job, _) = make_counting_job("x", CronSchedule::Interval(Duration::from_secs(60)));
    engine.register(job);
    assert_eq!(engine.job_count(), 1);

    engine.unregister("x");
    assert_eq!(engine.job_count(), 0);
}

/// unregister() is a no-op for non-existent names.
#[test]
fn unregister_nonexistent_is_noop() {
    let mut engine = CronEngine::new();
    engine.unregister("nonexistent");
    assert_eq!(engine.job_count(), 0);
}

/// force_trigger() resets last_run_at so the job fires on next tick.
#[test]
fn force_trigger_fires_on_next_tick() {
    let mut engine = CronEngine::new();
    let (job, count) = make_counting_job("ft", CronSchedule::Interval(Duration::from_secs(9999)));
    engine.register(job);

    // First tick fires (last_run_at is None).
    engine.tick();
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // Second tick should NOT fire because interval hasn't elapsed.
    engine.tick();
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // After force_trigger, it should fire again.
    engine.force_trigger("ft");
    engine.tick();
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

/// tick() continues executing remaining jobs after a handler error.
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

    // Counting job second.
    let (ok_job, count) = make_counting_job("ok", CronSchedule::Interval(Duration::from_secs(0)));
    engine.register(ok_job);

    engine.tick();
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "second job should still execute after first job fails"
    );
}

/// Multiple jobs with different schedules.
#[test]
fn multiple_jobs_different_schedules() {
    let mut engine = CronEngine::new();

    // Zero-interval job (fires every tick).
    let (fast_job, fast_count) =
        make_counting_job("fast", CronSchedule::Interval(Duration::from_secs(0)));
    engine.register(fast_job);

    // Large-interval job (fires only on first tick).
    let (slow_job, slow_count) =
        make_counting_job("slow", CronSchedule::Interval(Duration::from_secs(9999)));
    engine.register(slow_job);

    engine.tick();
    assert_eq!(fast_count.load(Ordering::SeqCst), 1);
    assert_eq!(slow_count.load(Ordering::SeqCst), 1); // First tick always fires.

    engine.tick();
    // fast runs again since interval=0, slow does not.
    assert!(fast_count.load(Ordering::SeqCst) >= 2);
    assert_eq!(slow_count.load(Ordering::SeqCst), 1);
}

/// Workspace-scoped jobs.
#[test]
fn workspace_scoped_jobs() {
    let mut engine = CronEngine::new();

    let count = Arc::new(AtomicU32::new(0));
    let job = CronJobDef {
        name: "ws1:evaluate".to_string(),
        schedule: CronSchedule::Interval(Duration::from_secs(0)),
        workspace: Some("ws1".to_string()),
        enabled: true,
        last_run_at: None,
        handler: Box::new(CountingHandler {
            count: Arc::clone(&count),
        }),
    };
    engine.register(job);

    engine.tick();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(engine.job_count(), 1);
}

/// CronSchedule::Interval -- should_run logic.
#[test]
fn interval_schedule_should_run() {
    let sched = CronSchedule::Interval(Duration::from_secs(60));

    // Never executed: should run.
    assert!(sched.should_run(None, Utc::now()));

    // Recently executed: should NOT run.
    let now = Utc::now();
    let last = now - chrono::Duration::seconds(30);
    assert!(!sched.should_run(Some(last), now));

    // Enough time elapsed: should run.
    let last = now - chrono::Duration::seconds(61);
    assert!(sched.should_run(Some(last), now));
}

/// CronSchedule::Daily -- should_run logic.
#[test]
fn daily_schedule_should_run() {
    let sched = CronSchedule::Daily { hour: 6, min: 0 };

    // Never executed, past scheduled time: should run.
    let now = Utc.with_ymd_and_hms(2026, 3, 24, 7, 0, 0).unwrap();
    assert!(sched.should_run(None, now));

    // Before scheduled time: should NOT run.
    let now_early = Utc.with_ymd_and_hms(2026, 3, 24, 5, 59, 0).unwrap();
    assert!(!sched.should_run(None, now_early));

    // Already ran today: should NOT run.
    let last = Utc.with_ymd_and_hms(2026, 3, 24, 6, 0, 0).unwrap();
    let now_later = Utc.with_ymd_and_hms(2026, 3, 24, 12, 0, 0).unwrap();
    assert!(!sched.should_run(Some(last), now_later));

    // Last ran yesterday, now past scheduled time: should run.
    let last_yesterday = Utc.with_ymd_and_hms(2026, 3, 23, 6, 0, 0).unwrap();
    let now_next_day = Utc.with_ymd_and_hms(2026, 3, 24, 6, 1, 0).unwrap();
    assert!(sched.should_run(Some(last_yesterday), now_next_day));
}

/// CronEngine::Default creates empty engine.
#[test]
fn default_engine_is_empty() {
    let engine = CronEngine::default();
    assert_eq!(engine.job_count(), 0);
}

/// Pause and resume non-existent jobs are no-ops.
#[test]
fn pause_resume_nonexistent_is_noop() {
    let mut engine = CronEngine::new();
    engine.pause("nonexistent");
    engine.resume("nonexistent");
    assert_eq!(engine.job_count(), 0);
}

/// force_trigger on non-existent job is a no-op.
#[test]
fn force_trigger_nonexistent_is_noop() {
    let mut engine = CronEngine::new();
    engine.force_trigger("nonexistent");
    assert_eq!(engine.job_count(), 0);
}

/// Job that records context time.
struct TimeRecordingHandler {
    recorded_time: Arc<std::sync::Mutex<Option<chrono::DateTime<Utc>>>>,
}

impl CronHandler for TimeRecordingHandler {
    fn execute(&self, ctx: &CronContext) -> Result<(), BeltError> {
        *self.recorded_time.lock().unwrap() = Some(ctx.now);
        Ok(())
    }
}

/// CronContext carries the current wall-clock time.
#[test]
fn cron_context_carries_time() {
    let recorded = Arc::new(std::sync::Mutex::new(None));
    let mut engine = CronEngine::new();
    engine.register(CronJobDef {
        name: "time-check".to_string(),
        schedule: CronSchedule::Interval(Duration::from_secs(0)),
        workspace: None,
        enabled: true,
        last_run_at: None,
        handler: Box::new(TimeRecordingHandler {
            recorded_time: Arc::clone(&recorded),
        }),
    });

    let before = Utc::now();
    engine.tick();
    let after = Utc::now();

    let recorded_time = recorded.lock().unwrap().unwrap();
    assert!(
        recorded_time >= before && recorded_time <= after,
        "context time should be between before and after tick"
    );
}
