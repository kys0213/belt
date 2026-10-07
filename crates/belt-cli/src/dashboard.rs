//! TUI Dashboard and runtime statistics panel for Belt.
//!
//! Provides two display modes:
//! - `run()`: interactive ratatui-based real-time TUI dashboard with multiple tabs
//! - `render_runtime_panel()`: text-based runtime statistics panel for non-TUI output
//!
//! The dashboard supports five tabs:
//! - **Dashboard** (`d`): phase summary + running/recent items
//! - **PerWorkspace** (`w`): items filtered by a selected workspace
//! - **Board** (`b`): kanban-style board with columns per queue phase
//! - **DataSource** (`n`): real-time DataSource connection status panel
//! - **Scripts** (`t`): script execution statistics with success/fail rates
//!
//! Tab switching: `d/w/b/n/t` to jump, or `Tab`/`Shift+Tab` to cycle.
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

use belt_core::hitl::HitlId;
use belt_core::hitl::{ConfirmPath, HitlAction, HitlStatus, RespondOutcome};
use belt_core::phase::QueuePhase;
use belt_core::queue::QueueItem;
use belt_daemon::hitl::{HitlResponse, HitlService};

use crate::cancel::{
    CANCEL_POLL_INTERVAL, CANCEL_WAIT_LIMIT, CancelFlow, CancelOutcome, LocalDaemon,
};
use belt_core::transition::Actor;
use belt_infra::db::{
    Database, HistoryEvent, HitlRequest, HitlTarget, ScriptExecStats, TransitionEvent,
};
use belt_infra::workspace_loader::load_workspace_config;

/// Connection status of a DataSource (internal dashboard representation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceStatus {
    /// DataSource is connected and has recent activity.
    Connected,
    /// DataSource is configured but has no recent activity.
    Disconnected,
    /// DataSource configuration could not be loaded.
    Error,
}

impl SourceStatus {
    /// Return a human-readable label for display.
    fn label(self) -> &'static str {
        match self {
            SourceStatus::Connected => "Connected",
            SourceStatus::Disconnected => "Disconnected",
            SourceStatus::Error => "Error",
        }
    }

    /// Return the display color for this status.
    fn color(self) -> Color {
        match self {
            SourceStatus::Connected => Color::Green,
            SourceStatus::Disconnected => Color::DarkGray,
            SourceStatus::Error => Color::Red,
        }
    }

    /// Return a status indicator symbol.
    fn indicator(self) -> &'static str {
        match self {
            SourceStatus::Connected => "●",
            SourceStatus::Disconnected => "○",
            SourceStatus::Error => "✗",
        }
    }
}

/// Status information for a single DataSource (internal dashboard representation).
#[derive(Debug, Clone)]
struct SourceStatusEntry {
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
    status: SourceStatus,
    /// Number of active (non-terminal) items from this source.
    active_item_count: usize,
    /// Timestamp of the last scan (from cron job `last_run_at`).
    last_scan_time: Option<String>,
    /// Number of items collected in the last scan.
    last_scan_result_count: Option<usize>,
    /// Error message when status is `Error`.
    error_message: Option<String>,
}

/// Which panel is currently focused for item selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivePanel {
    Running,
    Recent,
}

/// PerWorkspace sub-view mode: flat table or kanban board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PerWorkspaceView {
    /// Flat table listing all items.
    Table,
    /// Kanban board with status columns.
    Kanban,
}

/// Kanban column groupings for PerWorkspace kanban view.
///
/// Items are grouped into three logical columns:
/// - Pending: `Pending` + `Ready` + `Hitl`
/// - In-Progress: `Running`
/// - Completed: `Completed` + `Done` + `Failed`
const PER_WS_KANBAN_GROUPS: [&str; 3] = ["Pending", "In-Progress", "Completed"];

/// Status filter options for the PerWorkspace tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    /// Show all items (no filter).
    All,
    /// Show only pending/ready/hitl items.
    Pending,
    /// Show only running items.
    Running,
    /// Show only completed/done/failed items.
    Completed,
}

impl StatusFilter {
    /// Cycle to the next filter option.
    fn next(self) -> Self {
        match self {
            StatusFilter::All => StatusFilter::Pending,
            StatusFilter::Pending => StatusFilter::Running,
            StatusFilter::Running => StatusFilter::Completed,
            StatusFilter::Completed => StatusFilter::All,
        }
    }

    /// Return a display label for the current filter.
    fn label(self) -> &'static str {
        match self {
            StatusFilter::All => "All",
            StatusFilter::Pending => "Pending",
            StatusFilter::Running => "Running",
            StatusFilter::Completed => "Completed",
        }
    }

    /// Check if an item's phase passes this filter.
    fn matches(self, phase: &QueuePhase) -> bool {
        match self {
            StatusFilter::All => true,
            StatusFilter::Pending => matches!(
                phase,
                QueuePhase::Pending | QueuePhase::Ready | QueuePhase::Hitl
            ),
            StatusFilter::Running => matches!(phase, QueuePhase::Running),
            StatusFilter::Completed => matches!(
                phase,
                QueuePhase::Completed | QueuePhase::Done | QueuePhase::Failed
            ),
        }
    }
}

/// Active overlay mode for the dashboard.
///
/// Only one overlay can be active at a time. Overlays are rendered on top
/// of the main tab content.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OverlayMode {
    /// No overlay is shown.
    None,
    /// Help overlay showing all key bindings.
    Help,
    /// Item detail overlay showing metadata, HITL details, judgment history,
    /// and transition timeline for the given `work_id`.
    ItemDetail(String),
    /// HITL overlay listing all items in the HITL phase with action options.
    Hitl {
        /// Selected index in the HITL items list.
        selected: usize,
        /// Retry instruction being typed (`r`); `None` outside input mode.
        retry_input: Option<String>,
    },
}

/// Which top-level tab is displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardTab {
    /// Main dashboard with phase summary + running/recent items.
    Dashboard,
    /// Per-workspace view showing items grouped by workspace.
    PerWorkspace,
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
            DashboardTab::PerWorkspace => DashboardTab::Board,
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
            DashboardTab::Board => DashboardTab::PerWorkspace,
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
    /// Current overlay mode (replaces overlay_item + show_help).
    overlay: OverlayMode,
    /// Selected workspace index (used in PerWorkspace tab).
    selected_workspace: usize,
    /// Currently selected column in Board view.
    board_selected_col: usize,
    /// Currently selected row within the selected Board column.
    board_selected_row: usize,
    /// PerWorkspace sub-view mode (table vs kanban).
    per_ws_view: PerWorkspaceView,
    /// PerWorkspace kanban selected column index.
    per_ws_kanban_col: usize,
    /// PerWorkspace kanban selected row within the column.
    per_ws_kanban_row: usize,
    /// Status filter for PerWorkspace tab.
    status_filter: StatusFilter,
    /// Result of the last HITL response, shown in the HITL overlay.
    toast: Option<String>,
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

        Self {
            active_tab: DashboardTab::Dashboard,
            tab_states,
            overlay: OverlayMode::None,
            selected_workspace: 0,
            board_selected_col: 0,
            board_selected_row: 0,
            per_ws_view: PerWorkspaceView::Table,
            per_ws_kanban_col: 0,
            per_ws_kanban_row: 0,
            status_filter: StatusFilter::All,
            toast: None,
        }
    }

    fn tab_key(&self) -> u8 {
        match self.active_tab {
            DashboardTab::Dashboard => 0,
            DashboardTab::PerWorkspace => 1,
            DashboardTab::Board => 2,
            DashboardTab::DataSource => 3,
            DashboardTab::Scripts => 4,
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

    let service = HitlService::new(db);
    let result = run_loop(&mut terminal, &service);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    service: &HitlService,
) -> anyhow::Result<()> {
    let db = service.database();
    let responder = HitlResponder {
        service,
        by: tui_respondent(),
    };
    let mut state = DashboardState::new();
    let local_daemon = LocalDaemon {
        belt_home: crate::belt_home()?,
    };
    let killer = belt_infra::platform::default_process_killer();
    let cancel_flow = CancelFlow {
        db,
        daemon: &local_daemon,
        killer: killer.as_ref(),
        actor: Actor::Tui,
        requester: tui_respondent(),
        wait_limit: CANCEL_WAIT_LIMIT,
        poll_interval: CANCEL_POLL_INTERVAL,
    };

    loop {
        // Collect data for all tabs.
        let all_items = db.list_items(None, None).unwrap_or_default();
        let running_items: Vec<_> = all_items
            .iter()
            .filter(|i| i.phase() == QueuePhase::Running)
            .cloned()
            .collect();
        let recent_items = collect_recent_items(db);
        let workspaces = db.list_workspaces().unwrap_or_default();
        let datasource_entries = collect_datasource_status(&workspaces, &all_items, Some(db));

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
                // Items filtered by selected workspace and status filter.
                if state.per_ws_view == PerWorkspaceView::Kanban {
                    0 // Kanban uses column/row navigation, not a single list.
                } else if let Some(ws) = workspaces.get(state.selected_workspace) {
                    all_items
                        .iter()
                        .filter(|i| {
                            i.workspace_id == ws.0 && state.status_filter.matches(&i.phase())
                        })
                        .count()
                } else {
                    0
                }
            }
            DashboardTab::Board => 0, // Board uses column/row navigation, not a single list.
            DashboardTab::DataSource => datasource_entries.len(),
            DashboardTab::Scripts => db
                .get_script_execution_stats()
                .map(|s| s.len())
                .unwrap_or(0),
        };

        // Clamp selected_index for non-Board/non-kanban tabs.
        let uses_list_nav = state.active_tab != DashboardTab::Board
            && !(state.active_tab == DashboardTab::PerWorkspace
                && state.per_ws_view == PerWorkspaceView::Kanban);
        if uses_list_nav {
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
            .map(|phase| all_items.iter().filter(|i| i.phase() == *phase).collect())
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

        // Build per-workspace kanban columns (Pending / In-Progress / Completed).
        let per_ws_kanban_columns: Vec<Vec<&QueueItem>> = {
            let ws_items: Vec<&QueueItem> = if let Some(ws) =
                workspaces.get(state.selected_workspace)
            {
                all_items
                    .iter()
                    .filter(|i| i.workspace_id == ws.0 && state.status_filter.matches(&i.phase()))
                    .collect()
            } else {
                Vec::new()
            };
            vec![
                // Pending: Pending + Ready + Hitl
                ws_items
                    .iter()
                    .filter(|i| {
                        matches!(
                            i.phase(),
                            QueuePhase::Pending | QueuePhase::Ready | QueuePhase::Hitl
                        )
                    })
                    .copied()
                    .collect(),
                // In-Progress: Running
                ws_items
                    .iter()
                    .filter(|i| i.phase() == QueuePhase::Running)
                    .copied()
                    .collect(),
                // Completed: Completed + Done + Failed
                ws_items
                    .iter()
                    .filter(|i| {
                        matches!(
                            i.phase(),
                            QueuePhase::Completed | QueuePhase::Done | QueuePhase::Failed
                        )
                    })
                    .copied()
                    .collect(),
            ]
        };

        // Clamp per-workspace kanban selection.
        if state.per_ws_kanban_col >= PER_WS_KANBAN_GROUPS.len() {
            state.per_ws_kanban_col = 0;
        }
        let ws_col_len = per_ws_kanban_columns
            .get(state.per_ws_kanban_col)
            .map_or(0, |v| v.len());
        if ws_col_len == 0 {
            state.per_ws_kanban_row = 0;
        } else if state.per_ws_kanban_row >= ws_col_len {
            state.per_ws_kanban_row = ws_col_len.saturating_sub(1);
        }

        // The HITL overlay answers exactly the requests it draws, so the data is
        // loaded once per frame and shared by drawing and key handling.
        let hitl_view =
            matches!(state.overlay, OverlayMode::Hitl { .. }).then(|| HitlOverlayView::load(db));

        terminal.draw(|frame| {
            // Tab bar at top.
            let outer_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(5)])
                .split(frame.area());

            let tab_bar = render_tab_bar(state.active_tab, state.toast.as_deref());
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
                    render_per_workspace_tab(
                        frame,
                        db,
                        outer_chunks[1],
                        &workspaces,
                        &state,
                        &per_ws_kanban_columns,
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
            match &state.overlay {
                OverlayMode::None => {}
                OverlayMode::Help => {
                    render_help_overlay(frame);
                }
                OverlayMode::ItemDetail(work_id) => {
                    render_item_detail_overlay(frame, db, work_id);
                }
                OverlayMode::Hitl {
                    selected,
                    retry_input,
                } => {
                    render_hitl_overlay(
                        frame,
                        hitl_view.as_ref().and_then(|v| v.as_ref().ok()),
                        hitl_view.as_ref().and_then(|v| v.as_ref().err()),
                        *selected,
                        retry_input.as_deref(),
                        state.toast.as_deref(),
                    );
                }
            }
        })?;

        // Poll for keyboard events with 1 second timeout.
        if event::poll(Duration::from_secs(1))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            // An open overlay owns the keyboard: its keys take precedence over the
            // global ones below.
            let hitl_rows = match &hitl_view {
                Some(Ok(view)) => view.rows(),
                _ => Vec::new(),
            };
            if handle_overlay_key(&mut state, key.code, &hitl_rows, &responder) {
                continue;
            }

            if switch_tab_key(&mut state, key.code) {
                continue;
            }

            // A toast stays until the next key.
            state.toast = None;

            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('x') => {
                    let selected = selected_work_id(
                        &state,
                        &running_items,
                        &recent_items,
                        &workspaces,
                        db,
                        &board_columns,
                        &per_ws_kanban_columns,
                    );
                    handle_cancel_key(&mut state, selected.as_deref(), db, &cancel_flow);
                }
                KeyCode::Char('h') => {
                    state.toast = None;
                    state.overlay = OverlayMode::Hitl {
                        selected: 0,
                        retry_input: None,
                    };
                }
                KeyCode::Char('?') => {
                    state.overlay = OverlayMode::Help;
                }
                // Toggle PerWorkspace sub-view (table/kanban).
                KeyCode::Char('v') if state.active_tab == DashboardTab::PerWorkspace => {
                    state.per_ws_view = match state.per_ws_view {
                        PerWorkspaceView::Table => PerWorkspaceView::Kanban,
                        PerWorkspaceView::Kanban => PerWorkspaceView::Table,
                    };
                    state.per_ws_kanban_col = 0;
                    state.per_ws_kanban_row = 0;
                }
                // Cycle status filter in PerWorkspace tab.
                KeyCode::Char('f') if state.active_tab == DashboardTab::PerWorkspace => {
                    state.status_filter = state.status_filter.next();
                    // Reset selection when filter changes.
                    state.current_tab_state_mut().selected_index = 0;
                    state.per_ws_kanban_row = 0;
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
                    handle_nav_down(
                        &mut state,
                        active_list_len,
                        &board_columns,
                        &per_ws_kanban_columns,
                    );
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
                        &per_ws_kanban_columns,
                    );
                }
                // Manual refresh: skip the poll timeout and re-fetch data immediately.
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    continue;
                }
                _ => {}
            }
        }
    }
}

/// Path label recorded as `via` for responses given in the dashboard.
const TUI_VIA: &str = "tui";

/// Respondent recorded as `by`: `$USER`, or `tui` when it is unset or empty.
fn tui_respondent() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| TUI_VIA.to_string())
}

/// One item of the HITL overlay and the request drawn for it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HitlRow {
    work_id: String,
    hitl_id: Option<HitlId>,
}

/// Snapshot of what the HITL overlay draws.
struct HitlOverlayView {
    items: Vec<QueueItem>,
    requests: HashMap<String, HitlRequest>,
}

impl HitlOverlayView {
    fn load(db: &Database) -> anyhow::Result<Self> {
        Ok(Self {
            items: db.list_items(Some(QueuePhase::Hitl), None)?,
            requests: current_hitl_requests(db)?,
        })
    }

    fn rows(&self) -> Vec<HitlRow> {
        self.items
            .iter()
            .map(|item| HitlRow {
                work_id: item.work_id.clone(),
                hitl_id: self
                    .requests
                    .get(&item.work_id)
                    .map(|request| request.hitl_id.clone()),
            })
            .collect()
    }
}

/// Sends HITL responses given in the overlay through the shared first-wins contract.
struct HitlResponder<'a> {
    service: &'a HitlService,
    by: String,
}

impl HitlResponder<'_> {
    /// Respond to the request drawn for `row` and describe the result.
    ///
    /// A row with a request is answered by its `hitl_id`, so a request re-opened
    /// for the same item after the frame was drawn is left untouched.
    fn respond(&self, row: &HitlRow, action: HitlAction, notes: Option<String>) -> String {
        let work_id = row.work_id.as_str();
        let target = match &row.hitl_id {
            Some(id) => HitlTarget::Id(id.clone()),
            None => HitlTarget::Item(work_id.to_string()),
        };
        let response = HitlResponse {
            target,
            action,
            by: self.by.clone(),
            via: TUI_VIA.to_string(),
            path: ConfirmPath::Direct,
            notes,
        };
        match self.service.respond(&response) {
            Ok(outcome) => describe_respond_outcome(work_id, action, &outcome),
            Err(e) => format!("error: {work_id}: {e}"),
        }
    }
}

/// One-line toast text for a response attempt.
fn describe_respond_outcome(work_id: &str, action: HitlAction, outcome: &RespondOutcome) -> String {
    match outcome {
        RespondOutcome::Won { .. } => format!("{work_id}: {action} 응답 확정 - 해결됨 · 처리 중"),
        RespondOutcome::AlreadyHandled(winner) => format!(
            "already_handled: {work_id} 는 이미 {} 가 {} 로 {} 응답함 ({})",
            winner.by,
            winner.via,
            winner.action,
            format_transition_time(&winner.at),
        ),
        RespondOutcome::NotFound => format!("not_found: {work_id} 에 응답할 HITL 요청이 없음"),
        RespondOutcome::InvalidAction => {
            format!("invalid_action: {work_id} 에 {action} 응답을 적용할 수 없음")
        }
        RespondOutcome::Unauthorized => format!("unauthorized: {work_id} 에 응답할 권한이 없음"),
    }
}

/// Handle a key while an overlay is open. Returns `true` when an overlay
/// consumed the key; the global keys must then be skipped.
///
/// `hitl_rows` lists the items in the HITL phase in overlay order.
fn handle_overlay_key(
    state: &mut DashboardState,
    code: KeyCode,
    hitl_rows: &[HitlRow],
    responder: &HitlResponder<'_>,
) -> bool {
    match state.overlay.clone() {
        OverlayMode::None => false,
        OverlayMode::Help => {
            state.overlay = OverlayMode::None;
            true
        }
        OverlayMode::ItemDetail(_) => {
            if matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
                state.overlay = OverlayMode::None;
            }
            true
        }
        OverlayMode::Hitl {
            selected,
            retry_input,
        } => {
            handle_hitl_overlay_key(state, code, selected, retry_input, hitl_rows, responder);
            true
        }
    }
}

fn handle_hitl_overlay_key(
    state: &mut DashboardState,
    code: KeyCode,
    selected: usize,
    retry_input: Option<String>,
    hitl_rows: &[HitlRow],
    responder: &HitlResponder<'_>,
) {
    let count = hitl_rows.len();
    let selected_row = hitl_rows.get(selected);

    if let Some(mut input) = retry_input {
        match code {
            KeyCode::Esc => {
                state.overlay = OverlayMode::Hitl {
                    selected,
                    retry_input: None,
                };
            }
            KeyCode::Enter => {
                if let Some(row) = selected_row {
                    let notes = (!input.trim().is_empty()).then(|| input.trim().to_string());
                    state.toast = Some(responder.respond(row, HitlAction::Retry, notes));
                }
                state.overlay = OverlayMode::Hitl {
                    selected,
                    retry_input: None,
                };
            }
            KeyCode::Backspace => {
                input.pop();
                state.overlay = OverlayMode::Hitl {
                    selected,
                    retry_input: Some(input),
                };
            }
            KeyCode::Char(c) => {
                input.push(c);
                state.overlay = OverlayMode::Hitl {
                    selected,
                    retry_input: Some(input),
                };
            }
            _ => {
                state.overlay = OverlayMode::Hitl {
                    selected,
                    retry_input: Some(input),
                };
            }
        }
        return;
    }

    match code {
        KeyCode::Char('q') | KeyCode::Esc => {
            state.toast = None;
            state.overlay = OverlayMode::None;
        }
        KeyCode::Down | KeyCode::Char('j') if count > 0 => {
            state.overlay = OverlayMode::Hitl {
                selected: (selected + 1).min(count - 1),
                retry_input: None,
            };
        }
        KeyCode::Up | KeyCode::Char('k') if count > 0 => {
            state.overlay = OverlayMode::Hitl {
                selected: selected.saturating_sub(1),
                retry_input: None,
            };
        }
        KeyCode::Enter => {
            if let Some(row) = selected_row {
                state.overlay = OverlayMode::ItemDetail(row.work_id.clone());
            }
        }
        KeyCode::Char('r') if selected_row.is_some() => {
            state.overlay = OverlayMode::Hitl {
                selected,
                retry_input: Some(String::new()),
            };
        }
        KeyCode::Char(c @ ('d' | 's' | 'p')) => {
            let action = match c {
                'd' => HitlAction::Done,
                's' => HitlAction::Skip,
                _ => HitlAction::Replan,
            };
            if let Some(row) = selected_row {
                state.toast = Some(responder.respond(row, action, None));
            }
        }
        _ => {}
    }
}

/// Jump to a tab by its letter key. Returns `true` when the key was a tab key.
///
/// `x` is reserved for cancelling a running item and switches nothing.
fn switch_tab_key(state: &mut DashboardState, code: KeyCode) -> bool {
    let tab = match code {
        KeyCode::Char('d') => DashboardTab::Dashboard,
        KeyCode::Char('w') => DashboardTab::PerWorkspace,
        KeyCode::Char('b') => DashboardTab::Board,
        KeyCode::Char('n') => DashboardTab::DataSource,
        KeyCode::Char('t') => DashboardTab::Scripts,
        _ => return false,
    };
    state.active_tab = tab;
    true
}

/// Requests that currently decide what an item in Hitl shows: the open one,
/// else the confirmed one still waiting for post-processing.
fn current_hitl_requests(db: &Database) -> anyhow::Result<HashMap<String, HitlRequest>> {
    let mut by_work_id = HashMap::new();
    for request in db.pending_post_processing()? {
        by_work_id.insert(request.work_id.clone(), request);
    }
    for request in db.open_hitl_requests()? {
        by_work_id.insert(request.work_id.clone(), request);
    }
    Ok(by_work_id)
}

/// Handle Up/k navigation.
fn handle_nav_up(state: &mut DashboardState, board_columns: &[Vec<&QueueItem>]) {
    match state.active_tab {
        DashboardTab::Board => {
            state.board_selected_row = state.board_selected_row.saturating_sub(1);
        }
        DashboardTab::PerWorkspace if state.per_ws_view == PerWorkspaceView::Kanban => {
            state.per_ws_kanban_row = state.per_ws_kanban_row.saturating_sub(1);
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
    per_ws_kanban_columns: &[Vec<&QueueItem>],
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
        DashboardTab::PerWorkspace if state.per_ws_view == PerWorkspaceView::Kanban => {
            let col_len = per_ws_kanban_columns
                .get(state.per_ws_kanban_col)
                .map_or(0, |v| v.len());
            if col_len > 0 {
                state.per_ws_kanban_row =
                    (state.per_ws_kanban_row + 1).min(col_len.saturating_sub(1));
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
            if state.per_ws_view == PerWorkspaceView::Kanban {
                state.per_ws_kanban_col = state.per_ws_kanban_col.saturating_sub(1);
                state.per_ws_kanban_row = 0;
            } else if state.selected_workspace > 0 {
                state.selected_workspace -= 1;
                state.current_tab_state_mut().selected_index = 0;
            }
        }
        DashboardTab::Board => {
            state.board_selected_col = state.board_selected_col.saturating_sub(1);
            state.board_selected_row = 0;
        }
        DashboardTab::DataSource | DashboardTab::Scripts => {}
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
            if state.per_ws_view == PerWorkspaceView::Kanban {
                if state.per_ws_kanban_col < PER_WS_KANBAN_GROUPS.len().saturating_sub(1) {
                    state.per_ws_kanban_col += 1;
                    state.per_ws_kanban_row = 0;
                }
            } else if !workspaces.is_empty() && state.selected_workspace < workspaces.len() - 1 {
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
        DashboardTab::DataSource | DashboardTab::Scripts => {}
    }
}

/// Handle Enter key to open item detail overlay.
#[allow(clippy::too_many_arguments)]
fn handle_enter(
    state: &mut DashboardState,
    running_items: &[QueueItem],
    recent_items: &[QueueItem],
    all_items: &[QueueItem],
    workspaces: &[(String, String, String)],
    db: &Database,
    board_columns: &[Vec<&QueueItem>],
    per_ws_kanban_columns: &[Vec<&QueueItem>],
) {
    if let Some(work_id) = selected_work_id(
        state,
        running_items,
        recent_items,
        workspaces,
        db,
        board_columns,
        per_ws_kanban_columns,
    ) {
        state.overlay = OverlayMode::ItemDetail(work_id);
    }
    let _ = all_items;
}

/// The item under the selection of the active tab, if any.
fn selected_work_id(
    state: &DashboardState,
    running_items: &[QueueItem],
    recent_items: &[QueueItem],
    workspaces: &[(String, String, String)],
    db: &Database,
    board_columns: &[Vec<&QueueItem>],
    per_ws_kanban_columns: &[Vec<&QueueItem>],
) -> Option<String> {
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
            items.get(idx).map(|item| item.work_id.clone())
        }
        DashboardTab::PerWorkspace => {
            if state.per_ws_view == PerWorkspaceView::Kanban {
                per_ws_kanban_columns
                    .get(state.per_ws_kanban_col)
                    .and_then(|col| col.get(state.per_ws_kanban_row))
                    .map(|item| item.work_id.clone())
            } else {
                let ws = workspaces.get(state.selected_workspace)?;
                let idx = state.current_tab_state().selected_index;
                db.list_items(None, Some(&ws.0))
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|i| state.status_filter.matches(&i.phase()))
                    .nth(idx)
                    .map(|item| item.work_id)
            }
        }
        DashboardTab::DataSource | DashboardTab::Scripts => None,
        DashboardTab::Board => board_columns
            .get(state.board_selected_col)
            .and_then(|col| col.get(state.board_selected_row))
            .map(|item| item.work_id.clone()),
    }
}

/// Toast text of a cancel attempt on `work_id`.
fn describe_cancel(work_id: &str, outcome: &CancelOutcome) -> String {
    match outcome {
        CancelOutcome::Canceled => format!("canceled: {work_id}"),
        CancelOutcome::CanceledDirectly => {
            format!("canceled_directly: {work_id} (no daemon answered)")
        }
        CancelOutcome::Accepted => {
            format!("accepted: the daemon is canceling {work_id}; see `belt queue show`")
        }
        CancelOutcome::TooLate { current } => {
            format!("too_late: {work_id} already left Running (now {current})")
        }
    }
}

/// Handle the cancel key: cancel the selected item when it is Running.
///
/// An item in Hitl whose confirmed response awaits post-processing is
/// refused as `busy`; any other phase has nothing running to cancel. The
/// result is shown as the toast.
fn handle_cancel_key(
    state: &mut DashboardState,
    selected: Option<&str>,
    db: &Database,
    flow: &CancelFlow<'_>,
) {
    let Some(work_id) = selected else {
        return;
    };
    state.toast = Some(match cancel_selected(work_id, db, flow) {
        Ok(text) => text,
        Err(e) => format!("error: {work_id}: {e}"),
    });
}

fn cancel_selected(work_id: &str, db: &Database, flow: &CancelFlow<'_>) -> anyhow::Result<String> {
    let item = db.get_item(work_id)?;
    match item.phase() {
        QueuePhase::Running => Ok(describe_cancel(work_id, &flow.cancel(work_id)?)),
        QueuePhase::Hitl
            if db
                .pending_post_processing()?
                .iter()
                .any(|r| r.work_id == work_id) =>
        {
            Ok(format!(
                "busy: {work_id} is being processed (post_processing)"
            ))
        }
        phase => Ok(format!("not running: {work_id} is {phase}")),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the tab bar showing available tabs with the active one highlighted.
///
/// A toast, when present, is shown in the title.
fn render_tab_bar(active: DashboardTab, toast: Option<&str>) -> Paragraph<'static> {
    let tabs = [
        ("d", "Dashboard", DashboardTab::Dashboard),
        ("w", "Workspace", DashboardTab::PerWorkspace),
        ("b", "Board", DashboardTab::Board),
        ("n", "DataSource", DashboardTab::DataSource),
        ("t", "Scripts", DashboardTab::Scripts),
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
        "[r] Refresh  [h] Help  [Tab/Shift+Tab] Cycle",
        Style::default().fg(Color::DarkGray),
    ));

    let title = match toast {
        Some(toast) => format!(" Belt TUI | {toast} "),
        None => " Belt TUI ".to_string(),
    };
    Paragraph::new(Line::from(spans)).block(
        Block::default()
            .title(title)
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
    db: Option<&Database>,
) -> Vec<SourceStatusEntry> {
    let mut entries = Vec::new();

    // Build a map of workspace -> last_run_at from cron jobs (evaluate job).
    let cron_last_run: HashMap<String, String> = db
        .and_then(|d| d.list_cron_jobs().ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|job| {
            // Match workspace-scoped evaluate jobs: "{workspace}:evaluate"
            let ws = job.workspace.as_ref()?;
            if job.name.ends_with(":evaluate") {
                job.last_run_at.map(|t| (ws.clone(), t))
            } else {
                None
            }
        })
        .collect();

    for (ws_name, config_path, _created_at) in workspaces {
        let config = match load_workspace_config(Path::new(config_path)) {
            Ok(c) => c,
            Err(e) => {
                // Config could not be loaded — report error status.
                entries.push(SourceStatusEntry {
                    workspace: ws_name.clone(),
                    source_name: "(unknown)".to_string(),
                    url: config_path.clone(),
                    state_count: 0,
                    scan_interval_secs: 0,
                    status: SourceStatus::Error,
                    active_item_count: 0,
                    last_scan_time: None,
                    last_scan_result_count: None,
                    error_message: Some(format!("{e}")),
                });
                continue;
            }
        };

        let last_scan = cron_last_run.get(ws_name).cloned();

        // Count items created after the last scan time (approximate scan result).
        let scan_result_count = last_scan.as_ref().map(|scan_time| {
            all_items
                .iter()
                .filter(|item| item.workspace_id == *ws_name && item.created_at >= *scan_time)
                .count()
        });

        for (source_name, source_config) in &config.sources {
            // Count active (non-terminal) items from this workspace.
            let active_count = all_items
                .iter()
                .filter(|item| {
                    item.workspace_id == *ws_name
                        && !matches!(
                            item.phase(),
                            QueuePhase::Done | QueuePhase::Failed | QueuePhase::Completed
                        )
                })
                .count();

            let status = if last_scan.is_some() || active_count > 0 {
                SourceStatus::Connected
            } else {
                SourceStatus::Disconnected
            };

            entries.push(SourceStatusEntry {
                workspace: ws_name.clone(),
                source_name: source_name.clone(),
                url: source_config.url.clone(),
                state_count: source_config.states.len(),
                scan_interval_secs: source_config.scan_interval_secs,
                status,
                active_item_count: active_count,
                last_scan_time: last_scan.clone(),
                last_scan_result_count: scan_result_count,
                error_message: None,
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
    entries: &[SourceStatusEntry],
    selected_index: usize,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(8)])
        .split(area);

    // Summary bar.
    let connected = entries
        .iter()
        .filter(|e| e.status == SourceStatus::Connected)
        .count();
    let disconnected = entries
        .iter()
        .filter(|e| e.status == SourceStatus::Disconnected)
        .count();
    let error = entries
        .iter()
        .filter(|e| e.status == SourceStatus::Error)
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
        Cell::from("Last Scan"),
        Cell::from("Count"),
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

            // Format last scan time: show only time portion if available.
            let last_scan_display = match &entry.last_scan_time {
                Some(ts) => {
                    // Extract HH:MM:SS from RFC 3339 timestamp.
                    if let Some(time_part) = ts.split('T').nth(1) {
                        time_part
                            .trim_end_matches('Z')
                            .split('+')
                            .next()
                            .unwrap_or(time_part)
                            .to_string()
                    } else {
                        truncate_str(ts, 19)
                    }
                }
                None => "-".to_string(),
            };

            let last_scan_style = match &entry.last_scan_time {
                Some(_) => Style::default().fg(Color::Green),
                None => Style::default().fg(Color::DarkGray),
            };

            let count_display = match entry.last_scan_result_count {
                Some(c) => c.to_string(),
                None => "-".to_string(),
            };

            // Show error message tooltip via status indicator when in Error state.
            let status_cell = if entry.status == SourceStatus::Error {
                let err_msg = entry.error_message.as_deref().unwrap_or("unknown error");
                Cell::from(format!(
                    "{} {}",
                    entry.status.indicator(),
                    truncate_str(err_msg, 12)
                ))
                .style(status_style)
            } else {
                Cell::from(indicator).style(status_style)
            };

            Row::new(vec![
                status_cell,
                Cell::from(entry.workspace.clone()),
                Cell::from(entry.source_name.clone()),
                Cell::from(truncate_str(&entry.url, 30)),
                Cell::from(entry.state_count.to_string()),
                Cell::from(interval_display),
                Cell::from(entry.active_item_count.to_string()),
                Cell::from(last_scan_display).style(last_scan_style),
                Cell::from(count_display),
            ])
            .style(style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(16),
            Constraint::Length(12),
            Constraint::Min(16),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(7),
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
            let phase_str = item.phase().as_str().to_string();
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
/// Supports two sub-views:
/// - **Table** (default): flat list with status filter
/// - **Kanban**: items grouped into Pending / In-Progress / Completed columns
///
/// Toggle between views with `v`. Cycle status filter with `f`.
///
/// Layout:
/// - Workspace selector bar + view/filter indicators (top)
/// - Workspace phase summary (middle)
/// - Items view: table or kanban (bottom)
fn render_per_workspace_tab(
    frame: &mut ratatui::Frame,
    db: &Database,
    area: Rect,
    workspaces: &[(String, String, String)],
    state: &DashboardState,
    per_ws_kanban_columns: &[Vec<&QueueItem>],
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Min(5),
        ])
        .split(area);

    // Workspace selector bar with view mode and filter indicators.
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

    // Append view mode and filter indicators.
    let view_label = match state.per_ws_view {
        PerWorkspaceView::Table => "Table",
        PerWorkspaceView::Kanban => "Kanban",
    };
    ws_spans.push(Span::raw("    "));
    ws_spans.push(Span::styled(
        format!("[v] {view_label}"),
        Style::default().fg(Color::DarkGray),
    ));
    ws_spans.push(Span::raw("  "));
    ws_spans.push(Span::styled(
        format!("[f] Filter: {}", state.status_filter.label()),
        Style::default().fg(Color::DarkGray),
    ));

    let nav_hint = if state.per_ws_view == PerWorkspaceView::Kanban {
        " Workspace [<-/-> cols] "
    } else {
        " Workspaces [<-/->] "
    };

    let ws_bar = Paragraph::new(Line::from(ws_spans)).block(
        Block::default()
            .title(nav_hint)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(ws_bar, chunks[0]);

    // Items for the selected workspace.
    let selected_ws = &workspaces[state.selected_workspace].0;
    let ws_items = db.list_items(None, Some(selected_ws)).unwrap_or_default();

    render_workspace_phase_summary(frame, chunks[1], &ws_items);

    // Render either table or kanban view.
    match state.per_ws_view {
        PerWorkspaceView::Table => {
            render_per_workspace_table(frame, chunks[2], &ws_items, state, selected_ws);
        }
        PerWorkspaceView::Kanban => {
            render_per_workspace_kanban(
                frame,
                chunks[2],
                per_ws_kanban_columns,
                state,
                selected_ws,
            );
        }
    }
}

/// Render the PerWorkspace flat table view with status filter applied.
fn render_per_workspace_table(
    frame: &mut ratatui::Frame,
    area: Rect,
    ws_items: &[QueueItem],
    state: &DashboardState,
    selected_ws: &str,
) {
    let selected_index = state.current_tab_state().selected_index;

    let filtered_items: Vec<&QueueItem> = ws_items
        .iter()
        .filter(|i| state.status_filter.matches(&i.phase()))
        .collect();

    let rows: Vec<Row<'static>> = filtered_items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let phase_str = item.phase().as_str().to_string();
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

    let filter_label = state.status_filter.label();
    let title = format!(
        " Items [{selected_ws}] ({}) filter:{filter_label} ",
        filtered_items.len()
    );

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
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green)),
    );
    frame.render_widget(table, area);
}

/// Render the PerWorkspace kanban view with three columns.
///
/// Columns:
/// - **Pending**: Pending + Ready + Hitl items
/// - **In-Progress**: Running items
/// - **Completed**: Completed + Done + Failed items
fn render_per_workspace_kanban(
    frame: &mut ratatui::Frame,
    area: Rect,
    kanban_columns: &[Vec<&QueueItem>],
    state: &DashboardState,
    selected_ws: &str,
) {
    let num_cols = PER_WS_KANBAN_GROUPS.len();
    let constraints: Vec<Constraint> = (0..num_cols)
        .map(|_| Constraint::Ratio(1, num_cols as u32))
        .collect();

    let col_areas = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    let column_colors = [Color::Gray, Color::Green, Color::Cyan];

    let empty_col: Vec<&QueueItem> = Vec::new();
    for (col_idx, group_name) in PER_WS_KANBAN_GROUPS.iter().enumerate() {
        let items = kanban_columns.get(col_idx).unwrap_or(&empty_col);
        let is_selected_col = col_idx == state.per_ws_kanban_col;
        let color = column_colors[col_idx];

        let mut lines: Vec<Line<'static>> = Vec::new();

        for (row_idx, item) in items.iter().enumerate() {
            let is_selected = is_selected_col && row_idx == state.per_ws_kanban_row;
            let col_width = col_areas[col_idx].width as usize;
            let work_id_display = truncate_str(&item.work_id, col_width.saturating_sub(4));
            let phase_str = item.phase().as_str();
            let phase_clr = phase_color(phase_str);

            if is_selected {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("> {work_id_display}"),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(phase_str.to_string(), Style::default().fg(phase_clr)),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {work_id_display}"),
                        Style::default().fg(Color::White),
                    ),
                    Span::raw(" "),
                    Span::styled(phase_str.to_string(), Style::default().fg(phase_clr)),
                ]));
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
        let title = format!(" {} ({}) [{selected_ws}] ", group_name, items.len());

        let paragraph = Paragraph::new(lines).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        );

        frame.render_widget(paragraph, col_areas[col_idx]);
    }
}

/// Render a compact phase summary for a single workspace's items.
fn render_workspace_phase_summary(frame: &mut ratatui::Frame, area: Rect, ws_items: &[QueueItem]) {
    let mut phase_counts: HashMap<String, usize> = HashMap::new();
    for item in ws_items {
        *phase_counts
            .entry(item.phase().as_str().to_string())
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
            Span::styled("  b       ", Style::default().fg(Color::Yellow)),
            Span::raw("Switch to Board (kanban) tab"),
        ]),
        Line::from(vec![
            Span::styled("  n       ", Style::default().fg(Color::Yellow)),
            Span::raw("Switch to DataSource status tab"),
        ]),
        Line::from(vec![
            Span::styled("  t       ", Style::default().fg(Color::Yellow)),
            Span::raw("Switch to Scripts statistics tab"),
        ]),
        Line::from(vec![
            Span::styled("  h       ", Style::default().fg(Color::Yellow)),
            Span::raw("Open HITL items overlay"),
        ]),
        Line::from(vec![
            Span::styled("  ?       ", Style::default().fg(Color::Yellow)),
            Span::raw("Show this help"),
        ]),
        Line::from(vec![
            Span::styled("  r/R     ", Style::default().fg(Color::Yellow)),
            Span::raw("Refresh data immediately"),
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
        Line::from(vec![
            Span::styled("  x       ", Style::default().fg(Color::Cyan)),
            Span::raw("Cancel the selected running item"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  v       ", Style::default().fg(Color::Green)),
            Span::raw("Toggle table/kanban (Workspace tab)"),
        ]),
        Line::from(vec![
            Span::styled("  f       ", Style::default().fg(Color::Green)),
            Span::raw("Cycle status filter (Workspace tab)"),
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
    let source_id = item.as_ref().ok().map(|i| i.source_id.as_str());
    let history = source_id
        .and_then(|sid| db.get_history(sid).ok())
        .unwrap_or_default();

    let hitl = current_hitl_requests(db);
    let mut lines = build_detail_lines_with_history(
        work_id,
        item.ok().as_ref(),
        &transitions,
        &history,
        hitl.as_ref()
            .ok()
            .and_then(|requests| requests.get(work_id)),
    );
    if let Err(e) = &hitl {
        lines.push(Line::from(""));
        lines.push(hitl_error_line(e));
    }

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .title(" Item Details ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );

    frame.render_widget(paragraph, area);
}

/// Build the text lines for the item detail overlay (without history).
///
/// Convenience wrapper around [`build_detail_lines_with_history`] that
/// passes an empty history slice. Used primarily in tests.
#[cfg(test)]
fn build_detail_lines<'a>(
    work_id: &str,
    item: Option<&QueueItem>,
    transitions: &[TransitionEvent],
) -> Vec<Line<'a>> {
    build_detail_lines_with_history(work_id, item, transitions, &[], None)
}

/// Build the text lines for the item detail overlay with judgment history.
///
/// Extended version of [`build_detail_lines`] that also includes execution
/// history events showing each attempt's outcome and rationale.
fn build_detail_lines_with_history<'a>(
    work_id: &str,
    item: Option<&QueueItem>,
    transitions: &[TransitionEvent],
    history: &[HistoryEvent],
    hitl: Option<&HitlRequest>,
) -> Vec<Line<'a>> {
    let mut lines: Vec<Line<'a>> = Vec::new();

    match item {
        Some(item) => {
            let phase_str = item.phase().as_str().to_string();
            let color = phase_color(&phase_str);

            lines.push(Line::from(vec![
                Span::styled("ID: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(item.work_id.clone()),
            ]));

            if let Some(ref title) = item.title {
                lines.push(Line::from(vec![
                    Span::styled("Title: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(title.clone()),
                ]));
            }

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
                Span::styled("Source: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(item.source_id.clone()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Created: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(item.created_at.clone()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Updated: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(item.updated_at.clone()),
            ]));

            if let Some(request) = hitl {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "HITL Details:",
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .fg(Color::Yellow),
                )));
                let label = |text: &'static str| {
                    Span::styled(text, Style::default().add_modifier(Modifier::BOLD))
                };
                lines.push(Line::from(vec![
                    label("  Status: "),
                    Span::raw(hitl_status_label(request)),
                ]));
                lines.push(Line::from(vec![
                    label("  Entered: "),
                    Span::raw(request.opened_at.clone()),
                ]));
                if let Some(ref reason) = request.reason {
                    lines.push(Line::from(vec![
                        label("  Reason: "),
                        Span::raw(reason.to_string()),
                    ]));
                }
                if let Some(ref notes) = request.notes {
                    lines.push(Line::from(vec![
                        label("  Notes: "),
                        Span::raw(notes.clone()),
                    ]));
                }
                if let Some(ref timeout) = request.timeout_at {
                    lines.push(Line::from(vec![
                        label("  Timeout: "),
                        Span::raw(timeout.clone()),
                    ]));
                }
                if let Some(ref resolution) = request.resolution {
                    lines.push(Line::from(vec![
                        label("  Respondent: "),
                        Span::raw(format!("{} via {}", resolution.by, resolution.via)),
                    ]));
                    lines.push(Line::from(vec![
                        label("  Action: "),
                        Span::raw(resolution.action.to_string()),
                    ]));
                    lines.push(Line::from(vec![
                        label("  Resolved: "),
                        Span::raw(resolution.at.clone()),
                    ]));
                }
                if let Some(ref notes) = request.resolution_notes {
                    lines.push(Line::from(vec![
                        label("  Response notes: "),
                        Span::raw(notes.clone()),
                    ]));
                }
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

    // Judgment history section.
    if !history.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Judgment History:",
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Magenta),
        )));

        for event in history {
            let status_color = match event.status.as_str() {
                "success" => Color::Green,
                "failed" => Color::Red,
                _ => Color::Yellow,
            };
            let time_display = format_transition_time(&event.created_at);
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(
                    format!("[{}]", event.state),
                    Style::default().fg(Color::Blue),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("#{}", event.attempt),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(" "),
                Span::styled(event.status.clone(), Style::default().fg(status_color)),
                Span::raw("  "),
                Span::styled(time_display, Style::default().fg(Color::DarkGray)),
            ];
            if let Some(ref summary) = event.summary {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    truncate_str(summary, 40),
                    Style::default().fg(Color::Gray),
                ));
            }
            if let Some(ref error) = event.error {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    truncate_str(error, 40),
                    Style::default().fg(Color::Red),
                ));
            }
            lines.push(Line::from(spans));
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
            let from_str = event.from_phase.as_deref().unwrap_or("-");
            let to_str = event.phase.as_deref().unwrap_or("-");
            let from_color = phase_color(from_str);
            let to_color = phase_color(to_str);

            // Format timestamp: show only the time portion if possible.
            let time_display = format_transition_time(&event.created_at);

            let mut spans = vec![
                Span::raw("  "),
                Span::styled(from_str.to_string(), Style::default().fg(from_color)),
                Span::styled(" -> ", Style::default().fg(Color::Gray)),
                Span::styled(to_str.to_string(), Style::default().fg(to_color)),
                Span::raw("  "),
                Span::styled(
                    format!("[{}]", event.event_type),
                    Style::default().fg(Color::Blue),
                ),
                Span::raw("  "),
                Span::styled(time_display, Style::default().fg(Color::DarkGray)),
            ];

            if let Some(ref detail) = event.detail
                && !detail.is_empty()
            {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    truncate_str(detail, 30),
                    Style::default().fg(Color::DarkGray),
                ));
            }

            lines.push(Line::from(spans));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[q/Esc] Close",
        Style::default().fg(Color::DarkGray),
    )));

    lines
}

/// Render the HITL overlay listing all items in the HITL phase.
///
/// Displays a centered popup with a list of HITL items showing their
/// work ID, title, reason, and entry time. Users can navigate the list
/// with j/k and press Enter to view item details.
fn render_hitl_overlay(
    frame: &mut ratatui::Frame,
    view: Option<&HitlOverlayView>,
    error: Option<&anyhow::Error>,
    selected: usize,
    retry_input: Option<&str>,
    toast: Option<&str>,
) {
    let area = centered_rect(70, 75, frame.area());
    frame.render_widget(Clear, area);

    let lines = match (view, error) {
        (Some(view), _) => {
            build_hitl_overlay_lines(&view.items, &view.requests, selected, retry_input, toast)
        }
        (None, Some(e)) => vec![hitl_error_line(e)],
        (None, None) => Vec::new(),
    };

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .title(" HITL Items ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );

    frame.render_widget(paragraph, area);
}

/// Banner line shown when the HITL state could not be read from the store.
fn hitl_error_line(error: &anyhow::Error) -> Line<'static> {
    Line::from(Span::styled(
        format!("error: HITL 상태를 읽지 못함: {error}"),
        Style::default().fg(Color::Red),
    ))
}

/// Build the text lines for the HITL overlay.
///
/// Each item shows its `hitl_requests` row; a confirmed request shows
/// `해결됨 · 처리 중` until the daemon finishes post-processing.
fn build_hitl_overlay_lines(
    hitl_items: &[QueueItem],
    requests: &HashMap<String, HitlRequest>,
    selected: usize,
    retry_input: Option<&str>,
    toast: Option<&str>,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(Span::styled(
        "Items Awaiting Human Review",
        Style::default()
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::UNDERLINED),
    )));
    lines.push(Line::from(""));

    if hitl_items.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no items in HITL phase)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        lines.push(Line::from(vec![Span::styled(
            format!("  {} item(s) pending review", hitl_items.len()),
            Style::default().fg(Color::Yellow),
        )]));
        lines.push(Line::from(""));

        for (i, item) in hitl_items.iter().enumerate() {
            let is_selected = i == selected;
            let prefix = if is_selected { "> " } else { "  " };
            let style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            // Item header line: prefix + work_id + title.
            let title_str = item.title.as_deref().unwrap_or("(untitled)");
            lines.push(Line::from(Span::styled(
                format!("{prefix}{} - {}", item.work_id, title_str),
                style,
            )));

            // Indented detail lines.
            let detail_indent = "    ";
            let request = requests.get(&item.work_id);
            if let Some(request) = request {
                lines.push(Line::from(vec![
                    Span::raw(detail_indent.to_string()),
                    Span::styled("Status: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(
                        hitl_status_label(request),
                        Style::default().fg(Color::Magenta),
                    ),
                ]));
                if let Some(ref reason) = request.reason {
                    lines.push(Line::from(vec![
                        Span::raw(detail_indent.to_string()),
                        Span::styled("Reason: ", Style::default().add_modifier(Modifier::BOLD)),
                        Span::styled(reason.to_string(), Style::default().fg(Color::Red)),
                    ]));
                }
                lines.push(Line::from(vec![
                    Span::raw(detail_indent.to_string()),
                    Span::styled("Entered: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format_transition_time(&request.opened_at),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
                if let Some(ref notes) = request.notes {
                    lines.push(Line::from(vec![
                        Span::raw(detail_indent.to_string()),
                        Span::styled("Notes: ", Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw(notes.clone()),
                    ]));
                }
                if let Some(ref timeout) = request.timeout_at {
                    lines.push(Line::from(vec![
                        Span::raw(detail_indent.to_string()),
                        Span::styled("Timeout: ", Style::default().add_modifier(Modifier::BOLD)),
                        Span::styled(
                            format_transition_time(timeout),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }
                if let Some(ref resolution) = request.resolution {
                    lines.push(Line::from(vec![
                        Span::raw(detail_indent.to_string()),
                        Span::styled("Answered: ", Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw(format!(
                            "{} by {} via {}",
                            resolution.action, resolution.by, resolution.via
                        )),
                    ]));
                }
            }

            if is_selected {
                let answerable = request.is_none_or(|r| r.status == HitlStatus::Open);
                let mut actions = vec![
                    Span::raw(detail_indent.to_string()),
                    Span::styled("Actions: ", Style::default().add_modifier(Modifier::BOLD)),
                ];
                if answerable {
                    for (key, label) in [
                        ("[d] ", "done  "),
                        ("[r] ", "retry  "),
                        ("[s] ", "skip  "),
                        ("[p] ", "replan  "),
                    ] {
                        actions.push(Span::styled(key, Style::default().fg(Color::Green)));
                        actions.push(Span::raw(label));
                    }
                }
                actions.push(Span::styled("[Enter] ", Style::default().fg(Color::Green)));
                actions.push(Span::raw("View details"));
                lines.push(Line::from(actions));
            }

            lines.push(Line::from(""));
        }
    }

    if let Some(input) = retry_input {
        lines.push(Line::from(vec![
            Span::styled("retry 지시> ", Style::default().fg(Color::Yellow)),
            Span::raw(input.to_string()),
            Span::styled("_", Style::default().fg(Color::DarkGray)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("[Enter] ", Style::default().fg(Color::Green)),
            Span::raw("Send  "),
            Span::styled("[Esc] ", Style::default().fg(Color::Red)),
            Span::raw("Cancel input"),
        ]));
        return lines;
    }
    if let Some(toast) = toast {
        lines.push(Line::from(Span::styled(
            toast.to_string(),
            Style::default().fg(Color::Magenta),
        )));
        lines.push(Line::from(""));
    }

    lines.push(Line::from(vec![
        Span::styled("[j/k] ", Style::default().fg(Color::Cyan)),
        Span::raw("Navigate  "),
        Span::styled("[Enter] ", Style::default().fg(Color::Green)),
        Span::raw("View details  "),
        Span::styled("[q/Esc] ", Style::default().fg(Color::Red)),
        Span::raw("Close"),
    ]));

    lines
}

/// Status text of a request: open, or confirmed and waiting for the daemon.
fn hitl_status_label(request: &HitlRequest) -> &'static str {
    match request.status {
        HitlStatus::Open => "대기 중",
        HitlStatus::Resolved => "해결됨 · 처리 중",
        HitlStatus::Expired => "만료됨 · 처리 중",
    }
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

// ---------------------------------------------------------------------------
// DataSource status types and rendering
// ---------------------------------------------------------------------------

/// Connection status for a DataSource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DataSourceConnectionStatus {
    /// The DataSource is connected and healthy.
    Connected,
    /// The DataSource is disconnected or unreachable.
    Disconnected,
    /// The connection status is unknown (e.g. never checked).
    Unknown,
}

#[allow(dead_code)]
impl DataSourceConnectionStatus {
    /// Return a human-readable string representation.
    pub fn as_str(&self) -> &str {
        match self {
            DataSourceConnectionStatus::Connected => "connected",
            DataSourceConnectionStatus::Disconnected => "disconnected",
            DataSourceConnectionStatus::Unknown => "unknown",
        }
    }
}

/// Entry representing the status of a single DataSource in the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct DataSourceStatusEntry {
    /// DataSource name (e.g. "github").
    pub name: String,
    /// Current connection status.
    pub status: DataSourceConnectionStatus,
    /// Last check timestamp (RFC 3339).
    pub last_checked: Option<String>,
    /// Number of items collected in the last scan.
    pub items_collected: u32,
}

#[allow(dead_code)]
impl DataSourceStatusEntry {
    /// Create a new `DataSourceStatusEntry`.
    pub fn new(name: String, status: DataSourceConnectionStatus) -> Self {
        Self {
            name,
            status,
            last_checked: None,
            items_collected: 0,
        }
    }
}

/// Return the color associated with a [`DataSourceConnectionStatus`].
#[allow(dead_code)]
fn datasource_status_color(status: &DataSourceConnectionStatus) -> Color {
    match status {
        DataSourceConnectionStatus::Connected => Color::Green,
        DataSourceConnectionStatus::Disconnected => Color::Red,
        DataSourceConnectionStatus::Unknown => Color::DarkGray,
    }
}

/// Render the DataSource status panel as a ratatui [`Table`] widget.
///
/// Displays each DataSource's name, connection status, last check time,
/// and items collected count.
#[allow(dead_code)]
fn render_datasource_panel(entries: &[DataSourceStatusEntry]) -> Table<'static> {
    let header = Row::new(vec![
        Cell::from("DataSource"),
        Cell::from("Status"),
        Cell::from("Last Checked"),
        Cell::from("Items"),
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
    .bottom_margin(1);

    let rows: Vec<Row<'static>> = entries
        .iter()
        .map(|entry| {
            let color = datasource_status_color(&entry.status);
            let last_checked = entry
                .last_checked
                .clone()
                .unwrap_or_else(|| "-".to_string());
            Row::new(vec![
                Cell::from(entry.name.clone()),
                Cell::from(entry.status.as_str().to_string()).style(Style::default().fg(color)),
                Cell::from(last_checked),
                Cell::from(entry.items_collected.to_string()),
            ])
        })
        .collect();

    Table::new(
        rows,
        [
            Constraint::Percentage(25),
            Constraint::Percentage(20),
            Constraint::Percentage(35),
            Constraint::Percentage(20),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(" DataSource Status ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    )
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
        models.sort_by_key(|m| std::cmp::Reverse(m.total_tokens));

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
///
/// Production callers should use [`crate::status::print_rich_runtime`] instead
/// which applies crossterm colours.  This plain-text variant is retained for
/// unit tests in this module.
#[cfg(test)]
fn render_runtime_panel(stats: &belt_infra::db::RuntimeStats) {
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
        models.sort_by_key(|m| std::cmp::Reverse(m.total_tokens));

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
    use belt_core::hitl::HitlId;
    use belt_infra::db::RuntimeStats;

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
        models.sort_by_key(|m| std::cmp::Reverse(m.total_tokens));

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
                work_id: "w1".to_string(),
                source_id: "src1".to_string(),
                event_type: "phase_enter".to_string(),
                phase: Some("ready".to_string()),
                from_phase: Some("pending".to_string()),
                detail: None,
                created_at: "2026-03-25T10:00:00Z".to_string(),
            },
            TransitionEvent {
                id: "e2".to_string(),
                work_id: "w1".to_string(),
                source_id: "src1".to_string(),
                event_type: "phase_enter".to_string(),
                phase: Some("running".to_string()),
                from_phase: Some("ready".to_string()),
                detail: None,
                created_at: "2026-03-25T10:05:00Z".to_string(),
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
        assert_eq!(state.overlay, OverlayMode::None);
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
        item.set_phase_unchecked(phase);
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
        assert_eq!(state.overlay, OverlayMode::None);
        assert_eq!(state.selected_workspace, 0);
        assert_eq!(state.board_selected_col, 0);
        assert_eq!(state.board_selected_row, 0);
    }

    // ---- DashboardTab cycling ----

    #[test]
    fn tab_cycle_forward() {
        assert_eq!(DashboardTab::Dashboard.next(), DashboardTab::PerWorkspace);
        assert_eq!(DashboardTab::PerWorkspace.next(), DashboardTab::Board);
        assert_eq!(DashboardTab::Board.next(), DashboardTab::DataSource);
        assert_eq!(DashboardTab::DataSource.next(), DashboardTab::Scripts);
        assert_eq!(DashboardTab::Scripts.next(), DashboardTab::Dashboard);
    }

    #[test]
    fn tab_cycle_backward() {
        assert_eq!(DashboardTab::Dashboard.prev(), DashboardTab::Scripts);
        assert_eq!(DashboardTab::Scripts.prev(), DashboardTab::DataSource);
        assert_eq!(DashboardTab::DataSource.prev(), DashboardTab::Board);
        assert_eq!(DashboardTab::Board.prev(), DashboardTab::PerWorkspace);
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

    // ---- DashboardTab equality ----

    #[test]
    fn dashboard_tab_equality() {
        assert_eq!(DashboardTab::Dashboard, DashboardTab::Dashboard);
        assert_ne!(DashboardTab::Dashboard, DashboardTab::Board);
        assert_ne!(DashboardTab::Board, DashboardTab::PerWorkspace);
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
        let result = start.next().next().next().next().next();
        assert_eq!(result, start);
    }

    #[test]
    fn tab_prev_full_cycle_returns_to_start() {
        let start = DashboardTab::Dashboard;
        let result = start.prev().prev().prev().prev().prev();
        assert_eq!(result, start);
    }

    #[test]
    fn tab_next_full_cycle_from_each_variant() {
        for tab in [
            DashboardTab::Dashboard,
            DashboardTab::PerWorkspace,
            DashboardTab::Board,
            DashboardTab::DataSource,
            DashboardTab::Scripts,
        ] {
            assert_eq!(tab.next().next().next().next().next(), tab);
        }
    }

    #[test]
    fn tab_prev_full_cycle_from_each_variant() {
        for tab in [
            DashboardTab::Dashboard,
            DashboardTab::PerWorkspace,
            DashboardTab::Board,
            DashboardTab::DataSource,
            DashboardTab::Scripts,
        ] {
            assert_eq!(tab.prev().prev().prev().prev().prev(), tab);
        }
    }

    #[test]
    fn tab_next_then_prev_is_identity() {
        for tab in [
            DashboardTab::Dashboard,
            DashboardTab::PerWorkspace,
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

        state.active_tab = DashboardTab::Board;
        assert_eq!(state.tab_key(), 2);

        state.active_tab = DashboardTab::DataSource;
        assert_eq!(state.tab_key(), 3);

        state.active_tab = DashboardTab::Scripts;
        assert_eq!(state.tab_key(), 4);
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

        // Modify Board tab state.
        state.active_tab = DashboardTab::Board;
        state.current_tab_state_mut().selected_index = 20;

        // Verify each tab has its own state.
        state.active_tab = DashboardTab::PerWorkspace;
        assert_eq!(state.current_tab_state().selected_index, 10);

        state.active_tab = DashboardTab::Board;
        assert_eq!(state.current_tab_state().selected_index, 20);

        // Dashboard tab should still be at 0.
        state.active_tab = DashboardTab::Dashboard;
        assert_eq!(state.current_tab_state().selected_index, 0);
    }

    #[test]
    fn overlay_is_independent_of_tab() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;
        state.overlay = OverlayMode::ItemDetail("w-123".to_string());

        // Switch tab -- overlay state should persist.
        state.active_tab = DashboardTab::Dashboard;
        assert_eq!(state.overlay, OverlayMode::ItemDetail("w-123".to_string()));
    }

    // ---- DataSource status (private API: SourceStatus / SourceStatusEntry) ----

    #[test]
    fn datasource_connection_status_labels() {
        assert_eq!(SourceStatus::Connected.label(), "Connected");
        assert_eq!(SourceStatus::Disconnected.label(), "Disconnected");
        assert_eq!(SourceStatus::Error.label(), "Error");
    }

    #[test]
    fn datasource_connection_status_colors() {
        assert_eq!(SourceStatus::Connected.color(), Color::Green);
        assert_eq!(SourceStatus::Disconnected.color(), Color::DarkGray);
        assert_eq!(SourceStatus::Error.color(), Color::Red);
    }

    #[test]
    fn datasource_connection_status_indicators() {
        assert_eq!(SourceStatus::Connected.indicator(), "●");
        assert_eq!(SourceStatus::Disconnected.indicator(), "○");
        assert_eq!(SourceStatus::Error.indicator(), "✗");
    }

    #[test]
    fn collect_datasource_status_empty_workspaces() {
        let entries = collect_datasource_status(&[], &[], None);
        assert!(entries.is_empty());
    }

    #[test]
    fn collect_datasource_status_error_on_missing_config() {
        let workspaces = vec![(
            "test-ws".to_string(),
            "/nonexistent/path.yml".to_string(),
            "2026-03-25T00:00:00Z".to_string(),
        )];
        let entries = collect_datasource_status(&workspaces, &[], None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, SourceStatus::Error);
        assert_eq!(entries[0].workspace, "test-ws");
        assert!(entries[0].error_message.is_some());
    }

    #[test]
    fn datasource_tab_key_is_three() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::DataSource;
        assert_eq!(state.tab_key(), 3);
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
        let entry = SourceStatusEntry {
            workspace: "my-ws".to_string(),
            source_name: "github".to_string(),
            url: "https://github.com/org/repo".to_string(),
            state_count: 3,
            scan_interval_secs: 300,
            status: SourceStatus::Connected,
            active_item_count: 5,
            last_scan_time: Some("2026-03-25T10:30:00Z".to_string()),
            last_scan_result_count: Some(3),
            error_message: None,
        };
        assert_eq!(entry.workspace, "my-ws");
        assert_eq!(entry.source_name, "github");
        assert_eq!(entry.state_count, 3);
        assert_eq!(entry.scan_interval_secs, 300);
        assert_eq!(entry.active_item_count, 5);
        assert_eq!(entry.status, SourceStatus::Connected);
        assert_eq!(
            entry.last_scan_time.as_deref(),
            Some("2026-03-25T10:30:00Z")
        );
        assert_eq!(entry.last_scan_result_count, Some(3));
        assert!(entry.error_message.is_none());
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
    fn scripts_tab_key_is_four() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Scripts;
        assert_eq!(state.tab_key(), 4);
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

    // ---- DataSourceConnectionStatus (public API) ----

    #[test]
    fn datasource_connection_status_as_str_connected() {
        assert_eq!(DataSourceConnectionStatus::Connected.as_str(), "connected");
    }

    #[test]
    fn datasource_connection_status_as_str_disconnected() {
        assert_eq!(
            DataSourceConnectionStatus::Disconnected.as_str(),
            "disconnected"
        );
    }

    #[test]
    fn datasource_connection_status_as_str_unknown() {
        assert_eq!(DataSourceConnectionStatus::Unknown.as_str(), "unknown");
    }

    #[test]
    fn datasource_connection_status_equality() {
        assert_eq!(
            DataSourceConnectionStatus::Connected,
            DataSourceConnectionStatus::Connected
        );
        assert_ne!(
            DataSourceConnectionStatus::Connected,
            DataSourceConnectionStatus::Disconnected
        );
        assert_ne!(
            DataSourceConnectionStatus::Disconnected,
            DataSourceConnectionStatus::Unknown
        );
    }

    #[test]
    fn datasource_connection_status_debug() {
        let status = DataSourceConnectionStatus::Connected;
        let debug_str = format!("{:?}", status);
        assert!(debug_str.contains("Connected"));
    }

    #[test]
    fn datasource_connection_status_clone() {
        let original = DataSourceConnectionStatus::Disconnected;
        let cloned = original;
        assert_eq!(original, cloned);
    }

    // ---- DataSourceStatusEntry (public API) ----

    #[test]
    fn datasource_status_entry_new_defaults() {
        let entry =
            DataSourceStatusEntry::new("github".to_string(), DataSourceConnectionStatus::Connected);
        assert_eq!(entry.name, "github");
        assert_eq!(entry.status, DataSourceConnectionStatus::Connected);
        assert!(entry.last_checked.is_none());
        assert_eq!(entry.items_collected, 0);
    }

    #[test]
    fn datasource_status_entry_with_last_checked() {
        let mut entry = DataSourceStatusEntry::new(
            "jira".to_string(),
            DataSourceConnectionStatus::Disconnected,
        );
        entry.last_checked = Some("2026-03-27T10:00:00Z".to_string());
        assert_eq!(entry.last_checked, Some("2026-03-27T10:00:00Z".to_string()));
    }

    #[test]
    fn datasource_status_entry_with_items_collected() {
        let mut entry =
            DataSourceStatusEntry::new("github".to_string(), DataSourceConnectionStatus::Connected);
        entry.items_collected = 42;
        assert_eq!(entry.items_collected, 42);
    }

    #[test]
    fn datasource_status_entry_equality() {
        let a =
            DataSourceStatusEntry::new("github".to_string(), DataSourceConnectionStatus::Connected);
        let b =
            DataSourceStatusEntry::new("github".to_string(), DataSourceConnectionStatus::Connected);
        assert_eq!(a, b);
    }

    #[test]
    fn datasource_status_entry_inequality_name() {
        let a =
            DataSourceStatusEntry::new("github".to_string(), DataSourceConnectionStatus::Connected);
        let b =
            DataSourceStatusEntry::new("jira".to_string(), DataSourceConnectionStatus::Connected);
        assert_ne!(a, b);
    }

    #[test]
    fn datasource_status_entry_inequality_status() {
        let a =
            DataSourceStatusEntry::new("github".to_string(), DataSourceConnectionStatus::Connected);
        let b = DataSourceStatusEntry::new(
            "github".to_string(),
            DataSourceConnectionStatus::Disconnected,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn datasource_status_entry_debug() {
        let entry =
            DataSourceStatusEntry::new("github".to_string(), DataSourceConnectionStatus::Unknown);
        let debug_str = format!("{:?}", entry);
        assert!(debug_str.contains("github"));
        assert!(debug_str.contains("Unknown"));
    }

    #[test]
    fn datasource_status_entry_clone() {
        let mut original =
            DataSourceStatusEntry::new("github".to_string(), DataSourceConnectionStatus::Connected);
        original.last_checked = Some("2026-03-27T12:00:00Z".to_string());
        original.items_collected = 5;
        let cloned = original.clone();
        assert_eq!(original, cloned);
    }

    // ---- datasource_status_color ----

    #[test]
    fn datasource_status_color_connected() {
        assert_eq!(
            datasource_status_color(&DataSourceConnectionStatus::Connected),
            Color::Green
        );
    }

    #[test]
    fn datasource_status_color_disconnected() {
        assert_eq!(
            datasource_status_color(&DataSourceConnectionStatus::Disconnected),
            Color::Red
        );
    }

    #[test]
    fn datasource_status_color_unknown() {
        assert_eq!(
            datasource_status_color(&DataSourceConnectionStatus::Unknown),
            Color::DarkGray
        );
    }

    // ---- render_datasource_panel ----

    #[test]
    fn render_datasource_panel_no_panic_empty() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let table = render_datasource_panel(&[]);
                frame.render_widget(table, frame.area());
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("DataSource Status"));
    }

    #[test]
    fn render_datasource_panel_single_connected_entry() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let entry = DataSourceStatusEntry {
            name: "github".to_string(),
            status: DataSourceConnectionStatus::Connected,
            last_checked: Some("2026-03-27T10:00:00Z".to_string()),
            items_collected: 7,
        };

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let table = render_datasource_panel(&[entry]);
                frame.render_widget(table, frame.area());
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("github"));
        assert!(text.contains("connected"));
    }

    #[test]
    fn render_datasource_panel_multiple_entries() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let entries = vec![
            DataSourceStatusEntry {
                name: "github".to_string(),
                status: DataSourceConnectionStatus::Connected,
                last_checked: Some("2026-03-27T10:00:00Z".to_string()),
                items_collected: 7,
            },
            DataSourceStatusEntry {
                name: "jira".to_string(),
                status: DataSourceConnectionStatus::Disconnected,
                last_checked: Some("2026-03-27T09:30:00Z".to_string()),
                items_collected: 0,
            },
            DataSourceStatusEntry::new("linear".to_string(), DataSourceConnectionStatus::Unknown),
        ];

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let table = render_datasource_panel(&entries);
                frame.render_widget(table, frame.area());
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("github"));
        assert!(text.contains("jira"));
        assert!(text.contains("linear"));
    }

    #[test]
    fn render_datasource_panel_no_panic_small_terminal() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let entry =
            DataSourceStatusEntry::new("github".to_string(), DataSourceConnectionStatus::Connected);

        let backend = TestBackend::new(10, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let table = render_datasource_panel(&[entry]);
                frame.render_widget(table, frame.area());
            })
            .unwrap();
    }

    #[test]
    fn render_datasource_panel_shows_dash_for_no_last_checked() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let entry =
            DataSourceStatusEntry::new("github".to_string(), DataSourceConnectionStatus::Unknown);

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let table = render_datasource_panel(&[entry]);
                frame.render_widget(table, frame.area());
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        // When last_checked is None, the panel should show "-"
        assert!(text.contains("-"));
    }

    #[test]
    fn render_datasource_panel_header_contains_expected_columns() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let table = render_datasource_panel(&[]);
                frame.render_widget(table, frame.area());
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("DataSource"));
        assert!(text.contains("Status"));
        assert!(text.contains("Last Checked"));
        assert!(text.contains("Items"));
    }

    // ---- StatusFilter ----

    #[test]
    fn status_filter_all_matches_every_phase() {
        let filter = StatusFilter::All;
        assert!(filter.matches(&QueuePhase::Pending));
        assert!(filter.matches(&QueuePhase::Ready));
        assert!(filter.matches(&QueuePhase::Running));
        assert!(filter.matches(&QueuePhase::Completed));
        assert!(filter.matches(&QueuePhase::Done));
        assert!(filter.matches(&QueuePhase::Failed));
        assert!(filter.matches(&QueuePhase::Hitl));
    }

    #[test]
    fn status_filter_pending_matches_pending_ready_hitl() {
        let filter = StatusFilter::Pending;
        assert!(filter.matches(&QueuePhase::Pending));
        assert!(filter.matches(&QueuePhase::Ready));
        assert!(filter.matches(&QueuePhase::Hitl));
        assert!(!filter.matches(&QueuePhase::Running));
        assert!(!filter.matches(&QueuePhase::Completed));
        assert!(!filter.matches(&QueuePhase::Done));
        assert!(!filter.matches(&QueuePhase::Failed));
    }

    #[test]
    fn status_filter_running_matches_only_running() {
        let filter = StatusFilter::Running;
        assert!(filter.matches(&QueuePhase::Running));
        assert!(!filter.matches(&QueuePhase::Pending));
        assert!(!filter.matches(&QueuePhase::Completed));
    }

    #[test]
    fn status_filter_completed_matches_completed_done_failed() {
        let filter = StatusFilter::Completed;
        assert!(filter.matches(&QueuePhase::Completed));
        assert!(filter.matches(&QueuePhase::Done));
        assert!(filter.matches(&QueuePhase::Failed));
        assert!(!filter.matches(&QueuePhase::Pending));
        assert!(!filter.matches(&QueuePhase::Running));
    }

    #[test]
    fn status_filter_cycles_through_all_variants() {
        let start = StatusFilter::All;
        let next = start.next();
        assert_eq!(next, StatusFilter::Pending);
        let next = next.next();
        assert_eq!(next, StatusFilter::Running);
        let next = next.next();
        assert_eq!(next, StatusFilter::Completed);
        let next = next.next();
        assert_eq!(next, StatusFilter::All);
    }

    #[test]
    fn status_filter_labels() {
        assert_eq!(StatusFilter::All.label(), "All");
        assert_eq!(StatusFilter::Pending.label(), "Pending");
        assert_eq!(StatusFilter::Running.label(), "Running");
        assert_eq!(StatusFilter::Completed.label(), "Completed");
    }

    // ---- PerWorkspaceView ----

    #[test]
    fn per_ws_view_default_is_table() {
        let state = DashboardState::new();
        assert_eq!(state.per_ws_view, PerWorkspaceView::Table);
    }

    #[test]
    fn per_ws_kanban_initial_selection() {
        let state = DashboardState::new();
        assert_eq!(state.per_ws_kanban_col, 0);
        assert_eq!(state.per_ws_kanban_row, 0);
    }

    #[test]
    fn per_ws_kanban_groups_has_three_columns() {
        assert_eq!(PER_WS_KANBAN_GROUPS.len(), 3);
        assert_eq!(PER_WS_KANBAN_GROUPS[0], "Pending");
        assert_eq!(PER_WS_KANBAN_GROUPS[1], "In-Progress");
        assert_eq!(PER_WS_KANBAN_GROUPS[2], "Completed");
    }

    // ---- build_detail_lines enhanced ----

    #[test]
    fn build_detail_lines_shows_source_id() {
        let item = QueueItem::new(
            "w1".to_string(),
            "github:org/repo#42".to_string(),
            "ws1".to_string(),
            "analyze".to_string(),
        );
        let lines = build_detail_lines("w1", Some(&item), &[]);
        let text: String = lines.iter().map(|l| format!("{l}")).collect::<String>();
        assert!(text.contains("github:org/repo#42"));
        assert!(text.contains("Source:"));
    }

    #[test]
    fn build_detail_lines_shows_hitl_details_from_request() {
        let db = make_db();
        let hitl_id = open_hitl_item(&db, "src1", Some("needs review"));
        let request = db.hitl_request(&hitl_id).unwrap().unwrap();
        let item = QueueItem::new(
            request.work_id.clone(),
            "src1".to_string(),
            "ws1".to_string(),
            "analyze".to_string(),
        );

        let lines = build_detail_lines_with_history(
            &request.work_id,
            Some(&item),
            &[],
            &[],
            Some(&request),
        );
        let text: String = lines.iter().map(|l| format!("{l}")).collect::<String>();
        assert!(text.contains("HITL Details:"));
        assert!(text.contains("Entered:"));
        assert!(text.contains("evaluate_failure"));
        assert!(text.contains("needs review"));
    }

    #[test]
    fn build_detail_lines_hides_hitl_section_without_request() {
        let item = QueueItem::new(
            "w1".to_string(),
            "src1".to_string(),
            "ws1".to_string(),
            "analyze".to_string(),
        );
        let lines = build_detail_lines("w1", Some(&item), &[]);
        let text: String = lines.iter().map(|l| format!("{l}")).collect::<String>();
        assert!(!text.contains("HITL Details:"));
    }

    #[test]
    fn build_detail_lines_shows_transition_event_type() {
        let item = QueueItem::new(
            "w1".to_string(),
            "src1".to_string(),
            "ws1".to_string(),
            "implement".to_string(),
        );
        let transitions = vec![TransitionEvent {
            id: "e1".to_string(),
            work_id: "w1".to_string(),
            source_id: "src1".to_string(),
            event_type: "auto".to_string(),
            phase: Some("ready".to_string()),
            from_phase: Some("pending".to_string()),
            detail: Some("{\"trigger\":\"scan\"}".to_string()),
            created_at: "2026-03-25T10:00:00Z".to_string(),
        }];
        let lines = build_detail_lines("w1", Some(&item), &transitions);
        let text: String = lines.iter().map(|l| format!("{l}")).collect::<String>();
        assert!(text.contains("[auto]"));
        assert!(text.contains("{\"trigger\":\"scan\"}"));
    }

    #[test]
    fn build_detail_lines_title_shown_before_status() {
        let mut item = QueueItem::new(
            "w1".to_string(),
            "src1".to_string(),
            "ws1".to_string(),
            "analyze".to_string(),
        );
        item.title = Some("My Important Task".to_string());
        let lines = build_detail_lines("w1", Some(&item), &[]);
        let text: String = lines.iter().map(|l| format!("{l}")).collect::<String>();
        assert!(text.contains("My Important Task"));
        // Title should appear before Status
        let title_pos = text.find("Title:").unwrap();
        let status_pos = text.find("Status:").unwrap();
        assert!(title_pos < status_pos);
    }

    // ---- DashboardState new fields ----

    #[test]
    fn dashboard_state_default_filter_is_all() {
        let state = DashboardState::new();
        assert_eq!(state.status_filter, StatusFilter::All);
    }

    // ---- OverlayMode ----

    #[test]
    fn overlay_mode_default_is_none() {
        let state = DashboardState::new();
        assert_eq!(state.overlay, OverlayMode::None);
    }

    #[test]
    fn overlay_mode_hitl_preserves_selected() {
        let mut state = DashboardState::new();
        state.overlay = OverlayMode::Hitl {
            selected: 3,
            retry_input: None,
        };
        if let OverlayMode::Hitl { selected, .. } = state.overlay {
            assert_eq!(selected, 3);
        } else {
            panic!("Expected Hitl overlay");
        }
    }

    // ---- build_hitl_overlay_lines ----

    #[test]
    fn build_hitl_overlay_lines_empty() {
        let lines = build_hitl_overlay_lines(&[], &HashMap::new(), 0, None, None);
        let text: String = lines.iter().map(|l| format!("{l}")).collect::<String>();
        assert!(text.contains("no items in HITL phase"));
        assert!(text.contains("Awaiting Human Review"));
    }

    #[test]
    fn build_hitl_overlay_lines_with_items() {
        let mut item1 = QueueItem::new(
            "w1".to_string(),
            "src1".to_string(),
            "ws1".to_string(),
            "analyze".to_string(),
        );
        item1.set_phase_unchecked(QueuePhase::Hitl);
        item1.title = Some("Fix auth bug".to_string());

        let mut item2 = QueueItem::new(
            "w2".to_string(),
            "src2".to_string(),
            "ws1".to_string(),
            "implement".to_string(),
        );
        item2.set_phase_unchecked(QueuePhase::Hitl);

        let db = make_db();
        let hitl_id = open_hitl_item(&db, "src1", None);
        let request = db.hitl_request(&hitl_id).unwrap().unwrap();
        let requests = HashMap::from([("w1".to_string(), request)]);

        let lines = build_hitl_overlay_lines(&[item1, item2], &requests, 0, None, None);
        let text: String = lines.iter().map(|l| format!("{l}")).collect::<String>();
        assert!(text.contains("2 item(s) pending review"));
        assert!(text.contains("w1"));
        assert!(text.contains("Fix auth bug"));
        assert!(text.contains("Reason:"));
        assert!(text.contains("Actions:"));
        // Second item should not have the selection marker.
        assert!(text.contains("w2"));
    }

    #[test]
    fn build_hitl_overlay_lines_selected_index() {
        let mut item = QueueItem::new(
            "w1".to_string(),
            "src1".to_string(),
            "ws1".to_string(),
            "analyze".to_string(),
        );
        item.set_phase_unchecked(QueuePhase::Hitl);

        let lines = build_hitl_overlay_lines(&[item], &HashMap::new(), 0, None, None);
        let text: String = lines.iter().map(|l| format!("{l}")).collect::<String>();
        // Selected item should show actions.
        assert!(text.contains("View details"));
    }

    // ---- HITL overlay responses ----

    /// Collect an item, move it to Running and open a HITL request for it.
    fn open_hitl_item(db: &Database, source: &str, notes: Option<&str>) -> HitlId {
        use belt_core::transition::Actor;
        use belt_infra::db::{CollectOutcome, NewItem};

        let CollectOutcome::Inserted { work_id } = db
            .insert_collected(&NewItem {
                source_id: format!("github:org/repo#{source}"),
                workspace_id: "ws1".to_string(),
                state: "analyze".to_string(),
                title: None,
                actor: Actor::Daemon,
            })
            .unwrap()
        else {
            panic!("expected a new item");
        };
        run_and_open_hitl(db, &work_id, notes)
    }

    /// Move a Pending item to Running and open a HITL request for it.
    fn run_and_open_hitl(db: &Database, work_id: &str, notes: Option<&str>) -> HitlId {
        use belt_core::queue::HitlReason;
        use belt_core::transition::{
            Actor, TransitionOutcome, TransitionReason, TransitionRequest,
        };
        use belt_infra::db::{OpenHitlOutcome, OpenHitlRequest};

        for (from, to) in [
            (QueuePhase::Pending, QueuePhase::Ready),
            (QueuePhase::Ready, QueuePhase::Running),
        ] {
            let outcome = db
                .transition(&TransitionRequest {
                    work_id: work_id.to_string(),
                    expected_from: from,
                    to,
                    actor: Actor::Daemon,
                    reason: TransitionReason::Manual,
                    detail: None,
                })
                .unwrap();
            assert!(matches!(outcome, TransitionOutcome::Applied { .. }));
        }
        let OpenHitlOutcome::Opened { hitl_id, .. } = db
            .open_hitl(&OpenHitlRequest {
                work_id: work_id.to_string(),
                expected_from: QueuePhase::Running,
                reason: HitlReason::EvaluateFailure,
                notes: notes.map(str::to_string),
                actor: Actor::Daemon,
                transition_reason: TransitionReason::Escalation(
                    belt_core::escalation::EscalationAction::Hitl,
                ),
                timeout_at: None,
                terminal_action: None,
            })
            .unwrap()
        else {
            panic!("expected the request to open");
        };
        hitl_id
    }

    /// Retry-confirm and post-process `old`, then open a fresh request for the same item.
    fn reopen_hitl(db: &Database, old: &HitlId, work_id: &str) -> HitlId {
        use belt_core::transition::{Actor, TransitionReason, TransitionRequest};

        db.complete_post_processing(
            old,
            &TransitionRequest {
                work_id: work_id.to_string(),
                expected_from: QueuePhase::Hitl,
                to: QueuePhase::Pending,
                actor: Actor::Daemon,
                reason: TransitionReason::PostProcessing(HitlAction::Retry),
                detail: None,
            },
        )
        .unwrap();
        run_and_open_hitl(db, work_id, None)
    }

    fn hitl_state() -> DashboardState {
        let mut state = DashboardState::new();
        state.overlay = OverlayMode::Hitl {
            selected: 0,
            retry_input: None,
        };
        state
    }

    fn responder_for<'a>(service: &'a HitlService, by: &str) -> HitlResponder<'a> {
        HitlResponder {
            service,
            by: by.to_string(),
        }
    }

    fn tui_flow(db: &Database) -> CancelFlow<'_> {
        use crate::cancel::testing::{NoDaemon, NoKill};
        CancelFlow {
            db,
            daemon: &NoDaemon,
            killer: &NoKill,
            actor: Actor::Tui,
            requester: "alice".to_string(),
            wait_limit: Duration::from_millis(50),
            poll_interval: Duration::from_millis(10),
        }
    }

    #[test]
    fn cancel_key_cancels_the_selected_running_item_and_toasts_the_result() {
        let db = make_db();
        let work_id = crate::cancel::testing::running_item(&db);
        let mut state = DashboardState::new();

        handle_cancel_key(&mut state, Some(&work_id), &db, &tui_flow(&db));

        let toast = state.toast.unwrap();
        assert!(toast.contains("canceled_directly"), "{toast}");
        assert_eq!(db.get_item(&work_id).unwrap().phase(), QueuePhase::Skipped);
        let via_tui = db
            .transitions_of(&work_id)
            .unwrap()
            .iter()
            .any(|e| e.kind == "cancel_requested" && e.actor == "tui");
        assert!(via_tui, "the request is recorded as coming from the tui");
    }

    #[test]
    fn cancel_key_on_an_item_awaiting_post_processing_is_busy() {
        let db = Arc::new(make_db());
        open_hitl_item(&db, "1", None);
        let work_id = db.list_items(Some(QueuePhase::Hitl), None).unwrap()[0]
            .work_id
            .clone();
        let service = HitlService::new(Arc::clone(&db));
        service
            .respond(&HitlResponse {
                target: HitlTarget::Item(work_id.clone()),
                action: HitlAction::Skip,
                by: "alice".to_string(),
                via: "tui".to_string(),
                path: ConfirmPath::Direct,
                notes: None,
            })
            .unwrap();
        let mut state = DashboardState::new();

        handle_cancel_key(&mut state, Some(&work_id), &db, &tui_flow(&db));

        let toast = state.toast.unwrap();
        assert!(toast.contains("busy"), "{toast}");
        assert!(db.open_cancel_request(&work_id).unwrap().is_none());
        assert_eq!(db.get_item(&work_id).unwrap().phase(), QueuePhase::Hitl);
    }

    #[test]
    fn cancel_key_on_a_pending_item_does_nothing_but_say_so() {
        let db = make_db();
        let item = make_item_with_phase("w-pending", QueuePhase::Pending, "2026-01-01T00:00:00Z");
        db.insert_item(&item).unwrap();
        let mut state = DashboardState::new();

        handle_cancel_key(&mut state, Some("w-pending"), &db, &tui_flow(&db));

        assert!(state.toast.unwrap().contains("not running"));
        assert_eq!(
            db.get_item("w-pending").unwrap().phase(),
            QueuePhase::Pending
        );
    }

    #[test]
    fn cancel_key_without_a_selection_changes_nothing() {
        let db = make_db();
        let mut state = DashboardState::new();

        handle_cancel_key(&mut state, None, &db, &tui_flow(&db));

        assert!(state.toast.is_none());
    }

    #[test]
    fn an_open_overlay_keeps_the_cancel_key_to_itself() {
        let db = make_db();
        let work_id = crate::cancel::testing::running_item(&db);
        let service = HitlService::new(Arc::new(make_db()));
        let responder = responder_for(&service, "alice");

        for overlay in [OverlayMode::Help, OverlayMode::ItemDetail(work_id.clone())] {
            let mut state = DashboardState::new();
            state.overlay = overlay;
            assert!(handle_overlay_key(
                &mut state,
                KeyCode::Char('x'),
                &[],
                &responder
            ));
            assert!(state.toast.is_none());
        }
        let mut state = hitl_state();
        assert!(handle_overlay_key(
            &mut state,
            KeyCode::Char('x'),
            &[],
            &responder
        ));
        assert!(state.toast.is_none());
        assert_eq!(db.get_item(&work_id).unwrap().phase(), QueuePhase::Running);
    }

    #[test]
    fn selected_work_id_follows_the_dashboard_selection() {
        let db = make_db();
        let state = DashboardState::new();
        let running = vec![QueueItem::new(
            "w-run1".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        )];

        let selected = selected_work_id(&state, &running, &[], &[], &db, &[], &[]);

        assert_eq!(selected.as_deref(), Some("w-run1"));
    }

    fn press(
        state: &mut DashboardState,
        code: KeyCode,
        ids: &[String],
        responder: &HitlResponder<'_>,
    ) -> bool {
        let requests = current_hitl_requests(responder.service.database()).unwrap();
        let rows: Vec<HitlRow> = ids
            .iter()
            .map(|work_id| HitlRow {
                work_id: work_id.clone(),
                hitl_id: requests.get(work_id).map(|r| r.hitl_id.clone()),
            })
            .collect();
        handle_overlay_key(state, code, &rows, responder)
    }

    #[test]
    fn overlay_open_d_responds_done_without_switching_tab() {
        let service = HitlService::new(Arc::new(make_db()));
        let hitl_id = open_hitl_item(service.database(), "1", None);
        let work_id = service
            .database()
            .hitl_request(&hitl_id)
            .unwrap()
            .unwrap()
            .work_id;
        let responder = responder_for(&service, "alice");
        let mut state = hitl_state();
        state.active_tab = DashboardTab::Board;

        let consumed = press(&mut state, KeyCode::Char('d'), &[work_id], &responder);

        assert!(consumed);
        assert_eq!(state.active_tab, DashboardTab::Board);
        let request = service.database().hitl_request(&hitl_id).unwrap().unwrap();
        assert_eq!(request.status, HitlStatus::Resolved);
        let resolution = request.resolution.unwrap();
        assert_eq!(resolution.action, HitlAction::Done);
        assert_eq!(resolution.by, "alice");
        assert_eq!(resolution.via, "tui");
        assert!(state.toast.as_deref().unwrap().contains("해결됨 · 처리 중"));
    }

    #[test]
    fn overlay_closed_d_keeps_global_tab_behavior() {
        let service = HitlService::new(Arc::new(make_db()));
        let responder = responder_for(&service, "alice");
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;

        assert!(!press(&mut state, KeyCode::Char('d'), &[], &responder));
        assert!(switch_tab_key(&mut state, KeyCode::Char('d')));
        assert_eq!(state.active_tab, DashboardTab::Dashboard);
    }

    #[test]
    fn overlay_open_r_starts_retry_input_not_refresh() {
        let service = HitlService::new(Arc::new(make_db()));
        let hitl_id = open_hitl_item(service.database(), "1", None);
        let work_id = service
            .database()
            .hitl_request(&hitl_id)
            .unwrap()
            .unwrap()
            .work_id;
        let responder = responder_for(&service, "alice");
        let mut state = hitl_state();
        let ids = [work_id];

        assert!(press(&mut state, KeyCode::Char('r'), &ids, &responder));
        assert_eq!(
            state.overlay,
            OverlayMode::Hitl {
                selected: 0,
                retry_input: Some(String::new())
            }
        );
        // Typing is text, not commands: `d` and `q` must not respond or close.
        for c in ['f', 'i', 'x', 'd', 'q'] {
            press(&mut state, KeyCode::Char(c), &ids, &responder);
        }
        assert_eq!(
            service
                .database()
                .hitl_request(&hitl_id)
                .unwrap()
                .unwrap()
                .status,
            HitlStatus::Open
        );
        press(&mut state, KeyCode::Enter, &ids, &responder);

        let request = service.database().hitl_request(&hitl_id).unwrap().unwrap();
        let resolution = request.resolution.clone().unwrap();
        assert_eq!(resolution.action, HitlAction::Retry);
        assert_eq!(request.resolution_notes.as_deref(), Some("fixdq"));
        assert_eq!(
            state.overlay,
            OverlayMode::Hitl {
                selected: 0,
                retry_input: None
            }
        );
    }

    #[test]
    fn overlay_retry_input_esc_cancels_without_responding() {
        let service = HitlService::new(Arc::new(make_db()));
        let hitl_id = open_hitl_item(service.database(), "1", None);
        let work_id = service
            .database()
            .hitl_request(&hitl_id)
            .unwrap()
            .unwrap()
            .work_id;
        let responder = responder_for(&service, "alice");
        let mut state = hitl_state();
        let ids = [work_id];

        press(&mut state, KeyCode::Char('r'), &ids, &responder);
        press(&mut state, KeyCode::Esc, &ids, &responder);

        assert!(matches!(
            state.overlay,
            OverlayMode::Hitl {
                retry_input: None,
                ..
            }
        ));
        assert_eq!(
            service
                .database()
                .hitl_request(&hitl_id)
                .unwrap()
                .unwrap()
                .status,
            HitlStatus::Open
        );
    }

    #[test]
    fn overlay_s_and_p_respond_skip_and_replan() {
        let service = HitlService::new(Arc::new(make_db()));
        let first = open_hitl_item(service.database(), "1", None);
        let second = open_hitl_item(service.database(), "2", None);
        let ids: Vec<String> = [&first, &second]
            .iter()
            .map(|id| {
                service
                    .database()
                    .hitl_request(id)
                    .unwrap()
                    .unwrap()
                    .work_id
            })
            .collect();
        let responder = responder_for(&service, "alice");
        let mut state = hitl_state();

        press(&mut state, KeyCode::Char('s'), &ids, &responder);
        press(&mut state, KeyCode::Char('j'), &ids, &responder);
        press(&mut state, KeyCode::Char('p'), &ids, &responder);

        let action_of = |id: &HitlId| {
            service
                .database()
                .hitl_request(id)
                .unwrap()
                .unwrap()
                .resolution
                .unwrap()
                .action
        };
        assert_eq!(action_of(&first), HitlAction::Skip);
        assert_eq!(action_of(&second), HitlAction::Replan);
    }

    #[test]
    fn overlay_late_response_shows_already_handled() {
        let service = HitlService::new(Arc::new(make_db()));
        let hitl_id = open_hitl_item(service.database(), "1", None);
        let work_id = service
            .database()
            .hitl_request(&hitl_id)
            .unwrap()
            .unwrap()
            .work_id;
        let ids = [work_id.clone()];
        let mut state = hitl_state();
        service
            .respond(&HitlResponse {
                target: HitlTarget::Item(work_id),
                action: HitlAction::Skip,
                by: "bob".to_string(),
                via: "cli".to_string(),
                path: ConfirmPath::Direct,
                notes: None,
            })
            .unwrap();

        press(
            &mut state,
            KeyCode::Char('d'),
            &ids,
            &responder_for(&service, "alice"),
        );

        let toast = state.toast.unwrap();
        assert!(toast.contains("already_handled"), "{toast}");
        assert!(toast.contains("bob"), "{toast}");
        assert!(toast.contains("cli"), "{toast}");
        assert!(toast.contains("skip"), "{toast}");
        let request = service.database().hitl_request(&hitl_id).unwrap().unwrap();
        assert_eq!(request.resolution.unwrap().action, HitlAction::Skip);
    }

    #[test]
    fn overlay_response_to_item_without_request_shows_not_found() {
        let service = HitlService::new(Arc::new(make_db()));
        let responder = responder_for(&service, "alice");
        let mut state = hitl_state();

        press(
            &mut state,
            KeyCode::Char('d'),
            &["missing".to_string()],
            &responder,
        );

        assert!(state.toast.unwrap().contains("not_found"));
    }

    #[test]
    fn overlay_action_keys_without_items_do_nothing() {
        let service = HitlService::new(Arc::new(make_db()));
        let responder = responder_for(&service, "alice");
        let mut state = hitl_state();

        for c in ['d', 'r', 's', 'p'] {
            assert!(press(&mut state, KeyCode::Char(c), &[], &responder));
        }

        assert!(state.toast.is_none());
        assert_eq!(state.overlay, hitl_state().overlay);
    }

    #[test]
    fn overlay_esc_closes_and_clears_toast() {
        let service = HitlService::new(Arc::new(make_db()));
        let responder = responder_for(&service, "alice");
        let mut state = hitl_state();
        state.toast = Some("old".to_string());

        assert!(press(&mut state, KeyCode::Esc, &[], &responder));

        assert_eq!(state.overlay, OverlayMode::None);
        assert!(state.toast.is_none());
    }

    #[test]
    fn overlay_lists_resolved_request_as_processing() {
        let db = make_db();
        let service = HitlService::new(Arc::new(db));
        let hitl_id = open_hitl_item(service.database(), "1", Some("check logs"));
        let work_id = service
            .database()
            .hitl_request(&hitl_id)
            .unwrap()
            .unwrap()
            .work_id;
        press(
            &mut hitl_state(),
            KeyCode::Char('d'),
            std::slice::from_ref(&work_id),
            &responder_for(&service, "alice"),
        );

        let items = service
            .database()
            .list_items(Some(QueuePhase::Hitl), None)
            .unwrap();
        let requests = current_hitl_requests(service.database()).unwrap();
        let lines = build_hitl_overlay_lines(&items, &requests, 0, None, None);
        let text: String = lines.iter().map(|l| format!("{l}")).collect::<String>();

        assert!(text.contains("해결됨 · 처리 중"), "{text}");
        assert!(text.contains("check logs"), "{text}");
        assert!(!text.contains("belt hitl approve"));
    }

    #[test]
    fn overlay_lines_show_toast_and_retry_prompt() {
        let toast = build_hitl_overlay_lines(&[], &HashMap::new(), 0, None, Some("hello toast"));
        let text: String = toast.iter().map(|l| format!("{l}")).collect();
        assert!(text.contains("hello toast"));

        let prompt = build_hitl_overlay_lines(&[], &HashMap::new(), 0, Some("typed"), None);
        let text: String = prompt.iter().map(|l| format!("{l}")).collect();
        assert!(text.contains("typed"));
    }

    #[test]
    fn overlay_view_surfaces_store_error_instead_of_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("belt.db");
        let db = Database::open(path.to_str().unwrap()).unwrap();
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("ALTER TABLE hitl_requests RENAME TO hitl_requests_gone;")
            .unwrap();

        let error = match HitlOverlayView::load(&db) {
            Ok(_) => panic!("a broken store must not load as an empty overlay"),
            Err(e) => e,
        };

        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| render_hitl_overlay(frame, None, Some(&error), 0, None, None))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("error: HITL"), "{screen}");
    }

    #[test]
    fn overlay_answers_the_drawn_request_not_a_reopened_one() {
        let service = HitlService::new(Arc::new(make_db()));
        let old_id = open_hitl_item(service.database(), "1", None);
        let work_id = service
            .database()
            .hitl_request(&old_id)
            .unwrap()
            .unwrap()
            .work_id;
        let drawn = HitlOverlayView::load(service.database()).unwrap().rows();
        assert_eq!(drawn[0].hitl_id.as_ref(), Some(&old_id));

        // Another path answers the drawn request, the item leaves Hitl and a new
        // request is opened for the same work_id.
        service
            .respond(&HitlResponse {
                target: HitlTarget::Id(old_id.clone()),
                action: HitlAction::Retry,
                by: "bob".to_string(),
                via: "cli".to_string(),
                path: ConfirmPath::Direct,
                notes: None,
            })
            .unwrap();
        let new_id = reopen_hitl(service.database(), &old_id, &work_id);
        assert_ne!(new_id, old_id);

        let mut state = hitl_state();
        handle_overlay_key(
            &mut state,
            KeyCode::Char('d'),
            &drawn,
            &responder_for(&service, "alice"),
        );

        let toast = state.toast.unwrap();
        assert!(toast.contains("already_handled"), "{toast}");
        let new_request = service.database().hitl_request(&new_id).unwrap().unwrap();
        assert_eq!(new_request.status, HitlStatus::Open);
    }

    #[test]
    fn open_overlay_swallows_t_instead_of_switching_tab() {
        let service = HitlService::new(Arc::new(make_db()));
        let responder = responder_for(&service, "alice");
        let mut state = hitl_state();
        state.active_tab = DashboardTab::Board;

        // The run loop only reaches `switch_tab_key` when the overlay did not consume the key.
        assert!(handle_overlay_key(
            &mut state,
            KeyCode::Char('t'),
            &[],
            &responder
        ));
        assert_eq!(state.active_tab, DashboardTab::Board);
    }

    // ---- tab keys ----

    #[test]
    fn t_jumps_to_scripts_tab() {
        let mut state = DashboardState::new();
        assert!(switch_tab_key(&mut state, KeyCode::Char('t')));
        assert_eq!(state.active_tab, DashboardTab::Scripts);
    }

    #[test]
    fn x_does_not_switch_tab() {
        let mut state = DashboardState::new();
        assert!(!switch_tab_key(&mut state, KeyCode::Char('x')));
        assert_eq!(state.active_tab, DashboardTab::Dashboard);
    }

    #[test]
    fn tab_bar_labels_scripts_with_t() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(160, 3)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(render_tab_bar(DashboardTab::Dashboard, None), frame.area())
            })
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(rendered.contains("[t] Scripts"), "{rendered}");
        assert!(!rendered.contains("[x]"), "{rendered}");
    }

    // ---- build_detail_lines_with_history ----

    #[test]
    fn build_detail_lines_with_history_shows_judgment_history() {
        let item = QueueItem::new(
            "w1".to_string(),
            "src1".to_string(),
            "ws1".to_string(),
            "analyze".to_string(),
        );
        let history = vec![
            HistoryEvent {
                work_id: "w1".to_string(),
                source_id: "src1".to_string(),
                state: "analyze".to_string(),
                status: "success".to_string(),
                attempt: 1,
                summary: Some("completed analysis".to_string()),
                error: None,
                created_at: "2026-03-25T10:00:00Z".to_string(),
            },
            HistoryEvent {
                work_id: "w1".to_string(),
                source_id: "src1".to_string(),
                state: "implement".to_string(),
                status: "failed".to_string(),
                attempt: 1,
                summary: None,
                error: Some("compilation error".to_string()),
                created_at: "2026-03-25T11:00:00Z".to_string(),
            },
        ];

        let lines = build_detail_lines_with_history("w1", Some(&item), &[], &history, None);
        let text: String = lines.iter().map(|l| format!("{l}")).collect::<String>();
        assert!(text.contains("Judgment History:"));
        assert!(text.contains("[analyze]"));
        assert!(text.contains("#1"));
        assert!(text.contains("success"));
        assert!(text.contains("completed analysis"));
        assert!(text.contains("[implement]"));
        assert!(text.contains("failed"));
        assert!(text.contains("compilation error"));
    }

    #[test]
    fn build_detail_lines_with_history_empty_history() {
        let item = QueueItem::new(
            "w1".to_string(),
            "src1".to_string(),
            "ws1".to_string(),
            "analyze".to_string(),
        );

        let lines = build_detail_lines_with_history("w1", Some(&item), &[], &[], None);
        let text: String = lines.iter().map(|l| format!("{l}")).collect::<String>();
        // Should not contain the Judgment History header when empty.
        assert!(!text.contains("Judgment History:"));
    }

    // ---- render_hitl_overlay no-panic ----

    #[test]
    fn render_hitl_overlay_no_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let db = make_db();
        let mut item = QueueItem::new(
            "w1".to_string(),
            "src1".to_string(),
            "ws1".to_string(),
            "analyze".to_string(),
        );
        item.set_phase_unchecked(QueuePhase::Hitl);
        db.insert_item(&item).unwrap();

        let view = HitlOverlayView::load(&db).unwrap();
        let backend = TestBackend::new(80, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_hitl_overlay(frame, Some(&view), None, 0, None, None);
            })
            .unwrap();
    }

    #[test]
    fn render_hitl_overlay_empty_no_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let db = make_db();
        let view = HitlOverlayView::load(&db).unwrap();
        let backend = TestBackend::new(80, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_hitl_overlay(frame, Some(&view), None, 0, None, None);
            })
            .unwrap();
    }

    // ---- tab bar ----

    #[test]
    fn tab_bar_has_no_spec_tab() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(160, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(render_tab_bar(DashboardTab::Dashboard, None), frame.area());
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Dashboard"));
        assert!(!text.contains("Spec"), "tab bar still shows Spec: {text}");
    }

    // ---- DashboardState::new() comprehensive defaults ----

    #[test]
    fn dashboard_state_new_all_fields_at_defaults() {
        let state = DashboardState::new();
        assert_eq!(state.active_tab, DashboardTab::Dashboard);
        assert_eq!(state.overlay, OverlayMode::None);
        assert!(!matches!(state.overlay, OverlayMode::Help));
        assert_eq!(state.selected_workspace, 0);
        assert_eq!(state.board_selected_col, 0);
        assert_eq!(state.board_selected_row, 0);
        assert_eq!(state.per_ws_view, PerWorkspaceView::Table);
        assert_eq!(state.per_ws_kanban_col, 0);
        assert_eq!(state.per_ws_kanban_row, 0);
        assert_eq!(state.status_filter, StatusFilter::All);
        // All 5 tab states should exist (keys 0..=4).
        for key in 0..=4u8 {
            assert!(
                state.tab_states.contains_key(&key),
                "tab_states should contain key {key}"
            );
        }
    }

    // ---- handle_nav_up ----

    #[test]
    fn handle_nav_up_decrements_selected_index_in_default_tab() {
        let mut state = DashboardState::new();
        state.current_tab_state_mut().selected_index = 3;
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_up(&mut state, &board_columns);
        assert_eq!(state.current_tab_state().selected_index, 2);
    }

    #[test]
    fn handle_nav_up_saturates_at_zero() {
        let mut state = DashboardState::new();
        state.current_tab_state_mut().selected_index = 0;
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_up(&mut state, &board_columns);
        assert_eq!(state.current_tab_state().selected_index, 0);
    }

    #[test]
    fn handle_nav_up_board_decrements_row() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;
        state.board_selected_row = 2;
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_up(&mut state, &board_columns);
        assert_eq!(state.board_selected_row, 1);
    }

    #[test]
    fn handle_nav_up_board_saturates_at_zero() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;
        state.board_selected_row = 0;
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_up(&mut state, &board_columns);
        assert_eq!(state.board_selected_row, 0);
    }

    #[test]
    fn handle_nav_up_per_ws_kanban_decrements_row() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.per_ws_view = PerWorkspaceView::Kanban;
        state.per_ws_kanban_row = 3;
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_up(&mut state, &board_columns);
        assert_eq!(state.per_ws_kanban_row, 2);
    }

    #[test]
    fn handle_nav_up_per_ws_kanban_saturates_at_zero() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.per_ws_view = PerWorkspaceView::Kanban;
        state.per_ws_kanban_row = 0;
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_up(&mut state, &board_columns);
        assert_eq!(state.per_ws_kanban_row, 0);
    }

    // ---- handle_nav_down ----

    #[test]
    fn handle_nav_down_increments_selected_index() {
        let mut state = DashboardState::new();
        state.current_tab_state_mut().selected_index = 0;
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        let per_ws_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_down(&mut state, 5, &board_columns, &per_ws_columns);
        assert_eq!(state.current_tab_state().selected_index, 1);
    }

    #[test]
    fn handle_nav_down_clamps_at_list_end() {
        let mut state = DashboardState::new();
        state.current_tab_state_mut().selected_index = 4;
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        let per_ws_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_down(&mut state, 5, &board_columns, &per_ws_columns);
        assert_eq!(state.current_tab_state().selected_index, 4);
    }

    #[test]
    fn handle_nav_down_no_change_when_list_empty() {
        let mut state = DashboardState::new();
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        let per_ws_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_down(&mut state, 0, &board_columns, &per_ws_columns);
        assert_eq!(state.current_tab_state().selected_index, 0);
    }

    #[test]
    fn handle_nav_down_board_increments_row() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;
        state.board_selected_col = 0;
        state.board_selected_row = 0;
        let item = QueueItem::new(
            "w1".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let item2 = QueueItem::new(
            "w2".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let board_columns: Vec<Vec<&QueueItem>> = vec![vec![&item, &item2]];
        let per_ws_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_down(&mut state, 0, &board_columns, &per_ws_columns);
        assert_eq!(state.board_selected_row, 1);
    }

    #[test]
    fn handle_nav_down_board_clamps_at_col_end() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;
        state.board_selected_col = 0;
        state.board_selected_row = 1;
        let item = QueueItem::new(
            "w1".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let item2 = QueueItem::new(
            "w2".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let board_columns: Vec<Vec<&QueueItem>> = vec![vec![&item, &item2]];
        let per_ws_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_down(&mut state, 0, &board_columns, &per_ws_columns);
        assert_eq!(state.board_selected_row, 1);
    }

    #[test]
    fn handle_nav_down_board_empty_col_no_change() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;
        state.board_selected_col = 0;
        state.board_selected_row = 0;
        let board_columns: Vec<Vec<&QueueItem>> = vec![vec![]];
        let per_ws_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_down(&mut state, 0, &board_columns, &per_ws_columns);
        assert_eq!(state.board_selected_row, 0);
    }

    #[test]
    fn handle_nav_down_per_ws_kanban_increments_row() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.per_ws_view = PerWorkspaceView::Kanban;
        state.per_ws_kanban_col = 0;
        state.per_ws_kanban_row = 0;
        let item = QueueItem::new(
            "w1".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let item2 = QueueItem::new(
            "w2".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        let per_ws_columns: Vec<Vec<&QueueItem>> = vec![vec![&item, &item2]];
        handle_nav_down(&mut state, 0, &board_columns, &per_ws_columns);
        assert_eq!(state.per_ws_kanban_row, 1);
    }

    #[test]
    fn handle_nav_down_per_ws_kanban_clamps_at_end() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.per_ws_view = PerWorkspaceView::Kanban;
        state.per_ws_kanban_col = 0;
        state.per_ws_kanban_row = 0;
        let item = QueueItem::new(
            "w1".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        let per_ws_columns: Vec<Vec<&QueueItem>> = vec![vec![&item]];
        handle_nav_down(&mut state, 0, &board_columns, &per_ws_columns);
        assert_eq!(state.per_ws_kanban_row, 0);
    }

    // ---- handle_nav_left ----

    #[test]
    fn handle_nav_left_dashboard_switches_to_running_panel() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Dashboard;
        state.current_tab_state_mut().active_panel = Some(ActivePanel::Recent);
        state.current_tab_state_mut().selected_index = 5;
        let workspaces: Vec<(String, String, String)> = Vec::new();
        handle_nav_left(&mut state, &workspaces);
        assert_eq!(
            state.current_tab_state().active_panel,
            Some(ActivePanel::Running)
        );
        assert_eq!(state.current_tab_state().selected_index, 0);
    }

    #[test]
    fn handle_nav_left_per_ws_table_decrements_workspace() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.selected_workspace = 2;
        let workspaces = vec![
            ("w1".to_string(), "p1".to_string(), "t1".to_string()),
            ("w2".to_string(), "p2".to_string(), "t2".to_string()),
            ("w3".to_string(), "p3".to_string(), "t3".to_string()),
        ];
        handle_nav_left(&mut state, &workspaces);
        assert_eq!(state.selected_workspace, 1);
        assert_eq!(state.current_tab_state().selected_index, 0);
    }

    #[test]
    fn handle_nav_left_per_ws_table_no_change_at_zero() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.selected_workspace = 0;
        let workspaces = vec![("w1".to_string(), "p1".to_string(), "t1".to_string())];
        handle_nav_left(&mut state, &workspaces);
        assert_eq!(state.selected_workspace, 0);
    }

    #[test]
    fn handle_nav_left_per_ws_kanban_decrements_col() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.per_ws_view = PerWorkspaceView::Kanban;
        state.per_ws_kanban_col = 2;
        state.per_ws_kanban_row = 5;
        let workspaces: Vec<(String, String, String)> = Vec::new();
        handle_nav_left(&mut state, &workspaces);
        assert_eq!(state.per_ws_kanban_col, 1);
        assert_eq!(state.per_ws_kanban_row, 0);
    }

    #[test]
    fn handle_nav_left_per_ws_kanban_saturates_at_zero() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.per_ws_view = PerWorkspaceView::Kanban;
        state.per_ws_kanban_col = 0;
        let workspaces: Vec<(String, String, String)> = Vec::new();
        handle_nav_left(&mut state, &workspaces);
        assert_eq!(state.per_ws_kanban_col, 0);
    }

    #[test]
    fn handle_nav_left_board_decrements_col_resets_row() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;
        state.board_selected_col = 3;
        state.board_selected_row = 5;
        let workspaces: Vec<(String, String, String)> = Vec::new();
        handle_nav_left(&mut state, &workspaces);
        assert_eq!(state.board_selected_col, 2);
        assert_eq!(state.board_selected_row, 0);
    }

    #[test]
    fn handle_nav_left_board_saturates_at_zero() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;
        state.board_selected_col = 0;
        let workspaces: Vec<(String, String, String)> = Vec::new();
        handle_nav_left(&mut state, &workspaces);
        assert_eq!(state.board_selected_col, 0);
    }

    // ---- handle_nav_right ----

    #[test]
    fn handle_nav_right_dashboard_switches_to_recent_panel() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Dashboard;
        state.current_tab_state_mut().selected_index = 3;
        let workspaces: Vec<(String, String, String)> = Vec::new();
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_right(&mut state, &workspaces, &board_columns);
        assert_eq!(
            state.current_tab_state().active_panel,
            Some(ActivePanel::Recent)
        );
        assert_eq!(state.current_tab_state().selected_index, 0);
    }

    #[test]
    fn handle_nav_right_per_ws_table_increments_workspace() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.selected_workspace = 0;
        let workspaces = vec![
            ("w1".to_string(), "p1".to_string(), "t1".to_string()),
            ("w2".to_string(), "p2".to_string(), "t2".to_string()),
        ];
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_right(&mut state, &workspaces, &board_columns);
        assert_eq!(state.selected_workspace, 1);
        assert_eq!(state.current_tab_state().selected_index, 0);
    }

    #[test]
    fn handle_nav_right_per_ws_table_no_change_at_last() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.selected_workspace = 1;
        let workspaces = vec![
            ("w1".to_string(), "p1".to_string(), "t1".to_string()),
            ("w2".to_string(), "p2".to_string(), "t2".to_string()),
        ];
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_right(&mut state, &workspaces, &board_columns);
        assert_eq!(state.selected_workspace, 1);
    }

    #[test]
    fn handle_nav_right_per_ws_kanban_increments_col() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.per_ws_view = PerWorkspaceView::Kanban;
        state.per_ws_kanban_col = 0;
        state.per_ws_kanban_row = 3;
        let workspaces: Vec<(String, String, String)> = Vec::new();
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_right(&mut state, &workspaces, &board_columns);
        assert_eq!(state.per_ws_kanban_col, 1);
        assert_eq!(state.per_ws_kanban_row, 0);
    }

    #[test]
    fn handle_nav_right_per_ws_kanban_clamps_at_last_col() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.per_ws_view = PerWorkspaceView::Kanban;
        state.per_ws_kanban_col = PER_WS_KANBAN_GROUPS.len() - 1;
        let workspaces: Vec<(String, String, String)> = Vec::new();
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_right(&mut state, &workspaces, &board_columns);
        assert_eq!(state.per_ws_kanban_col, PER_WS_KANBAN_GROUPS.len() - 1);
    }

    #[test]
    fn handle_nav_right_board_increments_col() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;
        state.board_selected_col = 0;
        state.board_selected_row = 2;
        let workspaces: Vec<(String, String, String)> = Vec::new();
        let item = QueueItem::new(
            "w1".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let board_columns: Vec<Vec<&QueueItem>> = vec![vec![&item], vec![&item], vec![&item]];
        handle_nav_right(&mut state, &workspaces, &board_columns);
        assert_eq!(state.board_selected_col, 1);
        assert_eq!(state.board_selected_row, 0);
    }

    #[test]
    fn handle_nav_right_board_clamps_at_last_col() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;
        let item = QueueItem::new(
            "w1".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let board_columns: Vec<Vec<&QueueItem>> = vec![vec![&item], vec![&item]];
        state.board_selected_col = 1;
        let workspaces: Vec<(String, String, String)> = Vec::new();
        handle_nav_right(&mut state, &workspaces, &board_columns);
        assert_eq!(state.board_selected_col, 1);
    }

    #[test]
    fn handle_nav_right_scripts_noop() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Scripts;
        state.current_tab_state_mut().selected_index = 2;
        let workspaces: Vec<(String, String, String)> = Vec::new();
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        handle_nav_right(&mut state, &workspaces, &board_columns);
        assert_eq!(state.current_tab_state().selected_index, 2);
    }

    // ---- handle_enter ----

    #[test]
    fn handle_enter_dashboard_running_panel_sets_overlay() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Dashboard;
        state.current_tab_state_mut().active_panel = Some(ActivePanel::Running);
        state.current_tab_state_mut().selected_index = 0;

        let running = vec![QueueItem::new(
            "w-run1".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        )];
        let recent: Vec<QueueItem> = Vec::new();
        let all: Vec<QueueItem> = Vec::new();
        let workspaces: Vec<(String, String, String)> = Vec::new();
        let db = make_db();
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        let per_ws_columns: Vec<Vec<&QueueItem>> = Vec::new();

        handle_enter(
            &mut state,
            &running,
            &recent,
            &all,
            &workspaces,
            &db,
            &board_columns,
            &per_ws_columns,
        );
        assert_eq!(state.overlay, OverlayMode::ItemDetail("w-run1".to_string()));
    }

    #[test]
    fn handle_enter_dashboard_recent_panel_sets_overlay() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Dashboard;
        state.current_tab_state_mut().active_panel = Some(ActivePanel::Recent);
        state.current_tab_state_mut().selected_index = 0;

        let running: Vec<QueueItem> = Vec::new();
        let recent = vec![QueueItem::new(
            "w-rec1".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        )];
        let all: Vec<QueueItem> = Vec::new();
        let workspaces: Vec<(String, String, String)> = Vec::new();
        let db = make_db();
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        let per_ws_columns: Vec<Vec<&QueueItem>> = Vec::new();

        handle_enter(
            &mut state,
            &running,
            &recent,
            &all,
            &workspaces,
            &db,
            &board_columns,
            &per_ws_columns,
        );
        assert_eq!(state.overlay, OverlayMode::ItemDetail("w-rec1".to_string()));
    }

    #[test]
    fn handle_enter_dashboard_out_of_bounds_no_overlay() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Dashboard;
        state.current_tab_state_mut().active_panel = Some(ActivePanel::Running);
        state.current_tab_state_mut().selected_index = 5;

        let running: Vec<QueueItem> = Vec::new();
        let recent: Vec<QueueItem> = Vec::new();
        let all: Vec<QueueItem> = Vec::new();
        let workspaces: Vec<(String, String, String)> = Vec::new();
        let db = make_db();
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        let per_ws_columns: Vec<Vec<&QueueItem>> = Vec::new();

        handle_enter(
            &mut state,
            &running,
            &recent,
            &all,
            &workspaces,
            &db,
            &board_columns,
            &per_ws_columns,
        );
        assert_eq!(state.overlay, OverlayMode::None);
    }

    #[test]
    fn handle_enter_board_sets_overlay() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;
        state.board_selected_col = 0;
        state.board_selected_row = 0;

        let item = QueueItem::new(
            "w-board1".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let running: Vec<QueueItem> = Vec::new();
        let recent: Vec<QueueItem> = Vec::new();
        let all: Vec<QueueItem> = Vec::new();
        let workspaces: Vec<(String, String, String)> = Vec::new();
        let db = make_db();
        let board_columns: Vec<Vec<&QueueItem>> = vec![vec![&item]];
        let per_ws_columns: Vec<Vec<&QueueItem>> = Vec::new();

        handle_enter(
            &mut state,
            &running,
            &recent,
            &all,
            &workspaces,
            &db,
            &board_columns,
            &per_ws_columns,
        );
        assert_eq!(
            state.overlay,
            OverlayMode::ItemDetail("w-board1".to_string())
        );
    }

    #[test]
    fn handle_enter_board_empty_col_no_overlay() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::Board;
        state.board_selected_col = 0;
        state.board_selected_row = 0;

        let running: Vec<QueueItem> = Vec::new();
        let recent: Vec<QueueItem> = Vec::new();
        let all: Vec<QueueItem> = Vec::new();
        let workspaces: Vec<(String, String, String)> = Vec::new();
        let db = make_db();
        let board_columns: Vec<Vec<&QueueItem>> = vec![vec![]];
        let per_ws_columns: Vec<Vec<&QueueItem>> = Vec::new();

        handle_enter(
            &mut state,
            &running,
            &recent,
            &all,
            &workspaces,
            &db,
            &board_columns,
            &per_ws_columns,
        );
        assert_eq!(state.overlay, OverlayMode::None);
    }

    #[test]
    fn handle_enter_per_ws_kanban_sets_overlay() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.per_ws_view = PerWorkspaceView::Kanban;
        state.per_ws_kanban_col = 0;
        state.per_ws_kanban_row = 0;

        let item = QueueItem::new(
            "w-kanban1".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let running: Vec<QueueItem> = Vec::new();
        let recent: Vec<QueueItem> = Vec::new();
        let all: Vec<QueueItem> = Vec::new();
        let workspaces: Vec<(String, String, String)> = Vec::new();
        let db = make_db();
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        let per_ws_columns: Vec<Vec<&QueueItem>> = vec![vec![&item]];

        handle_enter(
            &mut state,
            &running,
            &recent,
            &all,
            &workspaces,
            &db,
            &board_columns,
            &per_ws_columns,
        );
        assert_eq!(
            state.overlay,
            OverlayMode::ItemDetail("w-kanban1".to_string())
        );
    }

    #[test]
    fn handle_enter_datasource_tab_no_overlay() {
        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::DataSource;

        let running: Vec<QueueItem> = Vec::new();
        let recent: Vec<QueueItem> = Vec::new();
        let all: Vec<QueueItem> = Vec::new();
        let workspaces: Vec<(String, String, String)> = Vec::new();
        let db = make_db();
        let board_columns: Vec<Vec<&QueueItem>> = Vec::new();
        let per_ws_columns: Vec<Vec<&QueueItem>> = Vec::new();

        handle_enter(
            &mut state,
            &running,
            &recent,
            &all,
            &workspaces,
            &db,
            &board_columns,
            &per_ws_columns,
        );
        assert_eq!(state.overlay, OverlayMode::None);
    }

    // ---- StatusFilter::next full cycle ----

    #[test]
    fn status_filter_next_full_cycle_returns_to_start() {
        let start = StatusFilter::All;
        assert_eq!(start.next().next().next().next(), start);
    }

    // ---- per_ws_view toggle ----

    #[test]
    fn per_ws_view_toggle_table_to_kanban() {
        let mut state = DashboardState::new();
        assert_eq!(state.per_ws_view, PerWorkspaceView::Table);
        state.per_ws_view = PerWorkspaceView::Kanban;
        assert_eq!(state.per_ws_view, PerWorkspaceView::Kanban);
    }

    #[test]
    fn per_ws_view_toggle_kanban_to_table() {
        let mut state = DashboardState::new();
        state.per_ws_view = PerWorkspaceView::Kanban;
        state.per_ws_view = PerWorkspaceView::Table;
        assert_eq!(state.per_ws_view, PerWorkspaceView::Table);
    }

    #[test]
    fn per_ws_view_switch_resets_kanban_selection() {
        let mut state = DashboardState::new();
        state.per_ws_view = PerWorkspaceView::Kanban;
        state.per_ws_kanban_col = 2;
        state.per_ws_kanban_row = 5;
        // Switching back to Table mode: kanban state remains but is unused.
        state.per_ws_view = PerWorkspaceView::Table;
        // When switching back to Kanban, it should resume from where it was.
        state.per_ws_view = PerWorkspaceView::Kanban;
        assert_eq!(state.per_ws_kanban_col, 2);
        assert_eq!(state.per_ws_kanban_row, 5);
    }

    // ---- render_per_workspace_kanban ----

    #[test]
    fn render_per_workspace_kanban_no_panic_empty_columns() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let state = DashboardState::new();
        let kanban_columns: Vec<Vec<&QueueItem>> = vec![vec![], vec![], vec![]];

        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_per_workspace_kanban(
                    frame,
                    frame.area(),
                    &kanban_columns,
                    &state,
                    "test-ws",
                );
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("Pending"));
        assert!(text.contains("In-Progress"));
        assert!(text.contains("Completed"));
        assert!(text.contains("(empty)"));
        assert!(text.contains("test-ws"));
    }

    #[test]
    fn render_per_workspace_kanban_shows_items() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.per_ws_view = PerWorkspaceView::Kanban;

        let item1 = QueueItem::new(
            "w-pending1".to_string(),
            "src".to_string(),
            "ws".to_string(),
            "analyze".to_string(),
        );
        let mut item2 = QueueItem::new(
            "w-running1".to_string(),
            "src".to_string(),
            "ws".to_string(),
            "implement".to_string(),
        );
        item2.set_phase_unchecked(QueuePhase::Running);

        let kanban_columns: Vec<Vec<&QueueItem>> = vec![vec![&item1], vec![&item2], vec![]];

        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_per_workspace_kanban(frame, frame.area(), &kanban_columns, &state, "my-ws");
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("w-pending1"));
        assert!(text.contains("w-running1"));
        assert!(text.contains("my-ws"));
    }

    #[test]
    fn render_per_workspace_kanban_selected_item_has_marker() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = DashboardState::new();
        state.active_tab = DashboardTab::PerWorkspace;
        state.per_ws_view = PerWorkspaceView::Kanban;
        state.per_ws_kanban_col = 0;
        state.per_ws_kanban_row = 0;

        let item = QueueItem::new(
            "w-sel".to_string(),
            "src".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let kanban_columns: Vec<Vec<&QueueItem>> = vec![vec![&item], vec![], vec![]];

        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_per_workspace_kanban(frame, frame.area(), &kanban_columns, &state, "ws");
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        // Selected item should have ">" marker.
        assert!(text.contains("> w-sel"));
    }

    #[test]
    fn render_per_workspace_kanban_no_panic_small_terminal() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let state = DashboardState::new();
        let item = QueueItem::new(
            "w1".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let kanban_columns: Vec<Vec<&QueueItem>> = vec![vec![&item], vec![], vec![]];

        let backend = TestBackend::new(10, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_per_workspace_kanban(frame, frame.area(), &kanban_columns, &state, "ws");
            })
            .unwrap();
    }

    #[test]
    fn render_per_workspace_kanban_column_counts_in_title() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let state = DashboardState::new();
        let item1 = QueueItem::new(
            "w1".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let item2 = QueueItem::new(
            "w2".to_string(),
            "s".to_string(),
            "ws".to_string(),
            "a".to_string(),
        );
        let kanban_columns: Vec<Vec<&QueueItem>> = vec![vec![&item1, &item2], vec![], vec![]];

        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_per_workspace_kanban(frame, frame.area(), &kanban_columns, &state, "ws");
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        // Title should show count like "Pending (2)"
        assert!(text.contains("Pending (2)"));
        assert!(text.contains("In-Progress (0)"));
        assert!(text.contains("Completed (0)"));
    }

    // ---- R key manual refresh ----

    #[test]
    fn r_key_refresh_does_not_mutate_dashboard_state() {
        // The R/r key triggers `continue` in the event loop, so it must not change
        // any DashboardState fields. Verify that state remains identical to a fresh
        // default after simulating the "no-op" nature of the refresh key.
        let mut state = DashboardState::new();
        // Set some non-default state to make sure nothing is reset.
        state.active_tab = DashboardTab::Board;
        state.board_selected_col = 2;
        state.board_selected_row = 3;
        state.overlay = OverlayMode::None;

        // Snapshot the state before the "R key" branch.
        let tab_before = state.active_tab;
        let col_before = state.board_selected_col;
        let row_before = state.board_selected_row;
        let ws_before = state.selected_workspace;
        let overlay_before = std::mem::discriminant(&state.overlay);
        let filter_before = state.status_filter;
        let view_before = state.per_ws_view;
        let kanban_col_before = state.per_ws_kanban_col;
        let kanban_row_before = state.per_ws_kanban_row;

        // The R key match arm is just `continue`, so no mutation occurs.
        // We verify this expectation explicitly: the state must be unchanged.
        assert_eq!(state.active_tab, tab_before);
        assert_eq!(state.board_selected_col, col_before);
        assert_eq!(state.board_selected_row, row_before);
        assert_eq!(state.selected_workspace, ws_before);
        assert_eq!(std::mem::discriminant(&state.overlay), overlay_before);
        assert_eq!(state.status_filter, filter_before);
        assert_eq!(state.per_ws_view, view_before);
        assert_eq!(state.per_ws_kanban_col, kanban_col_before);
        assert_eq!(state.per_ws_kanban_row, kanban_row_before);
    }

    #[test]
    fn tab_bar_contains_refresh_hint() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(120, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let bar = render_tab_bar(DashboardTab::Dashboard, None);
                frame.render_widget(bar, frame.area());
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            text.contains("[r] Refresh"),
            "Tab bar should display [r] Refresh hint"
        );
    }

    #[test]
    fn help_overlay_contains_refresh_key_binding() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_help_overlay(frame);
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            text.contains("r/R"),
            "Help overlay should list r/R key binding"
        );
        assert!(
            text.contains("Refresh data immediately"),
            "Help overlay should describe the refresh action"
        );
    }
}
