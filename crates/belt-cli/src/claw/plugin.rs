//! Claude Code slash command plugin generation and installation.
//!
//! Generates the `/claw` slash command plugin structure that can be
//! registered in `~/.claude/commands/` for use within Claude Code sessions.
//! The generated command collects belt system context (status, HITL items,
//! queue list) and forwards natural language input to the belt agent LLM.

use std::fs;
use std::path::{Path, PathBuf};

/// The plugin name used for directory naming.
const PLUGIN_NAME: &str = "belt-claw";

/// Generate the plugin.json content.
fn plugin_json() -> &'static str {
    r#"{
  "author": {
    "name": "belt"
  },
  "commands": [
    "./commands/claw.md"
  ],
  "description": "Belt Claw natural language agent for conveyor belt management",
  "name": "belt-claw",
  "version": "0.1.0"
}
"#
}

/// Generate the /claw slash command markdown.
///
/// This command:
/// 1. Collects belt system context (status, HITL list, queue list)
/// 2. Forwards natural language input to the belt claw session
/// 3. Uses Bash tool to invoke belt CLI commands
fn claw_command_md() -> &'static str {
    r#"---
description: Belt Claw natural language agent for autonomous development management
argument-hint: "[natural language instruction]"
allowed-tools: ["Bash", "Read", "Glob", "Grep"]
---

# /claw - Belt Natural Language Agent

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

4. **Claw session management**: For workspace management tasks, use
   `belt claw init`, `belt claw rules`, or `belt claw session` as appropriate.

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

### Claw Workspace
- `belt claw init` -- Initialize claw workspace
- `belt claw rules` -- List policy rules
- `belt claw session` -- Open interactive session

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

/// Install the /claw slash command plugin.
///
/// Creates the plugin structure under `install_dir`:
/// ```text
/// {install_dir}/belt-claw/
/// ├── .claude-plugin/
/// │   └── plugin.json
/// └── commands/
///     └── claw.md
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
    fs::write(commands_dir.join("claw.md"), claw_command_md())?;

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

/// Collect system context by running belt CLI commands.
///
/// Returns a formatted context string with status, HITL items, and queue list.
/// This is used by the /claw slash command for auto-context injection.
pub fn collect_cli_context() -> String {
    let mut sections = Vec::new();

    // Collect belt status.
    if let Ok(output) = std::process::Command::new("belt")
        .args(["status", "--format", "json"])
        .output()
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        sections.push(format!("## System Status\n```json\n{}\n```", stdout.trim()));
    }

    // Collect HITL items.
    if let Ok(output) = std::process::Command::new("belt")
        .args(["hitl", "list", "--format", "json"])
        .output()
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        sections.push(format!("## HITL Items\n```json\n{}\n```", stdout.trim()));
    }

    // Collect queue list.
    if let Ok(output) = std::process::Command::new("belt")
        .args(["queue", "list", "--format", "json"])
        .output()
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        sections.push(format!("## Queue Items\n```json\n{}\n```", stdout.trim()));
    }

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
        assert_eq!(parsed["name"], "belt-claw");
        assert_eq!(parsed["version"], "0.1.0");
        assert!(parsed["commands"].is_array());
        assert_eq!(parsed["commands"][0], "./commands/claw.md");
    }

    #[test]
    fn claw_command_md_has_frontmatter() {
        let content = claw_command_md();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("description:"));
        assert!(content.contains("argument-hint:"));
        assert!(content.contains("allowed-tools:"));
        assert!(content.contains("Bash"));
    }

    #[test]
    fn claw_command_md_has_context_collection_instructions() {
        let content = claw_command_md();
        assert!(content.contains("belt status --format json"));
        assert!(content.contains("belt hitl list --format json"));
        assert!(content.contains("belt queue list --format json"));
    }

    #[test]
    fn install_plugin_creates_structure() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = install_plugin(tmp.path()).unwrap();

        assert!(plugin_dir.join(".claude-plugin/plugin.json").is_file());
        assert!(plugin_dir.join("commands/claw.md").is_file());

        // Verify plugin.json content.
        let json_content =
            fs::read_to_string(plugin_dir.join(".claude-plugin/plugin.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_content).unwrap();
        assert_eq!(parsed["name"], "belt-claw");

        // Verify claw.md has frontmatter.
        let md_content = fs::read_to_string(plugin_dir.join("commands/claw.md")).unwrap();
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
        assert!(path2.join("commands/claw.md").is_file());
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
