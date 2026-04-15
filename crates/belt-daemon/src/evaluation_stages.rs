//! Concrete evaluation stage implementations for the progressive pipeline.
//!
//! - [`MechanicalStage`] — runs deterministic shell commands in the worktree (cost $0).
//! - [`SemanticStage`] — delegates to an LLM via `belt agent -p` subprocess (cost: 1 LLM call).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use belt_core::evaluation::{EvalContext, EvalDecision, EvaluationStage};
use belt_core::platform::ShellExecutor;

use crate::executor::{ActionEnv, ActionExecutor};

// ---------------------------------------------------------------------------
// MechanicalStage
// ---------------------------------------------------------------------------

/// Stage 1: deterministic verification in the worktree.
///
/// Runs each configured shell command sequentially. If any command fails
/// (non-zero exit code), returns `Retry` so the handler re-executes without
/// incurring LLM cost. If all commands pass (or no commands are configured),
/// returns `Inconclusive` to delegate to the next stage.
pub struct MechanicalStage {
    /// Shell commands to execute (from workspace yaml `evaluate.mechanical`).
    commands: Vec<String>,
    /// Platform shell executor for running commands.
    shell: Arc<dyn ShellExecutor>,
}

impl MechanicalStage {
    /// Create a new mechanical stage with the given commands and shell executor.
    pub fn new(commands: Vec<String>, shell: Arc<dyn ShellExecutor>) -> Self {
        Self { commands, shell }
    }
}

#[async_trait]
impl EvaluationStage for MechanicalStage {
    fn name(&self) -> &str {
        "mechanical"
    }

    async fn evaluate(&self, ctx: &EvalContext) -> Result<EvalDecision> {
        // No commands configured — pass through to next stage.
        if self.commands.is_empty() {
            tracing::debug!(
                work_id = %ctx.work_id,
                "mechanical stage: no commands configured, passing through"
            );
            return Ok(EvalDecision::Inconclusive);
        }

        let worktree_path = match &ctx.worktree_path {
            Some(path) => path.clone(),
            None => {
                tracing::warn!(
                    work_id = %ctx.work_id,
                    "mechanical stage: no worktree path, skipping"
                );
                return Ok(EvalDecision::Inconclusive);
            }
        };

        let env_vars = HashMap::new();

        for cmd in &self.commands {
            tracing::info!(
                work_id = %ctx.work_id,
                command = %cmd,
                worktree = %worktree_path.display(),
                "mechanical stage: running command"
            );

            let output = self.shell.execute(cmd, &worktree_path, &env_vars).await;

            match output {
                Ok(result) if result.success() => {
                    tracing::debug!(
                        work_id = %ctx.work_id,
                        command = %cmd,
                        "mechanical stage: command passed"
                    );
                }
                Ok(result) => {
                    tracing::info!(
                        work_id = %ctx.work_id,
                        command = %cmd,
                        exit_code = ?result.exit_code,
                        stderr = %result.stderr.chars().take(500).collect::<String>(),
                        "mechanical stage: command failed, returning Retry"
                    );
                    return Ok(EvalDecision::Retry);
                }
                Err(e) => {
                    tracing::warn!(
                        work_id = %ctx.work_id,
                        command = %cmd,
                        error = %e,
                        "mechanical stage: command execution error, returning Retry"
                    );
                    return Ok(EvalDecision::Retry);
                }
            }
        }

        tracing::info!(
            work_id = %ctx.work_id,
            commands_passed = self.commands.len(),
            "mechanical stage: all commands passed, proceeding to next stage"
        );
        Ok(EvalDecision::Inconclusive)
    }
}

// ---------------------------------------------------------------------------
// SemanticStage
// ---------------------------------------------------------------------------

/// Stage 2: LLM-based semantic judgment.
///
/// Wraps the existing `belt agent -p` subprocess invocation. The LLM
/// evaluates whether the completed work is sufficient based on the issue
/// context, handler output, and history.
///
/// This stage always returns a definitive decision (Done or Hitl) — it
/// never returns `Inconclusive` since it is the final stage in v6.
pub struct SemanticStage {
    /// The workspace name for prompt construction.
    workspace_name: String,
    /// The action executor for running the LLM prompt.
    executor: Arc<ActionExecutor>,
}

impl SemanticStage {
    /// Create a new semantic stage.
    pub fn new(workspace_name: String, executor: Arc<ActionExecutor>) -> Self {
        Self {
            workspace_name,
            executor,
        }
    }

    /// Build a structured evaluate prompt for the LLM.
    ///
    /// Includes all available context: issue body, handler output,
    /// execution history, and classify-policy.md guidelines (R-015).
    fn build_prompt(&self, ctx: &EvalContext) -> String {
        let mut sections = Vec::new();

        sections.push(format!(
            "Completed 아이템의 완료 여부를 판단하고, belt queue done 또는 belt queue hitl 을 실행해줘 (workspace: {ws})",
            ws = self.workspace_name
        ));

        if let Some(ref policy) = ctx.classify_policy {
            sections.push(format!("## classify-policy.md\n{policy}"));
        }

        if let Some(ref body) = ctx.issue_body {
            sections.push(format!("## Issue Body\n{body}"));
        }

        if let Some(ref stdout) = ctx.handler_stdout {
            sections.push(format!("## Handler stdout\n{stdout}"));
        }

        if let Some(ref stderr) = ctx.handler_stderr {
            sections.push(format!("## Handler stderr\n{stderr}"));
        }

        if let Some(ref history) = ctx.execution_history {
            sections.push(format!("## Execution History\n{history}"));
        }

        sections.join("\n\n")
    }
}

#[async_trait]
impl EvaluationStage for SemanticStage {
    fn name(&self) -> &str {
        "semantic"
    }

    async fn evaluate(&self, ctx: &EvalContext) -> Result<EvalDecision> {
        let action = belt_core::action::Action::prompt(&self.build_prompt(ctx));
        let working_dir = ctx.worktree_path.as_deref().unwrap_or(Path::new("/tmp"));
        let env = ActionEnv::new(&ctx.work_id, working_dir);

        tracing::info!(
            work_id = %ctx.work_id,
            workspace = %self.workspace_name,
            "semantic stage: invoking LLM evaluation"
        );

        match self.executor.execute_one(&action, &env).await {
            Ok(result) if result.success() => {
                tracing::info!(
                    work_id = %ctx.work_id,
                    "semantic stage: LLM evaluation succeeded, returning Done"
                );
                Ok(EvalDecision::Done)
            }
            Ok(result) => {
                tracing::warn!(
                    work_id = %ctx.work_id,
                    exit_code = result.exit_code,
                    stderr = %result.stderr.chars().take(500).collect::<String>(),
                    "semantic stage: LLM evaluation failed, returning Hitl"
                );
                Ok(EvalDecision::Hitl {
                    reason: format!(
                        "semantic evaluation failed (exit_code={}): {}",
                        result.exit_code,
                        result.stderr.chars().take(200).collect::<String>()
                    ),
                })
            }
            Err(e) => {
                tracing::error!(
                    work_id = %ctx.work_id,
                    error = %e,
                    "semantic stage: LLM invocation error"
                );
                Err(e)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline builder
// ---------------------------------------------------------------------------

/// Build an [`EvaluationPipeline`] from workspace configuration.
///
/// If `mechanical_commands` is empty, only the semantic stage is registered.
/// This preserves backward compatibility — behavior is identical to the
/// previous single-batch LLM call when no mechanical config exists.
pub fn build_pipeline(
    workspace_name: &str,
    mechanical_commands: Vec<String>,
    executor: Arc<ActionExecutor>,
    shell: Arc<dyn ShellExecutor>,
) -> belt_core::evaluation::EvaluationPipeline {
    let mut stages: Vec<Box<dyn EvaluationStage>> = Vec::new();

    // Stage 1: Mechanical (only if commands are configured).
    if !mechanical_commands.is_empty() {
        stages.push(Box::new(MechanicalStage::new(mechanical_commands, shell)));
    }

    // Stage 2: Semantic (always present).
    stages.push(Box::new(SemanticStage::new(
        workspace_name.to_string(),
        executor,
    )));

    belt_core::evaluation::EvaluationPipeline::new(stages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::error::BeltError;
    use belt_core::evaluation::EvalContext;
    use belt_core::platform::ShellOutput;

    /// A mock shell executor for testing.
    struct MockShell {
        /// Exit codes to return for each successive call.
        exit_codes: std::sync::Mutex<Vec<i32>>,
    }

    impl MockShell {
        fn new(exit_codes: Vec<i32>) -> Self {
            Self {
                exit_codes: std::sync::Mutex::new(exit_codes),
            }
        }
    }

    #[async_trait]
    impl ShellExecutor for MockShell {
        async fn execute(
            &self,
            _command: &str,
            _working_dir: &Path,
            _env_vars: &HashMap<String, String>,
        ) -> Result<ShellOutput, BeltError> {
            let code = {
                let mut codes = self.exit_codes.lock().unwrap();
                if codes.is_empty() { 0 } else { codes.remove(0) }
            };
            Ok(ShellOutput {
                exit_code: Some(code),
                stdout: String::new(),
                stderr: if code != 0 {
                    "command failed".into()
                } else {
                    String::new()
                },
            })
        }
    }

    fn test_ctx_with_worktree() -> EvalContext {
        EvalContext {
            work_id: "test:implement".into(),
            source_id: "test".into(),
            workspace_name: "test-ws".into(),
            worktree_path: Some(std::path::PathBuf::from("/tmp/worktree")),
            issue_body: None,
            handler_stdout: None,
            handler_stderr: None,
            execution_history: None,
            classify_policy: None,
        }
    }

    fn test_ctx_no_worktree() -> EvalContext {
        EvalContext {
            work_id: "test:implement".into(),
            source_id: "test".into(),
            workspace_name: "test-ws".into(),
            worktree_path: None,
            issue_body: None,
            handler_stdout: None,
            handler_stderr: None,
            execution_history: None,
            classify_policy: None,
        }
    }

    // --- MechanicalStage tests ---

    #[tokio::test]
    async fn mechanical_no_commands_returns_inconclusive() {
        let shell = Arc::new(MockShell::new(vec![]));
        let stage = MechanicalStage::new(vec![], shell);

        let decision = stage.evaluate(&test_ctx_with_worktree()).await.unwrap();
        assert_eq!(decision, EvalDecision::Inconclusive);
    }

    #[tokio::test]
    async fn mechanical_all_pass_returns_inconclusive() {
        let shell = Arc::new(MockShell::new(vec![0, 0]));
        let stage = MechanicalStage::new(vec!["cargo test".into(), "cargo clippy".into()], shell);

        let decision = stage.evaluate(&test_ctx_with_worktree()).await.unwrap();
        assert_eq!(decision, EvalDecision::Inconclusive);
    }

    #[tokio::test]
    async fn mechanical_first_fails_returns_retry() {
        let shell = Arc::new(MockShell::new(vec![1]));
        let stage = MechanicalStage::new(vec!["cargo test".into(), "cargo clippy".into()], shell);

        let decision = stage.evaluate(&test_ctx_with_worktree()).await.unwrap();
        assert_eq!(decision, EvalDecision::Retry);
    }

    #[tokio::test]
    async fn mechanical_second_fails_returns_retry() {
        let shell = Arc::new(MockShell::new(vec![0, 1]));
        let stage = MechanicalStage::new(vec!["cargo test".into(), "cargo clippy".into()], shell);

        let decision = stage.evaluate(&test_ctx_with_worktree()).await.unwrap();
        assert_eq!(decision, EvalDecision::Retry);
    }

    #[tokio::test]
    async fn mechanical_no_worktree_returns_inconclusive() {
        let shell = Arc::new(MockShell::new(vec![0]));
        let stage = MechanicalStage::new(vec!["cargo test".into()], shell);

        let decision = stage.evaluate(&test_ctx_no_worktree()).await.unwrap();
        assert_eq!(decision, EvalDecision::Inconclusive);
    }

    // --- build_pipeline tests ---

    #[test]
    fn build_pipeline_without_mechanical_has_one_stage() {
        use belt_core::runtime::RuntimeRegistry;

        let registry = Arc::new(RuntimeRegistry::new("mock".into()));
        let executor = Arc::new(ActionExecutor::new(registry));
        let shell = Arc::new(MockShell::new(vec![]));

        let pipeline = build_pipeline("test-ws", vec![], executor, shell);
        assert_eq!(pipeline.stage_count(), 1);
        assert_eq!(pipeline.stage_names(), vec!["semantic"]);
    }

    #[test]
    fn build_pipeline_with_mechanical_has_two_stages() {
        use belt_core::runtime::RuntimeRegistry;

        let registry = Arc::new(RuntimeRegistry::new("mock".into()));
        let executor = Arc::new(ActionExecutor::new(registry));
        let shell = Arc::new(MockShell::new(vec![]));

        let pipeline = build_pipeline("test-ws", vec!["cargo test".into()], executor, shell);
        assert_eq!(pipeline.stage_count(), 2);
        assert_eq!(pipeline.stage_names(), vec!["mechanical", "semantic"]);
    }

    // --- SemanticStage prompt tests (R-015) ---

    #[test]
    fn semantic_build_prompt_includes_all_context_fields() {
        use belt_core::runtime::RuntimeRegistry;

        let registry = Arc::new(RuntimeRegistry::new("mock".into()));
        let executor = Arc::new(ActionExecutor::new(registry));
        let stage = SemanticStage::new("test-ws".into(), executor);

        let ctx = EvalContext {
            work_id: "test:implement".into(),
            source_id: "test".into(),
            workspace_name: "test-ws".into(),
            worktree_path: None,
            issue_body: Some("Fix the login bug".into()),
            handler_stdout: Some("All tests passed".into()),
            handler_stderr: Some("warning: unused import".into()),
            execution_history: Some("attempt 1: failed, attempt 2: success".into()),
            classify_policy: Some("Tests must pass and PR description is required".into()),
        };

        let prompt = stage.build_prompt(&ctx);

        assert!(
            prompt.contains("## Issue Body"),
            "prompt should include issue body section"
        );
        assert!(
            prompt.contains("Fix the login bug"),
            "prompt should include issue body content"
        );
        assert!(
            prompt.contains("## Handler stdout"),
            "prompt should include handler stdout section"
        );
        assert!(
            prompt.contains("All tests passed"),
            "prompt should include handler stdout content"
        );
        assert!(
            prompt.contains("## Handler stderr"),
            "prompt should include handler stderr section"
        );
        assert!(
            prompt.contains("warning: unused import"),
            "prompt should include handler stderr content"
        );
        assert!(
            prompt.contains("## Execution History"),
            "prompt should include execution history section"
        );
        assert!(
            prompt.contains("attempt 1: failed"),
            "prompt should include execution history content"
        );
        assert!(
            prompt.contains("## classify-policy.md"),
            "prompt should include classify-policy section"
        );
        assert!(
            prompt.contains("Tests must pass"),
            "prompt should include classify-policy content"
        );
    }

    #[test]
    fn semantic_build_prompt_omits_absent_fields() {
        use belt_core::runtime::RuntimeRegistry;

        let registry = Arc::new(RuntimeRegistry::new("mock".into()));
        let executor = Arc::new(ActionExecutor::new(registry));
        let stage = SemanticStage::new("test-ws".into(), executor);

        let ctx = EvalContext {
            work_id: "test:implement".into(),
            source_id: "test".into(),
            workspace_name: "test-ws".into(),
            worktree_path: None,
            issue_body: None,
            handler_stdout: None,
            handler_stderr: None,
            execution_history: None,
            classify_policy: None,
        };

        let prompt = stage.build_prompt(&ctx);

        assert!(
            !prompt.contains("## Issue Body"),
            "prompt should omit absent issue body"
        );
        assert!(
            !prompt.contains("## Handler stdout"),
            "prompt should omit absent handler stdout"
        );
        assert!(
            !prompt.contains("## Handler stderr"),
            "prompt should omit absent handler stderr"
        );
        assert!(
            !prompt.contains("## Execution History"),
            "prompt should omit absent execution history"
        );
        assert!(
            !prompt.contains("## classify-policy.md"),
            "prompt should omit absent classify-policy"
        );
        // Should still contain the base instruction.
        assert!(
            prompt.contains("test-ws"),
            "prompt should contain workspace name"
        );
    }

    #[test]
    fn semantic_build_prompt_partial_context() {
        use belt_core::runtime::RuntimeRegistry;

        let registry = Arc::new(RuntimeRegistry::new("mock".into()));
        let executor = Arc::new(ActionExecutor::new(registry));
        let stage = SemanticStage::new("test-ws".into(), executor);

        let ctx = EvalContext {
            work_id: "test:implement".into(),
            source_id: "test".into(),
            workspace_name: "test-ws".into(),
            worktree_path: None,
            issue_body: Some("Implement feature X".into()),
            handler_stdout: None,
            handler_stderr: Some("error: compilation failed".into()),
            execution_history: None,
            classify_policy: Some("All tests must pass".into()),
        };

        let prompt = stage.build_prompt(&ctx);

        assert!(prompt.contains("## Issue Body"));
        assert!(prompt.contains("Implement feature X"));
        assert!(!prompt.contains("## Handler stdout"));
        assert!(prompt.contains("## Handler stderr"));
        assert!(prompt.contains("error: compilation failed"));
        assert!(!prompt.contains("## Execution History"));
        assert!(prompt.contains("## classify-policy.md"));
        assert!(prompt.contains("All tests must pass"));
    }
}
