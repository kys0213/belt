//! Claude Code slash command plugin installer for `/auto`.
//!
//! Writes `.md` command files into the project's `.claude/commands/` directory
//! so that Claude Code recognises `/auto` as a slash command and routes
//! subcommands to the `belt` CLI.

use std::fs;
use std::path::{Path, PathBuf};

/// Describes a single slash-command file to install.
struct CommandFile {
    /// File name (e.g. `auto.md`).
    name: &'static str,
    /// Markdown content with YAML front-matter.
    content: &'static str,
}

/// All command files that make up the `/auto` plugin.
fn command_files() -> Vec<CommandFile> {
    vec![CommandFile {
        name: "auto.md",
        content: AUTO_COMMAND_MD,
    }]
}

/// Install the `/auto` slash command plugin into the given project root.
///
/// Creates `<project_root>/.claude/commands/auto.md` containing the slash
/// command definition.  Existing files are only overwritten when `force` is
/// `true`.
///
/// Returns the list of paths that were written.
pub fn install(project_root: &Path, force: bool) -> anyhow::Result<Vec<PathBuf>> {
    let commands_dir = project_root.join(".claude/commands");
    fs::create_dir_all(&commands_dir)?;

    let mut written: Vec<PathBuf> = Vec::new();

    for file in command_files() {
        let dest = commands_dir.join(file.name);
        if dest.exists() && !force {
            tracing::info!(path = %dest.display(), "file already exists, skipping (use --force to overwrite)");
            continue;
        }
        fs::write(&dest, file.content)?;
        written.push(dest);
    }

    Ok(written)
}

/// Remove the `/auto` slash command plugin from the given project root.
///
/// Deletes `<project_root>/.claude/commands/auto.md` if it exists.
///
/// Returns the list of paths that were removed.
pub fn uninstall(project_root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let commands_dir = project_root.join(".claude/commands");
    let mut removed: Vec<PathBuf> = Vec::new();

    for file in command_files() {
        let dest = commands_dir.join(file.name);
        if dest.exists() {
            fs::remove_file(&dest)?;
            removed.push(dest);
        }
    }

    Ok(removed)
}

/// Check whether the `/auto` plugin is installed in the given project root.
pub fn is_installed(project_root: &Path) -> bool {
    let commands_dir = project_root.join(".claude/commands");
    command_files()
        .iter()
        .all(|f| commands_dir.join(f.name).exists())
}

// ---------------------------------------------------------------------------
// Slash command markdown template
// ---------------------------------------------------------------------------

/// The `/auto` slash command definition for Claude Code.
///
/// This markdown file is placed in `.claude/commands/auto.md` and instructs
/// Claude Code how to handle `/auto <subcommand>` invocations.
const AUTO_COMMAND_MD: &str = r#"---
description: Control the belt autopilot daemon (start/stop/restart/status)
argument-hint: "<start|stop|restart|status> [options]"
allowed-tools:
  - Bash
---

# /auto -- Belt Autopilot Daemon Control

Runs belt daemon control commands inside the current project.

## Usage

The user invokes `/auto <subcommand>` where subcommand is one of:

| Subcommand | Belt CLI equivalent | Description |
|------------|-------------------|-------------|
| `start`    | `belt start`      | Start the daemon |
| `stop`     | `belt stop`       | Stop the daemon |
| `restart`  | `belt restart`    | Restart the daemon |
| `status`   | `belt status --format json` | Show daemon and queue status |

## Execution

Parse the argument to determine the subcommand and run the corresponding
belt CLI command via Bash:

1. If the argument is empty or `status`, run: `belt status --format json`
2. If the argument is `start`, run: `belt start`
3. If the argument is `stop`, run: `belt stop`
4. If the argument is `restart`, run: `belt restart`
5. For any other argument, show this help message.

Always capture and display the command output to the user.

## Examples

```
/auto start        => belt start
/auto stop         => belt stop
/auto restart      => belt restart
/auto status       => belt status --format json
/auto              => belt status --format json
```
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_creates_command_file() {
        let tmp = tempfile::tempdir().unwrap();
        let written = install(tmp.path(), false).unwrap();

        assert_eq!(written.len(), 1);
        let auto_md = tmp.path().join(".claude/commands/auto.md");
        assert!(auto_md.is_file());

        let content = fs::read_to_string(&auto_md).unwrap();
        assert!(content.contains("belt autopilot daemon"));
        assert!(content.contains("belt start"));
        assert!(content.contains("belt stop"));
        assert!(content.contains("belt restart"));
    }

    #[test]
    fn install_skips_existing_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        install(tmp.path(), false).unwrap();

        // Write custom content.
        let auto_md = tmp.path().join(".claude/commands/auto.md");
        let custom = "# custom";
        fs::write(&auto_md, custom).unwrap();

        // Re-install without force -- should skip.
        let written = install(tmp.path(), false).unwrap();
        assert!(written.is_empty());

        let content = fs::read_to_string(&auto_md).unwrap();
        assert_eq!(content, custom);
    }

    #[test]
    fn install_force_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        install(tmp.path(), false).unwrap();

        let auto_md = tmp.path().join(".claude/commands/auto.md");
        fs::write(&auto_md, "# custom").unwrap();

        let written = install(tmp.path(), true).unwrap();
        assert_eq!(written.len(), 1);

        let content = fs::read_to_string(&auto_md).unwrap();
        assert!(content.contains("belt autopilot daemon"));
    }

    #[test]
    fn uninstall_removes_command_file() {
        let tmp = tempfile::tempdir().unwrap();
        install(tmp.path(), false).unwrap();

        let auto_md = tmp.path().join(".claude/commands/auto.md");
        assert!(auto_md.is_file());

        let removed = uninstall(tmp.path()).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(!auto_md.exists());
    }

    #[test]
    fn uninstall_noop_when_not_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let removed = uninstall(tmp.path()).unwrap();
        assert!(removed.is_empty());
    }

    #[test]
    fn is_installed_reports_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_installed(tmp.path()));

        install(tmp.path(), false).unwrap();
        assert!(is_installed(tmp.path()));

        uninstall(tmp.path()).unwrap();
        assert!(!is_installed(tmp.path()));
    }

    #[test]
    fn command_template_has_valid_frontmatter() {
        // The template must start with YAML front-matter delimiters.
        assert!(AUTO_COMMAND_MD.starts_with("---\n"));
        assert!(AUTO_COMMAND_MD.contains("\n---\n"));
        // Must declare allowed-tools.
        assert!(AUTO_COMMAND_MD.contains("allowed-tools:"));
        assert!(AUTO_COMMAND_MD.contains("Bash"));
    }
}
