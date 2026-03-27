//! Windows platform implementations for [`ShellExecutor`] and [`DaemonNotifier`].

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use belt_core::platform::{DaemonNotifier, ShellExecutor, ShellOutput};

/// Windows shell executor that runs scripts via `cmd.exe /C`.
#[derive(Debug, Default)]
pub struct WindowsShellExecutor;

impl WindowsShellExecutor {
    /// Create a new `WindowsShellExecutor`.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ShellExecutor for WindowsShellExecutor {
    async fn execute(
        &self,
        script: &str,
        working_dir: &Path,
        env_vars: &HashMap<String, String>,
    ) -> anyhow::Result<ShellOutput> {
        let output = tokio::process::Command::new("cmd.exe")
            .arg("/C")
            .arg(script)
            .current_dir(working_dir)
            .envs(env_vars)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to spawn cmd.exe command: {e}"))?;

        Ok(ShellOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}

/// Windows daemon notifier.
///
/// On Windows, Unix signals are not available. This implementation returns
/// an error indicating that daemon notification is not yet supported.
/// Future implementations may use named pipes or TCP localhost.
#[derive(Debug, Default)]
pub struct WindowsDaemonNotifier;

impl WindowsDaemonNotifier {
    /// Create a new `WindowsDaemonNotifier`.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl DaemonNotifier for WindowsDaemonNotifier {
    async fn notify(&self, pid: u32) -> anyhow::Result<()> {
        anyhow::bail!(
            "daemon notification is not yet supported on Windows (pid: {pid}). \
             Use tick-based polling as a fallback."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── WindowsShellExecutor tests ──
    // These tests execute cmd.exe and only run on Windows.

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn execute_echo_command() {
        let executor = WindowsShellExecutor::new();
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::new();

        let output = executor
            .execute("echo hello", tmp.path(), &env)
            .await
            .unwrap();
        assert!(output.success());
        assert!(output.stdout.contains("hello"));
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn execute_with_env_vars() {
        let executor = WindowsShellExecutor::new();
        let tmp = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert("BELT_TEST_VAR".to_string(), "test_value".to_string());

        let output = executor
            .execute("echo %BELT_TEST_VAR%", tmp.path(), &env)
            .await
            .unwrap();
        assert!(output.success());
        assert!(output.stdout.contains("test_value"));
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn execute_failing_command() {
        let executor = WindowsShellExecutor::new();
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::new();

        let output = executor
            .execute("exit /b 42", tmp.path(), &env)
            .await
            .unwrap();
        assert!(!output.success());
        assert_eq!(output.exit_code, 42);
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn execute_captures_stderr() {
        let executor = WindowsShellExecutor::new();
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::new();

        let output = executor
            .execute("echo error_msg >&2", tmp.path(), &env)
            .await
            .unwrap();
        assert!(output.stderr.contains("error_msg"));
    }

    // ── WindowsShellExecutor: non-Windows platform tests ──
    // On non-Windows, cmd.exe is unavailable so we verify that execute returns an error.

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn execute_fails_on_non_windows() {
        let executor = WindowsShellExecutor::new();
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::new();

        let result = executor.execute("echo hello", tmp.path(), &env).await;
        assert!(
            result.is_err(),
            "cmd.exe should not be available on non-Windows"
        );
    }

    // ── WindowsDaemonNotifier tests ──
    // The notifier currently returns an error on all platforms.

    #[tokio::test]
    async fn notify_returns_not_supported_error() {
        let notifier = WindowsDaemonNotifier::new();
        let result = notifier.notify(1234).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not yet supported on Windows"),
            "unexpected error message: {err_msg}"
        );
    }

    #[tokio::test]
    async fn notify_error_contains_pid() {
        let notifier = WindowsDaemonNotifier::new();
        let result = notifier.notify(5678).await;
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
    fn windows_shell_executor_new() {
        let _executor = WindowsShellExecutor::new();
    }

    #[test]
    fn windows_daemon_notifier_default() {
        let _notifier = WindowsDaemonNotifier::default();
    }

    #[test]
    fn windows_daemon_notifier_new() {
        let _notifier = WindowsDaemonNotifier::new();
    }
}
