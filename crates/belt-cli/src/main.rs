use std::path::Path;

use chrono::Utc;
use clap::{Parser, Subcommand};
use serde_json::json;

use belt_core::phase::QueuePhase;
use belt_core::queue::HistoryEvent;
use belt_infra::db::Database;

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
    Start,
    /// Stop the daemon.
    Stop,
    /// Show system status.
    Status {
        /// Output format.
        #[arg(long, default_value = "text")]
        format: String,
        /// Output as JSON (shorthand for --format json).
        #[arg(long)]
        json: bool,
    },
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
    /// Retrieve item context for scripts.
    Context {
        /// Queue item work_id.
        work_id: String,
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
    },
    /// List registered workspaces.
    List {
        /// Output format.
        #[arg(long, default_value = "text")]
        format: String,
        /// Output as JSON (shorthand for --format json).
        #[arg(long)]
        json: bool,
    },
    /// Show workspace details.
    Show {
        name: String,
        /// Output format.
        #[arg(long, default_value = "text")]
        format: String,
        /// Output as JSON (shorthand for --format json).
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
        /// Output as JSON (shorthand for --format json).
        #[arg(long)]
        json: bool,
    },
    /// Show queue item details.
    Show {
        work_id: String,
        /// Output format.
        #[arg(long, default_value = "text")]
        format: String,
        /// Output as JSON (shorthand for --format json).
        #[arg(long)]
        json: bool,
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

/// Determine the effective output format from --format and --json flags.
fn resolve_format(format: &str, json_flag: bool) -> &str {
    if json_flag { "json" } else { format }
}

/// Open the Belt database, creating the BELT_HOME directory if needed.
fn get_db() -> anyhow::Result<Database> {
    let home = std::env::var("BELT_HOME").unwrap_or_else(|_| {
        let home_dir = std::env::var("HOME").expect("cannot determine home directory");
        format!("{home_dir}/.belt")
    });
    std::fs::create_dir_all(&home)?;
    let db_path = Path::new(&home).join("belt.db");
    let db = Database::open(db_path.to_str().expect("invalid db path"))?;
    Ok(db)
}

/// Parse a phase string into a `QueuePhase`.
fn parse_phase(s: &str) -> anyhow::Result<QueuePhase> {
    match s.to_lowercase().as_str() {
        "pending" => Ok(QueuePhase::Pending),
        "ready" => Ok(QueuePhase::Ready),
        "running" => Ok(QueuePhase::Running),
        "completed" => Ok(QueuePhase::Completed),
        "done" => Ok(QueuePhase::Done),
        "hitl" => Ok(QueuePhase::Hitl),
        "failed" => Ok(QueuePhase::Failed),
        "skipped" => Ok(QueuePhase::Skipped),
        _ => anyhow::bail!("unknown phase: {s}"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("belt=info".parse()?),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Start => {
            println!("daemon not yet implemented");
        }
        Commands::Stop => {
            println!("daemon not yet implemented");
        }
        Commands::Status { format, json } => {
            cmd_status(resolve_format(&format, json))?;
        }
        Commands::Workspace { command } => match command {
            WorkspaceCommands::Add { config } => {
                cmd_workspace_add(&config)?;
            }
            WorkspaceCommands::List { format, json } => {
                cmd_workspace_list(resolve_format(&format, json))?;
            }
            WorkspaceCommands::Show { name, format, json } => {
                cmd_workspace_show(&name, resolve_format(&format, json))?;
            }
        },
        Commands::Queue { command } => match command {
            QueueCommands::List {
                phase,
                workspace,
                format,
                json,
            } => {
                cmd_queue_list(
                    phase.as_deref(),
                    workspace.as_deref(),
                    resolve_format(&format, json),
                )?;
            }
            QueueCommands::Show {
                work_id,
                format,
                json,
            } => {
                cmd_queue_show(&work_id, resolve_format(&format, json))?;
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
        Commands::Context { work_id, json: _ } => {
            cmd_context(&work_id)?;
        }
    }

    Ok(())
}

// ---- Command handlers ------------------------------------------------------

/// Display per-phase queue item counts.
fn cmd_status(format: &str) -> anyhow::Result<()> {
    let db = get_db()?;

    let all_phases = [
        QueuePhase::Pending,
        QueuePhase::Ready,
        QueuePhase::Running,
        QueuePhase::Completed,
        QueuePhase::Done,
        QueuePhase::Hitl,
        QueuePhase::Failed,
        QueuePhase::Skipped,
    ];

    let mut counts: Vec<(QueuePhase, usize)> = Vec::new();
    let mut total: usize = 0;

    for phase in all_phases {
        let items = db.list_items(Some(phase), None)?;
        let count = items.len();
        total += count;
        counts.push((phase, count));
    }

    if format == "json" {
        let mut map = serde_json::Map::new();
        for (phase, count) in &counts {
            map.insert(phase.to_string().to_lowercase(), json!(count));
        }
        map.insert("total".to_string(), json!(total));
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Object(map))?
        );
    } else {
        println!("Belt Status");
        // Unicode box-drawing thin horizontal line
        println!(
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}"
        );
        for (phase, count) in &counts {
            println!("{:<10} {}", format!("{phase}:"), count);
        }
        println!("{:<10} {}", "Total:", total);
    }

    Ok(())
}

/// Register a workspace from a config file path.
fn cmd_workspace_add(config: &str) -> anyhow::Result<()> {
    let db = get_db()?;
    let path = Path::new(config);

    anyhow::ensure!(path.exists(), "config file not found: {config}");

    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("cannot extract workspace name from path: {config}"))?;

    let abs_path = std::fs::canonicalize(path)?;
    let abs_str = abs_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8"))?;
    db.add_workspace(name, abs_str)?;
    println!("workspace '{name}' registered (config: {abs_str})");
    Ok(())
}

/// List all registered workspaces.
fn cmd_workspace_list(format: &str) -> anyhow::Result<()> {
    let db = get_db()?;
    let workspaces = db.list_workspaces()?;

    if format == "json" {
        let list: Vec<serde_json::Value> = workspaces
            .iter()
            .map(
                |(name, config_path, created_at): &(
                    String,
                    String,
                    chrono::DateTime<chrono::Utc>,
                )| {
                    json!({
                        "name": name,
                        "config_path": config_path,
                        "created_at": created_at.to_rfc3339(),
                    })
                },
            )
            .collect();
        println!("{}", serde_json::to_string_pretty(&list)?);
    } else if workspaces.is_empty() {
        println!("No workspaces registered.");
    } else {
        let header = format!("{:<20} {:<50} CREATED", "NAME", "CONFIG");
        println!("{header}");
        for (name, config_path, created_at) in &workspaces {
            println!(
                "{:<20} {:<50} {}",
                name,
                config_path,
                created_at.format("%Y-%m-%d %H:%M:%S")
            );
        }
    }

    Ok(())
}

/// Show details for a single workspace.
fn cmd_workspace_show(name: &str, format: &str) -> anyhow::Result<()> {
    let db = get_db()?;
    let (ws_name, config_path, created_at) = db.get_workspace(name)?;

    if format == "json" {
        let value = json!({
            "name": ws_name,
            "config_path": config_path,
            "created_at": created_at.to_rfc3339(),
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("Name:       {ws_name}");
        println!("Config:     {config_path}");
        println!("Created at: {}", created_at.format("%Y-%m-%d %H:%M:%S"));
    }

    Ok(())
}

/// List queue items with optional phase/workspace filters.
fn cmd_queue_list(
    phase: Option<&str>,
    workspace: Option<&str>,
    format: &str,
) -> anyhow::Result<()> {
    let db = get_db()?;
    let phase_filter = phase.map(parse_phase).transpose()?;
    let items = db.list_items(phase_filter, workspace)?;

    if format == "json" {
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else if items.is_empty() {
        println!("No queue items found.");
    } else {
        println!(
            "{:<40} {:<20} {:<12} {:<12}",
            "WORK_ID", "WORKSPACE", "STATE", "PHASE"
        );
        for item in &items {
            println!(
                "{:<40} {:<20} {:<12} {:<12}",
                item.work_id, item.workspace, item.state, item.phase,
            );
        }
    }

    Ok(())
}

/// Show details for a single queue item, including its history events.
fn cmd_queue_show(work_id: &str, format: &str) -> anyhow::Result<()> {
    let db = get_db()?;
    let item = db.get_item(work_id)?;
    let history = db.get_history(&item.source_id)?;

    if format == "json" {
        let value = json!({
            "item": item,
            "history": history,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("Work ID:    {}", item.work_id);
        println!("Source ID:  {}", item.source_id);
        println!("Workspace:  {}", item.workspace);
        println!("State:      {}", item.state);
        println!("Phase:      {}", item.phase);
        println!(
            "Worktree:   {}",
            item.worktree.as_deref().unwrap_or("(none)")
        );
        println!(
            "Created at: {}",
            item.created_at.format("%Y-%m-%d %H:%M:%S")
        );
        println!(
            "Updated at: {}",
            item.updated_at.format("%Y-%m-%d %H:%M:%S")
        );

        if !history.is_empty() {
            println!();
            println!("History:");
            for event in &history {
                println!(
                    "  [{}] attempt={} status={} state={}{}{}",
                    event.created_at.format("%Y-%m-%d %H:%M:%S"),
                    event.attempt,
                    event.status,
                    event.state,
                    event
                        .summary
                        .as_ref()
                        .map(|s| format!(" summary={s}"))
                        .unwrap_or_default(),
                    event
                        .error
                        .as_ref()
                        .map(|e| format!(" error={e}"))
                        .unwrap_or_default(),
                );
            }
        }
    }

    Ok(())
}

/// Transition a queue item to `Done` and record history.
fn cmd_queue_done(work_id: &str) -> anyhow::Result<()> {
    let db = get_db()?;
    let item = db.get_item(work_id)?;

    anyhow::ensure!(
        item.phase.can_transition_to(&QueuePhase::Done),
        "cannot transition from {} to Done",
        item.phase
    );

    db.update_phase(work_id, QueuePhase::Done)?;

    let event = HistoryEvent {
        work_id: item.work_id.clone(),
        source_id: item.source_id.clone(),
        state: item.state.clone(),
        status: "done".to_string(),
        attempt: 1,
        summary: Some("marked as done via CLI".to_string()),
        error: None,
        created_at: Utc::now(),
    };
    db.append_history(&event)?;

    println!("item '{work_id}' marked as Done");
    Ok(())
}

/// Transition a queue item to `Hitl` and record history.
fn cmd_queue_hitl(work_id: &str, reason: Option<&str>) -> anyhow::Result<()> {
    let db = get_db()?;
    let item = db.get_item(work_id)?;

    anyhow::ensure!(
        item.phase.can_transition_to(&QueuePhase::Hitl),
        "cannot transition from {} to HITL",
        item.phase
    );

    db.update_phase(work_id, QueuePhase::Hitl)?;

    let summary = reason
        .map(|r| format!("HITL via CLI: {r}"))
        .unwrap_or_else(|| "HITL via CLI".to_string());

    let event = HistoryEvent {
        work_id: item.work_id.clone(),
        source_id: item.source_id.clone(),
        state: item.state.clone(),
        status: "hitl".to_string(),
        attempt: 1,
        summary: Some(summary),
        error: None,
        created_at: Utc::now(),
    };
    db.append_history(&event)?;

    println!("item '{work_id}' marked as HITL");
    Ok(())
}

/// Transition a queue item to `Skipped` and record history.
fn cmd_queue_skip(work_id: &str) -> anyhow::Result<()> {
    let db = get_db()?;
    let item = db.get_item(work_id)?;

    anyhow::ensure!(
        item.phase.can_transition_to(&QueuePhase::Skipped),
        "cannot transition from {} to Skipped",
        item.phase
    );

    db.update_phase(work_id, QueuePhase::Skipped)?;

    let event = HistoryEvent {
        work_id: item.work_id.clone(),
        source_id: item.source_id.clone(),
        state: item.state.clone(),
        status: "skipped".to_string(),
        attempt: 1,
        summary: Some("skipped via CLI".to_string()),
        error: None,
        created_at: Utc::now(),
    };
    db.append_history(&event)?;

    println!("item '{work_id}' marked as Skipped");
    Ok(())
}

/// Output basic item context as JSON (stub for future DataSource integration).
fn cmd_context(work_id: &str) -> anyhow::Result<()> {
    let db = get_db()?;
    let item = db.get_item(work_id)?;

    let value = json!({
        "work_id": item.work_id,
        "source_id": item.source_id,
        "workspace": item.workspace,
        "state": item.state,
        "phase": item.phase,
        "worktree": item.worktree,
        "created_at": item.created_at.to_rfc3339(),
        "updated_at": item.updated_at.to_rfc3339(),
    });
    println!("{}", serde_json::to_string_pretty(&value)?);

    Ok(())
}
