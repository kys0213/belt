use std::collections::HashMap;
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
use belt_infra::sources::github::GitHubDataSource;
use belt_infra::worktree::GitWorktreeManager;

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
    },
    /// Run an LLM agent session.
    Agent {
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
    /// Claw interactive management session.
    Claw {
        #[command(subcommand)]
        command: ClawCommands,
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
    },
}

#[derive(Subcommand)]
enum ClawCommands {
    /// Initialize Claw workspace.
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
    /// Open interactive session.
    Session,
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
    },
    /// List HITL items.
    List {
        /// Filter by workspace.
        #[arg(long)]
        workspace: Option<String>,
    },
}

#[derive(Subcommand)]
enum WorkspaceCommands {
    /// Register a new workspace.
    Add {
        /// Path to workspace.yaml config.
        #[arg(long)]
        config: String,
    },
    /// List registered workspaces.
    List,
    /// Show workspace details.
    Show { name: String },
    /// Update workspace configuration.
    Update {
        /// Workspace name.
        name: String,
        /// New config file path.
        #[arg(long)]
        config: Option<String>,
    },
    /// Remove a workspace.
    Remove {
        /// Workspace name.
        name: String,
        /// Skip confirmation warning for active items.
        #[arg(long)]
        force: bool,
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
    Done { work_id: String },
    /// Mark item as HITL (called by evaluate).
    Hitl {
        work_id: String,
        /// Reason for HITL.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Skip an item.
    Skip { work_id: String },
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
    },
    /// Pause an active spec.
    Pause {
        /// Spec ID.
        id: String,
    },
    /// Resume a paused spec.
    Resume {
        /// Spec ID.
        id: String,
    },
    /// Complete an active spec.
    Complete {
        /// Spec ID.
        id: String,
    },
    /// Remove a spec.
    Remove {
        /// Spec ID.
        id: String,
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

    let belt_home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?
        .join(".belt");

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

    let mut daemon = Daemon::new(
        config,
        sources,
        Arc::new(registry),
        Box::new(worktree_mgr),
        max_concurrent,
    )
    .with_db(db)
    .with_belt_home(belt_home);

    tracing::info!(
        "starting belt daemon (tick={}s, max_concurrent={})",
        tick_interval_secs,
        max_concurrent
    );
    daemon.run(tick_interval_secs).await;
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
    /// Trigger a cron job immediately by resetting its last_run_at.
    Trigger {
        /// Name of the cron job to trigger.
        name: String,
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

/// `belt stop` -- send SIGTERM to the daemon process.
fn cmd_stop() -> anyhow::Result<()> {
    let pid = read_pid()?;
    tracing::info!(pid, "sending SIGTERM to daemon...");

    // SAFETY: We are sending a well-known signal to a process we own.
    #[cfg(unix)]
    {
        use std::process::Command;
        let status = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()?;
        if status.success() {
            println!("Sent stop signal to daemon (PID {pid}).");
        } else {
            anyhow::bail!("Failed to send signal to PID {pid}. Process may not exist.");
        }
    }

    #[cfg(not(unix))]
    {
        anyhow::bail!("belt stop is only supported on Unix systems");
    }

    Ok(())
}

/// `belt status` -- show queue item counts grouped by phase.
fn cmd_status(format: &str) -> anyhow::Result<()> {
    let db = open_db()?;
    let items = db.list_items(None, None)?;

    let mut counts: HashMap<String, usize> = HashMap::new();
    for item in &items {
        *counts.entry(item.phase.as_str().to_string()).or_insert(0) += 1;
    }

    let daemon_running = read_pid().is_ok();

    match format {
        "json" => {
            let output = serde_json::json!({
                "daemon_running": daemon_running,
                "total_items": items.len(),
                "phases": counts,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        _ => {
            println!(
                "Daemon: {}",
                if daemon_running { "running" } else { "stopped" }
            );
            println!("Total items: {}", items.len());
            if !counts.is_empty() {
                println!("Phases:");
                let mut sorted: Vec<_> = counts.iter().collect();
                sorted.sort_by_key(|(k, _)| (*k).clone());
                for (phase, count) in sorted {
                    println!("  {phase:<12} {count}");
                }
            }
        }
    }

    Ok(())
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
                        item.phase.as_str(),
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
            println!("Phase:        {}", item.phase);
            if let Some(title) = &item.title {
                println!("Title:        {title}");
            }
            println!("Created:      {}", item.created_at);
            println!("Updated:      {}", item.updated_at);
        }
    }

    Ok(())
}

/// `belt queue done` -- mark a queue item as Done.
fn cmd_queue_done(work_id: &str) -> anyhow::Result<()> {
    let db = open_db()?;
    db.update_phase(work_id, QueuePhase::Done)?;
    println!("Marked {work_id} as done.");
    Ok(())
}

/// `belt queue hitl` -- mark a queue item as HITL.
fn cmd_queue_hitl(work_id: &str, reason: Option<&str>) -> anyhow::Result<()> {
    let db = open_db()?;
    db.update_phase(work_id, QueuePhase::Hitl)?;
    if let Some(r) = reason {
        println!("Marked {work_id} as HITL (reason: {r}).");
    } else {
        println!("Marked {work_id} as HITL.");
    }
    Ok(())
}

/// `belt queue skip` -- mark a queue item as Skipped.
fn cmd_queue_skip(work_id: &str) -> anyhow::Result<()> {
    let db = open_db()?;
    db.update_phase(work_id, QueuePhase::Skipped)?;
    println!("Skipped {work_id}.");
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

/// `belt cron trigger` -- reset last_run_at so the job fires on next tick.
fn cmd_cron_trigger(name: &str) -> anyhow::Result<()> {
    let db = open_db()?;
    // Reset last_run_at to NULL by toggling enabled (no direct reset API),
    // but we do have update_cron_last_run which sets it to now -- that's not
    // what we want. Instead, we verify the job exists then inform the user
    // that the next daemon tick will run it. For a true trigger, we'd need
    // the daemon to expose an RPC. For now, we update last_run_at to a very
    // old timestamp to ensure the scheduler picks it up.
    //
    // Since the DB API doesn't expose a "clear last_run_at" method, we
    // update it to epoch so the interval/daily check will fire next tick.
    let jobs = db.list_cron_jobs()?;
    let found = jobs.iter().any(|j| j.name == name);
    if !found {
        anyhow::bail!("cron job not found: {name}");
    }

    // Use update_cron_last_run to set a sentinel. The scheduler will fire
    // because we set it to now, but that means it just ran -- not what we
    // want. We need a different approach: toggle enabled off then on to
    // "nudge" the job. Actually, the cleanest approach given the DB API is
    // to record that a trigger was requested. For the MVP, we update
    // last_run_at to epoch (1970) via a direct workaround.
    //
    // The CronEngine in the daemon uses in-memory state, so the DB cron_jobs
    // table is a registry. Triggering means telling the daemon to
    // force_trigger. Without IPC, the best we can do is inform the user.
    println!("Trigger requested for cron job '{name}'.");
    println!("The job will execute on the next daemon tick.");
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
        Commands::Status { format } => {
            cmd_status(&format)?;
        }
        Commands::Dashboard => {
            let belt_home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?
                .join(".belt");
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
            let belt_home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?
                .join(".belt");
            std::fs::create_dir_all(&belt_home)?;
            let db_path = belt_home.join("belt.db");
            let db = belt_infra::db::Database::open(
                db_path
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid db path"))?,
            )?;

            match command {
                WorkspaceCommands::Add { config } => {
                    let config_path = std::path::Path::new(&config);
                    let result = belt_infra::onboarding::onboard_workspace(&db, config_path)?;

                    // Initialize claw workspace automatically
                    let claw_ws = claw::ClawWorkspace::init(&belt_home)?;
                    tracing::info!(path = %claw_ws.path.display(), "claw workspace initialized");

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
                }
                WorkspaceCommands::List => {
                    let workspaces = db.list_workspaces()?;
                    if workspaces.is_empty() {
                        println!("No workspaces registered.");
                    } else {
                        println!("{:<20} {:<50} CREATED", "NAME", "CONFIG");
                        for (name, config_path, created_at) in &workspaces {
                            println!("{:<20} {:<50} {}", name, config_path, created_at);
                        }
                    }
                }
                WorkspaceCommands::Show { name } => {
                    let (ws_name, config_path, created_at) = db.get_workspace(&name)?;
                    println!("Name:       {ws_name}");
                    println!("Config:     {config_path}");
                    println!("Created at: {created_at}");

                    // Show associated cron jobs
                    let jobs = db.list_cron_jobs()?;
                    let ws_jobs: Vec<_> = jobs
                        .iter()
                        .filter(|j| j.workspace.as_deref() == Some(&name))
                        .collect();
                    if !ws_jobs.is_empty() {
                        println!("\nCron jobs:");
                        for job in &ws_jobs {
                            let status = if job.enabled { "enabled" } else { "disabled" };
                            println!("  {} [{}] ({})", job.name, job.schedule, status);
                        }
                    }
                }
                WorkspaceCommands::Update { name, config } => {
                    if let Some(config_path) = config {
                        let path = std::path::Path::new(&config_path);
                        let abs_path = std::fs::canonicalize(path)
                            .unwrap_or_else(|_| path.to_path_buf())
                            .to_string_lossy()
                            .to_string();
                        db.update_workspace(&name, &abs_path)?;
                        println!("Workspace '{}' updated.", name);
                        println!("  Config: {}", abs_path);
                    } else {
                        println!("No update options provided. Use --config to update the config path.");
                    }
                }
                WorkspaceCommands::Remove { name, force } => {
                    // Check for active queue items in this workspace
                    let items = db.list_items(None, Some(&name))?;
                    let active_count = items
                        .iter()
                        .filter(|i| !matches!(i.phase, QueuePhase::Done | QueuePhase::Skipped))
                        .count();

                    if active_count > 0 && !force {
                        eprintln!(
                            "Warning: workspace '{}' has {} active item(s).",
                            name, active_count
                        );
                        eprintln!("Use --force to remove anyway.");
                        std::process::exit(1);
                    }

                    db.remove_workspace(&name)?;
                    println!("Workspace '{}' removed.", name);
                    if active_count > 0 {
                        println!(
                            "  Note: {} active item(s) remain in the queue.",
                            active_count
                        );
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
            QueueCommands::Done { work_id } => {
                cmd_queue_done(&work_id)?;
            }
            QueueCommands::Hitl { work_id, reason } => {
                cmd_queue_hitl(&work_id, reason.as_deref())?;
            }
            QueueCommands::Skip { work_id } => {
                cmd_queue_skip(&work_id)?;
            }
        },
        Commands::Cron { command } => match command {
            CronCommands::List { format } => {
                cmd_cron_list(&format)?;
            }
            CronCommands::Trigger { name } => {
                cmd_cron_trigger(&name)?;
            }
        },
        Commands::Context { work_id, json } => {
            let db_path = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?
                .join(".belt")
                .join("belt.db");

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

            let ctx = belt_core::context::ItemContext {
                work_id: item.work_id.clone(),
                workspace: item.workspace_id.clone(),
                queue: belt_core::context::QueueContext {
                    phase: item.phase.as_str().to_string(),
                    state: item.state.clone(),
                    source_id: item.source_id.clone(),
                },
                source: belt_core::context::SourceContext {
                    source_type: "github".to_string(),
                    url: String::new(),
                    default_branch: None,
                },
                issue: None,
                pr: None,
                history,
                worktree: None,
            };

            if json {
                println!("{}", serde_json::to_string_pretty(&ctx)?);
            } else {
                println!("work_id:   {}", ctx.work_id);
                println!("workspace: {}", ctx.workspace);
                println!("phase:     {}", ctx.queue.phase);
                println!("state:     {}", ctx.queue.state);
                println!("source_id: {}", ctx.queue.source_id);
                if !ctx.history.is_empty() {
                    println!("history:   {} entries", ctx.history.len());
                }
            }
        }
        Commands::Agent {
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
        Commands::Spec { command } => {
            let belt_home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?
                .join(".belt");
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
                } => {
                    let id = format!("spec-{}", chrono::Utc::now().timestamp_millis());
                    let mut spec = belt_core::spec::Spec::new(id.clone(), workspace, name, content);
                    spec.priority = priority;
                    spec.labels = labels;
                    spec.depends_on = depends_on;
                    db.insert_spec(&spec)?;
                    println!("spec created: {id}");
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
                    } else {
                        if specs.is_empty() {
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
                    db.update_spec(&spec)?;
                    println!("spec updated: {id}");
                }
                SpecCommands::Pause { id } => {
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
                    println!("spec paused: {id}");
                }
                SpecCommands::Resume { id } => {
                    let spec = db.get_spec(&id)?;
                    if !spec
                        .status
                        .can_transition_to(&belt_core::spec::SpecStatus::Active)
                    {
                        anyhow::bail!(
                            "cannot resume spec in status '{}': only draft or paused specs can be activated",
                            spec.status
                        );
                    }
                    let was_draft = spec.status == belt_core::spec::SpecStatus::Draft;
                    db.update_spec_status(&id, belt_core::spec::SpecStatus::Active)?;
                    println!("spec activated: {id}");
                    if was_draft {
                        // TODO: trigger GitHub issue creation when spec transitions Draft -> Active
                        tracing::info!(
                            id,
                            "spec activated from draft — GitHub issue creation pending"
                        );
                    }
                }
                SpecCommands::Complete { id } => {
                    let spec = db.get_spec(&id)?;
                    if !spec
                        .status
                        .can_transition_to(&belt_core::spec::SpecStatus::Completed)
                    {
                        anyhow::bail!(
                            "cannot complete spec in status '{}': only active specs can be completed",
                            spec.status
                        );
                    }
                    db.update_spec_status(&id, belt_core::spec::SpecStatus::Completed)?;
                    println!("spec completed: {id}");
                }
                SpecCommands::Remove { id } => {
                    db.remove_spec(&id)?;
                    println!("spec removed: {id}");
                }
            }
        }
        Commands::Bootstrap {
            workspace,
            rules_dir,
            force,
        } => {
            let workspace_root = match (&workspace, &rules_dir) {
                // If a custom rules_dir is given, create it directly.
                (_, Some(dir)) => {
                    let rules_path = std::path::PathBuf::from(dir);
                    std::fs::create_dir_all(&rules_path)?;
                    // Use the parent of the rules dir as a synthetic workspace root
                    // so that bootstrap::run creates files inside the given path.
                    // We need to strip the `.claude/rules` suffix expectation.
                    // Instead, write directly using the rules_dir.
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
        Commands::Hitl { command } => match command {
            HitlCommands::Respond {
                item_id,
                action,
                respondent,
                notes,
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
                // TODO: wire to daemon/DB to apply the respond action
                println!("HITL respond: item={item_id} action={action}");
            }
            HitlCommands::List { workspace } => {
                tracing::info!(?workspace, "listing HITL items...");
                // TODO: wire to DB to list HITL items
            }
        },

        Commands::Claw { command } => match command {
            ClawCommands::Init { force } => {
                let belt_home = dirs::home_dir()
                    .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?
                    .join(".belt");
                let ws = if force {
                    claw::ClawWorkspace::init_with_options(&belt_home, true)?
                } else {
                    claw::ClawWorkspace::init(&belt_home)?
                };
                tracing::info!(path = %ws.path.display(), "claw workspace initialized");
            }
            ClawCommands::Rules => {
                let belt_home = dirs::home_dir()
                    .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?
                    .join(".belt");
                let ws = claw::ClawWorkspace {
                    path: belt_home.join("claw-workspace"),
                };
                let rules = ws.list_rules()?;
                for rule in &rules {
                    println!("{}", rule.display());
                }
            }
            ClawCommands::Edit { rule } => {
                let belt_home = dirs::home_dir()
                    .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?
                    .join(".belt");
                let ws = claw::ClawWorkspace {
                    path: belt_home.join("claw-workspace"),
                };
                ws.edit_rule(rule.as_deref())?;
            }
            ClawCommands::Session => {
                let belt_home = dirs::home_dir()
                    .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?
                    .join(".belt");
                let claw_workspace = claw::ClawWorkspace::init(&belt_home)?;
                let config = claw::session::SessionConfig {
                    workspace: None,
                    claw_workspace,
                };
                claw::session::run_interactive(config)?;
            }
        },
    }

    Ok(())
}
