//! Status display for `belt status` and `belt spec status`.
//!
//! Supports `text`, `json`, and `rich` output formats.
//! The `rich` format uses crossterm colours, box-drawing tables, and a
//! progress bar to visualise queue-phase distribution in the terminal.

use std::io::{self, Write};

use belt_core::phase::QueuePhase;
use belt_infra::db::{Database, RuntimeStats};
use crossterm::style::{self, Stylize};
use crossterm::terminal;
use serde::Serialize;

use crate::dashboard;

/// Minimum box width for rich output (characters inside borders).
const MIN_BOX_WIDTH: usize = 38;

/// Maximum box width for rich output.
const MAX_BOX_WIDTH: usize = 100;

/// Query the terminal width, falling back to 80 columns when unavailable.
fn terminal_width() -> usize {
    terminal::size().map(|(w, _)| w as usize).unwrap_or(80)
}

/// Compute the inner box width (content area between border characters).
///
/// Leaves a 2-column margin on each side of the terminal and clamps the
/// result between [`MIN_BOX_WIDTH`] and [`MAX_BOX_WIDTH`].
fn box_content_width() -> usize {
    let tw = terminal_width();
    // outer width = tw - 4 (2 margin each side), inner = outer - 2 (borders)
    let inner = tw.saturating_sub(6);
    inner.clamp(MIN_BOX_WIDTH, MAX_BOX_WIDTH)
}

/// System status summary returned by `belt status`.
#[derive(Debug, Serialize)]
pub struct SystemStatus {
    pub total_items: u32,
    pub hitl_count: u32,
    pub phase_counts: Vec<PhaseCount>,
    pub running_items: Vec<ItemSummary>,
    pub recent_events: Vec<EventSummary>,
    pub runtime_stats: Option<RuntimeStats>,
    /// Per-workspace item breakdown for the rich status table.
    pub workspace_summary: Vec<WorkspaceSummary>,
    /// Items currently in the failed phase.
    pub error_items: Vec<ItemSummary>,
    /// Items currently awaiting human intervention.
    pub hitl_items: Vec<ItemSummary>,
}

/// Per-workspace item summary for the system status table.
#[derive(Debug, Serialize)]
pub struct WorkspaceSummary {
    pub workspace: String,
    pub total: u32,
    pub phase_counts: Vec<PhaseCount>,
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

/// Token usage summary for a workspace.
#[derive(Debug, Serialize)]
pub struct TokenUsageSummary {
    /// Total input tokens consumed.
    pub total_input: u64,
    /// Total output tokens produced.
    pub total_output: u64,
    /// Grand total of input + output tokens.
    pub total_tokens: u64,
    /// Number of runtime invocations.
    pub executions: u64,
    /// Per-model breakdown: (model_name, input, output, total, count).
    pub by_model: Vec<ModelTokenSummary>,
}

/// Per-model token usage breakdown.
#[derive(Debug, Serialize)]
pub struct ModelTokenSummary {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub executions: u64,
}

/// Workspace spec status.
#[derive(Debug, Serialize)]
pub struct SpecStatus {
    pub workspace: String,
    pub config_path: String,
    pub item_count: u32,
    pub phase_counts: Vec<PhaseCount>,
    /// Optional token usage summary, populated when rich format is requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsageSummary>,
}

/// Gather system status from the database.
pub fn gather_status(db: &Database) -> anyhow::Result<SystemStatus> {
    let phase_counts_raw = db.count_items_by_phase()?;
    let total_items: u32 = phase_counts_raw.iter().map(|(_, c)| *c).sum();
    let hitl_count = phase_counts_raw
        .iter()
        .find(|(p, _)| p == "hitl")
        .map(|(_, c)| *c)
        .unwrap_or(0);

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
            item_id: e.work_id,
            from_state: e.from_phase.unwrap_or_default(),
            to_state: e.phase.unwrap_or_default(),
            event_type: e.event_type,
            timestamp: e.created_at,
        })
        .collect();

    let runtime_stats = db.get_runtime_stats().ok();

    // Per-workspace breakdown
    let workspaces = db.list_workspaces().unwrap_or_default();
    let mut workspace_summary = Vec::new();
    for (ws_name, _config, _created) in &workspaces {
        let ws_items = db.list_items(None, Some(ws_name)).unwrap_or_default();
        if ws_items.is_empty() {
            continue;
        }
        let ws_total = ws_items.len() as u32;
        let mut counts = std::collections::HashMap::<String, u32>::new();
        for item in &ws_items {
            *counts.entry(item.phase.as_str().to_string()).or_insert(0) += 1;
        }
        let mut ws_phases: Vec<PhaseCount> = counts
            .into_iter()
            .map(|(phase, count)| PhaseCount { phase, count })
            .collect();
        ws_phases.sort_by(|a, b| a.phase.cmp(&b.phase));
        workspace_summary.push(WorkspaceSummary {
            workspace: ws_name.clone(),
            total: ws_total,
            phase_counts: ws_phases,
        });
    }

    // Error items (failed phase)
    let failed = db.list_items(Some(QueuePhase::Failed), None)?;
    let error_items = failed
        .into_iter()
        .map(|item| ItemSummary {
            work_id: item.work_id,
            workspace: item.workspace_id,
            state: item.state,
            phase: item.phase.as_str().to_string(),
            updated_at: item.updated_at,
        })
        .collect();

    // HITL items
    let hitl = db.list_items(Some(QueuePhase::Hitl), None)?;
    let hitl_items = hitl
        .into_iter()
        .map(|item| ItemSummary {
            work_id: item.work_id,
            workspace: item.workspace_id,
            state: item.state,
            phase: item.phase.as_str().to_string(),
            updated_at: item.updated_at,
        })
        .collect();

    Ok(SystemStatus {
        total_items,
        hitl_count,
        phase_counts,
        running_items,
        recent_events,
        runtime_stats,
        workspace_summary,
        error_items,
        hitl_items,
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

    // Gather token usage for the workspace.
    let token_usage = gather_workspace_token_usage(db, workspace);

    Ok(SpecStatus {
        workspace: name,
        config_path,
        item_count,
        phase_counts,
        token_usage,
    })
}

/// Aggregate token usage rows into a summary for a workspace.
fn gather_workspace_token_usage(db: &Database, workspace: &str) -> Option<TokenUsageSummary> {
    let rows = db.get_token_usage_by_workspace(workspace).ok()?;
    if rows.is_empty() {
        return None;
    }

    let mut total_input: u64 = 0;
    let mut total_output: u64 = 0;
    let mut model_map = std::collections::HashMap::<String, (u64, u64, u64)>::new();

    for row in &rows {
        total_input += row.input_tokens;
        total_output += row.output_tokens;
        let entry = model_map.entry(row.model.clone()).or_insert((0, 0, 0));
        entry.0 += row.input_tokens;
        entry.1 += row.output_tokens;
        entry.2 += 1;
    }

    let mut by_model: Vec<ModelTokenSummary> = model_map
        .into_iter()
        .map(|(model, (inp, out, cnt))| ModelTokenSummary {
            model,
            input_tokens: inp,
            output_tokens: out,
            total_tokens: inp + out,
            executions: cnt,
        })
        .collect();
    by_model.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));

    Some(TokenUsageSummary {
        total_input,
        total_output,
        total_tokens: total_input + total_output,
        executions: rows.len() as u64,
        by_model,
    })
}

/// Print system status in the requested format.
///
/// When `daemon_running` is `Some`, the daemon status indicator is included
/// in the rich header box.  Pass `None` to omit it (useful in tests).
pub fn print_status(
    status: &SystemStatus,
    format: &str,
    daemon_running: Option<bool>,
) -> anyhow::Result<()> {
    match format {
        "json" => {
            println!("{}", serde_json::to_string_pretty(status)?);
        }
        "rich" => {
            print_rich_status(status, daemon_running);
            if let Some(ref s) = status.runtime_stats {
                print_rich_runtime(s);
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

#[cfg(test)]
mod tests {
    use belt_core::phase::QueuePhase;
    use belt_core::queue::QueueItem;
    use belt_infra::db::{Database, RuntimeStats, TransitionEvent};

    use super::*;

    /// Lightweight status output for JSON serialization.
    #[derive(Debug, Serialize)]
    pub struct StatusOutput<'a> {
        pub status: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub runtime_stats: Option<RuntimeStats>,
    }

    // ---- test helpers ----

    fn test_db() -> Database {
        Database::open_in_memory().expect("in-memory DB should open")
    }

    fn make_item(
        work_id: &str,
        source_id: &str,
        workspace_id: &str,
        phase: QueuePhase,
    ) -> QueueItem {
        let mut item = QueueItem::new(
            work_id.to_string(),
            source_id.to_string(),
            workspace_id.to_string(),
            "implement".to_string(),
        );
        item.phase = phase;
        item
    }

    // ---- StatusOutput serialization ----

    /// Build a `RuntimeStats` value with predictable fields for assertions.
    fn sample_stats() -> RuntimeStats {
        use std::collections::HashMap;
        RuntimeStats {
            total_tokens_input: 1_000,
            total_tokens_output: 500,
            total_tokens: 1_500,
            executions: 7,
            avg_duration_ms: Some(250.0),
            by_model: HashMap::new(),
        }
    }

    #[test]
    fn status_output_json_ok_without_stats() {
        let output = StatusOutput {
            status: "ok",
            runtime_stats: None,
        };
        let json = serde_json::to_string(&output).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["status"], "ok");
        assert!(v["runtime_stats"].is_null());
    }

    #[test]
    fn status_output_json_ok_with_stats() {
        let output = StatusOutput {
            status: "ok",
            runtime_stats: Some(sample_stats()),
        };
        let json = serde_json::to_string(&output).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["status"], "ok");
        let stats = &v["runtime_stats"];
        assert!(!stats.is_null());
        assert_eq!(stats["total_tokens"], 1_500);
        assert_eq!(stats["executions"], 7);
        assert_eq!(stats["total_tokens_input"], 1_000);
        assert_eq!(stats["total_tokens_output"], 500);
    }

    #[test]
    fn status_output_always_reports_ok() {
        // The status field must always be the literal string "ok".
        let output = StatusOutput {
            status: "ok",
            runtime_stats: None,
        };
        assert_eq!(output.status, "ok");
    }

    // ---- open_db error path ----

    #[test]
    fn open_db_invalid_path_returns_error() {
        // Providing a path inside a non-existent directory tree should fail.
        let result = belt_infra::db::Database::open("/nonexistent/dir/belt.db");
        assert!(result.is_err());
    }

    // ---- gather_status ----

    #[test]
    fn gather_status_empty_db_returns_zero_totals() {
        let db = test_db();
        let status = gather_status(&db).unwrap();

        assert_eq!(status.total_items, 0);
        assert_eq!(status.hitl_count, 0);
        assert!(status.phase_counts.is_empty());
        assert!(status.running_items.is_empty());
        assert!(status.recent_events.is_empty());
        assert!(status.workspace_summary.is_empty());
        assert!(status.error_items.is_empty());
        assert!(status.hitl_items.is_empty());
    }

    #[test]
    fn gather_status_counts_items_across_phases() {
        let db = test_db();

        let pending = make_item("w1:implement", "w1", "ws-a", QueuePhase::Pending);
        let running1 = make_item("w2:implement", "w2", "ws-a", QueuePhase::Running);
        let running2 = make_item("w3:implement", "w3", "ws-b", QueuePhase::Running);
        let done = make_item("w4:implement", "w4", "ws-a", QueuePhase::Done);

        db.insert_item(&pending).unwrap();
        db.insert_item(&running1).unwrap();
        db.insert_item(&running2).unwrap();
        db.insert_item(&done).unwrap();

        let status = gather_status(&db).unwrap();

        assert_eq!(status.total_items, 4);

        let phase_map: std::collections::HashMap<&str, u32> = status
            .phase_counts
            .iter()
            .map(|pc| (pc.phase.as_str(), pc.count))
            .collect();
        assert_eq!(phase_map.get("pending").copied(), Some(1));
        assert_eq!(phase_map.get("running").copied(), Some(2));
        assert_eq!(phase_map.get("done").copied(), Some(1));
    }

    #[test]
    fn gather_status_running_items_populated() {
        let db = test_db();

        let running = make_item("run1:implement", "run1", "ws-x", QueuePhase::Running);
        let pending = make_item("pend1:implement", "pend1", "ws-x", QueuePhase::Pending);

        db.insert_item(&running).unwrap();
        db.insert_item(&pending).unwrap();

        let status = gather_status(&db).unwrap();

        assert_eq!(status.running_items.len(), 1);
        assert_eq!(status.running_items[0].work_id, "run1:implement");
        assert_eq!(status.running_items[0].workspace, "ws-x");
        assert_eq!(status.running_items[0].phase, "running");
    }

    #[test]
    fn gather_status_recent_events_populated() {
        let db = test_db();

        let ev = TransitionEvent {
            id: "ev-1".to_string(),
            work_id: "w1:implement".to_string(),
            source_id: "github:org/repo#1".to_string(),
            event_type: "phase_enter".to_string(),
            phase: Some("running".to_string()),
            from_phase: Some("pending".to_string()),
            detail: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        db.insert_transition_event(&ev).unwrap();

        let status = gather_status(&db).unwrap();

        assert_eq!(status.recent_events.len(), 1);
        assert_eq!(status.recent_events[0].item_id, "w1:implement");
        assert_eq!(status.recent_events[0].from_state, "pending");
        assert_eq!(status.recent_events[0].to_state, "running");
        assert_eq!(status.recent_events[0].event_type, "phase_enter");
    }

    #[test]
    fn gather_status_recent_events_capped_at_ten() {
        let db = test_db();

        for i in 0..15u32 {
            let ev = TransitionEvent {
                id: format!("ev-{i}"),
                work_id: format!("w{i}:implement"),
                source_id: format!("github:org/repo#{i}"),
                event_type: "phase_enter".to_string(),
                phase: Some("running".to_string()),
                from_phase: Some("pending".to_string()),
                detail: None,
                created_at: chrono::Utc::now().to_rfc3339(),
            };
            db.insert_transition_event(&ev).unwrap();
        }

        let status = gather_status(&db).unwrap();

        assert!(
            status.recent_events.len() <= 10,
            "expected at most 10 events, got {}",
            status.recent_events.len()
        );
    }

    #[test]
    fn gather_status_hitl_count_populated() {
        let db = test_db();

        let pending = make_item("w1:implement", "w1", "ws-a", QueuePhase::Pending);
        let hitl1 = make_item("w2:implement", "w2", "ws-a", QueuePhase::Hitl);
        let hitl2 = make_item("w3:implement", "w3", "ws-b", QueuePhase::Hitl);

        db.insert_item(&pending).unwrap();
        db.insert_item(&hitl1).unwrap();
        db.insert_item(&hitl2).unwrap();

        let status = gather_status(&db).unwrap();

        assert_eq!(status.total_items, 3);
        assert_eq!(status.hitl_count, 2);
    }

    #[test]
    fn gather_status_error_and_hitl_items_populated() {
        let db = test_db();
        db.add_workspace("ws-a", "/a.yaml").unwrap();

        let failed = make_item("f1:implement", "f1", "ws-a", QueuePhase::Failed);
        let hitl = make_item("h1:implement", "h1", "ws-a", QueuePhase::Hitl);
        let pending = make_item("p1:implement", "p1", "ws-a", QueuePhase::Pending);
        db.insert_item(&failed).unwrap();
        db.insert_item(&hitl).unwrap();
        db.insert_item(&pending).unwrap();

        let status = gather_status(&db).unwrap();

        assert_eq!(status.error_items.len(), 1);
        assert_eq!(status.error_items[0].work_id, "f1:implement");
        assert_eq!(status.hitl_items.len(), 1);
        assert_eq!(status.hitl_items[0].work_id, "h1:implement");
        assert_eq!(status.workspace_summary.len(), 1);
        assert_eq!(status.workspace_summary[0].workspace, "ws-a");
        assert_eq!(status.workspace_summary[0].total, 3);
    }

    #[test]
    fn gather_status_hitl_count_zero_when_none() {
        let db = test_db();

        let pending = make_item("w1:implement", "w1", "ws-a", QueuePhase::Pending);
        let running = make_item("w2:implement", "w2", "ws-a", QueuePhase::Running);
        db.insert_item(&pending).unwrap();
        db.insert_item(&running).unwrap();

        let status = gather_status(&db).unwrap();

        assert_eq!(status.hitl_count, 0);
    }

    // ---- gather_spec_status ----

    #[test]
    fn gather_spec_status_workspace_not_found_returns_error() {
        let db = test_db();
        let result = gather_spec_status(&db, "no-such-workspace");
        assert!(result.is_err());
    }

    #[test]
    fn gather_spec_status_empty_workspace() {
        let db = test_db();
        db.add_workspace("ws-empty", "/path/to/workspace.yaml")
            .unwrap();

        let spec_status = gather_spec_status(&db, "ws-empty").unwrap();

        assert_eq!(spec_status.workspace, "ws-empty");
        assert_eq!(spec_status.config_path, "/path/to/workspace.yaml");
        assert_eq!(spec_status.item_count, 0);
        assert!(spec_status.phase_counts.is_empty());
    }

    #[test]
    fn gather_spec_status_counts_items_in_workspace() {
        let db = test_db();
        db.add_workspace("ws-main", "/path/to/ws.yaml").unwrap();
        db.add_workspace("ws-other", "/other/ws.yaml").unwrap();

        let item1 = make_item("a1:implement", "a1", "ws-main", QueuePhase::Pending);
        let item2 = make_item("a2:implement", "a2", "ws-main", QueuePhase::Running);
        let item3 = make_item("a3:implement", "a3", "ws-main", QueuePhase::Done);
        let other = make_item("b1:implement", "b1", "ws-other", QueuePhase::Pending);

        db.insert_item(&item1).unwrap();
        db.insert_item(&item2).unwrap();
        db.insert_item(&item3).unwrap();
        db.insert_item(&other).unwrap();

        let spec_status = gather_spec_status(&db, "ws-main").unwrap();

        assert_eq!(spec_status.item_count, 3);

        let phase_map: std::collections::HashMap<&str, u32> = spec_status
            .phase_counts
            .iter()
            .map(|pc| (pc.phase.as_str(), pc.count))
            .collect();
        assert_eq!(phase_map.get("pending").copied(), Some(1));
        assert_eq!(phase_map.get("running").copied(), Some(1));
        assert_eq!(phase_map.get("done").copied(), Some(1));
    }

    #[test]
    fn gather_spec_status_phase_counts_sorted_alphabetically() {
        let db = test_db();
        db.add_workspace("ws-sort", "/sort/ws.yaml").unwrap();

        let items = vec![
            make_item("s1:impl", "s1", "ws-sort", QueuePhase::Running),
            make_item("s2:impl", "s2", "ws-sort", QueuePhase::Done),
            make_item("s3:impl", "s3", "ws-sort", QueuePhase::Failed),
        ];
        for item in &items {
            db.insert_item(item).unwrap();
        }

        let spec_status = gather_spec_status(&db, "ws-sort").unwrap();

        let phases: Vec<&str> = spec_status
            .phase_counts
            .iter()
            .map(|pc| pc.phase.as_str())
            .collect();
        let mut sorted = phases.clone();
        sorted.sort_unstable();
        assert_eq!(phases, sorted, "phase_counts must be sorted alphabetically");
    }

    #[test]
    fn gather_spec_status_excludes_other_workspace_items() {
        let db = test_db();
        db.add_workspace("ws-a", "/a.yaml").unwrap();
        db.add_workspace("ws-b", "/b.yaml").unwrap();

        let item_a = make_item("ia:impl", "ia", "ws-a", QueuePhase::Pending);
        let item_b = make_item("ib:impl", "ib", "ws-b", QueuePhase::Running);
        db.insert_item(&item_a).unwrap();
        db.insert_item(&item_b).unwrap();

        let spec_status = gather_spec_status(&db, "ws-a").unwrap();

        assert_eq!(spec_status.item_count, 1);
        assert_eq!(spec_status.phase_counts.len(), 1);
        assert_eq!(spec_status.phase_counts[0].phase, "pending");
    }

    // ---- token usage in spec status ----

    #[test]
    fn gather_spec_status_no_token_usage_returns_none() {
        let db = test_db();
        db.add_workspace("ws-empty", "/e.yaml").unwrap();

        let spec_status = gather_spec_status(&db, "ws-empty").unwrap();
        assert!(spec_status.token_usage.is_none());
    }

    #[test]
    fn gather_spec_status_with_token_usage() {
        use belt_core::runtime::TokenUsage;

        let db = test_db();
        db.add_workspace("ws-tok", "/tok.yaml").unwrap();

        let usage = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cache_read_tokens: None,
            cache_write_tokens: None,
        };
        db.record_token_usage("w1:impl", "ws-tok", "claude", "sonnet", &usage, Some(200))
            .unwrap();
        db.record_token_usage("w2:impl", "ws-tok", "claude", "haiku", &usage, None)
            .unwrap();

        let spec_status = gather_spec_status(&db, "ws-tok").unwrap();
        let tu = spec_status.token_usage.as_ref().unwrap();

        assert_eq!(tu.total_input, 2_000);
        assert_eq!(tu.total_output, 1_000);
        assert_eq!(tu.total_tokens, 3_000);
        assert_eq!(tu.executions, 2);
        assert_eq!(tu.by_model.len(), 2);
        // by_model should be sorted by total_tokens descending (equal here)
        for m in &tu.by_model {
            assert_eq!(m.total_tokens, 1_500);
            assert_eq!(m.executions, 1);
        }
    }

    // ---- rich format helpers ----

    #[test]
    fn to_crossterm_color_maps_known_phases() {
        use ratatui::style::Color as RC;

        assert_eq!(
            super::to_crossterm_color(RC::Gray),
            crossterm::style::Color::Grey,
        );
        assert_eq!(
            super::to_crossterm_color(RC::Red),
            crossterm::style::Color::Red,
        );
        assert_eq!(
            super::to_crossterm_color(RC::Green),
            crossterm::style::Color::Green,
        );
        assert_eq!(
            super::to_crossterm_color(RC::Yellow),
            crossterm::style::Color::Yellow,
        );
    }

    #[test]
    fn to_crossterm_color_unknown_falls_back_to_white() {
        assert_eq!(
            super::to_crossterm_color(ratatui::style::Color::Magenta),
            crossterm::style::Color::White,
        );
    }

    #[test]
    fn print_status_rich_does_not_panic() {
        let status = SystemStatus {
            total_items: 5,
            hitl_count: 0,
            phase_counts: vec![
                PhaseCount {
                    phase: "pending".to_string(),
                    count: 2,
                },
                PhaseCount {
                    phase: "running".to_string(),
                    count: 1,
                },
                PhaseCount {
                    phase: "done".to_string(),
                    count: 2,
                },
            ],
            running_items: vec![ItemSummary {
                work_id: "w1:impl".to_string(),
                workspace: "ws-a".to_string(),
                state: "running".to_string(),
                phase: "running".to_string(),
                updated_at: "2026-03-25T00:00:00Z".to_string(),
            }],
            recent_events: vec![EventSummary {
                item_id: "w1:impl".to_string(),
                from_state: "pending".to_string(),
                to_state: "running".to_string(),
                event_type: "phase_change".to_string(),
                timestamp: "2026-03-25T00:00:00Z".to_string(),
            }],
            runtime_stats: None,
            workspace_summary: vec![],
            error_items: vec![],
            hitl_items: vec![],
        };
        // Should not panic
        super::print_rich_status(&status, None);
    }

    // ---- print_rich_spec_status output ----

    #[test]
    fn print_spec_status_rich_does_not_panic() {
        let status = SpecStatus {
            workspace: "test-ws".to_string(),
            config_path: "/test.yaml".to_string(),
            item_count: 5,
            phase_counts: vec![
                PhaseCount {
                    phase: "done".to_string(),
                    count: 3,
                },
                PhaseCount {
                    phase: "running".to_string(),
                    count: 2,
                },
            ],
            token_usage: Some(TokenUsageSummary {
                total_input: 10_000,
                total_output: 5_000,
                total_tokens: 15_000,
                executions: 10,
                by_model: vec![ModelTokenSummary {
                    model: "sonnet".to_string(),
                    input_tokens: 10_000,
                    output_tokens: 5_000,
                    total_tokens: 15_000,
                    executions: 10,
                }],
            }),
        };
        // Must not panic.
        print_spec_status(&status, "rich").unwrap();
    }

    #[test]
    fn print_spec_status_rich_no_token_usage() {
        let status = SpecStatus {
            workspace: "ws".to_string(),
            config_path: "/ws.yaml".to_string(),
            item_count: 0,
            phase_counts: vec![],
            token_usage: None,
        };
        print_spec_status(&status, "rich").unwrap();
    }

    #[test]
    fn print_spec_status_rich_multi_model_aggregation() {
        let status = SpecStatus {
            workspace: "ws-multi".to_string(),
            config_path: "/multi.yaml".to_string(),
            item_count: 3,
            phase_counts: vec![PhaseCount {
                phase: "running".to_string(),
                count: 3,
            }],
            token_usage: Some(TokenUsageSummary {
                total_input: 15_000,
                total_output: 7_500,
                total_tokens: 22_500,
                executions: 20,
                by_model: vec![
                    ModelTokenSummary {
                        model: "sonnet".to_string(),
                        input_tokens: 10_000,
                        output_tokens: 5_000,
                        total_tokens: 15_000,
                        executions: 12,
                    },
                    ModelTokenSummary {
                        model: "haiku".to_string(),
                        input_tokens: 5_000,
                        output_tokens: 2_500,
                        total_tokens: 7_500,
                        executions: 8,
                    },
                ],
            }),
        };
        // Must not panic; exercises the aggregation totals row.
        print_spec_status(&status, "rich").unwrap();
    }

    // ---- render_progress_bar ----

    #[test]
    fn render_progress_bar_zero_percent() {
        let bar = super::render_progress_bar(0, 10);
        assert_eq!(bar, "[..........]");
    }

    #[test]
    fn render_progress_bar_fifty_percent() {
        let bar = super::render_progress_bar(50, 10);
        assert_eq!(bar, "[#####.....]");
    }

    #[test]
    fn render_progress_bar_hundred_percent() {
        let bar = super::render_progress_bar(100, 10);
        assert_eq!(bar, "[##########]");
    }

    #[test]
    fn render_progress_bar_over_hundred_clamped() {
        let bar = super::render_progress_bar(150, 10);
        assert_eq!(bar, "[##########]");
    }

    // ---- fmt_num ----

    #[test]
    fn fmt_num_small() {
        assert_eq!(super::fmt_num(0), "0");
        assert_eq!(super::fmt_num(999), "999");
    }

    #[test]
    fn fmt_num_thousands() {
        assert_eq!(super::fmt_num(1_000), "1,000");
        assert_eq!(super::fmt_num(1_234_567), "1,234,567");
    }

    #[test]
    fn print_status_rich_format_dispatches_correctly() {
        let status = SystemStatus {
            total_items: 0,
            hitl_count: 0,
            phase_counts: vec![],
            running_items: vec![],
            recent_events: vec![],
            runtime_stats: None,
            workspace_summary: vec![],
            error_items: vec![],
            hitl_items: vec![],
        };
        // Calling print_status with "rich" should not panic.
        super::print_status(&status, "rich", None).unwrap();
    }

    // ---- formatter test helpers ----

    fn sample_system_status() -> SystemStatus {
        SystemStatus {
            total_items: 3,
            hitl_count: 0,
            phase_counts: vec![
                PhaseCount {
                    phase: "pending".to_string(),
                    count: 1,
                },
                PhaseCount {
                    phase: "running".to_string(),
                    count: 2,
                },
            ],
            running_items: vec![ItemSummary {
                work_id: "w1:implement".to_string(),
                workspace: "ws-a".to_string(),
                state: "active".to_string(),
                phase: "running".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
            }],
            recent_events: vec![EventSummary {
                item_id: "w1:implement".to_string(),
                from_state: "pending".to_string(),
                to_state: "running".to_string(),
                event_type: "phase_change".to_string(),
                timestamp: "2026-01-01T00:00:00Z".to_string(),
            }],
            runtime_stats: Some(sample_stats()),
            workspace_summary: vec![WorkspaceSummary {
                workspace: "ws-a".to_string(),
                total: 3,
                phase_counts: vec![
                    PhaseCount {
                        phase: "pending".to_string(),
                        count: 1,
                    },
                    PhaseCount {
                        phase: "running".to_string(),
                        count: 2,
                    },
                ],
            }],
            error_items: vec![],
            hitl_items: vec![],
        }
    }

    fn empty_system_status() -> SystemStatus {
        SystemStatus {
            total_items: 0,
            hitl_count: 0,
            phase_counts: vec![],
            running_items: vec![],
            recent_events: vec![],
            runtime_stats: None,
            workspace_summary: vec![],
            error_items: vec![],
            hitl_items: vec![],
        }
    }

    fn sample_spec_status() -> SpecStatus {
        SpecStatus {
            workspace: "ws-test".to_string(),
            config_path: "/path/to/config.yaml".to_string(),
            item_count: 5,
            phase_counts: vec![
                PhaseCount {
                    phase: "done".to_string(),
                    count: 3,
                },
                PhaseCount {
                    phase: "pending".to_string(),
                    count: 2,
                },
            ],
            token_usage: None,
        }
    }

    fn empty_spec_status() -> SpecStatus {
        SpecStatus {
            workspace: "ws-empty".to_string(),
            config_path: "/empty/config.yaml".to_string(),
            item_count: 0,
            phase_counts: vec![],
            token_usage: None,
        }
    }

    // ---- print_status (format dispatcher) ----

    #[test]
    fn print_status_json_format_returns_ok() {
        let status = sample_system_status();
        let result = print_status(&status, "json", None);
        assert!(result.is_ok());
    }

    #[test]
    fn print_status_json_contains_expected_fields() {
        let status = sample_system_status();
        let json = serde_json::to_string_pretty(&status).unwrap();
        assert!(json.contains("\"total_items\": 3"));
        assert!(json.contains("\"phase_counts\""));
        assert!(json.contains("\"running_items\""));
        assert!(json.contains("\"recent_events\""));
        assert!(json.contains("\"runtime_stats\""));
        assert!(json.contains("\"pending\""));
        assert!(json.contains("\"running\""));
    }

    #[test]
    fn print_status_text_format_returns_ok() {
        let status = sample_system_status();
        let result = print_status(&status, "text", None);
        assert!(result.is_ok());
    }

    #[test]
    fn print_status_rich_format_returns_ok() {
        let status = sample_system_status();
        let result = print_status(&status, "rich", None);
        assert!(result.is_ok());
    }

    #[test]
    fn print_status_unknown_format_falls_back_to_text() {
        let status = sample_system_status();
        // Unknown format should not error — falls back to text.
        let result = print_status(&status, "unknown", None);
        assert!(result.is_ok());
    }

    // ---- print_spec_status (format dispatcher) ----

    #[test]
    fn print_spec_status_json_format_returns_ok() {
        let status = sample_spec_status();
        let result = print_spec_status(&status, "json");
        assert!(result.is_ok());
    }

    #[test]
    fn print_spec_status_json_contains_expected_fields() {
        let status = sample_spec_status();
        let json = serde_json::to_string_pretty(&status).unwrap();
        assert!(json.contains("\"workspace\": \"ws-test\""));
        assert!(json.contains("\"config_path\""));
        assert!(json.contains("\"item_count\": 5"));
        assert!(json.contains("\"phase_counts\""));
    }

    #[test]
    fn print_spec_status_text_format_returns_ok() {
        let status = sample_spec_status();
        let result = print_spec_status(&status, "text");
        assert!(result.is_ok());
    }

    #[test]
    fn print_spec_status_rich_format_returns_ok() {
        let status = sample_spec_status();
        let result = print_spec_status(&status, "rich");
        assert!(result.is_ok());
    }

    #[test]
    fn print_spec_status_unknown_format_falls_back_to_text() {
        let status = sample_spec_status();
        let result = print_spec_status(&status, "unknown");
        assert!(result.is_ok());
    }

    // ---- print_text_status ----

    #[test]
    fn print_text_status_with_data_does_not_panic() {
        let status = sample_system_status();
        // Exercises all branches: phase_counts non-empty, running_items non-empty,
        // recent_events non-empty, runtime_stats present.
        print_text_status(&status);
    }

    #[test]
    fn print_text_status_empty_does_not_panic() {
        let status = empty_system_status();
        // Exercises empty branches: no phases, no running items ("No items currently running."),
        // no events, no runtime stats.
        print_text_status(&status);
    }

    // ---- print_rich_status ----

    #[test]
    fn print_rich_status_with_data_does_not_panic() {
        let status = sample_system_status();
        print_rich_status(&status, None);
    }

    #[test]
    fn print_rich_status_empty_does_not_panic() {
        let status = empty_system_status();
        print_rich_status(&status, None);
    }

    #[test]
    fn print_rich_status_with_errors_and_hitl_does_not_panic() {
        let status = SystemStatus {
            total_items: 4,
            hitl_count: 1,
            phase_counts: vec![
                PhaseCount {
                    phase: "failed".to_string(),
                    count: 2,
                },
                PhaseCount {
                    phase: "hitl".to_string(),
                    count: 1,
                },
                PhaseCount {
                    phase: "running".to_string(),
                    count: 1,
                },
            ],
            running_items: vec![],
            recent_events: vec![],
            runtime_stats: None,
            workspace_summary: vec![WorkspaceSummary {
                workspace: "ws-err".to_string(),
                total: 4,
                phase_counts: vec![
                    PhaseCount {
                        phase: "failed".to_string(),
                        count: 2,
                    },
                    PhaseCount {
                        phase: "hitl".to_string(),
                        count: 1,
                    },
                    PhaseCount {
                        phase: "running".to_string(),
                        count: 1,
                    },
                ],
            }],
            error_items: vec![
                ItemSummary {
                    work_id: "err1:impl".to_string(),
                    workspace: "ws-err".to_string(),
                    state: "failed".to_string(),
                    phase: "failed".to_string(),
                    updated_at: "2026-03-25T10:00:00Z".to_string(),
                },
                ItemSummary {
                    work_id: "err2:impl".to_string(),
                    workspace: "ws-err".to_string(),
                    state: "failed".to_string(),
                    phase: "failed".to_string(),
                    updated_at: "2026-03-25T11:00:00Z".to_string(),
                },
            ],
            hitl_items: vec![ItemSummary {
                work_id: "hitl1:impl".to_string(),
                workspace: "ws-err".to_string(),
                state: "hitl".to_string(),
                phase: "hitl".to_string(),
                updated_at: "2026-03-25T12:00:00Z".to_string(),
            }],
        };
        // Exercises workspace summary, error items, and HITL items branches.
        print_rich_status(&status, None);
    }

    #[test]
    fn print_rich_status_with_daemon_running_does_not_panic() {
        let status = sample_system_status();
        print_rich_status(&status, Some(true));
    }

    #[test]
    fn print_rich_status_with_daemon_stopped_does_not_panic() {
        let status = empty_system_status();
        print_rich_status(&status, Some(false));
    }

    // ---- terminal width helpers ----

    #[test]
    fn box_content_width_within_bounds() {
        let w = super::box_content_width();
        assert!(
            w >= super::MIN_BOX_WIDTH,
            "box width {w} below minimum {}",
            super::MIN_BOX_WIDTH
        );
        assert!(
            w <= super::MAX_BOX_WIDTH,
            "box width {w} above maximum {}",
            super::MAX_BOX_WIDTH
        );
    }

    #[test]
    fn terminal_width_returns_positive() {
        let tw = super::terminal_width();
        assert!(tw > 0);
    }

    // ---- print_rich_runtime ----

    #[test]
    fn print_rich_runtime_does_not_panic() {
        let stats = sample_stats();
        super::print_rich_runtime(&stats);
    }

    #[test]
    fn print_rich_runtime_with_models_does_not_panic() {
        use belt_infra::db::ModelStats;
        use std::collections::HashMap;

        let mut by_model = HashMap::new();
        by_model.insert(
            "sonnet".to_string(),
            ModelStats {
                model: "sonnet".to_string(),
                input_tokens: 800,
                output_tokens: 400,
                total_tokens: 1_200,
                executions: 5,
                avg_duration_ms: Some(150.0),
            },
        );
        by_model.insert(
            "haiku".to_string(),
            ModelStats {
                model: "haiku".to_string(),
                input_tokens: 200,
                output_tokens: 100,
                total_tokens: 300,
                executions: 2,
                avg_duration_ms: None,
            },
        );
        let stats = RuntimeStats {
            total_tokens_input: 1_000,
            total_tokens_output: 500,
            total_tokens: 1_500,
            executions: 7,
            avg_duration_ms: Some(250.0),
            by_model,
        };
        super::print_rich_runtime(&stats);
    }

    // ---- print_text_spec_status ----

    #[test]
    fn print_text_spec_status_with_data_does_not_panic() {
        let status = sample_spec_status();
        print_text_spec_status(&status);
    }

    #[test]
    fn print_text_spec_status_empty_does_not_panic() {
        let status = empty_spec_status();
        // Exercises "No items in this workspace." branch.
        print_text_spec_status(&status);
    }

    // ---- print_rich_spec_status ----

    #[test]
    fn print_rich_spec_status_with_data_does_not_panic() {
        let status = sample_spec_status();
        print_rich_spec_status(&status);
    }

    #[test]
    fn print_rich_spec_status_empty_does_not_panic() {
        let status = empty_spec_status();
        print_rich_spec_status(&status);
    }

    // ---- WorkspaceSummary ----

    #[test]
    fn workspace_summary_serializes_to_json() {
        let ws = WorkspaceSummary {
            workspace: "my-ws".to_string(),
            total: 5,
            phase_counts: vec![
                PhaseCount {
                    phase: "pending".to_string(),
                    count: 2,
                },
                PhaseCount {
                    phase: "running".to_string(),
                    count: 3,
                },
            ],
        };
        let json = serde_json::to_string(&ws).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["workspace"], "my-ws");
        assert_eq!(v["total"], 5);
        assert!(v["phase_counts"].is_array());
        assert_eq!(v["phase_counts"].as_array().unwrap().len(), 2);
        assert_eq!(v["phase_counts"][0]["phase"], "pending");
        assert_eq!(v["phase_counts"][0]["count"], 2);
        assert_eq!(v["phase_counts"][1]["phase"], "running");
        assert_eq!(v["phase_counts"][1]["count"], 3);
    }

    #[test]
    fn workspace_summary_empty_phase_counts() {
        let ws = WorkspaceSummary {
            workspace: "empty-ws".to_string(),
            total: 0,
            phase_counts: vec![],
        };
        assert_eq!(ws.workspace, "empty-ws");
        assert_eq!(ws.total, 0);
        assert!(ws.phase_counts.is_empty());
    }

    // ---- per-workspace phase breakdown (gather_status) ----

    #[test]
    fn gather_status_workspace_summary_multiple_workspaces() {
        let db = test_db();
        db.add_workspace("ws-alpha", "/alpha.yaml").unwrap();
        db.add_workspace("ws-beta", "/beta.yaml").unwrap();

        // ws-alpha: 2 pending, 1 running
        let a1 = make_item("a1:implement", "a1", "ws-alpha", QueuePhase::Pending);
        let a2 = make_item("a2:implement", "a2", "ws-alpha", QueuePhase::Pending);
        let a3 = make_item("a3:implement", "a3", "ws-alpha", QueuePhase::Running);
        // ws-beta: 1 done, 1 failed
        let b1 = make_item("b1:implement", "b1", "ws-beta", QueuePhase::Done);
        let b2 = make_item("b2:implement", "b2", "ws-beta", QueuePhase::Failed);

        db.insert_item(&a1).unwrap();
        db.insert_item(&a2).unwrap();
        db.insert_item(&a3).unwrap();
        db.insert_item(&b1).unwrap();
        db.insert_item(&b2).unwrap();

        let status = gather_status(&db).unwrap();

        assert_eq!(status.workspace_summary.len(), 2);

        let alpha = status
            .workspace_summary
            .iter()
            .find(|ws| ws.workspace == "ws-alpha")
            .expect("ws-alpha should be present");
        assert_eq!(alpha.total, 3);
        let alpha_phases: std::collections::HashMap<&str, u32> = alpha
            .phase_counts
            .iter()
            .map(|pc| (pc.phase.as_str(), pc.count))
            .collect();
        assert_eq!(alpha_phases.get("pending").copied(), Some(2));
        assert_eq!(alpha_phases.get("running").copied(), Some(1));

        let beta = status
            .workspace_summary
            .iter()
            .find(|ws| ws.workspace == "ws-beta")
            .expect("ws-beta should be present");
        assert_eq!(beta.total, 2);
        let beta_phases: std::collections::HashMap<&str, u32> = beta
            .phase_counts
            .iter()
            .map(|pc| (pc.phase.as_str(), pc.count))
            .collect();
        assert_eq!(beta_phases.get("done").copied(), Some(1));
        assert_eq!(beta_phases.get("failed").copied(), Some(1));
    }

    #[test]
    fn gather_status_workspace_summary_skips_empty_workspaces() {
        let db = test_db();
        db.add_workspace("ws-active", "/active.yaml").unwrap();
        db.add_workspace("ws-empty", "/empty.yaml").unwrap();

        let item = make_item("x1:implement", "x1", "ws-active", QueuePhase::Pending);
        db.insert_item(&item).unwrap();

        let status = gather_status(&db).unwrap();

        assert_eq!(status.workspace_summary.len(), 1);
        assert_eq!(status.workspace_summary[0].workspace, "ws-active");
    }

    #[test]
    fn gather_status_workspace_summary_phase_counts_sorted() {
        let db = test_db();
        db.add_workspace("ws-sort", "/sort.yaml").unwrap();

        // Insert items in phases that would be out of alphabetical order
        let items = vec![
            make_item("s1:impl", "s1", "ws-sort", QueuePhase::Running),
            make_item("s2:impl", "s2", "ws-sort", QueuePhase::Done),
            make_item("s3:impl", "s3", "ws-sort", QueuePhase::Failed),
            make_item("s4:impl", "s4", "ws-sort", QueuePhase::Pending),
        ];
        for item in &items {
            db.insert_item(item).unwrap();
        }

        let status = gather_status(&db).unwrap();

        assert_eq!(status.workspace_summary.len(), 1);
        let ws = &status.workspace_summary[0];
        let phases: Vec<&str> = ws.phase_counts.iter().map(|pc| pc.phase.as_str()).collect();
        let mut sorted = phases.clone();
        sorted.sort_unstable();
        assert_eq!(
            phases, sorted,
            "workspace phase_counts must be sorted alphabetically"
        );
    }

    // ---- error_items detail verification ----

    #[test]
    fn gather_status_error_items_fields_verified() {
        let db = test_db();
        db.add_workspace("ws-err", "/err.yaml").unwrap();

        let f1 = make_item("fail1:implement", "fail1", "ws-err", QueuePhase::Failed);
        let f2 = make_item("fail2:implement", "fail2", "ws-err", QueuePhase::Failed);
        let ok = make_item("ok1:implement", "ok1", "ws-err", QueuePhase::Done);
        db.insert_item(&f1).unwrap();
        db.insert_item(&f2).unwrap();
        db.insert_item(&ok).unwrap();

        let status = gather_status(&db).unwrap();

        assert_eq!(status.error_items.len(), 2);
        for item in &status.error_items {
            assert_eq!(item.phase, "failed");
            assert_eq!(item.workspace, "ws-err");
            assert!(
                item.work_id.starts_with("fail"),
                "unexpected work_id: {}",
                item.work_id
            );
            assert!(!item.updated_at.is_empty());
        }
    }

    #[test]
    fn gather_status_error_items_empty_when_no_failures() {
        let db = test_db();
        db.add_workspace("ws-ok", "/ok.yaml").unwrap();

        let p = make_item("p1:implement", "p1", "ws-ok", QueuePhase::Pending);
        let d = make_item("d1:implement", "d1", "ws-ok", QueuePhase::Done);
        db.insert_item(&p).unwrap();
        db.insert_item(&d).unwrap();

        let status = gather_status(&db).unwrap();

        assert!(status.error_items.is_empty());
    }

    #[test]
    fn gather_status_error_items_across_workspaces() {
        let db = test_db();
        db.add_workspace("ws-a", "/a.yaml").unwrap();
        db.add_workspace("ws-b", "/b.yaml").unwrap();

        let fa = make_item("fa:implement", "fa", "ws-a", QueuePhase::Failed);
        let fb = make_item("fb:implement", "fb", "ws-b", QueuePhase::Failed);
        db.insert_item(&fa).unwrap();
        db.insert_item(&fb).unwrap();

        let status = gather_status(&db).unwrap();

        assert_eq!(status.error_items.len(), 2);
        let workspaces: std::collections::HashSet<&str> = status
            .error_items
            .iter()
            .map(|i| i.workspace.as_str())
            .collect();
        assert!(workspaces.contains("ws-a"));
        assert!(workspaces.contains("ws-b"));
    }

    // ---- hitl_items detail verification ----

    #[test]
    fn gather_status_hitl_items_fields_verified() {
        let db = test_db();
        db.add_workspace("ws-hitl", "/hitl.yaml").unwrap();

        let h1 = make_item("hitl1:implement", "hitl1", "ws-hitl", QueuePhase::Hitl);
        let h2 = make_item("hitl2:implement", "hitl2", "ws-hitl", QueuePhase::Hitl);
        let ok = make_item("ok1:implement", "ok1", "ws-hitl", QueuePhase::Running);
        db.insert_item(&h1).unwrap();
        db.insert_item(&h2).unwrap();
        db.insert_item(&ok).unwrap();

        let status = gather_status(&db).unwrap();

        assert_eq!(status.hitl_items.len(), 2);
        assert_eq!(status.hitl_count, 2);
        for item in &status.hitl_items {
            assert_eq!(item.phase, "hitl");
            assert_eq!(item.workspace, "ws-hitl");
            assert!(
                item.work_id.starts_with("hitl"),
                "unexpected work_id: {}",
                item.work_id
            );
            assert!(!item.updated_at.is_empty());
        }
    }

    #[test]
    fn gather_status_hitl_items_empty_when_no_hitl() {
        let db = test_db();
        db.add_workspace("ws-clean", "/clean.yaml").unwrap();

        let r = make_item("r1:implement", "r1", "ws-clean", QueuePhase::Running);
        let d = make_item("d1:implement", "d1", "ws-clean", QueuePhase::Done);
        db.insert_item(&r).unwrap();
        db.insert_item(&d).unwrap();

        let status = gather_status(&db).unwrap();

        assert!(status.hitl_items.is_empty());
        assert_eq!(status.hitl_count, 0);
    }

    #[test]
    fn gather_status_hitl_items_across_workspaces() {
        let db = test_db();
        db.add_workspace("ws-a", "/a.yaml").unwrap();
        db.add_workspace("ws-b", "/b.yaml").unwrap();

        let ha = make_item("ha:implement", "ha", "ws-a", QueuePhase::Hitl);
        let hb = make_item("hb:implement", "hb", "ws-b", QueuePhase::Hitl);
        db.insert_item(&ha).unwrap();
        db.insert_item(&hb).unwrap();

        let status = gather_status(&db).unwrap();

        assert_eq!(status.hitl_items.len(), 2);
        assert_eq!(status.hitl_count, 2);
        let workspaces: std::collections::HashSet<&str> = status
            .hitl_items
            .iter()
            .map(|i| i.workspace.as_str())
            .collect();
        assert!(workspaces.contains("ws-a"));
        assert!(workspaces.contains("ws-b"));
    }

    // ---- combined error + hitl + workspace breakdown ----

    #[test]
    fn gather_status_mixed_error_hitl_workspace_breakdown() {
        let db = test_db();
        db.add_workspace("ws-mix", "/mix.yaml").unwrap();

        let items = vec![
            make_item("p1:implement", "p1", "ws-mix", QueuePhase::Pending),
            make_item("r1:implement", "r1", "ws-mix", QueuePhase::Running),
            make_item("f1:implement", "f1", "ws-mix", QueuePhase::Failed),
            make_item("h1:implement", "h1", "ws-mix", QueuePhase::Hitl),
            make_item("d1:implement", "d1", "ws-mix", QueuePhase::Done),
        ];
        for item in &items {
            db.insert_item(item).unwrap();
        }

        let status = gather_status(&db).unwrap();

        assert_eq!(status.total_items, 5);
        assert_eq!(status.error_items.len(), 1);
        assert_eq!(status.error_items[0].work_id, "f1:implement");
        assert_eq!(status.hitl_items.len(), 1);
        assert_eq!(status.hitl_items[0].work_id, "h1:implement");
        assert_eq!(status.hitl_count, 1);

        // Workspace summary should contain all 5 items
        assert_eq!(status.workspace_summary.len(), 1);
        let ws = &status.workspace_summary[0];
        assert_eq!(ws.workspace, "ws-mix");
        assert_eq!(ws.total, 5);

        let phase_map: std::collections::HashMap<&str, u32> = ws
            .phase_counts
            .iter()
            .map(|pc| (pc.phase.as_str(), pc.count))
            .collect();
        assert_eq!(phase_map.get("pending").copied(), Some(1));
        assert_eq!(phase_map.get("running").copied(), Some(1));
        assert_eq!(phase_map.get("failed").copied(), Some(1));
        assert_eq!(phase_map.get("hitl").copied(), Some(1));
        assert_eq!(phase_map.get("done").copied(), Some(1));
    }
}

fn print_text_status(status: &SystemStatus) {
    println!("Belt System Status");
    println!("==================");
    println!("Total items: {}", status.total_items);
    if status.hitl_count > 0 {
        println!("\x1b[31mHITL: {}\x1b[0m", status.hitl_count);
    } else {
        println!("HITL: 0");
    }
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
    if status.hitl_count > 0 {
        println!();
        println!(
            "\x1b[33m\u{26a0}\u{fe0f} {} items require human intervention. Run 'belt claw' to review.\x1b[0m",
            status.hitl_count
        );
    }
}

/// Map a ratatui [`ratatui::style::Color`] to the corresponding
/// [`crossterm::style::Color`] for terminal output outside the TUI.
fn to_crossterm_color(c: ratatui::style::Color) -> crossterm::style::Color {
    match c {
        ratatui::style::Color::Gray => crossterm::style::Color::Grey,
        ratatui::style::Color::Blue => crossterm::style::Color::Blue,
        ratatui::style::Color::Green => crossterm::style::Color::Green,
        ratatui::style::Color::Cyan => crossterm::style::Color::Cyan,
        ratatui::style::Color::White => crossterm::style::Color::White,
        ratatui::style::Color::Yellow => crossterm::style::Color::Yellow,
        ratatui::style::Color::Red => crossterm::style::Color::Red,
        ratatui::style::Color::DarkGray => crossterm::style::Color::DarkGrey,
        _ => crossterm::style::Color::White,
    }
}

fn print_rich_status(status: &SystemStatus, daemon_running: Option<bool>) {
    let mut stdout = io::stdout();
    let w = box_content_width();

    // Header box
    let border_h = "\u{2500}".repeat(w);
    let _ = writeln!(stdout, "\u{250c}{border_h}\u{2510}");
    let title = format!("{:^width$}", "Belt System Status", width = w - 2);
    let _ = writeln!(stdout, "\u{2502} {} \u{2502}", title.bold());
    let _ = writeln!(stdout, "\u{251c}{border_h}\u{2524}");

    // Daemon status row (when provided)
    if let Some(running) = daemon_running {
        let (label, indicator) = if running {
            ("running", "\u{25cf} running".green())
        } else {
            ("stopped", "\u{25cb} stopped".red())
        };
        let pad = w - 2 - "Daemon: ".len() - label.len();
        let _ = writeln!(
            stdout,
            "\u{2502} Daemon: {indicator}{:pad$} \u{2502}",
            "",
            pad = pad,
        );
    }

    let items_str = format!("{}", status.total_items);
    let pad = w - 2 - "Total items: ".len() - items_str.len();
    let _ = writeln!(
        stdout,
        "\u{2502} Total items: {items_str}{:pad$} \u{2502}",
        "",
        pad = pad,
    );

    if status.hitl_count > 0 {
        let hitl_str = format!("{}", status.hitl_count);
        let hpad = w - 2 - "HITL items:  ".len() - hitl_str.len();
        let _ = writeln!(
            stdout,
            "\u{2502} HITL items:  {}{:hpad$} \u{2502}",
            hitl_str.yellow(),
            "",
            hpad = hpad,
        );
    }

    let _ = writeln!(stdout, "\u{2514}{border_h}\u{2518}");

    // Phase table with colours and progress bar
    if !status.phase_counts.is_empty() {
        let total = status.total_items.max(1);
        // Adaptive progress bar width: use remaining space after phase (16) + count (10) + separators
        let bar_width = (w.saturating_sub(32)).max(8);

        let _ = writeln!(stdout);
        let _ = writeln!(
            stdout,
            "  {} {:<14} {} {:<8} {} {}",
            "\u{2502}".dark_grey(),
            "Phase".bold().underlined(),
            "\u{2502}".dark_grey(),
            "Count".bold().underlined(),
            "\u{2502}".dark_grey(),
            "Progress".bold().underlined(),
        );
        let _ = writeln!(
            stdout,
            "  {}{}{}{}{}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{253c}".dark_grey(),
            "\u{2500}".repeat(16),
            "\u{253c}".dark_grey(),
            "\u{2500}".repeat(9),
            "\u{253c}".dark_grey(),
        );
        for pc in &status.phase_counts {
            let color = to_crossterm_color(dashboard::phase_color(&pc.phase));
            let phase_styled = style::style(format!("{:<14}", pc.phase)).with(color);
            let pct = (pc.count as f64 / total as f64 * 100.0) as u32;
            let bar = render_progress_bar(pct, bar_width);
            let _ = writeln!(
                stdout,
                "  {} {phase_styled} {} {:<8} {} {} {:>3}%",
                "\u{2502}".dark_grey(),
                "\u{2502}".dark_grey(),
                pc.count,
                "\u{2502}".dark_grey(),
                bar,
                pct,
            );
        }
    }

    // Per-workspace breakdown
    if !status.workspace_summary.is_empty() {
        let phases_col_width = w.saturating_sub(26).max(10);

        let _ = writeln!(stdout);
        let _ = writeln!(stdout, "  {}", "Per-Workspace Status".bold().cyan());
        let _ = writeln!(
            stdout,
            "  {}{}{}{}{}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{253c}".dark_grey(),
            "\u{2500}".repeat(16),
            "\u{253c}".dark_grey(),
            "\u{2500}".repeat(7),
            "\u{253c}".dark_grey(),
        );
        let _ = writeln!(
            stdout,
            "  {} {:<14} {} {:<5} {} {}",
            "\u{2502}".dark_grey(),
            "Workspace".bold().underlined(),
            "\u{2502}".dark_grey(),
            "Items".bold().underlined(),
            "\u{2502}".dark_grey(),
            "Phases".bold().underlined(),
        );
        let _ = writeln!(
            stdout,
            "  {}{}{}{}{}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{253c}".dark_grey(),
            "\u{2500}".repeat(16),
            "\u{253c}".dark_grey(),
            "\u{2500}".repeat(7),
            "\u{253c}".dark_grey(),
        );
        for ws in &status.workspace_summary {
            let phases_str: String = ws
                .phase_counts
                .iter()
                .map(|pc| format!("{}:{}", pc.phase, pc.count))
                .collect::<Vec<_>>()
                .join(", ");
            // Truncate phases string if it exceeds available width.
            let display_phases = if phases_str.len() > phases_col_width {
                format!("{}\u{2026}", &phases_str[..phases_col_width - 1])
            } else {
                phases_str
            };
            let _ = writeln!(
                stdout,
                "  {} {:<14} {} {:<5} {} {}",
                "\u{2502}".dark_grey(),
                ws.workspace,
                "\u{2502}".dark_grey(),
                ws.total,
                "\u{2502}".dark_grey(),
                display_phases.dark_grey(),
            );
        }
    }

    // Running items
    if !status.running_items.is_empty() {
        let _ = writeln!(stdout);
        let _ = writeln!(stdout, "  {}", "Running Items".bold().green());
        for item in &status.running_items {
            let _ = writeln!(
                stdout,
                "    {} {} {}",
                "\u{25cf}".green(),
                item.work_id,
                format!("({})", item.workspace).dark_grey(),
            );
        }
    }

    // Recent transitions
    if !status.recent_events.is_empty() {
        let _ = writeln!(stdout);
        let _ = writeln!(stdout, "  {}", "Recent Transitions".bold());
        for ev in &status.recent_events {
            let from_color = to_crossterm_color(dashboard::phase_color(&ev.from_state));
            let to_color = to_crossterm_color(dashboard::phase_color(&ev.to_state));
            let from = style::style(&ev.from_state).with(from_color);
            let to = style::style(&ev.to_state).with(to_color);
            let _ = writeln!(
                stdout,
                "    {from} \u{2192} {to}  {} {}",
                ev.item_id.as_str().dark_grey(),
                format!("[{}]", ev.event_type).dark_grey(),
            );
        }
    }

    // Error/HITL summary
    if !status.error_items.is_empty() {
        let _ = writeln!(stdout);
        let header = format!("Recent Errors ({} items)", status.error_items.len());
        let _ = writeln!(stdout, "  {}", header.bold().red());
        for item in &status.error_items {
            let _ = writeln!(
                stdout,
                "    {} {} {}",
                "\u{2716}".red(),
                item.work_id.as_str().red(),
                format!("({}) {}", item.workspace, item.updated_at).dark_grey(),
            );
        }
    }

    if !status.hitl_items.is_empty() {
        let _ = writeln!(stdout);
        let header = format!("HITL Pending ({} items)", status.hitl_items.len());
        let _ = writeln!(stdout, "  {}", header.bold().yellow());
        for item in &status.hitl_items {
            let _ = writeln!(
                stdout,
                "    {} {} {}",
                "\u{26a0}".yellow(),
                item.work_id.as_str().yellow(),
                format!("({}) {}", item.workspace, item.updated_at).dark_grey(),
            );
        }
        let _ = writeln!(stdout);
        let _ = writeln!(
            stdout,
            "  {}",
            format!(
                "{} items require human intervention. Run 'belt claw' to review.",
                status.hitl_items.len()
            )
            .yellow(),
        );
    } else if status.hitl_count > 0 {
        let _ = writeln!(stdout);
        let _ = writeln!(
            stdout,
            "  {}",
            format!(
                "{} items require human intervention. Run 'belt claw' to review.",
                status.hitl_count
            )
            .yellow(),
        );
    }
}

/// Render runtime statistics with crossterm colours for the rich format.
fn print_rich_runtime(stats: &RuntimeStats) {
    let mut stdout = io::stdout();

    let _ = writeln!(stdout);
    let _ = writeln!(stdout, "  {}", "Runtime Stats (last 24h)".bold().cyan());
    let _ = writeln!(
        stdout,
        "  {}",
        "\u{2500}".repeat(box_content_width().min(60)).dark_grey()
    );
    let _ = writeln!(
        stdout,
        "  Total tokens:  {} (in: {} / out: {})",
        fmt_num(stats.total_tokens).bold(),
        fmt_num(stats.total_tokens_input),
        fmt_num(stats.total_tokens_output),
    );
    let _ = writeln!(
        stdout,
        "  Executions:    {}",
        stats.executions.to_string().bold()
    );
    match stats.avg_duration_ms {
        Some(d) => {
            let _ = writeln!(stdout, "  Avg duration:  {:.0}ms", d);
        }
        None => {
            let _ = writeln!(stdout, "  Avg duration:  -");
        }
    }

    if !stats.by_model.is_empty() {
        let _ = writeln!(stdout);
        let _ = writeln!(
            stdout,
            "  {:<20} {:>10} {:>10} {:>10} {:>6} {:>10}",
            "Model".bold().underlined(),
            "Input".bold().underlined(),
            "Output".bold().underlined(),
            "Total".bold().underlined(),
            "Runs".bold().underlined(),
            "Avg ms".bold().underlined(),
        );
        let _ = writeln!(stdout, "  {}", "\u{2500}".repeat(70).dark_grey(),);

        let mut models: Vec<_> = stats.by_model.values().collect();
        models.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));

        for m in &models {
            let avg = m
                .avg_duration_ms
                .map_or_else(|| "-".to_string(), |d| format!("{d:.0}"));
            let _ = writeln!(
                stdout,
                "  {:<20} {:>10} {:>10} {:>10} {:>6} {:>10}",
                m.model.as_str().cyan(),
                fmt_num(m.input_tokens),
                fmt_num(m.output_tokens),
                fmt_num(m.total_tokens),
                m.executions,
                avg,
            );
        }

        // Aggregation totals row when multiple models present
        if models.len() > 1 {
            let _ = writeln!(stdout, "  {}", "\u{2500}".repeat(70).dark_grey());
            let _ = writeln!(
                stdout,
                "  {:<20} {:>10} {:>10} {:>10} {:>6}",
                "Total".bold(),
                fmt_num(stats.total_tokens_input),
                fmt_num(stats.total_tokens_output),
                fmt_num(stats.total_tokens),
                stats.executions,
            );
        }
    }
    let _ = writeln!(stdout);
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
    let mut stdout = io::stdout();
    let w = box_content_width();

    let border_h = "\u{2500}".repeat(w);
    let _ = writeln!(stdout, "\u{250c}{border_h}\u{2510}");

    // Workspace name (bold) with padding
    let ws_label = "Workspace: ";
    let ws_pad = w.saturating_sub(2 + ws_label.len() + status.workspace.len());
    let _ = writeln!(
        stdout,
        "\u{2502} {ws_label}{}{:ws_pad$} \u{2502}",
        status.workspace.clone().bold(),
        "",
        ws_pad = ws_pad,
    );

    let cfg_label = "Config:    ";
    let cfg_val = &status.config_path;
    let cfg_pad = w.saturating_sub(2 + cfg_label.len() + cfg_val.len());
    let _ = writeln!(
        stdout,
        "\u{2502} {cfg_label}{cfg_val}{:cfg_pad$} \u{2502}",
        "",
        cfg_pad = cfg_pad,
    );

    let items_label = "Items:     ";
    let items_val = status.item_count.to_string();
    let items_pad = w.saturating_sub(2 + items_label.len() + items_val.len());
    let _ = writeln!(
        stdout,
        "\u{2502} {items_label}{items_val}{:items_pad$} \u{2502}",
        "",
        items_pad = items_pad,
    );

    let _ = writeln!(stdout, "\u{2514}{border_h}\u{2518}");

    if !status.phase_counts.is_empty() {
        let bar_width = (w.saturating_sub(32)).max(8);

        let _ = writeln!(stdout);
        let _ = writeln!(
            stdout,
            "  {} {:<14} {} {:<8} {} {}",
            "\u{2502}".dark_grey(),
            "Phase".bold().underlined(),
            "\u{2502}".dark_grey(),
            "Count".bold().underlined(),
            "\u{2502}".dark_grey(),
            "Progress".bold().underlined(),
        );
        let _ = writeln!(
            stdout,
            "  {}{}{}{}{}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{253c}".dark_grey(),
            "\u{2500}".repeat(16),
            "\u{253c}".dark_grey(),
            "\u{2500}".repeat(9),
            "\u{253c}".dark_grey(),
        );
        let total = status.item_count.max(1);
        for pc in &status.phase_counts {
            let color = to_crossterm_color(dashboard::phase_color(&pc.phase));
            let phase_styled = style::style(format!("{:<14}", pc.phase)).with(color);
            let pct = (pc.count as f64 / total as f64 * 100.0) as u32;
            let bar = render_progress_bar(pct, bar_width);
            let _ = writeln!(
                stdout,
                "  {} {phase_styled} {} {:<8} {} {} {:>3}%",
                "\u{2502}".dark_grey(),
                "\u{2502}".dark_grey(),
                pc.count,
                "\u{2502}".dark_grey(),
                bar,
                pct,
            );
        }

        // Overall completion: done + completed + skipped as "finished"
        let finished: u32 = status
            .phase_counts
            .iter()
            .filter(|pc| matches!(pc.phase.as_str(), "done" | "completed" | "skipped"))
            .map(|pc| pc.count)
            .sum();
        let overall_pct = (finished as f64 / total as f64 * 100.0) as u32;
        let overall_bar_width = (w.saturating_sub(22)).max(10);
        let overall_bar = render_progress_bar(overall_pct, overall_bar_width);
        let _ = writeln!(stdout);
        let _ = writeln!(
            stdout,
            "  Overall: {}/{} {} {:>3}%",
            finished, total, overall_bar, overall_pct
        );
    }

    // Token usage analysis
    if let Some(ref usage) = status.token_usage {
        let _ = writeln!(stdout);
        let _ = writeln!(stdout, "  {}", "Token Usage".bold().cyan());
        let _ = writeln!(stdout, "  {}", "\u{2500}".repeat(w.min(60)).dark_grey());
        let _ = writeln!(
            stdout,
            "  Total:  {} (in: {} / out: {})",
            fmt_num(usage.total_tokens).bold(),
            fmt_num(usage.total_input),
            fmt_num(usage.total_output),
        );
        let _ = writeln!(
            stdout,
            "  Executions: {}",
            usage.executions.to_string().bold()
        );
        if !usage.by_model.is_empty() {
            let _ = writeln!(stdout);
            let _ = writeln!(
                stdout,
                "  {:<20} {:>10} {:>10} {:>10} {:>5}",
                "Model".bold().underlined(),
                "Input".bold().underlined(),
                "Output".bold().underlined(),
                "Total".bold().underlined(),
                "Runs".bold().underlined(),
            );
            let _ = writeln!(stdout, "  {}", "\u{2500}".repeat(59).dark_grey(),);
            for m in &usage.by_model {
                let _ = writeln!(
                    stdout,
                    "  {:<20} {:>10} {:>10} {:>10} {:>5}",
                    m.model.as_str().cyan(),
                    fmt_num(m.input_tokens),
                    fmt_num(m.output_tokens),
                    fmt_num(m.total_tokens),
                    m.executions,
                );
            }
            // Aggregation totals row
            if usage.by_model.len() > 1 {
                let _ = writeln!(stdout, "  {}", "\u{2500}".repeat(59).dark_grey());
                let _ = writeln!(
                    stdout,
                    "  {:<20} {:>10} {:>10} {:>10} {:>5}",
                    "Total".bold(),
                    fmt_num(usage.total_input),
                    fmt_num(usage.total_output),
                    fmt_num(usage.total_tokens),
                    usage.executions,
                );
            }
        }
    }
}

/// Render a text-based progress bar of the given width.
///
/// `pct` is a value 0..=100 and `width` is the number of character cells.
fn render_progress_bar(pct: u32, width: usize) -> String {
    let filled = (pct as usize * width / 100).min(width);
    let empty = width - filled;
    format!("[{}{}]", "#".repeat(filled), ".".repeat(empty),)
}

/// Format a number with comma separators for readability.
fn fmt_num(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}
