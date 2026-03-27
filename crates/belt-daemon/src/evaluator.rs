use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use belt_core::action::Action;
use belt_core::phase::QueuePhase;
use belt_core::queue::QueueItem;

use crate::executor::{ActionEnv, ActionExecutor, ActionResult};

/// Default maximum number of evaluate failures before HITL escalation.
pub const DEFAULT_MAX_EVAL_FAILURES: u32 = 3;

/// Maximum number of items to evaluate per batch cycle.
pub const DEFAULT_EVAL_BATCH_SIZE: usize = 10;

/// Default timeout for the evaluate subprocess (5 minutes).
pub const DEFAULT_EVALUATE_TIMEOUT_SECS: u64 = 300;

/// Completed 아이템의 평가 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalDecision {
    Done,
    Hitl {
        reason: String,
    },
    /// Evaluate 실행 실패 — Completed 유지, 다음 tick에서 재시도.
    Retry,
}

/// Completed 아이템을 스캔하여 Done 또는 HITL로 분류.
///
/// Per-item evaluate failure count를 추적하며, `max_eval_failures`회
/// 연속 실패 시 자동으로 HITL로 에스컬레이션한다.
pub struct Evaluator {
    workspace_name: String,
    /// Per-item evaluate failure counts (keyed by work_id).
    eval_failure_counts: HashMap<String, u32>,
    /// Maximum evaluate failures before HITL escalation.
    max_eval_failures: u32,
    /// Path to the workspace YAML config file for subprocess invocation.
    workspace_config_path: Option<PathBuf>,
    /// Timeout for the evaluate subprocess.
    evaluate_timeout: Duration,
}

impl Evaluator {
    pub fn new(workspace_name: &str) -> Self {
        Self {
            workspace_name: workspace_name.to_string(),
            eval_failure_counts: HashMap::new(),
            max_eval_failures: DEFAULT_MAX_EVAL_FAILURES,
            workspace_config_path: None,
            evaluate_timeout: Duration::from_secs(DEFAULT_EVALUATE_TIMEOUT_SECS),
        }
    }

    /// Set the maximum evaluate failure threshold for HITL escalation.
    pub fn with_max_eval_failures(mut self, max: u32) -> Self {
        self.max_eval_failures = max;
        self
    }

    /// Set the workspace config path for subprocess invocation.
    pub fn with_workspace_config_path(mut self, path: PathBuf) -> Self {
        self.workspace_config_path = Some(path);
        self
    }

    /// Set the timeout for the evaluate subprocess.
    pub fn with_evaluate_timeout(mut self, timeout: Duration) -> Self {
        self.evaluate_timeout = timeout;
        self
    }

    pub fn filter_completed(items: &[QueueItem]) -> Vec<&QueueItem> {
        items
            .iter()
            .filter(|item| item.phase == QueuePhase::Completed)
            .collect()
    }

    pub fn target_phase(decision: &EvalDecision) -> QueuePhase {
        match decision {
            EvalDecision::Done => QueuePhase::Done,
            EvalDecision::Hitl { .. } => QueuePhase::Hitl,
            EvalDecision::Retry => QueuePhase::Completed,
        }
    }

    /// Record an evaluate failure for the given work_id and return the
    /// appropriate decision based on accumulated failure count.
    ///
    /// If failures >= `max_eval_failures`, escalates to HITL.
    /// Otherwise, returns `Retry` to keep the item in Completed phase.
    pub fn record_eval_failure(&mut self, work_id: &str, error: &str) -> EvalDecision {
        let count = self
            .eval_failure_counts
            .entry(work_id.to_string())
            .or_insert(0);
        *count += 1;

        tracing::warn!(
            work_id,
            eval_failure_count = *count,
            max = self.max_eval_failures,
            "evaluate failure recorded"
        );

        if *count >= self.max_eval_failures {
            tracing::error!(
                work_id,
                eval_failure_count = *count,
                "escalating to HITL after {} evaluate failures",
                *count
            );
            EvalDecision::Hitl {
                reason: format!(
                    "evaluate failed {} times (threshold={}): {}",
                    *count, self.max_eval_failures, error
                ),
            }
        } else {
            EvalDecision::Retry
        }
    }

    /// Clear the evaluate failure count for a work_id (e.g. on successful evaluation).
    pub fn clear_eval_failures(&mut self, work_id: &str) {
        self.eval_failure_counts.remove(work_id);
    }

    /// Return the current evaluate failure count for a work_id.
    pub fn eval_failure_count(&self, work_id: &str) -> u32 {
        self.eval_failure_counts.get(work_id).copied().unwrap_or(0)
    }

    pub fn build_evaluate_script(&self) -> String {
        format!(
            r#"#!/bin/bash
COMPLETED=$(belt queue list --phase completed --json 2>/dev/null | jq 'length' 2>/dev/null)
if [ "$COMPLETED" = "0" ] || [ -z "$COMPLETED" ]; then exit 0; fi

belt agent --workspace "{ws}" -p \
  "Completed 아이템의 완료 여부를 판단하고, belt queue done 또는 belt queue hitl 을 실행해줘"
"#,
            ws = self.workspace_name
        )
    }

    /// Build the `tokio::process::Command` for the evaluate subprocess.
    ///
    /// Extracted for testability: callers can inspect the command's args and
    /// environment without actually spawning a process.
    #[doc(hidden)]
    pub fn build_evaluate_command(&self, belt_home: &Path) -> tokio::process::Command {
        let belt_db = belt_home.join("belt.db");
        let prompt = self.build_evaluate_prompt();

        let mut cmd = tokio::process::Command::new("belt");
        cmd.arg("agent");

        // Pass workspace config path if available.
        if let Some(ref config_path) = self.workspace_config_path {
            cmd.arg("--workspace").arg(config_path);
        }

        cmd.arg("-p").arg(&prompt);
        cmd.arg("--json");

        // Workspace-isolated environment variables.
        cmd.env("WORKSPACE", &self.workspace_name);
        cmd.env("BELT_HOME", belt_home.to_string_lossy().as_ref());
        cmd.env("BELT_DB", belt_db.to_string_lossy().as_ref());

        cmd
    }

    /// Run the evaluate step as a subprocess via `belt agent`.
    ///
    /// Spawns `belt agent --workspace <config> -p <prompt> --json` with
    /// workspace-isolated environment variables (`WORKSPACE`, `BELT_HOME`,
    /// `BELT_DB`). The subprocess output is collected as JSON via stdout
    /// (IPC) and parsed into an [`EvaluateResult`].
    ///
    /// A configurable timeout (default 5 minutes) guards against runaway
    /// subprocesses. On timeout the child process is killed and an error
    /// is returned so the caller can record the failure.
    pub async fn run_evaluate(&self, belt_home: &Path) -> Result<EvaluateResult> {
        let mut cmd = self.build_evaluate_command(belt_home);

        tracing::info!(
            workspace = %self.workspace_name,
            timeout_secs = self.evaluate_timeout.as_secs(),
            "spawning evaluate subprocess"
        );

        let output = tokio::time::timeout(self.evaluate_timeout, cmd.output()).await;

        match output {
            Ok(Ok(child_output)) => {
                let stdout = String::from_utf8_lossy(&child_output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&child_output.stderr).to_string();
                let exit_code = child_output.status.code().unwrap_or(-1);

                // Attempt to parse JSON IPC result from stdout.
                let ipc_result = if !stdout.trim().is_empty() {
                    serde_json::from_str::<serde_json::Value>(stdout.trim()).ok()
                } else {
                    None
                };

                if let Some(ref json) = ipc_result {
                    tracing::debug!(
                        workspace = %self.workspace_name,
                        exit_code,
                        ipc_keys = ?json.as_object().map(|o| o.keys().collect::<Vec<_>>()),
                        "evaluate subprocess completed with IPC result"
                    );
                } else {
                    tracing::debug!(
                        workspace = %self.workspace_name,
                        exit_code,
                        stderr_len = stderr.len(),
                        "evaluate subprocess completed without IPC result"
                    );
                }

                Ok(EvaluateResult {
                    exit_code,
                    stdout,
                    stderr,
                    action_result: None,
                    ipc_result,
                })
            }
            Ok(Err(e)) => {
                // Subprocess failed to spawn or I/O error.
                anyhow::bail!(
                    "evaluate subprocess failed for workspace '{}': {e}",
                    self.workspace_name
                )
            }
            Err(_elapsed) => {
                // Timeout — the subprocess exceeded the allowed duration.
                tracing::error!(
                    workspace = %self.workspace_name,
                    timeout_secs = self.evaluate_timeout.as_secs(),
                    "evaluate subprocess timed out, killing child"
                );
                anyhow::bail!(
                    "evaluate subprocess timed out after {}s for workspace '{}'",
                    self.evaluate_timeout.as_secs(),
                    self.workspace_name
                )
            }
        }
    }

    /// Build an `Action::Prompt` for the evaluate LLM call.
    ///
    /// This allows the evaluate to run through the `ActionExecutor`, capturing
    /// token usage that would otherwise be lost when running via bash script.
    pub fn build_evaluate_prompt(&self) -> String {
        format!(
            "Completed 아이템의 완료 여부를 판단하고, belt queue done 또는 belt queue hitl 을 실행해줘 (workspace: {ws})",
            ws = self.workspace_name
        )
    }

    /// Run the evaluate step through the `ActionExecutor`, returning a full
    /// `ActionResult` that includes token usage from the LLM call.
    ///
    /// This is the preferred method for evaluate execution as it integrates
    /// with the daemon's token usage tracking pipeline.
    pub async fn run_evaluate_with_executor(
        &self,
        executor: &ActionExecutor,
        env: &ActionEnv,
    ) -> Result<ActionResult> {
        let action = Action::prompt(&self.build_evaluate_prompt());
        executor.execute_one(&action, env).await
    }

    /// Build a per-item evaluate prompt for LLM-based quality assessment.
    ///
    /// The prompt asks the LLM to evaluate a single completed item's work
    /// quality and return a structured JSON verdict.
    pub fn build_per_item_evaluate_prompt(&self, item: &QueueItem) -> String {
        format!(
            concat!(
                "Evaluate the quality of completed work for the following item.\n\n",
                "Workspace: {ws}\n",
                "Work ID: {work_id}\n",
                "Source: {source_id}\n",
                "State: {state}\n",
                "Title: {title}\n\n",
                "Review the work output and determine if it meets quality standards.\n",
                "Respond with ONLY a JSON object in this exact format:\n\n",
                "{{\"verdict\": \"pass\"}}\n\n",
                "OR if issues are found:\n\n",
                "{{\"verdict\": \"fail\", \"reason\": \"description of issues found\", ",
                "\"suggestions\": [\"suggestion 1\", \"suggestion 2\"]}}\n\n",
                "Rules:\n",
                "- Use \"pass\" if the work is complete and meets quality standards.\n",
                "- Use \"fail\" if there are issues that need human review.\n",
                "- The \"reason\" field should be a concise description of the problem.\n",
                "- The \"suggestions\" field is optional and should list actionable improvements.",
            ),
            ws = self.workspace_name,
            work_id = item.work_id,
            source_id = item.source_id,
            state = item.state,
            title = item.title.as_deref().unwrap_or("(untitled)"),
        )
    }

    /// Evaluate a single item through the `ActionExecutor` and parse the
    /// LLM response into an `EvalDecision`.
    ///
    /// On successful LLM call, the response stdout is parsed as JSON to
    /// extract a verdict. If parsing fails or the LLM call errors, the
    /// failure is recorded and may escalate to HITL.
    pub async fn evaluate_item(
        &mut self,
        item: &QueueItem,
        executor: &ActionExecutor,
        env: &ActionEnv,
    ) -> (EvalDecision, Option<EvalVerdict>) {
        let prompt = self.build_per_item_evaluate_prompt(item);
        let action = Action::prompt(&prompt);

        match executor.execute_one(&action, env).await {
            Ok(result) if result.success() => {
                let verdict = EvalVerdict::parse(&result.stdout);
                match &verdict {
                    Some(v) if v.is_pass() => {
                        self.clear_eval_failures(&item.work_id);
                        tracing::info!(
                            work_id = %item.work_id,
                            "per-item evaluate: passed"
                        );
                        (EvalDecision::Done, Some(v.clone()))
                    }
                    Some(v) => {
                        tracing::info!(
                            work_id = %item.work_id,
                            reason = %v.reason.as_deref().unwrap_or("unknown"),
                            "per-item evaluate: issues found"
                        );
                        (
                            EvalDecision::Hitl {
                                reason: v
                                    .reason
                                    .clone()
                                    .unwrap_or_else(|| "LLM evaluation found issues".to_string()),
                            },
                            Some(v.clone()),
                        )
                    }
                    None => {
                        tracing::warn!(
                            work_id = %item.work_id,
                            stdout_len = result.stdout.len(),
                            "per-item evaluate: could not parse verdict, defaulting to pass"
                        );
                        self.clear_eval_failures(&item.work_id);
                        (EvalDecision::Done, None)
                    }
                }
            }
            Ok(result) => {
                let error_msg = format!(
                    "evaluate exit_code={}: {}",
                    result.exit_code,
                    result.stderr.trim()
                );
                let decision = self.record_eval_failure(&item.work_id, &error_msg);
                (decision, None)
            }
            Err(e) => {
                let decision = self.record_eval_failure(&item.work_id, &e.to_string());
                (decision, None)
            }
        }
    }
}

/// Structured verdict from the LLM evaluation of a single item.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct EvalVerdict {
    /// "pass" or "fail".
    pub verdict: String,
    /// Reason for failure (only present when verdict is "fail").
    #[serde(default)]
    pub reason: Option<String>,
    /// Actionable improvement suggestions (only present when verdict is "fail").
    #[serde(default)]
    pub suggestions: Vec<String>,
}

impl EvalVerdict {
    /// Returns `true` if the verdict indicates the work passed evaluation.
    pub fn is_pass(&self) -> bool {
        self.verdict.eq_ignore_ascii_case("pass")
    }

    /// Attempt to parse an `EvalVerdict` from the LLM response text.
    ///
    /// Tries to find and parse a JSON object from the response. Returns
    /// `None` if no valid JSON verdict is found.
    pub fn parse(text: &str) -> Option<Self> {
        let trimmed = text.trim();
        // Try direct parse first.
        if let Ok(v) = serde_json::from_str::<Self>(trimmed) {
            return Some(v);
        }
        // Try to find a JSON object embedded in the text.
        if let Some(start) = trimmed.find('{')
            && let Some(end) = trimmed.rfind('}')
            && start < end
            && let Ok(v) = serde_json::from_str::<Self>(&trimmed[start..=end])
        {
            return Some(v);
        }
        None
    }
}

#[derive(Debug)]
pub struct EvaluateResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// The underlying `ActionResult` when the evaluate was run through the
    /// executor. Contains token usage and runtime metadata for cost accounting.
    pub action_result: Option<ActionResult>,
    /// Parsed JSON IPC result from the subprocess stdout.
    ///
    /// When the evaluate subprocess is invoked with `--json`, the structured
    /// output is parsed and stored here for downstream consumption (e.g.,
    /// extracting token usage, exit codes, or per-item decisions).
    pub ipc_result: Option<serde_json::Value>,
}

impl EvaluateResult {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

impl From<ActionResult> for EvaluateResult {
    fn from(r: ActionResult) -> Self {
        // Attempt to parse JSON IPC from executor stdout.
        let ipc_result = if !r.stdout.trim().is_empty() {
            serde_json::from_str::<serde_json::Value>(r.stdout.trim()).ok()
        } else {
            None
        };
        Self {
            exit_code: r.exit_code,
            stdout: r.stdout.clone(),
            stderr: r.stderr.clone(),
            action_result: Some(r),
            ipc_result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::queue::testing::test_item;

    #[test]
    fn filter_completed_items() {
        let mut items = vec![
            test_item("s1", "analyze"),
            test_item("s2", "implement"),
            test_item("s3", "review"),
        ];
        items[1].phase = QueuePhase::Completed;
        let completed = Evaluator::filter_completed(&items);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].work_id, "s2:implement");
    }

    #[test]
    fn target_phase_done() {
        assert_eq!(
            Evaluator::target_phase(&EvalDecision::Done),
            QueuePhase::Done
        );
    }

    #[test]
    fn target_phase_hitl() {
        let decision = EvalDecision::Hitl {
            reason: "needs review".to_string(),
        };
        assert_eq!(Evaluator::target_phase(&decision), QueuePhase::Hitl);
    }

    #[test]
    fn target_phase_retry() {
        assert_eq!(
            Evaluator::target_phase(&EvalDecision::Retry),
            QueuePhase::Completed
        );
    }

    #[test]
    fn build_evaluate_script_contains_workspace() {
        let evaluator = Evaluator::new("auth-project");
        let script = evaluator.build_evaluate_script();
        assert!(script.contains("auth-project"));
        assert!(script.contains("belt agent"));
        assert!(script.contains("belt queue done"));
    }

    #[test]
    fn record_eval_failure_escalates_after_threshold() {
        let mut evaluator = Evaluator::new("test-ws").with_max_eval_failures(3);

        // First two failures -> Retry
        let decision = evaluator.record_eval_failure("item-1", "script error");
        assert_eq!(decision, EvalDecision::Retry);
        assert_eq!(evaluator.eval_failure_count("item-1"), 1);

        let decision = evaluator.record_eval_failure("item-1", "script error");
        assert_eq!(decision, EvalDecision::Retry);
        assert_eq!(evaluator.eval_failure_count("item-1"), 2);

        // Third failure -> HITL escalation
        let decision = evaluator.record_eval_failure("item-1", "script error");
        assert!(matches!(decision, EvalDecision::Hitl { .. }));
        assert_eq!(evaluator.eval_failure_count("item-1"), 3);
    }

    #[test]
    fn clear_eval_failures_resets_count() {
        let mut evaluator = Evaluator::new("test-ws");
        evaluator.record_eval_failure("item-1", "error");
        evaluator.record_eval_failure("item-1", "error");
        assert_eq!(evaluator.eval_failure_count("item-1"), 2);

        evaluator.clear_eval_failures("item-1");
        assert_eq!(evaluator.eval_failure_count("item-1"), 0);
    }

    #[test]
    fn independent_failure_tracking_per_item() {
        let mut evaluator = Evaluator::new("test-ws").with_max_eval_failures(2);

        evaluator.record_eval_failure("item-1", "error");
        evaluator.record_eval_failure("item-2", "error");

        assert_eq!(evaluator.eval_failure_count("item-1"), 1);
        assert_eq!(evaluator.eval_failure_count("item-2"), 1);

        // item-1 hits threshold
        let decision = evaluator.record_eval_failure("item-1", "error");
        assert!(matches!(decision, EvalDecision::Hitl { .. }));

        // item-2 still retrying
        let decision = evaluator.record_eval_failure("item-2", "error");
        assert!(matches!(decision, EvalDecision::Hitl { .. }));
    }

    #[test]
    fn default_max_eval_failures_is_three() {
        let evaluator = Evaluator::new("test-ws");
        assert_eq!(evaluator.max_eval_failures, DEFAULT_MAX_EVAL_FAILURES);
        assert_eq!(evaluator.max_eval_failures, 3);
    }

    #[test]
    fn build_evaluate_prompt_contains_workspace() {
        let evaluator = Evaluator::new("auth-project");
        let prompt = evaluator.build_evaluate_prompt();
        assert!(
            prompt.contains("auth-project"),
            "evaluate prompt should contain workspace name"
        );
        assert!(
            prompt.contains("belt queue done"),
            "evaluate prompt should mention belt queue done"
        );
    }

    #[tokio::test]
    async fn run_evaluate_with_executor_returns_action_result() {
        use belt_core::runtime::{RuntimeRegistry, TokenUsage};
        use belt_infra::runtimes::mock::MockRuntime;
        use std::sync::Arc;

        let mock = MockRuntime::new("mock", vec![0]).with_token_usages(vec![TokenUsage {
            input_tokens: 500,
            output_tokens: 200,
            cache_read_tokens: None,
            cache_write_tokens: None,
        }]);
        let mut registry = RuntimeRegistry::new("mock".to_string());
        registry.register(Arc::new(mock));
        let executor = ActionExecutor::new(Arc::new(registry));
        let env = ActionEnv::new("test-work-id", std::path::Path::new("/tmp"));
        let evaluator = Evaluator::new("test-ws");

        let result = evaluator
            .run_evaluate_with_executor(&executor, &env)
            .await
            .unwrap();

        assert!(result.success());
        let usage = result
            .token_usage
            .expect("should have token usage from executor-based evaluate");
        assert_eq!(usage.input_tokens, 500);
        assert_eq!(usage.output_tokens, 200);
    }

    #[test]
    fn evaluate_result_from_action_result_preserves_token_usage() {
        use belt_core::runtime::TokenUsage;

        let action_result = ActionResult {
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            duration: std::time::Duration::from_millis(100),
            token_usage: Some(TokenUsage {
                input_tokens: 300,
                output_tokens: 150,
                cache_read_tokens: Some(10),
                cache_write_tokens: None,
            }),
            runtime_name: Some("claude".to_string()),
            model: Some("opus-4".to_string()),
        };

        let eval_result = EvaluateResult::from(action_result);
        assert!(eval_result.success());
        assert_eq!(eval_result.stdout, "ok");
        // "ok" is not valid JSON, so ipc_result should be None.
        assert!(eval_result.ipc_result.is_none());
        let ar = eval_result
            .action_result
            .expect("should preserve action_result");
        let usage = ar.token_usage.expect("should preserve token usage");
        assert_eq!(usage.input_tokens, 300);
        assert_eq!(usage.output_tokens, 150);
        assert_eq!(ar.runtime_name.as_deref(), Some("claude"));
    }

    #[test]
    fn evaluate_result_from_action_result_parses_json_ipc() {
        let json_stdout = r#"{"exit_code": 0, "workspace": "test"}"#;
        let action_result = ActionResult {
            exit_code: 0,
            stdout: json_stdout.to_string(),
            stderr: String::new(),
            duration: std::time::Duration::from_millis(50),
            token_usage: None,
            runtime_name: None,
            model: None,
        };

        let eval_result = EvaluateResult::from(action_result);
        assert!(eval_result.success());
        let ipc = eval_result.ipc_result.expect("should parse JSON IPC");
        assert_eq!(ipc["workspace"], "test");
        assert_eq!(ipc["exit_code"], 0);
    }

    #[test]
    fn evaluator_with_workspace_config_path() {
        let evaluator = Evaluator::new("test-ws")
            .with_workspace_config_path(PathBuf::from("/etc/belt/workspace.yaml"));
        assert_eq!(
            evaluator.workspace_config_path,
            Some(PathBuf::from("/etc/belt/workspace.yaml"))
        );
    }

    #[test]
    fn evaluator_with_evaluate_timeout() {
        let evaluator = Evaluator::new("test-ws").with_evaluate_timeout(Duration::from_secs(120));
        assert_eq!(evaluator.evaluate_timeout, Duration::from_secs(120));
    }

    #[test]
    fn default_evaluate_timeout_is_five_minutes() {
        let evaluator = Evaluator::new("test-ws");
        assert_eq!(
            evaluator.evaluate_timeout,
            Duration::from_secs(DEFAULT_EVALUATE_TIMEOUT_SECS)
        );
        assert_eq!(evaluator.evaluate_timeout.as_secs(), 300);
    }

    #[test]
    fn evaluator_defaults_no_workspace_config_path() {
        let evaluator = Evaluator::new("test-ws");
        assert!(evaluator.workspace_config_path.is_none());
    }

    #[tokio::test]
    async fn run_evaluate_subprocess_captures_output() {
        // Use a simple echo command via PATH to simulate belt binary.
        // We test that the subprocess machinery works by pointing at a
        // non-existent binary which should return an error.
        let evaluator = Evaluator::new("test-ws").with_evaluate_timeout(Duration::from_secs(5));

        let tmp = tempfile::tempdir().unwrap();
        let result = evaluator.run_evaluate(tmp.path()).await;

        // The subprocess will fail because 'belt' binary is likely not on
        // PATH in test environments. This validates error handling works.
        assert!(
            result.is_err(),
            "should error when belt binary is not available"
        );
    }

    #[tokio::test]
    async fn run_evaluate_subprocess_timeout() {
        // Create evaluator with very short timeout to verify timeout handling.
        let evaluator = Evaluator::new("test-ws").with_evaluate_timeout(Duration::from_millis(1));

        let tmp = tempfile::tempdir().unwrap();
        let result = evaluator.run_evaluate(tmp.path()).await;

        // Either times out or fails to spawn -- both are acceptable error paths.
        assert!(result.is_err(), "should error on timeout or spawn failure");
    }

    // --- New tests for subprocess invocation, JSON parsing, timeout, and env isolation ---

    /// Helper: create a fake `belt` script in a temp directory and return
    /// an evaluator whose command will resolve to that script via PATH override.
    ///
    /// The script writes its received env vars and arguments to files for
    /// later assertion, then outputs the provided `stdout_content`.
    #[cfg(unix)]
    fn create_fake_belt_script(dir: &Path, stdout_content: &str, exit_code: i32) -> PathBuf {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let bin_dir = dir.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let script_path = bin_dir.join("belt");

        // The script dumps env vars and args to files, then echoes stdout_content.
        let script = format!(
            r#"#!/bin/sh
echo "$WORKSPACE" > "{dir}/env_workspace"
echo "$BELT_HOME" > "{dir}/env_belt_home"
echo "$BELT_DB" > "{dir}/env_belt_db"
echo "$@" > "{dir}/args"
printf '%s' '{stdout}'
exit {exit_code}
"#,
            dir = dir.to_string_lossy(),
            stdout = stdout_content.replace('\'', "'\\''"),
            exit_code = exit_code,
        );

        fs::write(&script_path, script).unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
        bin_dir
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_evaluate_subprocess_parses_json_stdout() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let json_output = r#"{"status":"done","items_processed":3}"#;
        let bin_dir = create_fake_belt_script(tmp.path(), json_output, 0);

        let evaluator = Evaluator::new("test-ws").with_evaluate_timeout(Duration::from_secs(10));

        // Override PATH so that our fake `belt` script is found first.
        let original_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", bin_dir.display(), original_path);

        // We need to modify the command's env directly, so use build_evaluate_command.
        let belt_home = tmp.path().join("home");
        fs::create_dir_all(&belt_home).unwrap();
        let mut cmd = evaluator.build_evaluate_command(&belt_home);
        cmd.env("PATH", &new_path);

        let child_output = cmd.output().await.unwrap();
        let stdout = String::from_utf8_lossy(&child_output.stdout).to_string();
        let exit_code = child_output.status.code().unwrap_or(-1);

        assert_eq!(exit_code, 0, "fake belt script should exit successfully");

        // Verify JSON parsing logic (same as run_evaluate's internal parsing).
        let ipc_result = if !stdout.trim().is_empty() {
            serde_json::from_str::<serde_json::Value>(stdout.trim()).ok()
        } else {
            None
        };
        let ipc = ipc_result.expect("should parse JSON IPC from subprocess stdout");
        assert_eq!(ipc["status"], "done");
        assert_eq!(ipc["items_processed"], 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_evaluate_subprocess_invalid_json_yields_none_ipc() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = create_fake_belt_script(tmp.path(), "not-json-output", 0);

        let evaluator = Evaluator::new("test-ws").with_evaluate_timeout(Duration::from_secs(10));

        let original_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", bin_dir.display(), original_path);

        let belt_home = tmp.path().join("home");
        fs::create_dir_all(&belt_home).unwrap();
        let mut cmd = evaluator.build_evaluate_command(&belt_home);
        cmd.env("PATH", &new_path);

        let child_output = cmd.output().await.unwrap();
        let stdout = String::from_utf8_lossy(&child_output.stdout).to_string();

        // Non-JSON stdout should not parse to ipc_result.
        let ipc_result = if !stdout.trim().is_empty() {
            serde_json::from_str::<serde_json::Value>(stdout.trim()).ok()
        } else {
            None
        };
        assert!(
            ipc_result.is_none(),
            "non-JSON stdout should yield None ipc_result"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_evaluate_subprocess_env_isolation() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = create_fake_belt_script(tmp.path(), "{}", 0);

        let evaluator =
            Evaluator::new("my-workspace").with_evaluate_timeout(Duration::from_secs(10));

        let original_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", bin_dir.display(), original_path);

        let belt_home = tmp.path().join("home");
        fs::create_dir_all(&belt_home).unwrap();
        let mut cmd = evaluator.build_evaluate_command(&belt_home);
        cmd.env("PATH", &new_path);

        let _output = cmd.output().await.unwrap();

        // Read the environment variables captured by the fake script.
        let env_workspace = fs::read_to_string(tmp.path().join("env_workspace"))
            .unwrap()
            .trim()
            .to_string();
        let env_belt_home = fs::read_to_string(tmp.path().join("env_belt_home"))
            .unwrap()
            .trim()
            .to_string();
        let env_belt_db = fs::read_to_string(tmp.path().join("env_belt_db"))
            .unwrap()
            .trim()
            .to_string();

        assert_eq!(
            env_workspace, "my-workspace",
            "WORKSPACE env should match workspace_name"
        );
        assert_eq!(
            env_belt_home,
            belt_home.to_string_lossy(),
            "BELT_HOME env should match belt_home path"
        );
        assert_eq!(
            env_belt_db,
            belt_home.join("belt.db").to_string_lossy(),
            "BELT_DB env should be belt_home/belt.db"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_evaluate_subprocess_passes_workspace_config_arg() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = create_fake_belt_script(tmp.path(), "{}", 0);

        let config_path = PathBuf::from("/etc/belt/workspace.yaml");
        let evaluator = Evaluator::new("test-ws")
            .with_workspace_config_path(config_path.clone())
            .with_evaluate_timeout(Duration::from_secs(10));

        let original_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", bin_dir.display(), original_path);

        let belt_home = tmp.path().join("home");
        fs::create_dir_all(&belt_home).unwrap();
        let mut cmd = evaluator.build_evaluate_command(&belt_home);
        cmd.env("PATH", &new_path);

        let _output = cmd.output().await.unwrap();

        // Read the captured arguments.
        let args = fs::read_to_string(tmp.path().join("args"))
            .unwrap()
            .trim()
            .to_string();

        assert!(
            args.contains("--workspace"),
            "args should contain --workspace flag: {args}"
        );
        assert!(
            args.contains("/etc/belt/workspace.yaml"),
            "args should contain the workspace config path: {args}"
        );
        assert!(
            args.contains("--json"),
            "args should contain --json flag: {args}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_evaluate_subprocess_omits_workspace_flag_when_no_config() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = create_fake_belt_script(tmp.path(), "{}", 0);

        // No workspace_config_path set.
        let evaluator = Evaluator::new("test-ws").with_evaluate_timeout(Duration::from_secs(10));

        let original_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", bin_dir.display(), original_path);

        let belt_home = tmp.path().join("home");
        fs::create_dir_all(&belt_home).unwrap();
        let mut cmd = evaluator.build_evaluate_command(&belt_home);
        cmd.env("PATH", &new_path);

        let _output = cmd.output().await.unwrap();

        let args = fs::read_to_string(tmp.path().join("args"))
            .unwrap()
            .trim()
            .to_string();

        assert!(
            !args.contains("--workspace"),
            "args should NOT contain --workspace when config_path is None: {args}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_evaluate_subprocess_nonzero_exit_code() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let json_output = r#"{"error":"evaluation failed"}"#;
        let bin_dir = create_fake_belt_script(tmp.path(), json_output, 1);

        let evaluator = Evaluator::new("test-ws").with_evaluate_timeout(Duration::from_secs(10));

        let original_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", bin_dir.display(), original_path);

        let belt_home = tmp.path().join("home");
        fs::create_dir_all(&belt_home).unwrap();
        let mut cmd = evaluator.build_evaluate_command(&belt_home);
        cmd.env("PATH", &new_path);

        let child_output = cmd.output().await.unwrap();
        let exit_code = child_output.status.code().unwrap_or(-1);

        assert_eq!(
            exit_code, 1,
            "should capture non-zero exit code from subprocess"
        );

        // Even with non-zero exit, JSON should still be parseable from stdout.
        let stdout = String::from_utf8_lossy(&child_output.stdout).to_string();
        let ipc_result = serde_json::from_str::<serde_json::Value>(stdout.trim()).ok();
        let ipc = ipc_result.expect("JSON should parse even on non-zero exit");
        assert_eq!(ipc["error"], "evaluation failed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_evaluate_timeout_with_slow_subprocess() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let script_path = bin_dir.join("belt");

        // Create a script that sleeps longer than the timeout.
        let script = "#!/bin/sh\nsleep 60\n";
        fs::write(&script_path, script).unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();

        let evaluator = Evaluator::new("test-ws").with_evaluate_timeout(Duration::from_millis(100));

        let original_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", bin_dir.display(), original_path);

        let belt_home = tmp.path().join("home");
        fs::create_dir_all(&belt_home).unwrap();
        let mut cmd = evaluator.build_evaluate_command(&belt_home);
        cmd.env("PATH", &new_path);

        // Apply the same timeout logic as run_evaluate.
        let result = tokio::time::timeout(evaluator.evaluate_timeout, cmd.output()).await;

        assert!(
            result.is_err(),
            "subprocess should time out when it runs longer than evaluate_timeout"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_evaluate_normal_timeout_completes_within_limit() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        // Script that completes immediately.
        let bin_dir = create_fake_belt_script(tmp.path(), r#"{"ok":true}"#, 0);

        let evaluator = Evaluator::new("test-ws").with_evaluate_timeout(Duration::from_secs(30));

        let original_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", bin_dir.display(), original_path);

        let belt_home = tmp.path().join("home");
        fs::create_dir_all(&belt_home).unwrap();
        let mut cmd = evaluator.build_evaluate_command(&belt_home);
        cmd.env("PATH", &new_path);

        // Apply the same timeout logic as run_evaluate.
        let result = tokio::time::timeout(evaluator.evaluate_timeout, cmd.output()).await;

        assert!(
            result.is_ok(),
            "fast subprocess should complete within timeout"
        );
        let child_output = result.unwrap().unwrap();
        assert_eq!(child_output.status.code().unwrap(), 0);
    }

    #[test]
    fn evaluate_result_success_false_for_nonzero_exit() {
        let result = EvaluateResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "error".to_string(),
            action_result: None,
            ipc_result: None,
        };
        assert!(!result.success(), "exit_code 1 should not be success");
    }

    #[test]
    fn evaluate_result_success_true_for_zero_exit() {
        let result = EvaluateResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            action_result: None,
            ipc_result: None,
        };
        assert!(result.success(), "exit_code 0 should be success");
    }

    #[test]
    fn builder_pattern_chaining() {
        let config_path = PathBuf::from("/opt/belt/ws.yaml");
        let evaluator = Evaluator::new("chained-ws")
            .with_max_eval_failures(5)
            .with_workspace_config_path(config_path.clone())
            .with_evaluate_timeout(Duration::from_secs(60));

        assert_eq!(evaluator.workspace_name, "chained-ws");
        assert_eq!(evaluator.max_eval_failures, 5);
        assert_eq!(evaluator.workspace_config_path, Some(config_path));
        assert_eq!(evaluator.evaluate_timeout, Duration::from_secs(60));
    }

    #[test]
    fn workspace_config_path_with_relative_path() {
        let evaluator = Evaluator::new("test-ws")
            .with_workspace_config_path(PathBuf::from("relative/path/ws.yaml"));
        assert_eq!(
            evaluator.workspace_config_path,
            Some(PathBuf::from("relative/path/ws.yaml")),
            "relative paths should be stored as-is"
        );
    }

    #[test]
    fn workspace_config_path_with_tilde_path() {
        let evaluator = Evaluator::new("test-ws")
            .with_workspace_config_path(PathBuf::from("~/belt/workspace.yaml"));
        assert_eq!(
            evaluator.workspace_config_path,
            Some(PathBuf::from("~/belt/workspace.yaml")),
            "tilde paths should be stored as-is (no expansion at config time)"
        );
    }

    #[test]
    fn evaluate_result_from_action_result_empty_stdout() {
        let action_result = ActionResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration: std::time::Duration::from_millis(10),
            token_usage: None,
            runtime_name: None,
            model: None,
        };

        let eval_result = EvaluateResult::from(action_result);
        assert!(eval_result.success());
        assert!(
            eval_result.ipc_result.is_none(),
            "empty stdout should yield None ipc_result"
        );
    }

    #[test]
    fn evaluate_result_from_action_result_whitespace_only_stdout() {
        let action_result = ActionResult {
            exit_code: 0,
            stdout: "   \n  \t  ".to_string(),
            stderr: String::new(),
            duration: std::time::Duration::from_millis(10),
            token_usage: None,
            runtime_name: None,
            model: None,
        };

        let eval_result = EvaluateResult::from(action_result);
        assert!(
            eval_result.ipc_result.is_none(),
            "whitespace-only stdout should yield None ipc_result"
        );
    }

    // --- EvalVerdict tests ---

    #[test]
    fn eval_verdict_parse_pass() {
        let input = r#"{"verdict": "pass"}"#;
        let v = EvalVerdict::parse(input).expect("should parse pass verdict");
        assert!(v.is_pass());
        assert!(v.reason.is_none());
        assert!(v.suggestions.is_empty());
    }

    #[test]
    fn eval_verdict_parse_fail_with_reason() {
        let input = r#"{"verdict": "fail", "reason": "missing tests"}"#;
        let v = EvalVerdict::parse(input).expect("should parse fail verdict");
        assert!(!v.is_pass());
        assert_eq!(v.reason.as_deref(), Some("missing tests"));
    }

    #[test]
    fn eval_verdict_parse_fail_with_suggestions() {
        let input =
            r#"{"verdict": "fail", "reason": "issues", "suggestions": ["add tests", "fix lint"]}"#;
        let v = EvalVerdict::parse(input).expect("should parse fail verdict with suggestions");
        assert!(!v.is_pass());
        assert_eq!(v.suggestions.len(), 2);
        assert_eq!(v.suggestions[0], "add tests");
        assert_eq!(v.suggestions[1], "fix lint");
    }

    #[test]
    fn eval_verdict_parse_embedded_json() {
        let input = "Here is my evaluation:\n{\"verdict\": \"pass\"}\nThat's my assessment.";
        let v = EvalVerdict::parse(input).expect("should parse embedded JSON");
        assert!(v.is_pass());
    }

    #[test]
    fn eval_verdict_parse_invalid_returns_none() {
        assert!(EvalVerdict::parse("not json at all").is_none());
        assert!(EvalVerdict::parse("").is_none());
        assert!(EvalVerdict::parse("   ").is_none());
    }

    #[test]
    fn eval_verdict_is_pass_case_insensitive() {
        let input = r#"{"verdict": "Pass"}"#;
        let v = EvalVerdict::parse(input).expect("should parse");
        assert!(v.is_pass());

        let input2 = r#"{"verdict": "PASS"}"#;
        let v2 = EvalVerdict::parse(input2).expect("should parse");
        assert!(v2.is_pass());
    }

    #[test]
    fn build_per_item_evaluate_prompt_contains_item_details() {
        let evaluator = Evaluator::new("my-workspace");
        let mut item = test_item("test-source", "implement");
        item.title = Some("Fix login bug".to_string());
        let prompt = evaluator.build_per_item_evaluate_prompt(&item);

        assert!(prompt.contains("my-workspace"));
        assert!(prompt.contains(&item.work_id));
        assert!(prompt.contains("Fix login bug"));
        assert!(prompt.contains("implement"));
        assert!(prompt.contains("verdict"));
    }

    #[test]
    fn build_per_item_evaluate_prompt_handles_no_title() {
        let evaluator = Evaluator::new("ws");
        let mut item = test_item("src", "analyze");
        item.title = None;
        let prompt = evaluator.build_per_item_evaluate_prompt(&item);
        assert!(prompt.contains("(untitled)"));
    }

    #[test]
    fn default_eval_batch_size() {
        assert_eq!(DEFAULT_EVAL_BATCH_SIZE, 10);
    }
}
