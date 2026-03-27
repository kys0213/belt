//! Platform abstraction traits for cross-platform compatibility.
//!
//! Defines [`ShellExecutor`] for shell script execution and [`DaemonNotifier`]
//! for inter-process communication between daemon and CLI. Implementations
//! live in `belt-infra`.

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;

/// Result of a shell script execution.
#[derive(Debug, Clone)]
pub struct ShellOutput {
    /// Process exit code (0 = success).
    pub exit_code: i32,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

impl ShellOutput {
    /// Returns `true` if the process exited successfully (code 0).
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Platform-specific shell script execution.
///
/// Unix implementations use `sh -c`, Windows uses `cmd.exe /C`.
#[async_trait]
pub trait ShellExecutor: Send + Sync {
    /// Execute a shell script in the given working directory with environment variables.
    async fn execute(
        &self,
        script: &str,
        working_dir: &Path,
        env_vars: &HashMap<String, String>,
    ) -> anyhow::Result<ShellOutput>;
}

/// Platform-specific daemon notification (IPC).
///
/// Unix implementations use SIGUSR1, Windows uses alternative mechanisms.
#[async_trait]
pub trait DaemonNotifier: Send + Sync {
    /// Send a notification to the daemon process identified by `pid`.
    async fn notify(&self, pid: u32) -> anyhow::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_output_success_when_exit_code_zero() {
        let output = ShellOutput {
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
        };
        assert!(output.success());
    }

    #[test]
    fn shell_output_failure_when_exit_code_nonzero() {
        let output = ShellOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: "error".to_string(),
        };
        assert!(!output.success());
    }

    #[test]
    fn shell_output_negative_exit_code() {
        let output = ShellOutput {
            exit_code: -1,
            stdout: String::new(),
            stderr: "signal".to_string(),
        };
        assert!(!output.success());
    }
}
