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
    /// HITL items grouped by escalation reason for display on claw entry.
    pub hitl_items: Vec<HitlItemSummary>,
}

/// Brief summary of a HITL queue item for display.
#[derive(Debug, Clone)]
pub struct HitlItemSummary {
    /// The queue item work_id.
    pub work_id: String,
    /// Workspace the item belongs to.
    pub workspace: String,
    /// Escalation reason (hitl_reason field), or "other" if unset.
    pub reason: String,
    /// Item title if available.
    pub title: Option<String>,
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

    let hitl_items = db
        .list_items(Some(belt_core::phase::QueuePhase::Hitl), None)
        .ok()
        .unwrap_or_default()
        .into_iter()
        .map(|item| HitlItemSummary {
            work_id: item.work_id,
            workspace: item.workspace_id,
            reason: item
                .hitl_reason
                .map(|r| r.to_string())
                .unwrap_or_else(|| "other".to_string()),
            title: item.title,
        })
        .collect();

    Some(StatusSummary {
        total_items,
        phase_counts,
        hitl_pending,
        recent_events,
        hitl_items,
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

    // Display HITL items grouped by escalation reason with priority ordering.
    if !summary.hitl_items.is_empty() {
        write_hitl_list(output, &summary.hitl_items)?;
    }

    Ok(())
}

/// Priority order for HITL escalation reasons.
///
/// Lower value = higher priority. Reasons not in this list get the lowest
/// priority.
fn reason_priority(reason: &str) -> u32 {
    match reason {
        "evaluate_failure" => 0,               // spec-conflict equivalent
        "retry_max_exceeded" | "timeout" => 1, // failure category
        _ => 2,                                // other (manual_escalation, unknown)
    }
}

/// Display label for an escalation reason group.
fn reason_display_label(reason: &str) -> &str {
    match reason {
        "evaluate_failure" => "Spec Conflict (evaluate_failure)",
        "retry_max_exceeded" => "Failure (retry_max_exceeded)",
        "timeout" => "Failure (timeout)",
        "manual_escalation" => "Other (manual_escalation)",
        _ => "Other",
    }
}

/// Write HITL items grouped by escalation reason to the output stream.
fn write_hitl_list<W: Write>(output: &mut W, items: &[HitlItemSummary]) -> io::Result<()> {
    use std::collections::BTreeMap;

    // Group items by reason.
    let mut groups: BTreeMap<String, Vec<&HitlItemSummary>> = BTreeMap::new();
    for item in items {
        groups.entry(item.reason.clone()).or_default().push(item);
    }

    // Sort groups by priority.
    let mut sorted_groups: Vec<(String, Vec<&HitlItemSummary>)> = groups.into_iter().collect();
    sorted_groups.sort_by_key(|(reason, _)| reason_priority(reason));

    writeln!(output)?;
    writeln!(
        output,
        "--- HITL Items ({} awaiting review) ---",
        items.len()
    )?;

    for (reason, group_items) in &sorted_groups {
        writeln!(output, "  [{}]", reason_display_label(reason))?;
        for item in group_items {
            let title = item.title.as_deref().unwrap_or("-");
            writeln!(
                output,
                "    {:<40} {:<16} {}",
                item.work_id, item.workspace, title
            )?;
        }
    }

    writeln!(output, "---------------------------------------")?;
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
            hitl_items: vec![],
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
            hitl_items: vec![],
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
        assert!(summary.hitl_items.is_empty());
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

        let mut item2 = QueueItem::new(
            "w2".to_string(),
            "s2".to_string(),
            "ws1".to_string(),
            "implement".to_string(),
        );
        item2.hitl_reason = Some(belt_core::queue::HitlReason::EvaluateFailure);
        db.insert_item(&item2).unwrap();
        db.update_phase("w2", QueuePhase::Hitl).unwrap();

        let summary = collect_status_from_db(&db).unwrap();
        assert_eq!(summary.total_items, 2);
        assert_eq!(summary.hitl_pending, 1);
        assert_eq!(summary.hitl_items.len(), 1);
        assert_eq!(summary.hitl_items[0].work_id, "w2");
    }

    #[test]
    fn collect_status_hitl_items_empty_when_no_hitl() {
        let db = belt_infra::db::Database::open_in_memory().unwrap();

        let item = belt_core::queue::QueueItem::new(
            "w1".to_string(),
            "s1".to_string(),
            "ws1".to_string(),
            "analyze".to_string(),
        );
        db.insert_item(&item).unwrap();

        let summary = collect_status_from_db(&db).unwrap();
        assert!(summary.hitl_items.is_empty());
    }

    #[test]
    fn claw_entry_displays_hitl_list_grouped_by_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let summary = StatusSummary {
            total_items: 5,
            phase_counts: vec![("pending".to_string(), 2), ("hitl".to_string(), 3)],
            hitl_pending: 3,
            recent_events: vec![],
            hitl_items: vec![
                HitlItemSummary {
                    work_id: "w1:impl".to_string(),
                    workspace: "ws-a".to_string(),
                    reason: "evaluate_failure".to_string(),
                    title: Some("Spec conflict item".to_string()),
                },
                HitlItemSummary {
                    work_id: "w2:impl".to_string(),
                    workspace: "ws-a".to_string(),
                    reason: "retry_max_exceeded".to_string(),
                    title: Some("Retry exceeded item".to_string()),
                },
                HitlItemSummary {
                    work_id: "w3:impl".to_string(),
                    workspace: "ws-b".to_string(),
                    reason: "other".to_string(),
                    title: None,
                },
            ],
        };
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, Some(&summary)).unwrap();
        let out = String::from_utf8(output).unwrap();

        // Verify HITL list is displayed.
        assert!(out.contains("HITL Items (3 awaiting review)"));
        // Verify grouping labels appear.
        assert!(out.contains("Spec Conflict (evaluate_failure)"));
        assert!(out.contains("Failure (retry_max_exceeded)"));
        assert!(out.contains("Other"));
        // Verify items are listed.
        assert!(out.contains("w1:impl"));
        assert!(out.contains("w2:impl"));
        assert!(out.contains("w3:impl"));
    }

    #[test]
    fn claw_entry_no_hitl_list_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let summary = StatusSummary {
            total_items: 2,
            phase_counts: vec![("pending".to_string(), 2)],
            hitl_pending: 0,
            recent_events: vec![],
            hitl_items: vec![],
        };
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, Some(&summary)).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(!out.contains("HITL Items"));
    }

    #[test]
    fn collect_status_from_db_multiple_phases() {
        use belt_core::phase::QueuePhase;
        use belt_core::queue::QueueItem;

        let db = belt_infra::db::Database::open_in_memory().unwrap();

        // Insert items and move them to different phases.
        for (wid, spec, ws) in [
            ("a1", "s1", "ws1"),
            ("a2", "s2", "ws1"),
            ("b1", "s3", "ws2"),
            ("c1", "s4", "ws1"),
        ] {
            let item = QueueItem::new(
                wid.to_string(),
                spec.to_string(),
                ws.to_string(),
                "step".to_string(),
            );
            db.insert_item(&item).unwrap();
        }
        // a1, a2 stay pending; b1 -> running; c1 -> done
        db.update_phase("b1", QueuePhase::Running).unwrap();
        db.update_phase("c1", QueuePhase::Done).unwrap();

        let summary = collect_status_from_db(&db).unwrap();
        assert_eq!(summary.total_items, 4);
        assert_eq!(summary.hitl_pending, 0);
        assert!(summary.hitl_items.is_empty());
        // Verify all phases are represented.
        let phase_names: Vec<&str> = summary
            .phase_counts
            .iter()
            .map(|(p, _)| p.as_str())
            .collect();
        assert!(phase_names.contains(&"pending"));
        assert!(phase_names.contains(&"running"));
        assert!(phase_names.contains(&"done"));
        // Check counts.
        let pending_count = summary
            .phase_counts
            .iter()
            .find(|(p, _)| p == "pending")
            .map(|(_, c)| *c);
        assert_eq!(pending_count, Some(2));
    }

    #[test]
    fn collect_status_hitl_item_has_correct_reason_and_title() {
        use belt_core::phase::QueuePhase;
        use belt_core::queue::QueueItem;

        let db = belt_infra::db::Database::open_in_memory().unwrap();

        let mut item = QueueItem::new(
            "w-hitl".to_string(),
            "s1".to_string(),
            "ws-test".to_string(),
            "implement".to_string(),
        );
        item.title = Some("My HITL task".to_string());
        item.hitl_reason = Some(belt_core::queue::HitlReason::Timeout);
        db.insert_item(&item).unwrap();
        db.update_phase("w-hitl", QueuePhase::Hitl).unwrap();

        let summary = collect_status_from_db(&db).unwrap();
        assert_eq!(summary.hitl_items.len(), 1);
        assert_eq!(summary.hitl_items[0].work_id, "w-hitl");
        assert_eq!(summary.hitl_items[0].workspace, "ws-test");
        assert_eq!(summary.hitl_items[0].reason, "timeout");
        assert_eq!(summary.hitl_items[0].title.as_deref(), Some("My HITL task"));
    }

    #[test]
    fn status_banner_empty_phases_omits_phases_line() {
        let summary = StatusSummary {
            total_items: 0,
            phase_counts: vec![],
            hitl_pending: 0,
            recent_events: vec![],
            hitl_items: vec![],
        };
        let mut output = Vec::new();
        write_status_banner(&mut output, &summary).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Queue: 0 items"));
        assert!(!out.contains("Phases:"));
    }

    #[test]
    fn status_banner_omits_recent_events_section_when_empty() {
        let summary = StatusSummary {
            total_items: 1,
            phase_counts: vec![("pending".to_string(), 1)],
            hitl_pending: 0,
            recent_events: vec![],
            hitl_items: vec![],
        };
        let mut output = Vec::new();
        write_status_banner(&mut output, &summary).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(!out.contains("Recent events:"));
    }

    #[test]
    fn status_banner_shows_multiple_events() {
        let summary = StatusSummary {
            total_items: 2,
            phase_counts: vec![("done".to_string(), 2)],
            hitl_pending: 0,
            recent_events: vec![
                RecentEvent {
                    item_id: "ev-1".to_string(),
                    from_state: "pending".to_string(),
                    to_state: "running".to_string(),
                    timestamp: "2026-03-24T09:00:00Z".to_string(),
                },
                RecentEvent {
                    item_id: "ev-2".to_string(),
                    from_state: "running".to_string(),
                    to_state: "done".to_string(),
                    timestamp: "2026-03-24T10:00:00Z".to_string(),
                },
            ],
            hitl_items: vec![],
        };
        let mut output = Vec::new();
        write_status_banner(&mut output, &summary).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Recent events:"));
        assert!(out.contains("ev-1"));
        assert!(out.contains("ev-2"));
        assert!(out.contains("pending -> running"));
        assert!(out.contains("running -> done"));
    }

    #[test]
    fn session_dispatches_auto_command() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/auto run task\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("[auto]"));
        assert!(out.contains("run task"));
    }

    #[test]
    fn session_dispatches_spec_command() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/spec issue-42\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("[spec]"));
        assert!(out.contains("issue-42"));
    }

    #[test]
    fn session_dispatches_claw_command() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/claw status\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("[claw]"));
        assert!(out.contains("test-ws"));
    }

    #[test]
    fn session_dispatches_unknown_command() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/unknown\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Unknown command"));
    }

    #[test]
    fn session_exit_alias_works() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/exit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Goodbye."));
    }

    #[test]
    fn session_skips_empty_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"\n\n\nhello\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        // Empty lines should be skipped, only the freeform text echoed.
        assert!(out.contains(">> hello"));
        assert!(out.contains("Goodbye."));
    }

    #[test]
    fn session_multiple_commands_in_sequence() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/help\n/auto\n/spec\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("/auto"));
        assert!(out.contains("[auto]"));
        assert!(out.contains("[spec]"));
        assert!(out.contains("Goodbye."));
    }

    #[test]
    fn session_no_workspace_context_omits_context_line() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ClawWorkspace::init(tmp.path()).unwrap();
        let config = SessionConfig {
            workspace: None,
            claw_workspace: ws,
        };
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(!out.contains("Context:"));
    }

    #[test]
    fn reason_priority_ordering() {
        assert!(reason_priority("evaluate_failure") < reason_priority("retry_max_exceeded"));
        assert!(reason_priority("evaluate_failure") < reason_priority("timeout"));
        assert_eq!(
            reason_priority("retry_max_exceeded"),
            reason_priority("timeout")
        );
        assert!(reason_priority("timeout") < reason_priority("manual_escalation"));
        assert!(reason_priority("timeout") < reason_priority("other"));
    }

    #[test]
    fn reason_display_labels_are_correct() {
        assert_eq!(
            reason_display_label("evaluate_failure"),
            "Spec Conflict (evaluate_failure)"
        );
        assert_eq!(
            reason_display_label("retry_max_exceeded"),
            "Failure (retry_max_exceeded)"
        );
        assert_eq!(reason_display_label("timeout"), "Failure (timeout)");
        assert_eq!(
            reason_display_label("manual_escalation"),
            "Other (manual_escalation)"
        );
        assert_eq!(reason_display_label("something_else"), "Other");
    }

    #[test]
    fn hitl_list_shows_title_dash_when_none() {
        let items = vec![HitlItemSummary {
            work_id: "w-no-title".to_string(),
            workspace: "ws".to_string(),
            reason: "other".to_string(),
            title: None,
        }];
        let mut output = Vec::new();
        write_hitl_list(&mut output, &items).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("w-no-title"));
        assert!(out.contains("-"));
    }

    #[test]
    fn hitl_list_priority_order_spec_conflict_first() {
        let items = vec![
            HitlItemSummary {
                work_id: "other-item".to_string(),
                workspace: "ws".to_string(),
                reason: "other".to_string(),
                title: None,
            },
            HitlItemSummary {
                work_id: "failure-item".to_string(),
                workspace: "ws".to_string(),
                reason: "timeout".to_string(),
                title: None,
            },
            HitlItemSummary {
                work_id: "spec-item".to_string(),
                workspace: "ws".to_string(),
                reason: "evaluate_failure".to_string(),
                title: None,
            },
        ];
        let mut output = Vec::new();
        write_hitl_list(&mut output, &items).unwrap();
        let out = String::from_utf8(output).unwrap();

        // Spec Conflict should appear before Failure, which should appear before Other.
        let spec_pos = out.find("Spec Conflict").unwrap();
        let failure_pos = out.find("Failure (timeout)").unwrap();
        let other_pos = out.find("Other").unwrap();
        assert!(
            spec_pos < failure_pos,
            "spec-conflict should appear before failure"
        );
        assert!(
            failure_pos < other_pos,
            "failure should appear before other"
        );
    }
}
