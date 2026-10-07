//! Platform abstraction traits for shell execution and daemon notification.
//!
//! These traits decouple the daemon and infrastructure layers from
//! platform-specific details such as the shell binary (`bash` vs `cmd.exe`)
//! and inter-process signaling (`SIGUSR1` vs named pipes).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::BeltError;

/// Result of executing a shell command.
#[derive(Debug, Clone)]
pub struct ShellOutput {
    /// Process exit code (`None` if terminated by signal).
    pub exit_code: Option<i32>,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

impl ShellOutput {
    /// Returns `true` when the process exited with code 0.
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Platform-agnostic shell command executor.
///
/// Implementations translate a command string into the appropriate shell
/// invocation for the target platform (e.g. `bash -c` on Unix,
/// `cmd.exe /C` on Windows).
#[async_trait]
pub trait ShellExecutor: Send + Sync {
    /// Execute `command` asynchronously in `working_dir` with optional
    /// environment variables.
    async fn execute(
        &self,
        command: &str,
        working_dir: &Path,
        env_vars: &HashMap<String, String>,
    ) -> Result<ShellOutput, BeltError>;

    /// Same as [`execute`](Self::execute), but reports the pid of the spawned
    /// process to `sink` right after the spawn, exactly once.
    ///
    /// The default delegates to `execute` and never calls `sink`: an
    /// implementation that spawns a process **must** override this method,
    /// otherwise its process cannot be identified for cancellation.
    async fn execute_with_sink(
        &self,
        command: &str,
        working_dir: &Path,
        env_vars: &HashMap<String, String>,
        sink: Arc<dyn ProcessSink>,
    ) -> Result<ShellOutput, BeltError> {
        let _ = sink;
        self.execute(command, working_dir, env_vars).await
    }
}

/// Receives the pid of a handler process as soon as it is spawned.
///
/// The pid identifies the leader of a dedicated process group, so that
/// [`ProcessKiller::kill_group`] can terminate the process and its children.
pub trait ProcessSink: Send + Sync {
    /// Called exactly once per spawned process, right after the spawn.
    fn spawned(&self, pid: u32);
}

/// A [`ProcessSink`] that ignores the pid.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopProcessSink;

impl ProcessSink for NoopProcessSink {
    fn spawned(&self, _pid: u32) {}
}

/// Terminates a handler process together with its children.
pub trait ProcessKiller: Send + Sync {
    /// Terminate the process group led by `pid`.
    ///
    /// # Errors
    /// `BeltError::Runtime` when `pid` is not a valid group leader pid or the
    /// platform refuses the termination (including a group that no longer exists).
    fn kill_group(&self, pid: u32) -> Result<(), BeltError>;
}

/// Platform-agnostic daemon notification mechanism.
///
/// Used to signal a running daemon process to wake up and re-evaluate
/// its work queue (e.g. after a new issue is collected).
pub trait DaemonNotifier: Send + Sync {
    /// Send a wake-up notification to the daemon identified by `pid`.
    fn notify(&self, pid: u32) -> Result<(), BeltError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_output_success_when_zero() {
        let output = ShellOutput {
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(output.success());
    }

    #[test]
    fn shell_output_failure_when_nonzero() {
        for code in [1, 2, 127, 255] {
            let output = ShellOutput {
                exit_code: Some(code),
                stdout: String::new(),
                stderr: String::new(),
            };
            assert!(!output.success(), "exit_code {code} should not be success");
        }
    }

    #[test]
    fn shell_output_failure_when_none() {
        let output = ShellOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(!output.success());
    }
}
