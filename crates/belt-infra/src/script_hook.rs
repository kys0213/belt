//! `ScriptLifecycleHook` — adapter that wraps yaml-based `ScriptAction`
//! lists into the `LifecycleHook` trait.
//!
//! This preserves backward compatibility with v5 workspace yaml files
//! where `on_enter`, `on_done`, and `on_fail` are defined as inline
//! script arrays.  The adapter simply executes the corresponding
//! scripts in order when the lifecycle method is called.
//!
//! `on_escalation` is a no-op because v5 yaml has no equivalent concept.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use belt_core::escalation::EscalationAction;
use belt_core::lifecycle::{HookContext, LifecycleHook};
use belt_core::platform::ShellExecutor;
use belt_core::workspace::{ScriptAction, StateConfig};

/// Adapter that executes yaml-defined lifecycle scripts through the
/// `LifecycleHook` trait interface.
///
/// Created from a workspace's `StateConfig` map.  Each hook method
/// looks up the current item's state to find the matching script list.
pub struct ScriptLifecycleHook {
    /// state name -> StateConfig (contains on_enter/on_done/on_fail scripts).
    state_configs: HashMap<String, StateConfig>,
    shell: Arc<dyn ShellExecutor>,
}

impl ScriptLifecycleHook {
    /// Create a new adapter from state configurations and a shell executor.
    pub fn new(state_configs: HashMap<String, StateConfig>, shell: Arc<dyn ShellExecutor>) -> Self {
        Self {
            state_configs,
            shell,
        }
    }

    /// Execute a list of script actions sequentially.
    ///
    /// Environment variables `WORK_ID` and `WORKTREE` are injected per
    /// the Belt convention.  Returns an error on the first non-zero
    /// exit code or execution failure.
    async fn run_scripts(
        &self,
        scripts: &[ScriptAction],
        work_id: &str,
        worktree: &Path,
    ) -> Result<()> {
        let mut env_vars = HashMap::new();
        env_vars.insert("WORK_ID".to_string(), work_id.to_string());
        env_vars.insert(
            "WORKTREE".to_string(),
            worktree.to_string_lossy().to_string(),
        );
        for script in scripts {
            let output = self
                .shell
                .execute(&script.script, worktree, &env_vars)
                .await?;
            if !output.success() {
                let code = output.exit_code.unwrap_or(-1);
                anyhow::bail!("script failed (exit {code}): {}", output.stderr);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl LifecycleHook for ScriptLifecycleHook {
    async fn on_enter(&self, ctx: &HookContext) -> Result<()> {
        if let Some(state_config) = self.state_configs.get(&ctx.item.state) {
            self.run_scripts(&state_config.on_enter, &ctx.work_id, &ctx.worktree)
                .await?;
        }
        Ok(())
    }

    async fn on_done(&self, ctx: &HookContext) -> Result<()> {
        if let Some(state_config) = self.state_configs.get(&ctx.item.state) {
            self.run_scripts(&state_config.on_done, &ctx.work_id, &ctx.worktree)
                .await?;
        }
        Ok(())
    }

    async fn on_fail(&self, ctx: &HookContext) -> Result<()> {
        if let Some(state_config) = self.state_configs.get(&ctx.item.state) {
            self.run_scripts(&state_config.on_fail, &ctx.work_id, &ctx.worktree)
                .await?;
        }
        Ok(())
    }

    async fn on_escalation(&self, _ctx: &HookContext, _action: EscalationAction) -> Result<()> {
        // v5 yaml has no on_escalation concept — no-op.
        Ok(())
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
    use belt_core::workspace::{ScriptAction, StateConfig, TriggerConfig};
    use std::path::PathBuf;

    /// A test shell that returns configurable results.
    struct MockShell {
        succeed: bool,
    }

    #[async_trait]
    impl ShellExecutor for MockShell {
        async fn execute(
            &self,
            _command: &str,
            _working_dir: &Path,
            _env_vars: &HashMap<String, String>,
        ) -> std::result::Result<ShellOutput, BeltError> {
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

    fn make_state_configs() -> HashMap<String, StateConfig> {
        let mut map = HashMap::new();
        map.insert(
            "implement".to_string(),
            StateConfig {
                trigger: TriggerConfig::default(),
                handlers: vec![],
                on_enter: vec![ScriptAction {
                    script: "echo entering".to_string(),
                }],
                on_done: vec![ScriptAction {
                    script: "echo done".to_string(),
                }],
                on_fail: vec![ScriptAction {
                    script: "echo failed".to_string(),
                }],
            },
        );
        map
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

    #[tokio::test]
    async fn on_enter_executes_scripts() {
        let hook =
            ScriptLifecycleHook::new(make_state_configs(), Arc::new(MockShell { succeed: true }));
        let ctx = make_hook_context("implement");
        assert!(hook.on_enter(&ctx).await.is_ok());
    }

    #[tokio::test]
    async fn on_done_executes_scripts() {
        let hook =
            ScriptLifecycleHook::new(make_state_configs(), Arc::new(MockShell { succeed: true }));
        let ctx = make_hook_context("implement");
        assert!(hook.on_done(&ctx).await.is_ok());
    }

    #[tokio::test]
    async fn on_fail_executes_scripts() {
        let hook =
            ScriptLifecycleHook::new(make_state_configs(), Arc::new(MockShell { succeed: true }));
        let ctx = make_hook_context("implement");
        assert!(hook.on_fail(&ctx).await.is_ok());
    }

    #[tokio::test]
    async fn on_escalation_is_noop() {
        let hook =
            ScriptLifecycleHook::new(make_state_configs(), Arc::new(MockShell { succeed: true }));
        let ctx = make_hook_context("implement");
        assert!(
            hook.on_escalation(&ctx, EscalationAction::Hitl)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn script_failure_propagates_error() {
        let hook =
            ScriptLifecycleHook::new(make_state_configs(), Arc::new(MockShell { succeed: false }));
        let ctx = make_hook_context("implement");
        assert!(hook.on_enter(&ctx).await.is_err());
    }

    #[tokio::test]
    async fn unknown_state_is_noop() {
        let hook =
            ScriptLifecycleHook::new(make_state_configs(), Arc::new(MockShell { succeed: true }));
        let ctx = make_hook_context("review"); // not in state_configs
        assert!(hook.on_enter(&ctx).await.is_ok());
        assert!(hook.on_done(&ctx).await.is_ok());
        assert!(hook.on_fail(&ctx).await.is_ok());
    }
}
