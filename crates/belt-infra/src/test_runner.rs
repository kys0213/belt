//! Test runner for spec verification commands.
//!
//! Executes a list of shell commands and reports the aggregate result.
//! Used by the gap-detection cron job to verify that a spec's test suite
//! passes before advancing it from Completing to Completed.

use std::path::Path;
use std::process::Command;

use belt_core::error::BeltError;

/// Result of executing a single test command.
#[derive(Debug, Clone)]
pub struct TestCommandResult {
    /// The command that was executed.
    pub command: String,
    /// Whether the command exited successfully (exit code 0).
    pub success: bool,
    /// Combined stdout/stderr output (truncated if very large).
    pub output: String,
}

/// Aggregate result of running all test commands for a spec.
#[derive(Debug, Clone)]
pub struct TestRunResult {
    /// Individual command results.
    pub results: Vec<TestCommandResult>,
    /// `true` only when every command succeeded.
    pub all_passed: bool,
}

/// Run a list of shell commands sequentially in the given working directory.
///
/// Each command is executed via `sh -c` (on Unix) so that pipes, redirects,
/// and other shell features work as expected. Execution stops at the first
/// failure when `fail_fast` is `true`.
///
/// # Errors
///
/// Returns `BeltError::Runtime` if a command cannot be spawned at all
/// (e.g. `sh` not found). Individual command failures are captured in
/// the returned `TestRunResult` rather than as errors.
pub fn run_test_commands(
    commands: &[&str],
    working_dir: &Path,
    fail_fast: bool,
) -> Result<TestRunResult, BeltError> {
    let mut results = Vec::with_capacity(commands.len());
    let mut all_passed = true;

    for cmd in commands {
        let output = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(working_dir)
            .output()
            .map_err(|e| {
                BeltError::Runtime(format!("failed to spawn test command '{cmd}': {e}"))
            })?;

        let success = output.status.success();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{stdout}{stderr}");

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
        // Only the first (failing) command should have been executed.
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
}
