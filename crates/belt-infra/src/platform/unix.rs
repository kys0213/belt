//! Unix platform implementations for [`ShellExecutor`] and [`DaemonNotifier`].

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use belt_core::platform::{DaemonNotifier, ShellExecutor, ShellOutput};

/// Unix shell executor that runs scripts via `sh -c`.
#[derive(Debug, Default)]
pub struct UnixShellExecutor;

impl UnixShellExecutor {
    /// Create a new `UnixShellExecutor`.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ShellExecutor for UnixShellExecutor {
    async fn execute(
        &self,
        script: &str,
        working_dir: &Path,
        env_vars: &HashMap<String, String>,
    ) -> anyhow::Result<ShellOutput> {
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(working_dir)
            .envs(env_vars)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to spawn shell command: {e}"))?;

        Ok(ShellOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}

/// Unix daemon notifier that sends SIGUSR1 to the daemon process.
#[derive(Debug, Default)]
pub struct UnixDaemonNotifier;

impl UnixDaemonNotifier {
    /// Create a new `UnixDaemonNotifier`.
    pub fn new() -> Self {
        Self
    }
}

#[cfg(unix)]
#[async_trait]
impl DaemonNotifier for UnixDaemonNotifier {
    async fn notify(&self, pid: u32) -> anyhow::Result<()> {
        let status = tokio::process::Command::new("kill")
            .args(["-USR1", &pid.to_string()])
            .status()
            .await
            .map_err(|e| anyhow::anyhow!("failed to execute kill command: {e}"))?;

        if !status.success() {
            anyhow::bail!("failed to send SIGUSR1 to pid {pid}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn execute_echo_command() {
        let executor = UnixShellExecutor::new();
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::new();

        let output = executor
            .execute("echo hello", tmp.path(), &env)
            .await
            .unwrap();
        assert!(output.success());
        assert_eq!(output.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn execute_with_env_vars() {
        let executor = UnixShellExecutor::new();
        let tmp = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert("BELT_TEST_VAR".to_string(), "test_value".to_string());

        let output = executor
            .execute("echo $BELT_TEST_VAR", tmp.path(), &env)
            .await
            .unwrap();
        assert!(output.success());
        assert_eq!(output.stdout.trim(), "test_value");
    }

    #[tokio::test]
    async fn execute_failing_command() {
        let executor = UnixShellExecutor::new();
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::new();

        let output = executor.execute("exit 42", tmp.path(), &env).await.unwrap();
        assert!(!output.success());
        assert_eq!(output.exit_code, 42);
    }

    #[tokio::test]
    async fn execute_captures_stderr() {
        let executor = UnixShellExecutor::new();
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::new();

        let output = executor
            .execute("echo error_msg >&2", tmp.path(), &env)
            .await
            .unwrap();
        assert!(output.success());
        assert!(output.stderr.contains("error_msg"));
    }

    #[tokio::test]
    async fn execute_respects_working_dir() {
        let executor = UnixShellExecutor::new();
        let tmp = tempfile::tempdir().unwrap();
        let env = HashMap::new();

        let output = executor.execute("pwd", tmp.path(), &env).await.unwrap();
        assert!(output.success());
        // Resolve symlinks for macOS /tmp -> /private/tmp
        let canonical = tmp.path().canonicalize().unwrap();
        assert_eq!(output.stdout.trim(), canonical.to_str().unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_invalid_pid_returns_error() {
        let notifier = UnixDaemonNotifier::new();
        // PID 0 refers to the process group; a very large PID should not exist.
        let result = notifier.notify(999_999_999).await;
        assert!(result.is_err());
    }
}
