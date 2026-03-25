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

        let resolved_model = self.registry.resolve_model(model.clone());
        let request = RuntimeRequest {
            working_dir: env.worktree.clone(),
            prompt: text.to_string(),
            model: resolved_model.clone(),
            system_prompt: env.system_prompt.clone(),
            session_id: None,
            structured_output: None,
        };

        let response = runtime.invoke(request).await;
        Ok(ActionResult {
            exit_code: response.exit_code,
            stdout: response.stdout,
            stderr: response.stderr,
            duration: response.duration,
            token_usage: response.token_usage,
            runtime_name: Some(name.to_string()),
            model: resolved_model,
        })
    }

    async fn execute_script(&self, command: &str, env: &ActionEnv) -> Result<ActionResult> {
        let start = Instant::now();
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(command);
        cmd.current_dir(&env.worktree);
        cmd.env("WORK_ID", &env.work_id);
        cmd.env("WORKTREE", env.worktree.to_string_lossy().as_ref());
        // Inject BELT_DB path derived from BELT_HOME (or extra_vars override).
        if let Some(belt_db) = env.extra_vars.get("BELT_DB") {
            cmd.env("BELT_DB", belt_db);
        } else if let Some(belt_home) = env.extra_vars.get("BELT_HOME") {
            let db_path = Path::new(belt_home).join("belt.db");
            cmd.env("BELT_DB", db_path.to_string_lossy().as_ref());
        } else if let Ok(belt_home) = std::env::var("BELT_HOME") {
            let db_path = Path::new(&belt_home).join("belt.db");
            cmd.env("BELT_DB", db_path.to_string_lossy().as_ref());
        }
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
    /// Optional system prompt injected from workspace rules.
    ///
    /// When set, this is passed as the `system_prompt` field on
    /// [`RuntimeRequest`] for prompt actions.
    pub system_prompt: Option<String>,
}

impl ActionEnv {
    pub fn new(work_id: &str, worktree: &Path) -> Self {
        Self {
            work_id: work_id.to_string(),
            worktree: worktree.to_path_buf(),
            extra_vars: HashMap::new(),
            system_prompt: None,
        }
    }

    pub fn with_var(mut self, key: &str, value: &str) -> Self {
        self.extra_vars.insert(key.to_string(), value.to_string());
        self
    }

    /// Set the system prompt for agent runtime invocations.
    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = Some(prompt);
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

    // --- Additional tests covering gaps in the original suite ---

    #[test]
    fn action_result_success_boundary() {
        // exit_code == 0 is success
        let ok = ActionResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration: std::time::Duration::ZERO,
            token_usage: None,
            runtime_name: None,
            model: None,
        };
        assert!(ok.success());

        // Any non-zero exit code is a failure
        for code in [1, -1, 127, 255] {
            let fail = ActionResult {
                exit_code: code,
                ..ok.clone()
            };
            assert!(!fail.success(), "exit_code {code} should not be success");
        }
    }

    #[test]
    fn action_env_with_var_builder() {
        let env = ActionEnv::new("wid", Path::new("/workspace"))
            .with_var("FOO", "bar")
            .with_var("BAZ", "qux");

        assert_eq!(env.work_id, "wid");
        assert_eq!(env.worktree, PathBuf::from("/workspace"));
        assert_eq!(env.extra_vars.get("FOO").map(|s| s.as_str()), Some("bar"));
        assert_eq!(env.extra_vars.get("BAZ").map(|s| s.as_str()), Some("qux"));
    }

    #[test]
    fn action_env_with_system_prompt_builder() {
        let env = ActionEnv::new("wid", Path::new("/workspace"))
            .with_system_prompt("You are a code assistant.".to_string());

        assert_eq!(
            env.system_prompt.as_deref(),
            Some("You are a code assistant.")
        );
    }

    #[test]
    fn action_env_system_prompt_defaults_to_none() {
        let env = ActionEnv::new("wid", Path::new("/workspace"));
        assert!(env.system_prompt.is_none());
    }

    #[tokio::test]
    async fn execute_script_injects_worktree_env() {
        let executor = ActionExecutor::new(setup_registry());
        // The executor must inject WORKTREE as well as WORK_ID.
        let action = Action::script("echo $WORKTREE");
        let env = ActionEnv::new("wid", Path::new("/tmp"));
        let result = executor.execute_one(&action, &env).await.unwrap();
        assert!(result.success());
        assert!(
            result.stdout.contains("/tmp"),
            "WORKTREE should be injected; got: {}",
            result.stdout
        );
    }

    #[tokio::test]
    async fn execute_script_injects_extra_vars() {
        let executor = ActionExecutor::new(setup_registry());
        let action = Action::script("echo $MY_VAR");
        let env = ActionEnv::new("wid", Path::new("/tmp")).with_var("MY_VAR", "hello-extra");
        let result = executor.execute_one(&action, &env).await.unwrap();
        assert!(result.success());
        assert!(
            result.stdout.contains("hello-extra"),
            "extra var should be injected; got: {}",
            result.stdout
        );
    }

    #[tokio::test]
    async fn execute_prompt_sets_runtime_name_field() {
        let executor = ActionExecutor::new(setup_registry());
        let action = Action::prompt("check runtime name");
        let result = executor.execute_one(&action, &test_env()).await.unwrap();
        assert_eq!(
            result.runtime_name.as_deref(),
            Some("mock"),
            "runtime_name should reflect the resolved runtime"
        );
    }

    #[tokio::test]
    async fn execute_prompt_with_runtime_sets_runtime_name_and_model() {
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(MockRuntime::new("named-rt", vec![0])));
        let executor = ActionExecutor::new(Arc::new(registry));

        let action = Action::prompt_with_runtime("do work", "named-rt", Some("turbo-model"));
        let result = executor.execute_one(&action, &test_env()).await.unwrap();

        assert!(result.success());
        assert_eq!(result.runtime_name.as_deref(), Some("named-rt"));
        assert_eq!(result.model.as_deref(), Some("turbo-model"));
    }

    #[tokio::test]
    async fn execute_prompt_unknown_runtime_returns_error() {
        // A registry with no runtimes registered causes an error when the
        // prompt action tries to resolve its runtime.
        let registry = RuntimeRegistry::new("nonexistent".to_string());
        let executor = ActionExecutor::new(Arc::new(registry));
        let action = Action::prompt("this will fail");
        let err = executor.execute_one(&action, &test_env()).await;
        assert!(err.is_err(), "should error when runtime not found");
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("runtime not found"),
            "error message should mention runtime not found; got: {msg}"
        );
    }

    #[tokio::test]
    async fn execute_script_has_no_token_usage() {
        let executor = ActionExecutor::new(setup_registry());
        let action = Action::script("echo ok");
        let result = executor.execute_one(&action, &test_env()).await.unwrap();
        assert!(result.success());
        assert!(
            result.token_usage.is_none(),
            "script actions should never report token usage"
        );
        assert!(
            result.runtime_name.is_none(),
            "script actions should not set runtime_name"
        );
    }

    #[tokio::test]
    async fn execute_all_single_success_returns_some() {
        let executor = ActionExecutor::new(setup_registry());
        let actions = vec![Action::prompt("only action")];
        let result = executor.execute_all(&actions, &test_env()).await.unwrap();
        assert!(
            result.is_some(),
            "single successful action should return Some"
        );
        assert!(result.unwrap().success());
    }

    #[tokio::test]
    async fn execute_all_first_action_fails_stops_immediately() {
        // MockRuntime with [1, 0]: first invocation returns exit_code 1, second would return 0.
        // Verifies execute_all returns immediately after the first failure.
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(MockRuntime::new("mock", vec![1, 0])));
        let executor = ActionExecutor::new(Arc::new(registry));

        let actions = vec![
            Action::prompt("should fail"),
            Action::prompt("should not run"),
        ];
        let result = executor
            .execute_all(&actions, &test_env())
            .await
            .unwrap()
            .unwrap();

        // The result should be a failure from the first action.
        assert!(!result.success());
        assert_eq!(result.exit_code, 1);
    }

    #[tokio::test]
    async fn execute_all_scripts_produce_no_token_usage() {
        let executor = ActionExecutor::new(setup_registry());
        // All script actions — accumulated token usage should be None.
        let actions = vec![Action::script("echo one"), Action::script("echo two")];
        let result = executor
            .execute_all(&actions, &test_env())
            .await
            .unwrap()
            .unwrap();

        assert!(result.success());
        assert!(
            result.token_usage.is_none(),
            "all-script execute_all should report no token usage"
        );
    }

    #[tokio::test]
    async fn execute_all_mixed_prompt_script_accumulates_only_prompt_usage() {
        use belt_core::runtime::TokenUsage;

        let mock = MockRuntime::new("mock", vec![0, 0]).with_token_usages(vec![TokenUsage {
            input_tokens: 50,
            output_tokens: 25,
            cache_read_tokens: None,
            cache_write_tokens: None,
        }]);
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(mock));
        let executor = ActionExecutor::new(Arc::new(registry));

        // First: prompt (has token usage), second: script (no token usage).
        let actions = vec![Action::prompt("summarize"), Action::script("echo done")];
        let result = executor
            .execute_all(&actions, &test_env())
            .await
            .unwrap()
            .unwrap();

        assert!(result.success());
        let usage = result
            .token_usage
            .expect("should carry prompt token usage through mixed chain");
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.output_tokens, 25);
    }

    #[tokio::test]
    async fn execute_all_cache_tokens_accumulate_when_only_some_have_them() {
        use belt_core::runtime::TokenUsage;

        // First invocation has cache tokens; second does not.
        let mock = MockRuntime::new("mock", vec![0, 0]).with_token_usages(vec![
            TokenUsage {
                input_tokens: 100,
                output_tokens: 40,
                cache_read_tokens: Some(8),
                cache_write_tokens: Some(4),
            },
            TokenUsage {
                input_tokens: 60,
                output_tokens: 20,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
        ]);
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(mock));
        let executor = ActionExecutor::new(Arc::new(registry));

        let actions = vec![Action::prompt("first"), Action::prompt("second")];
        let result = executor
            .execute_all(&actions, &test_env())
            .await
            .unwrap()
            .unwrap();

        let usage = result.token_usage.expect("should have usage");
        assert_eq!(usage.input_tokens, 160);
        assert_eq!(usage.output_tokens, 60);
        // Cache fields should retain whatever was accumulated from the first prompt.
        assert_eq!(usage.cache_read_tokens, Some(8));
        assert_eq!(usage.cache_write_tokens, Some(4));
    }
}
