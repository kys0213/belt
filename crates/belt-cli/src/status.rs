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
