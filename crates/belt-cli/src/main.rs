use clap::{Parser, Subcommand};

mod agent;
mod claw;
mod dashboard;
mod status;

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
    /// Claw interactive management session.
    Claw {
        #[command(subcommand)]
        command: ClawCommands,
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
    },
    /// Show queue item details.
    Show { work_id: String },
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
            tracing::info!("starting belt daemon...");
            // TODO: daemon start
        }
        Commands::Stop => {
            tracing::info!("stopping belt daemon...");
            // TODO: daemon stop
        }
        Commands::Status { format } => {
            status::show_status(&format)?;
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
            }
        }
        Commands::Queue { command } => match command {
            QueueCommands::List { phase, workspace } => {
                tracing::info!(?phase, ?workspace, "listing queue items...");
            }
            QueueCommands::Show { work_id } => {
                tracing::info!(work_id, "showing queue item...");
            }
            QueueCommands::Done { work_id } => {
                tracing::info!(work_id, "marking as done...");
            }
            QueueCommands::Hitl { work_id, reason } => {
                tracing::info!(work_id, ?reason, "marking as HITL...");
            }
            QueueCommands::Skip { work_id } => {
                tracing::info!(work_id, "skipping item...");
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
                tracing::info!("interactive session not yet implemented");
            }
        },
    }

    Ok(())
}
