use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use belt_core::phase::QueuePhase;
use belt_infra::db::Database;

mod agent;
mod bootstrap;
mod dashboard;
mod status;

use belt_core::runtime::RuntimeRegistry;
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
    /// Respond to a HITL request (the item's current request, or one by id).
    Respond {
        /// Queue item work_id.
        #[arg(required_unless_present = "hitl_id", conflicts_with = "hitl_id")]
        item_id: Option<String>,
        /// Address one specific (for example past) request instead of an item.
        #[arg(long)]
        hitl_id: Option<String>,
        /// Action to take: done, retry, skip, replan.
        #[arg(long)]
        action: String,
        /// Respondent name (defaults to the OS user).
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
        /// Terminal action when timeout fires.
        #[arg(long, value_parser = ["skip", "replan"])]
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
        /// Output as JSON (same as `--format json`).
        #[arg(long)]
        json: bool,
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
        db,
    )
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
    let result = daemon.run(tick_interval_secs).await;

    // Clean up PID file on graceful shutdown and on a failed start.
    if let Err(e) = std::fs::remove_file(&pid_path) {
        tracing::warn!("failed to remove PID file: {e}");
    }

    result.map_err(|e| anyhow::anyhow!("daemon failed to start: {e}"))
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

/// `belt queue show` -- show a queue item with its transition history.
fn cmd_queue_show(work_id: &str, format: &str, json: bool) -> anyhow::Result<()> {
    let db = open_db()?;
    let item = db.get_item(work_id)?;
    let transitions = db.transitions_of(work_id)?;
    let processing = processing_of_item(&db, &item)?;

    if json || format == "json" {
        let mut value = serde_json::to_value(&item)?;
        merge_json(
            &mut value,
            serde_json::json!({
                "processing": processing.map(processing_name),
                "transitions": transitions.iter().map(|t| serde_json::json!({
                    "seq": t.seq,
                    "kind": t.kind,
                    "from_phase": t.from_phase,
                    "to_phase": t.to_phase,
                    "actor": t.actor,
                    "reason": t.reason,
                    "detail": t.detail,
                    "created_at": t.created_at,
                })).collect::<Vec<_>>(),
            }),
        );
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    println!("Work ID:      {}", item.work_id);
    println!("Source ID:    {}", item.source_id);
    println!("Workspace:    {}", item.workspace_id);
    println!("State:        {}", item.state);
    println!("Phase:        {}", item.phase());
    if let Some(p) = processing {
        println!("Processing:   {}", processing_name(p));
    }
    if let Some(title) = &item.title {
        println!("Title:        {title}");
    }
    if let Some(origin) = &item.derived_from {
        println!("Derived from: {origin}");
    }
    println!("Lineage root: {}", item.lineage_root);
    println!("Created:      {}", item.created_at);
    println!("Updated:      {}", item.updated_at);
    println!("History:");
    for t in &transitions {
        let phases = match (&t.from_phase, &t.to_phase) {
            (Some(from), Some(to)) => format!("{from} -> {to}"),
            (None, Some(to)) => format!("-> {to}"),
            (Some(from), None) => format!("{from} ->"),
            (None, None) => String::new(),
        };
        let mut line = format!(
            "  #{} {} {} {phases} [{}]",
            t.seq, t.created_at, t.kind, t.actor
        );
        if let Some(reason) = &t.reason {
            line.push_str(&format!(" reason={reason}"));
        }
        if let Some(detail) = &t.detail {
            line.push_str(&format!(" detail={detail}"));
        }
        println!("{line}");
    }

    Ok(())
}

/// Exit code of a command whose request was refused by the contract.
const EXIT_REFUSED: i32 = 1;

fn exit_if_refused(code: i32) {
    if code != 0 {
        std::process::exit(code);
    }
}

fn processing_name(processing: belt_core::transition::Processing) -> &'static str {
    match processing {
        belt_core::transition::Processing::Handler => "handler",
        belt_core::transition::Processing::PostProcessing => "post_processing",
    }
}

/// Whether the daemon owns the item right now: a handler is running, or a
/// confirmed HITL request awaits post-processing.
fn processing_of_item(
    db: &Database,
    item: &belt_core::queue::QueueItem,
) -> anyhow::Result<Option<belt_core::transition::Processing>> {
    use belt_core::transition::Processing;
    Ok(match item.phase() {
        QueuePhase::Running => Some(Processing::Handler),
        QueuePhase::Hitl => db
            .pending_post_processing()?
            .iter()
            .any(|r| r.work_id == item.work_id)
            .then_some(Processing::PostProcessing),
        QueuePhase::Pending
        | QueuePhase::Ready
        | QueuePhase::Completed
        | QueuePhase::Done
        | QueuePhase::Failed
        | QueuePhase::Skipped => None,
    })
}

fn merge_json(base: &mut serde_json::Value, extra: serde_json::Value) {
    if let (Some(base), serde_json::Value::Object(extra)) = (base.as_object_mut(), extra) {
        base.extend(extra);
    }
}

/// A request refused as a value; rendered as `{"success":false,"reason":...}`
/// with a non-zero exit code.
struct Refusal {
    reason: &'static str,
    fields: serde_json::Value,
    text: String,
}

impl Refusal {
    fn new(reason: &'static str, fields: serde_json::Value, text: impl Into<String>) -> Self {
        Self {
            reason,
            fields,
            text: text.into(),
        }
    }

    fn not_found() -> Self {
        Self::new(
            "not_found",
            serde_json::json!({}),
            "no such item or HITL request",
        )
    }

    fn from_transition(outcome: belt_core::transition::TransitionOutcome) -> Self {
        use belt_core::transition::TransitionOutcome;
        match outcome {
            TransitionOutcome::Busy { processing } => Self::new(
                "busy",
                serde_json::json!({ "processing": processing_name(processing) }),
                format!(
                    "busy: the item is being processed ({})",
                    processing_name(processing)
                ),
            ),
            TransitionOutcome::Conflict { current } => Self::new(
                "conflict",
                serde_json::json!({ "current": current.as_str() }),
                format!("conflict: another path already moved the item (now {current})"),
            ),
            TransitionOutcome::InvalidAction { current } => Self::new(
                "invalid_action",
                serde_json::json!({ "current": current.as_str() }),
                format!("invalid_action: not allowed while the item is {current}"),
            ),
            TransitionOutcome::Applied { .. } => {
                unreachable!("an applied transition is not a refusal")
            }
        }
    }
}

/// The `--json` body of a refusal.
fn refusal_value(work_id: &str, refusal: &Refusal) -> serde_json::Value {
    let mut value = serde_json::json!({
        "success": false,
        "reason": refusal.reason,
        "work_id": work_id,
    });
    merge_json(&mut value, refusal.fields.clone());
    value
}

fn emit_refusal(work_id: &str, json: bool, refusal: Refusal) -> anyhow::Result<i32> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&refusal_value(work_id, &refusal))?
        );
    } else {
        eprintln!("{work_id}: {}", refusal.text);
    }
    Ok(EXIT_REFUSED)
}

fn emit_success(
    work_id: &str,
    json: bool,
    result: &str,
    fields: serde_json::Value,
    text: String,
) -> anyhow::Result<i32> {
    if json {
        let mut value = serde_json::json!({
            "success": true,
            "result": result,
            "work_id": work_id,
        });
        merge_json(&mut value, fields);
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{text}");
    }
    Ok(0)
}

/// The item, or `None` when `work_id` is unknown.
fn find_item(db: &Database, work_id: &str) -> anyhow::Result<Option<belt_core::queue::QueueItem>> {
    match db.get_item(work_id) {
        Ok(item) => Ok(Some(item)),
        Err(belt_core::error::BeltError::ItemNotFound(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// What happened to a manual request to move an item to a target phase.
enum ManualOutcome {
    Applied,
    /// The request left Hitl, so it joined the HITL response race instead.
    HitlResponse {
        action: belt_core::hitl::HitlAction,
        /// Respondent the response was recorded for.
        by: String,
        outcome: belt_core::hitl::RespondOutcome,
    },
    Refused(belt_core::transition::TransitionOutcome),
}

/// The respondent recorded for CLI responses: the OS user, else `cli`.
fn cli_respondent() -> String {
    std::env::var("USER").unwrap_or_else(|_| "cli".to_string())
}

/// Answer a HITL request as the CLI. Local users are trusted, so no
/// allowlist applies.
fn respond_as_cli(
    hitl: &belt_daemon::hitl::HitlService,
    target: belt_infra::db::HitlTarget,
    action: belt_core::hitl::HitlAction,
    by: String,
    notes: Option<String>,
) -> anyhow::Result<belt_core::hitl::RespondOutcome> {
    Ok(hitl.respond(&belt_daemon::hitl::HitlResponse {
        target,
        action,
        by,
        via: "cli".to_string(),
        path: belt_core::hitl::ConfirmPath::Direct,
        notes,
    })?)
}

/// Request `item -> to` as the CLI.
///
/// A request that leaves Hitl is a HITL response when `hitl_response_for`
/// maps its target, so it is decided there before `transition` is asked:
/// `transition` reports `InvalidAction { Hitl }` for unmapped requests too.
/// An item whose request is already confirmed is processing, so it goes to
/// `transition` and is refused as `busy`.
fn request_manual_transition(
    hitl: &belt_daemon::hitl::HitlService,
    item: &belt_core::queue::QueueItem,
    to: QueuePhase,
    detail: Option<String>,
) -> anyhow::Result<ManualOutcome> {
    use belt_core::transition::{Actor, TransitionOutcome, TransitionReason, TransitionRequest};

    let db = hitl.database();
    if item.phase() == QueuePhase::Hitl
        && let Some(action) = belt_core::transition::hitl_response_for(to)
        && processing_of_item(db, item)?.is_none()
    {
        let by = cli_respondent();
        let outcome = respond_as_cli(
            hitl,
            belt_infra::db::HitlTarget::Item(item.work_id.clone()),
            action,
            by.clone(),
            None,
        )?;
        return Ok(ManualOutcome::HitlResponse {
            action,
            by,
            outcome,
        });
    }

    let outcome = db.transition(&TransitionRequest {
        work_id: item.work_id.clone(),
        expected_from: item.phase(),
        to,
        actor: Actor::Cli,
        reason: TransitionReason::Manual,
        detail,
    })?;
    Ok(match outcome {
        TransitionOutcome::Applied { .. } => ManualOutcome::Applied,
        refused => ManualOutcome::Refused(refused),
    })
}

/// The refusal for a HITL response that lost the race to `resolution`.
fn already_handled_refusal(resolution: &belt_core::hitl::HitlResolution) -> Refusal {
    Refusal::new(
        "already_handled",
        serde_json::json!({
            "by": resolution.by,
            "via": resolution.via,
            "action": resolution.action.to_string(),
            "at": resolution.at,
        }),
        format!(
            "already_handled: '{}' was chosen by {} via {} at {}",
            resolution.action, resolution.by, resolution.via, resolution.at
        ),
    )
}

/// Render a [`ManualOutcome`]; `applied_fields` extends the applied JSON.
fn emit_manual_outcome(
    work_id: &str,
    json: bool,
    to: QueuePhase,
    outcome: ManualOutcome,
    applied_fields: serde_json::Value,
    applied_text: String,
) -> anyhow::Result<i32> {
    use belt_core::hitl::RespondOutcome;

    match outcome {
        ManualOutcome::Applied => {
            let mut fields = serde_json::json!({ "phase": to.as_str() });
            merge_json(&mut fields, applied_fields);
            if to == QueuePhase::Failed {
                // The item did move, but the request (`queue done`) failed:
                // callers such as the evaluator judge the exit code.
                return emit_refusal(
                    work_id,
                    json,
                    Refusal::new("on_done_failed", fields, applied_text),
                );
            }
            emit_success(work_id, json, "applied", fields, applied_text)
        }
        ManualOutcome::Refused(refused) => {
            emit_refusal(work_id, json, Refusal::from_transition(refused))
        }
        ManualOutcome::HitlResponse {
            action,
            by,
            outcome,
        } => match outcome {
            RespondOutcome::Won { hitl_id } => emit_success(
                work_id,
                json,
                "hitl_response",
                serde_json::json!({
                    "action": action.to_string(),
                    "hitl_id": hitl_id.as_str(),
                    "by": by,
                    "via": "cli",
                }),
                format!(
                    "Recorded HITL response '{action}' by {by} via cli for {work_id}; the daemon applies it."
                ),
            ),
            RespondOutcome::AlreadyHandled(r) => {
                emit_refusal(work_id, json, already_handled_refusal(&r))
            }
            RespondOutcome::NotFound => emit_refusal(work_id, json, Refusal::not_found()),
            RespondOutcome::InvalidAction => emit_refusal(
                work_id,
                json,
                Refusal::new(
                    "invalid_action",
                    serde_json::json!({}),
                    "invalid_action: the HITL request does not accept this response",
                ),
            ),
            RespondOutcome::Unauthorized => emit_refusal(
                work_id,
                json,
                Refusal::new("unauthorized", serde_json::json!({}), "unauthorized"),
            ),
        },
    }
}

fn worktree_manager() -> anyhow::Result<GitWorktreeManager> {
    let worktree_base = belt_home()?.join("worktrees");
    Ok(GitWorktreeManager::new(
        worktree_base,
        std::path::PathBuf::from("."),
    ))
}

/// Cleanup after a terminal transition (matches daemon pattern: warn on failure, don't abort).
fn cleanup_worktree(mgr: &GitWorktreeManager, work_id: &str, command: &str) {
    if let Err(e) = mgr.cleanup(work_id) {
        tracing::warn!(work_id, error = %e, "worktree cleanup failed on {command}, continuing");
    }
}

/// `belt queue done` -- finish a Completed item, running its on_done scripts first.
///
/// A Completed item whose scripts fail goes to Failed. Any other phase is
/// decided by the transition contract without running scripts.
async fn cmd_queue_done(work_id: &str, json: bool) -> anyhow::Result<i32> {
    let db = Arc::new(open_db()?);
    let hitl = belt_daemon::hitl::HitlService::new(Arc::clone(&db));
    let Some(item) = find_item(&db, work_id)? else {
        return emit_refusal(work_id, json, Refusal::not_found());
    };
    let worktree_mgr = worktree_manager()?;

    let mut target = QueuePhase::Done;
    let mut detail = None;
    let mut scripts_run = false;
    let mut script_exit = None;

    if item.phase() == QueuePhase::Completed {
        let (_, config_path, _) = db.get_workspace(&item.workspace_id)?;
        let config = belt_infra::workspace_loader::load_workspace_config(std::path::Path::new(
            &config_path,
        ))?;
        let on_done_actions: Vec<belt_core::action::Action> = config
            .sources
            .values()
            .find_map(|source| source.states.get(&item.state))
            .map(|sc| {
                sc.on_done
                    .iter()
                    .map(belt_core::action::Action::from)
                    .collect()
            })
            .unwrap_or_default();

        if !on_done_actions.is_empty() {
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
            let executor =
                belt_daemon::executor::ActionExecutor::new(std::sync::Arc::new(registry));

            if !json {
                println!("Running on_done scripts for '{work_id}'...");
            }
            scripts_run = true;
            if let Some(r) = executor.execute_all(&on_done_actions, &env).await?
                && !r.success()
            {
                target = QueuePhase::Failed;
                detail = Some(format!("on_done failed with exit code {}", r.exit_code));
                script_exit = Some(r.exit_code);
            }
        }
    }

    let outcome = request_manual_transition(&hitl, &item, target, detail)?;
    if matches!(outcome, ManualOutcome::Applied) && target == QueuePhase::Done {
        cleanup_worktree(&worktree_mgr, work_id, "queue done");
    }
    let mut fields = serde_json::json!({ "scripts_run": scripts_run });
    if let Some(code) = script_exit {
        merge_json(&mut fields, serde_json::json!({ "exit_code": code }));
    }
    let text = match script_exit {
        Some(code) => {
            format!(
                "on_done scripts failed (exit code {code}). Item '{work_id}' transitioned to failed."
            )
        }
        None => format!("Marked '{work_id}' as done."),
    };
    emit_manual_outcome(work_id, json, target, outcome, fields, text)
}

/// `belt queue hitl` -- move a Completed item to Hitl and open a HITL request.
fn cmd_queue_hitl(work_id: &str, reason: Option<&str>, json: bool) -> anyhow::Result<i32> {
    use belt_core::transition::{Actor, TransitionReason};
    use belt_infra::db::{OpenHitlOutcome, OpenHitlRequest};

    let db = open_db()?;
    let Some(item) = find_item(&db, work_id)? else {
        return emit_refusal(work_id, json, Refusal::not_found());
    };
    let opened = db.open_hitl(&OpenHitlRequest {
        work_id: work_id.to_string(),
        expected_from: item.phase(),
        reason: belt_core::queue::HitlReason::ManualEscalation,
        notes: reason.map(str::to_string),
        actor: Actor::Cli,
        transition_reason: TransitionReason::Manual,
        timeout_at: None,
        terminal_action: None,
    })?;
    match opened {
        OpenHitlOutcome::Opened { hitl_id, .. } => {
            let text = match reason {
                Some(r) => format!("Marked {work_id} as HITL (reason: {r})."),
                None => format!("Marked {work_id} as HITL."),
            };
            emit_success(
                work_id,
                json,
                "applied",
                serde_json::json!({
                    "phase": "hitl",
                    "hitl_id": hitl_id.as_str(),
                    "notes": reason,
                }),
                text,
            )
        }
        OpenHitlOutcome::Rejected(refused) => {
            emit_refusal(work_id, json, Refusal::from_transition(refused))
        }
    }
}

/// `belt queue skip` -- skip an item through the transition contract.
///
/// A Running item is refused as `busy` until execution cancel exists.
fn cmd_queue_skip(work_id: &str, json: bool) -> anyhow::Result<i32> {
    let db = Arc::new(open_db()?);
    let hitl = belt_daemon::hitl::HitlService::new(Arc::clone(&db));
    let Some(item) = find_item(&db, work_id)? else {
        return emit_refusal(work_id, json, Refusal::not_found());
    };
    let outcome = request_manual_transition(&hitl, &item, QueuePhase::Skipped, None)?;
    if matches!(outcome, ManualOutcome::Applied) {
        cleanup_worktree(&worktree_manager()?, work_id, "queue skip");
    }
    emit_manual_outcome(
        work_id,
        json,
        QueuePhase::Skipped,
        outcome,
        serde_json::json!({}),
        format!("Skipped {work_id}."),
    )
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
/// suggested `HitlAction` string and `explanation` describes why.
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

/// The request an item is currently held by: its open request, else a
/// confirmed one the daemon has not post-processed yet.
fn current_request_of(
    db: &Database,
    work_id: &str,
) -> anyhow::Result<Option<belt_infra::db::HitlRequest>> {
    if let Some(open) = open_request_of(db, work_id)? {
        return Ok(Some(open));
    }
    Ok(db
        .pending_post_processing()?
        .into_iter()
        .find(|r| r.work_id == work_id))
}

/// The open request of an item (at most one per item).
fn open_request_of(
    db: &Database,
    work_id: &str,
) -> anyhow::Result<Option<belt_infra::db::HitlRequest>> {
    Ok(db
        .open_hitl_requests()?
        .into_iter()
        .find(|r| r.work_id == work_id))
}

/// Where a request is in its life: open, confirmed and waiting for the
/// daemon, or fully processed.
fn processing_label(request: &belt_infra::db::HitlRequest) -> &'static str {
    use belt_core::hitl::HitlStatus;
    match (request.status, &request.post_processed_at) {
        (HitlStatus::Open, _) => "open",
        (HitlStatus::Resolved | HitlStatus::Expired, None) => "awaiting_post_processing",
        (HitlStatus::Resolved | HitlStatus::Expired, Some(_)) => "post_processed",
    }
}

fn hitl_request_json(request: &belt_infra::db::HitlRequest) -> serde_json::Value {
    serde_json::json!({
        "hitl_id": request.hitl_id.as_str(),
        "work_id": request.work_id,
        "status": request.status,
        "processing": processing_label(request),
        "reason": request.reason.map(|r| r.to_string()),
        "notes": request.notes,
        "opened_at": request.opened_at,
        "timeout_at": request.timeout_at,
        "terminal_action": request.terminal_action.map(|a| a.to_string()),
        "resolution": request.resolution,
        "resolution_notes": request.resolution_notes,
        "post_processed_at": request.post_processed_at,
    })
}

/// `belt hitl respond` -- answer a HITL request through the shared contract.
fn cmd_hitl_respond(
    item_id: Option<String>,
    hitl_id: Option<String>,
    action: &str,
    respondent: Option<String>,
    notes: Option<String>,
    json: bool,
) -> anyhow::Result<i32> {
    use belt_infra::db::HitlTarget;

    let db = Arc::new(open_db()?);
    let hitl = belt_daemon::hitl::HitlService::new(Arc::clone(&db));

    let (target, work_id) = match (item_id, hitl_id) {
        (Some(work_id), None) => (HitlTarget::Item(work_id.clone()), work_id),
        (None, Some(id)) => {
            let id = belt_core::hitl::HitlId::new(id);
            let Some(request) = db.hitl_request(&id)? else {
                return emit_refusal(id.as_str(), json, Refusal::not_found());
            };
            (HitlTarget::Id(id), request.work_id)
        }
        (Some(_), Some(_)) | (None, None) => {
            anyhow::bail!("give either a work_id or --hitl-id")
        }
    };

    let action: belt_core::hitl::HitlAction = match action.parse() {
        Ok(action) => action,
        Err(e) => {
            return emit_refusal(
                &work_id,
                json,
                Refusal::new("invalid_action", serde_json::json!({}), e),
            );
        }
    };
    let by = respondent.unwrap_or_else(cli_respondent);
    let outcome = respond_as_cli(&hitl, target, action, by.clone(), notes)?;
    emit_manual_outcome(
        &work_id,
        json,
        QueuePhase::Hitl,
        ManualOutcome::HitlResponse {
            action,
            by,
            outcome,
        },
        serde_json::json!({}),
        String::new(),
    )
}

/// `belt hitl list` -- open HITL requests.
fn cmd_hitl_list(workspace: Option<&str>, format: &str) -> anyhow::Result<()> {
    let db = open_db()?;
    let mut rows = Vec::new();
    for request in db.open_hitl_requests()? {
        let item = db.get_item(&request.work_id)?;
        if workspace.is_some_and(|w| w != item.workspace_id) {
            continue;
        }
        rows.push((request, item));
    }

    if format == "json" {
        let values: Vec<serde_json::Value> = rows
            .iter()
            .map(|(request, item)| {
                let mut value = hitl_request_json(request);
                merge_json(
                    &mut value,
                    serde_json::json!({
                        "workspace_id": item.workspace_id,
                        "state": item.state,
                        "title": item.title,
                    }),
                );
                value
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&values)?);
    } else if rows.is_empty() {
        println!("No items awaiting human review.");
    } else {
        println!(
            "{:<40} {:<20} {:<12} {:<24} TITLE",
            "WORK_ID", "WORKSPACE", "STATE", "REASON"
        );
        println!("{}", "-".repeat(104));
        for (request, item) in &rows {
            let reason = request
                .reason
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
        println!("\n{} item(s) awaiting review.", rows.len());
    }
    Ok(())
}

/// `belt hitl show` -- show the HITL request an item is held by.
fn cmd_hitl_show(item_id: &str, format: &str, interactive: bool) -> anyhow::Result<i32> {
    let db = Arc::new(open_db()?);
    let hitl = belt_daemon::hitl::HitlService::new(Arc::clone(&db));
    let item = db.get_item(item_id)?;

    if item.phase() != QueuePhase::Hitl {
        anyhow::bail!(
            "item '{}' is in phase '{}', not 'hitl'",
            item_id,
            item.phase()
        );
    }
    let Some(request) = current_request_of(&db, item_id)? else {
        anyhow::bail!("item '{item_id}' has no HITL request");
    };

    let (rec_action, rec_explanation) = recommended_action(request.reason.as_ref());

    match format {
        "json" => {
            let mut value = serde_json::to_value(&item)?;
            merge_json(
                &mut value,
                serde_json::json!({
                    "hitl_request": hitl_request_json(&request),
                    "recommended": {
                        "action": rec_action,
                        "explanation": rec_explanation,
                    },
                }),
            );
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
            println!("HITL ID:      {}", request.hitl_id);
            println!("Request:      {}", processing_label(&request));
            println!("HITL Since:   {}", request.opened_at);
            if let Some(reason) = &request.reason {
                println!("HITL Reason:  {reason}");
            }
            if let Some(notes) = &request.notes {
                println!("Notes:        {notes}");
            }
            if let Some(timeout_at) = &request.timeout_at {
                println!("Timeout At:   {timeout_at}");
            }
            if let Some(action) = &request.terminal_action {
                println!("Timeout Act:  {action}");
            }
            if let Some(r) = &request.resolution {
                println!(
                    "Resolved:     {} by {} via {} at {} ({:?})",
                    r.action, r.by, r.via, r.at, r.path
                );
            }
            if let Some(notes) = &request.resolution_notes {
                println!("Resp. Notes:  {notes}");
            }
            println!();
            println!("Recommended:  {rec_action}");
            println!("              {rec_explanation}");
        }
    }

    if !interactive {
        return Ok(0);
    }

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
    let action: belt_core::hitl::HitlAction =
        chosen.parse().map_err(|e: String| anyhow::anyhow!(e))?;

    print!("Notes (optional): ");
    std::io::stdout().flush()?;
    let mut notes_input = String::new();
    std::io::stdin().read_line(&mut notes_input)?;
    let notes = notes_input.trim();
    let notes = (!notes.is_empty()).then(|| notes.to_string());

    let by = cli_respondent();
    let outcome = respond_as_cli(
        &hitl,
        belt_infra::db::HitlTarget::Id(request.hitl_id.clone()),
        action,
        by.clone(),
        notes,
    )?;
    emit_manual_outcome(
        item_id,
        false,
        QueuePhase::Hitl,
        ManualOutcome::HitlResponse {
            action,
            by,
            outcome,
        },
        serde_json::json!({}),
        String::new(),
    )
}

/// `belt hitl timeout set|ls` -- manage HITL timeouts.
fn cmd_hitl_timeout(command: HitlTimeoutCommands) -> anyhow::Result<()> {
    let db = open_db()?;
    match command {
        HitlTimeoutCommands::Set {
            item_id,
            duration,
            action,
            json: _,
        } => {
            let Some(request) = open_request_of(&db, &item_id)? else {
                anyhow::bail!("item '{item_id}' has no open HITL request");
            };
            // The deadline and terminal action belong on `request`, which the
            // store cannot update yet; the legacy `queue_items` columns are not
            // a substitute because nothing reads them any more.
            anyhow::bail!(
                "setting a timeout on HITL request {} (duration {duration}s, action {}) is not supported yet",
                request.hitl_id,
                action.as_deref().unwrap_or("workspace default"),
            )
        }
        HitlTimeoutCommands::Ls { json } => {
            let mut rows = Vec::new();
            for request in db.open_hitl_requests()? {
                if request.timeout_at.is_none() {
                    continue;
                }
                let item = db.get_item(&request.work_id)?;
                rows.push((request, item));
            }
            if json {
                let entries: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|(request, item)| {
                        serde_json::json!({
                            "work_id": request.work_id,
                            "hitl_id": request.hitl_id.as_str(),
                            "timeout_at": request.timeout_at,
                            "action": request.terminal_action.map(|a| a.to_string()),
                            "workspace": item.workspace_id,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if rows.is_empty() {
                println!("No HITL items with active timeouts.");
            } else {
                println!(
                    "{:<40} {:<28} {:<10} {:<20}",
                    "WORK_ID", "TIMEOUT_AT", "ACTION", "WORKSPACE"
                );
                for (request, item) in &rows {
                    let action = request
                        .terminal_action
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| "-".to_string());
                    println!(
                        "{:<40} {:<28} {:<10} {:<20}",
                        truncate(&request.work_id, 40),
                        request.timeout_at.as_deref().unwrap_or("-"),
                        action,
                        &item.workspace_id,
                    );
                }
                println!("\n{} item(s) with timeout", rows.len());
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
        .with_writer(std::io::stderr)
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
            QueueCommands::Show {
                work_id,
                format,
                json,
            } => {
                cmd_queue_show(&work_id, &format, json)?;
            }
            QueueCommands::Done { work_id, json } => {
                exit_if_refused(cmd_queue_done(&work_id, json).await?);
            }
            QueueCommands::Hitl {
                work_id,
                reason,
                json,
            } => {
                exit_if_refused(cmd_queue_hitl(&work_id, reason.as_deref(), json)?);
            }
            QueueCommands::Skip { work_id, json } => {
                exit_if_refused(cmd_queue_skip(&work_id, json)?);
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
                            derived_from: item.derived_from.clone(),
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
                hitl_id,
                action,
                respondent,
                notes,
                json: json_output,
            } => {
                exit_if_refused(cmd_hitl_respond(
                    item_id,
                    hitl_id,
                    &action,
                    respondent,
                    notes,
                    json_output,
                )?);
            }
            HitlCommands::List { workspace, format } => {
                tracing::info!(?workspace, "listing HITL items...");
                cmd_hitl_list(workspace.as_deref(), &format)?;
            }
            HitlCommands::Show {
                item_id,
                format,
                interactive,
            } => {
                exit_if_refused(cmd_hitl_show(&item_id, &format, interactive)?);
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
    fn already_handled_refusal_reports_who_how_what_and_when() {
        use belt_core::hitl::{ConfirmPath, HitlAction, HitlResolution, RespondOutcome};

        let resolution = HitlResolution {
            action: HitlAction::Skip,
            by: "alice".to_string(),
            via: "tui".to_string(),
            at: "2026-01-02T03:04:05+00:00".to_string(),
            path: ConfirmPath::Direct,
        };

        let value = refusal_value("w1", &already_handled_refusal(&resolution));
        assert_eq!(value["success"], false);
        assert_eq!(value["reason"], "already_handled");
        assert_eq!(value["work_id"], "w1");
        assert_eq!(value["by"], "alice");
        assert_eq!(value["via"], "tui");
        assert_eq!(value["action"], "skip");
        assert_eq!(value["at"], "2026-01-02T03:04:05+00:00");

        let code = emit_manual_outcome(
            "w1",
            true,
            QueuePhase::Done,
            ManualOutcome::HitlResponse {
                action: HitlAction::Done,
                by: "carol".to_string(),
                outcome: RespondOutcome::AlreadyHandled(resolution),
            },
            serde_json::json!({}),
            String::new(),
        )
        .unwrap();
        assert_eq!(code, EXIT_REFUSED);
    }

    #[test]
    fn conflict_refusal_exits_with_refused_code_and_current_phase() {
        let refusal =
            Refusal::from_transition(belt_core::transition::TransitionOutcome::Conflict {
                current: QueuePhase::Done,
            });
        let value = refusal_value("w1", &refusal);
        assert_eq!(value["reason"], "conflict");
        assert_eq!(value["current"], "done");
        assert_eq!(emit_refusal("w1", true, refusal).unwrap(), EXIT_REFUSED);
    }

    #[test]
    fn agent_without_session_subcommand_fails_to_parse() {
        // `belt agent` requires an `AgentCommands` subcommand (Session, Init,
        // Rules, ...). Flags belonging to `Session` (--workspace, -p, --json)
        // are not valid directly under `Agent`.
        let result =
            Cli::try_parse_from(["belt", "agent", "--workspace", "x", "-p", "y", "--json"]);
        assert!(
            result.is_err(),
            "expected clap parse error for `belt agent --workspace x -p y --json` \
             (missing `session` subcommand), got Ok(..)"
        );
    }

    #[test]
    fn agent_session_subcommand_parses_successfully() {
        let result = Cli::try_parse_from([
            "belt",
            "agent",
            "session",
            "--workspace",
            "x",
            "-p",
            "y",
            "--json",
        ]);
        assert!(
            result.is_ok(),
            "expected `belt agent session --workspace x -p y --json` to parse, got error: {:?}",
            result.err()
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
    fn cron_trigger_parses_name() {
        let cli = Cli::try_parse_from(["belt", "cron", "trigger", "daily-report"]).unwrap();
        match cli.command {
            Commands::Cron {
                command: CronCommands::Trigger { name, .. },
            } => assert_eq!(name, "daily-report"),
            _ => panic!("expected Cron Trigger command"),
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
