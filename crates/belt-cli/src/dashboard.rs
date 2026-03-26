//! TUI Dashboard and runtime statistics panel for Belt.
//!
//! Provides two display modes:
//! - `run()`: interactive ratatui-based real-time TUI dashboard with multiple tabs
//! - `render_runtime_panel()`: text-based runtime statistics panel for non-TUI output
//!
//! The dashboard supports six tabs:
//! - **Dashboard** (`d`): phase summary + running/recent items
//! - **PerWorkspace** (`w`): items filtered by a selected workspace
//! - **Spec** (`s`): spec progress view
//! - **Board** (`b`): kanban-style board with columns per queue phase
//! - **DataSource** (`n`): real-time DataSource connection status panel
//! - **Scripts** (`x`): script execution statistics with success/fail rates
//!
//! Tab switching: `d/w/s/b/n/x` to jump, or `Tab`/`Shift+Tab` to cycle.
//! Item selection with arrow keys and item detail overlay (Enter).
//! Help overlay (`h`) showing all available key bindings.
//! Scroll positions are preserved per tab.

use std::collections::HashMap;
use std::io;
use std::path::Path;
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
use belt_core::spec::SpecStatus;
use belt_infra::db::{Database, HistoryEvent, RuntimeStats, ScriptExecStats, TransitionEvent};
use belt_infra::workspace_loader::load_workspace_config;

/// Connection status of a DataSource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataSourceConnectionStatus {
    /// DataSource is connected and has recent activity.
    Connected,
    /// DataSource is configured but has no recent activity.
    Disconnected,
    /// DataSource configuration could not be loaded.
    Error,
}

impl DataSourceConnectionStatus {
    /// Return a human-readable label for display.
    fn label(self) -> &'static str {
        match self {
            DataSourceConnectionStatus::Connected => "Connected",
            DataSourceConnectionStatus::Disconnected => "Disconnected",
            DataSourceConnectionStatus::Error => "Error",
        }
    }

    /// Return the display color for this status.
    fn color(self) -> Color {
        match self {
            DataSourceConnectionStatus::Connected => Color::Green,
            DataSourceConnectionStatus::Disconnected => Color::DarkGray,
            DataSourceConnectionStatus::Error => Color::Red,
        }
    }

    /// Return a status indicator symbol.
    fn indicator(self) -> &'static str {
        match self {
            DataSourceConnectionStatus::Connected => "●",
            DataSourceConnectionStatus::Disconnected => "○",
            DataSourceConnectionStatus::Error => "✗",
        }
    }
}

/// Status information for a single DataSource.
#[derive(Debug, Clone)]
struct DataSourceStatusEntry {
    /// Workspace name this source belongs to.
    workspace: String,
    /// Source type name (e.g. "github").
    source_name: String,
    /// Source URL.
    url: String,
    /// Number of configured states/triggers.
    state_count: usize,
    /// Scan interval in seconds.
    scan_interval_secs: u64,
    /// Current connection status.
    status: DataSourceConnectionStatus,
    /// Number of active (non-terminal) items from this source.
    active_item_count: usize,
}

/// Which panel is currently focused for item selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivePanel {
    Running,
    Recent,
}

/// Which top-level tab is displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardTab {
    /// Main dashboard with phase summary + running/recent items.
    Dashboard,
    /// Per-workspace view showing items grouped by workspace.
    PerWorkspace,
    /// Spec progress view showing specs and their statuses.
    Spec,
    /// Kanban board with columns per queue phase.
    Board,
    /// DataSource connection status panel.
    DataSource,
    /// Scripts execution statistics panel.
    Scripts,
}

impl DashboardTab {
    /// Return the next tab in forward order (Tab key).
    fn next(self) -> Self {
        match self {
            DashboardTab::Dashboard => DashboardTab::PerWorkspace,
            DashboardTab::PerWorkspace => DashboardTab::Spec,
            DashboardTab::Spec => DashboardTab::Board,
            DashboardTab::Board => DashboardTab::DataSource,
            DashboardTab::DataSource => DashboardTab::Scripts,
            DashboardTab::Scripts => DashboardTab::Dashboard,
        }
    }

    /// Return the previous tab in reverse order (Shift+Tab).
    fn prev(self) -> Self {
        match self {
            DashboardTab::Dashboard => DashboardTab::Scripts,
            DashboardTab::PerWorkspace => DashboardTab::Dashboard,
            DashboardTab::Spec => DashboardTab::PerWorkspace,
            DashboardTab::Board => DashboardTab::Spec,
            DashboardTab::DataSource => DashboardTab::Board,
            DashboardTab::Scripts => DashboardTab::DataSource,
        }
    }
}

/// Board view columns corresponding to queue phases.
const BOARD_COLUMNS: [QueuePhase; 7] = [
    QueuePhase::Pending,
    QueuePhase::Ready,
    QueuePhase::Running,
    QueuePhase::Completed,
    QueuePhase::Done,
    QueuePhase::Hitl,
    QueuePhase::Failed,
];

/// Per-tab scroll/selection state, preserved across tab switches.
#[derive(Debug, Clone, Default)]
struct TabState {
    /// Selected row index within the primary list.
    selected_index: usize,
    /// For Dashboard tab: which sub-panel is focused.
    active_panel: Option<ActivePanel>,
}

/// Dashboard UI state for navigation and overlay management.
struct DashboardState {
    /// Which top-level tab is active.
    active_tab: DashboardTab,
    /// Per-tab state (scroll positions preserved on tab switch).
    tab_states: HashMap<u8, TabState>,
    /// When set, the item detail overlay is shown for this work_id.
    overlay_item: Option<String>,
    /// Whether the help overlay is visible.
    show_help: bool,
    /// Selected workspace index (used in PerWorkspace tab).
    selected_workspace: usize,
    /// Currently selected column in Board view.
    board_selected_col: usize,
    /// Currently selected row within the selected Board column.
    board_selected_row: usize,
}

impl DashboardState {
    fn new() -> Self {
        let mut tab_states = HashMap::new();
        tab_states.insert(
            0,
            TabState {
                selected_index: 0,
                active_panel: Some(ActivePanel::Running),
            },
        );
        tab_states.insert(1, TabState::default());
        tab_states.insert(2, TabState::default());
        tab_states.insert(3, TabState::default());
        tab_states.insert(4, TabState::default());
        tab_states.insert(5, TabState::default());

        Self {
            active_tab: DashboardTab::Dashboard,
            tab_states,
            overlay_item: None,
            show_help: false,
            selected_workspace: 0,
            board_selected_col: 0,
            board_selected_row: 0,
        }
    }

    fn tab_key(&self) -> u8 {
        match self.active_tab {
            DashboardTab::Dashboard => 0,
            DashboardTab::PerWorkspace => 1,
            DashboardTab::Spec => 2,
            DashboardTab::Board => 3,
            DashboardTab::DataSource => 4,
            DashboardTab::Scripts => 5,
        }
    }

    fn current_tab_state(&self) -> &TabState {
        self.tab_states.get(&self.tab_key()).unwrap()
    }

    fn current_tab_state_mut(&mut self) -> &mut TabState {
        let key = self.tab_key();
        self.tab_states.entry(key).or_default()
    }
}

/// Run the interactive TUI dashboard.
///
/// The dashboard refreshes every second and exits on `q` or `Esc`.
/// Use arrow keys or `j`/`k` to navigate items, `Tab`/`Shift+Tab` to switch
/// tabs, number keys `1`/`2`/`3` to jump to a tab, and `Enter` to open the
/// item detail overlay with transition timeline.
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
        // Collect data for all tabs.
        let all_items = db.list_items(None, None).unwrap_or_default();
        let running_items: Vec<_> = all_items
            .iter()
            .filter(|i| i.phase == QueuePhase::Running)
            .cloned()
            .collect();
        let recent_items = collect_recent_items(db);
        let workspaces = db.list_workspaces().unwrap_or_default();
        let specs = db.list_specs(None, None).unwrap_or_default();
        let datasource_entries = collect_datasource_status(&workspaces, &all_items);

        // Compute list length for navigation clamping.
        let active_list_len = match state.active_tab {
            DashboardTab::Dashboard => {
                let panel = state
                    .current_tab_state()
                    .active_panel
                    .unwrap_or(ActivePanel::Running);
                match panel {
                    ActivePanel::Running => running_items.len(),
                    ActivePanel::Recent => recent_items.len(),
                }
            }
            DashboardTab::PerWorkspace => {
                // Items filtered by selected workspace.
                if let Some(ws) = workspaces.get(state.selected_workspace) {
                    all_items.iter().filter(|i| i.workspace_id == ws.0).count()
                } else {
                    0
                }
            }
            DashboardTab::Spec => specs.len(),
            DashboardTab::Board => 0, // Board uses column/row navigation, not a single list.
            DashboardTab::DataSource => datasource_entries.len(),
            DashboardTab::Scripts => db
                .get_script_execution_stats()
                .map(|s| s.len())
                .unwrap_or(0),
        };

        // Clamp selected_index for non-Board tabs.
        if state.active_tab != DashboardTab::Board {
            let ts = state.current_tab_state_mut();
            if active_list_len == 0 {
                ts.selected_index = 0;
            } else if ts.selected_index >= active_list_len {
                ts.selected_index = active_list_len.saturating_sub(1);
            }
        }

        // Clamp workspace index.
        if !workspaces.is_empty() && state.selected_workspace >= workspaces.len() {
            state.selected_workspace = workspaces.len().saturating_sub(1);
        }

        // Build per-column items for Board view.
        let board_columns: Vec<Vec<&QueueItem>> = BOARD_COLUMNS
            .iter()
            .map(|phase| all_items.iter().filter(|i| i.phase == *phase).collect())
            .collect();

        // Clamp board selection.
        if state.board_selected_col >= BOARD_COLUMNS.len() {
            state.board_selected_col = 0;
        }
        let col_len = board_columns
            .get(state.board_selected_col)
            .map_or(0, |v| v.len());
        if col_len == 0 {
            state.board_selected_row = 0;
        } else if state.board_selected_row >= col_len {
            state.board_selected_row = col_len.saturating_sub(1);
        }

        terminal.draw(|frame| {
            // Tab bar at top.
            let outer_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(5)])
                .split(frame.area());

            let tab_bar = render_tab_bar(state.active_tab);
            frame.render_widget(tab_bar, outer_chunks[0]);

            match state.active_tab {
                DashboardTab::Dashboard => {
                    render_dashboard_tab(
                        frame,
                        db,
                        outer_chunks[1],
                        &running_items,
                        &recent_items,
                        &state,
                    );
                }
                DashboardTab::PerWorkspace => {
                    render_per_workspace_tab(frame, db, outer_chunks[1], &workspaces, &state);
                }
                DashboardTab::Spec => {
                    render_spec_tab(
                        frame,
                        outer_chunks[1],
                        &specs,
                        state.current_tab_state().selected_index,
                    );
                }
                DashboardTab::Board => {
                    render_board_tab(frame, outer_chunks[1], &board_columns, &state);
                }
                DashboardTab::DataSource => {
                    render_datasource_tab(
                        frame,
                        outer_chunks[1],
                        &datasource_entries,
                        state.current_tab_state().selected_index,
                    );
                }
                DashboardTab::Scripts => {
                    render_scripts_tab(
                        frame,
                        db,
                        outer_chunks[1],
                        state.current_tab_state().selected_index,
                    );
                }
            }

            // Overlays (rendered on top of everything).
            if let Some(ref work_id) = state.overlay_item {
                render_item_detail_overlay(frame, db, work_id);
            }
            if state.show_help {
                render_help_overlay(frame);
            }
        })?;

        // Poll for keyboard events with 1 second timeout.
        if event::poll(Duration::from_secs(1))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            // Help overlay: close on any key.
            if state.show_help {
                state.show_help = false;
                continue;
            }

            // Item detail overlay: close on q/Esc.
            if state.overlay_item.is_some() {
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                    state.overlay_item = None;
                }
                continue;
            }

            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                // Tab switching keys: letter keys for direct jump.
                KeyCode::Char('d') => {
                    state.active_tab = DashboardTab::Dashboard;
                }
                KeyCode::Char('s') => {
                    state.active_tab = DashboardTab::Spec;
                }
                KeyCode::Char('w') => {
                    state.active_tab = DashboardTab::PerWorkspace;
                }
                KeyCode::Char('b') => {
                    state.active_tab = DashboardTab::Board;
                }
                KeyCode::Char('n') => {
                    state.active_tab = DashboardTab::DataSource;
                }
                KeyCode::Char('x') => {
                    state.active_tab = DashboardTab::Scripts;
                }
                KeyCode::Char('h') => {
                    state.show_help = true;
                }
                // Tab/Shift+Tab to cycle through tabs.
                KeyCode::Tab => {
                    state.active_tab = state.active_tab.next();
                }
                KeyCode::BackTab => {
                    state.active_tab = state.active_tab.prev();
                }
                // Navigation within the current tab.
                KeyCode::Up | KeyCode::Char('k') => {
                    handle_nav_up(&mut state, &board_columns);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    handle_nav_down(&mut state, active_list_len, &board_columns);
                }
                KeyCode::Left => {
                    handle_nav_left(&mut state, &workspaces);
                }
                KeyCode::Right => {
                    handle_nav_right(&mut state, &workspaces, &board_columns);
                }

                KeyCode::Enter => {
                    handle_enter(
                        &mut state,
                        &running_items,
                        &recent_items,
                        &all_items,
                        &workspaces,
                        db,
                        &board_columns,
                    );
                }
                _ => {}
            }
        }
    }
}

/// Handle Up/k navigation.
fn handle_nav_up(state: &mut DashboardState, board_columns: &[Vec<&QueueItem>]) {
    match state.active_tab {
        DashboardTab::Board => {
            state.board_selected_row = state.board_selected_row.saturating_sub(1);
        }
        _ => {
            let ts = state.current_tab_state_mut();
            ts.selected_index = ts.selected_index.saturating_sub(1);
        }
    }
    let _ = board_columns; // suppress unused warning in non-Board paths
}

/// Handle Down/j navigation.
fn handle_nav_down(
    state: &mut DashboardState,
    active_list_len: usize,
    board_columns: &[Vec<&QueueItem>],
) {
    match state.active_tab {
        DashboardTab::Board => {
            let col_len = board_columns
                .get(state.board_selected_col)
                .map_or(0, |v| v.len());
            if col_len > 0 {
                state.board_selected_row =
                    (state.board_selected_row + 1).min(col_len.saturating_sub(1));
            }
        }
        _ => {
            if active_list_len > 0 {
                let ts = state.current_tab_state_mut();
                ts.selected_index = (ts.selected_index + 1).min(active_list_len.saturating_sub(1));
            }
        }
    }
}

/// Handle Left arrow navigation.
fn handle_nav_left(state: &mut DashboardState, workspaces: &[(String, String, String)]) {
    match state.active_tab {
        DashboardTab::Dashboard => {
            let ts = state.current_tab_state_mut();
            ts.active_panel = Some(ActivePanel::Running);
            ts.selected_index = 0;
        }
        DashboardTab::PerWorkspace => {
            if state.selected_workspace > 0 {
                state.selected_workspace -= 1;
                state.current_tab_state_mut().selected_index = 0;
            }
        }
        DashboardTab::Board => {
            state.board_selected_col = state.board_selected_col.saturating_sub(1);
            state.board_selected_row = 0;
        }
        DashboardTab::Spec | DashboardTab::DataSource | DashboardTab::Scripts => {}
    }
    let _ = workspaces;
}

/// Handle Right arrow navigation.
fn handle_nav_right(
    state: &mut DashboardState,
    workspaces: &[(String, String, String)],
    board_columns: &[Vec<&QueueItem>],
) {
    match state.active_tab {
        DashboardTab::Dashboard => {
            let ts = state.current_tab_state_mut();
            ts.active_panel = Some(ActivePanel::Recent);
            ts.selected_index = 0;
        }
        DashboardTab::PerWorkspace => {
            if !workspaces.is_empty() && state.selected_workspace < workspaces.len() - 1 {
                state.selected_workspace += 1;
                state.current_tab_state_mut().selected_index = 0;
            }
        }
        DashboardTab::Board => {
            if state.board_selected_col < board_columns.len().saturating_sub(1) {
                state.board_selected_col += 1;
                state.board_selected_row = 0;
            }
        }
        DashboardTab::Spec | DashboardTab::DataSource | DashboardTab::Scripts => {}
    }
}

/// Handle Enter key to open item detail overlay.
fn handle_enter(
    state: &mut DashboardState,
    running_items: &[QueueItem],
    recent_items: &[QueueItem],
    all_items: &[QueueItem],
    workspaces: &[(String, String, String)],
    db: &Database,
    board_columns: &[Vec<&QueueItem>],
) {
    match state.active_tab {
        DashboardTab::Dashboard => {
            let panel = state
                .current_tab_state()
                .active_panel
                .unwrap_or(ActivePanel::Running);
            let idx = state.current_tab_state().selected_index;
            let items: &[QueueItem] = match panel {
                ActivePanel::Running => running_items,
                ActivePanel::Recent => recent_items,
            };
            if let Some(item) = items.get(idx) {
                state.overlay_item = Some(item.work_id.clone());
            }
        }
        DashboardTab::PerWorkspace => {
            if let Some(ws) = workspaces.get(state.selected_workspace) {
                let ws_items = db.list_items(None, Some(&ws.0)).unwrap_or_default();
                let idx = state.current_tab_state().selected_index;
                if let Some(item) = ws_items.get(idx) {
                    state.overlay_item = Some(item.work_id.clone());
                }
            }
        }
        DashboardTab::Spec | DashboardTab::DataSource | DashboardTab::Scripts => {
            // No overlay on Enter for Spec/DataSource/Scripts tabs.
        }
        DashboardTab::Board => {
            if let Some(col) = board_columns.get(state.board_selected_col)
                && let Some(item) = col.get(state.board_selected_row)
            {
                state.overlay_item = Some(item.work_id.clone());
            }
        }
    }
    let _ = all_items;
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the tab bar showing available tabs with the active one highlighted.
fn render_tab_bar(active: DashboardTab) -> Paragraph<'static> {
    let tabs = [
        ("d", "Dashboard", DashboardTab::Dashboard),
        ("w", "Workspace", DashboardTab::PerWorkspace),
        ("s", "Spec", DashboardTab::Spec),
        ("b", "Board", DashboardTab::Board),
        ("n", "DataSource", DashboardTab::DataSource),
        ("x", "Scripts", DashboardTab::Scripts),
    ];

    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, (key, label, tab)) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if *tab == active {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(format!("[{key}] {label}"), style));
    }

    spans.push(Span::raw("    "));
    spans.push(Span::styled(
        "[h] Help  [Tab/Shift+Tab] Cycle",
        Style::default().fg(Color::DarkGray),
    ));

    Paragraph::new(Line::from(spans)).block(
        Block::default()
            .title(" Belt TUI ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    )
}

/// Render the Board tab as a kanban-style view with columns per phase.
fn render_board_tab(
    frame: &mut ratatui::Frame,
    area: Rect,
    board_columns: &[Vec<&QueueItem>],
    state: &DashboardState,
) {
    let num_cols = BOARD_COLUMNS.len();
    let constraints: Vec<Constraint> = (0..num_cols)
        .map(|_| Constraint::Ratio(1, num_cols as u32))
        .collect();

    let col_areas = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    for (col_idx, phase) in BOARD_COLUMNS.iter().enumerate() {
        let items = &board_columns[col_idx];
        let is_selected_col = col_idx == state.board_selected_col;
        let phase_str = phase.as_str();
        let color = phase_color(phase_str);

        let mut lines: Vec<Line<'static>> = Vec::new();

        for (row_idx, item) in items.iter().enumerate() {
            let is_selected = is_selected_col && row_idx == state.board_selected_row;
            let work_id_display =
                truncate_str(&item.work_id, col_areas[col_idx].width as usize - 4);

            if is_selected {
                lines.push(Line::from(Span::styled(
                    format!("> {work_id_display}"),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    format!("  {work_id_display}"),
                    Style::default().fg(Color::White),
                )));
            }
        }

        if items.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (empty)",
                Style::default().fg(Color::DarkGray),
            )));
        }

        let border_color = if is_selected_col {
            color
        } else {
            Color::DarkGray
        };
        let title = format!(" {} ({}) ", phase_str, items.len());

        let paragraph = Paragraph::new(lines).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        );

        frame.render_widget(paragraph, col_areas[col_idx]);
    }
}

/// Collect DataSource status entries from workspace configurations.
///
/// For each workspace, loads its config file and extracts configured data sources.
/// Determines connection status based on whether the workspace has active queue items
/// from that source.
fn collect_datasource_status(
    workspaces: &[(String, String, String)],
    all_items: &[QueueItem],
) -> Vec<DataSourceStatusEntry> {
    let mut entries = Vec::new();

    for (ws_name, config_path, _created_at) in workspaces {
        let config = match load_workspace_config(Path::new(config_path)) {
            Ok(c) => c,
            Err(_) => {
                // Config could not be loaded — report error status.
                entries.push(DataSourceStatusEntry {
                    workspace: ws_name.clone(),
                    source_name: "(unknown)".to_string(),
                    url: config_path.clone(),
                    state_count: 0,
                    scan_interval_secs: 0,
                    status: DataSourceConnectionStatus::Error,
                    active_item_count: 0,
                });
                continue;
            }
        };

        for (source_name, source_config) in &config.sources {
            // Count active (non-terminal) items from this workspace.
            let active_count = all_items
                .iter()
                .filter(|item| {
                    item.workspace_id == *ws_name
                        && !matches!(
                            item.phase,
                            QueuePhase::Done | QueuePhase::Failed | QueuePhase::Completed
                        )
                })
                .count();

            let status = if active_count > 0 {
                DataSourceConnectionStatus::Connected
            } else {
                DataSourceConnectionStatus::Disconnected
            };

            entries.push(DataSourceStatusEntry {
                workspace: ws_name.clone(),
                source_name: source_name.clone(),
                url: source_config.url.clone(),
                state_count: source_config.states.len(),
                scan_interval_secs: source_config.scan_interval_secs,
                status,
                active_item_count: active_count,
            });
        }
    }

    entries
}

/// Render the DataSource status tab showing connection status for all configured sources.
///
/// Layout:
/// - Summary bar (top): count of connected/disconnected/error sources
/// - DataSource table (bottom): detailed per-source status
fn render_datasource_tab(
    frame: &mut ratatui::Frame,
    area: Rect,
    entries: &[DataSourceStatusEntry],
    selected_index: usize,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(8)])
        .split(area);

    // Summary bar.
    let connected = entries
        .iter()
        .filter(|e| e.status == DataSourceConnectionStatus::Connected)
        .count();
    let disconnected = entries
        .iter()
        .filter(|e| e.status == DataSourceConnectionStatus::Disconnected)
        .count();
    let error = entries
        .iter()
        .filter(|e| e.status == DataSourceConnectionStatus::Error)
        .count();

    let summary_spans = vec![
        Span::styled(
            format!(" {} Connected  ", connected),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!(" {} Disconnected  ", disconnected),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!(" {} Error ", error),
            Style::default().fg(Color::Red),
        ),
    ];

    let summary = Paragraph::new(Line::from(summary_spans)).block(
        Block::default()
            .title(" DataSource Status Summary ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(summary, chunks[0]);

    // DataSource table.
    let header = Row::new(vec![
        Cell::from("Status"),
        Cell::from("Workspace"),
        Cell::from("Source"),
        Cell::from("URL"),
        Cell::from("States"),
        Cell::from("Interval"),
        Cell::from("Active"),
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let is_selected = i == selected_index;
            let style = if is_selected {
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::DarkGray)
            } else {
                Style::default()
            };

            let status_style = Style::default().fg(entry.status.color());
            let indicator = format!("{} {}", entry.status.indicator(), entry.status.label());

            let interval_display = if entry.scan_interval_secs >= 60 {
                format!("{}m", entry.scan_interval_secs / 60)
            } else {
                format!("{}s", entry.scan_interval_secs)
            };

            Row::new(vec![
                Cell::from(indicator).style(status_style),
                Cell::from(entry.workspace.clone()),
                Cell::from(entry.source_name.clone()),
                Cell::from(truncate_str(&entry.url, 40)),
                Cell::from(entry.state_count.to_string()),
                Cell::from(interval_display),
                Cell::from(entry.active_item_count.to_string()),
            ])
            .style(style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(18),
            Constraint::Length(12),
            Constraint::Min(20),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(8),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(format!(" DataSource ({}) ", entries.len()))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    frame.render_widget(table, chunks[1]);
}

/// Truncate a string to fit within a given width.
fn truncate_str(s: &str, max_width: usize) -> String {
    if max_width < 4 {
        return s.chars().take(max_width).collect();
    }
    if s.len() <= max_width {
        s.to_string()
    } else {
        let mut result: String = s.chars().take(max_width - 2).collect();
        result.push_str("..");
        result
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

/// Render the main dashboard tab content (phase summary + running/recent items).
fn render_dashboard_tab(
    frame: &mut ratatui::Frame,
    db: &Database,
    area: Rect,
    running_items: &[QueueItem],
    recent_items: &[QueueItem],
    state: &DashboardState,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(8),
            Constraint::Length(10),
            Constraint::Length(10),
        ])
        .split(area);

    let summary = render_phase_summary(db);
    frame.render_widget(summary, chunks[0]);

    let tab_state = state.current_tab_state();
    let active_panel = tab_state.active_panel.unwrap_or(ActivePanel::Running);

    let running_table = render_running_items_stateful(
        running_items,
        active_panel == ActivePanel::Running,
        tab_state.selected_index,
    );
    frame.render_widget(running_table, chunks[1]);

    let recent_table = render_recent_items_stateful(
        recent_items,
        active_panel == ActivePanel::Recent,
        tab_state.selected_index,
    );
    frame.render_widget(recent_table, chunks[2]);

    let runtime_widget = render_runtime_panel_tui(db);
    frame.render_widget(runtime_widget, chunks[3]);
}

/// Render the per-workspace tab showing items filtered by the selected workspace.
///
/// Layout:
/// - Workspace selector bar (top)
/// - Workspace phase summary + spec progress side by side (middle)
/// - Items table for the selected workspace (bottom)
fn render_per_workspace_tab(
    frame: &mut ratatui::Frame,
    db: &Database,
    area: Rect,
    workspaces: &[(String, String, String)],
    state: &DashboardState,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Min(5),
        ])
        .split(area);

    // Workspace selector bar.
    if workspaces.is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            "(no workspaces registered)",
            Style::default().fg(Color::DarkGray),
        )))
        .block(
            Block::default()
                .title(" Workspaces [<-/->] ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );
        frame.render_widget(msg, chunks[0]);
        return;
    }

    let mut ws_spans: Vec<Span<'static>> = Vec::new();
    for (i, ws) in workspaces.iter().enumerate() {
        if i > 0 {
            ws_spans.push(Span::raw("  "));
        }
        let style = if i == state.selected_workspace {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        ws_spans.push(Span::styled(ws.0.clone(), style));
    }

    let ws_bar = Paragraph::new(Line::from(ws_spans)).block(
        Block::default()
            .title(" Workspaces [<-/->] ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(ws_bar, chunks[0]);

    // Items for the selected workspace.
    let selected_ws = &workspaces[state.selected_workspace].0;
    let ws_items = db.list_items(None, Some(selected_ws)).unwrap_or_default();

    // Workspace summary: phase counts + spec progress side by side.
    let summary_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    render_workspace_phase_summary(frame, summary_cols[0], &ws_items);
    render_workspace_spec_progress(frame, db, summary_cols[1], selected_ws);

    // Item table.
    let selected_index = state.current_tab_state().selected_index;

    let rows: Vec<Row<'static>> = ws_items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let phase_str = item.phase.as_str().to_string();
            let color = phase_color(&phase_str);
            let row = Row::new(vec![
                Cell::from(item.work_id.clone()),
                Cell::from(phase_str).style(Style::default().fg(color)),
                Cell::from(item.state.clone()),
                Cell::from(item.updated_at.clone()),
            ]);
            if i == selected_index {
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

    let header = Row::new(vec!["Work ID", "Phase", "State", "Updated"])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(1);

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(30),
            Constraint::Percentage(15),
            Constraint::Percentage(25),
            Constraint::Percentage(30),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(format!(" Items [{selected_ws}] "))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green)),
    );
    frame.render_widget(table, chunks[2]);
}

/// Render a compact phase summary for a single workspace's items.
fn render_workspace_phase_summary(frame: &mut ratatui::Frame, area: Rect, ws_items: &[QueueItem]) {
    let mut phase_counts: HashMap<String, usize> = HashMap::new();
    for item in ws_items {
        *phase_counts
            .entry(item.phase.as_str().to_string())
            .or_insert(0) += 1;
    }

    let all_phases = [
        "pending",
        "ready",
        "running",
        "completed",
        "done",
        "hitl",
        "failed",
    ];

    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, phase) in all_phases.iter().enumerate() {
        let count = phase_counts.get(*phase).copied().unwrap_or(0);
        let color = phase_color(phase);
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!("{phase}:{count}"),
            Style::default().fg(color),
        ));
    }

    let total = ws_items.len();
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("total:{total}"),
        Style::default().add_modifier(Modifier::BOLD),
    ));

    let summary = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .title(" Phase Summary ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(summary, area);
}

/// Render spec progress for a single workspace.
fn render_workspace_spec_progress(
    frame: &mut ratatui::Frame,
    db: &Database,
    area: Rect,
    workspace_id: &str,
) {
    let specs = db.list_specs(Some(workspace_id), None).unwrap_or_default();
    let total = specs.len();
    let completed = specs
        .iter()
        .filter(|s| s.status == SpecStatus::Completed)
        .count();
    let active = specs
        .iter()
        .filter(|s| s.status == SpecStatus::Active)
        .count();
    let progress_pct = if total > 0 {
        (completed as f64 / total as f64) * 100.0
    } else {
        0.0
    };

    let bar_width = 15usize;
    let filled = if total > 0 {
        (completed * bar_width) / total
    } else {
        0
    };
    let empty = bar_width.saturating_sub(filled);

    let line = Line::from(vec![
        Span::styled(
            format!("{total} specs"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(format!("active:{active}"), Style::default().fg(Color::Blue)),
        Span::raw(" "),
        Span::styled(
            format!("done:{completed}"),
            Style::default().fg(Color::Green),
        ),
        Span::raw("  ["),
        Span::styled("#".repeat(filled), Style::default().fg(Color::Green)),
        Span::styled("-".repeat(empty), Style::default().fg(Color::DarkGray)),
        Span::raw("]"),
        Span::styled(
            format!(" {progress_pct:.0}%"),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let paragraph = Paragraph::new(line).block(
        Block::default()
            .title(" Spec Progress ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta)),
    );
    frame.render_widget(paragraph, area);
}

/// Render the spec progress tab showing all specs with status and progress.
fn render_spec_tab(
    frame: &mut ratatui::Frame,
    area: Rect,
    specs: &[belt_core::spec::Spec],
    selected: usize,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(5)])
        .split(area);

    // Spec status summary (progress overview).
    let status_counts = count_spec_statuses(specs);
    let total = specs.len();
    let completed = status_counts.completed;
    let progress_pct = if total > 0 {
        (completed as f64 / total as f64) * 100.0
    } else {
        0.0
    };

    let summary_spans = vec![
        Span::styled(
            format!("Total: {total}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("draft: {}", status_counts.draft),
            Style::default().fg(Color::Gray),
        ),
        Span::raw("  "),
        Span::styled(
            format!("active: {}", status_counts.active),
            Style::default().fg(Color::Blue),
        ),
        Span::raw("  "),
        Span::styled(
            format!("paused: {}", status_counts.paused),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("  "),
        Span::styled(
            format!("completing: {}", status_counts.completing),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("  "),
        Span::styled(
            format!("completed: {completed}"),
            Style::default().fg(Color::Green),
        ),
        Span::raw("  "),
        Span::styled(
            format!("Progress: {progress_pct:.0}%"),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
    ];

    // Build a progress bar line.
    let bar_width = 30usize;
    let filled = if total > 0 {
        (completed * bar_width) / total
    } else {
        0
    };
    let empty = bar_width.saturating_sub(filled);
    let bar_line = Line::from(vec![
        Span::raw("  ["),
        Span::styled("#".repeat(filled), Style::default().fg(Color::Green)),
        Span::styled("-".repeat(empty), Style::default().fg(Color::DarkGray)),
        Span::raw("]"),
    ]);

    let summary = Paragraph::new(vec![Line::from(summary_spans), Line::from(""), bar_line]).block(
        Block::default()
            .title(" Spec Progress ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta)),
    );
    frame.render_widget(summary, chunks[0]);

    // Spec list table.
    let rows: Vec<Row<'static>> = specs
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let status_str = spec.status.as_str().to_string();
            let color = spec_status_color(&status_str);
            let ws = spec.workspace_id.clone();
            let row = Row::new(vec![
                Cell::from(spec.name.clone()),
                Cell::from(ws),
                Cell::from(status_str).style(Style::default().fg(color)),
                Cell::from(spec.updated_at.clone()),
            ]);
            if i == selected {
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

    let header = Row::new(vec!["Name", "Workspace", "Status", "Updated"])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(1);

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(30),
            Constraint::Percentage(20),
            Constraint::Percentage(15),
            Constraint::Percentage(35),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(" Specs ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta)),
    );
    frame.render_widget(table, chunks[1]);
}

/// Render the scripts execution statistics tab.
///
/// Layout:
/// - Summary bar with overall success rate (top)
/// - Per-script statistics table (middle)
/// - Recent 10 script executions log (bottom)
fn render_scripts_tab(frame: &mut ratatui::Frame, db: &Database, area: Rect, selected: usize) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(8),
            Constraint::Length(14),
        ])
        .split(area);

    let script_stats = db.get_script_execution_stats().unwrap_or_default();
    let recent_execs = db.get_recent_script_executions(10).unwrap_or_default();

    // -- Summary bar --
    render_scripts_summary(frame, chunks[0], &script_stats);

    // -- Per-script stats table --
    render_scripts_stats_table(frame, chunks[1], &script_stats, selected);

    // -- Recent executions log --
    render_recent_executions(frame, chunks[2], &recent_execs);
}

/// Render overall scripts execution summary.
fn render_scripts_summary(frame: &mut ratatui::Frame, area: Rect, stats: &[ScriptExecStats]) {
    let total_runs: u64 = stats.iter().map(|s| s.total_runs).sum();
    let total_success: u64 = stats.iter().map(|s| s.success_count).sum();
    let total_fail: u64 = stats.iter().map(|s| s.fail_count).sum();
    let overall_rate = if total_runs > 0 {
        (total_success as f64 / total_runs as f64) * 100.0
    } else {
        0.0
    };

    let rate_color = if overall_rate >= 90.0 {
        Color::Green
    } else if overall_rate >= 70.0 {
        Color::Yellow
    } else {
        Color::Red
    };

    let line1 = Line::from(vec![
        Span::styled(
            format!("Total Runs: {total_runs}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("Success: {total_success}"),
            Style::default().fg(Color::Green),
        ),
        Span::raw("  "),
        Span::styled(
            format!("Failed: {total_fail}"),
            Style::default().fg(Color::Red),
        ),
        Span::raw("  "),
        Span::styled(
            format!("Rate: {overall_rate:.1}%"),
            Style::default().fg(rate_color).add_modifier(Modifier::BOLD),
        ),
    ]);

    let unique_scripts = stats.len();
    let line2 = Line::from(vec![Span::styled(
        format!("Scripts: {unique_scripts}"),
        Style::default().fg(Color::Cyan),
    )]);

    let paragraph = Paragraph::new(vec![line1, Line::from(""), line2]).block(
        Block::default()
            .title(" Scripts Execution Summary ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(paragraph, area);
}

/// Render per-script statistics as a table.
fn render_scripts_stats_table(
    frame: &mut ratatui::Frame,
    area: Rect,
    stats: &[ScriptExecStats],
    selected: usize,
) {
    let rows: Vec<Row<'static>> = stats
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let rate_color = if s.success_rate >= 90.0 {
                Color::Green
            } else if s.success_rate >= 70.0 {
                Color::Yellow
            } else {
                Color::Red
            };

            let avg_dur = s
                .avg_duration_ms
                .map_or_else(|| "-".to_string(), |d| format!("{d:.0}"));

            let row = Row::new(vec![
                Cell::from(s.state.clone()),
                Cell::from(s.total_runs.to_string()),
                Cell::from(s.success_count.to_string()).style(Style::default().fg(Color::Green)),
                Cell::from(s.fail_count.to_string()).style(Style::default().fg(Color::Red)),
                Cell::from(format!("{:.1}%", s.success_rate))
                    .style(Style::default().fg(rate_color)),
                Cell::from(avg_dur),
            ]);

            if i == selected {
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

    let header = Row::new(vec!["Script", "Runs", "OK", "Fail", "Rate", "Avg ms"])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(1);

    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(" Per-Script Statistics ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green)),
    );
    frame.render_widget(table, area);
}

/// Render recent script execution log.
fn render_recent_executions(frame: &mut ratatui::Frame, area: Rect, events: &[HistoryEvent]) {
    let rows: Vec<Row<'static>> = events
        .iter()
        .map(|e| {
            let status_color = if e.status == "success" {
                Color::Green
            } else {
                Color::Red
            };
            Row::new(vec![
                Cell::from(e.state.clone()),
                Cell::from(e.work_id.clone()),
                Cell::from(e.status.clone()).style(Style::default().fg(status_color)),
                Cell::from(e.attempt.to_string()),
                Cell::from(format_transition_time(&e.created_at)),
            ])
        })
        .collect();

    let header = Row::new(vec!["Script", "Work ID", "Status", "Attempt", "Time"])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(1);

    let table = Table::new(
        rows,
        [
            Constraint::Min(15),
            Constraint::Min(20),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(22),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(" Recent Executions (last 10) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta)),
    );
    frame.render_widget(table, area);
}

/// Aggregate counts of spec statuses.
struct SpecStatusCounts {
    draft: usize,
    active: usize,
    paused: usize,
    completing: usize,
    completed: usize,
}

fn count_spec_statuses(specs: &[belt_core::spec::Spec]) -> SpecStatusCounts {
    let mut counts = SpecStatusCounts {
        draft: 0,
        active: 0,
        paused: 0,
        completing: 0,
        completed: 0,
    };
    for spec in specs {
        match spec.status {
            SpecStatus::Draft => counts.draft += 1,
            SpecStatus::Active => counts.active += 1,
            SpecStatus::Paused => counts.paused += 1,
            SpecStatus::Completing => counts.completing += 1,
            SpecStatus::Completed => counts.completed += 1,
            SpecStatus::Archived => {} // excluded from list by default
        }
    }
    counts
}

/// Map spec status strings to colors.
fn spec_status_color(status: &str) -> Color {
    match status {
        "draft" => Color::Gray,
        "active" => Color::Blue,
        "paused" => Color::Yellow,
        "completing" => Color::Cyan,
        "completed" => Color::Green,
        "archived" => Color::DarkGray,
        _ => Color::White,
    }
}

/// Render the help overlay showing all key bindings.
fn render_help_overlay(frame: &mut ratatui::Frame) {
    let area = centered_rect(50, 60, frame.area());
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::from(Span::styled(
            "Key Bindings",
            Style::default()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  d       ", Style::default().fg(Color::Yellow)),
            Span::raw("Switch to Dashboard tab"),
        ]),
        Line::from(vec![
            Span::styled("  w       ", Style::default().fg(Color::Yellow)),
            Span::raw("Switch to Per-Workspace tab"),
        ]),
        Line::from(vec![
            Span::styled("  s       ", Style::default().fg(Color::Yellow)),
            Span::raw("Switch to Spec progress tab"),
        ]),
        Line::from(vec![
            Span::styled("  b       ", Style::default().fg(Color::Yellow)),
            Span::raw("Switch to Board (kanban) tab"),
        ]),
        Line::from(vec![
            Span::styled("  n       ", Style::default().fg(Color::Yellow)),
            Span::raw("Switch to DataSource status tab"),
        ]),
        Line::from(vec![
            Span::styled("  x       ", Style::default().fg(Color::Yellow)),
            Span::raw("Switch to Scripts statistics tab"),
        ]),
        Line::from(vec![
            Span::styled("  h       ", Style::default().fg(Color::Yellow)),
            Span::raw("Show this help"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  j/Down  ", Style::default().fg(Color::Cyan)),
            Span::raw("Move selection down"),
        ]),
        Line::from(vec![
            Span::styled("  k/Up    ", Style::default().fg(Color::Cyan)),
            Span::raw("Move selection up"),
        ]),
        Line::from(vec![
            Span::styled("  Left    ", Style::default().fg(Color::Cyan)),
            Span::raw("Previous workspace (Workspace tab)"),
        ]),
        Line::from(vec![
            Span::styled("  Right   ", Style::default().fg(Color::Cyan)),
            Span::raw("Next workspace (Workspace tab)"),
        ]),
        Line::from(vec![
            Span::styled("  Tab     ", Style::default().fg(Color::Cyan)),
            Span::raw("Cycle to next tab (Shift+Tab: previous)"),
        ]),
        Line::from(vec![
            Span::styled("  Enter   ", Style::default().fg(Color::Cyan)),
            Span::raw("Open item detail overlay"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  q/Esc   ", Style::default().fg(Color::Red)),
            Span::raw("Quit / close overlay"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Press any key to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .title(" Help ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(paragraph, area);
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

/// Return the ratatui [`Color`] associated with a queue phase name.
///
/// Used by the dashboard TUI and the `belt status --format rich` output.
pub fn phase_color(phase: &str) -> Color {
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

/// Render the runtime statistics as a ratatui [`Table`] widget for the TUI dashboard.
///
/// Fetches runtime stats from the database and displays token totals, execution
/// count, average duration, and a per-model breakdown inside a bordered panel.
fn render_runtime_panel_tui(db: &Database) -> Table<'static> {
    let stats = db.get_runtime_stats().ok();

    let header = Row::new(vec![
        Cell::from("Model"),
        Cell::from("Input"),
        Cell::from("Output"),
        Cell::from("Total"),
        Cell::from("Runs"),
        Cell::from("Avg ms"),
    ])
    .style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );

    let mut rows: Vec<Row<'static>> = Vec::new();

    if let Some(ref stats) = stats {
        // Summary row with overall totals.
        let avg = stats
            .avg_duration_ms
            .map_or_else(|| "-".to_string(), |d| format!("{d:.0}"));
        rows.push(
            Row::new(vec![
                Cell::from("(all)"),
                Cell::from(format_number(stats.total_tokens_input)),
                Cell::from(format_number(stats.total_tokens_output)),
                Cell::from(format_number(stats.total_tokens)),
                Cell::from(stats.executions.to_string()),
                Cell::from(avg),
            ])
            .style(Style::default().add_modifier(Modifier::BOLD)),
        );

        // Per-model rows sorted by total_tokens descending.
        let mut models: Vec<_> = stats.by_model.values().collect();
        models.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));

        for m in models {
            let avg = m
                .avg_duration_ms
                .map_or_else(|| "-".to_string(), |d| format!("{d:.0}"));
            rows.push(Row::new(vec![
                Cell::from(m.model.clone()),
                Cell::from(format_number(m.input_tokens)),
                Cell::from(format_number(m.output_tokens)),
                Cell::from(format_number(m.total_tokens)),
                Cell::from(m.executions.to_string()),
                Cell::from(avg),
            ]));
        }
    }

    Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(6),
            Constraint::Length(10),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(" Runtime Stats (24h) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta)),
    )
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

    // ---- DashboardState ----

    #[test]
    fn active_panel_default_is_running() {
        let state = DashboardState::new();
        let ts = state.current_tab_state();
        assert_eq!(ts.active_panel, Some(ActivePanel::Running));
        assert_eq!(ts.selected_index, 0);
        assert!(state.overlay_item.is_none());
    }

    // ---- collect_recent_items (DB-backed) ----

    fn make_db() -> Database {
        Database::open_in_memory().expect("in-memory DB should open")
    }

    fn make_item_with_phase(work_id: &str, phase: QueuePhase, updated_at: &str) -> QueueItem {
        let mut item = QueueItem::new(
            work_id.to_string(),
            "src".to_string(),
            "ws".to_string(),
            "s".to_string(),
        );
        item.phase = phase;
        item.updated_at = updated_at.to_string();
        item
    }

    #[test]
    fn collect_recent_items_empty_db() {
        let db = make_db();
        let items = collect_recent_items(&db);
        assert!(items.is_empty());
    }

    #[test]
    fn collect_recent_items_only_collects_done_failed_completed() {
        let db = make_db();

        // Insert items of various phases.
        let pending =
            make_item_with_phase("w-pending", QueuePhase::Pending, "2026-03-25T01:00:00Z");
        let running =
            make_item_with_phase("w-running", QueuePhase::Running, "2026-03-25T02:00:00Z");
        let done = make_item_with_phase("w-done", QueuePhase::Done, "2026-03-25T03:00:00Z");
        let failed = make_item_with_phase("w-failed", QueuePhase::Failed, "2026-03-25T04:00:00Z");
        let completed =
            make_item_with_phase("w-completed", QueuePhase::Completed, "2026-03-25T05:00:00Z");
        let ready = make_item_with_phase("w-ready", QueuePhase::Ready, "2026-03-25T06:00:00Z");

        for item in [&pending, &running, &done, &failed, &completed, &ready] {
            db.insert_item(item).unwrap();
        }

        let recent = collect_recent_items(&db);

        // Only Done, Failed, Completed should be collected.
        assert_eq!(recent.len(), 3);
        let ids: Vec<&str> = recent.iter().map(|i| i.work_id.as_str()).collect();
        assert!(ids.contains(&"w-done"));
        assert!(ids.contains(&"w-failed"));
        assert!(ids.contains(&"w-completed"));
        assert!(!ids.contains(&"w-pending"));
        assert!(!ids.contains(&"w-running"));
        assert!(!ids.contains(&"w-ready"));
    }

    #[test]
    fn collect_recent_items_sorted_by_updated_at_descending() {
        let db = make_db();

        let older = make_item_with_phase("w-old", QueuePhase::Done, "2026-03-25T01:00:00Z");
        let middle = make_item_with_phase("w-mid", QueuePhase::Failed, "2026-03-25T05:00:00Z");
        let newest = make_item_with_phase("w-new", QueuePhase::Completed, "2026-03-25T10:00:00Z");

        for item in [&older, &middle, &newest] {
            db.insert_item(item).unwrap();
        }

        let recent = collect_recent_items(&db);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].work_id, "w-new");
        assert_eq!(recent[1].work_id, "w-mid");
        assert_eq!(recent[2].work_id, "w-old");
    }

    #[test]
    fn collect_recent_items_truncates_to_8() {
        let db = make_db();

        // Insert 12 Done items.
        for i in 0..12 {
            let item = make_item_with_phase(
                &format!("w-{i}"),
                QueuePhase::Done,
                &format!("2026-03-25T{:02}:00:00Z", i),
            );
            db.insert_item(&item).unwrap();
        }

        let recent = collect_recent_items(&db);
        assert_eq!(recent.len(), 8);
        // The most recent (highest hour) should come first.
        assert_eq!(recent[0].work_id, "w-11");
        assert_eq!(recent[7].work_id, "w-4");
    }

    // ---- render_item_detail_overlay ----

    #[test]
    fn render_item_detail_overlay_no_panic_item_exists() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let db = make_db();
        let item = QueueItem::new(
            "w1".to_string(),
            "src1".to_string(),
            "ws1".to_string(),
            "analyze".to_string(),
        );
        db.insert_item(&item).unwrap();

        let backend = TestBackend::new(80, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_item_detail_overlay(frame, &db, "w1");
            })
            .unwrap();

        // Verify the overlay rendered content containing "Item Details".
        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("Item Details"));
        assert!(text.contains("w1"));
    }

    #[test]
    fn render_item_detail_overlay_no_panic_item_missing() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let db = make_db();

        let backend = TestBackend::new(80, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_item_detail_overlay(frame, &db, "nonexistent");
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("item not found"));
    }

    #[test]
    fn render_item_detail_overlay_no_panic_zero_size_frame() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let db = make_db();

        // A very small terminal should not cause a panic.
        let backend = TestBackend::new(5, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_item_detail_overlay(frame, &db, "any-id");
            })
            .unwrap();
    }

    // ---- DashboardState new defaults ----

    #[test]
    fn dashboard_state_defaults() {
        let state = DashboardState::new();
        assert_eq!(state.active_tab, DashboardTab::Dashboard);
        assert!(!state.show_help);
        assert_eq!(state.selected_workspace, 0);
        assert_eq!(state.board_selected_col, 0);
        assert_eq!(state.board_selected_row, 0);
    }

    // ---- DashboardTab cycling ----

    #[test]
    fn tab_cycle_forward() {
        assert_eq!(DashboardTab::Dashboard.next(), DashboardTab::PerWorkspace);
        assert_eq!(DashboardTab::PerWorkspace.next(), DashboardTab::Spec);
        assert_eq!(DashboardTab::Spec.next(), DashboardTab::Board);
        assert_eq!(DashboardTab::Board.next(), DashboardTab::DataSource);
        assert_eq!(DashboardTab::DataSource.next(), DashboardTab::Scripts);
        assert_eq!(DashboardTab::Scripts.next(), DashboardTab::Dashboard);
    }

    #[test]
    fn tab_cycle_backward() {
        assert_eq!(DashboardTab::Dashboard.prev(), DashboardTab::Scripts);
        assert_eq!(DashboardTab::Scripts.prev(), DashboardTab::DataSource);
        assert_eq!(DashboardTab::DataSource.prev(), DashboardTab::Board);
        assert_eq!(DashboardTab::Board.prev(), DashboardTab::Spec);
        assert_eq!(DashboardTab::Spec.prev(), DashboardTab::PerWorkspace);
        assert_eq!(DashboardTab::PerWorkspace.prev(), DashboardTab::Dashboard);
    }

    // ---- BOARD_COLUMNS ----

    #[test]
    fn board_columns_count_is_seven() {
        assert_eq!(BOARD_COLUMNS.len(), 7);
    }

    #[test]
    fn board_columns_match_expected_phases() {
        let expected = [
            QueuePhase::Pending,
            QueuePhase::Ready,
            QueuePhase::Running,
            QueuePhase::Completed,
            QueuePhase::Done,
            QueuePhase::Hitl,
            QueuePhase::Failed,
        ];
        assert_eq!(BOARD_COLUMNS, expected);
    }

    // ---- tab_state_preserved ----

    #[test]
    fn tab_state_preserved_across_switches() {
        let mut state = DashboardState::new();
        // Modify Dashboard tab state.
        state.current_tab_state_mut().selected_index = 5;
        // Switch to Board tab.
        state.active_tab = DashboardTab::Board;
        assert_eq!(state.current_tab_state().selected_index, 0);
        // Switch back to Dashboard.
        state.active_tab = DashboardTab::Dashboard;
        assert_eq!(state.current_tab_state().selected_index, 5);
    }

    // ---- spec_status_color ----

    #[test]
    fn spec_status_color_known_statuses() {
        assert_eq!(spec_status_color("draft"), Color::Gray);
        assert_eq!(spec_status_color("active"), Color::Blue);
        assert_eq!(spec_status_color("paused"), Color::Yellow);
        assert_eq!(spec_status_color("completing"), Color::Cyan);
        assert_eq!(spec_status_color("completed"), Color::Green);
        assert_eq!(spec_status_color("archived"), Color::DarkGray);
    }

    #[test]
    fn spec_status_color_unknown_returns_white() {
        assert_eq!(spec_status_color("unknown"), Color::White);
        assert_eq!(spec_status_color(""), Color::White);
    }

    // ---- count_spec_statuses ----

    #[test]
    fn count_spec_statuses_empty() {
        let counts = count_spec_statuses(&[]);
        assert_eq!(counts.draft, 0);
        assert_eq!(counts.active, 0);
        assert_eq!(counts.paused, 0);
        assert_eq!(counts.completing, 0);
        assert_eq!(counts.completed, 0);
    }

    #[test]
    fn count_spec_statuses_mixed() {
        use belt_core::spec::Spec;

        let mut specs = Vec::new();
        let mut s1 = Spec::new("s1".into(), "ws".into(), "n1".into(), "c".into());
        // Draft by default
        specs.push(s1.clone());

        s1.id = "s2".into();
        s1.status = SpecStatus::Active;
        specs.push(s1.clone());

        s1.id = "s3".into();
        s1.status = SpecStatus::Completed;
        specs.push(s1.clone());

        s1.id = "s4".into();
        s1.status = SpecStatus::Completed;
        specs.push(s1);

        let counts = count_spec_statuses(&specs);
        assert_eq!(counts.draft, 1);
        assert_eq!(counts.active, 1);
        assert_eq!(counts.completed, 2);
        assert_eq!(counts.paused, 0);
        assert_eq!(counts.completing, 0);
    }

    // ---- DashboardTab equality ----

    #[test]
    fn dashboard_tab_equality() {
        assert_eq!(DashboardTab::Dashboard, DashboardTab::Dashboard);
        assert_ne!(DashboardTab::Dashboard, DashboardTab::Spec);
        assert_ne!(DashboardTab::Spec, DashboardTab::PerWorkspace);
        assert_ne!(DashboardTab::Board, DashboardTab::Dashboard);
    }

    // ---- truncate_str ----

    #[test]
    fn truncate_str_short_string() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_exact_length() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_str_long_string() {
        assert_eq!(truncate_str("hello world", 7), "hello..");
    }

    #[test]
    fn truncate_str_very_small_max() {
        assert_eq!(truncate_str("hello", 3), "hel");
    }

    // ---- DashboardTab next/prev roundtrip ----

    #[test]
    fn tab_next_full_cycle_returns_to_start() {
        let start = DashboardTab::Dashboard;
        let result = start.next().next().next().next().next().next();
        assert_eq!(result, start);
    }

    #[test]
    fn tab_prev_full_cycle_returns_to_start() {
        let start = DashboardTab::Dashboard;
        let result = start.prev().prev().prev().prev().prev().prev();
        assert_eq!(result, start);
    }

    #[test]
    fn tab_next_full_cycle_from_each_variant() {
        for tab in [
            DashboardTab::Dashboard,
            DashboardTab::PerWorkspace,
            DashboardTab::Spec,
            DashboardTab::Board,
            DashboardTab::DataSource,
            DashboardTab::Scripts,
        ] {
            assert_eq!(tab.next().next().next().next().next().next(), tab);
        }
    }

    #[test]
    fn tab_prev_full_cycle_from_each_variant() {
        for tab in [
            DashboardTab::Dashboard,
            DashboardTab::PerWorkspace,
            DashboardTab::Spec,
            DashboardTab::Board,
            DashboardTab::DataSource,
            DashboardTab::Scripts,
        ] {
            assert_eq!(tab.prev().prev().prev().prev().prev().prev(), tab);
        }
    }

    #[test]
    fn tab_next_then_prev_is_identity() {
        for tab in [
            DashboardTab::Dashboard,
            DashboardTab::PerWorkspace,
            DashboardTab::Spec,
            DashboardTab::Board,
            DashboardTab::DataSource,
            DashboardTab::Scripts,
        ] {
            assert_eq!(tab.next().prev(), tab);
        }
    }

    #[test]
    fn tab_prev_then_next_is_identity() {
        for tab in [
            DashboardTab::Dashboard,
            DashboardTab::PerWorkspace,
            DashboardTab::Spec,
            DashboardTab::Board,
            DashboardTab::DataSource,
            DashboardTab::Scripts,
        ] {
            assert_eq!(tab.prev().next(), tab);
        }
    }

    // ---- DashboardState tab_key mapping ----

    #[test]
    fn tab_key_maps_correctly_for_all_tabs() {
        let mut state = DashboardState::new();

        state.active_tab = DashboardTab::Dashboard;
        assert_eq!(state.tab_key(), 0);

        state.active_tab = DashboardTab::PerWorkspace;
        assert_eq!(state.tab_key(), 1);

        state.active_tab = DashboardTab::Spec;
        assert_eq!(state.tab_key(), 2);

        state.active_tab = DashboardTab::Board;
        assert_eq!(state.tab_key(), 3);

        state.active_tab = DashboardTab::DataSource;
        assert_eq!(state.tab_key(), 4);

        state.active_tab = DashboardTab::Scripts;
        assert_eq!(state.tab_key(), 5);
    }

    // ---- Board view state ----

    #[test]
    fn board_state_initial_selection() {
        let state = DashboardState::new();
        assert_eq!(state.board_selected_col, 0);
        assert_eq!(state.board_selected_row, 0);
    }

    #[test]
    fn board_selected_col_within_board_columns_range() {
        let mut state = DashboardState::new();
        // Simulate navigating columns within the valid range.
        for col in 0..BOARD_COLUMNS.len() {
            state.board_selected_col = col;
            assert!(state.board_selected_col < BOARD_COLUMNS.len());
        }
    }

    #[test]
    fn board_state_preserves_selection_across_tab_switch() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;
        state.board_selected_col = 3;
        state.board_selected_row = 2;

        // Switch away and back.
        state.active_tab = DashboardTab::Dashboard;
        state.active_tab = DashboardTab::Board;

        // Board col/row are stored on DashboardState directly, so they persist.
        assert_eq!(state.board_selected_col, 3);
        assert_eq!(state.board_selected_row, 2);
    }

    // ---- Tab state management ----

    #[test]
    fn tab_states_initialized_for_all_tabs() {
        let state = DashboardState::new();
        // All five tab states (keys 0..=4) should exist.
        for key in 0..=4u8 {
            assert!(
                state.tab_states.contains_key(&key),
                "tab_states should contain key {key}"
            );
        }
    }

    #[test]
    fn tab_state_default_has_zero_selected_index() {
        let ts = TabState::default();
        assert_eq!(ts.selected_index, 0);
        assert_eq!(ts.active_panel, None);
    }

    #[test]
    fn dashboard_tab_state_has_running_panel() {
        let state = DashboardState::new();
        let dashboard_state = state.tab_states.get(&0).unwrap();
        assert_eq!(dashboard_state.active_panel, Some(ActivePanel::Running));
    }

    #[test]
    fn non_dashboard_tab_states_have_no_active_panel() {
        let state = DashboardState::new();
        for key in 1..=4u8 {
            let ts = state.tab_states.get(&key).unwrap();
            assert_eq!(
                ts.active_panel, None,
                "tab {key} should have no active_panel"
            );
        }
    }

    #[test]
    fn current_tab_state_mut_modifies_correct_tab() {
        let mut state = DashboardState::new();

        // Modify PerWorkspace tab state.
        state.active_tab = DashboardTab::PerWorkspace;
        state.current_tab_state_mut().selected_index = 10;

        // Modify Spec tab state.
        state.active_tab = DashboardTab::Spec;
        state.current_tab_state_mut().selected_index = 20;

        // Verify each tab has its own state.
        state.active_tab = DashboardTab::PerWorkspace;
        assert_eq!(state.current_tab_state().selected_index, 10);

        state.active_tab = DashboardTab::Spec;
        assert_eq!(state.current_tab_state().selected_index, 20);

        // Dashboard tab should still be at 0.
        state.active_tab = DashboardTab::Dashboard;
        assert_eq!(state.current_tab_state().selected_index, 0);
    }

    #[test]
    fn overlay_and_help_are_independent_of_tab() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;
        state.overlay_item = Some("w-123".to_string());
        state.show_help = true;

        // Switch tab -- overlay and help state should persist.
        state.active_tab = DashboardTab::Dashboard;
        assert_eq!(state.overlay_item, Some("w-123".to_string()));
        assert!(state.show_help);
    }

    // ---- DataSource status ----

    #[test]
    fn datasource_connection_status_labels() {
        assert_eq!(DataSourceConnectionStatus::Connected.label(), "Connected");
        assert_eq!(
            DataSourceConnectionStatus::Disconnected.label(),
            "Disconnected"
        );
        assert_eq!(DataSourceConnectionStatus::Error.label(), "Error");
    }

    #[test]
    fn datasource_connection_status_colors() {
        assert_eq!(DataSourceConnectionStatus::Connected.color(), Color::Green);
        assert_eq!(
            DataSourceConnectionStatus::Disconnected.color(),
            Color::DarkGray
        );
        assert_eq!(DataSourceConnectionStatus::Error.color(), Color::Red);
    }

    #[test]
    fn datasource_connection_status_indicators() {
        assert_eq!(DataSourceConnectionStatus::Connected.indicator(), "●");
        assert_eq!(DataSourceConnectionStatus::Disconnected.indicator(), "○");
        assert_eq!(DataSourceConnectionStatus::Error.indicator(), "✗");
    }

    #[test]
    fn collect_datasource_status_empty_workspaces() {
        let entries = collect_datasource_status(&[], &[]);
        assert!(entries.is_empty());
    }

    #[test]
    fn collect_datasource_status_error_on_missing_config() {
        let workspaces = vec![(
            "test-ws".to_string(),
            "/nonexistent/path.yml".to_string(),
            "2026-03-25T00:00:00Z".to_string(),
        )];
        let entries = collect_datasource_status(&workspaces, &[]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, DataSourceConnectionStatus::Error);
        assert_eq!(entries[0].workspace, "test-ws");
    }

    #[test]
    fn datasource_tab_key_is_four() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::DataSource;
        assert_eq!(state.tab_key(), 4);
    }

    #[test]
    fn datasource_tab_state_preserved() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::DataSource;
        state.current_tab_state_mut().selected_index = 7;
        state.active_tab = DashboardTab::Dashboard;
        state.active_tab = DashboardTab::DataSource;
        assert_eq!(state.current_tab_state().selected_index, 7);
    }

    #[test]
    fn datasource_status_entry_fields() {
        let entry = DataSourceStatusEntry {
            workspace: "my-ws".to_string(),
            source_name: "github".to_string(),
            url: "https://github.com/org/repo".to_string(),
            state_count: 3,
            scan_interval_secs: 300,
            status: DataSourceConnectionStatus::Connected,
            active_item_count: 5,
        };
        assert_eq!(entry.workspace, "my-ws");
        assert_eq!(entry.source_name, "github");
        assert_eq!(entry.state_count, 3);
        assert_eq!(entry.scan_interval_secs, 300);
        assert_eq!(entry.active_item_count, 5);
        assert_eq!(entry.status, DataSourceConnectionStatus::Connected);
    }

    #[test]
    fn datasource_tab_equality() {
        assert_eq!(DashboardTab::DataSource, DashboardTab::DataSource);
        assert_ne!(DashboardTab::DataSource, DashboardTab::Dashboard);
        assert_ne!(DashboardTab::DataSource, DashboardTab::Board);
    }

    // ---- Scripts tab ----

    #[test]
    fn scripts_tab_in_tab_cycle() {
        // Verify Scripts tab is reachable via next/prev cycling.
        assert_eq!(DashboardTab::DataSource.next(), DashboardTab::Scripts);
        assert_eq!(DashboardTab::Scripts.next(), DashboardTab::Dashboard);
        assert_eq!(DashboardTab::Dashboard.prev(), DashboardTab::Scripts);
        assert_eq!(DashboardTab::Scripts.prev(), DashboardTab::DataSource);
    }

    #[test]
    fn scripts_tab_key_is_five() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Scripts;
        assert_eq!(state.tab_key(), 5);
    }

    #[test]
    fn render_scripts_tab_no_panic_empty_db() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let db = make_db();
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_scripts_tab(frame, &db, frame.area(), 0);
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let content: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(content.contains("Scripts Execution Summary"));
        assert!(content.contains("Per-Script Statistics"));
        assert!(content.contains("Recent Executions"));
    }

    #[test]
    fn render_scripts_tab_no_panic_with_data() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let db = make_db();

        // Insert some history events.
        let event = belt_infra::db::HistoryEvent {
            work_id: "w1".to_string(),
            source_id: "s1".to_string(),
            state: "analyze".to_string(),
            status: "success".to_string(),
            attempt: 1,
            summary: None,
            error: None,
            created_at: "2026-03-25T10:00:00Z".to_string(),
        };
        db.append_history(&event).unwrap();

        let event2 = belt_infra::db::HistoryEvent {
            work_id: "w2".to_string(),
            source_id: "s2".to_string(),
            state: "analyze".to_string(),
            status: "failed".to_string(),
            attempt: 1,
            summary: None,
            error: Some("timeout".to_string()),
            created_at: "2026-03-25T11:00:00Z".to_string(),
        };
        db.append_history(&event2).unwrap();

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_scripts_tab(frame, &db, frame.area(), 0);
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let content: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(content.contains("analyze"));
        assert!(content.contains("Total Runs: 2"));
    }
}
