//! Claw interactive agent workspace management.
//!
//! Provides initialization, policy file management, workspace structure,
//! interactive session (REPL), and slash command handling for the Claw
//! interactive management session.

use std::fs;
use std::path::{Path, PathBuf};

pub mod plugin;
#[allow(dead_code)]
pub mod session;
#[allow(dead_code)]
pub mod slash;

/// Represents an initialized Claw workspace directory.
pub struct ClawWorkspace {
    /// Root path of the Claw workspace.
    pub path: PathBuf,
}

/// Write `contents` to `path` only if the file does not already exist.
///
/// Returns `true` if the file was written, `false` if it was skipped.
fn write_if_absent(path: &Path, contents: &str) -> std::io::Result<bool> {
    if path.exists() {
        Ok(false)
    } else {
        fs::write(path, contents)?;
        Ok(true)
    }
}

impl ClawWorkspace {
    /// Initialize a new Claw workspace under `belt_home`.
    ///
    /// Creates the directory structure:
    /// ```text
    /// {belt_home}/claw-workspace/
    /// ├── CLAUDE.md
    /// ├── classify-policy.yaml          # machine-readable (daemon evaluator)
    /// ├── hitl-policy.yaml              # machine-readable (daemon evaluator)
    /// ├── commands/
    /// ├── skills/
    /// │   ├── gap-detect/
    /// │   └── prioritize/
    /// └── .claude/rules/                # ← agent system prompt policies
    ///     ├── classify-policy.md        # Done vs HITL classification criteria
    ///     ├── hitl-policy.md            # HITL escalation criteria
    ///     └── auto-approve-policy.md    # auto-approve criteria
    /// ```
    ///
    /// ## Policy file loading (classify-policy.md)
    ///
    /// At runtime, `agent::resolve_rules_dir` locates the rules directory
    /// using the following priority (first existing directory wins):
    ///
    /// 1. `claw_config.rules_path` -- explicit path in workspace YAML
    /// 2. `$BELT_HOME/workspaces/<name>/claw/system/` -- per-workspace override
    /// 3. `$BELT_HOME/claw-workspace/.claude/rules/` -- global default (here)
    ///
    /// All `.md` files in the resolved directory are loaded by
    /// `agent::load_rules_from_dir`, concatenated, and injected into the
    /// agent system prompt. If no rules directory is found, the agent runs
    /// with only the built-in Claw rules (no error).
    ///
    /// Existing files are preserved by default. Pass `force = true` to
    /// overwrite them.
    pub fn init(belt_home: &Path) -> anyhow::Result<ClawWorkspace> {
        Self::init_with_options(belt_home, false)
    }

    /// Initialize with explicit force-overwrite flag.
    pub fn init_with_options(belt_home: &Path, force: bool) -> anyhow::Result<ClawWorkspace> {
        let workspace_path = belt_home.join("claw-workspace");
        let rules_dir = workspace_path.join(".claude/rules");
        let commands_dir = workspace_path.join("commands");
        let skills_dir = workspace_path.join("skills");
        let gap_detect_dir = skills_dir.join("gap-detect");
        let prioritize_dir = skills_dir.join("prioritize");

        // Create all directories (idempotent).
        fs::create_dir_all(&rules_dir)?;
        fs::create_dir_all(&commands_dir)?;
        fs::create_dir_all(&gap_detect_dir)?;
        fs::create_dir_all(&prioritize_dir)?;

        let write_file = |path: &Path, contents: &str| -> anyhow::Result<()> {
            if force {
                fs::write(path, contents)?;
            } else {
                let written = write_if_absent(path, contents)?;
                if !written {
                    tracing::info!(path = %path.display(), "file already exists, skipping");
                }
            }
            Ok(())
        };

        write_file(&workspace_path.join("CLAUDE.md"), default_claude_md())?;
        write_file(
            &rules_dir.join("classify-policy.md"),
            default_classify_policy(),
        )?;
        write_file(&rules_dir.join("hitl-policy.md"), default_hitl_policy())?;
        write_file(
            &rules_dir.join("auto-approve-policy.md"),
            default_auto_approve_policy(),
        )?;

        // YAML policy files for machine-readable classification and HITL rules.
        write_file(
            &workspace_path.join("classify-policy.yaml"),
            default_classify_policy_yaml(),
        )?;
        write_file(
            &workspace_path.join("hitl-policy.yaml"),
            default_hitl_policy_yaml(),
        )?;

        // Default skill example.
        write_file(&skills_dir.join("default.py"), default_skill_py())?;

        Ok(ClawWorkspace {
            path: workspace_path,
        })
    }

    /// Edit a rule file by name.
    ///
    /// Opens the rule file in `$EDITOR` (falls back to `vi`). If `rule` is
    /// `None`, lists available rule files instead.
    pub fn edit_rule(&self, rule: Option<&str>) -> anyhow::Result<()> {
        let rules_dir = self.path.join(".claude/rules");
        if !rules_dir.exists() {
            anyhow::bail!("rules directory not found: {}", rules_dir.display());
        }

        let rule_name = match rule {
            Some(name) => name,
            None => {
                // List available rules when no specific rule is given.
                let rules = self.list_rules()?;
                println!("Available rules:");
                for r in &rules {
                    if let Some(stem) = r.file_stem().and_then(|s| s.to_str()) {
                        println!("  {stem}");
                    }
                }
                return Ok(());
            }
        };

        let file_path = rules_dir.join(format!("{rule_name}.md"));
        if !file_path.exists() {
            anyhow::bail!("rule file not found: {}", file_path.display());
        }

        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
        let status = std::process::Command::new(&editor)
            .arg(&file_path)
            .status()?;

        if !status.success() {
            anyhow::bail!("editor exited with status: {status}");
        }

        Ok(())
    }

    /// List policy rule files in the workspace.
    pub fn list_rules(&self) -> anyhow::Result<Vec<PathBuf>> {
        let rules_dir = self.path.join(".claude/rules");
        if !rules_dir.exists() {
            anyhow::bail!("rules directory not found: {}", rules_dir.display());
        }

        let mut files = Vec::new();
        for entry in fs::read_dir(&rules_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                files.push(path);
            }
        }
        files.sort();
        Ok(files)
    }
}

/// Returns the default CLAUDE.md content with belt CLI usage guide.
pub fn default_claude_md() -> &'static str {
    r#"# Belt Claw Workspace

## Available Belt Commands

### System Status
- `belt status` — Show overall system status
- `belt status --format json` — Machine-readable status output

### Queue Management
- `belt queue list` — List all queue items
- `belt queue list --phase <phase>` — Filter items by phase
- `belt queue list --workspace <name>` — Filter items by workspace
- `belt queue show <work_id>` — Show details for a specific item
- `belt queue done <work_id>` — Mark an item as completed
- `belt queue hitl <work_id> --reason "<reason>"` — Escalate to human-in-the-loop
- `belt queue skip <work_id>` — Skip an item

### Workspace Management
- `belt workspace list` — List registered workspaces
- `belt workspace show <name>` — Show workspace details
- `belt workspace add --config <path>` — Register a new workspace

### Context
- `belt context <work_id>` — Retrieve item context
- `belt context <work_id> --json` — Retrieve context as JSON

## Workflow
1. Check system status with `belt status`
2. Review queue items with `belt queue list`
3. For each item, retrieve context and apply classification rules
4. Items that pass auto-approve policy proceed automatically
5. Items matching HITL policy are escalated for human review
"#
}

/// Returns the default `classify-policy.md` template.
///
/// This template is written to `<workspace>/.claude/rules/classify-policy.md`
/// during [`ClawWorkspace::init`].  At runtime the file is loaded as part of
/// the rules directory resolution chain described in `agent::resolve_rules_dir`:
///
/// 1. `claw_config.rules_path` (explicit override in workspace YAML)
/// 2. `$BELT_HOME/workspaces/<name>/claw/system/` (per-workspace override)
/// 3. `$BELT_HOME/claw-workspace/.claude/rules/` (global default -- written here)
///
/// If the file is missing at all priority levels, the agent proceeds without
/// classification policy guidance (no error is raised).
pub fn default_classify_policy() -> &'static str {
    r#"# Classification Policy

## Purpose
Classify incoming queue items into categories for routing.

## Default Rules
- Items with unknown data sources → HITL
- Items matching a registered workspace pattern → auto-route
- Items with missing required fields → reject

## Categories
- **auto**: Can be processed without human intervention
- **hitl**: Requires human review before proceeding
- **reject**: Invalid or incomplete items
"#
}

/// Returns the default HITL policy template.
pub fn default_hitl_policy() -> &'static str {
    r#"# Human-in-the-Loop (HITL) Policy

## Purpose
Define when items require human review.

## Default Triggers
- Classification confidence below threshold
- Destructive operations (delete, overwrite)
- Items touching sensitive paths or data
- First-time patterns not seen before

## Escalation
- When in doubt, escalate to HITL (safe default)
- Provide context and reasoning with escalation
- Include suggested action for reviewer
"#
}

/// Returns the default auto-approve policy template.
pub fn default_auto_approve_policy() -> &'static str {
    r#"# Auto-Approve Policy

## Purpose
Define conditions under which items can proceed without human review.

## Default Conditions
- Item matches a known, previously-approved pattern
- All required fields are present and valid
- No destructive operations involved
- Data source is registered and trusted

## Safeguards
- Auto-approved items are still logged for audit
- Repeated failures from auto-approved patterns trigger re-evaluation
- Rate limits apply to prevent runaway automation
"#
}

/// Returns the default `classify-policy.yaml` template.
///
/// Provides machine-readable classification rules that the daemon uses
/// to route incoming queue items.  This file is written to
/// `<workspace>/classify-policy.yaml` (at the workspace root, **not**
/// inside `.claude/rules/`) during [`ClawWorkspace::init`].
///
/// Unlike `classify-policy.md` (which is loaded into the agent system
/// prompt), the YAML variant is intended for programmatic consumption by
/// the daemon's evaluator.
pub fn default_classify_policy_yaml() -> &'static str {
    r#"# Classification Policy (machine-readable)
#
# Rules evaluated top-to-bottom; first match wins.

version: 1

rules:
  - name: unknown-source
    description: Items from unregistered data sources require human review
    condition:
      source_registered: false
    action: hitl

  - name: missing-fields
    description: Items missing required fields are rejected
    condition:
      required_fields_present: false
    action: reject

  - name: workspace-match
    description: Items matching a registered workspace pattern are auto-routed
    condition:
      workspace_pattern_match: true
    action: auto

  - name: default
    description: Fallback — escalate to human review
    condition: {}
    action: hitl
"#
}

/// Returns the default HITL policy YAML template.
///
/// Defines machine-readable escalation conditions for human-in-the-loop
/// review triggers.
pub fn default_hitl_policy_yaml() -> &'static str {
    r#"# Human-in-the-Loop (HITL) Policy (machine-readable)
#
# Defines when items require human review before proceeding.

version: 1

triggers:
  - name: low-confidence
    description: Classification confidence below threshold
    condition:
      confidence_below: 0.7

  - name: destructive-operation
    description: Operations that delete or overwrite data
    condition:
      operation_type:
        - delete
        - overwrite

  - name: sensitive-path
    description: Items touching sensitive paths or data
    condition:
      path_pattern:
        - "*.env"
        - "**/secrets/**"
        - "**/credentials/**"

  - name: first-time-pattern
    description: Patterns not previously seen
    condition:
      pattern_seen_before: false

escalation:
  default_action: hitl
  require_context: true
  require_suggested_action: true
  timeout_minutes: 60
"#
}

/// Returns a default skill example script.
///
/// Placed in `skills/default.py` as a starting point for custom skill
/// development.
pub fn default_skill_py() -> &'static str {
    r#"#!/usr/bin/env python3
"""Default skill example for Belt Claw.

Skills are executable scripts invoked by the daemon. They receive
context via environment variables:

  WORK_ID   - The queue item identifier
  WORKTREE  - Path to the isolated worktree

The script should exit 0 on success, non-zero on failure.
Output is captured and attached to the queue item.
"""

import json
import os
import sys


def main():
    work_id = os.environ.get("WORK_ID", "")
    worktree = os.environ.get("WORKTREE", "")

    if not work_id:
        print("ERROR: WORK_ID not set", file=sys.stderr)
        sys.exit(1)

    result = {
        "work_id": work_id,
        "worktree": worktree,
        "status": "ok",
        "message": "Default skill executed successfully",
    }

    print(json.dumps(result))


if __name__ == "__main__":
    main()
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn init_creates_workspace_structure() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ClawWorkspace::init(tmp.path()).unwrap();

        assert!(ws.path.exists());
        assert!(ws.path.join("CLAUDE.md").is_file());
        assert!(ws.path.join(".claude/rules/classify-policy.md").is_file());
        assert!(ws.path.join(".claude/rules/hitl-policy.md").is_file());
        assert!(
            ws.path
                .join(".claude/rules/auto-approve-policy.md")
                .is_file()
        );
    }

    #[test]
    fn init_creates_commands_and_skills_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ClawWorkspace::init(tmp.path()).unwrap();

        assert!(ws.path.join("commands").is_dir());
        assert!(ws.path.join("skills").is_dir());
        assert!(ws.path.join("skills/gap-detect").is_dir());
        assert!(ws.path.join("skills/prioritize").is_dir());
        assert!(ws.path.join("skills/default.py").is_file());
    }

    #[test]
    fn init_preserves_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ClawWorkspace::init(tmp.path()).unwrap();

        // Modify CLAUDE.md and a YAML policy after initial init.
        let custom_content = "# Custom content";
        fs::write(ws.path.join("CLAUDE.md"), custom_content).unwrap();
        fs::write(ws.path.join("classify-policy.yaml"), custom_content).unwrap();

        // Re-init without force — should preserve the custom files.
        let ws2 = ClawWorkspace::init(tmp.path()).unwrap();
        let content = fs::read_to_string(ws2.path.join("CLAUDE.md")).unwrap();
        assert_eq!(content, custom_content);
        let yaml_content = fs::read_to_string(ws2.path.join("classify-policy.yaml")).unwrap();
        assert_eq!(yaml_content, custom_content);
    }

    #[test]
    fn init_force_overwrites_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ClawWorkspace::init(tmp.path()).unwrap();

        // Modify CLAUDE.md after initial init.
        let custom_content = "# Custom content";
        fs::write(ws.path.join("CLAUDE.md"), custom_content).unwrap();

        // Re-init with force — should overwrite.
        let ws2 = ClawWorkspace::init_with_options(tmp.path(), true).unwrap();
        let content = fs::read_to_string(ws2.path.join("CLAUDE.md")).unwrap();
        assert_eq!(content, default_claude_md());
    }

    #[test]
    fn init_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let ws1 = ClawWorkspace::init(tmp.path()).unwrap();
        let ws2 = ClawWorkspace::init(tmp.path()).unwrap();
        assert_eq!(ws1.path, ws2.path);
    }

    #[test]
    fn init_creates_yaml_policy_files() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ClawWorkspace::init(tmp.path()).unwrap();

        assert!(ws.path.join("classify-policy.yaml").is_file());
        assert!(ws.path.join("hitl-policy.yaml").is_file());

        let classify = fs::read_to_string(ws.path.join("classify-policy.yaml")).unwrap();
        assert!(classify.contains("version: 1"));
        assert!(classify.contains("action: hitl"));

        let hitl = fs::read_to_string(ws.path.join("hitl-policy.yaml")).unwrap();
        assert!(hitl.contains("version: 1"));
        assert!(hitl.contains("timeout_minutes:"));
    }

    #[test]
    fn init_creates_default_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ClawWorkspace::init(tmp.path()).unwrap();

        let skill_path = ws.path.join("skills/default.py");
        assert!(skill_path.is_file());

        let content = fs::read_to_string(&skill_path).unwrap();
        assert!(content.contains("WORK_ID"));
        assert!(content.contains("WORKTREE"));
        assert!(content.contains("def main()"));
    }

    #[test]
    fn default_templates_are_not_empty() {
        assert!(!default_claude_md().is_empty());
        assert!(!default_classify_policy().is_empty());
        assert!(!default_hitl_policy().is_empty());
        assert!(!default_auto_approve_policy().is_empty());
        assert!(!default_classify_policy_yaml().is_empty());
        assert!(!default_hitl_policy_yaml().is_empty());
        assert!(!default_skill_py().is_empty());
    }

    #[test]
    fn list_rules_returns_policy_files() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ClawWorkspace::init(tmp.path()).unwrap();
        let rules = ws.list_rules().unwrap();

        let filenames: Vec<&str> = rules
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();

        assert!(filenames.contains(&"classify-policy.md"));
        assert!(filenames.contains(&"hitl-policy.md"));
        assert!(filenames.contains(&"auto-approve-policy.md"));
        assert_eq!(filenames.len(), 3);
    }

    #[test]
    fn list_rules_fails_without_init() {
        let ws = ClawWorkspace {
            path: Path::new("/nonexistent/path").to_path_buf(),
        };
        assert!(ws.list_rules().is_err());
    }

    #[test]
    fn edit_rule_none_lists_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ClawWorkspace::init(tmp.path()).unwrap();
        // Calling edit_rule(None) should not error — it lists available rules.
        ws.edit_rule(None).unwrap();
    }

    #[test]
    fn edit_rule_missing_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ClawWorkspace::init(tmp.path()).unwrap();
        assert!(ws.edit_rule(Some("nonexistent")).is_err());
    }
}
