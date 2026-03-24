//! Status display for `belt status` and `belt spec status`.
//!
//! Supports `text`, `json`, and `rich` output formats.
//! The `rich` format includes runtime statistics alongside system status.

use belt_core::phase::QueuePhase;
use belt_infra::db::{Database, RuntimeStats};
use serde::Serialize;

use crate::dashboard;

/// System status summary returned by `belt status`.
#[derive(Debug, Serialize)]
pub struct SystemStatus {
    pub total_items: u32,
    pub phase_counts: Vec<PhaseCount>,
    pub running_items: Vec<ItemSummary>,
    pub recent_events: Vec<EventSummary>,
    pub runtime_stats: Option<RuntimeStats>,
}

/// Count of items in a specific phase.
#[derive(Debug, Serialize)]
pub struct PhaseCount {
    pub phase: String,
    pub count: u32,
}

/// Brief summary of a queue item.
#[derive(Debug, Serialize)]
pub struct ItemSummary {
    pub work_id: String,
    pub workspace: String,
    pub state: String,
    pub phase: String,
    pub updated_at: String,
}

/// Brief summary of a transition event.
#[derive(Debug, Serialize)]
pub struct EventSummary {
    pub item_id: String,
    pub from_state: String,
    pub to_state: String,
    pub event_type: String,
    pub timestamp: String,
}

/// Workspace spec status.
#[derive(Debug, Serialize)]
pub struct SpecStatus {
    pub workspace: String,
    pub config_path: String,
    pub item_count: u32,
    pub phase_counts: Vec<PhaseCount>,
}

/// Display system status in the requested format.
///
/// Opens the default Belt database automatically.
///
/// # Errors
/// Returns an error if the database cannot be opened or queried.
pub fn show_status(format: &str) -> anyhow::Result<()> {
    let db = open_db()?;
    let sys_status = gather_status(&db)?;
    print_status(&sys_status, format)
}

/// Gather system status from the database.
pub fn gather_status(db: &Database) -> anyhow::Result<SystemStatus> {
    let phase_counts_raw = db.count_items_by_phase()?;
    let total_items: u32 = phase_counts_raw.iter().map(|(_, c)| *c).sum();

    let phase_counts = phase_counts_raw
        .into_iter()
        .map(|(phase, count)| PhaseCount { phase, count })
        .collect();

    let running = db.list_items(Some(QueuePhase::Running), None)?;
    let running_items = running
        .into_iter()
        .map(|item| ItemSummary {
            work_id: item.work_id,
            workspace: item.workspace_id,
            state: item.state,
            phase: item.phase.as_str().to_string(),
            updated_at: item.updated_at,
        })
        .collect();

    let events = db.list_recent_transition_events(10)?;
    let recent_events = events
        .into_iter()
        .map(|e| EventSummary {
            item_id: e.item_id,
            from_state: e.from_state,
            to_state: e.to_state,
            event_type: e.event_type,
            timestamp: e.timestamp,
        })
        .collect();

    let runtime_stats = db.get_runtime_stats().ok();

    Ok(SystemStatus {
        total_items,
        phase_counts,
        running_items,
        recent_events,
        runtime_stats,
    })
}

/// Gather spec (workspace) status from the database.
pub fn gather_spec_status(db: &Database, workspace: &str) -> anyhow::Result<SpecStatus> {
    let (name, config_path, _created_at) = db.get_workspace(workspace)?;

    let all_items = db.list_items(None, Some(workspace))?;
    let item_count = all_items.len() as u32;

    let mut counts = std::collections::HashMap::<String, u32>::new();
    for item in &all_items {
        *counts.entry(item.phase.as_str().to_string()).or_insert(0) += 1;
    }

    let mut phase_counts: Vec<PhaseCount> = counts
        .into_iter()
        .map(|(phase, count)| PhaseCount { phase, count })
        .collect();
    phase_counts.sort_by(|a, b| a.phase.cmp(&b.phase));

    Ok(SpecStatus {
        workspace: name,
        config_path,
        item_count,
        phase_counts,
    })
}

/// Print system status in the requested format.
pub fn print_status(status: &SystemStatus, format: &str) -> anyhow::Result<()> {
    match format {
        "json" => {
            println!("{}", serde_json::to_string_pretty(status)?);
        }
        "rich" => {
            print_rich_status(status);
            if let Some(ref s) = status.runtime_stats {
                dashboard::render_runtime_panel(s);
            }
        }
        _ => {
            print_text_status(status);
        }
    }
    Ok(())
}

/// Print spec status in the requested format.
pub fn print_spec_status(status: &SpecStatus, format: &str) -> anyhow::Result<()> {
    match format {
        "json" => {
            println!("{}", serde_json::to_string_pretty(status)?);
        }
        "rich" => {
            print_rich_spec_status(status);
        }
        _ => {
            print_text_spec_status(status);
        }
    }
    Ok(())
}

/// Open the Belt database from the default location (`~/.belt/belt.db`).
fn open_db() -> anyhow::Result<Database> {
    let belt_home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?
        .join(".belt");
    let db_path = belt_home.join("belt.db");
    let db = Database::open(
        db_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid database path"))?,
    )?;
    Ok(db)
}

fn print_text_status(status: &SystemStatus) {
    println!("Belt System Status");
    println!("==================");
    println!("Total items: {}", status.total_items);
    println!();
    if !status.phase_counts.is_empty() {
        println!("Phase breakdown:");
        for pc in &status.phase_counts {
            println!("  {:<12} {}", pc.phase, pc.count);
        }
        println!();
    }
    if status.running_items.is_empty() {
        println!("No items currently running.");
    } else {
        println!("Running items:");
        for item in &status.running_items {
            println!("  {} ({}/{})", item.work_id, item.workspace, item.state);
        }
    }
    println!();
    if !status.recent_events.is_empty() {
        println!("Recent transitions:");
        for ev in &status.recent_events {
            println!(
                "  {} -> {} [{}] {} ({})",
                ev.from_state, ev.to_state, ev.event_type, ev.item_id, ev.timestamp
            );
        }
    }
    if let Some(ref s) = status.runtime_stats {
        println!();
        println!(
            "  Tokens (24h): {} total, {} executions",
            s.total_tokens, s.executions
        );
    }
}

fn print_rich_status(status: &SystemStatus) {
    println!("+--------------------------------------+");
    println!("|        Belt System Status            |");
    println!("+--------------------------------------+");
    println!("| Total items: {:<23} |", status.total_items);
    println!("+--------------------------------------+");
    if !status.phase_counts.is_empty() {
        println!("| Phase          | Count               |");
        println!("+----------------+---------------------+");
        for pc in &status.phase_counts {
            println!("| {:<14} | {:<19} |", pc.phase, pc.count);
        }
        println!("+----------------+---------------------+");
    }

    if !status.running_items.is_empty() {
        println!();
        println!("+-- Running Items ----------------------+");
        for item in &status.running_items {
            println!(
                "| {:<36} |",
                format!("{} ({})", item.work_id, item.workspace)
            );
        }
        println!("+--------------------------------------+");
    }

    if !status.recent_events.is_empty() {
        println!();
        println!("+-- Recent Transitions -----------------+");
        for ev in &status.recent_events {
            println!("| {} -> {} [{}]", ev.from_state, ev.to_state, ev.event_type);
        }
        println!("+--------------------------------------+");
    }
}

fn print_text_spec_status(status: &SpecStatus) {
    println!("Workspace: {}", status.workspace);
    println!("Config:    {}", status.config_path);
    println!("Items:     {}", status.item_count);
    println!();
    if status.phase_counts.is_empty() {
        println!("No items in this workspace.");
    } else {
        println!("Phase breakdown:");
        for pc in &status.phase_counts {
            println!("  {:<12} {}", pc.phase, pc.count);
        }
    }
}

fn print_rich_spec_status(status: &SpecStatus) {
    println!("+--------------------------------------+");
    println!("| Workspace: {:<25} |", status.workspace);
    println!("| Config:    {:<25} |", status.config_path);
    println!("| Items:     {:<25} |", status.item_count);
    println!("+--------------------------------------+");
    if !status.phase_counts.is_empty() {
        println!("| Phase          | Count               |");
        println!("+----------------+---------------------+");
        for pc in &status.phase_counts {
            println!("| {:<14} | {:<19} |", pc.phase, pc.count);
        }
        println!("+----------------+---------------------+");
    }
}
