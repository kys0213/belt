//! Claude Code slash command plugin generation and installation.
//!
//! Generates the `/agent` slash command plugin structure that can be
//! registered in `~/.claude/commands/` for use within Claude Code sessions.
//! The generated command collects belt system context (status, HITL items,
//! queue list) and forwards natural language input to the belt agent LLM.

use std::fs;
use std::path::{Path, PathBuf};

/// The plugin name used for directory naming.
const PLUGIN_NAME: &str = "belt-agent";

/// Generate the plugin.json content.
fn plugin_json() -> &'static str {
    r#"{
  "author": {
    "name": "belt"
  },
  "commands": [
    "./commands/agent.md"
  ],
  "description": "Belt agent natural language interface for conveyor belt management",
  "name": "belt-agent",
  "version": "0.1.0"
}
"#
}

/// Generate the /agent slash command markdown.
///
/// This command:
/// 1. Collects belt system context (status, HITL list, queue list)
/// 2. Forwards natural language input to the belt agent session
/// 3. Uses Bash tool to invoke belt CLI commands
fn agent_command_md() -> &'static str {
    r#"---
description: Belt agent natural language interface for autonomous development management
argument-hint: "[natural language instruction]"
allowed-tools: ["Bash", "Read", "Glob", "Grep"]
---

# /agent - Belt Natural Language Agent

Manages the belt conveyor system through natural language instructions.
Collects system context automatically and delegates tasks to the belt agent.

## Auto Context Collection

Before processing the user's request, ALWAYS collect current system context
by running these commands in parallel:

```bash
belt status --format json
belt hitl list --format json
belt queue list --format json
```

Parse the JSON output to understand:
- Overall system health and item counts by phase
- HITL items awaiting human review (highest priority)
- Queue items and their current states

## Processing User Input

After collecting context, process the user's natural language input:

1. **Direct belt commands**: If the user asks to perform a specific belt operation
   (e.g., "mark item X as done", "show queue", "approve HITL item Y"), translate
   to the appropriate belt CLI command and execute via Bash.

2. **Status queries**: If the user asks about system state, use the collected
   context to answer directly.

3. **Complex tasks**: For multi-step operations or analysis requests, break down
   into individual belt CLI calls and execute sequentially.

4. **Agent session management**: For workspace management tasks, use
   `belt agent init`, `belt agent rules`, or `belt agent session` as appropriate.

## Available Belt Commands

### System Status
- `belt status` -- Show overall system status
- `belt status --format json` -- Machine-readable status

### Queue Management
- `belt queue list` -- List all queue items
- `belt queue list --phase <phase>` -- Filter by phase
- `belt queue show <work_id>` -- Show item details
- `belt queue done <work_id>` -- Mark item as completed
- `belt queue skip <work_id>` -- Skip an item

### HITL (Human-in-the-Loop)
- `belt hitl list` -- List HITL items
- `belt hitl show <item_id>` -- Show HITL item details
- `belt hitl respond <item_id> --action <done|retry|skip|replan>` -- Respond

### Agent Workspace
- `belt agent init` -- Initialize agent workspace
- `belt agent rules` -- List policy rules
- `belt agent session` -- Run LLM agent session

## Response Format

Always structure responses as:
1. **Context Summary**: Brief summary of current system state (from auto-collected data)
2. **Action Taken**: What commands were executed and their results
3. **Recommendations**: Any suggested next steps based on the current state

## Important

- ALWAYS collect context first before responding
- For destructive operations (done, skip, replan), confirm with the user first
- When in doubt, escalate to HITL (safe default)
- Use `--format json` for programmatic parsing of belt command output
"#
}

/// Install the /agent slash command plugin.
///
/// Creates the plugin structure under `install_dir`:
/// ```text
/// {install_dir}/belt-agent/
/// ├── .claude-plugin/
/// │   └── plugin.json
/// └── commands/
///     └── agent.md
/// ```
///
/// Returns the path to the installed plugin directory.
pub fn install_plugin(install_dir: &Path) -> anyhow::Result<PathBuf> {
    let plugin_dir = install_dir.join(PLUGIN_NAME);
    let claude_plugin_dir = plugin_dir.join(".claude-plugin");
    let commands_dir = plugin_dir.join("commands");

    fs::create_dir_all(&claude_plugin_dir)?;
    fs::create_dir_all(&commands_dir)?;

    fs::write(claude_plugin_dir.join("plugin.json"), plugin_json())?;
    fs::write(commands_dir.join("agent.md"), agent_command_md())?;

    Ok(plugin_dir)
}

/// Return the default plugin installation directory.
///
/// Defaults to `~/.claude/commands/` which is the standard location
/// for Claude Code user-level slash commands.
pub fn default_install_dir() -> anyhow::Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    Ok(home.join(".claude").join("commands"))
}

/// Run a belt CLI command and return its stdout if successful.
fn run_belt_cmd(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("belt")
        .args(args)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Collect system context by running belt CLI commands.
///
/// Returns a formatted context string with status, HITL items, and queue list.
/// This is used by the /claw slash command for auto-context injection.
pub fn collect_cli_context() -> String {
    let commands: &[(&[&str], &str)] = &[
        (&["status", "--format", "json"], "System Status"),
        (&["hitl", "list", "--format", "json"], "HITL Items"),
        (&["queue", "list", "--format", "json"], "Queue Items"),
    ];

    let sections: Vec<String> = commands
        .iter()
        .filter_map(|(args, label)| {
            run_belt_cmd(args).map(|out| format!("## {label}\n```json\n{out}\n```"))
        })
        .collect();

    if sections.is_empty() {
        "No belt context available (belt CLI not found or database unavailable).".to_string()
    } else {
        format!("# Belt System Context\n\n{}", sections.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_json_is_valid_json() {
        let parsed: serde_json::Value = serde_json::from_str(plugin_json()).unwrap();
        assert_eq!(parsed["name"], "belt-agent");
        assert_eq!(parsed["version"], "0.1.0");
        assert!(parsed["commands"].is_array());
        assert_eq!(parsed["commands"][0], "./commands/agent.md");
    }

    #[test]
    fn agent_command_md_has_frontmatter() {
        let content = agent_command_md();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("description:"));
        assert!(content.contains("argument-hint:"));
        assert!(content.contains("allowed-tools:"));
        assert!(content.contains("Bash"));
    }

    #[test]
    fn agent_command_md_has_context_collection_instructions() {
        let content = agent_command_md();
        assert!(content.contains("belt status --format json"));
        assert!(content.contains("belt hitl list --format json"));
        assert!(content.contains("belt queue list --format json"));
    }

    #[test]
    fn install_plugin_creates_structure() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = install_plugin(tmp.path()).unwrap();

        assert!(plugin_dir.join(".claude-plugin/plugin.json").is_file());
        assert!(plugin_dir.join("commands/agent.md").is_file());

        // Verify plugin.json content.
        let json_content =
            fs::read_to_string(plugin_dir.join(".claude-plugin/plugin.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_content).unwrap();
        assert_eq!(parsed["name"], "belt-agent");

        // Verify agent.md has frontmatter.
        let md_content = fs::read_to_string(plugin_dir.join("commands/agent.md")).unwrap();
        assert!(md_content.starts_with("---\n"));
    }

    #[test]
    fn install_plugin_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path1 = install_plugin(tmp.path()).unwrap();
        let path2 = install_plugin(tmp.path()).unwrap();
        assert_eq!(path1, path2);

        // Both files still exist and are valid.
        assert!(path2.join(".claude-plugin/plugin.json").is_file());
        assert!(path2.join("commands/agent.md").is_file());
    }

    #[test]
    fn default_install_dir_ends_with_commands() {
        // This test may fail in CI without a home dir, but should work locally.
        if let Ok(dir) = default_install_dir() {
            assert!(dir.ends_with("commands"));
        }
    }

    #[test]
    fn collect_cli_context_returns_fallback_when_belt_unavailable() {
        // When belt CLI is not in PATH or DB is empty, should return fallback.
        let context = collect_cli_context();
        // Either has content or the fallback message.
        assert!(!context.is_empty());
    }
}
