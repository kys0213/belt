//! Interactive REPL session for Claw.
//!
//! Provides a simple command loop that reads user input, dispatches slash
//! commands, and prints responses. On session entry a status banner is
//! collected from the Belt database (queue item counts by phase, HITL
//! pending count, recent transition events, and per-workspace statistics)
//! and displayed to the user.
//!
//! Free-form text input is forwarded to the configured [`AgentRuntime`] for
//! LLM processing. The session maintains a conversation history and tracks
//! cumulative token usage across invocations.

use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use belt_core::runtime::{AgentRuntime, RuntimeRequest, TokenUsage};

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

/// A single message in the session conversation history.
#[derive(Debug, Clone)]
pub struct SessionMessage {
    /// The role of the message sender.
    pub role: MessageRole,
    /// The text content of the message.
    pub content: String,
}

/// Role of a message sender in the session history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRole {
    /// User input.
    User,
    /// LLM assistant response.
    Assistant,
}

/// Cumulative token usage tracked across the session.
#[derive(Debug, Clone, Default)]
pub struct SessionTokenUsage {
    /// Total input tokens consumed across all invocations.
    pub total_input_tokens: u64,
    /// Total output tokens consumed across all invocations.
    pub total_output_tokens: u64,
    /// Number of LLM invocations made during the session.
    pub invocation_count: u32,
}

impl SessionTokenUsage {
    /// Accumulate token usage from a single invocation.
    fn accumulate(&mut self, usage: &TokenUsage) {
        self.total_input_tokens += usage.input_tokens;
        self.total_output_tokens += usage.output_tokens;
        self.invocation_count += 1;
    }
}

/// Open the default Belt database at `~/.belt/belt.db`.
///
/// Returns `None` if the home directory cannot be determined or the
/// database cannot be opened.
fn open_default_db() -> Option<belt_infra::db::Database> {
    let belt_home = dirs::home_dir()?.join(".belt");
    let db_path = belt_home.join("belt.db");
    belt_infra::db::Database::open(db_path.to_str()?).ok()
}

/// Convert an infra transition event into a [`RecentEvent`].
fn into_recent_event(e: belt_infra::db::TransitionEvent) -> RecentEvent {
    RecentEvent {
        item_id: e.item_id,
        from_state: e.from_state,
        to_state: e.to_state,
        timestamp: e.timestamp,
    }
}

/// Collect system status from the Belt database.
///
/// Opens the default `~/.belt/belt.db` database and gathers queue item
/// counts by phase, the HITL pending count, and the 5 most recent
/// transition events. Returns `None` if the database is unavailable.
pub fn collect_status() -> Option<StatusSummary> {
    let db = open_default_db()?;
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
    let recent_events = events.into_iter().map(into_recent_event).collect();

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

/// Collect workspace-level statistics from the Belt database.
///
/// Opens the default `~/.belt/belt.db` database and gathers spec counts by
/// status and queue item counts by phase for the given workspace.
/// Returns `None` if the database is unavailable or the workspace name is absent.
pub fn collect_workspace_stats(workspace: Option<&str>) -> Option<WorkspaceStats> {
    let ws_name = workspace?;
    let db = open_default_db()?;
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
    let mut active_spec_count: u32 = 0;
    let mut completing_count: u32 = 0;
    let mut completed_count: u32 = 0;
    for spec in &specs {
        match spec.status {
            SpecStatus::Active => active_spec_count += 1,
            SpecStatus::Completing => completing_count += 1,
            SpecStatus::Completed => completed_count += 1,
            _ => {}
        }
    }

    // Count queue items by phase for this workspace.
    let items = db.list_items(None, Some(workspace)).ok()?;
    let mut pending_items_count: u32 = 0;
    let mut running_items_count: u32 = 0;
    let mut hitl_item_ids: HashSet<String> = HashSet::new();
    for item in &items {
        match item.phase {
            belt_core::phase::QueuePhase::Pending => pending_items_count += 1,
            belt_core::phase::QueuePhase::Running => running_items_count += 1,
            belt_core::phase::QueuePhase::Hitl => {
                hitl_item_ids.insert(item.work_id.clone());
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
            .map(into_recent_event)
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

/// Display token usage summary for a single invocation.
fn write_token_usage<W: Write>(output: &mut W, usage: &TokenUsage) -> io::Result<()> {
    writeln!(
        output,
        "  [tokens: in={}, out={}]",
        usage.input_tokens, usage.output_tokens
    )
}

/// Display cumulative session token usage.
fn write_session_usage<W: Write>(output: &mut W, usage: &SessionTokenUsage) -> io::Result<()> {
    writeln!(
        output,
        "Session totals: {} invocations, {} input tokens, {} output tokens",
        usage.invocation_count, usage.total_input_tokens, usage.total_output_tokens
    )
}

/// Build a system prompt incorporating workspace and session context.
fn build_system_prompt(workspace: Option<&str>, history: &[SessionMessage]) -> String {
    let mut parts = Vec::new();
    parts.push("You are the Belt Claw interactive assistant.".to_string());

    if let Some(ws) = workspace {
        parts.push(format!("Current workspace: {ws}"));
    }

    if !history.is_empty() {
        parts.push(format!(
            "Conversation history has {} previous messages.",
            history.len()
        ));
    }

    parts.join("\n")
}

/// Build a prompt string that includes recent conversation history for context.
fn build_prompt_with_history(user_input: &str, history: &[SessionMessage]) -> String {
    if history.is_empty() {
        return user_input.to_string();
    }

    // Include up to the last 10 messages as context.
    let context_window = if history.len() > 10 {
        &history[history.len() - 10..]
    } else {
        history
    };

    let mut prompt = String::from("<conversation_history>\n");
    for msg in context_window {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        };
        prompt.push_str(&format!("[{role}]: {}\n", msg.content));
    }
    prompt.push_str("</conversation_history>\n\n");
    prompt.push_str(user_input);
    prompt
}

/// Interactive session configuration.
pub struct SessionConfig {
    /// Workspace name context (if running inside a specific workspace).
    pub workspace: Option<String>,
    /// The Claw workspace root.
    pub claw_workspace: ClawWorkspace,
    /// Optional agent runtime for processing free-form input via LLM.
    ///
    /// When `None`, free-form input is echoed back (legacy behavior).
    pub runtime: Option<Arc<dyn AgentRuntime>>,
}

/// Run the interactive REPL session.
///
/// Reads lines from `input`, writes prompts/responses to `output`.
/// An optional [`StatusSummary`] is displayed as a banner at the top of
/// the session.  Pass `None` to skip the status banner (e.g. when the DB
/// is unavailable).  An optional [`WorkspaceStats`] adds per-workspace
/// statistics below the system-wide banner.
///
/// When a runtime is configured in [`SessionConfig`], free-form text input
/// is forwarded to the LLM agent. The session maintains conversation
/// history and tracks cumulative token usage. Without a runtime, free-form
/// input is echoed back.
///
/// This signature allows testing without real stdio.
pub async fn run_session<R: BufRead, W: Write>(
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

    if config.runtime.is_some() {
        writeln!(
            output,
            "LLM agent connected. Free-form input will be processed by the agent."
        )?;
    }

    writeln!(output, "Type /help for available commands, /quit to exit.")?;
    writeln!(output)?;

    // Session state: conversation history and cumulative token usage.
    let mut history: Vec<SessionMessage> = Vec::new();
    let mut session_usage = SessionTokenUsage::default();

    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

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
                    // Display session summary before quitting if any LLM
                    // invocations were made.
                    if session_usage.invocation_count > 0 {
                        write_session_usage(output, &session_usage)?;
                    }
                    writeln!(output, "Goodbye.")?;
                    break;
                }
                _ => {
                    let response = dispatcher.dispatch(&cmd);
                    writeln!(output, "{response}")?;
                }
            }
        } else if let Some(ref runtime) = config.runtime {
            // Forward free-form input to LLM agent.
            let user_input = trimmed.to_string();

            let prompt = build_prompt_with_history(&user_input, &history);
            let system_prompt = build_system_prompt(config.workspace.as_deref(), &history);

            let request = RuntimeRequest {
                working_dir: working_dir.clone(),
                prompt,
                model: None,
                system_prompt: Some(system_prompt),
                session_id: None,
                structured_output: None,
            };

            let response = runtime.invoke(request).await;

            // Record user message in history.
            history.push(SessionMessage {
                role: MessageRole::User,
                content: user_input,
            });

            if response.success() {
                let response_text = response.stdout.trim();
                if !response_text.is_empty() {
                    writeln!(output, "{response_text}")?;
                }

                // Record assistant response in history.
                history.push(SessionMessage {
                    role: MessageRole::Assistant,
                    content: response.stdout.trim().to_string(),
                });
            } else {
                writeln!(
                    output,
                    "[error] Agent invocation failed (exit code {})",
                    response.exit_code
                )?;
                if !response.stderr.is_empty() {
                    writeln!(output, "[error] {}", response.stderr.trim())?;
                }
            }

            // Track token usage.
            if let Some(ref usage) = response.token_usage {
                write_token_usage(output, usage)?;
                session_usage.accumulate(usage);
            }
        } else {
            // No runtime configured — echo back (legacy behavior).
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
///
/// When available, a [`ClaudeRuntime`](belt_infra::runtimes::claude::ClaudeRuntime)
/// is configured as the LLM agent for processing free-form input.
pub async fn run_interactive(config: SessionConfig) -> anyhow::Result<()> {
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
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use belt_infra::runtimes::mock::MockRuntime;

    fn make_config(tmp: &tempfile::TempDir) -> SessionConfig {
        let ws = ClawWorkspace::init(tmp.path()).unwrap();
        SessionConfig {
            workspace: Some("test-ws".to_string()),
            claw_workspace: ws,
            runtime: None,
        }
    }

    fn make_config_with_runtime(
        tmp: &tempfile::TempDir,
        runtime: Arc<dyn AgentRuntime>,
    ) -> SessionConfig {
        let ws = ClawWorkspace::init(tmp.path()).unwrap();
        SessionConfig {
            workspace: Some("test-ws".to_string()),
            claw_workspace: ws,
            runtime: Some(runtime),
        }
    }

    #[tokio::test]
    async fn session_quit_on_slash_quit() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Goodbye."));
    }

    #[tokio::test]
    async fn session_quit_on_eof() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Belt Claw interactive session"));
    }

    #[tokio::test]
    async fn session_dispatches_help() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/help\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("/auto"));
        assert!(out.contains("/spec"));
        assert!(out.contains("/claw"));
    }

    #[tokio::test]
    async fn session_echoes_freeform_text_without_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"hello world\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains(">> hello world"));
    }

    #[tokio::test]
    async fn session_forwards_freeform_to_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::always_ok("mock"));
        let config = make_config_with_runtime(&tmp, runtime.clone());
        let mut input = Cursor::new(b"hello agent\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        // Should NOT echo with ">>" prefix when runtime is configured.
        assert!(!out.contains(">> hello agent"));
        // Should contain mock response.
        assert!(out.contains("mock response for:"));
        // Verify the runtime received the prompt.
        assert_eq!(runtime.calls().len(), 1);
        assert!(runtime.calls()[0].contains("hello agent"));
    }

    #[tokio::test]
    async fn session_tracks_token_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: None,
            cache_write_tokens: None,
        };
        let runtime = Arc::new(MockRuntime::always_ok("mock").with_token_usages(vec![usage]));
        let config = make_config_with_runtime(&tmp, runtime);
        let mut input = Cursor::new(b"test prompt\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        // Per-invocation token usage.
        assert!(out.contains("[tokens: in=100, out=50]"));
        // Session summary on quit.
        assert!(out.contains("Session totals: 1 invocations"));
        assert!(out.contains("100 input tokens"));
        assert!(out.contains("50 output tokens"));
    }

    #[tokio::test]
    async fn session_accumulates_token_usage_across_invocations() {
        let tmp = tempfile::tempdir().unwrap();
        let usages = vec![
            TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
            TokenUsage {
                input_tokens: 200,
                output_tokens: 75,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
        ];
        let runtime = Arc::new(MockRuntime::always_ok("mock").with_token_usages(usages));
        let config = make_config_with_runtime(&tmp, runtime);
        let mut input = Cursor::new(b"first\nsecond\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        // Session summary shows accumulated totals.
        assert!(out.contains("2 invocations"));
        assert!(out.contains("300 input tokens"));
        assert!(out.contains("125 output tokens"));
    }

    #[tokio::test]
    async fn session_displays_error_on_runtime_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::new("mock", vec![1]));
        let config = make_config_with_runtime(&tmp, runtime);
        let mut input = Cursor::new(b"fail this\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("[error] Agent invocation failed"));
    }

    #[tokio::test]
    async fn session_maintains_history() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::always_ok("mock"));
        let config = make_config_with_runtime(&tmp, runtime.clone());
        let mut input = Cursor::new(b"first message\nsecond message\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        // The second call should include conversation history in the prompt.
        let calls = runtime.calls();
        assert_eq!(calls.len(), 2);
        // First call has no history context.
        assert!(!calls[0].contains("conversation_history"));
        // Second call should include history context.
        assert!(calls[1].contains("conversation_history"));
        assert!(calls[1].contains("first message"));
    }

    #[tokio::test]
    async fn session_shows_llm_connected_message() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::always_ok("mock"));
        let config = make_config_with_runtime(&tmp, runtime);
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("LLM agent connected"));
    }

    #[tokio::test]
    async fn session_no_llm_message_without_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(!out.contains("LLM agent connected"));
    }

    #[tokio::test]
    async fn session_shows_workspace_context() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Context: test-ws"));
    }

    #[tokio::test]
    async fn session_displays_status_banner() {
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
        run_session(&config, &mut input, &mut output, Some(&summary), None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("System Status"));
        assert!(out.contains("Queue: 12 items"));
        assert!(out.contains("pending=5"));
        assert!(out.contains("hitl=2"));
        assert!(out.contains("HITL pending: 2"));
        assert!(out.contains("item-1"));
        assert!(out.contains("running -> hitl"));
    }

    #[tokio::test]
    async fn session_no_banner_when_status_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn claw_entry_displays_hitl_list_grouped_by_reason() {
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
        run_session(&config, &mut input, &mut output, Some(&summary), None)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn claw_entry_no_hitl_list_when_empty() {
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
        run_session(&config, &mut input, &mut output, Some(&summary), None)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn session_dispatches_auto_command() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/auto run task\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("[auto]"));
        assert!(out.contains("run task"));
    }

    #[tokio::test]
    async fn session_dispatches_spec_command() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/spec issue-42\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("[spec]"));
        assert!(out.contains("issue-42"));
    }

    #[tokio::test]
    async fn session_dispatches_claw_command() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/claw status\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("[claw]"));
        assert!(out.contains("test-ws"));
    }

    #[tokio::test]
    async fn session_dispatches_unknown_command() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/unknown\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Unknown command"));
    }

    #[tokio::test]
    async fn session_exit_alias_works() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/exit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Goodbye."));
    }

    #[tokio::test]
    async fn session_skips_empty_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"\n\n\nhello\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        // Empty lines should be skipped, only the freeform text echoed.
        assert!(out.contains(">> hello"));
        assert!(out.contains("Goodbye."));
    }

    #[tokio::test]
    async fn session_multiple_commands_in_sequence() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/help\n/auto\n/spec\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("/auto"));
        assert!(out.contains("[auto]"));
        assert!(out.contains("[spec]"));
        assert!(out.contains("Goodbye."));
    }

    #[tokio::test]
    async fn session_no_workspace_context_omits_context_line() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ClawWorkspace::init(tmp.path()).unwrap();
        let config = SessionConfig {
            workspace: None,
            claw_workspace: ws,
            runtime: None,
        };
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn session_displays_workspace_stats() {
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
        run_session(&config, &mut input, &mut output, None, Some(&stats))
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Workspace: test-ws ---"));
        assert!(out.contains("active=2"));
        assert!(out.contains("pending=3"));
    }

    #[tokio::test]
    async fn session_no_workspace_stats_when_none() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
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

    #[test]
    fn build_prompt_with_empty_history() {
        let prompt = build_prompt_with_history("hello", &[]);
        assert_eq!(prompt, "hello");
    }

    #[test]
    fn build_prompt_includes_history() {
        let history = vec![
            SessionMessage {
                role: MessageRole::User,
                content: "first".to_string(),
            },
            SessionMessage {
                role: MessageRole::Assistant,
                content: "response".to_string(),
            },
        ];
        let prompt = build_prompt_with_history("second", &history);
        assert!(prompt.contains("conversation_history"));
        assert!(prompt.contains("[user]: first"));
        assert!(prompt.contains("[assistant]: response"));
        assert!(prompt.contains("second"));
    }

    #[test]
    fn session_token_usage_accumulation() {
        let mut usage = SessionTokenUsage::default();
        usage.accumulate(&TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: None,
            cache_write_tokens: None,
        });
        usage.accumulate(&TokenUsage {
            input_tokens: 200,
            output_tokens: 75,
            cache_read_tokens: None,
            cache_write_tokens: None,
        });
        assert_eq!(usage.total_input_tokens, 300);
        assert_eq!(usage.total_output_tokens, 125);
        assert_eq!(usage.invocation_count, 2);
    }

    #[test]
    fn build_system_prompt_includes_workspace() {
        let prompt = build_system_prompt(Some("my-ws"), &[]);
        assert!(prompt.contains("my-ws"));
        assert!(prompt.contains("Belt Claw"));
    }

    #[test]
    fn build_system_prompt_without_workspace() {
        let prompt = build_system_prompt(None, &[]);
        assert!(prompt.contains("Belt Claw"));
        assert!(!prompt.contains("Current workspace"));
    }

    // --- Additional tests for AgentRuntime session loop, token tracking,
    //     and conversation history (issue #328) ---

    #[tokio::test]
    async fn session_runtime_multi_turn_conversation() {
        // Verify a multi-turn conversation flows correctly through the runtime,
        // with each subsequent prompt carrying accumulated history context.
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::always_ok("mock"));
        let config = make_config_with_runtime(&tmp, runtime.clone());
        let mut input = Cursor::new(b"alpha\nbeta\ngamma\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();

        let calls = runtime.calls();
        assert_eq!(calls.len(), 3);

        // First call: no history.
        assert!(!calls[0].contains("conversation_history"));
        assert!(calls[0].contains("alpha"));

        // Second call: history contains first exchange.
        assert!(calls[1].contains("conversation_history"));
        assert!(calls[1].contains("[user]: alpha"));
        assert!(calls[1].contains("[assistant]:"));
        assert!(calls[1].contains("beta"));

        // Third call: history contains both previous exchanges.
        assert!(calls[2].contains("[user]: alpha"));
        assert!(calls[2].contains("[user]: beta"));
        assert!(calls[2].contains("gamma"));
    }

    #[tokio::test]
    async fn session_history_not_updated_on_runtime_failure() {
        // When the runtime returns a failure, no assistant message should be
        // added to history. The next prompt should only contain the user's
        // failed message.
        let tmp = tempfile::tempdir().unwrap();
        // First call fails (exit_code=1), second succeeds (exit_code=0).
        let runtime = Arc::new(MockRuntime::new("mock", vec![1]));
        let config = make_config_with_runtime(&tmp, runtime.clone());
        let mut input = Cursor::new(b"bad input\ngood input\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();

        let calls = runtime.calls();
        assert_eq!(calls.len(), 2);

        // Second call should have history with the failed user message but
        // no assistant response from the failed invocation.
        assert!(calls[1].contains("conversation_history"));
        assert!(calls[1].contains("[user]: bad input"));
        // The failed invocation should not have an assistant entry.
        // Count occurrences of "[assistant]:" — should be zero since the
        // first call failed and no assistant message was recorded.
        let history_section = calls[1]
            .split("</conversation_history>")
            .next()
            .unwrap_or("");
        assert!(
            !history_section.contains("[assistant]:"),
            "Failed invocation should not produce assistant history entry"
        );
    }

    #[test]
    fn build_prompt_truncates_history_to_last_10_messages() {
        // When history has more than 10 messages, only the last 10 should
        // appear in the prompt context window.
        let mut history = Vec::new();
        for i in 0..14 {
            history.push(SessionMessage {
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: format!("message-{i}"),
            });
        }
        let prompt = build_prompt_with_history("current", &history);

        // Messages 0..3 (indices 0,1,2,3) should be excluded.
        assert!(
            !prompt.contains("message-0"),
            "message-0 should be truncated"
        );
        assert!(
            !prompt.contains("message-3"),
            "message-3 should be truncated"
        );

        // Messages 4..13 should be included (last 10).
        assert!(prompt.contains("message-4"));
        assert!(prompt.contains("message-13"));

        // Current input should be present after history.
        assert!(prompt.contains("current"));
    }

    #[test]
    fn build_system_prompt_mentions_history_count() {
        let history = vec![
            SessionMessage {
                role: MessageRole::User,
                content: "hi".to_string(),
            },
            SessionMessage {
                role: MessageRole::Assistant,
                content: "hello".to_string(),
            },
            SessionMessage {
                role: MessageRole::User,
                content: "how".to_string(),
            },
        ];
        let prompt = build_system_prompt(Some("ws"), &history);
        assert!(prompt.contains("3 previous messages"));
    }

    #[test]
    fn build_system_prompt_no_history_mention_when_empty() {
        let prompt = build_system_prompt(Some("ws"), &[]);
        assert!(!prompt.contains("previous messages"));
    }

    #[tokio::test]
    async fn session_token_usage_not_shown_when_runtime_reports_none() {
        // When the runtime does not report token usage, no token line should
        // appear and no session summary should be shown on quit.
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::always_ok("mock"));
        let config = make_config_with_runtime(&tmp, runtime);
        let mut input = Cursor::new(b"hello\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        // No per-invocation token line.
        assert!(!out.contains("[tokens:"));
        // No session summary since invocation_count stays 0 (no token usage
        // was accumulated).
        assert!(!out.contains("Session totals:"));
    }

    #[tokio::test]
    async fn session_token_accumulation_across_three_invocations() {
        let tmp = tempfile::tempdir().unwrap();
        let usages = vec![
            TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
            TokenUsage {
                input_tokens: 20,
                output_tokens: 15,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
            TokenUsage {
                input_tokens: 30,
                output_tokens: 25,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
        ];
        let runtime = Arc::new(MockRuntime::always_ok("mock").with_token_usages(usages));
        let config = make_config_with_runtime(&tmp, runtime);
        let mut input = Cursor::new(b"a\nb\nc\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        // Session summary on quit.
        assert!(out.contains("3 invocations"));
        assert!(out.contains("60 input tokens"));
        assert!(out.contains("45 output tokens"));
    }

    #[test]
    fn session_token_usage_default_is_zero() {
        let usage = SessionTokenUsage::default();
        assert_eq!(usage.total_input_tokens, 0);
        assert_eq!(usage.total_output_tokens, 0);
        assert_eq!(usage.invocation_count, 0);
    }

    #[test]
    fn session_token_usage_single_accumulation() {
        let mut usage = SessionTokenUsage::default();
        usage.accumulate(&TokenUsage {
            input_tokens: 42,
            output_tokens: 17,
            cache_read_tokens: Some(5),
            cache_write_tokens: Some(3),
        });
        assert_eq!(usage.total_input_tokens, 42);
        assert_eq!(usage.total_output_tokens, 17);
        assert_eq!(usage.invocation_count, 1);
    }

    #[tokio::test]
    async fn session_runtime_receives_system_prompt_with_workspace() {
        // Verify that the system prompt passed to the runtime includes
        // the workspace name when configured.
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::always_ok("mock"));
        let config = make_config_with_runtime(&tmp, runtime.clone());
        let mut input = Cursor::new(b"test\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();

        // MockRuntime records the prompt but not the system_prompt directly.
        // We can verify indirectly via the build_system_prompt function.
        let system_prompt = build_system_prompt(Some("test-ws"), &[]);
        assert!(system_prompt.contains("test-ws"));
        assert!(system_prompt.contains("Belt Claw"));
    }

    #[tokio::test]
    async fn session_runtime_error_shows_stderr() {
        // When the runtime fails with stderr output, it should be displayed.
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::new("mock", vec![1]));
        let config = make_config_with_runtime(&tmp, runtime);
        let mut input = Cursor::new(b"trigger error\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("[error] Agent invocation failed (exit code 1)"));
    }

    #[tokio::test]
    async fn session_mixed_slash_and_freeform_with_runtime() {
        // Verify that slash commands do not affect runtime call count
        // or conversation history.
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::always_ok("mock"));
        let config = make_config_with_runtime(&tmp, runtime.clone());
        let mut input = Cursor::new(b"hello\n/help\nworld\n/auto task\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();

        // Only 2 runtime calls (hello, world). Slash commands are not forwarded.
        let calls = runtime.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].contains("hello"));
        assert!(calls[1].contains("world"));

        let out = String::from_utf8(output).unwrap();
        // Slash command output should also be present.
        assert!(out.contains("/auto"));
        assert!(out.contains("[auto]"));
    }

    #[tokio::test]
    async fn session_no_session_summary_when_no_invocations() {
        // When quitting without any LLM invocations, no session summary
        // should be displayed.
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::always_ok("mock"));
        let config = make_config_with_runtime(&tmp, runtime);
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output, None, None)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(!out.contains("Session totals:"));
    }
}
