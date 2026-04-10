//! LifecycleHook dynamic loading — factory pattern for DataSource-specific hooks.
//!
//! Hook implementations are created on demand (not at Daemon startup) to
//! support dynamic workspace addition and configuration changes without
//! restart.  The factory inspects the DataSource type (determined by the
//! source key in workspace yaml) and returns the appropriate hook impl.
//!
//! # Flow
//!
//! ```text
//! hook trigger → DB workspace lookup → yaml parse
//!   → source type identification → create_hook() → hook.on_*() → drop
//! ```

pub mod github;

use std::sync::Arc;

use anyhow::{Result, bail};

use belt_core::lifecycle::LifecycleHook;
use belt_core::platform::ShellExecutor;

use github::{GitHubHookConfig, GitHubLifecycleHook};

/// Parameters for creating a lifecycle hook instance.
///
/// Bundles everything the factory needs to construct the right hook
/// implementation without requiring callers to know the concrete types.
#[derive(Debug, Clone)]
pub struct HookParams {
    /// DataSource type identifier (e.g. "github", "jira").
    pub source_type: String,
    /// Repository or project URL used by the hook for API calls.
    pub repo: String,
    /// Optional HITL label override.
    pub hitl_label: Option<String>,
    /// Whether to post comments on successful completion.
    pub comment_on_done: bool,
    /// Whether to post comments on failure.
    pub comment_on_fail: bool,
}

impl HookParams {
    /// Create minimal parameters for a given source type and repo.
    pub fn new(source_type: &str, repo: &str) -> Self {
        Self {
            source_type: source_type.to_string(),
            repo: repo.to_string(),
            hitl_label: None,
            comment_on_done: false,
            comment_on_fail: true,
        }
    }
}

/// Create a lifecycle hook for the given DataSource type.
///
/// This is the main entry point for hook dynamic loading. The Daemon
/// calls this at hook trigger time with parameters derived from the
/// workspace configuration (DB + yaml).
///
/// # Supported source types
///
/// | source_type | Hook implementation        |
/// |-------------|----------------------------|
/// | `"github"`  | `GitHubLifecycleHook`      |
///
/// Unknown source types return an error. Callers should fall back to
/// `ScriptLifecycleHook` or `NoopLifecycleHook` when appropriate.
///
/// # Examples
///
/// ```ignore
/// let params = HookParams::new("github", "org/repo");
/// let hook = create_hook(&params, shell.clone())?;
/// hook.on_enter(&ctx).await?;
/// ```
pub fn create_hook(
    params: &HookParams,
    shell: Arc<dyn ShellExecutor>,
) -> Result<Box<dyn LifecycleHook>> {
    match params.source_type.as_str() {
        "github" => {
            let mut config = GitHubHookConfig::new(&params.repo);
            config.comment_on_done = params.comment_on_done;
            config.comment_on_fail = params.comment_on_fail;
            if let Some(ref label) = params.hitl_label {
                config = config.with_hitl_label(label);
            }
            Ok(Box::new(GitHubLifecycleHook::new(config, shell)))
        }
        other => {
            bail!(
                "unsupported DataSource type for lifecycle hook: '{other}'. \
                 Use ScriptLifecycleHook or NoopLifecycleHook as fallback."
            )
        }
    }
}

/// Resolve the appropriate hook for a workspace source configuration.
///
/// Inspects the source key name and URL to determine the DataSource type,
/// then delegates to [`create_hook`]. If the source type is not recognized,
/// returns `None` so the caller can fall back to `ScriptLifecycleHook`.
pub fn resolve_hook(
    source_key: &str,
    source_url: &str,
    shell: Arc<dyn ShellExecutor>,
) -> Option<Box<dyn LifecycleHook>> {
    // Determine source type from the yaml source key name.
    let source_type = match source_key {
        "github" => "github",
        _ => return None,
    };

    // Extract repo identifier from URL.
    let repo = extract_repo_from_url(source_url).unwrap_or_else(|| source_url.to_string());

    let params = HookParams::new(source_type, &repo);
    create_hook(&params, shell).ok()
}

/// Extract `owner/repo` from a GitHub URL.
///
/// Supports `https://github.com/owner/repo` and `git@github.com:owner/repo.git`.
fn extract_repo_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    let parts: Vec<&str> = trimmed.split('/').collect();
    if parts.len() >= 2 {
        Some(format!(
            "{}/{}",
            parts[parts.len() - 2],
            parts[parts.len() - 1]
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::context::{ItemContext, QueueContext, SourceContext};
    use belt_core::error::BeltError;
    use belt_core::lifecycle::HookContext;
    use belt_core::platform::ShellOutput;
    use belt_core::queue::testing::test_item;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    struct StubShell;

    #[async_trait::async_trait]
    impl ShellExecutor for StubShell {
        async fn execute(
            &self,
            _command: &str,
            _working_dir: &Path,
            _env_vars: &HashMap<String, String>,
        ) -> std::result::Result<ShellOutput, BeltError> {
            Ok(ShellOutput {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    fn make_hook_context() -> HookContext {
        let item = test_item("github:org/repo#42", "implement");
        HookContext {
            work_id: item.work_id.clone(),
            worktree: PathBuf::from("/tmp/belt/test-ws-42"),
            item,
            item_context: ItemContext {
                work_id: "github:org/repo#42:implement".to_string(),
                workspace: "test-ws".to_string(),
                queue: QueueContext {
                    phase: "running".to_string(),
                    state: "implement".to_string(),
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

    #[test]
    fn create_github_hook_succeeds() {
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);
        let params = HookParams::new("github", "org/repo");
        let hook = create_hook(&params, shell);
        assert!(hook.is_ok());
    }

    #[test]
    fn create_hook_unknown_type_fails() {
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);
        let params = HookParams::new("jira", "PROJ");
        let hook = create_hook(&params, shell);
        match hook {
            Ok(_) => panic!("expected error for unsupported type"),
            Err(err) => {
                let msg = err.to_string();
                assert!(msg.contains("unsupported"));
                assert!(msg.contains("jira"));
            }
        }
    }

    #[test]
    fn create_hook_with_custom_hitl_label() {
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);
        let mut params = HookParams::new("github", "org/repo");
        params.hitl_label = Some("custom:needs-help".to_string());
        let hook = create_hook(&params, shell);
        assert!(hook.is_ok());
    }

    #[tokio::test]
    async fn factory_hook_is_functional() {
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);
        let params = HookParams::new("github", "org/repo");
        let hook = create_hook(&params, shell).unwrap();
        let ctx = make_hook_context();

        // Verify the hook can be called through the trait object.
        assert!(hook.on_enter(&ctx).await.is_ok());
        assert!(hook.on_done(&ctx).await.is_ok());
        assert!(hook.on_fail(&ctx).await.is_ok());
        assert!(
            hook.on_escalation(&ctx, belt_core::escalation::EscalationAction::Hitl)
                .await
                .is_ok()
        );
    }

    #[test]
    fn resolve_hook_github() {
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);
        let hook = resolve_hook("github", "https://github.com/org/repo", shell);
        assert!(hook.is_some());
    }

    #[test]
    fn resolve_hook_unknown_returns_none() {
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);
        let hook = resolve_hook("jira", "https://jira.example.com", shell);
        assert!(hook.is_none());
    }

    #[test]
    fn extract_repo_from_github_url() {
        assert_eq!(
            extract_repo_from_url("https://github.com/org/repo"),
            Some("org/repo".to_string())
        );
        assert_eq!(
            extract_repo_from_url("https://github.com/org/repo/"),
            Some("org/repo".to_string())
        );
        assert_eq!(
            extract_repo_from_url("https://github.com/org/repo.git"),
            Some("org/repo".to_string())
        );
    }

    #[test]
    fn hook_params_defaults() {
        let params = HookParams::new("github", "org/repo");
        assert_eq!(params.source_type, "github");
        assert_eq!(params.repo, "org/repo");
        assert!(params.hitl_label.is_none());
        assert!(!params.comment_on_done);
        assert!(params.comment_on_fail);
    }
}
