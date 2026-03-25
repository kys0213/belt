//! Interactive REPL session for Claw.
//!
//! Provides a simple command loop that reads user input, dispatches slash
//! commands, and prints responses. On session entry a status banner is
//! collected from the Belt database (queue item counts by phase, HITL
//! pending count, recent transition events, and per-workspace statistics)
//! and displayed to the user.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use super::ClawWorkspace;
use super::slash::{SlashCommand, SlashDispatcher};

/// Summary of system status collected from the Belt database.
///
/// Displayed as a banner when a Claw session starts.
#[derive(Debug, Default)]
pub struct StatusSummary {
    /// Total queue items across all phases.
    pub total_items: u32,
    /// Queue item counts grouped by phase name.
    pub phase_counts: Vec<(String, u32)>,
    /// Number of items currently in the HITL phase.
    pub hitl_pending: u32,
    /// Most recent transition events (up to 5).
    pub recent_events: Vec<RecentEvent>,
}

/// A brief description of a recent transition event.
#[derive(Debug)]
pub struct RecentEvent {
    /// The queue item identifier.
    pub item_id: String,
    /// Phase/state the item transitioned from.
    pub from_state: String,
    /// Phase/state the item transitioned to.
    pub to_state: String,
    /// When the transition occurred (RFC 3339).
    pub timestamp: String,
}

/// Per-workspace statistics displayed alongside the system-wide status banner.
///
/// Provides a breakdown of spec lifecycle counts and queue item counts
/// scoped to a single workspace.
#[derive(Debug, Default)]
pub struct WorkspaceStats {
    /// Name of the workspace these stats belong to.
    pub workspace_name: String,
    /// Number of specs in `active` status.
    pub active_spec_count: u32,
    /// Number of specs in `completing` status.
    pub completing_count: u32,
    /// Number of specs in `completed` status.
    pub completed_count: u32,
    /// Number of queue items in `pending` phase for this workspace.
    pub pending_items_count: u32,
    /// Number of queue items in `running` phase for this workspace.
    pub running_items_count: u32,
    /// Recent HITL events scoped to this workspace (up to 5).
    pub recent_hitl_events: Vec<RecentEvent>,
}

/// Collect system status from the Belt database.
///
/// Opens the default `~/.belt/belt.db` database and gathers queue item
/// counts by phase, the HITL pending count, and the 5 most recent
/// transition events. Returns `None` if the database is unavailable.
pub fn collect_status() -> Option<StatusSummary> {
    let belt_home = dirs::home_dir()?.join(".belt");
    let db_path = belt_home.join("belt.db");
    let db = belt_infra::db::Database::open(db_path.to_str()?).ok()?;
    collect_status_from_db(&db)
}

/// Collect system status from a given database handle.
///
/// Separated from [`collect_status`] so tests can inject an in-memory DB.
fn collect_status_from_db(db: &belt_infra::db::Database) -> Option<StatusSummary> {
    let phase_counts = db.count_items_by_phase().ok()?;
    let total_items: u32 = phase_counts.iter().map(|(_, c)| *c).sum();
    let hitl_pending = phase_counts
        .iter()
        .find(|(p, _)| p == "hitl")
        .map(|(_, c)| *c)
        .unwrap_or(0);

    let events = db.list_recent_transition_events(5).ok()?;
    let recent_events = events
        .into_iter()
        .map(|e| RecentEvent {
            item_id: e.item_id,
            from_state: e.from_state,
            to_state: e.to_state,
            timestamp: e.timestamp,
        })
        .collect();

    Some(StatusSummary {
        total_items,
        phase_counts,
        hitl_pending,
        recent_events,
    })
}

/// Collect workspace-level statistics from the Belt database.
///
/// Opens the default `~/.belt/belt.db` database and gathers spec counts by
/// status and queue item counts by phase for the given workspace.
/// Returns `None` if the database is unavailable or the workspace name is absent.
pub fn collect_workspace_stats(workspace: Option<&str>) -> Option<WorkspaceStats> {
    let ws_name = workspace?;
    let belt_home = dirs::home_dir()?.join(".belt");
    let db_path = belt_home.join("belt.db");
    let db = belt_infra::db::Database::open(db_path.to_str()?).ok()?;
    collect_workspace_stats_from_db(&db, ws_name)
}

/// Collect workspace-level statistics from a given database handle.
///
/// Separated from [`collect_workspace_stats`] so tests can inject an in-memory DB.
fn collect_workspace_stats_from_db(
    db: &belt_infra::db::Database,
    workspace: &str,
) -> Option<WorkspaceStats> {
    use belt_core::spec::SpecStatus;

    // Count specs by status for this workspace.
    let specs = db.list_specs(Some(workspace), None).ok()?;
    let mut spec_status_counts: HashMap<SpecStatus, u32> = HashMap::new();
    for spec in &specs {
        *spec_status_counts.entry(spec.status).or_insert(0) += 1;
    }

    let active_spec_count = spec_status_counts
        .get(&SpecStatus::Active)
        .copied()
        .unwrap_or(0);
    let completing_count = spec_status_counts
        .get(&SpecStatus::Completing)
        .copied()
        .unwrap_or(0);
    let completed_count = spec_status_counts
        .get(&SpecStatus::Completed)
        .copied()
        .unwrap_or(0);

    // Count queue items by phase for this workspace.
    let items = db.list_items(None, Some(workspace)).ok()?;
    let mut pending_items_count: u32 = 0;
    let mut running_items_count: u32 = 0;
    let mut hitl_item_ids: Vec<String> = Vec::new();
    for item in &items {
        match item.phase {
            belt_core::phase::QueuePhase::Pending => pending_items_count += 1,
            belt_core::phase::QueuePhase::Running => running_items_count += 1,
            belt_core::phase::QueuePhase::Hitl => {
                hitl_item_ids.push(item.work_id.clone());
            }
            _ => {}
        }
    }

    // Gather recent HITL transition events for this workspace's items.
    let recent_hitl_events = if !hitl_item_ids.is_empty() {
        let all_events = db.list_recent_transition_events(50).ok()?;
        all_events
            .into_iter()
            .filter(|e| e.to_state == "hitl" && hitl_item_ids.contains(&e.item_id))
            .take(5)
            .map(|e| RecentEvent {
                item_id: e.item_id,
                from_state: e.from_state,
                to_state: e.to_state,
                timestamp: e.timestamp,
            })
            .collect()
    } else {
        Vec::new()
    };

    Some(WorkspaceStats {
        workspace_name: workspace.to_string(),
        active_spec_count,
        completing_count,
        completed_count,
        pending_items_count,
        running_items_count,
        recent_hitl_events,
    })
}

/// Write the status banner to the given output stream.
fn write_status_banner<W: Write>(output: &mut W, summary: &StatusSummary) -> io::Result<()> {
    writeln!(output, "--- System Status ---")?;
    writeln!(output, "Queue: {} items", summary.total_items)?;

    if !summary.phase_counts.is_empty() {
        let parts: Vec<String> = summary
            .phase_counts
            .iter()
            .map(|(phase, count)| format!("{phase}={count}"))
            .collect();
        writeln!(output, "  Phases: {}", parts.join(", "))?;
    }

    if summary.hitl_pending > 0 {
        writeln!(
            output,
            "  HITL pending: {} (needs human review)",
            summary.hitl_pending
        )?;
    }

    if !summary.recent_events.is_empty() {
        writeln!(output, "  Recent events:")?;
        for ev in &summary.recent_events {
            writeln!(
                output,
                "    {} : {} -> {} ({})",
                ev.item_id, ev.from_state, ev.to_state, ev.timestamp
            )?;
        }
    }

    writeln!(output, "---------------------")?;
    Ok(())
}

/// Write the workspace stats banner to the given output stream.
fn write_workspace_stats_banner<W: Write>(
    output: &mut W,
    stats: &WorkspaceStats,
) -> io::Result<()> {
    writeln!(output, "--- Workspace: {} ---", stats.workspace_name)?;
    writeln!(
        output,
        "Specs: active={}, completing={}, completed={}",
        stats.active_spec_count, stats.completing_count, stats.completed_count
    )?;
    writeln!(
        output,
        "Items: pending={}, running={}",
        stats.pending_items_count, stats.running_items_count
    )?;

    if !stats.recent_hitl_events.is_empty() {
        writeln!(output, "  Recent HITL events:")?;
        for ev in &stats.recent_hitl_events {
            writeln!(
                output,
                "    {} : {} -> {} ({})",
                ev.item_id, ev.from_state, ev.to_state, ev.timestamp
            )?;
        }
    }

    writeln!(output, "---------------------")?;
    Ok(())
}

/// Interactive session configuration.
pub struct SessionConfig {
    /// Workspace name context (if running inside a specific workspace).
    pub workspace: Option<String>,
    /// The Claw workspace root.
    pub claw_workspace: ClawWorkspace,
}

/// Run the interactive REPL session.
///
/// Reads lines from `input`, writes prompts/responses to `output`.
/// An optional [`StatusSummary`] is displayed as a banner at the top of
/// the session.  Pass `None` to skip the status banner (e.g. when the DB
/// is unavailable).  An optional [`WorkspaceStats`] adds per-workspace
/// statistics below the system-wide banner.
///
/// This signature allows testing without real stdio.
pub fn run_session<R: BufRead, W: Write>(
    config: &SessionConfig,
    input: &mut R,
    output: &mut W,
    status: Option<&StatusSummary>,
    workspace_stats: Option<&WorkspaceStats>,
) -> anyhow::Result<()> {
    let dispatcher = SlashDispatcher::new(config.workspace.clone());

    writeln!(output, "Belt Claw interactive session")?;
    writeln!(
        output,
        "Workspace: {}",
        config.claw_workspace.path.display()
    )?;
    if let Some(ref ws) = config.workspace {
        writeln!(output, "Context: {ws}")?;
    }

    // Display the status banner when available.
    if let Some(summary) = status {
        writeln!(output)?;
        write_status_banner(output, summary)?;
    }

    // Display per-workspace stats when available.
    if let Some(ws_stats) = workspace_stats {
        writeln!(output)?;
        write_workspace_stats_banner(output, ws_stats)?;
    }

    writeln!(output, "Type /help for available commands, /quit to exit.")?;
    writeln!(output)?;

    let mut line = String::new();
    loop {
        write!(output, "claw> ")?;
        output.flush()?;

        line.clear();
        let bytes_read = input.read_line(&mut line)?;
        if bytes_read == 0 {
            // EOF
            writeln!(output)?;
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(cmd) = SlashCommand::parse(trimmed) {
            match cmd {
                SlashCommand::Quit => {
                    writeln!(output, "Goodbye.")?;
                    break;
                }
                _ => {
                    let response = dispatcher.dispatch(&cmd);
                    writeln!(output, "{response}")?;
                }
            }
        } else {
            // Free-form text input (future: pass to LLM agent).
            writeln!(output, ">> {trimmed}")?;
        }
    }

    Ok(())
}

/// Run the interactive session using real stdin/stdout.
///
/// Automatically collects system status and per-workspace statistics from
/// the Belt database and displays them as banners.  If the database is
/// unavailable the session starts without banners.
pub fn run_interactive(config: SessionConfig) -> anyhow::Result<()> {
    let status = collect_status();
    let ws_stats = collect_workspace_stats(config.workspace.as_deref());
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    run_session(
        &config,
        &mut reader,
        &mut writer,
        status.as_ref(),
        ws_stats.as_ref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn make_config(tmp: &tempfile::TempDir) -> SessionConfig {
        let ws = ClawWorkspace::init(tmp.path()).unwrap();
        SessionConfig {
            workspace: Some("test-ws".to_string()),
            claw_workspace: ws,
        }
    }

    #[test]
    fn session_quit_on_slash_quit() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Goodbye."));
    }

    #[test]
    fn session_quit_on_eof() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Belt Claw interactive session"));
    }

    #[test]
    fn session_dispatches_help() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/help\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("/auto"));
        assert!(out.contains("/spec"));
        assert!(out.contains("/claw"));
    }

    #[test]
    fn session_echoes_freeform_text() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"hello world\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains(">> hello world"));
    }

    #[test]
    fn session_shows_workspace_context() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Context: test-ws"));
    }

    #[test]
    fn session_displays_status_banner() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let summary = StatusSummary {
            total_items: 12,
            phase_counts: vec![
                ("pending".to_string(), 5),
                ("running".to_string(), 3),
                ("hitl".to_string(), 2),
                ("done".to_string(), 2),
            ],
            hitl_pending: 2,
            recent_events: vec![RecentEvent {
                item_id: "item-1".to_string(),
                from_state: "running".to_string(),
                to_state: "hitl".to_string(),
                timestamp: "2026-03-24T10:00:00Z".to_string(),
            }],
        };
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, Some(&summary), None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("System Status"));
        assert!(out.contains("Queue: 12 items"));
        assert!(out.contains("pending=5"));
        assert!(out.contains("hitl=2"));
        assert!(out.contains("HITL pending: 2"));
        assert!(out.contains("item-1"));
        assert!(out.contains("running -> hitl"));
    }

    #[test]
    fn session_no_banner_when_status_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(!out.contains("System Status"));
    }

    #[test]
    fn status_banner_omits_hitl_line_when_zero() {
        let summary = StatusSummary {
            total_items: 3,
            phase_counts: vec![("pending".to_string(), 3)],
            hitl_pending: 0,
            recent_events: vec![],
        };
        let mut output = Vec::new();
        write_status_banner(&mut output, &summary).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Queue: 3 items"));
        assert!(!out.contains("HITL pending"));
    }

    #[test]
    fn collect_status_from_empty_db() {
        let db = belt_infra::db::Database::open_in_memory().unwrap();
        let summary = collect_status_from_db(&db).unwrap();
        assert_eq!(summary.total_items, 0);
        assert!(summary.phase_counts.is_empty());
        assert_eq!(summary.hitl_pending, 0);
        assert!(summary.recent_events.is_empty());
    }

    #[test]
    fn collect_status_from_populated_db() {
        use belt_core::phase::QueuePhase;
        use belt_core::queue::QueueItem;

        let db = belt_infra::db::Database::open_in_memory().unwrap();

        // Insert some items in different phases.
        let item1 = QueueItem::new(
            "w1".to_string(),
            "s1".to_string(),
            "ws1".to_string(),
            "analyze".to_string(),
        );
        db.insert_item(&item1).unwrap();

        let item2 = QueueItem::new(
            "w2".to_string(),
            "s2".to_string(),
            "ws1".to_string(),
            "implement".to_string(),
        );
        db.insert_item(&item2).unwrap();
        db.update_phase("w2", QueuePhase::Hitl).unwrap();

        let summary = collect_status_from_db(&db).unwrap();
        assert_eq!(summary.total_items, 2);
        assert_eq!(summary.hitl_pending, 1);
    }

    #[test]
    fn workspace_stats_banner_displays_counts() {
        let stats = WorkspaceStats {
            workspace_name: "my-project".to_string(),
            active_spec_count: 3,
            completing_count: 1,
            completed_count: 5,
            pending_items_count: 4,
            running_items_count: 2,
            recent_hitl_events: vec![],
        };
        let mut output = Vec::new();
        write_workspace_stats_banner(&mut output, &stats).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Workspace: my-project"));
        assert!(out.contains("active=3"));
        assert!(out.contains("completing=1"));
        assert!(out.contains("completed=5"));
        assert!(out.contains("pending=4"));
        assert!(out.contains("running=2"));
    }

    #[test]
    fn workspace_stats_banner_shows_hitl_events() {
        let stats = WorkspaceStats {
            workspace_name: "ws".to_string(),
            active_spec_count: 0,
            completing_count: 0,
            completed_count: 0,
            pending_items_count: 0,
            running_items_count: 0,
            recent_hitl_events: vec![RecentEvent {
                item_id: "hitl-item".to_string(),
                from_state: "running".to_string(),
                to_state: "hitl".to_string(),
                timestamp: "2026-03-25T12:00:00Z".to_string(),
            }],
        };
        let mut output = Vec::new();
        write_workspace_stats_banner(&mut output, &stats).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Recent HITL events"));
        assert!(out.contains("hitl-item"));
        assert!(out.contains("running -> hitl"));
    }

    #[test]
    fn workspace_stats_banner_omits_hitl_when_empty() {
        let stats = WorkspaceStats {
            workspace_name: "ws".to_string(),
            recent_hitl_events: vec![],
            ..WorkspaceStats::default()
        };
        let mut output = Vec::new();
        write_workspace_stats_banner(&mut output, &stats).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(!out.contains("Recent HITL events"));
    }

    #[test]
    fn session_displays_workspace_stats() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let stats = WorkspaceStats {
            workspace_name: "test-ws".to_string(),
            active_spec_count: 2,
            completing_count: 0,
            completed_count: 1,
            pending_items_count: 3,
            running_items_count: 1,
            recent_hitl_events: vec![],
        };
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, Some(&stats)).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Workspace: test-ws ---"));
        assert!(out.contains("active=2"));
        assert!(out.contains("pending=3"));
    }

    #[test]
    fn session_no_workspace_stats_when_none() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(!out.contains("Specs:"));
        assert!(!out.contains("Items:"));
    }

    #[test]
    fn collect_workspace_stats_from_empty_db() {
        let db = belt_infra::db::Database::open_in_memory().unwrap();
        let stats = collect_workspace_stats_from_db(&db, "ws1").unwrap();
        assert_eq!(stats.workspace_name, "ws1");
        assert_eq!(stats.active_spec_count, 0);
        assert_eq!(stats.completing_count, 0);
        assert_eq!(stats.completed_count, 0);
        assert_eq!(stats.pending_items_count, 0);
        assert_eq!(stats.running_items_count, 0);
        assert!(stats.recent_hitl_events.is_empty());
    }

    #[test]
    fn collect_workspace_stats_from_populated_db() {
        use belt_core::phase::QueuePhase;
        use belt_core::queue::QueueItem;
        use belt_core::spec::{Spec, SpecStatus};

        let db = belt_infra::db::Database::open_in_memory().unwrap();

        // Insert specs.
        let mut spec1 = Spec::new(
            "sp1".to_string(),
            "ws1".to_string(),
            "Spec 1".to_string(),
            "content".to_string(),
        );
        spec1.status = SpecStatus::Active;
        db.insert_spec(&spec1).unwrap();

        let mut spec2 = Spec::new(
            "sp2".to_string(),
            "ws1".to_string(),
            "Spec 2".to_string(),
            "content".to_string(),
        );
        spec2.status = SpecStatus::Completing;
        db.insert_spec(&spec2).unwrap();

        // Insert queue items.
        let item1 = QueueItem::new(
            "w1".to_string(),
            "s1".to_string(),
            "ws1".to_string(),
            "analyze".to_string(),
        );
        db.insert_item(&item1).unwrap();

        let item2 = QueueItem::new(
            "w2".to_string(),
            "s2".to_string(),
            "ws1".to_string(),
            "implement".to_string(),
        );
        db.insert_item(&item2).unwrap();
        db.update_phase("w2", QueuePhase::Running).unwrap();

        let stats = collect_workspace_stats_from_db(&db, "ws1").unwrap();
        assert_eq!(stats.active_spec_count, 1);
        assert_eq!(stats.completing_count, 1);
        assert_eq!(stats.completed_count, 0);
        assert_eq!(stats.pending_items_count, 1);
        assert_eq!(stats.running_items_count, 1);
    }

    #[test]
    fn collect_workspace_stats_filters_by_workspace() {
        use belt_core::queue::QueueItem;
        use belt_core::spec::{Spec, SpecStatus};

        let db = belt_infra::db::Database::open_in_memory().unwrap();

        // Insert spec in ws1.
        let mut spec = Spec::new(
            "sp1".to_string(),
            "ws1".to_string(),
            "Spec".to_string(),
            "content".to_string(),
        );
        spec.status = SpecStatus::Active;
        db.insert_spec(&spec).unwrap();

        // Insert spec in ws2.
        let mut spec2 = Spec::new(
            "sp2".to_string(),
            "ws2".to_string(),
            "Other".to_string(),
            "content".to_string(),
        );
        spec2.status = SpecStatus::Active;
        db.insert_spec(&spec2).unwrap();

        // Insert item in ws2.
        let item = QueueItem::new(
            "w1".to_string(),
            "s1".to_string(),
            "ws2".to_string(),
            "analyze".to_string(),
        );
        db.insert_item(&item).unwrap();

        // Stats for ws1 should not include ws2 data.
        let stats = collect_workspace_stats_from_db(&db, "ws1").unwrap();
        assert_eq!(stats.active_spec_count, 1);
        assert_eq!(stats.pending_items_count, 0);

        // Stats for ws2 should not include ws1 data.
        let stats2 = collect_workspace_stats_from_db(&db, "ws2").unwrap();
        assert_eq!(stats2.active_spec_count, 1);
        assert_eq!(stats2.pending_items_count, 1);
    }
}
