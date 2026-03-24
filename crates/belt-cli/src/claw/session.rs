//! Interactive REPL session for Claw.
//!
//! Provides a simple command loop that reads user input, dispatches slash
//! commands, and prints responses.

use std::io::{self, BufRead, Write};

use super::ClawWorkspace;
use super::slash::{SlashCommand, SlashDispatcher};

/// Interactive session configuration.
pub struct SessionConfig {
    /// Workspace name context (if running inside a specific workspace).
    pub workspace: Option<String>,
    /// The Claw workspace root.
    pub claw_workspace: ClawWorkspace,
}

/// Run the interactive REPL session.
///
/// Reads lines from `input`, writes prompts/responses to `output`.
/// This signature allows testing without real stdio.
pub fn run_session<R: BufRead, W: Write>(
    config: &SessionConfig,
    input: &mut R,
    output: &mut W,
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
    writeln!(output, "Type /help for available commands, /quit to exit.")?;
    writeln!(output)?;

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
                    writeln!(output, "Goodbye.")?;
                    break;
                }
                _ => {
                    let response = dispatcher.dispatch(&cmd);
                    writeln!(output, "{response}")?;
                }
            }
        } else {
            // Free-form text input (future: pass to LLM agent).
            writeln!(output, ">> {trimmed}")?;
        }
    }

    Ok(())
}

/// Run the interactive session using real stdin/stdout.
pub fn run_interactive(config: SessionConfig) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    run_session(&config, &mut reader, &mut writer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn make_config(tmp: &tempfile::TempDir) -> SessionConfig {
        let ws = ClawWorkspace::init(tmp.path()).unwrap();
        SessionConfig {
            workspace: Some("test-ws".to_string()),
            claw_workspace: ws,
        }
    }

    #[test]
    fn session_quit_on_slash_quit() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Goodbye."));
    }

    #[test]
    fn session_quit_on_eof() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Belt Claw interactive session"));
    }

    #[test]
    fn session_dispatches_help() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/help\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("/auto"));
        assert!(out.contains("/spec"));
        assert!(out.contains("/claw"));
    }

    #[test]
    fn session_echoes_freeform_text() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"hello world\n/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains(">> hello world"));
    }

    #[test]
    fn session_shows_workspace_context() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(&tmp);
        let mut input = Cursor::new(b"/quit\n" as &[u8]);
        let mut output = Vec::new();
        run_session(&config, &mut input, &mut output).unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Context: test-ws"));
    }
}
