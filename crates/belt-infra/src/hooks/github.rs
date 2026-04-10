//! `GitHubLifecycleHook` — GitHub-specific lifecycle hook implementation.
//!
//! Reacts to phase transitions by executing `gh` CLI commands:
//! - `on_enter`: post a "work started" comment
//! - `on_done`: post a "completed" comment, remove trigger label
//! - `on_fail`: post a failure comment with error details
//! - `on_escalation`: add HITL label, post escalation comments

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use belt_core::escalation::EscalationAction;
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
    /// Whether to post comments on phase transitions.
    pub comment_on_done: bool,
    /// Whether to post comments on failure.
    pub comment_on_fail: bool,
    /// Label to add when HITL escalation occurs.
    pub hitl_label: String,
}

impl GitHubHookConfig {
    /// Create a config with default behavior for the given repository.
    pub fn new(repo: &str) -> Self {
        Self {
            repo: repo.to_string(),
            comment_on_done: false,
            comment_on_fail: true,
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
    async fn on_enter(&self, ctx: &HookContext) -> Result<()> {
        let Some(number) = Self::extract_number(&ctx.work_id) else {
            tracing::debug!(
                work_id = %ctx.work_id,
                "skipping on_enter: could not extract issue number"
            );
            return Ok(());
        };

        let cmd = format!(
            "gh issue comment {number} --repo {repo} --body 'Belt: started processing (state: {state})'",
            repo = self.config.repo,
            state = ctx.item.state,
        );
        self.run_gh(&cmd, &ctx.worktree, ctx).await
    }

    async fn on_done(&self, ctx: &HookContext) -> Result<()> {
        if !self.config.comment_on_done {
            return Ok(());
        }

        let Some(number) = Self::extract_number(&ctx.work_id) else {
            return Ok(());
        };

        let cmd = format!(
            "gh issue comment {number} --repo {repo} --body 'Belt: completed (state: {state})'",
            repo = self.config.repo,
            state = ctx.item.state,
        );
        self.run_gh(&cmd, &ctx.worktree, ctx).await
    }

    async fn on_fail(&self, ctx: &HookContext) -> Result<()> {
        if !self.config.comment_on_fail {
            return Ok(());
        }

        let Some(number) = Self::extract_number(&ctx.work_id) else {
            return Ok(());
        };

        let cmd = format!(
            "gh issue comment {number} --repo {repo} --body 'Belt: failed (state: {state}, failures: {count})'",
            repo = self.config.repo,
            state = ctx.item.state,
            count = ctx.failure_count,
        );
        self.run_gh(&cmd, &ctx.worktree, ctx).await
    }

    async fn on_escalation(&self, ctx: &HookContext, action: EscalationAction) -> Result<()> {
        let Some(number) = Self::extract_number(&ctx.work_id) else {
            return Ok(());
        };

        match action {
            EscalationAction::Hitl | EscalationAction::Replan => {
                // Add HITL label to the issue.
                let label_cmd = format!(
                    "gh issue edit {number} --repo {repo} --add-label {label}",
                    repo = self.config.repo,
                    label = self.config.hitl_label,
                );
                self.run_gh(&label_cmd, &ctx.worktree, ctx).await?;

                // Post escalation comment.
                let action_str = action.to_string();
                let comment_cmd = format!(
                    "gh issue comment {number} --repo {repo} --body 'Belt: escalation ({action_str}) — human intervention requested'",
                    repo = self.config.repo,
                );
                self.run_gh(&comment_cmd, &ctx.worktree, ctx).await?;
            }
            EscalationAction::RetryWithComment => {
                let comment_cmd = format!(
                    "gh issue comment {number} --repo {repo} --body 'Belt: retrying after failure (attempt {count})'",
                    repo = self.config.repo,
                    count = ctx.failure_count + 1,
                );
                self.run_gh(&comment_cmd, &ctx.worktree, ctx).await?;
            }
            EscalationAction::Retry => {
                // Silent retry — no GitHub interaction.
            }
            EscalationAction::Skip => {
                let comment_cmd = format!(
                    "gh issue comment {number} --repo {repo} --body 'Belt: skipping item after {count} failures'",
                    repo = self.config.repo,
                    count = ctx.failure_count,
                );
                self.run_gh(&comment_cmd, &ctx.worktree, ctx).await?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::context::{ItemContext, QueueContext, SourceContext};
    use belt_core::error::BeltError;
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

    #[tokio::test]
    async fn on_enter_posts_comment() {
        let shell = Arc::new(RecordingShell::new(true));
        let hook = make_hook(shell.clone());
        let ctx = make_hook_context("implement");

        hook.on_enter(&ctx).await.unwrap();

        let cmds = shell.commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("gh issue comment 42"));
        assert!(cmds[0].contains("started processing"));
    }

    #[tokio::test]
    async fn on_done_skips_when_comment_disabled() {
        let shell = Arc::new(RecordingShell::new(true));
        // Default config has comment_on_done = false
        let hook = make_hook(shell.clone());
        let ctx = make_hook_context("implement");

        hook.on_done(&ctx).await.unwrap();

        assert!(shell.commands().is_empty());
    }

    #[tokio::test]
    async fn on_done_posts_comment_when_enabled() {
        let shell = Arc::new(RecordingShell::new(true));
        let mut config = GitHubHookConfig::new("org/repo");
        config.comment_on_done = true;
        let hook = GitHubLifecycleHook::new(config, shell.clone());
        let ctx = make_hook_context("implement");

        hook.on_done(&ctx).await.unwrap();

        let cmds = shell.commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("completed"));
    }

    #[tokio::test]
    async fn on_fail_posts_comment() {
        let shell = Arc::new(RecordingShell::new(true));
        let hook = make_hook(shell.clone());
        let ctx = make_hook_context("implement");

        hook.on_fail(&ctx).await.unwrap();

        let cmds = shell.commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("failed"));
        assert!(cmds[0].contains("failures: 0"));
    }

    #[tokio::test]
    async fn on_escalation_hitl_adds_label_and_comment() {
        let shell = Arc::new(RecordingShell::new(true));
        let hook = make_hook(shell.clone());
        let ctx = make_hook_context("implement");

        hook.on_escalation(&ctx, EscalationAction::Hitl)
            .await
            .unwrap();

        let cmds = shell.commands();
        assert_eq!(cmds.len(), 2);
        assert!(cmds[0].contains("--add-label belt:needs-human"));
        assert!(cmds[1].contains("human intervention requested"));
    }

    #[tokio::test]
    async fn on_escalation_retry_is_silent() {
        let shell = Arc::new(RecordingShell::new(true));
        let hook = make_hook(shell.clone());
        let ctx = make_hook_context("implement");

        hook.on_escalation(&ctx, EscalationAction::Retry)
            .await
            .unwrap();

        assert!(shell.commands().is_empty());
    }

    #[tokio::test]
    async fn on_escalation_retry_with_comment_posts() {
        let shell = Arc::new(RecordingShell::new(true));
        let hook = make_hook(shell.clone());
        let ctx = make_hook_context("implement");

        hook.on_escalation(&ctx, EscalationAction::RetryWithComment)
            .await
            .unwrap();

        let cmds = shell.commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("retrying"));
    }

    #[tokio::test]
    async fn on_escalation_skip_posts_comment() {
        let shell = Arc::new(RecordingShell::new(true));
        let hook = make_hook(shell.clone());
        let ctx = make_hook_context("implement");

        hook.on_escalation(&ctx, EscalationAction::Skip)
            .await
            .unwrap();

        let cmds = shell.commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("skipping"));
    }

    #[tokio::test]
    async fn shell_failure_propagates() {
        let shell = Arc::new(RecordingShell::new(false));
        let hook = make_hook(shell);
        let ctx = make_hook_context("implement");

        let result = hook.on_enter(&ctx).await;
        assert!(result.is_err());
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
        hook.on_enter(&ctx).await.unwrap();
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

        hook.on_enter(&ctx).await.unwrap();

        let captured = shell.env_vars.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(
            captured[0].get("WORK_ID").unwrap(),
            "github:org/repo#42:implement"
        );
        assert_eq!(captured[0].get("WORKTREE").unwrap(), "/tmp/belt/test-ws-42");
    }
}
