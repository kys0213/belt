//! TUI Dashboard and runtime statistics panel for Belt.
//!
//! Provides two display modes:
//! - `run()`: interactive ratatui-based real-time TUI dashboard (3-panel layout)
//! - `render_runtime_panel()`: text-based runtime statistics panel for non-TUI output
//!
//! The dashboard supports item selection with arrow keys and an item detail
//! overlay (Enter) that displays transition timeline history.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};

use belt_core::phase::QueuePhase;
use belt_core::queue::QueueItem;
use belt_infra::db::{Database, RuntimeStats, TransitionEvent};

/// Which panel is currently focused for item selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivePanel {
    Running,
    Recent,
}

/// Dashboard UI state for navigation and overlay management.
struct DashboardState {
    /// Which panel is focused.
    active_panel: ActivePanel,
    /// Selected row index within the active panel.
    selected_index: usize,
    /// When set, the item detail overlay is shown for this work_id.
    overlay_item: Option<String>,
}

impl DashboardState {
    fn new() -> Self {
        Self {
            active_panel: ActivePanel::Running,
            selected_index: 0,
            overlay_item: None,
        }
    }
}

/// Run the interactive TUI dashboard.
///
/// The dashboard refreshes every second and exits on `q` or `Esc`.
/// Use arrow keys or `j`/`k` to navigate items, `Tab` to switch panels,
/// and `Enter` to open the item detail overlay with transition timeline.
pub fn run(db: Arc<Database>) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &db);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    db: &Database,
) -> anyhow::Result<()> {
    let mut state = DashboardState::new();

    loop {
        // Collect items for display and selection.
        let running_items = db
            .list_items(Some(QueuePhase::Running), None)
            .unwrap_or_default();

        let recent_items = collect_recent_items(db);

        let active_panel_len = match state.active_panel {
            ActivePanel::Running => running_items.len(),
            ActivePanel::Recent => recent_items.len(),
        };

        // Clamp selected_index.
        if active_panel_len == 0 {
            state.selected_index = 0;
        } else if state.selected_index >= active_panel_len {
            state.selected_index = active_panel_len.saturating_sub(1);
        }

        terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(5),
                    Constraint::Min(8),
                    Constraint::Length(10),
                ])
                .split(frame.area());

            // Top: phase summary
            let summary = render_phase_summary(db);
            frame.render_widget(summary, chunks[0]);

            // Middle: running items (with selection highlight)
            let running_table = render_running_items_stateful(
                &running_items,
                state.active_panel == ActivePanel::Running,
                state.selected_index,
            );
            frame.render_widget(running_table, chunks[1]);

            // Bottom: recent completed/failed (with selection highlight)
            let recent_table = render_recent_items_stateful(
                &recent_items,
                state.active_panel == ActivePanel::Recent,
                state.selected_index,
            );
            frame.render_widget(recent_table, chunks[2]);

            // Overlay: item detail
            if let Some(ref work_id) = state.overlay_item {
                render_item_detail_overlay(frame, db, work_id);
            }
        })?;

        // Poll for keyboard events with 1 second timeout
        if event::poll(Duration::from_secs(1))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            // When overlay is open, only handle close keys.
            if state.overlay_item.is_some() {
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                    state.overlay_item = None;
                }
                continue;
            }

            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Up | KeyCode::Char('k') => {
                    state.selected_index = state.selected_index.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if active_panel_len > 0 {
                        state.selected_index =
                            (state.selected_index + 1).min(active_panel_len.saturating_sub(1));
                    }
                }
                KeyCode::Tab => {
                    state.active_panel = match state.active_panel {
                        ActivePanel::Running => ActivePanel::Recent,
                        ActivePanel::Recent => ActivePanel::Running,
                    };
                    state.selected_index = 0;
                }
                KeyCode::Enter => {
                    let items = match state.active_panel {
                        ActivePanel::Running => &running_items,
                        ActivePanel::Recent => &recent_items,
                    };
                    if let Some(item) = items.get(state.selected_index) {
                        state.overlay_item = Some(item.work_id.clone());
                    }
                }
                _ => {}
            }
        }
    }
}

/// Collect recent completed/failed/done items, sorted by updated_at descending.
fn collect_recent_items(db: &Database) -> Vec<QueueItem> {
    let done_items = db
        .list_items(Some(QueuePhase::Done), None)
        .unwrap_or_default();
    let failed_items = db
        .list_items(Some(QueuePhase::Failed), None)
        .unwrap_or_default();
    let completed_items = db
        .list_items(Some(QueuePhase::Completed), None)
        .unwrap_or_default();

    let mut all_items: Vec<_> = done_items
        .into_iter()
        .chain(failed_items)
        .chain(completed_items)
        .collect();

    all_items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    all_items.truncate(8);
    all_items
}

fn render_phase_summary(db: &Database) -> Paragraph<'static> {
    let counts = db.count_items_by_phase().unwrap_or_default();
    let mut spans: Vec<Span<'static>> = Vec::new();

    let all_phases = [
        "pending",
        "ready",
        "running",
        "completed",
        "done",
        "hitl",
        "failed",
        "skipped",
    ];

    for (i, phase) in all_phases.iter().enumerate() {
        let count = counts
            .iter()
            .find(|(p, _)| p == phase)
            .map(|(_, c)| *c)
            .unwrap_or(0);

        let color = phase_color(phase);
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("{phase}: {count}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    }

    let total: u32 = counts.iter().map(|(_, c)| *c).sum();
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        format!("total: {total}"),
        Style::default().add_modifier(Modifier::BOLD),
    ));

    Paragraph::new(Line::from(spans)).block(
        Block::default()
            .title(" Phase Summary ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    )
}

/// Render running items table with optional selection highlight.
fn render_running_items_stateful(
    items: &[QueueItem],
    is_active: bool,
    selected: usize,
) -> Table<'static> {
    let rows: Vec<Row<'static>> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let row = Row::new(vec![
                Cell::from(item.work_id.clone()),
                Cell::from(item.workspace_id.clone()),
                Cell::from(item.state.clone()),
                Cell::from(item.updated_at.clone()),
            ]);
            if is_active && i == selected {
                row.style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                row
            }
        })
        .collect();

    let header = Row::new(vec!["Work ID", "Workspace", "State", "Updated"])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(1);

    let border_color = if is_active {
        Color::Green
    } else {
        Color::DarkGray
    };

    Table::new(
        rows,
        [
            Constraint::Percentage(35),
            Constraint::Percentage(20),
            Constraint::Percentage(20),
            Constraint::Percentage(25),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(" Running Items ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color)),
    )
}

/// Render recent items table with optional selection highlight.
fn render_recent_items_stateful(
    items: &[QueueItem],
    is_active: bool,
    selected: usize,
) -> Table<'static> {
    let rows: Vec<Row<'static>> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let phase_str = item.phase.as_str().to_string();
            let color = phase_color(&phase_str);
            let row = Row::new(vec![
                Cell::from(item.work_id.clone()),
                Cell::from(phase_str).style(Style::default().fg(color)),
                Cell::from(item.workspace_id.clone()),
                Cell::from(item.updated_at.clone()),
            ]);
            if is_active && i == selected {
                row.style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                row
            }
        })
        .collect();

    let header = Row::new(vec!["Work ID", "Phase", "Workspace", "Updated"])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(1);

    let border_color = if is_active {
        Color::Magenta
    } else {
        Color::DarkGray
    };

    Table::new(
        rows,
        [
            Constraint::Percentage(35),
            Constraint::Percentage(15),
            Constraint::Percentage(20),
            Constraint::Percentage(30),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(" Recent Completed/Failed ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color)),
    )
}

/// Render the item detail overlay as a centered popup.
///
/// Displays item metadata and a transition timeline showing state changes
/// with timestamps.
fn render_item_detail_overlay(frame: &mut ratatui::Frame, db: &Database, work_id: &str) {
    let area = centered_rect(60, 70, frame.area());

    // Clear the area behind the overlay.
    frame.render_widget(Clear, area);

    let item = db.get_item(work_id);
    let transitions = db.list_transition_events(work_id).unwrap_or_default();

    let lines = build_detail_lines(work_id, item.ok().as_ref(), &transitions);

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .title(" Item Details ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );

    frame.render_widget(paragraph, area);
}

/// Build the text lines for the item detail overlay.
fn build_detail_lines<'a>(
    work_id: &str,
    item: Option<&QueueItem>,
    transitions: &[TransitionEvent],
) -> Vec<Line<'a>> {
    let mut lines: Vec<Line<'a>> = Vec::new();

    match item {
        Some(item) => {
            let phase_str = item.phase.as_str().to_string();
            let color = phase_color(&phase_str);

            lines.push(Line::from(vec![
                Span::styled("ID: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(item.work_id.clone()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Status: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(phase_str, Style::default().fg(color)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("State: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(item.state.clone()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Workspace: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(item.workspace_id.clone()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Created: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(item.created_at.clone()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Updated: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(item.updated_at.clone()),
            ]));

            if let Some(ref title) = item.title {
                lines.push(Line::from(vec![
                    Span::styled("Title: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(title.clone()),
                ]));
            }
        }
        None => {
            lines.push(Line::from(vec![
                Span::styled("ID: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(work_id.to_string()),
            ]));
            lines.push(Line::from(Span::styled(
                "(item not found in database)",
                Style::default().fg(Color::Red),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Transition Timeline:",
        Style::default()
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::UNDERLINED),
    )));

    if transitions.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no transitions recorded)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for event in transitions {
            let from_color = phase_color(&event.from_state);
            let to_color = phase_color(&event.to_state);

            // Format timestamp: show only the time portion if possible.
            let time_display = format_transition_time(&event.timestamp);

            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(event.from_state.clone(), Style::default().fg(from_color)),
                Span::styled(" -> ", Style::default().fg(Color::Gray)),
                Span::styled(event.to_state.clone(), Style::default().fg(to_color)),
                Span::raw("  "),
                Span::styled(time_display, Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[q/Esc] Close",
        Style::default().fg(Color::DarkGray),
    )));

    lines
}

/// Format an RFC 3339 timestamp to a shorter display form.
///
/// Extracts `HH:MM:SS` from the timestamp for compact display.
/// Falls back to the full timestamp if parsing fails.
fn format_transition_time(timestamp: &str) -> String {
    // RFC 3339 format: "2026-03-25T10:00:00+00:00" or "2026-03-25T10:00:00Z"
    // Extract date + time portion.
    if let Some(t_pos) = timestamp.find('T') {
        let time_part = &timestamp[t_pos + 1..];
        // Take up to the timezone offset (+, -, or Z).
        let end = time_part.find(['+', 'Z']).unwrap_or(time_part.len());
        let time_str = &time_part[..end];
        // Include the date for clarity.
        let date_part = &timestamp[..t_pos];
        format!("{date_part} {time_str}")
    } else {
        timestamp.to_string()
    }
}

/// Compute a centered rectangle within the given area.
///
/// `percent_x` and `percent_y` specify the percentage of the area to use
/// for the popup dimensions.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn phase_color(phase: &str) -> Color {
    match phase {
        "pending" => Color::Gray,
        "ready" => Color::Blue,
        "running" => Color::Green,
        "completed" => Color::Cyan,
        "done" => Color::White,
        "hitl" => Color::Yellow,
        "failed" => Color::Red,
        "skipped" => Color::DarkGray,
        _ => Color::White,
    }
}

/// Render the runtime statistics panel to stdout.
///
/// Displays overall token totals, execution count, average duration,
/// and a per-model breakdown table.
pub fn render_runtime_panel(stats: &RuntimeStats) {
    println!("=== Runtime Stats (last 24h) ===");
    println!();
    println!(
        "  Total tokens:  {} (in: {} / out: {})",
        format_number(stats.total_tokens),
        format_number(stats.total_tokens_input),
        format_number(stats.total_tokens_output),
    );
    println!("  Executions:    {}", stats.executions);
    match stats.avg_duration_ms {
        Some(d) => println!("  Avg duration:  {:.0}ms", d),
        None => println!("  Avg duration:  -"),
    }

    if !stats.by_model.is_empty() {
        println!();
        println!(
            "  {:<20} {:>10} {:>10} {:>10} {:>6} {:>10}",
            "Model", "Input", "Output", "Total", "Runs", "Avg ms"
        );
        println!("  {}", "-".repeat(70));

        let mut models: Vec<_> = stats.by_model.values().collect();
        models.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));

        for m in models {
            let avg = m
                .avg_duration_ms
                .map_or_else(|| "-".to_string(), |d| format!("{d:.0}"));
            println!(
                "  {:<20} {:>10} {:>10} {:>10} {:>6} {:>10}",
                m.model,
                format_number(m.input_tokens),
                format_number(m.output_tokens),
                format_number(m.total_tokens),
                m.executions,
                avg,
            );
        }
    }

    println!();
}

/// Format a number with comma-separated thousands for readability.
fn format_number(n: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_number_small() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
    }

    #[test]
    fn format_number_thousands() {
        assert_eq!(format_number(1_000), "1,000");
        assert_eq!(format_number(1_234_567), "1,234,567");
    }

    #[test]
    fn format_number_millions() {
        assert_eq!(format_number(10_000_000), "10,000,000");
    }

    #[test]
    fn format_number_exact_boundary_1000() {
        // 1_000 is the first value that requires a comma separator.
        assert_eq!(format_number(1_000), "1,000");
    }

    #[test]
    fn format_number_just_below_boundary() {
        assert_eq!(format_number(999), "999");
    }

    #[test]
    fn format_number_exactly_10000() {
        assert_eq!(format_number(10_000), "10,000");
    }

    #[test]
    fn format_number_exactly_100000() {
        assert_eq!(format_number(100_000), "100,000");
    }

    #[test]
    fn format_number_u64_large_value() {
        assert_eq!(format_number(1_000_000_000), "1,000,000,000");
    }

    // ---- render_runtime_panel: by_model sorting ----

    #[test]
    fn render_runtime_panel_does_not_panic_with_empty_stats() {
        use std::collections::HashMap;
        let stats = RuntimeStats {
            total_tokens_input: 0,
            total_tokens_output: 0,
            total_tokens: 0,
            executions: 0,
            avg_duration_ms: None,
            by_model: HashMap::new(),
        };
        // Must not panic.
        render_runtime_panel(&stats);
    }

    #[test]
    fn render_runtime_panel_does_not_panic_with_avg_duration() {
        use std::collections::HashMap;
        let stats = RuntimeStats {
            total_tokens_input: 500,
            total_tokens_output: 300,
            total_tokens: 800,
            executions: 5,
            avg_duration_ms: Some(123.4),
            by_model: HashMap::new(),
        };
        render_runtime_panel(&stats);
    }

    #[test]
    fn render_runtime_panel_does_not_panic_with_multiple_models() {
        use std::collections::HashMap;

        use belt_infra::db::ModelStats;

        let mut by_model = HashMap::new();
        by_model.insert(
            "claude-sonnet".to_string(),
            ModelStats {
                model: "claude-sonnet".to_string(),
                input_tokens: 1_000,
                output_tokens: 500,
                total_tokens: 1_500,
                executions: 3,
                avg_duration_ms: Some(200.0),
            },
        );
        by_model.insert(
            "claude-haiku".to_string(),
            ModelStats {
                model: "claude-haiku".to_string(),
                input_tokens: 200,
                output_tokens: 100,
                total_tokens: 300,
                executions: 1,
                avg_duration_ms: None,
            },
        );

        let stats = RuntimeStats {
            total_tokens_input: 1_200,
            total_tokens_output: 600,
            total_tokens: 1_800,
            executions: 4,
            avg_duration_ms: Some(175.0),
            by_model,
        };
        // Sorting by total_tokens descending must not panic.
        render_runtime_panel(&stats);
    }

    #[test]
    fn model_stats_sorted_by_total_tokens_descending() {
        use std::collections::HashMap;

        use belt_infra::db::ModelStats;

        let mut by_model = HashMap::new();
        by_model.insert(
            "small-model".to_string(),
            ModelStats {
                model: "small-model".to_string(),
                input_tokens: 10,
                output_tokens: 10,
                total_tokens: 20,
                executions: 1,
                avg_duration_ms: None,
            },
        );
        by_model.insert(
            "large-model".to_string(),
            ModelStats {
                model: "large-model".to_string(),
                input_tokens: 5_000,
                output_tokens: 3_000,
                total_tokens: 8_000,
                executions: 10,
                avg_duration_ms: Some(500.0),
            },
        );

        // Verify the sort logic used in render_runtime_panel directly.
        let mut models: Vec<_> = by_model.values().collect();
        models.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));

        assert_eq!(models[0].model, "large-model");
        assert_eq!(models[1].model, "small-model");
    }

    // ---- phase_color ----

    #[test]
    fn phase_color_pending() {
        assert_eq!(phase_color("pending"), Color::Gray);
    }

    #[test]
    fn phase_color_ready() {
        assert_eq!(phase_color("ready"), Color::Blue);
    }

    #[test]
    fn phase_color_running() {
        assert_eq!(phase_color("running"), Color::Green);
    }

    #[test]
    fn phase_color_completed() {
        assert_eq!(phase_color("completed"), Color::Cyan);
    }

    #[test]
    fn phase_color_done() {
        assert_eq!(phase_color("done"), Color::White);
    }

    #[test]
    fn phase_color_hitl() {
        assert_eq!(phase_color("hitl"), Color::Yellow);
    }

    #[test]
    fn phase_color_failed() {
        assert_eq!(phase_color("failed"), Color::Red);
    }

    #[test]
    fn phase_color_skipped() {
        assert_eq!(phase_color("skipped"), Color::DarkGray);
    }

    #[test]
    fn phase_color_unknown_returns_white() {
        assert_eq!(phase_color("unknown"), Color::White);
        assert_eq!(phase_color(""), Color::White);
        assert_eq!(phase_color("PENDING"), Color::White);
    }

    // ---- format_transition_time ----

    #[test]
    fn format_transition_time_rfc3339_utc() {
        assert_eq!(
            format_transition_time("2026-03-25T10:05:00Z"),
            "2026-03-25 10:05:00"
        );
    }

    #[test]
    fn format_transition_time_rfc3339_offset() {
        assert_eq!(
            format_transition_time("2026-03-25T10:05:00+09:00"),
            "2026-03-25 10:05:00"
        );
    }

    #[test]
    fn format_transition_time_no_t_separator() {
        let ts = "2026-03-25 10:05:00";
        assert_eq!(format_transition_time(ts), ts);
    }

    // ---- centered_rect ----

    #[test]
    fn centered_rect_produces_inner_area() {
        let area = Rect::new(0, 0, 100, 50);
        let result = centered_rect(60, 70, area);
        // The popup should be smaller than the full area.
        assert!(result.width <= 60);
        assert!(result.height <= 35);
        assert!(result.x > 0);
        assert!(result.y > 0);
    }

    // ---- build_detail_lines ----

    #[test]
    fn build_detail_lines_with_item_and_no_transitions() {
        let item = QueueItem::new(
            "w1".to_string(),
            "src1".to_string(),
            "ws1".to_string(),
            "analyze".to_string(),
        );
        let lines = build_detail_lines("w1", Some(&item), &[]);
        // Should contain ID, Status, State, Workspace, Created, Updated lines,
        // plus the timeline header and "(no transitions recorded)" and close hint.
        assert!(lines.len() >= 9);
        // Check the "no transitions" message is present.
        let text: String = lines.iter().map(|l| format!("{l}")).collect::<String>();
        assert!(text.contains("no transitions recorded"));
    }

    #[test]
    fn build_detail_lines_with_transitions() {
        let item = QueueItem::new(
            "w1".to_string(),
            "src1".to_string(),
            "ws1".to_string(),
            "implement".to_string(),
        );
        let transitions = vec![
            TransitionEvent {
                id: "e1".to_string(),
                item_id: "w1".to_string(),
                from_state: "pending".to_string(),
                to_state: "ready".to_string(),
                event_type: "phase_change".to_string(),
                timestamp: "2026-03-25T10:00:00Z".to_string(),
                metadata: None,
            },
            TransitionEvent {
                id: "e2".to_string(),
                item_id: "w1".to_string(),
                from_state: "ready".to_string(),
                to_state: "running".to_string(),
                event_type: "phase_change".to_string(),
                timestamp: "2026-03-25T10:05:00Z".to_string(),
                metadata: None,
            },
        ];
        let lines = build_detail_lines("w1", Some(&item), &transitions);
        let text: String = lines.iter().map(|l| format!("{l}")).collect::<String>();
        assert!(text.contains("pending"));
        assert!(text.contains("ready"));
        assert!(text.contains("running"));
        assert!(text.contains("10:00:00"));
        assert!(text.contains("10:05:00"));
    }

    #[test]
    fn build_detail_lines_item_not_found() {
        let lines = build_detail_lines("missing-id", None, &[]);
        let text: String = lines.iter().map(|l| format!("{l}")).collect::<String>();
        assert!(text.contains("missing-id"));
        assert!(text.contains("item not found"));
    }

    // ---- collect_recent_items ----

    #[test]
    fn active_panel_default_is_running() {
        let state = DashboardState::new();
        assert_eq!(state.active_panel, ActivePanel::Running);
        assert_eq!(state.selected_index, 0);
        assert!(state.overlay_item.is_none());
    }
}
