//! TUI Dashboard and runtime statistics panel for Belt.
//!
//! Provides two display modes:
//! - `run()`: interactive ratatui-based real-time TUI dashboard (3-panel layout)
//! - `render_runtime_panel()`: text-based runtime statistics panel for non-TUI output

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
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use belt_core::phase::QueuePhase;
use belt_infra::db::{Database, RuntimeStats};

/// Run the interactive TUI dashboard.
///
/// The dashboard refreshes every second and exits on `q` or `Esc`.
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
    loop {
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

            // Middle: running items
            let running = render_running_items(db);
            frame.render_widget(running, chunks[1]);

            // Bottom: recent completed/failed
            let recent = render_recent_items(db);
            frame.render_widget(recent, chunks[2]);
        })?;

        // Poll for keyboard events with 1 second timeout
        if event::poll(Duration::from_secs(1))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            return Ok(());
        }
    }
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

fn render_running_items(db: &Database) -> Table<'static> {
    let items = db
        .list_items(Some(QueuePhase::Running), None)
        .unwrap_or_default();

    let rows: Vec<Row<'static>> = items
        .into_iter()
        .map(|item| {
            Row::new(vec![
                Cell::from(item.work_id),
                Cell::from(item.workspace_id),
                Cell::from(item.state),
                Cell::from(item.updated_at),
            ])
        })
        .collect();

    let header = Row::new(vec!["Work ID", "Workspace", "State", "Updated"])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(1);

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
            .border_style(Style::default().fg(Color::Green)),
    )
}

fn render_recent_items(db: &Database) -> Table<'static> {
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

    // Sort by updated_at descending, take latest 8
    all_items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    all_items.truncate(8);

    let rows: Vec<Row<'static>> = all_items
        .into_iter()
        .map(|item| {
            let phase_str = item.phase.as_str().to_string();
            let color = phase_color(&phase_str);
            Row::new(vec![
                Cell::from(item.work_id),
                Cell::from(phase_str).style(Style::default().fg(color)),
                Cell::from(item.workspace_id),
                Cell::from(item.updated_at),
            ])
        })
        .collect();

    let header = Row::new(vec!["Work ID", "Phase", "Workspace", "Updated"])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(1);

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
            .border_style(Style::default().fg(Color::Magenta)),
    )
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
}
