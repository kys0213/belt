//! Claw interactive agent workspace management.
//!
//! Provides initialization, policy file management, and workspace structure
//! for the Claw interactive management session.

use std::fs;
use std::path::{Path, PathBuf};

/// Represents an initialized Claw workspace directory.
pub struct ClawWorkspace {
    /// Root path of the Claw workspace.
    pub path: PathBuf,
}

impl ClawWorkspace {
    /// Initialize a new Claw workspace under `belt_home`.
    ///
    /// Creates the directory structure:
    /// ```text
    /// {belt_home}/claw-workspace/
    /// ├── CLAUDE.md
    /// └── .claude/rules/
    ///     ├── classify-policy.md
    ///     ├── hitl-policy.md
    ///     └── auto-approve-policy.md
    /// ```
    pub fn init(belt_home: &Path) -> anyhow::Result<ClawWorkspace> {
        let workspace_path = belt_home.join("claw-workspace");
        let rules_dir = workspace_path.join(".claude/rules");

        fs::create_dir_all(&rules_dir)?;

        fs::write(workspace_path.join("CLAUDE.md"), default_claude_md())?;
        fs::write(
            rules_dir.join("classify-policy.md"),
            default_classify_policy(),
        )?;
        fs::write(rules_dir.join("hitl-policy.md"), default_hitl_policy())?;
        fs::write(
            rules_dir.join("auto-approve-policy.md"),
            default_auto_approve_policy(),
        )?;

        Ok(ClawWorkspace {
            path: workspace_path,
        })
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

/// Returns the default classify-policy template.
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
    fn init_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let ws1 = ClawWorkspace::init(tmp.path()).unwrap();
        let ws2 = ClawWorkspace::init(tmp.path()).unwrap();
        assert_eq!(ws1.path, ws2.path);
    }

    #[test]
    fn default_templates_are_not_empty() {
        assert!(!default_claude_md().is_empty());
        assert!(!default_classify_policy().is_empty());
        assert!(!default_hitl_policy().is_empty());
        assert!(!default_auto_approve_policy().is_empty());
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
}
