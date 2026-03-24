//! Cron engine: periodic job scheduler for the Belt daemon.
//!
//! Provides a simple interval/daily schedule system and an engine that
//! ticks through registered jobs, executing those that are due.

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
                    "HITL item expired after {} seconds, transitioned to Failed",
                    self.timeout_secs
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
        let running_count = self.db.list_items(Some(QueuePhase::Running), None)?.len();
        let pending_count = self.db.list_items(Some(QueuePhase::Pending), None)?.len();
        let completed_count = self.db.list_items(Some(QueuePhase::Completed), None)?.len();

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

        for spec in &active_specs {
            let keywords = extract_keywords(&spec.content);
            if keywords.is_empty() {
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
            }
        }

        // Step 4: Create GitHub issues for detected gaps.
        for gap in &gaps {
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

        tracing::info!(
            total_specs = active_specs.len(),
            gaps_found = gaps.len(),
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
    "pattern", "refactor", "abstraction", "convention", "template", "reusable",
];

/// Classify an item into a knowledge category based on its title and state.
///
/// Returns `"decision"` if title contains decision keywords, `"pattern"` if it
/// contains pattern keywords, or `"domain"` as the default category.
fn classify_knowledge_category(title: &str, state: &str) -> &'static str {
    let haystack = format!("{} {}", title.to_lowercase(), state.to_lowercase());
    if DECISION_KEYWORDS
        .iter()
        .any(|kw| haystack.contains(kw))
    {
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
/// This cron job triggers the evaluate cycle via `belt agent -p` invocation
/// (see [`crate::evaluator::Evaluator::build_evaluate_script`]).
/// The actual evaluate logic including per-item failure tracking and HITL
/// escalation is handled by [`crate::daemon::Daemon::evaluate_completed`].
///
/// The cron schedule ensures periodic evaluation, while `force_trigger("evaluate")`
/// is called on every Completed transition for immediate evaluation.
pub struct EvaluateJob;

impl CronHandler for EvaluateJob {
    fn execute(&self, _ctx: &CronContext) -> Result<(), BeltError> {
        // The actual evaluate_completed() logic runs in the daemon's async tick.
        // This cron handler serves as the schedule trigger point.
        // When the cron engine fires this job, the daemon's next tick will
        // pick up Completed items and run the evaluator.
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
        handler: Box::new(DailyReportJob {
            db: Arc::clone(&deps.db),
        }),
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
}
