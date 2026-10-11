//! Windows implementations of [`ShellExecutor`] and [`DaemonNotifier`].
//!
//! Shell commands are executed via `cmd.exe /C`. Daemon notifications use
//! a named pipe at `\\.\pipe\belt-daemon-{pid}`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::process::Command;

use belt_core::error::BeltError;
use belt_core::platform::{
    DaemonNotifier, NoopProcessSink, ProcessKiller, ProcessSink, ShellExecutor, ShellOutput,
};

use super::output_in_new_group;

/// Executes shell commands via `cmd.exe /C` on Windows systems.
#[derive(Debug, Default, Clone)]
pub struct WindowsShellExecutor;

#[async_trait]
impl ShellExecutor for WindowsShellExecutor {
    async fn execute(
        &self,
        command: &str,
        working_dir: &Path,
        env_vars: &HashMap<String, String>,
    ) -> Result<ShellOutput, BeltError> {
        self.execute_with_sink(command, working_dir, env_vars, Arc::new(NoopProcessSink))
            .await
    }

    async fn execute_with_sink(
        &self,
        command: &str,
        working_dir: &Path,
        env_vars: &HashMap<String, String>,
        sink: Arc<dyn ProcessSink>,
    ) -> Result<ShellOutput, BeltError> {
        let mut cmd = Command::new("cmd.exe");
        cmd.arg("/C")
            .arg(command)
            .current_dir(working_dir)
            .envs(env_vars);
        let output = output_in_new_group(&mut cmd, sink.as_ref())
            .await
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

/// Terminates a handler process tree with `taskkill /T /F` on Windows.
#[derive(Debug, Default, Clone)]
pub struct WindowsProcessKiller;

impl ProcessKiller for WindowsProcessKiller {
    fn kill_group(&self, pid: u32) -> Result<(), BeltError> {
        if pid == 0 {
            return Err(BeltError::Runtime(
                "refusing to kill process tree of invalid pid 0".to_string(),
            ));
        }
        let output = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output()
            .map_err(|e| {
                BeltError::Runtime(format!("failed to run taskkill for pid {pid}: {e}"))
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(BeltError::Runtime(format!(
                "taskkill failed for pid {pid}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
}

/// Windows side of [`super::probe_handler`]: the start time of a process is
/// not checked here, so a recorded pid cannot be told from a reused one.
pub(crate) fn probe_handler(
    pid: u32,
    _running_since: chrono::DateTime<chrono::Utc>,
) -> super::HandlerProbe {
    super::HandlerProbe::Unknown(format!(
        "pid {pid}: process identity is not verified on Windows"
    ))
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
    #[tokio::test]
    async fn execute_echo_command() {
        let executor = WindowsShellExecutor;
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
    async fn execute_with_sink_reports_the_spawned_pid_once() {
        use crate::platform::testing::RecordingSink;

        let sink = Arc::new(RecordingSink::default());
        let tmp = tempfile::tempdir().unwrap();

        let output = WindowsShellExecutor
            .execute_with_sink("echo hello", tmp.path(), &HashMap::new(), sink.clone())
            .await
            .unwrap();

        assert!(output.success());
        assert_eq!(sink.pids().len(), 1);
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn kill_group_terminates_the_handler_quickly() {
        use crate::platform::testing::RecordingSink;

        let sink = Arc::new(RecordingSink::default());
        let tmp = tempfile::tempdir().unwrap();
        let running = {
            let sink = sink.clone();
            let dir = tmp.path().to_path_buf();
            tokio::spawn(async move {
                WindowsShellExecutor
                    .execute_with_sink("ping -n 30 127.0.0.1 >nul", &dir, &HashMap::new(), sink)
                    .await
            })
        };
        let pid = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(pid) = sink.pids().first() {
                    break *pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the executor must report the pid after the spawn");

        WindowsProcessKiller.kill_group(pid).unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(5), running)
            .await
            .expect("handler must end after the tree kill")
            .unwrap()
            .unwrap();
    }

    #[test]
    fn kill_group_rejects_pid_zero() {
        assert!(WindowsProcessKiller.kill_group(0).is_err());
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn execute_with_env_vars() {
        let executor = WindowsShellExecutor;
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
        let executor = WindowsShellExecutor;
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::new();

        let output = executor
            .execute("exit /b 42", tmp.path(), &env)
            .await
            .unwrap();
        assert!(!output.success());
        assert_eq!(output.exit_code, Some(42));
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn execute_captures_stderr() {
        let executor = WindowsShellExecutor;
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
        let executor = WindowsShellExecutor;
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::new();

        let result = executor.execute("echo hello", tmp.path(), &env).await;
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
