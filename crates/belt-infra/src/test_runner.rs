//! Test runner for spec verification commands.
//!
//! Implements the [`belt_core::test_runner::TestRunner`] trait by delegating
//! shell execution to a [`ShellExecutor`]. Used by the gap-detection cron job
//! to verify that a spec's test suite passes before advancing from Completing
//! to Completed.
//!
//! [`ShellExecutor`]: belt_core::platform::ShellExecutor

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use belt_core::error::BeltError;
use belt_core::platform::ShellExecutor;
use belt_core::test_runner::{TestCommandResult, TestRunResult, TestRunner};

/// Shell-based test runner that delegates to a [`ShellExecutor`].
///
/// Each command is passed to the executor with the given working directory.
/// Pipes, redirects, and other shell features work as the underlying executor
/// permits.
pub struct ShellTestRunner {
    shell: Arc<dyn ShellExecutor>,
}

impl std::fmt::Debug for ShellTestRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellTestRunner").finish()
    }
}

impl Default for ShellTestRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellTestRunner {
    /// Create a new `ShellTestRunner` using the platform-default shell.
    pub fn new() -> Self {
        Self {
            shell: Arc::from(crate::platform::default_shell_executor()),
        }
    }

    /// Create a `ShellTestRunner` with a custom [`ShellExecutor`].
    pub fn with_shell_executor(shell: Arc<dyn ShellExecutor>) -> Self {
        Self { shell }
    }
}

impl TestRunner for ShellTestRunner {
    fn run(
        &self,
        commands: &[&str],
        working_dir: &Path,
        fail_fast: bool,
    ) -> Result<TestRunResult, BeltError> {
        run_test_commands_with_shell(&*self.shell, commands, working_dir, fail_fast)
    }
}

/// Run a list of shell commands sequentially in the given working directory,
/// using the provided [`ShellExecutor`].
///
/// Each command is passed to the executor so that pipes, redirects, and other
/// shell features work as the underlying implementation permits. Execution
/// stops at the first failure when `fail_fast` is `true`.
///
/// # Errors
///
/// Returns `BeltError::Runtime` if a command cannot be spawned at all.
/// Individual command failures are captured in the returned `TestRunResult`
/// rather than as errors.
pub fn run_test_commands_with_shell(
    shell: &dyn ShellExecutor,
    commands: &[&str],
    working_dir: &Path,
    fail_fast: bool,
) -> Result<TestRunResult, BeltError> {
    let mut results = Vec::with_capacity(commands.len());
    let mut all_passed = true;
    let empty_env = HashMap::new();

    for cmd in commands {
        let output = shell.execute(cmd, working_dir, &empty_env)?;

        let success = output.success();
        let combined = format!("{}{}", output.stdout, output.stderr);

        // Truncate output to avoid excessive memory usage.
        const MAX_OUTPUT_LEN: usize = 4096;
        let truncated = if combined.len() > MAX_OUTPUT_LEN {
            format!("{}... (truncated)", &combined[..MAX_OUTPUT_LEN])
        } else {
            combined
        };

        results.push(TestCommandResult {
            command: cmd.to_string(),
            success,
            output: truncated,
        });

        if !success {
            all_passed = false;
            if fail_fast {
                break;
            }
        }
    }

    Ok(TestRunResult {
        results,
        all_passed,
    })
}

/// Convenience wrapper: run test commands using the platform-default shell.
///
/// This preserves backward compatibility with existing callers that do not
/// need to inject a custom [`ShellExecutor`].
pub fn run_test_commands(
    commands: &[&str],
    working_dir: &Path,
    fail_fast: bool,
) -> Result<TestRunResult, BeltError> {
    let shell = crate::platform::default_shell_executor();
    run_test_commands_with_shell(&*shell, commands, working_dir, fail_fast)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_passing_command() {
        let tmp = tempfile::tempdir().unwrap();
        let result = run_test_commands(&["true"], tmp.path(), false).unwrap();
        assert!(result.all_passed);
        assert_eq!(result.results.len(), 1);
        assert!(result.results[0].success);
    }

    #[test]
    fn run_failing_command() {
        let tmp = tempfile::tempdir().unwrap();
        let result = run_test_commands(&["false"], tmp.path(), false).unwrap();
        assert!(!result.all_passed);
        assert_eq!(result.results.len(), 1);
        assert!(!result.results[0].success);
    }

    #[test]
    fn fail_fast_stops_at_first_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let result = run_test_commands(&["false", "true"], tmp.path(), true).unwrap();
        assert!(!result.all_passed);
        assert_eq!(result.results.len(), 1);
    }

    #[test]
    fn no_fail_fast_runs_all_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let result = run_test_commands(&["false", "true"], tmp.path(), false).unwrap();
        assert!(!result.all_passed);
        assert_eq!(result.results.len(), 2);
        assert!(!result.results[0].success);
        assert!(result.results[1].success);
    }

    #[test]
    fn empty_commands_all_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let result = run_test_commands(&[], tmp.path(), false).unwrap();
        assert!(result.all_passed);
        assert!(result.results.is_empty());
    }

    #[test]
    fn captures_output() {
        let tmp = tempfile::tempdir().unwrap();
        let result = run_test_commands(&["echo hello_test"], tmp.path(), false).unwrap();
        assert!(result.all_passed);
        assert!(result.results[0].output.contains("hello_test"));
    }

    #[test]
    fn multiple_passing_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let result = run_test_commands(&["true", "true", "true"], tmp.path(), false).unwrap();
        assert!(result.all_passed);
        assert_eq!(result.results.len(), 3);
    }

    #[test]
    fn shell_test_runner_trait_impl() {
        let runner = ShellTestRunner::new();
        let tmp = tempfile::tempdir().unwrap();
        let result = runner.run(&["true", "echo ok"], tmp.path(), false).unwrap();
        assert!(result.all_passed);
        assert_eq!(result.results.len(), 2);
    }

    #[test]
    fn shell_test_runner_fail_fast() {
        let runner = ShellTestRunner::new();
        let tmp = tempfile::tempdir().unwrap();
        let result = runner.run(&["false", "true"], tmp.path(), true).unwrap();
        assert!(!result.all_passed);
        assert_eq!(result.results.len(), 1);
    }
}
