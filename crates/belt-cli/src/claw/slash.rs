//! Slash command parsing and dispatch for the Claw interactive session.
//!
//! Supports `/auto`, `/spec`, `/claw`, `/help`, and `/quit` commands.

use std::process::Command;

/// Parsed slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// `/auto [args]` -- trigger automatic processing.
    Auto { args: String },
    /// `/spec [args]` -- generate or display a spec.
    Spec { args: String },
    /// `/claw [args]` -- claw workspace management within session.
    Claw { args: String },
    /// `/help` -- show available commands.
    Help,
    /// `/quit` -- exit the session.
    Quit,
    /// Unknown slash command.
    Unknown { name: String, args: String },
}

impl SlashCommand {
    /// Parse a line into a `SlashCommand`, or `None` if it does not start
    /// with `/`.
    pub fn parse(line: &str) -> Option<Self> {
        let trimmed = line.trim();
        if !trimmed.starts_with('/') {
            return None;
        }

        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("");
        let args = parts.next().unwrap_or("").trim().to_string();

        Some(match cmd {
            "/auto" => SlashCommand::Auto { args },
            "/spec" => SlashCommand::Spec { args },
            "/claw" => SlashCommand::Claw { args },
            "/help" => SlashCommand::Help,
            "/quit" | "/exit" => SlashCommand::Quit,
            other => SlashCommand::Unknown {
                name: other.to_string(),
                args,
            },
        })
    }
}

/// Dispatches parsed slash commands and returns a response string.
pub struct SlashDispatcher {
    /// Current workspace context, if any.
    workspace: Option<String>,
}

impl SlashDispatcher {
    /// Create a new dispatcher.
    pub fn new(workspace: Option<String>) -> Self {
        Self { workspace }
    }

    /// Dispatch a slash command and return the response text.
    pub fn dispatch(&self, cmd: &SlashCommand) -> String {
        match cmd {
            SlashCommand::Auto { args } => self.handle_auto(args),
            SlashCommand::Spec { args } => self.handle_spec(args),
            SlashCommand::Claw { args } => self.handle_claw(args),
            SlashCommand::Help => self.handle_help(),
            SlashCommand::Quit => "Goodbye.".to_string(),
            SlashCommand::Unknown { name, .. } => {
                format!("Unknown command: {name}. Type /help for available commands.")
            }
        }
    }

    fn handle_auto(&self, args: &str) -> String {
        let ws_label = self
            .workspace
            .as_deref()
            .unwrap_or("(no workspace selected)");
        if args.is_empty() {
            format!("[auto] Triggering automatic processing for workspace: {ws_label}")
        } else {
            format!("[auto] Processing: {args} (workspace: {ws_label})")
        }
    }

    fn handle_spec(&self, args: &str) -> String {
        if args.is_empty() {
            return "[spec] Usage: /spec <spec-id|issue-number>".to_string();
        }

        let spec_id = resolve_spec_id(args);
        match fetch_spec_json(&spec_id) {
            Ok(spec) => format_spec_output(&spec),
            Err(e) => format!("[spec] Error fetching {spec_id}: {e}"),
        }
    }

    fn handle_claw(&self, args: &str) -> String {
        match args {
            "" | "status" => {
                let ws_label = self
                    .workspace
                    .as_deref()
                    .unwrap_or("(no workspace selected)");
                format!("[claw] Claw session active. Workspace: {ws_label}")
            }
            "init" => "[claw] Re-initializing claw workspace...".to_string(),
            "rules" => "[claw] Listing policy rules...".to_string(),
            _ => format!("[claw] Unknown subcommand: {args}. Try: status, init, rules"),
        }
    }

    fn handle_help(&self) -> String {
        "\
Available commands:
  /auto [args]   Trigger automatic processing for current workspace
  /spec <id>     Show spec details, sections, and acceptance criteria
  /claw [sub]    Claw workspace management (status, init, rules)
  /help          Show this help message
  /quit          Exit the session"
            .to_string()
    }
}

/// Resolve a user-provided argument into a spec ID.
///
/// Accepts:
/// - A raw spec ID (e.g. `spec-1234567890`)
/// - An issue number with optional `#` prefix (e.g. `42` or `#42`)
///
/// Issue numbers are converted to the `spec-issue-{N}` naming convention used
/// when specs are linked to GitHub issues. If the argument already looks like
/// a spec ID (contains a non-numeric character other than `#`), it is returned
/// as-is.
fn resolve_spec_id(args: &str) -> String {
    let trimmed = args.trim().trim_start_matches('#');
    if trimmed.chars().all(|c| c.is_ascii_digit()) && !trimmed.is_empty() {
        // Bare issue number -- try the conventional spec ID first.
        format!("spec-issue-{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Invoke `belt spec show --json <id>` and parse the JSON result into a
/// [`serde_json::Value`].
fn fetch_spec_json(spec_id: &str) -> Result<serde_json::Value, String> {
    let exe = std::env::current_exe().unwrap_or_else(|_| "belt".into());
    let output = Command::new(exe)
        .args(["spec", "show", spec_id, "--json"])
        .output()
        .map_err(|e| format!("failed to invoke belt spec show: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr.trim();
        return Err(if msg.is_empty() {
            format!("belt spec show exited with {}", output.status)
        } else {
            msg.to_string()
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).map_err(|e| format!("failed to parse spec JSON: {e}"))
}

/// Format a spec JSON value into a human-readable summary showing metadata,
/// sections, and acceptance criteria.
fn format_spec_output(spec: &serde_json::Value) -> String {
    let mut out = String::new();

    // -- Header --
    let id = spec["id"].as_str().unwrap_or("?");
    let name = spec["name"].as_str().unwrap_or("?");
    let status = spec["status"].as_str().unwrap_or("?");
    let workspace = spec["workspace_id"].as_str().unwrap_or("?");

    out.push_str(&format!("[spec] {name} ({id})\n"));
    out.push_str(&format!("Status: {status}  |  Workspace: {workspace}\n"));

    if let Some(p) = spec["priority"].as_i64() {
        out.push_str(&format!("Priority: {p}\n"));
    }
    if let Some(labels) = spec["labels"].as_str() {
        out.push_str(&format!("Labels: {labels}\n"));
    }
    if let Some(deps) = spec["depends_on"].as_str() {
        out.push_str(&format!("Depends On: {deps}\n"));
    }
    if let Some(ep) = spec["entry_point"].as_str() {
        out.push_str(&format!("Entry Point: {ep}\n"));
    }
    if let Some(issues) = spec["decomposed_issues"].as_str() {
        out.push_str(&format!("Decomposed Issues: {issues}\n"));
    }

    // -- Sections --
    let content = spec["content"].as_str().unwrap_or("");
    let sections = extract_sections(content);
    if !sections.is_empty() {
        out.push_str("\nSections:\n");
        for (heading, preview) in &sections {
            out.push_str(&format!("  - {heading}"));
            if !preview.is_empty() {
                out.push_str(&format!("  ({preview})"));
            }
            out.push('\n');
        }
    }

    // -- Acceptance Criteria --
    let criteria = belt_core::spec::extract_acceptance_criteria(content);
    if !criteria.is_empty() {
        out.push_str("\nAcceptance Criteria:\n");
        for (i, c) in criteria.iter().enumerate() {
            out.push_str(&format!("  {}. {c}\n", i + 1));
        }
    }

    // Trim trailing newlines for consistency with other handlers.
    out.truncate(out.trim_end_matches('\n').len());
    out
}

/// Extract level-2 markdown section headings and a short preview of their
/// content (first non-empty line after the heading, truncated to 60 chars).
fn extract_sections(content: &str) -> Vec<(String, String)> {
    let mut sections = Vec::new();
    let mut lines = content.lines();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("## ") {
            let heading = heading.trim().to_string();
            // Grab first non-empty line as preview.
            let mut preview = String::new();
            for next_line in lines.by_ref() {
                let next_trimmed = next_line.trim();
                if next_trimmed.is_empty() {
                    continue;
                }
                if next_trimmed.starts_with('#') {
                    // Next heading reached; stop preview collection.
                    break;
                }
                preview = if next_trimmed.len() > 60 {
                    format!("{}...", &next_trimmed[..57])
                } else {
                    next_trimmed.to_string()
                };
                break;
            }
            sections.push((heading, preview));
        }
    }

    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_auto() {
        let cmd = SlashCommand::parse("/auto fix the bug").unwrap();
        assert_eq!(
            cmd,
            SlashCommand::Auto {
                args: "fix the bug".to_string()
            }
        );
    }

    #[test]
    fn parse_spec_no_args() {
        let cmd = SlashCommand::parse("/spec").unwrap();
        assert_eq!(
            cmd,
            SlashCommand::Spec {
                args: String::new()
            }
        );
    }

    #[test]
    fn parse_claw() {
        let cmd = SlashCommand::parse("/claw status").unwrap();
        assert_eq!(
            cmd,
            SlashCommand::Claw {
                args: "status".to_string()
            }
        );
    }

    #[test]
    fn parse_help() {
        let cmd = SlashCommand::parse("/help").unwrap();
        assert_eq!(cmd, SlashCommand::Help);
    }

    #[test]
    fn parse_quit_and_exit() {
        assert_eq!(SlashCommand::parse("/quit").unwrap(), SlashCommand::Quit);
        assert_eq!(SlashCommand::parse("/exit").unwrap(), SlashCommand::Quit);
    }

    #[test]
    fn parse_unknown() {
        let cmd = SlashCommand::parse("/foo bar").unwrap();
        assert_eq!(
            cmd,
            SlashCommand::Unknown {
                name: "/foo".to_string(),
                args: "bar".to_string()
            }
        );
    }

    #[test]
    fn parse_non_slash_returns_none() {
        assert!(SlashCommand::parse("hello world").is_none());
        assert!(SlashCommand::parse("").is_none());
    }

    #[test]
    fn dispatch_auto_with_workspace() {
        let d = SlashDispatcher::new(Some("my-ws".to_string()));
        let resp = d.dispatch(&SlashCommand::Auto {
            args: String::new(),
        });
        assert!(resp.contains("my-ws"));
        assert!(resp.contains("[auto]"));
    }

    #[test]
    fn dispatch_spec_empty_shows_usage() {
        let d = SlashDispatcher::new(None);
        let resp = d.dispatch(&SlashCommand::Spec {
            args: String::new(),
        });
        assert!(resp.contains("Usage"));
    }

    #[test]
    fn resolve_spec_id_bare_number() {
        assert_eq!(resolve_spec_id("42"), "spec-issue-42");
    }

    #[test]
    fn resolve_spec_id_hash_number() {
        assert_eq!(resolve_spec_id("#7"), "spec-issue-7");
    }

    #[test]
    fn resolve_spec_id_already_spec_id() {
        assert_eq!(resolve_spec_id("spec-1234"), "spec-1234");
    }

    #[test]
    fn resolve_spec_id_custom_string() {
        assert_eq!(resolve_spec_id("my-spec"), "my-spec");
    }

    #[test]
    fn extract_sections_basic() {
        let content = "## Overview\nSome overview text.\n\n## Requirements\nReq details.\n";
        let sections = extract_sections(content);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].0, "Overview");
        assert_eq!(sections[0].1, "Some overview text.");
        assert_eq!(sections[1].0, "Requirements");
        assert_eq!(sections[1].1, "Req details.");
    }

    #[test]
    fn extract_sections_long_preview_truncated() {
        let long_line = "A".repeat(80);
        let content = format!("## Heading\n{long_line}\n");
        let sections = extract_sections(&content);
        assert_eq!(sections.len(), 1);
        assert!(sections[0].1.ends_with("..."));
        assert!(sections[0].1.len() <= 63);
    }

    #[test]
    fn extract_sections_empty_content() {
        let sections = extract_sections("");
        assert!(sections.is_empty());
    }

    #[test]
    fn format_spec_output_basic() {
        let spec = serde_json::json!({
            "id": "spec-123",
            "name": "Auth Module",
            "status": "Draft",
            "workspace_id": "ws-1",
            "content": "## Overview\nAuth overview.\n\n## Acceptance Criteria\n- Login works\n- Logout works\n"
        });
        let output = format_spec_output(&spec);
        assert!(output.contains("[spec] Auth Module (spec-123)"));
        assert!(output.contains("Status: Draft"));
        assert!(output.contains("Sections:"));
        assert!(output.contains("Overview"));
        assert!(output.contains("Acceptance Criteria"));
        assert!(output.contains("1. Login works"));
        assert!(output.contains("2. Logout works"));
    }

    #[test]
    fn format_spec_output_no_content() {
        let spec = serde_json::json!({
            "id": "spec-0",
            "name": "Empty",
            "status": "Draft",
            "workspace_id": "ws-1",
            "content": ""
        });
        let output = format_spec_output(&spec);
        assert!(output.contains("[spec] Empty"));
        assert!(!output.contains("Sections:"));
        assert!(!output.contains("Acceptance Criteria:"));
    }

    #[test]
    fn dispatch_help_lists_commands() {
        let d = SlashDispatcher::new(None);
        let resp = d.dispatch(&SlashCommand::Help);
        assert!(resp.contains("/auto"));
        assert!(resp.contains("/spec"));
        assert!(resp.contains("/claw"));
        assert!(resp.contains("/quit"));
    }

    #[test]
    fn dispatch_unknown() {
        let d = SlashDispatcher::new(None);
        let resp = d.dispatch(&SlashCommand::Unknown {
            name: "/nope".to_string(),
            args: String::new(),
        });
        assert!(resp.contains("Unknown command"));
    }
}
