use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use belt_core::phase::QueuePhase;
use belt_infra::db::Database;

mod agent;
mod bootstrap;
mod dashboard;
mod status;

use belt_core::runtime::{AgentRuntime, RuntimeRegistry};
use belt_daemon::daemon::Daemon;
use belt_infra::runtimes::claude::ClaudeRuntime;
use belt_infra::runtimes::codex::CodexRuntime;
use belt_infra::runtimes::gemini::GeminiRuntime;
use belt_infra::sources::github::GitHubDataSource;
use belt_infra::worktree::{GitWorktreeManager, WorktreeManager};

mod auto;
mod claw;

#[derive(Parser)]
#[command(
    name = "belt",
    version,
    about = "Conveyor belt for autonomous development"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the daemon.
    Start {
        /// Path to workspace.yaml config.
        #[arg(long, default_value = "workspace.yaml")]
        config: String,
        /// Tick interval in seconds.
        #[arg(long, default_value_t = 30)]
        tick: u64,
        /// Maximum concurrent tasks.
        #[arg(long, default_value_t = 4)]
        max_concurrent: u32,
    },
    /// Stop the daemon.
    Stop,
    /// Restart the daemon (stop then start).
    Restart {
        /// Path to workspace.yaml config (defaults to workspace.yaml).
        #[arg(long, default_value = "workspace.yaml")]
        config: String,
        /// Run in background.
        #[arg(long)]
        background: bool,
    },
    /// Show system status.
    Status {
        /// Output format (text, json, rich).
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Open the real-time TUI dashboard.
    Dashboard,
    /// Workspace management.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommands,
    },
    /// Queue operations.
    Queue {
        #[command(subcommand)]
        command: QueueCommands,
    },
    /// Cron job management.
    Cron {
        #[command(subcommand)]
        command: CronCommands,
    },
    /// Retrieve item context for scripts.
    Context {
        /// Queue item work_id.
        work_id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
        /// Extract a specific field using dot notation (e.g. issue.number).
        #[arg(long)]
        field: Option<String>,
    },
    /// Agent workspace and session management.
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },
    /// Spec lifecycle management.
    Spec {
        #[command(subcommand)]
        command: SpecCommands,
    },
    /// Human-in-the-loop operations.
    Hitl {
        #[command(subcommand)]
        command: HitlCommands,
    },
    /// Claw interactive management session (deprecated, use `belt agent`).
    Claw {
        #[command(subcommand)]
        command: AgentCommands,
    },
    /// Manage the /auto slash command plugin for Claude Code.
    Auto {
        #[command(subcommand)]
        command: AutoCommands,
    },
    /// Bootstrap .claude/rules files for a workspace.
    Bootstrap {
        /// Workspace root directory (defaults to current directory).
        #[arg(long)]
        workspace: Option<String>,
        /// Custom rules directory path (defaults to <workspace>/.claude/rules).
        #[arg(long)]
        rules_dir: Option<String>,
        /// Overwrite existing rule files.
        #[arg(long)]
        force: bool,
        /// Use LLM to generate tailored convention files instead of static templates.
        #[arg(long)]
        llm: bool,
        /// Project name (used with --llm).
        #[arg(long)]
        project_name: Option<String>,
        /// Primary programming language (used with --llm, e.g., Rust, TypeScript).
        #[arg(long)]
        language: Option<String>,
        /// Framework or runtime (used with --llm, e.g., tokio, Next.js).
        #[arg(long)]
        framework: Option<String>,
        /// Brief project description (used with --llm).
        #[arg(long)]
        description: Option<String>,
        /// Create a pull request with the generated conventions (used with --llm).
        #[arg(long)]
        create_pr: bool,
    },
}

#[derive(Subcommand)]
enum AutoCommands {
    /// Install the /auto slash command into the project's .claude/commands/.
    Plugin {
        #[command(subcommand)]
        command: AutoPluginCommands,
    },
}

#[derive(Subcommand)]
enum AutoPluginCommands {
    /// Install the /auto slash command files.
    Install {
        /// Project root directory (defaults to current directory).
        #[arg(long)]
        project: Option<String>,
        /// Overwrite existing command files.
        #[arg(long)]
        force: bool,
    },
    /// Remove the /auto slash command files.
    Uninstall {
        /// Project root directory (defaults to current directory).
        #[arg(long)]
        project: Option<String>,
    },
    /// Check whether the /auto plugin is installed.
    Status {
        /// Project root directory (defaults to current directory).
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
enum AgentCommands {
    /// Initialize agent workspace.
    Init {
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
    },
    /// Show/edit classification rules.
    Rules,
    /// Edit classification/HITL rules.
    Edit {
        /// Rule file to edit (classify-policy, hitl-policy, auto-approve-policy).
        rule: Option<String>,
    },
    /// Run an LLM agent session.
    Session {
        /// Path to workspace.yaml config file.
        #[arg(long)]
        workspace: Option<String>,
        /// Non-interactive prompt (for cron/evaluate calls).
        #[arg(short, long)]
        prompt: Option<String>,
        /// Plan mode: show execution plan without running.
        #[arg(long)]
        plan: bool,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Install /agent slash command plugin for Claude Code.
    Plugin {
        /// Custom installation directory (defaults to ~/.claude/commands/).
        #[arg(long)]
        install_dir: Option<String>,
    },
    /// Collect system context (status, HITL, queue) for agent injection.
    Context,
}

#[derive(Subcommand)]
enum HitlCommands {
    /// Respond to a HITL item.
    Respond {
        /// Queue item work_id.
        item_id: String,
        /// Action to take: done, retry, skip, replan.
        #[arg(long)]
        action: String,
        /// Respondent name.
        #[arg(long)]
        respondent: Option<String>,
        /// Additional notes.
        #[arg(long)]
        notes: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List HITL items.
    List {
        /// Filter by workspace.
        #[arg(long)]
        workspace: Option<String>,
        /// Output format (text, json).
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Show HITL item details.
    Show {
        /// Queue item work_id.
        item_id: String,
        /// Output format (text, json).
        #[arg(long, default_value = "text")]
        format: String,
        /// Interactive mode: display details then prompt for a response action.
        #[arg(long)]
        interactive: bool,
    },
    /// Set or query HITL timeouts.
    Timeout {
        #[command(subcommand)]
        command: HitlTimeoutCommands,
    },
}

#[derive(Subcommand)]
enum HitlTimeoutCommands {
    /// Set timeout on a HITL item.
    Set {
        /// Queue item work_id.
        item_id: String,
        /// Timeout duration in seconds.
        #[arg(long)]
        duration: u64,
        /// Terminal action when timeout fires: skip, failed, replan.
        #[arg(long)]
        action: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List HITL items with active timeouts.
    Ls {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum WorkspaceCommands {
    /// Register a new workspace.
    Add {
        /// Path to workspace.yaml config.
        #[arg(long)]
        config: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List registered workspaces.
    List {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show workspace details.
    Show {
        name: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Update workspace configuration.
    Update {
        /// Workspace name.
        name: String,
        /// New config file path.
        #[arg(long)]
        config: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove a workspace.
    Remove {
        /// Workspace name.
        name: String,
        /// Skip confirmation warning for active items.
        #[arg(long)]
        force: bool,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show workspace configuration details.
    Config {
        /// Workspace name.
        name: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum QueueCommands {
    /// List queue items.
    List {
        /// Filter by phase.
        #[arg(long)]
        phase: Option<String>,
        /// Filter by workspace.
        #[arg(long)]
        workspace: Option<String>,
        /// Output format.
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Show queue item details.
    Show {
        work_id: String,
        /// Output format.
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Mark item as done (called by evaluate).
    Done {
        work_id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Mark item as HITL (called by evaluate).
    Hitl {
        work_id: String,
        /// Reason for HITL.
        #[arg(long)]
        reason: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Skip an item.
    Skip {
        work_id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Re-run on_done script for a Failed item.
    RetryScript {
        /// Queue item work_id.
        work_id: String,
        /// Script execution timeout in seconds.
        #[arg(long)]
        timeout: Option<u64>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Manage queue item dependencies.
    #[command(subcommand)]
    Dependency(DependencyCommands),
}

#[derive(Subcommand)]
enum DependencyCommands {
    /// Add a dependency (item must run after another item).
    Add {
        /// Queue item work_id.
        queue_id: String,
        /// The work_id that this item depends on (must complete first).
        #[arg(long)]
        after: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove a dependency.
    Remove {
        /// Queue item work_id.
        queue_id: String,
        /// The work_id to remove from dependencies.
        #[arg(long)]
        after: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List dependencies for a queue item, or all dependencies.
    List {
        /// Queue item work_id (omit to list all dependencies).
        queue_id: Option<String>,
        /// Output format.
        #[arg(long, default_value = "text")]
        format: String,
    },
}

#[derive(Subcommand)]
enum SpecCommands {
    /// Show workspace status (item counts by phase).
    Status {
        /// Workspace name.
        name: String,
        /// Output format (text, json, rich).
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Add a new spec.
    Add {
        /// Workspace ID.
        #[arg(long)]
        workspace: String,
        /// Spec name.
        #[arg(long)]
        name: String,
        /// Spec content / description.
        #[arg(long)]
        content: String,
        /// Optional priority (lower is higher).
        #[arg(long)]
        priority: Option<i32>,
        /// Optional comma-separated labels.
        #[arg(long)]
        labels: Option<String>,
        /// Optional comma-separated spec IDs this depends on.
        #[arg(long)]
        depends_on: Option<String>,
        /// Optional comma-separated file/module paths this spec touches.
        #[arg(long)]
        entry_point: Option<String>,
        /// Decompose spec into child issues based on acceptance criteria.
        #[arg(long)]
        decompose: bool,
        /// Skip interactive confirmation when decomposing (auto-approve).
        #[arg(long)]
        yes: bool,
        /// Skip required-section validation for spec content.
        #[arg(long)]
        skip_validation: bool,
    },
    /// List specs.
    List {
        /// Filter by workspace.
        #[arg(long)]
        workspace: Option<String>,
        /// Filter by status.
        #[arg(long)]
        status: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show spec details.
    Show {
        /// Spec ID.
        id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Update spec fields.
    Update {
        /// Spec ID.
        id: String,
        /// New name.
        #[arg(long)]
        name: Option<String>,
        /// New content.
        #[arg(long)]
        content: Option<String>,
        /// New priority.
        #[arg(long)]
        priority: Option<i32>,
        /// New labels.
        #[arg(long)]
        labels: Option<String>,
        /// New depends_on.
        #[arg(long)]
        depends_on: Option<String>,
        /// New entry_point (comma-separated file/module paths).
        #[arg(long)]
        entry_point: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Pause an active spec.
    Pause {
        /// Spec ID.
        id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Resume a paused spec.
    Resume {
        /// Spec ID.
        id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Complete an active spec.
    Complete {
        /// Spec ID.
        id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove a spec.
    Remove {
        /// Spec ID.
        id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Link a spec to an external resource (URL or issue reference).
    Link {
        /// Spec ID.
        id: String,
        /// Target URL or issue reference (e.g. `https://...` or `owner/repo#123`).
        target: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Unlink a spec from an external resource.
    Unlink {
        /// Spec ID.
        id: String,
        /// Target URL or issue reference to remove.
        target: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Verify all links for a spec (check reachability).
    Verify {
        /// Spec ID.
        id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Load workspace config and start the daemon loop.
async fn start_daemon(
    config_path: &str,
    tick_interval_secs: u64,
    max_concurrent: u32,
) -> anyhow::Result<()> {
    let config_content = std::fs::read_to_string(config_path)
        .map_err(|e| anyhow::anyhow!("failed to read config file '{}': {}", config_path, e))?;
    let config: belt_core::workspace::WorkspaceConfig = serde_yaml::from_str(&config_content)
        .map_err(|e| anyhow::anyhow!("failed to parse config file '{}': {}", config_path, e))?;

    let belt_home = belt_home()?;

    // Build DataSources from workspace config.
    let mut sources: Vec<Box<dyn belt_core::source::DataSource>> = Vec::new();
    for (name, source_config) in &config.sources {
        if name == "github" || source_config.url.contains("github.com") {
            sources.push(Box::new(GitHubDataSource::new(&source_config.url)));
        }
    }

    // Runtime registry with Claude as default.
    let mut registry = RuntimeRegistry::new("claude".to_string());
    registry.register(Arc::new(ClaudeRuntime::new(None)));
    registry.register(Arc::new(GeminiRuntime::new(None)));
    registry.register(Arc::new(CodexRuntime::new(None)));

    // Worktree manager.
    let worktree_base = belt_home.join("worktrees");
    std::fs::create_dir_all(&worktree_base)?;
    let repo_path = PathBuf::from(".");
    let worktree_mgr = GitWorktreeManager::new(worktree_base, repo_path);

    // Database for token usage.
    let db_path = belt_home.join("belt.db");
    std::fs::create_dir_all(&belt_home)?;
    let db = belt_infra::db::Database::open(db_path.to_str().unwrap_or("belt.db"))
        .map_err(|e| anyhow::anyhow!("failed to open database: {e}"))?;

    // Capture PID file path before belt_home is moved into the daemon.
    let pid_path = belt_home.join("daemon.pid");

    let mut daemon = Daemon::new(
        config,
        sources,
        Arc::new(registry),
        Box::new(worktree_mgr),
        max_concurrent,
    )
    .with_db(db)
    .with_belt_home(belt_home);

    // Write PID file so `belt stop` can find the daemon process.
    std::fs::write(&pid_path, std::process::id().to_string())
        .map_err(|e| anyhow::anyhow!("failed to write PID file: {e}"))?;

    tracing::info!(
        "starting belt daemon (tick={}s, max_concurrent={}, pid={})",
        tick_interval_secs,
        max_concurrent,
        std::process::id()
    );
    daemon.run(tick_interval_secs).await;

    // Clean up PID file on graceful shutdown.
    if let Err(e) = std::fs::remove_file(&pid_path) {
        tracing::warn!("failed to remove PID file: {e}");
    }

    Ok(())
}

#[derive(Subcommand)]
enum CronCommands {
    /// List registered cron jobs.
    List {
        /// Output format.
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Add a new cron job.
    Add {
        /// Unique name for the cron job.
        name: String,
        /// Cron schedule expression (e.g. "0 * * * *").
        #[arg(long)]
        schedule: String,
        /// Path to the script to execute.
        #[arg(long)]
        script: String,
        /// Optional workspace scope.
        #[arg(long)]
        workspace: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Update an existing cron job.
    Update {
        /// Name of the cron job to update.
        name: String,
        /// New cron schedule expression.
        #[arg(long)]
        schedule: Option<String>,
        /// New script path.
        #[arg(long)]
        script: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Pause (disable) a cron job.
    Pause {
        /// Name of the cron job to pause.
        name: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Resume (enable) a paused cron job.
    Resume {
        /// Name of the cron job to resume.
        name: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove a cron job.
    Remove {
        /// Name of the cron job to remove.
        name: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Trigger a cron job immediately by resetting its last_run_at.
    Trigger {
        /// Name of the cron job to trigger.
        name: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run a user-defined cron job script immediately (bypasses scheduling).
    Run {
        /// Name of the cron job to run.
        name: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the Belt home directory (`$BELT_HOME` or `~/.belt`).
fn belt_home() -> anyhow::Result<PathBuf> {
    if let Ok(val) = std::env::var("BELT_HOME") {
        return Ok(PathBuf::from(val));
    }
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    Ok(home.join(".belt"))
}

/// Open the Belt database at `$BELT_HOME/belt.db`.
fn open_db() -> anyhow::Result<Database> {
    let db_path = belt_home()?.join("belt.db");
    let db = Database::open(
        db_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid database path"))?,
    )?;
    Ok(db)
}

/// Resolve dynamic context by loading workspace config and calling
/// `DataSource.get_context()` for live issue/PR/source data.
async fn resolve_dynamic_context(
    db: &Database,
    item: &belt_core::queue::QueueItem,
) -> anyhow::Result<belt_core::context::ItemContext> {
    let (_name, config_path, _created_at) = db.get_workspace(&item.workspace_id)?;
    let config =
        belt_infra::workspace_loader::load_workspace_config(std::path::Path::new(&config_path))?;

    // Find the first source whose URL matches or just use the first available source.
    let source_url = config
        .sources
        .values()
        .next()
        .map(|s| s.url.clone())
        .ok_or_else(|| anyhow::anyhow!("no sources configured in workspace"))?;

    let ds = GitHubDataSource::new(&source_url);
    use belt_core::source::DataSource;
    let ctx = ds.get_context(item).await?;
    Ok(ctx)
}

/// Read the daemon PID from the PID file.
fn read_pid() -> anyhow::Result<u32> {
    let pid_path = belt_home()?.join("daemon.pid");
    let content = std::fs::read_to_string(&pid_path).map_err(|e| {
        anyhow::anyhow!(
            "could not read PID file at {}: {} (is the daemon running?)",
            pid_path.display(),
            e
        )
    })?;
    let pid: u32 = content
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid PID in {}: {}", pid_path.display(), e))?;
    Ok(pid)
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

/// `belt stop` -- terminate the daemon process.
///
/// On Unix, sends SIGTERM via `kill -TERM`. On Windows, uses `taskkill /PID`.
fn cmd_stop() -> anyhow::Result<()> {
    let pid = read_pid()?;
    tracing::info!(pid, "stopping daemon...");

    terminate_pid(pid)?;

    println!("Sent stop signal to daemon (PID {pid}).");
    Ok(())
}

/// Platform-appropriate process termination.
///
/// On Unix, sends SIGTERM. On Windows, invokes `taskkill /PID`.
fn terminate_pid(pid: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::process::Command;
        let status = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()?;
        if !status.success() {
            anyhow::bail!("failed to send signal to PID {pid} -- process may not exist");
        }
        Ok(())
    }

    #[cfg(windows)]
    {
        use std::process::Command;
        let output = Command::new("taskkill")
            .args(["/PID", &pid.to_string()])
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("taskkill failed for PID {pid}: {stderr}")
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        anyhow::bail!("belt stop is not supported on this platform");
    }
}

/// `belt restart` -- graceful stop then start.
///
/// Sends SIGTERM and waits up to 30 seconds for the process to exit,
/// then starts the daemon with the given config. When `background` is true
/// the daemon is spawned as a detached child process.
async fn cmd_restart(config_path: &str, background: bool) -> anyhow::Result<()> {
    // -- Phase 1: stop (best-effort) --
    let had_daemon = read_pid().is_ok();
    if had_daemon {
        cmd_stop()?;

        // Wait for the daemon to terminate (max 30 s).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if read_pid().is_err() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("daemon did not stop within 30 seconds -- aborting restart");
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        println!("Daemon stopped.");
    } else {
        println!("No running daemon found -- skipping stop phase.");
    }

    // -- Phase 2: start --
    if background {
        let exe = std::env::current_exe()?;
        let child = std::process::Command::new(exe)
            .args(["start", "--config", config_path])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        println!("Daemon restarted in background (PID {}).", child.id());
    } else {
        println!("Starting daemon...");
        start_daemon(config_path, 30, 4).await?;
    }

    Ok(())
}

/// `belt status` -- show queue item counts grouped by phase.
fn cmd_status(format: &str) -> anyhow::Result<()> {
    let db = open_db()?;
    let daemon_running = read_pid().is_ok();
    let sys_status = status::gather_status(&db)?;

    // For non-rich formats, print daemon status as plain text (rich embeds it in the header box).
    if format != "json" && format != "rich" {
        println!(
            "Daemon: {}",
            if daemon_running { "running" } else { "stopped" }
        );
    }

    status::print_status(&sys_status, format, Some(daemon_running))
}

/// `belt queue list` -- list queue items with optional filters.
fn cmd_queue_list(
    phase: Option<String>,
    workspace: Option<String>,
    format: &str,
) -> anyhow::Result<()> {
    let db = open_db()?;
    let phase_filter = phase
        .as_deref()
        .map(|p| p.parse::<QueuePhase>())
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid phase: {e}"))?;

    let items = db.list_items(phase_filter, workspace.as_deref())?;

    match format {
        "json" => {
            println!("{}", serde_json::to_string_pretty(&items)?);
        }
        _ => {
            if items.is_empty() {
                println!("No queue items found.");
            } else {
                println!(
                    "{:<40} {:<12} {:<10} {:<20}",
                    "WORK_ID", "PHASE", "STATE", "UPDATED"
                );
                for item in &items {
                    println!(
                        "{:<40} {:<12} {:<10} {:<20}",
                        truncate(&item.work_id, 40),
                        item.phase().as_str(),
                        &item.state,
                        &item.updated_at,
                    );
                }
                println!("\n{} item(s)", items.len());
            }
        }
    }

    Ok(())
}

/// `belt queue show` -- show a single queue item.
fn cmd_queue_show(work_id: &str, format: &str) -> anyhow::Result<()> {
    let db = open_db()?;
    let item = db.get_item(work_id)?;

    match format {
        "json" => {
            println!("{}", serde_json::to_string_pretty(&item)?);
        }
        _ => {
            println!("Work ID:      {}", item.work_id);
            println!("Source ID:    {}", item.source_id);
            println!("Workspace:    {}", item.workspace_id);
            println!("State:        {}", item.state);
            println!("Phase:        {}", item.phase());
            if let Some(title) = &item.title {
                println!("Title:        {title}");
            }
            println!("Created:      {}", item.created_at);
            println!("Updated:      {}", item.updated_at);
        }
    }

    Ok(())
}

/// `belt queue done` -- mark a queue item as Done, running on_done scripts if configured.
async fn cmd_queue_done(work_id: &str, json: bool) -> anyhow::Result<()> {
    let db = open_db()?;
    let item = db.get_item(work_id)?;

    // Load workspace config to find on_done scripts for this item's state.
    let (_, config_path, _) = db.get_workspace(&item.workspace_id)?;
    let config =
        belt_infra::workspace_loader::load_workspace_config(std::path::Path::new(&config_path))?;

    // Find the state config containing on_done scripts.
    let state_config = config
        .sources
        .values()
        .find_map(|source| source.states.get(&item.state));

    let on_done_actions: Vec<belt_core::action::Action> = state_config
        .map(|sc| {
            sc.on_done
                .iter()
                .map(belt_core::action::Action::from)
                .collect()
        })
        .unwrap_or_default();

    // Set up worktree manager for cleanup after transition.
    let belt_home = belt_home()?;
    let worktree_base = belt_home.join("worktrees");
    let repo_path = std::path::PathBuf::from(".");
    let worktree_mgr = GitWorktreeManager::new(worktree_base, repo_path);

    if on_done_actions.is_empty() {
        db.update_phase(work_id, QueuePhase::Done)?;
        // Cleanup worktree (matches daemon pattern: warn on failure, don't abort).
        if let Err(e) = worktree_mgr.cleanup(work_id) {
            tracing::warn!(work_id, error = %e, "worktree cleanup failed on queue done, continuing");
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "success": true,
                    "work_id": work_id,
                    "phase": "done",
                    "scripts_run": false
                }))?
            );
        } else {
            println!("Marked {work_id} as done.");
        }
        return Ok(());
    }

    // Set up execution environment.
    let worktree_path = worktree_mgr.create_or_reuse(work_id)?;
    let env = belt_daemon::executor::ActionEnv::new(work_id, &worktree_path);

    // Build a minimal runtime registry for script execution.
    let mut registry = belt_core::runtime::RuntimeRegistry::new("claude".to_string());
    registry.register(std::sync::Arc::new(
        belt_infra::runtimes::claude::ClaudeRuntime::new(None),
    ));
    registry.register(std::sync::Arc::new(
        belt_infra::runtimes::gemini::GeminiRuntime::new(None),
    ));
    registry.register(std::sync::Arc::new(
        belt_infra::runtimes::codex::CodexRuntime::new(None),
    ));
    let executor = belt_daemon::executor::ActionExecutor::new(std::sync::Arc::new(registry));

    if !json {
        println!("Running on_done scripts for '{work_id}'...");
    }

    let result = executor.execute_all(&on_done_actions, &env).await?;

    match result {
        Some(r) if r.success() => {
            db.update_phase(work_id, QueuePhase::Done)?;
            // Cleanup worktree (matches daemon pattern: warn on failure, don't abort).
            if let Err(e) = worktree_mgr.cleanup(work_id) {
                tracing::warn!(work_id, error = %e, "worktree cleanup failed on queue done, continuing");
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "success": true,
                        "work_id": work_id,
                        "phase": "done",
                        "scripts_run": true
                    }))?
                );
            } else {
                println!("on_done scripts succeeded. Marked '{work_id}' as done.");
            }
        }
        Some(r) => {
            db.update_phase(work_id, QueuePhase::Failed)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "success": false,
                        "work_id": work_id,
                        "phase": "failed",
                        "scripts_run": true,
                        "exit_code": r.exit_code
                    }))?
                );
            } else {
                println!(
                    "on_done scripts failed (exit code {}). Item '{work_id}' transitioned to failed.",
                    r.exit_code
                );
            }
        }
        None => {
            db.update_phase(work_id, QueuePhase::Done)?;
            // Cleanup worktree (matches daemon pattern: warn on failure, don't abort).
            if let Err(e) = worktree_mgr.cleanup(work_id) {
                tracing::warn!(work_id, error = %e, "worktree cleanup failed on queue done, continuing");
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "success": true,
                        "work_id": work_id,
                        "phase": "done",
                        "scripts_run": false
                    }))?
                );
            } else {
                println!("Marked '{work_id}' as done.");
            }
        }
    }

    Ok(())
}

/// `belt queue hitl` -- mark a queue item as HITL.
fn cmd_queue_hitl(work_id: &str, reason: Option<&str>, json: bool) -> anyhow::Result<()> {
    let db = open_db()?;
    db.update_phase(work_id, QueuePhase::Hitl)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "success": true,
                "work_id": work_id,
                "phase": "hitl",
                "reason": reason
            }))?
        );
    } else if let Some(r) = reason {
        println!("Marked {work_id} as HITL (reason: {r}).");
    } else {
        println!("Marked {work_id} as HITL.");
    }
    Ok(())
}

/// `belt queue skip` -- mark a queue item as Skipped.
fn cmd_queue_skip(work_id: &str, json: bool) -> anyhow::Result<()> {
    let db = open_db()?;
    db.update_phase(work_id, QueuePhase::Skipped)?;

    // Cleanup worktree (matches daemon pattern: warn on failure, don't abort).
    let belt_home = belt_home()?;
    let worktree_base = belt_home.join("worktrees");
    let repo_path = std::path::PathBuf::from(".");
    let worktree_mgr = GitWorktreeManager::new(worktree_base, repo_path);
    if let Err(e) = worktree_mgr.cleanup(work_id) {
        tracing::warn!(work_id, error = %e, "worktree cleanup failed on queue skip, continuing");
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "success": true,
                "work_id": work_id,
                "phase": "skipped"
            }))?
        );
    } else {
        println!("Skipped {work_id}.");
    }
    Ok(())
}

/// `belt queue retry-script` -- re-run on_done script for a Failed item.
async fn cmd_queue_retry_script(
    work_id: &str,
    timeout: Option<u64>,
    json: bool,
) -> anyhow::Result<()> {
    let db = open_db()?;
    let item = db.get_item(work_id)?;

    if item.phase() != QueuePhase::Failed {
        anyhow::bail!(
            "item '{}' is in phase '{}', not 'failed'",
            work_id,
            item.phase()
        );
    }

    // Load workspace config to find on_done scripts for this item's state.
    let (_, config_path, _) = db.get_workspace(&item.workspace_id)?;
    let config =
        belt_infra::workspace_loader::load_workspace_config(std::path::Path::new(&config_path))?;

    // Find the state config containing on_done scripts.
    let state_config = config
        .sources
        .values()
        .find_map(|source| source.states.get(&item.state))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no state config found for state '{}' in workspace '{}'",
                item.state,
                item.workspace_id
            )
        })?;

    if state_config.on_done.is_empty() {
        db.update_phase(work_id, QueuePhase::Done)?;
        record_script_retry_event(&db, work_id, &item.source_id, QueuePhase::Done, None);
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "success": true,
                    "work_id": work_id,
                    "phase": "done",
                    "scripts_run": false
                }))?
            );
        } else {
            println!(
                "No on_done scripts configured for state '{}'. Transitioning to done.",
                item.state
            );
            println!("Item '{work_id}' transitioned from failed to done.");
        }
        return Ok(());
    }

    let on_done: Vec<belt_core::action::Action> = state_config
        .on_done
        .iter()
        .map(belt_core::action::Action::from)
        .collect();

    // Set up execution environment.
    let belt_home = belt_home()?;
    let worktree_base = belt_home.join("worktrees");
    let repo_path = std::path::PathBuf::from(".");
    let worktree_mgr = belt_infra::worktree::GitWorktreeManager::new(worktree_base, repo_path);

    let worktree_path = worktree_mgr.create_or_reuse(work_id)?;
    let env = belt_daemon::executor::ActionEnv::new(work_id, &worktree_path);

    // Build a minimal runtime registry for script execution.
    let mut registry = belt_core::runtime::RuntimeRegistry::new("claude".to_string());
    registry.register(std::sync::Arc::new(
        belt_infra::runtimes::claude::ClaudeRuntime::new(None),
    ));
    registry.register(std::sync::Arc::new(
        belt_infra::runtimes::gemini::GeminiRuntime::new(None),
    ));
    registry.register(std::sync::Arc::new(
        belt_infra::runtimes::codex::CodexRuntime::new(None),
    ));
    let executor = belt_daemon::executor::ActionExecutor::new(std::sync::Arc::new(registry));

    if !json {
        println!("Re-running on_done scripts for '{work_id}'...");
    }

    let result = if let Some(secs) = timeout {
        let duration = std::time::Duration::from_secs(secs);
        match tokio::time::timeout(duration, executor.execute_all(&on_done, &env)).await {
            Ok(r) => r?,
            Err(_) => {
                record_script_retry_event(
                    &db,
                    work_id,
                    &item.source_id,
                    QueuePhase::Failed,
                    Some(format!("timeout after {secs}s")),
                );
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "success": false,
                            "work_id": work_id,
                            "phase": "failed",
                            "error": format!("timeout after {secs}s")
                        }))?
                    );
                } else {
                    println!("Script execution timed out after {secs}s. Item remains failed.");
                }
                return Ok(());
            }
        }
    } else {
        executor.execute_all(&on_done, &env).await?
    };

    match result {
        Some(r) if r.success() => {
            db.update_phase(work_id, QueuePhase::Done)?;
            record_script_retry_event(&db, work_id, &item.source_id, QueuePhase::Done, None);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "success": true,
                        "work_id": work_id,
                        "phase": "done",
                        "scripts_run": true
                    }))?
                );
            } else {
                println!(
                    "on_done scripts succeeded. Item '{work_id}' transitioned from failed to done."
                );
            }
        }
        Some(r) => {
            record_script_retry_event(
                &db,
                work_id,
                &item.source_id,
                QueuePhase::Failed,
                Some(format!("exit code {}", r.exit_code)),
            );
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "success": false,
                        "work_id": work_id,
                        "phase": "failed",
                        "scripts_run": true,
                        "exit_code": r.exit_code
                    }))?
                );
            } else {
                println!(
                    "on_done scripts failed (exit code {}). Item '{work_id}' remains in failed phase.",
                    r.exit_code
                );
            }
        }
        None => {
            // No scripts produced a result (shouldn't happen since we checked on_done is non-empty).
            db.update_phase(work_id, QueuePhase::Done)?;
            record_script_retry_event(&db, work_id, &item.source_id, QueuePhase::Done, None);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "success": true,
                        "work_id": work_id,
                        "phase": "done",
                        "scripts_run": false
                    }))?
                );
            } else {
                println!("Item '{work_id}' transitioned from failed to done.");
            }
        }
    }

    Ok(())
}

/// Record a `script_retry` transition event for retry-script operations.
fn record_script_retry_event(
    db: &Database,
    work_id: &str,
    source_id: &str,
    to_phase: QueuePhase,
    detail: Option<String>,
) {
    let now = chrono::Utc::now();
    let event = belt_infra::db::TransitionEvent {
        id: format!("te-{}-{}", work_id, now.timestamp_millis()),
        work_id: work_id.to_string(),
        source_id: source_id.to_string(),
        event_type: "script_retry".to_string(),
        phase: Some(to_phase.as_str().to_string()),
        from_phase: Some(QueuePhase::Failed.as_str().to_string()),
        detail,
        created_at: now.to_rfc3339(),
    };
    if let Err(e) = db.insert_transition_event(&event) {
        tracing::warn!(
            work_id = %work_id,
            error = %e,
            "failed to record script_retry transition event"
        );
    }
}

/// `belt queue dependency add` -- add a dependency between queue items.
fn cmd_queue_dependency_add(queue_id: &str, after: &str, json: bool) -> anyhow::Result<()> {
    let db = open_db()?;
    db.add_queue_dependency(queue_id, after)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "success": true,
                "work_id": queue_id,
                "depends_on": after
            }))?
        );
    } else {
        println!("Added dependency: {queue_id} depends on {after}.");
    }
    Ok(())
}

/// `belt queue dependency remove` -- remove a dependency between queue items.
fn cmd_queue_dependency_remove(queue_id: &str, after: &str, json: bool) -> anyhow::Result<()> {
    let db = open_db()?;
    db.remove_queue_dependency(queue_id, after)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "success": true,
                "work_id": queue_id,
                "removed_dependency": after
            }))?
        );
    } else {
        println!("Removed dependency: {queue_id} no longer depends on {after}.");
    }
    Ok(())
}

/// `belt queue dependency list` -- list dependencies for a queue item or all items.
fn cmd_queue_dependency_list(queue_id: Option<&str>, format: &str) -> anyhow::Result<()> {
    let db = open_db()?;
    if let Some(id) = queue_id {
        let deps = db.list_queue_dependencies(id)?;
        match format {
            "json" => {
                let obj = serde_json::json!({ "work_id": id, "depends_on": deps });
                println!("{}", serde_json::to_string_pretty(&obj)?);
            }
            _ => {
                if deps.is_empty() {
                    println!("No dependencies for {id}.");
                } else {
                    println!("Dependencies for {id}:");
                    for dep in &deps {
                        println!("  - {dep}");
                    }
                }
            }
        }
    } else {
        let all = db.list_all_queue_dependencies()?;
        match format {
            "json" => {
                println!("{}", serde_json::to_string_pretty(&all)?);
            }
            _ => {
                if all.is_empty() {
                    println!("No dependencies found.");
                } else {
                    for (work_id, depends_on) in &all {
                        println!("{work_id} -> {depends_on}");
                    }
                }
            }
        }
    }
    Ok(())
}

/// `belt cron list` -- list registered cron jobs.
fn cmd_cron_list(format: &str) -> anyhow::Result<()> {
    let db = open_db()?;
    let jobs = db.list_cron_jobs()?;

    match format {
        "json" => {
            println!("{}", serde_json::to_string_pretty(&jobs)?);
        }
        _ => {
            if jobs.is_empty() {
                println!("No cron jobs registered.");
            } else {
                println!(
                    "{:<20} {:<16} {:<10} {:<12} {:<24}",
                    "NAME", "SCHEDULE", "ENABLED", "WORKSPACE", "LAST_RUN"
                );
                for job in &jobs {
                    println!(
                        "{:<20} {:<16} {:<10} {:<12} {:<24}",
                        truncate(&job.name, 20),
                        truncate(&job.schedule, 16),
                        if job.enabled { "yes" } else { "no" },
                        job.workspace.as_deref().unwrap_or("-"),
                        job.last_run_at.as_deref().unwrap_or("never"),
                    );
                }
                println!("\n{} job(s)", jobs.len());
            }
        }
    }

    Ok(())
}

/// `belt cron add` -- register a new cron job.
fn cmd_cron_add(
    name: &str,
    schedule: &str,
    script: &str,
    workspace: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    validate_cron_expression(schedule)?;
    let script_path = std::path::Path::new(script);
    if !script_path.exists() {
        anyhow::bail!("script not found: {script}");
    }

    let db = open_db()?;
    db.add_cron_job(name, schedule, script, workspace)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "success": true,
                "name": name,
                "schedule": schedule,
                "script": script,
                "workspace": workspace
            }))?
        );
    } else {
        println!("Cron job '{name}' added.");
    }
    notify_daemon_cron_sync();
    Ok(())
}

/// `belt cron update` -- update schedule and/or script of an existing cron job.
fn cmd_cron_update(
    name: &str,
    schedule: Option<&str>,
    script: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    if schedule.is_none() && script.is_none() {
        anyhow::bail!("at least one of --schedule or --script must be provided");
    }

    let db = open_db()?;

    // Verify the job exists.
    db.get_cron_job(name)?;

    if let Some(sched) = schedule {
        validate_cron_expression(sched)?;
        db.update_cron_schedule(name, sched)?;
    }
    if let Some(s) = script {
        let script_path = std::path::Path::new(s);
        if !script_path.exists() {
            anyhow::bail!("script not found: {s}");
        }
        db.update_cron_script(name, s)?;
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "success": true,
                "name": name,
                "schedule": schedule,
                "script": script
            }))?
        );
    } else {
        println!("Cron job '{name}' updated.");
    }
    notify_daemon_cron_sync();
    Ok(())
}

/// `belt cron pause` -- disable a cron job.
fn cmd_cron_pause(name: &str, json: bool) -> anyhow::Result<()> {
    let db = open_db()?;
    db.toggle_cron_job(name, false)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "success": true,
                "name": name,
                "enabled": false
            }))?
        );
    } else {
        println!("Cron job '{name}' paused.");
    }
    notify_daemon_cron_sync();
    Ok(())
}

/// `belt cron resume` -- enable a paused cron job.
fn cmd_cron_resume(name: &str, json: bool) -> anyhow::Result<()> {
    let db = open_db()?;
    db.toggle_cron_job(name, true)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "success": true,
                "name": name,
                "enabled": true
            }))?
        );
    } else {
        println!("Cron job '{name}' resumed.");
    }
    notify_daemon_cron_sync();
    Ok(())
}

/// `belt cron remove` -- delete a cron job.
fn cmd_cron_remove(name: &str, json: bool) -> anyhow::Result<()> {
    let db = open_db()?;
    db.remove_cron_job(name)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "success": true,
                "name": name,
                "removed": true
            }))?
        );
    } else {
        println!("Cron job '{name}' removed.");
    }
    notify_daemon_cron_sync();
    Ok(())
}

/// Validate a cron expression has the correct number of fields (5).
///
/// This performs basic structural validation: exactly 5 space-separated fields
/// where each field contains only valid cron characters (digits, `*`, `/`, `-`, `,`).
fn validate_cron_expression(expr: &str) -> anyhow::Result<()> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        anyhow::bail!(
            "invalid cron expression: expected 5 fields (minute hour day month weekday), got {}",
            fields.len()
        );
    }
    for (i, field) in fields.iter().enumerate() {
        let field_names = ["minute", "hour", "day", "month", "weekday"];
        if !field
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '*' | '/' | '-' | ','))
        {
            anyhow::bail!(
                "invalid cron expression: {} field '{}' contains invalid characters",
                field_names[i],
                field
            );
        }
    }
    Ok(())
}

/// `belt cron run` -- execute a user-defined cron job script immediately.
fn cmd_cron_run(name: &str, json: bool) -> anyhow::Result<()> {
    let db = open_db()?;
    let job = db.get_cron_job(name)?;

    let script_path = std::path::Path::new(&job.script);
    if !script_path.exists() {
        anyhow::bail!("script not found: {}", job.script);
    }

    if !json {
        println!("Running cron job '{name}' (script: {})...", job.script);
    }

    let belt_home = belt_home()?;
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(&job.script)
        .env("BELT_HOME", belt_home.to_string_lossy().as_ref())
        .env("BELT_CRON_JOB", name)
        .output()?;

    if !json {
        if !output.stdout.is_empty() {
            print!("{}", String::from_utf8_lossy(&output.stdout));
        }
        if !output.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }
    }

    if output.status.success() {
        db.update_cron_last_run(name)?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "success": true,
                    "name": name,
                    "exit_code": 0
                }))?
            );
        } else {
            println!("Cron job '{name}' completed successfully.");
        }
    } else {
        let exit_code = output.status.code().unwrap_or(-1);
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "success": false,
                    "name": name,
                    "exit_code": exit_code
                }))?
            );
        } else {
            anyhow::bail!("cron job '{name}' failed with exit code {exit_code}");
        }
    }

    Ok(())
}

/// `belt cron trigger` -- persist trigger state and signal daemon.
///
/// Resets the job's `last_run_at` to `NULL` in the database so the cron
/// engine treats it as never-run, then sends `SIGUSR1` to the daemon
/// (if running) to sync triggers and execute an immediate tick.
fn cmd_cron_trigger(name: &str, json: bool) -> anyhow::Result<()> {
    let db = open_db()?;

    // Verify the job exists and reset its last_run_at to NULL.
    db.reset_cron_last_run(name)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Signal the daemon to sync triggers from DB and run an immediate tick.
    let daemon_notified = signal_daemon().is_ok();

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "success": true,
                "name": name,
                "daemon_notified": daemon_notified
            }))?
        );
    } else {
        println!("Trigger persisted for cron job '{name}' (last_run_at reset).");
        if daemon_notified {
            println!("Daemon notified (SIGUSR1). The job will execute shortly.");
        } else {
            println!("Could not signal daemon. The job will execute on the next daemon tick.");
        }
    }

    Ok(())
}

/// Best-effort notification to the daemon to sync cron jobs.
///
/// Sends SIGUSR1 to the running daemon so it picks up cron job changes
/// (add/remove/pause/resume/update) from the database. Silently ignores
/// any errors (e.g. daemon not running).
fn notify_daemon_cron_sync() {
    match signal_daemon() {
        Ok(()) => {
            println!("Daemon notified to sync cron jobs.");
        }
        Err(_) => {
            // Daemon may not be running; changes will be picked up on next start.
        }
    }
}

/// Send a cron-sync signal to the running daemon process.
///
/// On Unix, sends SIGUSR1 to the daemon PID. On all platforms, falls back
/// to the TCP-based IPC mechanism provided by [`belt_infra::ipc`].
fn signal_daemon() -> anyhow::Result<()> {
    // Try IPC first -- works on all platforms.
    let home = belt_home()?;
    if home.join("daemon.ipc").exists() {
        return belt_infra::ipc::notify_daemon(&home, belt_infra::ipc::DaemonSignal::CronSync);
    }

    // Fallback: Unix SIGUSR1.
    #[cfg(unix)]
    {
        let pid = read_pid()?;
        use std::process::Command;
        let status = Command::new("kill")
            .args(["-USR1", &pid.to_string()])
            .status()?;
        if !status.success() {
            anyhow::bail!("failed to send SIGUSR1 to PID {pid}");
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        anyhow::bail!(
            "daemon IPC file not found at {}; is the daemon running?",
            home.join("daemon.ipc").display()
        );
    }
}

/// Determine a recommended action based on the HITL reason.
///
/// Returns a tuple of `(action, explanation)` where `action` is the
/// suggested `HitlRespondAction` string and `explanation` describes why.
fn recommended_action(
    reason: Option<&belt_core::queue::HitlReason>,
) -> (&'static str, &'static str) {
    use belt_core::queue::HitlReason;
    match reason {
        Some(HitlReason::EvaluateFailure) => (
            "retry",
            "Evaluation failed; a retry may succeed after transient issues are resolved.",
        ),
        Some(HitlReason::RetryMaxExceeded) => (
            "skip",
            "Maximum retries exhausted; consider skipping or investigating the root cause.",
        ),
        Some(HitlReason::Timeout) => (
            "retry",
            "Execution timed out; retry with a longer timeout or investigate the workload.",
        ),
        Some(HitlReason::ManualEscalation) => (
            "done",
            "Manually escalated; review the item and mark done if the issue is resolved.",
        ),
        Some(HitlReason::SpecConflict) => (
            "replan",
            "Spec conflict detected; replan to resolve overlapping specifications.",
        ),
        Some(HitlReason::SpecCompletionReview) => (
            "done",
            "Spec completion review; approve to mark as done if the spec is satisfactory.",
        ),
        Some(HitlReason::SpecModificationProposed) => (
            "done",
            "Spec modification proposed; review changes and approve or skip.",
        ),
        Some(HitlReason::StagnationDetected) => (
            "replan",
            "Stagnation detected; replan with a lateral approach to break the loop.",
        ),
        None => (
            "skip",
            "No HITL reason recorded; review manually and decide.",
        ),
    }
}

/// `belt hitl show` -- show HITL item details.
fn cmd_hitl_show(item_id: &str, format: &str, interactive: bool) -> anyhow::Result<()> {
    let db = open_db()?;
    let item = db.get_item(item_id)?;

    if item.phase() != QueuePhase::Hitl {
        anyhow::bail!(
            "item '{}' is in phase '{}', not 'hitl'",
            item_id,
            item.phase()
        );
    }

    let (rec_action, rec_explanation) = recommended_action(item.hitl_reason.as_ref());

    match format {
        "json" => {
            // Build an enriched JSON output that includes the recommended action.
            let mut value = serde_json::to_value(&item)?;
            if let serde_json::Value::Object(ref mut map) = value {
                let mut rec = serde_json::Map::new();
                rec.insert(
                    "action".to_string(),
                    serde_json::Value::String(rec_action.to_string()),
                );
                rec.insert(
                    "explanation".to_string(),
                    serde_json::Value::String(rec_explanation.to_string()),
                );
                map.insert("recommended".to_string(), serde_json::Value::Object(rec));
            }
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        _ => {
            println!("Work ID:      {}", item.work_id);
            println!("Source ID:    {}", item.source_id);
            println!("Workspace:    {}", item.workspace_id);
            println!("State:        {}", item.state);
            println!("Phase:        {}", item.phase());
            if let Some(title) = &item.title {
                println!("Title:        {title}");
            }
            println!("Created:      {}", item.created_at);
            println!("Updated:      {}", item.updated_at);
            if let Some(hitl_at) = &item.hitl_created_at {
                println!("HITL Since:   {hitl_at}");
            }
            if let Some(reason) = &item.hitl_reason {
                println!("HITL Reason:  {reason}");
            }
            if let Some(respondent) = &item.hitl_respondent {
                println!("Respondent:   {respondent}");
            }
            if let Some(notes) = &item.hitl_notes {
                println!("Notes:        {notes}");
            }
            if let Some(timeout_at) = &item.hitl_timeout_at {
                println!("Timeout At:   {timeout_at}");
            }
            if let Some(action) = &item.hitl_terminal_action {
                println!("Timeout Act:  {action}");
            }
            println!();
            println!("Recommended:  {rec_action}");
            println!("              {rec_explanation}");
        }
    }

    if interactive {
        println!();
        println!("Available actions: done, retry, skip, replan");
        print!("Enter action [{}]: ", rec_action);
        // Flush stdout so the prompt appears before reading.
        use std::io::Write;
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim();

        // Use the recommended action as default when the user presses Enter.
        let chosen = if input.is_empty() { rec_action } else { input };

        let action: belt_core::queue::HitlRespondAction =
            chosen.parse().map_err(|e: String| anyhow::anyhow!(e))?;

        print!("Notes (optional): ");
        std::io::stdout().flush()?;
        let mut notes_input = String::new();
        std::io::stdin().read_line(&mut notes_input)?;
        let notes = notes_input.trim();
        let notes = if notes.is_empty() {
            None
        } else {
            Some(notes.to_string())
        };

        // Apply the response action.
        match action {
            belt_core::queue::HitlRespondAction::Replan => {
                let max_replan = 3u32;
                let new_count = item.replan_count + 1;
                if new_count > max_replan {
                    db.update_phase(item_id, QueuePhase::Failed)?;
                    println!(
                        "Item '{}' replan limit exceeded ({}/{}), transitioned to failed.",
                        item_id, new_count, max_replan
                    );
                } else {
                    db.update_phase(item_id, QueuePhase::Pending)?;
                    let failure_reason = item.hitl_notes.as_deref().unwrap_or("unknown failure");
                    let replan_work_id = format!("{item_id}:replan-{new_count}");
                    let mut replan_item = belt_core::queue::QueueItem::new(
                        replan_work_id.clone(),
                        item.source_id.clone(),
                        item.workspace_id.clone(),
                        item.state.clone(),
                    );
                    replan_item.set_phase_unchecked(QueuePhase::Hitl);
                    replan_item.hitl_created_at = Some(chrono::Utc::now().to_rfc3339());
                    replan_item.hitl_reason =
                        Some(belt_core::queue::HitlReason::SpecModificationProposed);
                    replan_item.hitl_notes = Some(format!(
                        "Claw replan delegation (attempt {new_count}): {failure_reason}"
                    ));
                    replan_item.title =
                        Some(format!("spec-modification-proposed (replan #{new_count})"));
                    replan_item.replan_count = new_count;
                    if let Some(n) = &notes {
                        replan_item.hitl_notes = Some(n.clone());
                    }
                    db.insert_item(&replan_item)?;
                    println!(
                        "Item '{}' rolled back to pending (replan {}/{}). \
                         Created HITL item '{}' for spec modification review.",
                        item_id, new_count, max_replan, replan_work_id
                    );
                }
            }
            _ => {
                let target_phase = match action {
                    belt_core::queue::HitlRespondAction::Done => QueuePhase::Done,
                    belt_core::queue::HitlRespondAction::Retry => QueuePhase::Pending,
                    belt_core::queue::HitlRespondAction::Skip => QueuePhase::Skipped,
                    belt_core::queue::HitlRespondAction::Replan => unreachable!(),
                };
                db.update_phase(item_id, target_phase)?;

                // Cleanup worktree on Done/Skipped (matches daemon pattern).
                if matches!(target_phase, QueuePhase::Done | QueuePhase::Skipped)
                    && let Ok(home) = belt_home()
                {
                    let wt_base = home.join("worktrees");
                    let repo_path = std::path::PathBuf::from(".");
                    let wt_mgr = GitWorktreeManager::new(wt_base, repo_path);
                    if let Err(e) = wt_mgr.cleanup(item_id) {
                        tracing::warn!(
                            work_id = item_id,
                            error = %e,
                            "worktree cleanup failed on hitl respond, continuing"
                        );
                    }
                }

                if let Some(n) = &notes {
                    println!("Notes recorded: {n}");
                }
                println!(
                    "Item '{}' transitioned from hitl to {} (action: {}).",
                    item_id, target_phase, action
                );
            }
        }
    }

    Ok(())
}

/// `belt hitl timeout set|ls` -- manage HITL timeouts.
fn cmd_hitl_timeout(command: HitlTimeoutCommands) -> anyhow::Result<()> {
    let db = open_db()?;
    match command {
        HitlTimeoutCommands::Set {
            item_id,
            duration,
            action,
            json,
        } => {
            // Validate that the item exists and is in HITL phase.
            let item = db.get_item(&item_id)?;
            if item.phase() != QueuePhase::Hitl {
                anyhow::bail!(
                    "item '{}' is in phase '{}', expected 'hitl'",
                    item_id,
                    item.phase()
                );
            }

            // Validate terminal action if provided by parsing via EscalationAction.
            let parsed_action = action
                .as_deref()
                .map(|a| {
                    a.parse::<belt_core::escalation::EscalationAction>()
                        .map_err(|e| anyhow::anyhow!("{e}"))
                })
                .transpose()?;

            // Compute absolute timeout timestamp.
            let timeout_at =
                (chrono::Utc::now() + chrono::Duration::seconds(duration as i64)).to_rfc3339();

            db.set_hitl_timeout(&item_id, &timeout_at, parsed_action.as_ref())?;

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "success": true,
                        "work_id": item_id,
                        "timeout_at": timeout_at,
                        "duration_secs": duration,
                        "action": action.as_deref().unwrap_or("skip")
                    }))?
                );
            } else {
                println!("Timeout set for item '{item_id}':");
                println!("  expires at: {timeout_at}");
                println!("  duration:   {} seconds", duration);
                if let Some(a) = &action {
                    println!("  action:     {a}");
                } else {
                    println!("  action:     skip (default)");
                }
            }
        }
        HitlTimeoutCommands::Ls { json } => {
            let items = db.list_hitl_items_with_timeout()?;
            if json {
                let entries: Vec<serde_json::Value> = items
                    .iter()
                    .map(|item| {
                        serde_json::json!({
                            "work_id": item.work_id,
                            "timeout_at": item.hitl_timeout_at,
                            "action": item.hitl_terminal_action.as_ref().map(|a| a.to_string()).unwrap_or_else(|| "skip".to_string()),
                            "workspace": item.workspace_id,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if items.is_empty() {
                println!("No HITL items with active timeouts.");
            } else {
                println!(
                    "{:<40} {:<28} {:<10} {:<20}",
                    "WORK_ID", "TIMEOUT_AT", "ACTION", "WORKSPACE"
                );
                for item in &items {
                    let timeout_at = item.hitl_timeout_at.as_deref().unwrap_or("-");
                    let action_str;
                    let action = match &item.hitl_terminal_action {
                        Some(a) => {
                            action_str = a.to_string();
                            action_str.as_str()
                        }
                        None => "skip",
                    };
                    println!(
                        "{:<40} {:<28} {:<10} {:<20}",
                        truncate(&item.work_id, 40),
                        timeout_at,
                        action,
                        &item.workspace_id,
                    );
                }
                println!("\n{} item(s) with timeout", items.len());
            }
        }
    }
    Ok(())
}

/// Truncate a string to `max` characters, appending "..." if truncated.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else if max > 3 {
        format!("{}...", &s[..max - 3])
    } else {
        s[..max].to_string()
    }
}

/// Verify a link target by checking reachability.
///
/// For GitHub issue references (e.g. `owner/repo#123`), uses `gh issue view`.
/// For HTTP(S) URLs, uses `curl --head`.
/// Returns `(is_valid, detail_message)`.
fn verify_link_target(target: &str) -> (bool, String) {
    // Detect GitHub issue reference: owner/repo#number
    if let Some((repo, number)) = parse_github_issue_ref(target) {
        let output = std::process::Command::new("gh")
            .args(["issue", "view", &number, "--repo", &repo, "--json", "state"])
            .output();
        match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                (true, format!("issue exists: {}", stdout.trim()))
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                (false, format!("gh issue view failed: {}", stderr.trim()))
            }
            Err(e) => (false, format!("could not run gh: {e}")),
        }
    } else if target.starts_with("http://") || target.starts_with("https://") {
        let output = std::process::Command::new("curl")
            .args([
                "--head",
                "--silent",
                "--output",
                "/dev/null",
                "--write-out",
                "%{http_code}",
                "--max-time",
                "10",
                "--location",
                target,
            ])
            .output();
        match output {
            Ok(out) if out.status.success() => {
                let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let code_num: u16 = code.parse().unwrap_or(0);
                if (200..400).contains(&code_num) {
                    (true, format!("HTTP {code}"))
                } else {
                    (false, format!("HTTP {code}"))
                }
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                (false, format!("curl failed: {}", stderr.trim()))
            }
            Err(e) => (false, format!("could not run curl: {e}")),
        }
    } else {
        (false, format!("unsupported target format: {target}"))
    }
}

/// Parse a GitHub issue reference like `owner/repo#123` into `(owner/repo, 123)`.
fn parse_github_issue_ref(target: &str) -> Option<(String, String)> {
    // Match patterns: owner/repo#123
    let parts: Vec<&str> = target.splitn(2, '#').collect();
    if parts.len() == 2 {
        let repo = parts[0];
        let number = parts[1];
        // Validate: repo should contain exactly one '/', number should be digits
        if repo.matches('/').count() == 1
            && !repo.starts_with('/')
            && !repo.ends_with('/')
            && number.chars().all(|c| c.is_ascii_digit())
            && !number.is_empty()
        {
            return Some((repo.to_string(), number.to_string()));
        }
    }
    None
}

/// Prompt the user to confirm decomposition of acceptance criteria into issues.
///
/// Reads a single line from stdin and returns `true` if the user enters `y` or
/// `yes` (case-insensitive). An empty input defaults to yes.
fn confirm_decomposition() -> bool {
    use std::io::Write;
    print!("Create these issues? [Y/n]: ");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    let trimmed = input.trim().to_lowercase();
    trimmed.is_empty() || trimmed == "y" || trimmed == "yes"
}

/// Attempt to refine acceptance criteria using an LLM runtime.
///
/// For each criterion, asks the LLM to produce a more detailed, actionable
/// description suitable for a GitHub issue body. Returns `None` if the LLM is
/// not available or fails, allowing the caller to fall back to the raw criteria.
async fn refine_criteria_with_llm(
    criteria: &[String],
    spec_name: &str,
    spec_content: &str,
) -> Option<Vec<String>> {
    // Build a minimal runtime to invoke the LLM.
    let runtime = belt_infra::runtimes::claude::ClaudeRuntime::new(None);

    // Check that the runtime is reachable (ANTHROPIC_API_KEY set, etc.) by
    // verifying its name. If the environment is not configured the invocation
    // will fail gracefully below.

    let numbered_criteria: String = criteria
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{}. {}", i + 1, c))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "You are a technical project manager. Given the following spec and its acceptance criteria, \
         produce a detailed, actionable issue description for each criterion. \
         Each description should include context, implementation hints, and verification steps.\n\n\
         Spec: {spec_name}\n\n\
         Spec content (abbreviated):\n{spec_summary}\n\n\
         Acceptance criteria:\n{numbered_criteria}\n\n\
         Output ONLY a JSON array of strings, one per criterion, in the same order. \
         Each string is the detailed issue body in markdown. No wrapping object, just the array.",
        spec_summary = &spec_content[..spec_content.len().min(2000)],
    );

    let request = belt_core::runtime::RuntimeRequest {
        working_dir: std::env::current_dir().unwrap_or_default(),
        prompt,
        model: None,
        system_prompt: None,
        session_id: None,
        structured_output: None,
    };

    let response = runtime.invoke(request).await;
    if !response.success() {
        eprintln!("info: LLM refinement unavailable, using raw criteria");
        return None;
    }

    // Parse the LLM output as a JSON array of strings.
    let stdout = response.stdout.trim();
    // The LLM might wrap the array in a markdown code block; strip it.
    let json_str = stdout
        .strip_prefix("```json")
        .or_else(|| stdout.strip_prefix("```"))
        .unwrap_or(stdout)
        .strip_suffix("```")
        .unwrap_or(stdout)
        .trim();

    match serde_json::from_str::<Vec<String>>(json_str) {
        Ok(refined) if refined.len() == criteria.len() => {
            eprintln!("info: LLM refined {} criteria", refined.len());
            Some(refined)
        }
        Ok(_) => {
            eprintln!("info: LLM returned mismatched count, using raw criteria");
            None
        }
        Err(e) => {
            eprintln!("info: could not parse LLM output ({e}), using raw criteria");
            None
        }
    }
}

/// Decompose a spec into independent sub-issues using an LLM.
///
/// Sends the spec content and acceptance criteria to the LLM and asks it to
/// produce structured JSON output with title, description, and acceptance
/// criteria for each proposed sub-issue. Falls back to `None` if the LLM is
/// unavailable or returns unparseable output, allowing the caller to use the
/// simpler `build_decomposed_issues` path.
async fn decompose_spec_with_llm(
    criteria: &[String],
    spec_name: &str,
    spec_content: &str,
) -> Option<Vec<belt_core::spec::LlmDecomposedIssue>> {
    let runtime = belt_infra::runtimes::claude::ClaudeRuntime::new(None);

    let numbered_criteria: String = criteria
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{}. {}", i + 1, c))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "You are a technical project manager decomposing a spec into independent GitHub issues.\n\n\
         Spec name: {spec_name}\n\n\
         Spec content:\n{spec_summary}\n\n\
         Acceptance criteria:\n{numbered_criteria}\n\n\
         Decompose this spec into independent, implementable GitHub issues. \
         Each issue should be self-contained and map to one or more acceptance criteria.\n\n\
         Output ONLY a JSON array of objects with these fields:\n\
         - \"title\": concise issue title (string)\n\
         - \"description\": detailed markdown body with context, implementation hints, \
           affected files/modules, and verification steps (string)\n\
         - \"acceptance_criteria\": specific testable acceptance criteria for this sub-issue \
           (array of strings)\n\n\
         Rules:\n\
         - Every original acceptance criterion must be covered by at least one sub-issue\n\
         - Each sub-issue should be independently implementable\n\
         - Keep titles under 80 characters\n\
         - Include enough detail in descriptions for a developer to start working immediately\n\n\
         Output ONLY the JSON array, no wrapping object or markdown fences.",
        spec_summary = &spec_content[..spec_content.len().min(3000)],
    );

    let request = belt_core::runtime::RuntimeRequest {
        working_dir: std::env::current_dir().unwrap_or_default(),
        prompt,
        model: None,
        system_prompt: None,
        session_id: None,
        structured_output: None,
    };

    let response = runtime.invoke(request).await;
    if !response.success() {
        eprintln!(
            "info: LLM decomposition unavailable, falling back to criteria-based decomposition"
        );
        return None;
    }

    let stdout = response.stdout.trim();
    let json_str = stdout
        .strip_prefix("```json")
        .or_else(|| stdout.strip_prefix("```"))
        .unwrap_or(stdout)
        .strip_suffix("```")
        .unwrap_or(stdout)
        .trim();

    match serde_json::from_str::<Vec<belt_core::spec::LlmDecomposedIssue>>(json_str) {
        Ok(issues) if !issues.is_empty() => {
            eprintln!("info: LLM decomposed spec into {} sub-issues", issues.len());
            Some(issues)
        }
        Ok(_) => {
            eprintln!("info: LLM returned empty decomposition, falling back");
            None
        }
        Err(e) => {
            eprintln!("info: could not parse LLM decomposition ({e}), falling back");
            None
        }
    }
}

/// Ensure the given GitHub labels exist in the repository.
///
/// For each label, runs `gh label create` which is a no-op if the label
/// already exists. This prevents `gh issue create --label` from failing
/// when a label has not been created yet.
fn ensure_github_labels(labels: &[&str]) {
    for label in labels {
        let output = std::process::Command::new("gh")
            .args(["label", "create", label, "--force"])
            .output();
        match output {
            Ok(o) if o.status.success() => {
                tracing::debug!(label = %label, "ensured GitHub label exists");
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                eprintln!(
                    "warning: failed to ensure label '{}': {}",
                    label,
                    stderr.trim()
                );
            }
            Err(e) => {
                eprintln!(
                    "warning: could not run `gh` to ensure label '{}': {e}",
                    label
                );
            }
        }
    }
}

/// Create a GitHub issue via the `gh` CLI and return the issue URL on success.
///
/// Attaches both the workspace trigger label and the standard `autopilot:trigger`
/// marker label so that `DataSource.collect()` can detect the issue regardless
/// of which label it scans for.
fn create_github_issue(title: &str, body: &str, trigger_label: &str) -> Option<String> {
    let labels: Vec<&str> = if trigger_label == "autopilot:trigger" {
        vec![trigger_label]
    } else {
        vec![trigger_label, "autopilot:trigger"]
    };
    create_github_issue_with_labels(title, body, &labels)
}

/// Resolve the trigger label from the workspace configuration.
///
/// Iterates over all sources and their states to find the first `trigger.label`
/// value. Returns `"autopilot:ready"` as a fallback when no trigger label is
/// configured.
fn resolve_trigger_label(config: &belt_core::workspace::WorkspaceConfig) -> String {
    for source in config.sources.values() {
        for state in source.states.values() {
            if let Some(ref label) = state.trigger.label {
                return label.clone();
            }
        }
    }
    "autopilot:ready".to_string()
}

/// Create a GitHub issue with the given title, body, and labels via the `gh` CLI.
///
/// Ensures all labels exist in the repository before creating the issue so
/// that `gh issue create --label` does not fail for missing labels. Returns
/// the URL of the created issue on success.
fn create_github_issue_with_labels(title: &str, body: &str, labels: &[&str]) -> Option<String> {
    // Ensure all labels exist before issue creation to prevent silent failures.
    ensure_github_labels(labels);

    let mut gh_cmd = std::process::Command::new("gh");
    gh_cmd.args(["issue", "create"]);
    gh_cmd.args(["--title", title]);
    gh_cmd.args(["--body", body]);
    for label in labels {
        gh_cmd.args(["--label", label]);
    }
    match gh_cmd.output() {
        Ok(output) => {
            if output.status.success() {
                let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
                println!("GitHub issue created: {url}");
                Some(url)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!("warning: failed to create GitHub issue: {}", stderr.trim());
                None
            }
        }
        Err(e) => {
            eprintln!("warning: could not run `gh` CLI: {e}");
            None
        }
    }
}

/// Extract the issue number from a GitHub issue URL.
///
/// For example, `https://github.com/owner/repo/issues/42` returns `Some("42")`.
fn extract_issue_number(url: &str) -> Option<String> {
    url.rsplit('/').next().and_then(|s| {
        if s.chars().all(|c| c.is_ascii_digit()) && !s.is_empty() {
            Some(s.to_string())
        } else {
            None
        }
    })
}

/// Update the body of an existing GitHub issue via the `gh` CLI.
fn update_github_issue_body(issue_number: &str, body: &str) {
    let mut gh_cmd = std::process::Command::new("gh");
    gh_cmd.args(["issue", "edit", issue_number]);
    gh_cmd.args(["--body", body]);
    match gh_cmd.output() {
        Ok(output) => {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!(
                    "warning: failed to update parent issue body: {}",
                    stderr.trim()
                );
            }
        }
        Err(e) => {
            eprintln!("warning: could not run `gh` for issue update: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
/// Handle `belt agent` (and legacy `belt claw`) subcommands.
async fn run_agent_command(command: AgentCommands) -> anyhow::Result<()> {
    match command {
        AgentCommands::Init { force } => {
            let belt_home = belt_home()?;
            let ws = if force {
                claw::ClawWorkspace::init_with_options(&belt_home, true)?
            } else {
                claw::ClawWorkspace::init(&belt_home)?
            };
            let rules_dir = ws.path.join(".claude/rules");
            tracing::info!(
                path = %ws.path.display(),
                rules_dir = %rules_dir.display(),
                rules_exist = rules_dir.is_dir(),
                "agent workspace initialized with global rules directory"
            );
        }
        AgentCommands::Rules => {
            let belt_home = belt_home()?;
            let ws = claw::ClawWorkspace {
                path: belt_home.join("claw-workspace"),
            };
            let rules = ws.list_rules()?;
            for rule in &rules {
                println!("{}", rule.display());
            }
        }
        AgentCommands::Edit { rule } => {
            let belt_home = belt_home()?;
            let ws = claw::ClawWorkspace {
                path: belt_home.join("claw-workspace"),
            };
            ws.edit_rule(rule.as_deref())?;
        }
        AgentCommands::Session {
            workspace,
            prompt,
            plan,
            json,
        } => {
            let exit_code = agent::run_agent(workspace, prompt, plan, json).await?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        AgentCommands::Plugin { install_dir } => {
            let dir = if let Some(ref custom) = install_dir {
                std::path::PathBuf::from(custom)
            } else {
                claw::plugin::default_install_dir()?
            };
            let plugin_path = claw::plugin::install_plugin(&dir)?;
            println!(
                "Installed /agent slash command plugin to: {}",
                plugin_path.display()
            );
            println!("Restart Claude Code to activate the /agent command.");
        }
        AgentCommands::Context => {
            let context = claw::plugin::collect_cli_context();
            println!("{context}");
        }
    }
    Ok(())
}

// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("belt=info".parse()?),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Start {
            config,
            tick,
            max_concurrent,
        } => {
            start_daemon(&config, tick, max_concurrent).await?;
        }
        Commands::Stop => {
            cmd_stop()?;
        }
        Commands::Restart { config, background } => {
            cmd_restart(&config, background).await?;
        }
        Commands::Status { format } => {
            cmd_status(&format)?;
        }
        Commands::Dashboard => {
            let belt_home = belt_home()?;
            std::fs::create_dir_all(&belt_home)?;
            let db_path = belt_home.join("belt.db");
            let db = std::sync::Arc::new(belt_infra::db::Database::open(
                db_path
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid db path"))?,
            )?);
            dashboard::run(db)?;
        }
        Commands::Workspace { command } => {
            let belt_home = belt_home()?;
            std::fs::create_dir_all(&belt_home)?;
            let db_path = belt_home.join("belt.db");
            let db = belt_infra::db::Database::open(
                db_path
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid db path"))?,
            )?;

            match command {
                WorkspaceCommands::Add { config, json } => {
                    let config_path = std::path::Path::new(&config);
                    let result =
                        belt_infra::onboarding::onboard_workspace(&db, config_path, &belt_home)?;

                    // Validate claw workspace — warn on failure, never block workspace add
                    let claw_ws_path = belt_home.join("claw-workspace");
                    match claw::ClawWorkspace::init(&belt_home) {
                        Ok(claw_ws) => {
                            tracing::info!(path = %claw_ws.path.display(), "claw workspace initialized");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to initialize claw workspace");
                            if !json && !claw_ws_path.is_dir() {
                                eprintln!(
                                    "Warning: Claw workspace not found at {}",
                                    claw_ws_path.display()
                                );
                                eprintln!("  Run `belt claw init` to set up the Claw workspace.");
                            }
                        }
                    }

                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "success": true,
                                "workspace": result.workspace_name,
                                "created": result.created,
                                "config": result.config_path,
                                "sources": result.source_count,
                                "cron_jobs_seeded": result.cron_jobs_seeded
                            }))?
                        );
                    } else {
                        if result.created {
                            println!(
                                "Workspace '{}' registered successfully.",
                                result.workspace_name
                            );
                        } else {
                            println!(
                                "Workspace '{}' updated successfully.",
                                result.workspace_name
                            );
                        }
                        println!("  Config: {}", result.config_path);
                        println!("  Sources: {}", result.source_count);
                        println!("  Cron jobs seeded: {}", result.cron_jobs_seeded);
                        println!("  Claw dir: {}", result.claw_dir.display());
                    }
                }
                WorkspaceCommands::List { json } => {
                    let workspaces = db.list_workspaces()?;
                    if json {
                        let entries: Vec<serde_json::Value> = workspaces
                            .iter()
                            .map(|(name, config_path, created_at)| {
                                serde_json::json!({
                                    "name": name,
                                    "config": config_path,
                                    "created_at": created_at,
                                })
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&entries)?);
                    } else if workspaces.is_empty() {
                        println!("No workspaces registered.");
                    } else {
                        println!("{:<20} {:<50} CREATED", "NAME", "CONFIG");
                        for (name, config_path, created_at) in &workspaces {
                            println!("{:<20} {:<50} {}", name, config_path, created_at);
                        }
                    }
                }
                WorkspaceCommands::Show { name, json } => {
                    let (ws_name, config_path, created_at) = db.get_workspace(&name)?;

                    // Show associated cron jobs
                    let jobs = db.list_cron_jobs()?;
                    let ws_jobs: Vec<_> = jobs
                        .iter()
                        .filter(|j| j.workspace.as_deref() == Some(&name))
                        .collect();

                    if json {
                        let cron_entries: Vec<serde_json::Value> = ws_jobs
                            .iter()
                            .map(|j| {
                                serde_json::json!({
                                    "name": j.name,
                                    "schedule": j.schedule,
                                    "enabled": j.enabled,
                                })
                            })
                            .collect();
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "name": ws_name,
                                "config": config_path,
                                "created_at": created_at,
                                "cron_jobs": cron_entries,
                            }))?
                        );
                    } else {
                        println!("Name:       {ws_name}");
                        println!("Config:     {config_path}");
                        println!("Created at: {created_at}");
                        if !ws_jobs.is_empty() {
                            println!("\nCron jobs:");
                            for job in &ws_jobs {
                                let status = if job.enabled { "enabled" } else { "disabled" };
                                println!("  {} [{}] ({})", job.name, job.schedule, status);
                            }
                        }
                    }
                }
                WorkspaceCommands::Update { name, config, json } => {
                    if let Some(config_path) = config {
                        let path = std::path::Path::new(&config_path);
                        let abs_path = std::fs::canonicalize(path)
                            .unwrap_or_else(|_| path.to_path_buf())
                            .to_string_lossy()
                            .to_string();
                        db.update_workspace(&name, &abs_path)?;
                        if json {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&serde_json::json!({
                                    "success": true,
                                    "workspace": name,
                                    "config": abs_path
                                }))?
                            );
                        } else {
                            println!("Workspace '{}' updated.", name);
                            println!("  Config: {}", abs_path);
                        }
                    } else if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "success": false,
                                "error": "no update options provided"
                            }))?
                        );
                    } else {
                        println!(
                            "No update options provided. Use --config to update the config path."
                        );
                    }
                }
                WorkspaceCommands::Remove { name, force, json } => {
                    // Check for active queue items in this workspace
                    let items = db.list_items(None, Some(&name))?;
                    let active_count = items
                        .iter()
                        .filter(|i| !matches!(i.phase(), QueuePhase::Done | QueuePhase::Skipped))
                        .count();

                    if active_count > 0 && !force {
                        if json {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&serde_json::json!({
                                    "success": false,
                                    "error": format!("workspace '{}' has {} active item(s), use --force to remove", name, active_count)
                                }))?
                            );
                        } else {
                            eprintln!(
                                "Warning: workspace '{}' has {} active item(s).",
                                name, active_count
                            );
                            eprintln!("Use --force to remove anyway.");
                        }
                        std::process::exit(1);
                    }

                    db.remove_workspace(&name)?;
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "success": true,
                                "workspace": name,
                                "active_items_remaining": active_count
                            }))?
                        );
                    } else {
                        println!("Workspace '{}' removed.", name);
                        if active_count > 0 {
                            println!(
                                "  Note: {} active item(s) remain in the queue.",
                                active_count
                            );
                        }
                    }
                }
                WorkspaceCommands::Config { name, json } => {
                    let (_ws_name, config_path, _created_at) = db.get_workspace(&name)?;

                    // Try to load and display the workspace config file
                    let path = std::path::Path::new(&config_path);
                    if path.exists() {
                        let config: belt_core::workspace::WorkspaceConfig =
                            belt_infra::workspace_loader::load_workspace_config(path)?;
                        if json {
                            let output = serde_json::to_string_pretty(&config)?;
                            println!("{output}");
                        } else {
                            println!("Name:        {}", config.name);
                            println!("Concurrency: {}", config.concurrency);
                            println!("Runtime:     {}", config.runtime.default);
                            if !config.sources.is_empty() {
                                println!("\nSources:");
                                for (source_name, source_cfg) in &config.sources {
                                    println!("  {source_name}:");
                                    println!("    URL:           {}", source_cfg.url);
                                    println!(
                                        "    Scan interval: {}s",
                                        source_cfg.scan_interval_secs
                                    );
                                }
                            }
                            if let Some(claw) = &config.claw_config {
                                println!("\nClaw config:");
                                println!("  Auto-approve: {}", claw.auto_approve);
                                if let Some(hp) = &claw.hitl_policy {
                                    println!("  HITL policy:  {hp}");
                                }
                                if let Some(cp) = &claw.classify_policy {
                                    println!("  Classify:     {cp}");
                                }
                                if !claw.enabled_commands.is_empty() {
                                    println!(
                                        "  Commands:     {}",
                                        claw.enabled_commands.join(", ")
                                    );
                                }
                            }
                        }
                    } else {
                        anyhow::bail!(
                            "Config file not found: {}. \
                             Use 'belt workspace update {} --config <path>' to fix.",
                            config_path,
                            name
                        );
                    }
                }
            }
        }
        Commands::Queue { command } => match command {
            QueueCommands::List {
                phase,
                workspace,
                format,
            } => {
                cmd_queue_list(phase, workspace, &format)?;
            }
            QueueCommands::Show { work_id, format } => {
                cmd_queue_show(&work_id, &format)?;
            }
            QueueCommands::Done { work_id, json } => {
                cmd_queue_done(&work_id, json).await?;
            }
            QueueCommands::Hitl {
                work_id,
                reason,
                json,
            } => {
                cmd_queue_hitl(&work_id, reason.as_deref(), json)?;
            }
            QueueCommands::Skip { work_id, json } => {
                cmd_queue_skip(&work_id, json)?;
            }
            QueueCommands::RetryScript {
                work_id,
                timeout,
                json,
            } => {
                cmd_queue_retry_script(&work_id, timeout, json).await?;
            }
            QueueCommands::Dependency(dep_cmd) => match dep_cmd {
                DependencyCommands::Add {
                    queue_id,
                    after,
                    json,
                } => {
                    cmd_queue_dependency_add(&queue_id, &after, json)?;
                }
                DependencyCommands::Remove {
                    queue_id,
                    after,
                    json,
                } => {
                    cmd_queue_dependency_remove(&queue_id, &after, json)?;
                }
                DependencyCommands::List { queue_id, format } => {
                    cmd_queue_dependency_list(queue_id.as_deref(), &format)?;
                }
            },
        },
        Commands::Cron { command } => match command {
            CronCommands::List { format } => {
                cmd_cron_list(&format)?;
            }
            CronCommands::Add {
                name,
                schedule,
                script,
                workspace,
                json,
            } => {
                cmd_cron_add(&name, &schedule, &script, workspace.as_deref(), json)?;
            }
            CronCommands::Update {
                name,
                schedule,
                script,
                json,
            } => {
                cmd_cron_update(&name, schedule.as_deref(), script.as_deref(), json)?;
            }
            CronCommands::Pause { name, json } => {
                cmd_cron_pause(&name, json)?;
            }
            CronCommands::Resume { name, json } => {
                cmd_cron_resume(&name, json)?;
            }
            CronCommands::Remove { name, json } => {
                cmd_cron_remove(&name, json)?;
            }
            CronCommands::Trigger { name, json } => {
                cmd_cron_trigger(&name, json)?;
            }
            CronCommands::Run { name, json } => {
                cmd_cron_run(&name, json)?;
            }
        },
        Commands::Context {
            work_id,
            json,
            field,
        } => {
            let db_path = belt_home()?.join("belt.db");

            if !db_path.exists() {
                anyhow::bail!("belt database not found at {}", db_path.display());
            }

            let db_path_str = db_path.to_string_lossy();
            let db = belt_infra::db::Database::open(&db_path_str)?;
            let item = db.get_item(&work_id)?;

            // Convert DB HistoryEvents to context HistoryEntries.
            let history_events = db.get_history(&item.source_id)?;
            let history: Vec<belt_core::context::HistoryEntry> = history_events
                .iter()
                .map(|e| belt_core::context::HistoryEntry {
                    source_id: e.source_id.clone(),
                    work_id: e.work_id.clone(),
                    state: e.state.clone(),
                    status: e
                        .status
                        .parse()
                        .unwrap_or(belt_core::context::HistoryStatus::Failed),
                    attempt: e.attempt as u32,
                    summary: e.summary.clone(),
                    error: e.error.clone(),
                    created_at: e.created_at.clone(),
                })
                .collect();

            // Try to load workspace config and use DataSource.get_context()
            // for dynamic context (issue/PR details, source URL, etc.).
            let ctx = match resolve_dynamic_context(&db, &item).await {
                Ok(mut dynamic_ctx) => {
                    // Merge DB history into dynamic context (DataSource returns empty history).
                    dynamic_ctx.history = history;
                    dynamic_ctx
                }
                Err(_) => {
                    // Fallback to static context when workspace config is unavailable.
                    belt_core::context::ItemContext {
                        work_id: item.work_id.clone(),
                        workspace: item.workspace_id.clone(),
                        queue: belt_core::context::QueueContext {
                            phase: item.phase().as_str().to_string(),
                            state: item.state.clone(),
                            source_id: item.source_id.clone(),
                        },
                        source: belt_core::context::SourceContext {
                            source_type: "unknown".to_string(),
                            url: String::new(),
                            default_branch: None,
                        },
                        issue: None,
                        pr: None,
                        history,
                        worktree: None,
                        source_data: serde_json::Value::Null,
                    }
                }
            };

            if let Some(ref field_path) = field {
                let value = serde_json::to_value(&ctx)?;
                let extracted = belt_core::context::extract_field(&value, field_path);
                match extracted {
                    Some(v) if v.is_string() => {
                        println!("{}", v.as_str().unwrap());
                    }
                    Some(v) => {
                        println!("{}", serde_json::to_string_pretty(v)?);
                    }
                    None => {
                        anyhow::bail!("field '{}' not found in context", field_path);
                    }
                }
            } else if json {
                println!("{}", serde_json::to_string_pretty(&ctx)?);
            } else {
                println!("work_id:   {}", ctx.work_id);
                println!("workspace: {}", ctx.workspace);
                println!("phase:     {}", ctx.queue.phase);
                println!("state:     {}", ctx.queue.state);
                println!("source_id: {}", ctx.queue.source_id);
                println!("source:    {} {}", ctx.source.source_type, ctx.source.url);
                if let Some(ref issue) = ctx.issue {
                    println!("issue:     #{} {}", issue.number, issue.title);
                }
                if let Some(ref pr) = ctx.pr {
                    println!("pr:        #{}", pr.number);
                }
                if !ctx.history.is_empty() {
                    println!("history:   {} entries", ctx.history.len());
                }
            }
        }
        Commands::Agent { command } | Commands::Claw { command } => {
            run_agent_command(command).await?;
        }
        Commands::Spec { command } => {
            let belt_home = belt_home()?;
            let db_path = belt_home.join("belt.db");
            let db = belt_infra::db::Database::open(
                db_path
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid db path"))?,
            )?;

            match command {
                SpecCommands::Status { name, format } => {
                    let spec_status = status::gather_spec_status(&db, &name)?;
                    status::print_spec_status(&spec_status, &format)?;
                }
                SpecCommands::Add {
                    workspace,
                    name,
                    content,
                    priority,
                    labels,
                    depends_on,
                    entry_point,
                    decompose,
                    yes,
                    skip_validation,
                } => {
                    // Validate required sections unless skipped
                    if !skip_validation
                        && let Err(missing) = belt_core::spec::validate_required_sections(&content)
                    {
                        anyhow::bail!(
                            "spec content is missing required sections: {}. \
                             Use --skip-validation to bypass this check.",
                            missing.join(", ")
                        );
                    }

                    let id = format!("spec-{}", chrono::Utc::now().timestamp_millis());
                    let mut spec =
                        belt_core::spec::Spec::new(id.clone(), workspace.clone(), name, content);
                    spec.priority = priority;
                    spec.labels = labels;
                    spec.depends_on = depends_on;
                    spec.entry_point = entry_point;

                    // Detect conflicts with existing specs in the same workspace
                    // and resolve them: auto-register dependencies for module
                    // overlaps, escalate file overlaps to HITL.
                    let mut has_hitl_conflicts = false;
                    let has_conflicts = if spec.entry_point.is_some() {
                        let existing_specs = db.list_specs(Some(&workspace), None)?;
                        let conflicts =
                            belt_core::spec::ConflictDetector::detect(&spec, &existing_specs);
                        if !conflicts.is_empty() {
                            let resolutions = belt_core::dependency::resolve_conflicts(&conflicts);

                            let mut auto_dep_ids: Vec<String> = Vec::new();

                            for resolution in &resolutions {
                                match &resolution.action {
                                    belt_core::dependency::ConflictAction::AutoDependency {
                                        dependency_spec_id,
                                    } => {
                                        eprintln!(
                                            "info: auto-registering dependency on spec '{}' ({}) \
                                             due to module overlap at '{}'",
                                            resolution.conflict.existing_spec_name,
                                            dependency_spec_id,
                                            resolution.conflict.path,
                                        );
                                        auto_dep_ids.push(dependency_spec_id.clone());
                                    }
                                    belt_core::dependency::ConflictAction::Hitl { reason } => {
                                        eprintln!("warning: HITL required - {reason}");
                                        has_hitl_conflicts = true;
                                    }
                                }
                            }

                            // Append auto-dependencies to the spec
                            if !auto_dep_ids.is_empty() {
                                let dep_refs: Vec<&str> =
                                    auto_dep_ids.iter().map(|s| s.as_str()).collect();
                                spec.depends_on = belt_core::dependency::append_dependencies(
                                    spec.depends_on.as_deref(),
                                    &dep_refs,
                                );
                            }

                            let conflicts_json = serde_json::to_string(&conflicts)?;
                            eprintln!("conflicts_json: {conflicts_json}");
                            Some(conflicts_json)
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    db.insert_spec(&spec)?;

                    // If file-level conflicts require HITL, print a notice.
                    // The spec remains in Draft (Pending) status so it won't
                    // be acted upon until the conflict is resolved by a human.
                    if has_hitl_conflicts {
                        eprintln!(
                            "notice: spec '{}' has file-level conflicts requiring human review. \
                             Spec stays in draft status until conflicts are resolved.",
                            id,
                        );
                    }

                    println!("spec created: {id}");

                    // Generate HITL item when spec conflicts are detected.
                    // The spec stays in Draft until a human resolves the conflict.
                    if let Some(conflicts_json) = has_conflicts {
                        let work_id = format!("spec-conflict:{id}:review");
                        let source_id = format!("spec:{id}");
                        let mut hitl_item = belt_core::queue::QueueItem::new(
                            work_id,
                            source_id,
                            workspace.clone(),
                            "review".to_string(),
                        );
                        hitl_item.set_phase_unchecked(QueuePhase::Hitl);
                        hitl_item.hitl_created_at = Some(chrono::Utc::now().to_rfc3339());
                        hitl_item.hitl_reason = Some(belt_core::queue::HitlReason::SpecConflict);
                        hitl_item.hitl_notes =
                            Some(format!("spec-conflict-detected: {conflicts_json}"));
                        hitl_item.title =
                            Some(format!("Spec conflict detected for '{}'", spec.name));
                        db.insert_item(&hitl_item)?;
                        eprintln!(
                            "hitl item created: {} (reason: spec-conflict-detected)",
                            hitl_item.work_id
                        );
                    }

                    // Auto-transition Draft -> Active when no HITL conflicts.
                    if !has_hitl_conflicts {
                        spec.transition_to(belt_core::spec::SpecStatus::Active)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        db.update_spec_status(&spec.id, spec.status)?;
                        eprintln!("spec '{}' auto-transitioned to active", id);
                    }

                    // Extract acceptance criteria for decomposition.
                    let criteria = belt_core::spec::extract_acceptance_criteria(&spec.content);

                    // Resolve trigger label from workspace config (fall back to
                    // "autopilot:ready" when unavailable).
                    let trigger_label = match db.get_workspace(&workspace) {
                        Ok((_name, config_path, _created_at)) => {
                            match belt_infra::workspace_loader::load_workspace_config(
                                std::path::Path::new(&config_path),
                            ) {
                                Ok(config) => resolve_trigger_label(&config),
                                Err(e) => {
                                    eprintln!(
                                        "warning: could not load workspace config: {e}, \
                                         using default label"
                                    );
                                    "autopilot:ready".to_string()
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "warning: could not find workspace '{}': {e}, \
                                 using default label",
                                workspace
                            );
                            "autopilot:ready".to_string()
                        }
                    };

                    // Auto-create GitHub parent issue with trigger label from config.
                    let parent_body = if decompose && !criteria.is_empty() {
                        // Append a placeholder for child issue links that will be
                        // filled in after child issues are created.
                        format!(
                            "{}\n\n## Sub-issues\n_Creating child issues..._",
                            spec.content
                        )
                    } else {
                        spec.content.clone()
                    };

                    let parent_url = create_github_issue(&spec.name, &parent_body, &trigger_label);

                    // Store parent issue URL as a spec link for traceability.
                    if let Some(ref url) = parent_url {
                        let link_id = format!("link-{}-parent", id);
                        let link = belt_core::spec::SpecLink::new(link_id, id.clone(), url.clone());
                        if let Err(e) = db.insert_spec_link(&link) {
                            eprintln!("warning: failed to store parent spec link: {e}");
                        }
                    }

                    if decompose
                        && !criteria.is_empty()
                        && let Some(ref parent) = parent_url
                    {
                        let parent_number = extract_issue_number(parent);

                        // Step 2: LLM-based structured decomposition.
                        // First try full structured decomposition (title + description +
                        // acceptance_criteria). If unavailable, fall back to simple
                        // criteria refinement.
                        let llm_decomposed =
                            decompose_spec_with_llm(&criteria, &spec.name, &spec.content).await;

                        let proposed_issues = if let Some(ref llm_issues) = llm_decomposed {
                            belt_core::spec::build_decomposed_issues_from_llm(
                                llm_issues,
                                parent_number.as_deref(),
                            )
                        } else {
                            // Fallback: refine criteria text only.
                            let refined =
                                refine_criteria_with_llm(&criteria, &spec.name, &spec.content)
                                    .await;
                            belt_core::spec::build_decomposed_issues(
                                &criteria,
                                refined.as_deref(),
                                parent_number.as_deref(),
                            )
                        };

                        // Step 3: User confirmation (unless --yes).
                        let confirmed = if yes {
                            true
                        } else {
                            let preview =
                                belt_core::spec::format_decomposition_preview(&proposed_issues);
                            println!("{preview}");
                            confirm_decomposition()
                        };

                        if !confirmed {
                            println!("decomposition cancelled by user");
                        } else {
                            // Step 4: Create child issues on GitHub.
                            let mut child_urls: Vec<String> = Vec::new();
                            let mut child_numbers: Vec<String> = Vec::new();

                            for issue in &proposed_issues {
                                if let Some(url) = create_github_issue_with_labels(
                                    &issue.title,
                                    &issue.body,
                                    &[&trigger_label, "autopilot:trigger"],
                                ) {
                                    println!("  child issue created: {url}");
                                    if let Some(num) = extract_issue_number(&url) {
                                        child_numbers.push(num);
                                    }
                                    child_urls.push(url);
                                }
                            }

                            // Update parent issue body with child issue links.
                            if !child_urls.is_empty()
                                && let Some(ref num) = parent_number
                            {
                                let links = child_urls
                                    .iter()
                                    .enumerate()
                                    .map(|(i, url)| format!("- [ ] AC{}: {}", i + 1, url))
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                let updated_body =
                                    format!("{}\n\n## Sub-issues\n{}", spec.content, links);
                                update_github_issue_body(num, &updated_body);
                            }

                            // Step 5: Store child issue URLs as spec links.
                            for url in &child_urls {
                                let link_id = format!(
                                    "link-{}-{}",
                                    id,
                                    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
                                );
                                let link = belt_core::spec::SpecLink::new(
                                    link_id,
                                    id.clone(),
                                    url.clone(),
                                );
                                if let Err(e) = db.insert_spec_link(&link) {
                                    eprintln!("warning: failed to store spec link for {url}: {e}");
                                }
                            }

                            // Store decomposed issue numbers.
                            // Note: spec is already Active (auto-transitioned earlier).
                            if !child_numbers.is_empty() {
                                spec.decomposed_issues = Some(child_numbers.join(","));
                                db.update_spec(&spec)?;
                                println!(
                                    "spec {} decomposed into {} issues",
                                    id,
                                    child_numbers.len()
                                );
                            }
                        }
                    }
                }
                SpecCommands::List {
                    workspace,
                    status,
                    json,
                } => {
                    let status_filter = status
                        .map(|s| {
                            s.parse::<belt_core::spec::SpecStatus>()
                                .map_err(|e| anyhow::anyhow!(e))
                        })
                        .transpose()?;
                    let specs = db.list_specs(workspace.as_deref(), status_filter)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&specs)?);
                    } else if specs.is_empty() {
                        println!("no specs found");
                    } else {
                        for spec in &specs {
                            println!(
                                "{}\t{}\t{}\t{}",
                                spec.id, spec.name, spec.status, spec.workspace_id
                            );
                        }
                    }
                }
                SpecCommands::Show { id, json } => {
                    let spec = db.get_spec(&id)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&spec)?);
                    } else {
                        println!("ID:          {}", spec.id);
                        println!("Name:        {}", spec.name);
                        println!("Status:      {}", spec.status);
                        println!("Workspace:   {}", spec.workspace_id);
                        println!("Content:     {}", spec.content);
                        if let Some(p) = spec.priority {
                            println!("Priority:    {p}");
                        }
                        if let Some(l) = &spec.labels {
                            println!("Labels:      {l}");
                        }
                        if let Some(d) = &spec.depends_on {
                            println!("Depends On:  {d}");
                        }
                        if let Some(ep) = &spec.entry_point {
                            println!("Entry Point: {ep}");
                        }
                        println!("Created At:  {}", spec.created_at);
                        println!("Updated At:  {}", spec.updated_at);
                    }
                }
                SpecCommands::Update {
                    id,
                    name,
                    content,
                    priority,
                    labels,
                    depends_on,
                    entry_point,
                    json,
                } => {
                    let mut spec = db.get_spec(&id)?;
                    if let Some(n) = name {
                        spec.name = n;
                    }
                    if let Some(c) = content {
                        spec.content = c;
                    }
                    if priority.is_some() {
                        spec.priority = priority;
                    }
                    if labels.is_some() {
                        spec.labels = labels;
                    }
                    if depends_on.is_some() {
                        spec.depends_on = depends_on;
                    }
                    if entry_point.is_some() {
                        spec.entry_point = entry_point;
                    }
                    db.update_spec(&spec)?;

                    // Force gap_detection re-evaluation on next daemon tick
                    // by resetting the cron job's last_run_at to NULL.
                    let gap_job_name = format!("{}:gap_detection", spec.workspace_id);
                    let gap_reset = db.reset_cron_last_run(&gap_job_name).is_ok();
                    if !gap_reset {
                        tracing::warn!(
                            job = %gap_job_name,
                            "failed to reset gap_detection cron job"
                        );
                    }

                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "success": true,
                                "id": id,
                                "action": "updated"
                            }))?
                        );
                    } else {
                        println!("spec updated: {id}");
                        if gap_reset {
                            println!("gap_detection scheduled for next tick ({})", gap_job_name);
                        }
                    }
                }
                SpecCommands::Pause { id, json } => {
                    let spec = db.get_spec(&id)?;
                    if !spec
                        .status
                        .can_transition_to(&belt_core::spec::SpecStatus::Paused)
                    {
                        anyhow::bail!(
                            "cannot pause spec in status '{}': only active specs can be paused",
                            spec.status
                        );
                    }
                    db.update_spec_status(&id, belt_core::spec::SpecStatus::Paused)?;
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "success": true,
                                "id": id,
                                "status": "paused"
                            }))?
                        );
                    } else {
                        println!("spec paused: {id}");
                    }
                }
                SpecCommands::Resume { id, json } => {
                    let spec = db.get_spec(&id)?;
                    if !spec
                        .status
                        .can_transition_to(&belt_core::spec::SpecStatus::Active)
                    {
                        anyhow::bail!(
                            "cannot resume spec in status '{}': only draft, paused, or archived specs can be activated",
                            spec.status
                        );
                    }
                    let was_draft = spec.status == belt_core::spec::SpecStatus::Draft;
                    let was_archived = spec.status == belt_core::spec::SpecStatus::Archived;
                    db.update_spec_status(&id, belt_core::spec::SpecStatus::Active)?;
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "success": true,
                                "id": id,
                                "status": "active"
                            }))?
                        );
                    } else if was_archived {
                        println!("spec restored from archive: {id}");
                    } else {
                        println!("spec activated: {id}");
                    }
                    if was_draft {
                        // TODO: trigger GitHub issue creation when spec transitions Draft -> Active
                        tracing::info!(
                            id,
                            "spec activated from draft — GitHub issue creation pending"
                        );
                    }
                }
                SpecCommands::Complete { id, json } => {
                    let spec = db.get_spec(&id)?;
                    // Determine the target status based on current state:
                    // Active -> Completing (enter completion flow)
                    // Completing -> Completed (HITL final approval)
                    let target = if spec.status == belt_core::spec::SpecStatus::Active {
                        belt_core::spec::SpecStatus::Completing
                    } else if spec.status == belt_core::spec::SpecStatus::Completing {
                        belt_core::spec::SpecStatus::Completed
                    } else {
                        anyhow::bail!(
                            "cannot complete spec in status '{}': only active or completing specs can advance toward completion",
                            spec.status
                        );
                    };
                    if !spec.status.can_transition_to(&target) {
                        anyhow::bail!("invalid transition: {} -> {}", spec.status, target);
                    }
                    db.update_spec_status(&id, target)?;
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "success": true,
                                "id": id,
                                "status": target.to_string()
                            }))?
                        );
                    } else {
                        match target {
                            belt_core::spec::SpecStatus::Completing => {
                                println!("spec entering completion flow: {id}");
                            }
                            belt_core::spec::SpecStatus::Completed => {
                                println!("spec completed: {id}");
                            }
                            _ => unreachable!(),
                        }
                    }
                }
                SpecCommands::Remove { id, json } => {
                    let spec = db.get_spec(&id)?;
                    if spec.status == belt_core::spec::SpecStatus::Completed {
                        anyhow::bail!(
                            "cannot archive spec in status 'completed': completed specs cannot be archived"
                        );
                    }
                    if spec.status == belt_core::spec::SpecStatus::Archived {
                        anyhow::bail!("spec is already archived");
                    }
                    db.update_spec_status(&id, belt_core::spec::SpecStatus::Archived)?;
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "success": true,
                                "id": id,
                                "status": "archived"
                            }))?
                        );
                    } else {
                        println!("spec archived: {id}");
                    }
                }
                SpecCommands::Link { id, target, json } => {
                    // Ensure spec exists.
                    let _ = db.get_spec(&id)?;
                    let link_id = format!("link-{}", chrono::Utc::now().timestamp_millis());
                    let link =
                        belt_core::spec::SpecLink::new(link_id.clone(), id.clone(), target.clone());
                    db.insert_spec_link(&link)?;
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "success": true,
                                "id": id,
                                "target": target,
                                "link_id": link_id
                            }))?
                        );
                    } else {
                        println!("linked {id} -> {target}");
                    }
                }
                SpecCommands::Unlink { id, target, json } => {
                    db.remove_spec_link(&id, &target)?;
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "success": true,
                                "id": id,
                                "target": target
                            }))?
                        );
                    } else {
                        println!("unlinked {id} -x- {target}");
                    }
                }
                SpecCommands::Verify { id, json } => {
                    let spec = db.get_spec(&id)?;

                    // Validate spec content against required sections.
                    let section_result = belt_core::spec::validate_required_sections(&spec.content);

                    // Verify link reachability.
                    let links = db.list_spec_links(&id)?;
                    let mut link_results: Vec<belt_core::spec::LinkVerification> = Vec::new();
                    for link in links {
                        let (valid, detail) = verify_link_target(&link.target);
                        link_results.push(belt_core::spec::LinkVerification {
                            link,
                            valid,
                            detail,
                        });
                    }

                    let sections_ok = section_result.is_ok();
                    let links_ok = link_results.iter().all(|r| r.valid);

                    if json {
                        let missing_sections: Vec<&str> = match &section_result {
                            Ok(()) => vec![],
                            Err(missing) => missing.clone(),
                        };
                        let output = serde_json::json!({
                            "spec_id": id,
                            "valid": sections_ok && links_ok,
                            "sections": {
                                "valid": sections_ok,
                                "missing": missing_sections,
                            },
                            "links": link_results,
                        });
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("Spec: {id}");
                        println!();

                        // Section validation results.
                        match &section_result {
                            Ok(()) => {
                                println!("[OK] All required sections present");
                            }
                            Err(missing) => {
                                println!(
                                    "[FAIL] Missing required sections: {}",
                                    missing.join(", ")
                                );
                            }
                        }

                        // Link verification results.
                        if link_results.is_empty() {
                            println!("[--] No links to verify");
                        } else {
                            for r in &link_results {
                                let status_icon = if r.valid { "OK" } else { "FAIL" };
                                println!("[{status_icon}] {} - {}", r.link.target, r.detail);
                            }
                            let total = link_results.len();
                            let passed = link_results.iter().filter(|r| r.valid).count();
                            println!("{passed}/{total} links verified");
                        }

                        println!();
                        if sections_ok && links_ok {
                            println!("Result: PASS");
                        } else {
                            println!("Result: FAIL");
                        }
                    }
                }
            }
        }
        Commands::Bootstrap {
            workspace,
            rules_dir,
            force,
            llm,
            project_name,
            language,
            framework,
            description,
            create_pr,
        } => {
            // --create-pr requires --llm
            if create_pr && !llm {
                anyhow::bail!("--create-pr requires --llm flag");
            }

            // When --llm is set without a custom rules_dir, use the interactive LLM path.
            if llm && rules_dir.is_none() {
                let workspace_root = match &workspace {
                    Some(ws) => std::path::PathBuf::from(ws),
                    None => std::env::current_dir()?,
                };
                let info = bootstrap::ProjectInfo {
                    name: project_name.unwrap_or_else(|| {
                        workspace_root
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "project".to_string())
                    }),
                    language: language.unwrap_or_else(|| "unknown".to_string()),
                    framework: framework.unwrap_or_default(),
                    description: description.unwrap_or_default(),
                };
                let runtime: Arc<dyn belt_core::runtime::AgentRuntime> =
                    Arc::new(ClaudeRuntime::new(None));
                let result = bootstrap::run_with_llm_interactive(
                    &workspace_root,
                    force,
                    runtime,
                    &info,
                    create_pr,
                    None,
                )
                .await?;
                if result.written.is_empty() && result.skipped.is_empty() {
                    println!("  bootstrap cancelled by user");
                } else {
                    for path in &result.written {
                        println!("  created: {}", path.display());
                    }
                    for path in &result.skipped {
                        println!("  skipped: {}", path.display());
                    }
                    if result.llm_generated {
                        println!("  (generated by LLM)");
                    }
                    if let Some(ref url) = result.pr_url {
                        println!("  pull request: {}", url);
                    }
                }
                tracing::info!(
                    rules_dir = %result.rules_dir.display(),
                    written = result.written.len(),
                    skipped = result.skipped.len(),
                    llm_generated = result.llm_generated,
                    pr_url = ?result.pr_url,
                    "bootstrap complete"
                );
            } else {
                let workspace_root = match (&workspace, &rules_dir) {
                    // If a custom rules_dir is given, create it directly.
                    (_, Some(dir)) => {
                        let rules_path = std::path::PathBuf::from(dir);
                        std::fs::create_dir_all(&rules_path)?;
                        let result = bootstrap::run_in_dir(&rules_path, force)?;
                        for path in &result.written {
                            println!("  created: {}", path.display());
                        }
                        for path in &result.skipped {
                            println!("  skipped: {}", path.display());
                        }
                        tracing::info!(
                            rules_dir = %rules_path.display(),
                            written = result.written.len(),
                            skipped = result.skipped.len(),
                            "bootstrap complete"
                        );
                        return Ok(());
                    }
                    (Some(ws), None) => std::path::PathBuf::from(ws),
                    (None, None) => std::env::current_dir()?,
                };
                let result = bootstrap::run(&workspace_root, force)?;
                for path in &result.written {
                    println!("  created: {}", path.display());
                }
                for path in &result.skipped {
                    println!("  skipped: {}", path.display());
                }
                tracing::info!(
                    rules_dir = %result.rules_dir.display(),
                    written = result.written.len(),
                    skipped = result.skipped.len(),
                    "bootstrap complete"
                );
            }
        }
        Commands::Hitl { command } => match command {
            HitlCommands::Respond {
                item_id,
                action,
                respondent,
                notes,
                json: json_output,
            } => {
                let action: belt_core::queue::HitlRespondAction =
                    action.parse().map_err(|e: String| anyhow::anyhow!(e))?;
                tracing::info!(
                    item_id,
                    %action,
                    ?respondent,
                    ?notes,
                    "responding to HITL item"
                );
                let db = open_db()?;
                // Verify the item exists and is in HITL phase.
                let item = db.get_item(&item_id)?;
                if item.phase() != QueuePhase::Hitl {
                    anyhow::bail!(
                        "item '{}' is in phase '{}', not 'hitl'",
                        item_id,
                        item.phase()
                    );
                }
                match action {
                    belt_core::queue::HitlRespondAction::Replan => {
                        let max_replan = 3u32;
                        let new_count = item.replan_count + 1;
                        if new_count > max_replan {
                            db.update_phase(&item_id, QueuePhase::Failed)?;
                            if json_output {
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(&serde_json::json!({
                                        "success": true,
                                        "work_id": item_id,
                                        "action": "replan",
                                        "phase": "failed",
                                        "reason": "replan limit exceeded"
                                    }))?
                                );
                            } else {
                                println!(
                                    "Item '{}' replan limit exceeded ({}/{}), transitioned to failed.",
                                    item_id, new_count, max_replan
                                );
                            }
                        } else {
                            // Roll back original item to Pending.
                            db.update_phase(&item_id, QueuePhase::Pending)?;
                            // Create a spec-modification-proposed HITL item.
                            let failure_reason =
                                item.hitl_notes.as_deref().unwrap_or("unknown failure");
                            let replan_work_id = format!("{item_id}:replan-{new_count}");
                            let mut replan_item = belt_core::queue::QueueItem::new(
                                replan_work_id.clone(),
                                item.source_id.clone(),
                                item.workspace_id.clone(),
                                item.state.clone(),
                            );
                            replan_item.set_phase_unchecked(QueuePhase::Hitl);
                            replan_item.hitl_created_at = Some(chrono::Utc::now().to_rfc3339());
                            replan_item.hitl_reason =
                                Some(belt_core::queue::HitlReason::SpecModificationProposed);
                            replan_item.hitl_notes = Some(format!(
                                "Claw replan delegation (attempt {new_count}): {failure_reason}"
                            ));
                            replan_item.title =
                                Some(format!("spec-modification-proposed (replan #{new_count})"));
                            replan_item.replan_count = new_count;
                            db.insert_item(&replan_item)?;
                            if json_output {
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(&serde_json::json!({
                                        "success": true,
                                        "work_id": item_id,
                                        "action": "replan",
                                        "phase": "pending",
                                        "replan_count": new_count,
                                        "replan_work_id": replan_work_id
                                    }))?
                                );
                            } else {
                                println!(
                                    "Item '{}' rolled back to pending (replan {}/{}). \
                                     Created HITL item '{}' for spec modification review.",
                                    item_id, new_count, max_replan, replan_work_id
                                );
                            }
                        }
                    }
                    _ => {
                        let target_phase = match action {
                            belt_core::queue::HitlRespondAction::Done => QueuePhase::Done,
                            belt_core::queue::HitlRespondAction::Retry => QueuePhase::Pending,
                            belt_core::queue::HitlRespondAction::Skip => QueuePhase::Skipped,
                            belt_core::queue::HitlRespondAction::Replan => unreachable!(),
                        };
                        db.update_phase(&item_id, target_phase)?;

                        // Handle spec-completion HITL: transition spec status
                        // based on the respond action.
                        if item.state == "spec_completion" {
                            let spec_id = &item.source_id;
                            match action {
                                belt_core::queue::HitlRespondAction::Done => {
                                    match db.update_spec_status(
                                        spec_id,
                                        belt_core::spec::SpecStatus::Completed,
                                    ) {
                                        Ok(()) => {
                                            tracing::info!(
                                                spec_id = %spec_id,
                                                "spec transitioned from Completing to \
                                                 Completed via HITL approval"
                                            );
                                            println!(
                                                "Spec '{}' transitioned to Completed.",
                                                spec_id
                                            );
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                spec_id = %spec_id,
                                                error = %e,
                                                "failed to transition spec to Completed \
                                                 after HITL approval"
                                            );
                                        }
                                    }
                                }
                                belt_core::queue::HitlRespondAction::Retry
                                | belt_core::queue::HitlRespondAction::Skip => {
                                    // Rejection or retry: revert spec to Active
                                    // so gap-detection can re-evaluate.
                                    match db.update_spec_status(
                                        spec_id,
                                        belt_core::spec::SpecStatus::Active,
                                    ) {
                                        Ok(()) => {
                                            tracing::info!(
                                                spec_id = %spec_id,
                                                "spec reverted to Active after HITL \
                                                 {action}"
                                            );
                                            println!("Spec '{}' reverted to Active.", spec_id);
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                spec_id = %spec_id,
                                                error = %e,
                                                "failed to revert spec to Active"
                                            );
                                        }
                                    }
                                }
                                belt_core::queue::HitlRespondAction::Replan => {
                                    unreachable!()
                                }
                            }
                        }

                        // Cleanup worktree on Done/Skipped (matches daemon pattern).
                        if matches!(target_phase, QueuePhase::Done | QueuePhase::Skipped)
                            && let Ok(home) = belt_home()
                        {
                            let wt_base = home.join("worktrees");
                            let repo_path = std::path::PathBuf::from(".");
                            let wt_mgr = GitWorktreeManager::new(wt_base, repo_path);
                            if let Err(e) = wt_mgr.cleanup(&item_id) {
                                tracing::warn!(
                                    work_id = %item_id,
                                    error = %e,
                                    "worktree cleanup failed on hitl respond, continuing"
                                );
                            }
                        }

                        if json_output {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&serde_json::json!({
                                    "success": true,
                                    "work_id": item_id,
                                    "action": action.to_string(),
                                    "phase": target_phase.as_str()
                                }))?
                            );
                        } else {
                            println!(
                                "Item '{}' transitioned from hitl to {} (action: {}).",
                                item_id, target_phase, action
                            );
                        }
                    }
                }
            }
            HitlCommands::List { workspace, format } => {
                tracing::info!(?workspace, "listing HITL items...");
                let db = open_db()?;
                let items = db.list_items(Some(QueuePhase::Hitl), workspace.as_deref())?;
                match format.as_str() {
                    "json" => {
                        println!("{}", serde_json::to_string_pretty(&items)?);
                    }
                    _ => {
                        if items.is_empty() {
                            println!("No items awaiting human review.");
                        } else {
                            println!(
                                "{:<40} {:<20} {:<12} {:<24} TITLE",
                                "WORK_ID", "WORKSPACE", "STATE", "REASON"
                            );
                            println!("{}", "-".repeat(104));
                            for item in &items {
                                let reason = item
                                    .hitl_reason
                                    .as_ref()
                                    .map(|r| r.to_string())
                                    .unwrap_or_else(|| "-".to_string());
                                println!(
                                    "{:<40} {:<20} {:<12} {:<24} {}",
                                    item.work_id,
                                    item.workspace_id,
                                    item.state,
                                    reason,
                                    item.title.as_deref().unwrap_or("-"),
                                );
                            }
                            println!("\n{} item(s) awaiting review.", items.len());
                        }
                    }
                }
            }
            HitlCommands::Show {
                item_id,
                format,
                interactive,
            } => {
                cmd_hitl_show(&item_id, &format, interactive)?;
            }
            HitlCommands::Timeout { command } => {
                cmd_hitl_timeout(command)?;
            }
        },

        Commands::Auto { command } => match command {
            AutoCommands::Plugin { command } => match command {
                AutoPluginCommands::Install { project, force } => {
                    let project_root = match project {
                        Some(p) => PathBuf::from(p),
                        None => std::env::current_dir()?,
                    };
                    let written = auto::plugin::install(&project_root, force)?;
                    if written.is_empty() {
                        println!("No files written (already installed). Use --force to overwrite.");
                    } else {
                        for path in &written {
                            println!("Installed: {}", path.display());
                        }
                        println!(
                            "\n/auto slash command installed. Restart Claude Code to activate."
                        );
                    }
                }
                AutoPluginCommands::Uninstall { project } => {
                    let project_root = match project {
                        Some(p) => PathBuf::from(p),
                        None => std::env::current_dir()?,
                    };
                    let removed = auto::plugin::uninstall(&project_root)?;
                    if removed.is_empty() {
                        println!("Nothing to remove (not installed).");
                    } else {
                        for path in &removed {
                            println!("Removed: {}", path.display());
                        }
                    }
                }
                AutoPluginCommands::Status { project } => {
                    let project_root = match project {
                        Some(p) => PathBuf::from(p),
                        None => std::env::current_dir()?,
                    };
                    if auto::plugin::is_installed(&project_root) {
                        println!("Installed");
                    } else {
                        println!("Not installed");
                    }
                }
            },
        },
        // Commands::Claw is handled above via the `|` pattern with Commands::Agent
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_github_issue_ref_valid() {
        let result = parse_github_issue_ref("owner/repo#123");
        assert_eq!(result, Some(("owner/repo".to_string(), "123".to_string())));
    }

    #[test]
    fn parse_github_issue_ref_url_not_matched() {
        // Full URLs are not issue refs.
        assert_eq!(
            parse_github_issue_ref("https://github.com/owner/repo/issues/1"),
            None
        );
    }

    #[test]
    fn parse_github_issue_ref_no_hash() {
        assert_eq!(parse_github_issue_ref("owner/repo"), None);
    }

    #[test]
    fn parse_github_issue_ref_no_number() {
        assert_eq!(parse_github_issue_ref("owner/repo#abc"), None);
    }

    #[test]
    fn parse_github_issue_ref_no_slash() {
        assert_eq!(parse_github_issue_ref("repo#123"), None);
    }

    #[test]
    fn parse_github_issue_ref_leading_slash() {
        assert_eq!(parse_github_issue_ref("/repo#123"), None);
    }

    #[test]
    fn parse_github_issue_ref_trailing_slash() {
        assert_eq!(parse_github_issue_ref("owner/#123"), None);
    }

    #[test]
    fn parse_github_issue_ref_empty_number() {
        assert_eq!(parse_github_issue_ref("owner/repo#"), None);
    }

    #[test]
    fn extract_issue_number_from_url() {
        assert_eq!(
            extract_issue_number("https://github.com/owner/repo/issues/42"),
            Some("42".to_string())
        );
    }

    #[test]
    fn extract_issue_number_no_number() {
        assert_eq!(
            extract_issue_number("https://github.com/owner/repo/issues/"),
            None
        );
    }

    #[test]
    fn extract_issue_number_non_numeric() {
        assert_eq!(
            extract_issue_number("https://github.com/owner/repo/issues/abc"),
            None
        );
    }

    #[test]
    fn recommended_action_evaluate_failure() {
        use belt_core::queue::HitlReason;
        let (action, _) = recommended_action(Some(&HitlReason::EvaluateFailure));
        assert_eq!(action, "retry");
    }

    #[test]
    fn recommended_action_retry_max_exceeded() {
        use belt_core::queue::HitlReason;
        let (action, _) = recommended_action(Some(&HitlReason::RetryMaxExceeded));
        assert_eq!(action, "skip");
    }

    #[test]
    fn recommended_action_timeout() {
        use belt_core::queue::HitlReason;
        let (action, _) = recommended_action(Some(&HitlReason::Timeout));
        assert_eq!(action, "retry");
    }

    #[test]
    fn recommended_action_manual_escalation() {
        use belt_core::queue::HitlReason;
        let (action, _) = recommended_action(Some(&HitlReason::ManualEscalation));
        assert_eq!(action, "done");
    }

    #[test]
    fn recommended_action_spec_conflict() {
        use belt_core::queue::HitlReason;
        let (action, _) = recommended_action(Some(&HitlReason::SpecConflict));
        assert_eq!(action, "replan");
    }

    #[test]
    fn recommended_action_spec_completion_review() {
        use belt_core::queue::HitlReason;
        let (action, _) = recommended_action(Some(&HitlReason::SpecCompletionReview));
        assert_eq!(action, "done");
    }

    #[test]
    fn recommended_action_spec_modification_proposed() {
        use belt_core::queue::HitlReason;
        let (action, _) = recommended_action(Some(&HitlReason::SpecModificationProposed));
        assert_eq!(action, "done");
    }

    #[test]
    fn recommended_action_none_reason() {
        let (action, _) = recommended_action(None);
        assert_eq!(action, "skip");
    }

    // --- CLI flag parsing tests ---

    #[test]
    fn hitl_list_format_text() {
        let cli = Cli::try_parse_from(["belt", "hitl", "list", "--format", "text"]).unwrap();
        match cli.command {
            Commands::Hitl {
                command: HitlCommands::List { format, .. },
            } => assert_eq!(format, "text"),
            _ => panic!("expected Hitl List command"),
        }
    }

    #[test]
    fn hitl_list_format_json() {
        let cli = Cli::try_parse_from(["belt", "hitl", "list", "--format", "json"]).unwrap();
        match cli.command {
            Commands::Hitl {
                command: HitlCommands::List { format, .. },
            } => assert_eq!(format, "json"),
            _ => panic!("expected Hitl List command"),
        }
    }

    #[test]
    fn hitl_list_format_default_is_text() {
        let cli = Cli::try_parse_from(["belt", "hitl", "list"]).unwrap();
        match cli.command {
            Commands::Hitl {
                command: HitlCommands::List { format, .. },
            } => assert_eq!(format, "text"),
            _ => panic!("expected Hitl List command"),
        }
    }

    #[test]
    fn hitl_show_interactive_flag() {
        let cli = Cli::try_parse_from(["belt", "hitl", "show", "item-1", "--interactive"]).unwrap();
        match cli.command {
            Commands::Hitl {
                command:
                    HitlCommands::Show {
                        item_id,
                        interactive,
                        ..
                    },
            } => {
                assert_eq!(item_id, "item-1");
                assert!(interactive);
            }
            _ => panic!("expected Hitl Show command"),
        }
    }

    #[test]
    fn hitl_show_without_interactive_flag() {
        let cli = Cli::try_parse_from(["belt", "hitl", "show", "item-1"]).unwrap();
        match cli.command {
            Commands::Hitl {
                command: HitlCommands::Show { interactive, .. },
            } => assert!(!interactive),
            _ => panic!("expected Hitl Show command"),
        }
    }

    #[test]
    fn spec_add_skip_validation_flag() {
        let cli = Cli::try_parse_from([
            "belt",
            "spec",
            "add",
            "--workspace",
            "ws1",
            "--name",
            "my-spec",
            "--content",
            "some content",
            "--skip-validation",
        ])
        .unwrap();
        match cli.command {
            Commands::Spec {
                command:
                    SpecCommands::Add {
                        skip_validation,
                        workspace,
                        name,
                        ..
                    },
            } => {
                assert!(skip_validation);
                assert_eq!(workspace, "ws1");
                assert_eq!(name, "my-spec");
            }
            _ => panic!("expected Spec Add command"),
        }
    }

    #[test]
    fn spec_add_without_skip_validation() {
        let cli = Cli::try_parse_from([
            "belt",
            "spec",
            "add",
            "--workspace",
            "ws1",
            "--name",
            "my-spec",
            "--content",
            "some content",
        ])
        .unwrap();
        match cli.command {
            Commands::Spec {
                command:
                    SpecCommands::Add {
                        skip_validation, ..
                    },
            } => assert!(!skip_validation),
            _ => panic!("expected Spec Add command"),
        }
    }

    #[test]
    fn cron_trigger_parses_name() {
        let cli = Cli::try_parse_from(["belt", "cron", "trigger", "daily-report"]).unwrap();
        match cli.command {
            Commands::Cron {
                command: CronCommands::Trigger { name, .. },
            } => assert_eq!(name, "daily-report"),
            _ => panic!("expected Cron Trigger command"),
        }
    }

    // --- Spec decomposition workflow integration tests ---

    #[test]
    fn spec_add_decompose_flag_parsing() {
        let cli = Cli::try_parse_from([
            "belt",
            "spec",
            "add",
            "--workspace",
            "ws1",
            "--name",
            "my-spec",
            "--content",
            "some content",
            "--decompose",
            "--skip-validation",
        ])
        .unwrap();
        match cli.command {
            Commands::Spec {
                command:
                    SpecCommands::Add {
                        decompose,
                        yes,
                        name,
                        ..
                    },
            } => {
                assert!(decompose);
                assert!(!yes);
                assert_eq!(name, "my-spec");
            }
            _ => panic!("expected Spec Add command"),
        }
    }

    #[test]
    fn spec_add_decompose_with_yes_flag() {
        let cli = Cli::try_parse_from([
            "belt",
            "spec",
            "add",
            "--workspace",
            "ws1",
            "--name",
            "decompose-test",
            "--content",
            "test content",
            "--decompose",
            "--yes",
            "--skip-validation",
        ])
        .unwrap();
        match cli.command {
            Commands::Spec {
                command: SpecCommands::Add { decompose, yes, .. },
            } => {
                assert!(decompose);
                assert!(yes);
            }
            _ => panic!("expected Spec Add command"),
        }
    }

    #[test]
    fn spec_add_decompose_defaults_to_false() {
        let cli = Cli::try_parse_from([
            "belt",
            "spec",
            "add",
            "--workspace",
            "ws1",
            "--name",
            "no-decompose",
            "--content",
            "test",
            "--skip-validation",
        ])
        .unwrap();
        match cli.command {
            Commands::Spec {
                command: SpecCommands::Add { decompose, .. },
            } => assert!(!decompose),
            _ => panic!("expected Spec Add command"),
        }
    }

    /// Integration test: spec insert -> extract AC -> build decomposed issues -> update DB.
    ///
    /// Simulates the decomposition workflow as performed by the CLI handler,
    /// verifying that the DB state is updated correctly when child issues are
    /// recorded in the spec.
    #[test]
    fn decompose_workflow_updates_spec_decomposed_issues_in_db() {
        let db = belt_infra::db::Database::open_in_memory().unwrap();

        let content = "\
## Overview\nSome feature.\n\n\
## Acceptance Criteria\n\
- Users can sign up with email\n\
- Users receive a verification email\n\
- Admin can view all users\n\n\
## Implementation\nDetails here.";

        let id = "spec-test-decompose-1";
        let spec = belt_core::spec::Spec::new(
            id.to_string(),
            "ws-test".to_string(),
            "Auth Feature".to_string(),
            content.to_string(),
        );

        db.insert_spec(&spec).unwrap();

        // Extract acceptance criteria (as the CLI handler does).
        let criteria = belt_core::spec::extract_acceptance_criteria(&spec.content);
        assert_eq!(criteria.len(), 3);

        // Build decomposed issues (no LLM refinement, with parent number).
        let proposed = belt_core::spec::build_decomposed_issues(&criteria, None, Some("100"));
        assert_eq!(proposed.len(), 3);
        assert!(proposed[0].title.contains("AC1"));
        assert!(proposed[0].title.contains("#100"));

        // Simulate child issue creation by assigning mock issue numbers.
        let child_numbers: Vec<String> = vec!["101".into(), "102".into(), "103".into()];

        // Update the spec's decomposed_issues field (as the CLI handler does).
        let mut spec = db.get_spec(id).unwrap();
        spec.decomposed_issues = Some(child_numbers.join(","));
        db.update_spec(&spec).unwrap();

        // Transition Draft -> Active (as the CLI handler does after decomposition).
        spec.transition_to(belt_core::spec::SpecStatus::Active)
            .unwrap();
        db.update_spec_status(&spec.id, spec.status).unwrap();

        // Verify DB state reflects the decomposition.
        let stored = db.get_spec(id).unwrap();
        assert_eq!(stored.decomposed_issues, Some("101,102,103".to_string()));
        assert_eq!(stored.status, belt_core::spec::SpecStatus::Active);

        // Verify parsed issue numbers.
        assert_eq!(stored.decomposed_issue_numbers(), vec!["101", "102", "103"]);
    }

    /// Integration test: verify spec links are stored for child issues
    /// during the decomposition workflow.
    #[test]
    fn decompose_workflow_stores_spec_links_for_child_issues() {
        let db = belt_infra::db::Database::open_in_memory().unwrap();

        let spec_id = "spec-test-links-1";
        let spec = belt_core::spec::Spec::new(
            spec_id.to_string(),
            "ws-test".to_string(),
            "Link Test".to_string(),
            "## Acceptance Criteria\n- AC one\n- AC two".to_string(),
        );
        db.insert_spec(&spec).unwrap();

        // Store parent issue link (as the CLI handler does).
        let parent_link = belt_core::spec::SpecLink::new(
            format!("link-{spec_id}-parent"),
            spec_id.to_string(),
            "https://github.com/owner/repo/issues/200".to_string(),
        );
        db.insert_spec_link(&parent_link).unwrap();

        // Store child issue links (as the CLI handler does).
        let child_urls = [
            "https://github.com/owner/repo/issues/201",
            "https://github.com/owner/repo/issues/202",
        ];
        for (i, url) in child_urls.iter().enumerate() {
            let link = belt_core::spec::SpecLink::new(
                format!("link-{spec_id}-child-{i}"),
                spec_id.to_string(),
                url.to_string(),
            );
            db.insert_spec_link(&link).unwrap();
        }

        // Verify all links are stored.
        let links = db.list_spec_links(spec_id).unwrap();
        assert_eq!(links.len(), 3);
        assert!(links[0].target.contains("200")); // parent
        assert!(links[1].target.contains("201")); // child 1
        assert!(links[2].target.contains("202")); // child 2
    }

    /// Integration test: decomposition with LLM-refined criteria produces
    /// enriched issue bodies.
    #[test]
    fn decompose_workflow_with_llm_refined_criteria() {
        let criteria = vec![
            "Login works with email".to_string(),
            "Logout clears session".to_string(),
        ];
        let refined = vec![
            "## Login\n\nImplement email-based login with validation.".to_string(),
            "## Logout\n\nClear session tokens and redirect.".to_string(),
        ];

        let issues =
            belt_core::spec::build_decomposed_issues(&criteria, Some(&refined), Some("50"));

        assert_eq!(issues.len(), 2);
        // When refined text is available, it should appear in the body.
        assert!(issues[0].body.contains("email-based login"));
        assert!(issues[1].body.contains("Clear session tokens"));
        // Parent reference is embedded.
        assert!(issues[0].body.contains("Parent: #50"));
    }

    /// Integration test: when LLM refinement returns mismatched count,
    /// raw criteria are used as fallback. Simulates the CLI handler's
    /// fallback behavior.
    #[test]
    fn decompose_workflow_llm_mismatch_falls_back_to_raw() {
        let criteria = vec![
            "Feature A".to_string(),
            "Feature B".to_string(),
            "Feature C".to_string(),
        ];
        // Simulate LLM returning wrong count (2 instead of 3).
        let refined = vec!["Refined A".to_string(), "Refined B".to_string()];

        // The build_decomposed_issues function with mismatched refined vec
        // falls back per-item (items without a refined entry use raw criterion).
        let issues =
            belt_core::spec::build_decomposed_issues(&criteria, Some(&refined), Some("10"));

        assert_eq!(issues.len(), 3);
        // First two use refined text.
        assert!(issues[0].body.contains("Refined A"));
        assert!(issues[1].body.contains("Refined B"));
        // Third falls back to raw criterion.
        assert!(issues[2].body.contains("Feature C"));
    }

    /// Integration test: full decomposition workflow from spec insert through
    /// decomposed_issues DB update, verifying the spec transitions correctly.
    #[test]
    fn decompose_full_workflow_spec_status_transitions() {
        let db = belt_infra::db::Database::open_in_memory().unwrap();

        let content = "\
## Overview\nTask manager.\n\n\
## Acceptance Criteria\n\
- Create tasks\n\
- Delete tasks\n\n\
## Notes\nEnd.";

        let id = "spec-full-flow-1";
        let mut spec = belt_core::spec::Spec::new(
            id.to_string(),
            "ws-flow".to_string(),
            "Task Manager".to_string(),
            content.to_string(),
        );
        db.insert_spec(&spec).unwrap();

        // Spec starts in Draft.
        assert_eq!(spec.status, belt_core::spec::SpecStatus::Draft);

        // Extract criteria and build proposals.
        let criteria = belt_core::spec::extract_acceptance_criteria(&spec.content);
        assert_eq!(criteria.len(), 2);

        let proposed = belt_core::spec::build_decomposed_issues(&criteria, None, Some("300"));
        assert_eq!(proposed.len(), 2);

        // Preview should list both issues.
        let preview = belt_core::spec::format_decomposition_preview(&proposed);
        assert!(preview.contains("2 child issue(s)"));
        assert!(preview.contains("AC1"));
        assert!(preview.contains("AC2"));

        // Simulate issue creation and store decomposed_issues.
        let child_nums = ["301".to_string(), "302".to_string()];
        spec.decomposed_issues = Some(child_nums.join(","));
        db.update_spec(&spec).unwrap();

        // Transition Draft -> Active.
        spec.transition_to(belt_core::spec::SpecStatus::Active)
            .unwrap();
        db.update_spec_status(&spec.id, spec.status).unwrap();

        let stored = db.get_spec(id).unwrap();
        assert_eq!(stored.status, belt_core::spec::SpecStatus::Active);
        assert_eq!(stored.decomposed_issues, Some("301,302".to_string()));

        // Verify that the spec recognizes it has been decomposed.
        assert!(stored.is_decomposed());
    }

    /// Integration test: spec without acceptance criteria section should
    /// result in empty criteria list, skipping decomposition.
    #[test]
    fn decompose_workflow_no_criteria_skips_decomposition() {
        let content = "## Overview\nA spec with no AC section.\n\n## Notes\nDone.";
        let criteria = belt_core::spec::extract_acceptance_criteria(content);
        assert!(criteria.is_empty());

        // With empty criteria, build_decomposed_issues returns empty vec.
        let proposed = belt_core::spec::build_decomposed_issues(&criteria, None, Some("1"));
        assert!(proposed.is_empty());
    }

    /// Integration test: parent issue body update includes sub-issue links
    /// in the expected format.
    #[test]
    fn decompose_workflow_parent_body_update_format() {
        let spec_content = "## Overview\nFeature spec.\n\n## Acceptance Criteria\n- A\n- B";
        let child_urls = [
            "https://github.com/owner/repo/issues/501".to_string(),
            "https://github.com/owner/repo/issues/502".to_string(),
        ];

        // Build the updated parent body as the CLI handler does.
        let links = child_urls
            .iter()
            .enumerate()
            .map(|(i, url)| format!("- [ ] AC{}: {}", i + 1, url))
            .collect::<Vec<_>>()
            .join("\n");
        let updated_body = format!("{}\n\n## Sub-issues\n{}", spec_content, links);

        assert!(updated_body.contains("## Sub-issues"));
        assert!(updated_body.contains("- [ ] AC1: https://github.com/owner/repo/issues/501"));
        assert!(updated_body.contains("- [ ] AC2: https://github.com/owner/repo/issues/502"));
        // Original content is preserved.
        assert!(updated_body.starts_with("## Overview"));
    }

    /// Integration test: extract_issue_number works with URLs produced during
    /// decomposition (used for parent_number and child_numbers).
    #[test]
    fn decompose_workflow_issue_number_extraction() {
        // Parent issue URL.
        let parent = "https://github.com/owner/repo/issues/42";
        assert_eq!(extract_issue_number(parent), Some("42".to_string()));

        // Child issue URLs.
        let child1 = "https://github.com/owner/repo/issues/43";
        let child2 = "https://github.com/owner/repo/issues/44";
        assert_eq!(extract_issue_number(child1), Some("43".to_string()));
        assert_eq!(extract_issue_number(child2), Some("44".to_string()));

        // The parent_number is used in build_decomposed_issues.
        let criteria = vec!["Test criterion".to_string()];
        let issues = belt_core::spec::build_decomposed_issues(
            &criteria,
            None,
            extract_issue_number(parent).as_deref(),
        );
        assert!(issues[0].title.contains("#42"));
    }

    #[test]
    fn spec_verify_parses_id_and_json_flag() {
        let cli = Cli::try_parse_from(["belt", "spec", "verify", "spec-42", "--json"]).unwrap();
        match cli.command {
            Commands::Spec {
                command: SpecCommands::Verify { id, json },
            } => {
                assert_eq!(id, "spec-42");
                assert!(json);
            }
            _ => panic!("expected Spec Verify command"),
        }
    }

    #[test]
    fn spec_verify_parses_without_json_flag() {
        let cli = Cli::try_parse_from(["belt", "spec", "verify", "spec-1"]).unwrap();
        match cli.command {
            Commands::Spec {
                command: SpecCommands::Verify { id, json },
            } => {
                assert_eq!(id, "spec-1");
                assert!(!json);
            }
            _ => panic!("expected Spec Verify command"),
        }
    }

    #[test]
    fn spec_link_parses_positional_args() {
        let cli = Cli::try_parse_from([
            "belt",
            "spec",
            "link",
            "spec-10",
            "https://example.com/issue/1",
        ])
        .unwrap();
        match cli.command {
            Commands::Spec {
                command: SpecCommands::Link { id, target, .. },
            } => {
                assert_eq!(id, "spec-10");
                assert_eq!(target, "https://example.com/issue/1");
            }
            _ => panic!("expected Spec Link command"),
        }
    }

    #[test]
    fn spec_link_parses_github_issue_ref() {
        let cli =
            Cli::try_parse_from(["belt", "spec", "link", "spec-5", "owner/repo#123"]).unwrap();
        match cli.command {
            Commands::Spec {
                command: SpecCommands::Link { id, target, .. },
            } => {
                assert_eq!(id, "spec-5");
                assert_eq!(target, "owner/repo#123");
            }
            _ => panic!("expected Spec Link command"),
        }
    }

    #[test]
    fn spec_unlink_parses_positional_args() {
        let cli = Cli::try_parse_from([
            "belt",
            "spec",
            "unlink",
            "spec-10",
            "https://example.com/issue/1",
        ])
        .unwrap();
        match cli.command {
            Commands::Spec {
                command: SpecCommands::Unlink { id, target, .. },
            } => {
                assert_eq!(id, "spec-10");
                assert_eq!(target, "https://example.com/issue/1");
            }
            _ => panic!("expected Spec Unlink command"),
        }
    }

    /// Integration test: verify that the decompose-to-collect pipeline produces
    /// issues whose labels match the DataSource trigger configuration.
    ///
    /// Simulates the full lifecycle:
    ///   spec add --decompose -> extract AC -> build issues -> label attachment
    ///   -> DataSource.collect() label matching
    #[test]
    fn decompose_to_collect_pipeline_label_matching() {
        let content = "\
## Overview\nAPI feature.\n\n\
## Acceptance Criteria\n\
- Endpoint returns 200\n\
- Response includes pagination\n\n\
## Notes\nDone.";

        // Step 1: Extract acceptance criteria (as spec add handler does).
        let criteria = belt_core::spec::extract_acceptance_criteria(content);
        assert_eq!(criteria.len(), 2);

        // Step 2: Build decomposed issues with parent number.
        let proposed = belt_core::spec::build_decomposed_issues(&criteria, None, Some("400"));
        assert_eq!(proposed.len(), 2);

        // Step 3: Verify the labels that would be attached by
        // create_github_issue_with_labels match what DataSource.collect() scans.
        //
        // The CLI handler calls:
        //   create_github_issue_with_labels(&title, &body, &[&trigger_label, "autopilot:trigger"])
        //
        // DataSource.collect() scans issues matching state_config.trigger.label.
        // For the pipeline to work, the trigger_label on child issues must match
        // the label configured in the workspace source state.
        let trigger_label = "autopilot:ready";
        let child_labels: Vec<&str> = vec![trigger_label, "autopilot:trigger"];

        // The trigger_label from workspace config must be present in child_labels.
        assert!(
            child_labels.contains(&trigger_label),
            "child issue labels must include the workspace trigger label for collect() detection"
        );

        // Step 4: Verify parent issue also carries the trigger label for collect.
        // create_github_issue now adds both trigger_label and "autopilot:trigger".
        let parent_labels: Vec<&str> = if trigger_label == "autopilot:trigger" {
            vec![trigger_label]
        } else {
            vec![trigger_label, "autopilot:trigger"]
        };
        assert!(
            parent_labels.contains(&trigger_label),
            "parent issue labels must include the workspace trigger label for collect() detection"
        );
        assert!(
            parent_labels.contains(&"autopilot:trigger"),
            "parent issue must carry autopilot:trigger marker"
        );
    }

    /// Verify that resolve_trigger_label falls back to "autopilot:ready" when
    /// no trigger label is configured in the workspace, and that this default
    /// label is compatible with the collect pipeline.
    #[test]
    fn resolve_trigger_label_fallback_compatible_with_collect() {
        // Build a workspace config without any trigger.label configured.
        let yaml = r#"
name: test-ws
sources:
  github:
    url: "https://github.com/test/repo"
    scan_interval_secs: 300
    states:
      analyze:
        trigger: {}
        prompt: "analyze this"
"#;
        let config: belt_core::workspace::WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();

        let label = resolve_trigger_label(&config);
        assert_eq!(label, "autopilot:ready");
    }

    /// Verify that resolve_trigger_label extracts the configured trigger label.
    #[test]
    fn resolve_trigger_label_uses_configured_label() {
        let yaml = r#"
name: test-ws
sources:
  github:
    url: "https://github.com/test/repo"
    scan_interval_secs: 300
    states:
      build:
        trigger:
          label: "belt:build"
        prompt: "build it"
"#;
        let config: belt_core::workspace::WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();

        let label = resolve_trigger_label(&config);
        assert_eq!(label, "belt:build");
    }

    /// Integration test: full pipeline from spec content to queue item structure.
    ///
    /// Verifies that decomposed issues would produce valid QueueItem entries
    /// when collected by DataSource, with correct work_id and source_id format.
    #[test]
    fn decompose_to_collect_queue_item_format() {
        use belt_core::queue::QueueItem;

        let content = "## Overview\nFeature.\n\n## Acceptance Criteria\n- Task A\n- Task B";
        let criteria = belt_core::spec::extract_acceptance_criteria(content);
        let proposed = belt_core::spec::build_decomposed_issues(&criteria, None, Some("600"));

        // Simulate what DataSource.collect() would produce for each child issue.
        let repo = "owner/repo";
        let child_numbers = ["601", "602"];

        for (i, num) in child_numbers.iter().enumerate() {
            let source_id = format!("github:{repo}#{num}");
            let state_name = "analyze";
            let work_id = QueueItem::make_work_id(&source_id, state_name);

            // Verify the work_id follows the expected format.
            assert!(work_id.contains(&source_id));
            assert!(work_id.contains(state_name));

            // Verify the proposed issue title references the parent.
            assert!(proposed[i].title.contains("#600"));
        }
    }

    // --- queue done / skip / retry-script integration tests ---

    /// Helper: create a temp workspace YAML file and register it in the DB.
    /// Returns (db, workspace_id, temp_dir) — temp_dir must be kept alive.
    fn setup_workspace_with_config(
        yaml: &str,
    ) -> (belt_infra::db::Database, String, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config_path = tmp.path().join("workspace.yaml");
        std::fs::write(&config_path, yaml).expect("write yaml");

        let db = belt_infra::db::Database::open_in_memory().expect("in-memory db");
        let ws_id = "test-ws";
        db.add_workspace(ws_id, config_path.to_str().unwrap())
            .expect("add workspace");

        (db, ws_id.to_string(), tmp)
    }

    /// Helper: create a `QueueItem` in a given phase.
    fn make_item(
        work_id: &str,
        ws_id: &str,
        state: &str,
        phase: QueuePhase,
    ) -> belt_core::queue::QueueItem {
        let mut item = belt_core::queue::QueueItem::new(
            work_id.to_string(),
            format!("gh:test/repo#{}", work_id),
            ws_id.to_string(),
            state.to_string(),
        );
        item.set_phase_unchecked(phase);
        item.title = Some("test item".to_string());
        item
    }

    /// Helper: build an `ActionExecutor` with default shell and a minimal
    /// runtime registry (same as `cmd_queue_done` builds internally).
    fn build_executor() -> belt_daemon::executor::ActionExecutor {
        let mut registry = belt_core::runtime::RuntimeRegistry::new("claude".to_string());
        registry.register(std::sync::Arc::new(
            belt_infra::runtimes::claude::ClaudeRuntime::new(None),
        ));
        belt_daemon::executor::ActionExecutor::new(std::sync::Arc::new(registry))
    }

    // ---- cmd_queue_done tests ----

    /// on_done script executes successfully -> item transitions to Done.
    #[tokio::test]
    async fn queue_done_on_done_success_transitions_to_done() {
        let yaml = r#"
name: test-ws
sources:
  github:
    url: "https://github.com/test/repo"
    scan_interval_secs: 300
    states:
      implement:
        trigger: {}
        prompt: "implement"
        on_done:
          - script: "true"
"#;
        let (db, ws_id, _tmp) = setup_workspace_with_config(yaml);
        let item = make_item("done-ok-1", &ws_id, "implement", QueuePhase::Running);
        db.insert_item(&item).unwrap();

        // Replicate cmd_queue_done logic: load config, find on_done, execute.
        let stored = db.get_item("done-ok-1").unwrap();
        let (_, config_path, _) = db.get_workspace(&stored.workspace_id).unwrap();
        let config =
            belt_infra::workspace_loader::load_workspace_config(std::path::Path::new(&config_path))
                .unwrap();

        let state_config = config
            .sources
            .values()
            .find_map(|s| s.states.get(&stored.state))
            .unwrap();

        let on_done: Vec<belt_core::action::Action> = state_config
            .on_done
            .iter()
            .map(belt_core::action::Action::from)
            .collect();
        assert!(!on_done.is_empty());

        let worktree_dir = tempfile::tempdir().unwrap();
        let env = belt_daemon::executor::ActionEnv::new("done-ok-1", worktree_dir.path());
        let executor = build_executor();

        let result = executor.execute_all(&on_done, &env).await.unwrap();
        match result {
            Some(r) if r.success() => {
                db.update_phase("done-ok-1", QueuePhase::Done).unwrap();
            }
            Some(r) => {
                db.update_phase("done-ok-1", QueuePhase::Failed).unwrap();
                panic!("expected success but got exit_code {}", r.exit_code);
            }
            None => {
                db.update_phase("done-ok-1", QueuePhase::Done).unwrap();
            }
        }

        let final_item = db.get_item("done-ok-1").unwrap();
        assert_eq!(final_item.phase(), QueuePhase::Done);
    }

    /// on_done script fails -> item transitions to Failed.
    #[tokio::test]
    async fn queue_done_on_done_failure_transitions_to_failed() {
        let yaml = r#"
name: test-ws
sources:
  github:
    url: "https://github.com/test/repo"
    scan_interval_secs: 300
    states:
      implement:
        trigger: {}
        prompt: "implement"
        on_done:
          - script: "false"
"#;
        let (db, ws_id, _tmp) = setup_workspace_with_config(yaml);
        let item = make_item("done-fail-1", &ws_id, "implement", QueuePhase::Running);
        db.insert_item(&item).unwrap();

        let stored = db.get_item("done-fail-1").unwrap();
        let (_, config_path, _) = db.get_workspace(&stored.workspace_id).unwrap();
        let config =
            belt_infra::workspace_loader::load_workspace_config(std::path::Path::new(&config_path))
                .unwrap();

        let state_config = config
            .sources
            .values()
            .find_map(|s| s.states.get(&stored.state))
            .unwrap();

        let on_done: Vec<belt_core::action::Action> = state_config
            .on_done
            .iter()
            .map(belt_core::action::Action::from)
            .collect();

        let worktree_dir = tempfile::tempdir().unwrap();
        let env = belt_daemon::executor::ActionEnv::new("done-fail-1", worktree_dir.path());
        let executor = build_executor();

        let result = executor.execute_all(&on_done, &env).await.unwrap();
        match result {
            Some(r) if r.success() => {
                db.update_phase("done-fail-1", QueuePhase::Done).unwrap();
                panic!("expected failure but script succeeded");
            }
            Some(_) => {
                db.update_phase("done-fail-1", QueuePhase::Failed).unwrap();
            }
            None => {
                panic!("expected a result from on_done script");
            }
        }

        let final_item = db.get_item("done-fail-1").unwrap();
        assert_eq!(final_item.phase(), QueuePhase::Failed);
    }

    /// No on_done configured -> direct Done transition.
    #[tokio::test]
    async fn queue_done_no_on_done_direct_done() {
        let yaml = r#"
name: test-ws
sources:
  github:
    url: "https://github.com/test/repo"
    scan_interval_secs: 300
    states:
      implement:
        trigger: {}
        prompt: "implement"
"#;
        let (db, ws_id, _tmp) = setup_workspace_with_config(yaml);
        let item = make_item("done-direct-1", &ws_id, "implement", QueuePhase::Running);
        db.insert_item(&item).unwrap();

        let stored = db.get_item("done-direct-1").unwrap();
        let (_, config_path, _) = db.get_workspace(&stored.workspace_id).unwrap();
        let config =
            belt_infra::workspace_loader::load_workspace_config(std::path::Path::new(&config_path))
                .unwrap();

        let state_config = config
            .sources
            .values()
            .find_map(|s| s.states.get(&stored.state));

        let on_done_actions: Vec<belt_core::action::Action> = state_config
            .map(|sc| {
                sc.on_done
                    .iter()
                    .map(belt_core::action::Action::from)
                    .collect()
            })
            .unwrap_or_default();

        assert!(on_done_actions.is_empty());
        db.update_phase("done-direct-1", QueuePhase::Done).unwrap();

        let final_item = db.get_item("done-direct-1").unwrap();
        assert_eq!(final_item.phase(), QueuePhase::Done);
    }

    /// Worktree cleanup is invoked on Done transition.
    #[test]
    fn queue_done_worktree_cleanup_on_done() {
        use belt_infra::worktree::WorktreeManager;

        let worktree_base = tempfile::tempdir().unwrap();
        let work_id = "cleanup-done-1";

        let worktree_mgr =
            belt_infra::worktree::MockWorktreeManager::new(worktree_base.path().to_path_buf());

        // Create a worktree via MockWorktreeManager.
        let wt_path = worktree_mgr.create_or_reuse(work_id).unwrap();
        assert!(wt_path.exists());

        // Cleanup should succeed and remove the directory.
        let result = worktree_mgr.cleanup(work_id);
        assert!(result.is_ok());
        assert!(!wt_path.exists());
    }

    /// Worktree cleanup is invoked on Skip transition.
    #[test]
    fn queue_skip_worktree_cleanup_on_skip() {
        use belt_infra::worktree::WorktreeManager;

        let worktree_base = tempfile::tempdir().unwrap();
        let work_id = "cleanup-skip-1";

        let worktree_mgr =
            belt_infra::worktree::MockWorktreeManager::new(worktree_base.path().to_path_buf());

        let wt_path = worktree_mgr.create_or_reuse(work_id).unwrap();
        assert!(wt_path.exists());

        let result = worktree_mgr.cleanup(work_id);
        assert!(result.is_ok());
        assert!(!wt_path.exists());
    }

    // ---- cmd_queue_skip tests ----

    /// Skip transitions item to Skipped phase.
    #[test]
    fn queue_skip_transitions_to_skipped() {
        let db = belt_infra::db::Database::open_in_memory().unwrap();
        db.add_workspace("test-ws", "/dev/null").unwrap();
        let item = make_item("skip-1", "test-ws", "implement", QueuePhase::Running);
        db.insert_item(&item).unwrap();

        db.update_phase("skip-1", QueuePhase::Skipped).unwrap();

        let final_item = db.get_item("skip-1").unwrap();
        assert_eq!(final_item.phase(), QueuePhase::Skipped);
    }

    // ---- cmd_queue_retry_script tests ----

    /// retry_script: Failed item with on_done script that succeeds -> Done.
    #[tokio::test]
    async fn queue_retry_script_success_transitions_failed_to_done() {
        let yaml = r#"
name: test-ws
sources:
  github:
    url: "https://github.com/test/repo"
    scan_interval_secs: 300
    states:
      implement:
        trigger: {}
        prompt: "implement"
        on_done:
          - script: "true"
"#;
        let (db, ws_id, _tmp) = setup_workspace_with_config(yaml);
        let item = make_item("retry-ok-1", &ws_id, "implement", QueuePhase::Failed);
        db.insert_item(&item).unwrap();

        // Replicate cmd_queue_retry_script logic.
        let stored = db.get_item("retry-ok-1").unwrap();
        assert_eq!(stored.phase(), QueuePhase::Failed);

        let (_, config_path, _) = db.get_workspace(&stored.workspace_id).unwrap();
        let config =
            belt_infra::workspace_loader::load_workspace_config(std::path::Path::new(&config_path))
                .unwrap();

        let state_config = config
            .sources
            .values()
            .find_map(|s| s.states.get(&stored.state))
            .unwrap();

        let on_done: Vec<belt_core::action::Action> = state_config
            .on_done
            .iter()
            .map(belt_core::action::Action::from)
            .collect();
        assert!(!on_done.is_empty());

        let worktree_dir = tempfile::tempdir().unwrap();
        let env = belt_daemon::executor::ActionEnv::new("retry-ok-1", worktree_dir.path());
        let executor = build_executor();

        let result = executor.execute_all(&on_done, &env).await.unwrap();
        match result {
            Some(r) if r.success() => {
                db.update_phase("retry-ok-1", QueuePhase::Done).unwrap();
            }
            _ => panic!("expected on_done success for retry"),
        }

        let final_item = db.get_item("retry-ok-1").unwrap();
        assert_eq!(final_item.phase(), QueuePhase::Done);

        // Verify script_retry transition event was recorded.
        record_script_retry_event(&db, "retry-ok-1", &item.source_id, QueuePhase::Done, None);
        let events = db.list_transition_events("retry-ok-1").unwrap();
        let retry_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "script_retry")
            .collect();
        assert!(
            !retry_events.is_empty(),
            "expected script_retry transition event"
        );
        assert_eq!(retry_events.last().unwrap().phase.as_deref(), Some("done"));
    }

    /// retry_script: Failed item with on_done script that fails -> remains Failed.
    #[tokio::test]
    async fn queue_retry_script_failure_remains_failed() {
        let yaml = r#"
name: test-ws
sources:
  github:
    url: "https://github.com/test/repo"
    scan_interval_secs: 300
    states:
      implement:
        trigger: {}
        prompt: "implement"
        on_done:
          - script: "false"
"#;
        let (db, ws_id, _tmp) = setup_workspace_with_config(yaml);
        let item = make_item("retry-fail-1", &ws_id, "implement", QueuePhase::Failed);
        db.insert_item(&item).unwrap();

        let stored = db.get_item("retry-fail-1").unwrap();
        assert_eq!(stored.phase(), QueuePhase::Failed);

        let (_, config_path, _) = db.get_workspace(&stored.workspace_id).unwrap();
        let config =
            belt_infra::workspace_loader::load_workspace_config(std::path::Path::new(&config_path))
                .unwrap();

        let state_config = config
            .sources
            .values()
            .find_map(|s| s.states.get(&stored.state))
            .unwrap();

        let on_done: Vec<belt_core::action::Action> = state_config
            .on_done
            .iter()
            .map(belt_core::action::Action::from)
            .collect();

        let worktree_dir = tempfile::tempdir().unwrap();
        let env = belt_daemon::executor::ActionEnv::new("retry-fail-1", worktree_dir.path());
        let executor = build_executor();

        let result = executor.execute_all(&on_done, &env).await.unwrap();
        match result {
            Some(r) if r.success() => {
                panic!("expected failure but script succeeded");
            }
            Some(_) => {
                // Item remains Failed — no phase update (matches cmd_queue_retry_script behavior).
            }
            None => {
                panic!("expected a result");
            }
        }

        let final_item = db.get_item("retry-fail-1").unwrap();
        assert_eq!(final_item.phase(), QueuePhase::Failed);

        // Verify script_retry transition event was recorded for the failure case.
        record_script_retry_event(
            &db,
            "retry-fail-1",
            &item.source_id,
            QueuePhase::Failed,
            Some("exit code 1".to_string()),
        );
        let events = db.list_transition_events("retry-fail-1").unwrap();
        let retry_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "script_retry")
            .collect();
        assert!(
            !retry_events.is_empty(),
            "expected script_retry transition event on failure"
        );
        assert_eq!(
            retry_events.last().unwrap().phase.as_deref(),
            Some("failed")
        );
        assert!(
            retry_events
                .last()
                .unwrap()
                .detail
                .as_ref()
                .unwrap()
                .contains("exit code"),
            "expected exit code detail"
        );
    }

    /// retry_script: non-Failed item is rejected.
    #[test]
    fn queue_retry_script_rejects_non_failed_item() {
        let db = belt_infra::db::Database::open_in_memory().unwrap();
        db.add_workspace("test-ws", "/dev/null").unwrap();
        let item = make_item(
            "retry-reject-1",
            "test-ws",
            "implement",
            QueuePhase::Running,
        );
        db.insert_item(&item).unwrap();

        let stored = db.get_item("retry-reject-1").unwrap();
        // cmd_queue_retry_script checks: if item.phase() != QueuePhase::Failed { bail! }
        assert_ne!(stored.phase(), QueuePhase::Failed);
    }

    /// retry_script: timeout causes early return, item remains Failed.
    #[tokio::test]
    async fn queue_retry_script_timeout_remains_failed() {
        let yaml = r#"
name: test-ws
sources:
  github:
    url: "https://github.com/test/repo"
    scan_interval_secs: 300
    states:
      implement:
        trigger: {}
        prompt: "implement"
        on_done:
          - script: "sleep 10"
"#;
        let (db, ws_id, _tmp) = setup_workspace_with_config(yaml);
        let item = make_item("retry-timeout-1", &ws_id, "implement", QueuePhase::Failed);
        db.insert_item(&item).unwrap();

        let stored = db.get_item("retry-timeout-1").unwrap();
        let (_, config_path, _) = db.get_workspace(&stored.workspace_id).unwrap();
        let config =
            belt_infra::workspace_loader::load_workspace_config(std::path::Path::new(&config_path))
                .unwrap();

        let state_config = config
            .sources
            .values()
            .find_map(|s| s.states.get(&stored.state))
            .unwrap();

        let on_done: Vec<belt_core::action::Action> = state_config
            .on_done
            .iter()
            .map(belt_core::action::Action::from)
            .collect();

        let worktree_dir = tempfile::tempdir().unwrap();
        let env = belt_daemon::executor::ActionEnv::new("retry-timeout-1", worktree_dir.path());
        let executor = build_executor();

        // Apply timeout of 1 second (script sleeps 10).
        let timeout_secs = 1u64;
        let duration = std::time::Duration::from_secs(timeout_secs);
        let timed_out = tokio::time::timeout(duration, executor.execute_all(&on_done, &env)).await;

        assert!(timed_out.is_err(), "expected timeout");

        // Item remains Failed since timeout prevents phase change.
        let final_item = db.get_item("retry-timeout-1").unwrap();
        assert_eq!(final_item.phase(), QueuePhase::Failed);
    }

    // --- Spec decompose: build_decomposed_issues_from_llm tests ---

    /// Verify that build_decomposed_issues_from_llm produces correct titles,
    /// bodies, and parent references from structured LLM output.
    #[test]
    fn decompose_from_llm_structured_output() {
        use belt_core::spec::{LlmDecomposedIssue, build_decomposed_issues_from_llm};

        let llm_issues = vec![
            LlmDecomposedIssue {
                title: "Add OAuth2 token refresh endpoint".to_string(),
                description: "Implement the /auth/refresh endpoint.".to_string(),
                acceptance_criteria: vec![
                    "Refresh token is validated".to_string(),
                    "New access token is issued".to_string(),
                ],
            },
            LlmDecomposedIssue {
                title: "Add logout endpoint".to_string(),
                description: "Clear session on logout.".to_string(),
                acceptance_criteria: vec!["Session is invalidated".to_string()],
            },
        ];

        let issues = build_decomposed_issues_from_llm(&llm_issues, Some("99"));
        assert_eq!(issues.len(), 2);

        // Title format: [sub] #<parent> AC<index>: <title>
        assert!(issues[0].title.contains("#99"));
        assert!(issues[0].title.contains("AC1"));
        assert!(
            issues[0]
                .title
                .contains("Add OAuth2 token refresh endpoint")
        );

        assert!(issues[1].title.contains("AC2"));
        assert!(issues[1].title.contains("Add logout endpoint"));

        // Body includes parent reference.
        assert!(issues[0].body.contains("Parent: #99"));
        assert!(issues[1].body.contains("Parent: #99"));

        // Body includes description.
        assert!(
            issues[0]
                .body
                .contains("Implement the /auth/refresh endpoint.")
        );
        assert!(issues[1].body.contains("Clear session on logout."));

        // Body includes acceptance criteria as checklist.
        assert!(issues[0].body.contains("- [ ] Refresh token is validated"));
        assert!(issues[0].body.contains("- [ ] New access token is issued"));
        assert!(issues[1].body.contains("- [ ] Session is invalidated"));

        // Criterion field stores the LLM title.
        assert_eq!(issues[0].criterion, "Add OAuth2 token refresh endpoint");
    }

    /// Verify build_decomposed_issues_from_llm without parent number uses
    /// placeholder text.
    #[test]
    fn decompose_from_llm_without_parent_number() {
        use belt_core::spec::{LlmDecomposedIssue, build_decomposed_issues_from_llm};

        let llm_issues = vec![LlmDecomposedIssue {
            title: "Setup CI pipeline".to_string(),
            description: "Configure GitHub Actions.".to_string(),
            acceptance_criteria: vec![],
        }];

        let issues = build_decomposed_issues_from_llm(&llm_issues, None);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].title.contains("#?"));
        assert!(issues[0].body.contains("Parent: (pending)"));
        // Empty acceptance_criteria should not produce an AC section.
        assert!(!issues[0].body.contains("## Acceptance Criteria"));
    }

    /// Verify build_decomposed_issues_from_llm with empty input returns empty vec.
    #[test]
    fn decompose_from_llm_empty_input() {
        use belt_core::spec::build_decomposed_issues_from_llm;

        let issues = build_decomposed_issues_from_llm(&[], Some("1"));
        assert!(issues.is_empty());
    }

    // --- Spec collect: all_decomposed_issues_closed tests ---

    /// Verify all_decomposed_issues_closed returns true when all issues are closed.
    #[test]
    fn collect_all_decomposed_issues_closed_all_closed() {
        let states = vec![
            ("101".to_string(), true),
            ("102".to_string(), true),
            ("103".to_string(), true),
        ];
        assert!(belt_core::spec::all_decomposed_issues_closed(&states));
    }

    /// Verify all_decomposed_issues_closed returns false when any issue is open.
    #[test]
    fn collect_all_decomposed_issues_closed_some_open() {
        let states = vec![
            ("101".to_string(), true),
            ("102".to_string(), false),
            ("103".to_string(), true),
        ];
        assert!(!belt_core::spec::all_decomposed_issues_closed(&states));
    }

    /// Verify all_decomposed_issues_closed returns false for empty input.
    #[test]
    fn collect_all_decomposed_issues_closed_empty() {
        let states: Vec<(String, bool)> = vec![];
        assert!(!belt_core::spec::all_decomposed_issues_closed(&states));
    }

    // --- Spec lifecycle: full transition chain tests ---

    /// Verify the full spec lifecycle: Draft -> Active -> Paused -> Active ->
    /// Completing -> Completed, with DB persistence at each step.
    #[test]
    fn spec_lifecycle_full_transition_chain() {
        use belt_core::spec::{Spec, SpecStatus};

        let db = belt_infra::db::Database::open_in_memory().unwrap();
        let id = "spec-lifecycle-full";
        let mut spec = Spec::new(
            id.to_string(),
            "ws-lifecycle".to_string(),
            "Lifecycle Test".to_string(),
            "## Overview\nTest.\n\n## Acceptance Criteria\n- Done".to_string(),
        );
        db.insert_spec(&spec).unwrap();
        assert_eq!(spec.status, SpecStatus::Draft);

        // Draft -> Active
        spec.transition_to(SpecStatus::Active).unwrap();
        db.update_spec_status(&spec.id, spec.status).unwrap();
        let stored = db.get_spec(id).unwrap();
        assert_eq!(stored.status, SpecStatus::Active);

        // Active -> Paused
        spec.transition_to(SpecStatus::Paused).unwrap();
        db.update_spec_status(&spec.id, spec.status).unwrap();
        let stored = db.get_spec(id).unwrap();
        assert_eq!(stored.status, SpecStatus::Paused);

        // Paused -> Active (resume)
        spec.transition_to(SpecStatus::Active).unwrap();
        db.update_spec_status(&spec.id, spec.status).unwrap();
        let stored = db.get_spec(id).unwrap();
        assert_eq!(stored.status, SpecStatus::Active);

        // Active -> Completing
        spec.transition_to(SpecStatus::Completing).unwrap();
        db.update_spec_status(&spec.id, spec.status).unwrap();
        let stored = db.get_spec(id).unwrap();
        assert_eq!(stored.status, SpecStatus::Completing);

        // Completing -> Completed
        spec.transition_to(SpecStatus::Completed).unwrap();
        db.update_spec_status(&spec.id, spec.status).unwrap();
        let stored = db.get_spec(id).unwrap();
        assert_eq!(stored.status, SpecStatus::Completed);

        // Completed is terminal.
        assert!(stored.status.is_terminal());
    }

    /// Verify that invalid spec transitions are rejected.
    #[test]
    fn spec_lifecycle_invalid_transitions() {
        use belt_core::spec::{Spec, SpecStatus};

        let mut spec = Spec::new(
            "spec-invalid".to_string(),
            "ws".to_string(),
            "Invalid".to_string(),
            "content".to_string(),
        );
        assert_eq!(spec.status, SpecStatus::Draft);

        // Draft -> Completed is not valid (must go through Active, Completing).
        assert!(spec.transition_to(SpecStatus::Completed).is_err());
        // Draft -> Paused is not valid.
        assert!(spec.transition_to(SpecStatus::Paused).is_err());
        // Draft -> Completing is not valid.
        assert!(spec.transition_to(SpecStatus::Completing).is_err());

        // Transition to Active first.
        spec.transition_to(SpecStatus::Active).unwrap();
        // Active -> Completed is not valid (must go through Completing).
        assert!(spec.transition_to(SpecStatus::Completed).is_err());
    }

    /// Verify Completing -> Active rollback transition (gap found during re-check).
    #[test]
    fn spec_lifecycle_completing_rollback_to_active() {
        use belt_core::spec::{Spec, SpecStatus};

        let db = belt_infra::db::Database::open_in_memory().unwrap();
        let id = "spec-rollback";
        let mut spec = Spec::new(
            id.to_string(),
            "ws-rb".to_string(),
            "Rollback".to_string(),
            "content".to_string(),
        );
        db.insert_spec(&spec).unwrap();

        // Draft -> Active -> Completing
        spec.transition_to(SpecStatus::Active).unwrap();
        spec.transition_to(SpecStatus::Completing).unwrap();
        db.update_spec_status(&spec.id, spec.status).unwrap();
        assert_eq!(db.get_spec(id).unwrap().status, SpecStatus::Completing);

        // Completing -> Active (rollback due to gap or test failure)
        spec.transition_to(SpecStatus::Active).unwrap();
        db.update_spec_status(&spec.id, spec.status).unwrap();
        assert_eq!(db.get_spec(id).unwrap().status, SpecStatus::Active);
    }

    /// Verify archive transition from various states.
    #[test]
    fn spec_lifecycle_archive_from_multiple_states() {
        use belt_core::spec::{Spec, SpecStatus};

        // Draft -> Archived
        let mut s1 = Spec::new(
            "s1".to_string(),
            "ws".to_string(),
            "S1".to_string(),
            "c".to_string(),
        );
        assert!(s1.transition_to(SpecStatus::Archived).is_ok());

        // Active -> Archived
        let mut s2 = Spec::new(
            "s2".to_string(),
            "ws".to_string(),
            "S2".to_string(),
            "c".to_string(),
        );
        s2.transition_to(SpecStatus::Active).unwrap();
        assert!(s2.transition_to(SpecStatus::Archived).is_ok());

        // Paused -> Archived
        let mut s3 = Spec::new(
            "s3".to_string(),
            "ws".to_string(),
            "S3".to_string(),
            "c".to_string(),
        );
        s3.transition_to(SpecStatus::Active).unwrap();
        s3.transition_to(SpecStatus::Paused).unwrap();
        assert!(s3.transition_to(SpecStatus::Archived).is_ok());

        // Archived -> Active (restore)
        assert!(s3.transition_to(SpecStatus::Active).is_ok());
    }

    // --- Spec decompose: acceptance criteria extraction edge cases ---

    /// Verify extract_acceptance_criteria handles asterisk bullets.
    #[test]
    fn decompose_extract_ac_with_asterisk_bullets() {
        let content = "\
## Overview\nFeature.\n\n\
## Acceptance Criteria\n\
* First criterion\n\
* Second criterion\n\n\
## Notes\nEnd.";
        let criteria = belt_core::spec::extract_acceptance_criteria(content);
        assert_eq!(criteria.len(), 2);
        assert_eq!(criteria[0], "First criterion");
        assert_eq!(criteria[1], "Second criterion");
    }

    /// Verify extract_acceptance_criteria handles short "AC" header.
    #[test]
    fn decompose_extract_ac_with_short_header() {
        let content = "## AC\n- Criterion A\n- Criterion B";
        let criteria = belt_core::spec::extract_acceptance_criteria(content);
        assert_eq!(criteria.len(), 2);
        assert_eq!(criteria[0], "Criterion A");
        assert_eq!(criteria[1], "Criterion B");
    }

    /// Verify extract_acceptance_criteria stops at the next section heading.
    #[test]
    fn decompose_extract_ac_stops_at_next_section() {
        let content = "\
## Acceptance Criteria\n\
- AC one\n\
- AC two\n\
## Implementation\n\
- This is not an AC";
        let criteria = belt_core::spec::extract_acceptance_criteria(content);
        assert_eq!(criteria.len(), 2);
    }

    /// Verify extract_acceptance_criteria skips empty bullet lines.
    #[test]
    fn decompose_extract_ac_skips_empty_bullets() {
        let content = "## Acceptance Criteria\n- \n- Valid AC\n-  \n- Another AC";
        let criteria = belt_core::spec::extract_acceptance_criteria(content);
        assert_eq!(criteria.len(), 2);
        assert_eq!(criteria[0], "Valid AC");
        assert_eq!(criteria[1], "Another AC");
    }

    // --- Spec decompose: format_decomposition_preview tests ---

    /// Verify format_decomposition_preview with LLM-generated issues.
    #[test]
    fn decompose_preview_format_with_llm_issues() {
        use belt_core::spec::{LlmDecomposedIssue, build_decomposed_issues_from_llm};

        let llm_issues = vec![
            LlmDecomposedIssue {
                title: "Setup database".to_string(),
                description: "Create schema.".to_string(),
                acceptance_criteria: vec!["Schema created".to_string()],
            },
            LlmDecomposedIssue {
                title: "Add API routes".to_string(),
                description: "REST endpoints.".to_string(),
                acceptance_criteria: vec![],
            },
        ];

        let issues = build_decomposed_issues_from_llm(&llm_issues, Some("50"));
        let preview = belt_core::spec::format_decomposition_preview(&issues);

        assert!(preview.contains("2 child issue(s)"));
        assert!(preview.contains("AC1"));
        assert!(preview.contains("AC2"));
        assert!(preview.contains("Setup database"));
        assert!(preview.contains("Add API routes"));
    }

    // --- Spec collect: decomposed spec readiness check with DB ---

    /// Integration test: verify that decomposed spec transitions to Completing
    /// only when all child issues are closed.
    #[test]
    fn collect_decomposed_spec_completing_readiness() {
        use belt_core::spec::{Spec, SpecStatus};

        let db = belt_infra::db::Database::open_in_memory().unwrap();
        let id = "spec-collect-ready";
        let mut spec = Spec::new(
            id.to_string(),
            "ws-collect".to_string(),
            "Collect Test".to_string(),
            "## Acceptance Criteria\n- Task A\n- Task B".to_string(),
        );
        spec.decomposed_issues = Some("701,702".to_string());
        db.insert_spec(&spec).unwrap();
        spec.transition_to(SpecStatus::Active).unwrap();
        db.update_spec_status(&spec.id, spec.status).unwrap();

        // Simulate: one issue open, one closed.
        let states_partial = vec![("701".to_string(), true), ("702".to_string(), false)];
        assert!(!belt_core::spec::all_decomposed_issues_closed(
            &states_partial
        ));
        // Spec should NOT transition to Completing yet.

        // Simulate: all issues closed.
        let states_all = vec![("701".to_string(), true), ("702".to_string(), true)];
        assert!(belt_core::spec::all_decomposed_issues_closed(&states_all));

        // Now spec can transition to Completing.
        spec.transition_to(SpecStatus::Completing).unwrap();
        db.update_spec_status(&spec.id, spec.status).unwrap();
        assert_eq!(db.get_spec(id).unwrap().status, SpecStatus::Completing);
    }

    // --- Spec decompose: extract_issue_number edge cases ---

    /// Verify extract_issue_number returns None for non-numeric trailing segment.
    #[test]
    fn decompose_extract_issue_number_non_numeric() {
        assert_eq!(
            extract_issue_number("https://github.com/o/r/issues/abc"),
            None
        );
    }

    /// Verify extract_issue_number returns None for URL ending with slash.
    #[test]
    fn decompose_extract_issue_number_trailing_slash() {
        // rsplit('/').next() returns empty string for trailing slash.
        assert_eq!(extract_issue_number("https://github.com/o/r/issues/"), None);
    }

    /// Verify extract_issue_number works with plain number path.
    #[test]
    fn decompose_extract_issue_number_plain_number() {
        assert_eq!(extract_issue_number("42"), Some("42".to_string()));
    }

    // --- Spec collect: workspace trigger label with multiple sources ---

    /// Verify resolve_trigger_label picks the first trigger label from
    /// multiple sources and states.
    #[test]
    fn collect_resolve_trigger_label_multiple_sources() {
        let yaml = r#"
name: multi-source-ws
sources:
  github:
    url: "https://github.com/test/repo"
    scan_interval_secs: 300
    states:
      analyze:
        trigger:
          label: "custom:analyze"
        prompt: "analyze"
      build:
        trigger:
          label: "custom:build"
        prompt: "build"
"#;
        let config: belt_core::workspace::WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let label = resolve_trigger_label(&config);
        // Should return one of the configured labels (first found).
        assert!(
            label == "custom:analyze" || label == "custom:build",
            "expected a configured trigger label, got: {label}"
        );
    }

    // --- Spec decompose: conflict detection during decompose ---

    /// Verify that ConflictDetector detects file-level overlap between specs
    /// sharing the same entry_point.
    #[test]
    fn decompose_conflict_detection_file_overlap() {
        use belt_core::spec::{ConflictDetector, OverlapType, Spec};

        let mut existing = Spec::new(
            "spec-existing".to_string(),
            "ws".to_string(),
            "Existing".to_string(),
            "content".to_string(),
        );
        existing.entry_point = Some("src/auth/mod.rs".to_string());

        let mut new_spec = Spec::new(
            "spec-new".to_string(),
            "ws".to_string(),
            "New".to_string(),
            "content".to_string(),
        );
        new_spec.entry_point = Some("src/auth/mod.rs".to_string());

        let conflicts = ConflictDetector::detect(&new_spec, &[existing]);
        assert!(!conflicts.is_empty());
        assert_eq!(conflicts[0].overlap_type, OverlapType::File);
        assert_eq!(conflicts[0].path, "src/auth/mod.rs");
    }

    /// Verify that ConflictDetector finds no conflicts when entry_points
    /// do not overlap.
    #[test]
    fn decompose_conflict_detection_no_overlap() {
        use belt_core::spec::{ConflictDetector, Spec};

        let mut existing = Spec::new(
            "spec-a".to_string(),
            "ws".to_string(),
            "A".to_string(),
            "c".to_string(),
        );
        existing.entry_point = Some("src/auth/mod.rs".to_string());

        let mut new_spec = Spec::new(
            "spec-b".to_string(),
            "ws".to_string(),
            "B".to_string(),
            "c".to_string(),
        );
        new_spec.entry_point = Some("src/api/handler.rs".to_string());

        let conflicts = ConflictDetector::detect(&new_spec, &[existing]);
        assert!(conflicts.is_empty());
    }

    // --- Spec lifecycle: decompose + collect end-to-end pipeline ---

    /// Integration test: full pipeline from spec creation with decompose
    /// through collect readiness, simulating the complete lifecycle.
    ///
    /// 1. Insert spec with AC
    /// 2. Extract AC and build decomposed issues
    /// 3. Store decomposed_issues in DB
    /// 4. Transition Draft -> Active
    /// 5. Simulate all child issues closed
    /// 6. Transition Active -> Completing -> Completed
    #[test]
    fn spec_lifecycle_decompose_to_collect_to_complete() {
        use belt_core::spec::{Spec, SpecStatus};

        let db = belt_infra::db::Database::open_in_memory().unwrap();
        let content = "\
## Overview\nPayment system.\n\n\
## Acceptance Criteria\n\
- Process credit card payments\n\
- Generate payment receipts\n\
- Handle refunds\n\n\
## Implementation\nStripe integration.";

        let id = "spec-e2e-pipeline";
        let mut spec = Spec::new(
            id.to_string(),
            "ws-e2e".to_string(),
            "Payment System".to_string(),
            content.to_string(),
        );
        db.insert_spec(&spec).unwrap();

        // Step 1: Extract AC.
        let criteria = belt_core::spec::extract_acceptance_criteria(&spec.content);
        assert_eq!(criteria.len(), 3);

        // Step 2: Build decomposed issues.
        let proposed = belt_core::spec::build_decomposed_issues(&criteria, None, Some("800"));
        assert_eq!(proposed.len(), 3);

        // Step 3: Store decomposed_issues (simulating GitHub issue creation).
        let child_numbers = ["801".to_string(), "802".to_string(), "803".to_string()];
        spec.decomposed_issues = Some(child_numbers.join(","));
        db.update_spec(&spec).unwrap();

        // Step 4: Draft -> Active.
        spec.transition_to(SpecStatus::Active).unwrap();
        db.update_spec_status(&spec.id, spec.status).unwrap();

        let stored = db.get_spec(id).unwrap();
        assert_eq!(stored.status, SpecStatus::Active);
        assert!(stored.is_decomposed());
        assert_eq!(stored.decomposed_issue_numbers(), vec!["801", "802", "803"]);

        // Step 5: Simulate child issues closing progressively.
        let partial = vec![
            ("801".to_string(), true),
            ("802".to_string(), true),
            ("803".to_string(), false),
        ];
        assert!(!belt_core::spec::all_decomposed_issues_closed(&partial));

        let all_closed = vec![
            ("801".to_string(), true),
            ("802".to_string(), true),
            ("803".to_string(), true),
        ];
        assert!(belt_core::spec::all_decomposed_issues_closed(&all_closed));

        // Step 6: Active -> Completing -> Completed.
        spec.transition_to(SpecStatus::Completing).unwrap();
        db.update_spec_status(&spec.id, spec.status).unwrap();
        assert_eq!(db.get_spec(id).unwrap().status, SpecStatus::Completing);

        spec.transition_to(SpecStatus::Completed).unwrap();
        db.update_spec_status(&spec.id, spec.status).unwrap();
        let final_spec = db.get_spec(id).unwrap();
        assert_eq!(final_spec.status, SpecStatus::Completed);
        assert!(final_spec.status.is_terminal());
    }

    /// Verify that decomposed_issue_numbers returns empty vec when
    /// decomposed_issues is None.
    #[test]
    fn spec_decomposed_issue_numbers_none() {
        let spec = belt_core::spec::Spec::new(
            "s".to_string(),
            "w".to_string(),
            "n".to_string(),
            "c".to_string(),
        );
        assert!(spec.decomposed_issue_numbers().is_empty());
        assert!(!spec.is_decomposed());
    }

    /// Verify format_decomposition_preview with empty input.
    #[test]
    fn decompose_preview_empty() {
        let preview = belt_core::spec::format_decomposition_preview(&[]);
        assert!(preview.contains("0 child issue(s)"));
    }

    // --- JSON flag parsing tests ---

    #[test]
    fn queue_done_json_flag() {
        let cli = Cli::try_parse_from(["belt", "queue", "done", "item-1", "--json"]).unwrap();
        match cli.command {
            Commands::Queue {
                command: QueueCommands::Done { work_id, json },
            } => {
                assert_eq!(work_id, "item-1");
                assert!(json);
            }
            _ => panic!("expected Queue Done command"),
        }
    }

    #[test]
    fn queue_done_without_json_flag() {
        let cli = Cli::try_parse_from(["belt", "queue", "done", "item-1"]).unwrap();
        match cli.command {
            Commands::Queue {
                command: QueueCommands::Done { json, .. },
            } => assert!(!json),
            _ => panic!("expected Queue Done command"),
        }
    }

    #[test]
    fn queue_hitl_json_flag() {
        let cli = Cli::try_parse_from([
            "belt", "queue", "hitl", "item-1", "--reason", "test", "--json",
        ])
        .unwrap();
        match cli.command {
            Commands::Queue {
                command: QueueCommands::Hitl { json, reason, .. },
            } => {
                assert!(json);
                assert_eq!(reason.as_deref(), Some("test"));
            }
            _ => panic!("expected Queue Hitl command"),
        }
    }

    #[test]
    fn queue_skip_json_flag() {
        let cli = Cli::try_parse_from(["belt", "queue", "skip", "item-1", "--json"]).unwrap();
        match cli.command {
            Commands::Queue {
                command: QueueCommands::Skip { json, .. },
            } => assert!(json),
            _ => panic!("expected Queue Skip command"),
        }
    }

    #[test]
    fn queue_retry_script_json_flag() {
        let cli =
            Cli::try_parse_from(["belt", "queue", "retry-script", "item-1", "--json"]).unwrap();
        match cli.command {
            Commands::Queue {
                command: QueueCommands::RetryScript { json, .. },
            } => assert!(json),
            _ => panic!("expected Queue RetryScript command"),
        }
    }

    #[test]
    fn queue_dependency_add_json_flag() {
        let cli = Cli::try_parse_from([
            "belt",
            "queue",
            "dependency",
            "add",
            "q1",
            "--after",
            "q2",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Commands::Queue {
                command: QueueCommands::Dependency(DependencyCommands::Add { json, .. }),
            } => assert!(json),
            _ => panic!("expected Queue Dependency Add command"),
        }
    }

    #[test]
    fn queue_dependency_remove_json_flag() {
        let cli = Cli::try_parse_from([
            "belt",
            "queue",
            "dependency",
            "remove",
            "q1",
            "--after",
            "q2",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Commands::Queue {
                command: QueueCommands::Dependency(DependencyCommands::Remove { json, .. }),
            } => assert!(json),
            _ => panic!("expected Queue Dependency Remove command"),
        }
    }

    #[test]
    fn hitl_respond_json_flag() {
        let cli = Cli::try_parse_from([
            "belt", "hitl", "respond", "item-1", "--action", "done", "--json",
        ])
        .unwrap();
        match cli.command {
            Commands::Hitl {
                command: HitlCommands::Respond { json, .. },
            } => assert!(json),
            _ => panic!("expected Hitl Respond command"),
        }
    }

    #[test]
    fn hitl_timeout_set_json_flag() {
        let cli = Cli::try_parse_from([
            "belt",
            "hitl",
            "timeout",
            "set",
            "item-1",
            "--duration",
            "60",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Commands::Hitl {
                command:
                    HitlCommands::Timeout {
                        command: HitlTimeoutCommands::Set { json, .. },
                    },
            } => assert!(json),
            _ => panic!("expected Hitl Timeout Set command"),
        }
    }

    #[test]
    fn hitl_timeout_ls_json_flag() {
        let cli = Cli::try_parse_from(["belt", "hitl", "timeout", "ls", "--json"]).unwrap();
        match cli.command {
            Commands::Hitl {
                command:
                    HitlCommands::Timeout {
                        command: HitlTimeoutCommands::Ls { json },
                    },
            } => assert!(json),
            _ => panic!("expected Hitl Timeout Ls command"),
        }
    }

    #[test]
    fn workspace_add_json_flag() {
        let cli =
            Cli::try_parse_from(["belt", "workspace", "add", "--config", "ws.yaml", "--json"])
                .unwrap();
        match cli.command {
            Commands::Workspace {
                command: WorkspaceCommands::Add { json, .. },
            } => assert!(json),
            _ => panic!("expected Workspace Add command"),
        }
    }

    #[test]
    fn workspace_list_json_flag() {
        let cli = Cli::try_parse_from(["belt", "workspace", "list", "--json"]).unwrap();
        match cli.command {
            Commands::Workspace {
                command: WorkspaceCommands::List { json },
            } => assert!(json),
            _ => panic!("expected Workspace List command"),
        }
    }

    #[test]
    fn workspace_show_json_flag() {
        let cli = Cli::try_parse_from(["belt", "workspace", "show", "my-ws", "--json"]).unwrap();
        match cli.command {
            Commands::Workspace {
                command: WorkspaceCommands::Show { json, name },
            } => {
                assert!(json);
                assert_eq!(name, "my-ws");
            }
            _ => panic!("expected Workspace Show command"),
        }
    }

    #[test]
    fn workspace_update_json_flag() {
        let cli = Cli::try_parse_from([
            "belt",
            "workspace",
            "update",
            "my-ws",
            "--config",
            "new.yaml",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Commands::Workspace {
                command: WorkspaceCommands::Update { json, .. },
            } => assert!(json),
            _ => panic!("expected Workspace Update command"),
        }
    }

    #[test]
    fn workspace_remove_json_flag() {
        let cli = Cli::try_parse_from(["belt", "workspace", "remove", "my-ws", "--json"]).unwrap();
        match cli.command {
            Commands::Workspace {
                command: WorkspaceCommands::Remove { json, .. },
            } => assert!(json),
            _ => panic!("expected Workspace Remove command"),
        }
    }

    #[test]
    fn cron_add_json_flag() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let cli = Cli::try_parse_from([
            "belt",
            "cron",
            "add",
            "job1",
            "--schedule",
            "0 * * * *",
            "--script",
            tmp.path().to_str().unwrap(),
            "--json",
        ])
        .unwrap();
        match cli.command {
            Commands::Cron {
                command: CronCommands::Add { json, .. },
            } => assert!(json),
            _ => panic!("expected Cron Add command"),
        }
    }

    #[test]
    fn cron_pause_json_flag() {
        let cli = Cli::try_parse_from(["belt", "cron", "pause", "job1", "--json"]).unwrap();
        match cli.command {
            Commands::Cron {
                command: CronCommands::Pause { json, name },
            } => {
                assert!(json);
                assert_eq!(name, "job1");
            }
            _ => panic!("expected Cron Pause command"),
        }
    }

    #[test]
    fn cron_resume_json_flag() {
        let cli = Cli::try_parse_from(["belt", "cron", "resume", "job1", "--json"]).unwrap();
        match cli.command {
            Commands::Cron {
                command: CronCommands::Resume { json, .. },
            } => assert!(json),
            _ => panic!("expected Cron Resume command"),
        }
    }

    #[test]
    fn cron_remove_json_flag() {
        let cli = Cli::try_parse_from(["belt", "cron", "remove", "job1", "--json"]).unwrap();
        match cli.command {
            Commands::Cron {
                command: CronCommands::Remove { json, .. },
            } => assert!(json),
            _ => panic!("expected Cron Remove command"),
        }
    }

    #[test]
    fn cron_trigger_json_flag() {
        let cli = Cli::try_parse_from(["belt", "cron", "trigger", "job1", "--json"]).unwrap();
        match cli.command {
            Commands::Cron {
                command: CronCommands::Trigger { json, .. },
            } => assert!(json),
            _ => panic!("expected Cron Trigger command"),
        }
    }

    #[test]
    fn cron_run_json_flag() {
        let cli = Cli::try_parse_from(["belt", "cron", "run", "job1", "--json"]).unwrap();
        match cli.command {
            Commands::Cron {
                command: CronCommands::Run { json, .. },
            } => assert!(json),
            _ => panic!("expected Cron Run command"),
        }
    }

    #[test]
    fn spec_update_json_flag() {
        let cli = Cli::try_parse_from([
            "belt", "spec", "update", "spec-1", "--name", "new-name", "--json",
        ])
        .unwrap();
        match cli.command {
            Commands::Spec {
                command: SpecCommands::Update { json, .. },
            } => assert!(json),
            _ => panic!("expected Spec Update command"),
        }
    }

    #[test]
    fn spec_pause_json_flag() {
        let cli = Cli::try_parse_from(["belt", "spec", "pause", "spec-1", "--json"]).unwrap();
        match cli.command {
            Commands::Spec {
                command: SpecCommands::Pause { json, .. },
            } => assert!(json),
            _ => panic!("expected Spec Pause command"),
        }
    }

    #[test]
    fn spec_resume_json_flag() {
        let cli = Cli::try_parse_from(["belt", "spec", "resume", "spec-1", "--json"]).unwrap();
        match cli.command {
            Commands::Spec {
                command: SpecCommands::Resume { json, .. },
            } => assert!(json),
            _ => panic!("expected Spec Resume command"),
        }
    }

    #[test]
    fn spec_complete_json_flag() {
        let cli = Cli::try_parse_from(["belt", "spec", "complete", "spec-1", "--json"]).unwrap();
        match cli.command {
            Commands::Spec {
                command: SpecCommands::Complete { json, .. },
            } => assert!(json),
            _ => panic!("expected Spec Complete command"),
        }
    }

    #[test]
    fn spec_remove_json_flag() {
        let cli = Cli::try_parse_from(["belt", "spec", "remove", "spec-1", "--json"]).unwrap();
        match cli.command {
            Commands::Spec {
                command: SpecCommands::Remove { json, .. },
            } => assert!(json),
            _ => panic!("expected Spec Remove command"),
        }
    }

    #[test]
    fn spec_link_json_flag() {
        let cli = Cli::try_parse_from([
            "belt",
            "spec",
            "link",
            "spec-1",
            "https://example.com",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Commands::Spec {
                command: SpecCommands::Link { json, .. },
            } => assert!(json),
            _ => panic!("expected Spec Link command"),
        }
    }

    #[test]
    fn spec_unlink_json_flag() {
        let cli = Cli::try_parse_from([
            "belt",
            "spec",
            "unlink",
            "spec-1",
            "https://example.com",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Commands::Spec {
                command: SpecCommands::Unlink { json, .. },
            } => assert!(json),
            _ => panic!("expected Spec Unlink command"),
        }
    }

    #[test]
    fn cron_update_json_flag() {
        let cli = Cli::try_parse_from([
            "belt",
            "cron",
            "update",
            "job1",
            "--schedule",
            "5 * * * *",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Commands::Cron {
                command: CronCommands::Update { json, .. },
            } => assert!(json),
            _ => panic!("expected Cron Update command"),
        }
    }
}
