//! Interactive REPL session for Claw.
//!
//! Provides a simple command loop that reads user input, dispatches slash
//! commands, and prints responses. On session entry a status banner is
//! collected from the Belt database (queue item counts by phase, HITL
//! pending count, and recent transition events) and displayed to the user.

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
/// is unavailable).
///
/// This signature allows testing without real stdio.
pub fn run_session<R: BufRead, W: Write>(
    config: &SessionConfig,
    input: &mut R,
    output: &mut W,
    status: Option<&StatusSummary>,
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
/// Automatically collects system status from the Belt database and
/// displays it as a banner.  If the database is unavailable the session
/// starts without the banner.
pub fn run_interactive(config: SessionConfig) -> anyhow::Result<()> {
    let status = collect_status();
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    run_session(&config, &mut reader, &mut writer, status.as_ref())
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
        run_session(&config, &mut input, &mut output, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Goodbye."));
    }

    #[test]
    fn session_quit_on_eof() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Belt Claw interactive session"));
    }

    #[test]
    fn session_dispatches_help() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/help\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None).unwrap();
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
        run_session(&config, &mut input, &mut output, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains(">> hello world"));
    }

    #[test]
    fn session_shows_workspace_context() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None).unwrap();
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
        run_session(&config, &mut input, &mut output, Some(&summary)).unwrap();
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
        run_session(&config, &mut input, &mut output, None).unwrap();
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
}
