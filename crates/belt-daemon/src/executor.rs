use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, bail};

use belt_core::action::Action;
use belt_core::runtime::{RuntimeRegistry, RuntimeRequest, TokenUsage};

/// Action 실행 결과.
#[derive(Debug, Clone)]
pub struct ActionResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration: std::time::Duration,
    /// Token usage reported by the runtime, if available.
    pub token_usage: Option<TokenUsage>,
    /// Name of the runtime that produced this result, if it was a prompt action.
    pub runtime_name: Option<String>,
    /// Model used for the prompt action, if available.
    pub model: Option<String>,
}

impl ActionResult {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Action 배열을 순차 실행하는 실행기.
pub struct ActionExecutor {
    registry: Arc<RuntimeRegistry>,
}

impl ActionExecutor {
    pub fn new(registry: Arc<RuntimeRegistry>) -> Self {
        Self { registry }
    }

    /// Execute all actions sequentially, accumulating token usage across the chain.
    ///
    /// If any action fails (non-zero exit code), execution stops and the failed
    /// result is returned with accumulated token usage up to that point.
    pub async fn execute_all(
        &self,
        actions: &[Action],
        env: &ActionEnv,
    ) -> Result<Option<ActionResult>> {
        let mut total_usage = TokenUsage::default();
        let mut has_usage = false;
        let mut last_result = None;

        for action in actions {
            let result = self.execute_one(action, env).await?;
            if let Some(usage) = &result.token_usage {
                total_usage.input_tokens += usage.input_tokens;
                total_usage.output_tokens += usage.output_tokens;
                if let Some(v) = usage.cache_read_tokens {
                    *total_usage.cache_read_tokens.get_or_insert(0) += v;
                }
                if let Some(v) = usage.cache_write_tokens {
                    *total_usage.cache_write_tokens.get_or_insert(0) += v;
                }
                has_usage = true;
            }
            let failed = !result.success();
            last_result = Some(result);
            if failed {
                break;
            }
        }

        // Attach accumulated token usage to the final result.
        if let Some(ref mut result) = last_result {
            result.token_usage = if has_usage { Some(total_usage) } else { None };
        }

        Ok(last_result)
    }

    pub async fn execute_one(&self, action: &Action, env: &ActionEnv) -> Result<ActionResult> {
        match action {
            Action::Prompt {
                text,
                runtime,
                model,
            } => {
                self.execute_prompt(text, runtime.as_deref(), model.clone(), env)
                    .await
            }
            Action::Script { command } => self.execute_script(command, env).await,
        }
    }

    async fn execute_prompt(
        &self,
        text: &str,
        runtime_name: Option<&str>,
        model: Option<String>,
        env: &ActionEnv,
    ) -> Result<ActionResult> {
        let name = runtime_name.unwrap_or(self.registry.default_name());
        let runtime = self
            .registry
            .resolve(name)
            .ok_or_else(|| anyhow::anyhow!("runtime not found: {name}"))?;

        let request = RuntimeRequest {
            working_dir: env.worktree.clone(),
            prompt: text.to_string(),
            model: model.clone(),
            system_prompt: None,
            session_id: None,
        };

        let response = runtime.invoke(request).await;
        Ok(ActionResult {
            exit_code: response.exit_code,
            stdout: response.stdout,
            stderr: response.stderr,
            duration: response.duration,
            token_usage: response.token_usage,
            runtime_name: Some(name.to_string()),
            model,
        })
    }

    async fn execute_script(&self, command: &str, env: &ActionEnv) -> Result<ActionResult> {
        let start = Instant::now();
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(command);
        cmd.current_dir(&env.worktree);
        cmd.env("WORK_ID", &env.work_id);
        cmd.env("WORKTREE", env.worktree.to_string_lossy().as_ref());
        for (k, v) in &env.extra_vars {
            cmd.env(k, v);
        }

        let output = cmd.output().await;
        let duration = start.elapsed();

        match output {
            Ok(output) => Ok(ActionResult {
                exit_code: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                duration,
                token_usage: None,
                runtime_name: None,
                model: None,
            }),
            Err(e) => bail!("script execution failed: {e}"),
        }
    }
}

/// Action 실행 시 주입되는 환경.
#[derive(Debug, Clone)]
pub struct ActionEnv {
    pub work_id: String,
    pub worktree: PathBuf,
    pub extra_vars: HashMap<String, String>,
}

impl ActionEnv {
    pub fn new(work_id: &str, worktree: &Path) -> Self {
        Self {
            work_id: work_id.to_string(),
            worktree: worktree.to_path_buf(),
            extra_vars: HashMap::new(),
        }
    }

    pub fn with_var(mut self, key: &str, value: &str) -> Self {
        self.extra_vars.insert(key.to_string(), value.to_string());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_infra::runtimes::mock::MockRuntime;

    fn setup_registry() -> Arc<RuntimeRegistry> {
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(MockRuntime::new("mock", vec![0])));
        Arc::new(registry)
    }

    fn test_env() -> ActionEnv {
        ActionEnv::new("test-work-id", Path::new("/tmp"))
    }

    #[tokio::test]
    async fn execute_prompt_success() {
        let executor = ActionExecutor::new(setup_registry());
        let action = Action::prompt("analyze this");
        let result = executor.execute_one(&action, &test_env()).await.unwrap();
        assert!(result.success());
    }

    #[tokio::test]
    async fn execute_script_with_env_vars() {
        let executor = ActionExecutor::new(setup_registry());
        let action = Action::script("echo $WORK_ID");
        let env = ActionEnv::new("my-work-id", Path::new("/tmp"));
        let result = executor.execute_one(&action, &env).await.unwrap();
        assert!(result.success());
        assert!(result.stdout.contains("my-work-id"));
    }

    #[tokio::test]
    async fn execute_script_failure() {
        let executor = ActionExecutor::new(setup_registry());
        let action = Action::script("exit 1");
        let result = executor.execute_one(&action, &test_env()).await.unwrap();
        assert!(!result.success());
    }

    #[tokio::test]
    async fn execute_all_stops_on_failure() {
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(MockRuntime::new("mock", vec![0, 1])));
        let executor = ActionExecutor::new(Arc::new(registry));
        let actions = vec![Action::prompt("first"), Action::prompt("second")];
        let result = executor.execute_all(&actions, &test_env()).await.unwrap();
        assert!(!result.unwrap().success());
    }

    #[tokio::test]
    async fn execute_all_empty() {
        let executor = ActionExecutor::new(setup_registry());
        let result = executor.execute_all(&[], &test_env()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn execute_all_accumulates_token_usage() {
        use belt_core::runtime::TokenUsage;

        let mock = MockRuntime::new("mock", vec![0, 0, 0]).with_token_usages(vec![
            TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: Some(10),
                cache_write_tokens: Some(5),
            },
            TokenUsage {
                input_tokens: 200,
                output_tokens: 80,
                cache_read_tokens: Some(20),
                cache_write_tokens: Some(10),
            },
            TokenUsage {
                input_tokens: 150,
                output_tokens: 60,
                cache_read_tokens: Some(15),
                cache_write_tokens: Some(8),
            },
        ]);
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(mock));
        let executor = ActionExecutor::new(Arc::new(registry));

        let actions = vec![
            Action::prompt("first"),
            Action::prompt("second"),
            Action::prompt("third"),
        ];
        let result = executor
            .execute_all(&actions, &test_env())
            .await
            .unwrap()
            .unwrap();

        let usage = result
            .token_usage
            .expect("should have accumulated token usage");
        assert_eq!(usage.input_tokens, 450);
        assert_eq!(usage.output_tokens, 190);
        assert_eq!(usage.cache_read_tokens, Some(45));
        assert_eq!(usage.cache_write_tokens, Some(23));
    }

    #[tokio::test]
    async fn execute_all_accumulates_usage_on_failure() {
        use belt_core::runtime::TokenUsage;

        let mock = MockRuntime::new("mock", vec![0, 1]).with_token_usages(vec![
            TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
            TokenUsage {
                input_tokens: 200,
                output_tokens: 80,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
        ]);
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(mock));
        let executor = ActionExecutor::new(Arc::new(registry));

        let actions = vec![
            Action::prompt("first"),
            Action::prompt("second"),
            Action::prompt("third"),
        ];
        let result = executor
            .execute_all(&actions, &test_env())
            .await
            .unwrap()
            .unwrap();

        // Should stop at second action (failure) but still accumulate both usages.
        assert!(!result.success());
        let usage = result
            .token_usage
            .expect("should have accumulated token usage");
        assert_eq!(usage.input_tokens, 300);
        assert_eq!(usage.output_tokens, 130);
    }
}
