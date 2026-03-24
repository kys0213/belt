use clap::{Parser, Subcommand};

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
        /// Target workspace name.
        #[arg(long)]
        workspace: Option<String>,
        /// Non-interactive prompt (for cron/evaluate calls).
        #[arg(short, long)]
        prompt: Option<String>,
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
        Commands::Workspace { command } => match command {
            WorkspaceCommands::Add { config } => {
                tracing::info!(config, "registering workspace...");
            }
            WorkspaceCommands::List => {
                tracing::info!("listing workspaces...");
            }
            WorkspaceCommands::Show { name } => {
                tracing::info!(name, "showing workspace...");
            }
        },
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
        Commands::Context { work_id, json: _ } => {
            tracing::info!(work_id, "fetching context...");
            // TODO: context retrieval
        }
        Commands::Agent { workspace, prompt } => {
            if let Some(name) = &workspace {
                tracing::info!(name, "running agent for workspace...");
            }
            if let Some(p) = &prompt {
                tracing::info!(p, "executing prompt...");
            }
            // TODO: AgentRuntime implementation
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
