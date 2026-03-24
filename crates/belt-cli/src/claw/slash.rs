//! Slash command parsing and dispatch for the Claw interactive session.
//!
//! Supports `/auto`, `/spec`, `/claw`, `/help`, and `/quit` commands.

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
            "[spec] Usage: /spec <issue-number|description>".to_string()
        } else {
            format!("[spec] Generating spec for: {args}")
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
  /spec <desc>   Generate or display a spec for the given description
  /claw [sub]    Claw workspace management (status, init, rules)
  /help          Show this help message
  /quit          Exit the session"
            .to_string()
    }
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
