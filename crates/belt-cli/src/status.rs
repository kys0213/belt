//! Status display for `belt status` and `belt spec status`.
//!
//! Supports `text`, `json`, and `rich` output formats.
//! The `rich` format uses crossterm colours, box-drawing tables, and a
//! progress bar to visualise queue-phase distribution in the terminal.

use std::io::{self, Write};

use belt_core::phase::QueuePhase;
use belt_infra::db::{Database, RuntimeStats};
use crossterm::style::{self, Stylize};
use serde::Serialize;

use crate::dashboard;

/// System status summary returned by `belt status`.
#[derive(Debug, Serialize)]
pub struct SystemStatus {
    pub total_items: u32,
    pub hitl_count: u32,
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
        hitl_count,
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
            item_id: "w1:implement".to_string(),
            from_state: "pending".to_string(),
            to_state: "running".to_string(),
            event_type: "phase_change".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };
        db.insert_transition_event(&ev).unwrap();

        let status = gather_status(&db).unwrap();

        assert_eq!(status.recent_events.len(), 1);
        assert_eq!(status.recent_events[0].item_id, "w1:implement");
        assert_eq!(status.recent_events[0].from_state, "pending");
        assert_eq!(status.recent_events[0].to_state, "running");
        assert_eq!(status.recent_events[0].event_type, "phase_change");
    }

    #[test]
    fn gather_status_recent_events_capped_at_ten() {
        let db = test_db();

        for i in 0..15u32 {
            let ev = TransitionEvent {
                id: format!("ev-{i}"),
                item_id: format!("w{i}:implement"),
                from_state: "pending".to_string(),
                to_state: "running".to_string(),
                event_type: "phase_change".to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                metadata: None,
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
        };
        // Should not panic
        super::print_rich_status(&status);
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
        };
        // Calling print_status with "rich" should not panic.
        super::print_status(&status, "rich").unwrap();
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
        let result = print_status(&status, "json");
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
        let result = print_status(&status, "text");
        assert!(result.is_ok());
    }

    #[test]
    fn print_status_rich_format_returns_ok() {
        let status = sample_system_status();
        let result = print_status(&status, "rich");
        assert!(result.is_ok());
    }

    #[test]
    fn print_status_unknown_format_falls_back_to_text() {
        let status = sample_system_status();
        // Unknown format should not error — falls back to text.
        let result = print_status(&status, "unknown");
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
        print_rich_status(&status);
    }

    #[test]
    fn print_rich_status_empty_does_not_panic() {
        let status = empty_system_status();
        print_rich_status(&status);
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

fn print_rich_status(status: &SystemStatus) {
    let mut stdout = io::stdout();

    // Header
    let _ = writeln!(
        stdout,
        "\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}"
    );
    let title = format!("{:^36}", "Belt System Status").bold();
    let _ = writeln!(stdout, "\u{2502} {title}  \u{2502}");
    let _ = writeln!(
        stdout,
        "\u{251c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2524}"
    );
    let _ = writeln!(
        stdout,
        "\u{2502} Total items: {:<23} \u{2502}",
        status.total_items
    );
    let _ = writeln!(
        stdout,
        "\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2518}"
    );

    // Phase table with colours
    if !status.phase_counts.is_empty() {
        let _ = writeln!(stdout);
        let _ = writeln!(
            stdout,
            "  {} {:<14} {} {:<8}",
            "\u{2502}".dark_grey(),
            "Phase".bold().underlined(),
            "\u{2502}".dark_grey(),
            "Count".bold().underlined(),
        );
        let _ = writeln!(
            stdout,
            "  {}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}{}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{253c}".dark_grey(),
            "\u{253c}".dark_grey(),
        );
        for pc in &status.phase_counts {
            let color = to_crossterm_color(dashboard::phase_color(&pc.phase));
            let phase_styled = style::style(format!("{:<14}", pc.phase)).with(color);
            let _ = writeln!(
                stdout,
                "  {} {phase_styled} {} {:<8}",
                "\u{2502}".dark_grey(),
                "\u{2502}".dark_grey(),
                pc.count,
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

    if status.hitl_count > 0 {
        println!();
        println!(
            "\x1b[33m\u{26a0}\u{fe0f} {} items require human intervention. Run 'belt claw' to review.\x1b[0m",
            status.hitl_count
        );
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
    let mut stdout = io::stdout();

    let _ = writeln!(
        stdout,
        "\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}"
    );
    let ws_name = status.workspace.clone().bold();
    let _ = writeln!(stdout, "\u{2502} Workspace: {ws_name:<25} \u{2502}");
    let _ = writeln!(
        stdout,
        "\u{2502} Config:    {:<25} \u{2502}",
        status.config_path
    );
    let _ = writeln!(
        stdout,
        "\u{2502} Items:     {:<25} \u{2502}",
        status.item_count
    );
    let _ = writeln!(
        stdout,
        "\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2518}"
    );

    if !status.phase_counts.is_empty() {
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
            "  {}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}{}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}{}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{253c}".dark_grey(),
            "\u{253c}".dark_grey(),
            "\u{253c}".dark_grey(),
        );
        let total = status.item_count.max(1);
        for pc in &status.phase_counts {
            let color = to_crossterm_color(dashboard::phase_color(&pc.phase));
            let phase_styled = style::style(format!("{:<14}", pc.phase)).with(color);
            let pct = (pc.count as f64 / total as f64 * 100.0) as u32;
            let bar = render_progress_bar(pct, 12);
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
        let overall_bar = render_progress_bar(overall_pct, 28);
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
        let _ = writeln!(stdout, "  {}", "Token Usage".bold());
        let _ = writeln!(
            stdout,
            "  Total:  {} (in: {} / out: {})",
            fmt_num(usage.total_tokens),
            fmt_num(usage.total_input),
            fmt_num(usage.total_output),
        );
        let _ = writeln!(stdout, "  Executions: {}", usage.executions);
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
            for m in &usage.by_model {
                let _ = writeln!(
                    stdout,
                    "  {:<20} {:>10} {:>10} {:>10} {:>5}",
                    m.model,
                    fmt_num(m.input_tokens),
                    fmt_num(m.output_tokens),
                    fmt_num(m.total_tokens),
                    m.executions,
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
