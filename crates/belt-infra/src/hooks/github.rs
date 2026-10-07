//! `GitHubLifecycleHook` — GitHub-specific lifecycle hook implementation.
//!
//! The hook only reflects HITL state on the origin system, through the
//! HITL label. Messages to people (progress, HITL requests) belong to the
//! origin `NotificationChannel`, so no callback posts a comment.
//! - `on_hitl_opened`: add the HITL label
//! - `on_hitl_resolved`: remove the HITL label (idempotent)
//! - every other callback: no `gh` call

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use belt_core::escalation::EscalationAction;
use belt_core::hitl::HitlAction;
use belt_core::lifecycle::{HookContext, LifecycleHook};
use belt_core::platform::ShellExecutor;

/// Configuration for GitHub lifecycle hook behavior.
///
/// Parsed from the `hooks` section of a workspace yaml's source config.
/// When absent, sensible defaults are used.
#[derive(Debug, Clone)]
pub struct GitHubHookConfig {
    /// Repository in `owner/repo` format.
    pub repo: String,
    /// Label present while a HITL request is open.
    pub hitl_label: String,
}

impl GitHubHookConfig {
    /// Create a config with default behavior for the given repository.
    pub fn new(repo: &str) -> Self {
        Self {
            repo: repo.to_string(),
            hitl_label: "belt:needs-human".to_string(),
        }
    }

    /// Override the HITL label.
    pub fn with_hitl_label(mut self, label: &str) -> Self {
        self.hitl_label = label.to_string();
        self
    }
}

/// GitHub-specific lifecycle hook.
///
/// Executes `gh` CLI commands to reflect phase transitions on GitHub
/// issues/PRs. Environment variables `WORK_ID` and `WORKTREE` are
/// injected per the Belt convention.
pub struct GitHubLifecycleHook {
    config: GitHubHookConfig,
    shell: Arc<dyn ShellExecutor>,
}

impl GitHubLifecycleHook {
    /// Create a new GitHub lifecycle hook.
    pub fn new(config: GitHubHookConfig, shell: Arc<dyn ShellExecutor>) -> Self {
        Self { config, shell }
    }

    /// Extract the issue/PR number from a work_id.
    ///
    /// Work IDs follow the pattern `github:owner/repo#NUMBER:state`.
    /// Returns `None` if the pattern doesn't match.
    fn extract_number(work_id: &str) -> Option<&str> {
        let after_hash = work_id.split('#').nth(1)?;
        let number = after_hash.split(':').next()?;
        if number.chars().all(|c| c.is_ascii_digit()) && !number.is_empty() {
            Some(number)
        } else {
            None
        }
    }

    /// Build the standard environment variables for gh CLI execution.
    fn build_env(ctx: &HookContext) -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert("WORK_ID".to_string(), ctx.work_id.clone());
        env.insert(
            "WORKTREE".to_string(),
            ctx.worktree.to_string_lossy().to_string(),
        );
        env
    }

    /// Add or remove the HITL label on the issue/PR of `ctx`. An item that
    /// is not a GitHub issue/PR is skipped.
    async fn edit_label(&self, ctx: &HookContext, flag: &str) -> Result<()> {
        let Some(number) = Self::extract_number(&ctx.work_id) else {
            tracing::debug!(
                work_id = %ctx.work_id,
                "skipping HITL label edit: could not extract issue number"
            );
            return Ok(());
        };
        let cmd = format!(
            "gh issue edit {number} --repo {repo} {flag} {label}",
            repo = self.config.repo,
            label = self.config.hitl_label,
        );
        self.run_gh(&cmd, &ctx.worktree, ctx).await
    }

    /// Execute a gh CLI command in the worktree directory.
    async fn run_gh(&self, command: &str, worktree: &Path, ctx: &HookContext) -> Result<()> {
        let env = Self::build_env(ctx);
        let output = self.shell.execute(command, worktree, &env).await?;
        if !output.success() {
            let code = output.exit_code.unwrap_or(-1);
            anyhow::bail!("gh command failed (exit {code}): {}", output.stderr);
        }
        Ok(())
    }
}

#[async_trait]
impl LifecycleHook for GitHubLifecycleHook {
    async fn on_enter(&self, _ctx: &HookContext) -> Result<()> {
        Ok(())
    }

    async fn on_done(&self, _ctx: &HookContext) -> Result<()> {
        Ok(())
    }

    async fn on_fail(&self, _ctx: &HookContext) -> Result<()> {
        Ok(())
    }

    async fn on_escalation(&self, _ctx: &HookContext, _action: EscalationAction) -> Result<()> {
        Ok(())
    }

    async fn on_hitl_opened(&self, ctx: &HookContext) -> Result<()> {
        self.edit_label(ctx, "--add-label").await
    }

    async fn on_hitl_resolved(&self, ctx: &HookContext, _action: HitlAction) -> Result<()> {
        self.edit_label(ctx, "--remove-label").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::context::{ItemContext, QueueContext, SourceContext};
    use belt_core::error::BeltError;
    use belt_core::hitl::HitlAction;
    use belt_core::platform::ShellOutput;
    use belt_core::queue::testing::test_item;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Mock shell that records executed commands.
    struct RecordingShell {
        succeed: bool,
        commands: Mutex<Vec<String>>,
    }

    impl RecordingShell {
        fn new(succeed: bool) -> Self {
            Self {
                succeed,
                commands: Mutex::new(Vec::new()),
            }
        }

        fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ShellExecutor for RecordingShell {
        async fn execute(
            &self,
            command: &str,
            _working_dir: &Path,
            _env_vars: &HashMap<String, String>,
        ) -> Result<ShellOutput, BeltError> {
            self.commands.lock().unwrap().push(command.to_string());
            if self.succeed {
                Ok(ShellOutput {
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                })
            } else {
                Ok(ShellOutput {
                    exit_code: Some(1),
                    stdout: String::new(),
                    stderr: "mock failure".to_string(),
                })
            }
        }
    }

    fn make_hook_context(state: &str) -> HookContext {
        let item = test_item("github:org/repo#42", state);
        HookContext {
            work_id: item.work_id.clone(),
            worktree: PathBuf::from("/tmp/belt/test-ws-42"),
            item,
            item_context: ItemContext {
                work_id: format!("github:org/repo#42:{state}"),
                workspace: "test-ws".to_string(),
                queue: QueueContext {
                    phase: "running".to_string(),
                    state: state.to_string(),
                    source_id: "github:org/repo#42".to_string(),
                    derived_from: None,
                },
                source: SourceContext {
                    source_type: "github".to_string(),
                    url: "https://github.com/org/repo".to_string(),
                    default_branch: Some("main".to_string()),
                },
                issue: None,
                pr: None,
                history: vec![],
                worktree: Some("/tmp/belt/test-ws-42".to_string()),
                source_data: serde_json::Value::Null,
            },
            failure_count: 0,
        }
    }

    fn make_hook(shell: Arc<RecordingShell>) -> GitHubLifecycleHook {
        let config = GitHubHookConfig::new("org/repo");
        GitHubLifecycleHook::new(config, shell)
    }

    #[test]
    fn extract_number_from_work_id() {
        assert_eq!(
            GitHubLifecycleHook::extract_number("github:org/repo#42:implement"),
            Some("42")
        );
        assert_eq!(
            GitHubLifecycleHook::extract_number("github:org/repo#123:review"),
            Some("123")
        );
        assert_eq!(GitHubLifecycleHook::extract_number("no-hash-here"), None);
        assert_eq!(
            GitHubLifecycleHook::extract_number("github:org/repo#:state"),
            None
        );
    }

    /// `gh issue comment`처럼 사람 대상 메시지를 남기는 호출이 있는지.
    fn comments(cmds: &[String]) -> Vec<&String> {
        cmds.iter().filter(|c| c.contains("comment")).collect()
    }

    #[tokio::test]
    async fn progress_and_escalation_callbacks_run_no_gh_command() {
        let shell = Arc::new(RecordingShell::new(true));
        let hook = make_hook(shell.clone());
        let ctx = make_hook_context("implement");

        hook.on_enter(&ctx).await.unwrap();
        hook.on_done(&ctx).await.unwrap();
        hook.on_fail(&ctx).await.unwrap();
        for action in [
            EscalationAction::Retry,
            EscalationAction::RetryWithComment,
            EscalationAction::Hitl,
            EscalationAction::Skip,
            EscalationAction::Replan,
        ] {
            hook.on_escalation(&ctx, action).await.unwrap();
        }

        assert!(shell.commands().is_empty(), "{:?}", shell.commands());
    }

    #[tokio::test]
    async fn on_hitl_opened_adds_only_the_label() {
        let shell = Arc::new(RecordingShell::new(true));
        let hook = make_hook(shell.clone());

        hook.on_hitl_opened(&make_hook_context("implement"))
            .await
            .unwrap();

        let cmds = shell.commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("gh issue edit 42"));
        assert!(cmds[0].contains("--repo org/repo"));
        assert!(cmds[0].contains("--add-label belt:needs-human"));
        assert!(comments(&cmds).is_empty());
    }

    #[tokio::test]
    async fn on_hitl_opened_uses_custom_label() {
        let shell = Arc::new(RecordingShell::new(true));
        let config = GitHubHookConfig::new("org/repo").with_hitl_label("custom:help");
        let hook = GitHubLifecycleHook::new(config, shell.clone());

        hook.on_hitl_opened(&make_hook_context("implement"))
            .await
            .unwrap();

        assert!(shell.commands()[0].contains("--add-label custom:help"));
    }

    #[tokio::test]
    async fn on_hitl_resolved_removes_the_label_and_is_repeatable() {
        let shell = Arc::new(RecordingShell::new(true));
        let hook = make_hook(shell.clone());
        let ctx = make_hook_context("implement");

        hook.on_hitl_resolved(&ctx, HitlAction::Done).await.unwrap();
        hook.on_hitl_resolved(&ctx, HitlAction::Done).await.unwrap();

        let cmds = shell.commands();
        assert_eq!(cmds.len(), 2);
        for cmd in &cmds {
            assert!(cmd.contains("gh issue edit 42"));
            assert!(cmd.contains("--remove-label belt:needs-human"));
        }
        assert!(comments(&cmds).is_empty());
    }

    #[tokio::test]
    async fn shell_failure_propagates() {
        let shell = Arc::new(RecordingShell::new(false));
        let hook = make_hook(shell);
        let ctx = make_hook_context("implement");

        assert!(hook.on_hitl_opened(&ctx).await.is_err());
        assert!(hook.on_hitl_resolved(&ctx, HitlAction::Skip).await.is_err());
    }

    #[tokio::test]
    async fn invalid_work_id_skips_silently() {
        let shell = Arc::new(RecordingShell::new(true));
        let hook = make_hook(shell.clone());

        let item = test_item("jira:PROJ-42", "implement");
        let ctx = HookContext {
            work_id: item.work_id.clone(),
            worktree: PathBuf::from("/tmp/belt/test-ws-42"),
            item,
            item_context: ItemContext {
                work_id: "jira:PROJ-42:implement".to_string(),
                workspace: "test-ws".to_string(),
                queue: QueueContext {
                    phase: "running".to_string(),
                    state: "implement".to_string(),
                    source_id: "jira:PROJ-42".to_string(),
                    derived_from: None,
                },
                source: SourceContext {
                    source_type: "jira".to_string(),
                    url: "https://jira.example.com".to_string(),
                    default_branch: None,
                },
                issue: None,
                pr: None,
                history: vec![],
                worktree: Some("/tmp/belt/test-ws-42".to_string()),
                source_data: serde_json::Value::Null,
            },
            failure_count: 0,
        };

        // Should not error, just skip.
        hook.on_hitl_opened(&ctx).await.unwrap();
        hook.on_hitl_resolved(&ctx, HitlAction::Done).await.unwrap();
        assert!(shell.commands().is_empty());
    }

    #[test]
    fn github_hook_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GitHubLifecycleHook>();
    }

    #[test]
    fn config_builder_pattern() {
        let config = GitHubHookConfig::new("org/repo").with_hitl_label("custom:label");
        assert_eq!(config.hitl_label, "custom:label");
        assert_eq!(config.repo, "org/repo");
    }

    #[tokio::test]
    async fn env_vars_injected() {
        /// Shell that captures env vars.
        struct EnvCapture {
            env_vars: Mutex<Vec<HashMap<String, String>>>,
        }

        #[async_trait]
        impl ShellExecutor for EnvCapture {
            async fn execute(
                &self,
                _command: &str,
                _working_dir: &Path,
                env_vars: &HashMap<String, String>,
            ) -> Result<ShellOutput, BeltError> {
                self.env_vars.lock().unwrap().push(env_vars.clone());
                Ok(ShellOutput {
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
        }

        let shell = Arc::new(EnvCapture {
            env_vars: Mutex::new(Vec::new()),
        });
        let config = GitHubHookConfig::new("org/repo");
        let hook = GitHubLifecycleHook::new(config, shell.clone());
        let ctx = make_hook_context("implement");

        hook.on_hitl_opened(&ctx).await.unwrap();

        let captured = shell.env_vars.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(
            captured[0].get("WORK_ID").unwrap(),
            "github:org/repo#42:implement"
        );
        assert_eq!(captured[0].get("WORKTREE").unwrap(), "/tmp/belt/test-ws-42");
    }
}
