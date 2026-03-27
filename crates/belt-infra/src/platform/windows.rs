//! Windows implementations of [`ShellExecutor`] and [`DaemonNotifier`].
//!
//! Shell commands are executed via `cmd.exe /C`. Daemon notifications use
//! a named pipe at `\\.\pipe\belt-daemon-{pid}`.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use belt_core::error::BeltError;
use belt_core::platform::{DaemonNotifier, ShellExecutor, ShellOutput};

/// Executes shell commands via `cmd.exe /C` on Windows systems.
#[derive(Debug, Default, Clone)]
pub struct WindowsShellExecutor;

impl ShellExecutor for WindowsShellExecutor {
    fn execute(
        &self,
        command: &str,
        working_dir: &Path,
        env_vars: &HashMap<String, String>,
    ) -> Result<ShellOutput, BeltError> {
        let output = Command::new("cmd.exe")
            .arg("/C")
            .arg(command)
            .current_dir(working_dir)
            .envs(env_vars)
            .output()
            .map_err(|e| {
                BeltError::Runtime(format!("failed to spawn shell command '{command}': {e}"))
            })?;

        Ok(ShellOutput {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}

/// Sends a wake-up notification to a daemon process via a named pipe on Windows.
///
/// The daemon is expected to listen on `\\.\pipe\belt-daemon-{pid}`.
#[derive(Debug, Default, Clone)]
pub struct WindowsDaemonNotifier;

impl DaemonNotifier for WindowsDaemonNotifier {
    fn notify(&self, pid: u32) -> Result<(), BeltError> {
        let pipe_name = format!(r"\\.\pipe\belt-daemon-{pid}");

        // Attempt to open the named pipe and write a wake-up byte.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&pipe_name)
            .map_err(|e| {
                BeltError::Runtime(format!(
                    "failed to open named pipe '{pipe_name}' for daemon pid {pid}: {e}"
                ))
            })?;

        use std::io::Write;
        file.write_all(b"wake").map_err(|e| {
            BeltError::Runtime(format!("failed to write to named pipe '{pipe_name}': {e}"))
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── WindowsShellExecutor tests ──
    // These tests execute cmd.exe and only run on Windows.

    #[cfg(target_os = "windows")]
    #[test]
    fn execute_echo_command() {
        let executor = WindowsShellExecutor;
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::new();

        let output = executor.execute("echo hello", tmp.path(), &env).unwrap();
        assert!(output.success());
        assert!(output.stdout.contains("hello"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn execute_with_env_vars() {
        let executor = WindowsShellExecutor;
        let tmp = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert("BELT_TEST_VAR".to_string(), "test_value".to_string());

        let output = executor
            .execute("echo %BELT_TEST_VAR%", tmp.path(), &env)
            .unwrap();
        assert!(output.success());
        assert!(output.stdout.contains("test_value"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn execute_failing_command() {
        let executor = WindowsShellExecutor;
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::new();

        let output = executor.execute("exit /b 42", tmp.path(), &env).unwrap();
        assert!(!output.success());
        assert_eq!(output.exit_code, Some(42));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn execute_captures_stderr() {
        let executor = WindowsShellExecutor;
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::new();

        let output = executor
            .execute("echo error_msg >&2", tmp.path(), &env)
            .unwrap();
        assert!(output.stderr.contains("error_msg"));
    }

    // ── WindowsShellExecutor: non-Windows platform tests ──
    // On non-Windows, cmd.exe is unavailable so we verify that execute returns an error.

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn execute_fails_on_non_windows() {
        let executor = WindowsShellExecutor;
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::new();

        let result = executor.execute("echo hello", tmp.path(), &env);
        assert!(
            result.is_err(),
            "cmd.exe should not be available on non-Windows"
        );
    }

    // ── WindowsDaemonNotifier tests ──
    // On non-Windows platforms, the named pipe will not exist, so notify returns an error.

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn notify_returns_error_on_non_windows() {
        let notifier = WindowsDaemonNotifier;
        let result = notifier.notify(1234);
        assert!(result.is_err());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn notify_error_contains_pid() {
        let notifier = WindowsDaemonNotifier;
        let result = notifier.notify(5678);
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("5678"),
            "error should contain the pid: {err_msg}"
        );
    }

    // ── Struct construction tests (platform-independent) ──

    #[test]
    fn windows_shell_executor_default() {
        let _executor = WindowsShellExecutor::default();
    }

    #[test]
    fn windows_daemon_notifier_default() {
        let _notifier = WindowsDaemonNotifier::default();
    }
}
