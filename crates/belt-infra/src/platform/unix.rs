//! Unix implementations of [`ShellExecutor`] and [`DaemonNotifier`].
//!
//! Shell commands are executed via `bash -c` (falling back to `sh -c` if
//! `bash` is not available). Daemon notifications use `SIGUSR1`.

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

/// Executes shell commands via `bash -c` on Unix systems.
#[derive(Debug, Default, Clone)]
pub struct UnixShellExecutor;

#[async_trait]
impl ShellExecutor for UnixShellExecutor {
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
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
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

/// Terminates a handler process group with `SIGKILL` on Unix.
///
/// The handler is spawned as the leader of its own process group, so the
/// group id equals the reported pid and children die with it.
#[derive(Debug, Default, Clone)]
pub struct UnixProcessKiller;

impl ProcessKiller for UnixProcessKiller {
    fn kill_group(&self, pid: u32) -> Result<(), BeltError> {
        // pid 0 and 1 would address the caller's own group or init's group;
        // values above i32::MAX wrap to negative ids that mean other targets.
        let group = match libc::pid_t::try_from(pid) {
            Ok(group) if group > 1 => group,
            _ => {
                return Err(BeltError::Runtime(format!(
                    "refusing to kill process group of invalid pid {pid}"
                )));
            }
        };
        // Safety: `group` is validated above and the call only delivers a signal.
        let ret = unsafe { libc::killpg(group, libc::SIGKILL) };
        if ret == 0 {
            Ok(())
        } else {
            let err = std::io::Error::last_os_error();
            Err(BeltError::Runtime(format!(
                "failed to kill process group {pid}: {err}"
            )))
        }
    }
}

/// Sends `SIGUSR1` to a daemon process on Unix.
#[derive(Debug, Default, Clone)]
pub struct UnixDaemonNotifier;

impl DaemonNotifier for UnixDaemonNotifier {
    fn notify(&self, pid: u32) -> Result<(), BeltError> {
        // Safety: We are sending a well-defined signal to a known PID.
        // SIGUSR1 is the conventional signal for user-defined wake-up.
        let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGUSR1) };
        if ret == 0 {
            Ok(())
        } else {
            let err = std::io::Error::last_os_error();
            Err(BeltError::Runtime(format!(
                "failed to send SIGUSR1 to pid {pid}: {err}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::testing::RecordingSink;

    #[tokio::test]
    async fn execute_echo() {
        let executor = UnixShellExecutor;
        let result = executor
            .execute("echo hello", Path::new("/tmp"), &HashMap::new())
            .await
            .unwrap();
        assert!(result.success());
        assert!(result.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn execute_with_env_vars() {
        let executor = UnixShellExecutor;
        let mut env = HashMap::new();
        env.insert("MY_TEST_VAR".to_string(), "test_value".to_string());
        let result = executor
            .execute("echo $MY_TEST_VAR", Path::new("/tmp"), &env)
            .await
            .unwrap();
        assert!(result.success());
        assert!(result.stdout.contains("test_value"));
    }

    #[tokio::test]
    async fn execute_failing_command() {
        let executor = UnixShellExecutor;
        let result = executor
            .execute("exit 42", Path::new("/tmp"), &HashMap::new())
            .await
            .unwrap();
        assert!(!result.success());
        assert_eq!(result.exit_code, Some(42));
    }

    #[tokio::test]
    async fn execute_captures_stderr() {
        let executor = UnixShellExecutor;
        let result = executor
            .execute("echo err_msg >&2", Path::new("/tmp"), &HashMap::new())
            .await
            .unwrap();
        assert!(result.success());
        assert!(result.stderr.contains("err_msg"));
    }

    #[tokio::test]
    async fn execute_respects_working_dir() {
        let executor = UnixShellExecutor;
        let tmp = tempfile::tempdir().unwrap();
        let result = executor
            .execute("pwd", tmp.path(), &HashMap::new())
            .await
            .unwrap();
        assert!(result.success());
        // Resolve symlinks for macOS /tmp -> /private/tmp
        let canonical = tmp.path().canonicalize().unwrap();
        assert_eq!(result.stdout.trim(), canonical.to_str().unwrap());
    }

    #[tokio::test]
    async fn execute_with_sink_reports_the_spawned_pid_once() {
        let sink = Arc::new(RecordingSink::default());
        let result = UnixShellExecutor
            .execute_with_sink("echo hi", Path::new("/tmp"), &HashMap::new(), sink.clone())
            .await
            .unwrap();
        assert!(result.success());
        assert!(result.stdout.contains("hi"));
        assert_eq!(sink.pids().len(), 1);
        assert!(sink.pids()[0] > 1);
    }

    #[tokio::test]
    async fn kill_group_terminates_the_handler_and_its_children_quickly() {
        let sink = Arc::new(RecordingSink::default());
        let started = std::time::Instant::now();
        let running = {
            let sink = sink.clone();
            tokio::spawn(async move {
                // The shell forks `sleep`; only a group kill reaches the child.
                UnixShellExecutor
                    .execute_with_sink("sleep 30 & wait", Path::new("/tmp"), &HashMap::new(), sink)
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

        UnixProcessKiller.kill_group(pid).unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), running)
            .await
            .expect("handler must end within 1s of the group kill")
            .unwrap()
            .unwrap();
        assert!(!result.success());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn kill_group_rejects_pids_that_address_other_targets() {
        for pid in [0, 1, u32::MAX] {
            assert!(
                UnixProcessKiller.kill_group(pid).is_err(),
                "pid {pid} must be refused"
            );
        }
    }

    #[test]
    fn kill_group_of_a_missing_group_is_an_error() {
        assert!(UnixProcessKiller.kill_group(999_999_999).is_err());
    }

    #[test]
    fn notify_invalid_pid_returns_error() {
        let notifier = UnixDaemonNotifier;
        // PID 0 would signal the entire process group; use an invalid PID instead.
        let result = notifier.notify(999_999_999);
        assert!(result.is_err());
    }

    #[test]
    fn notify_self_process() {
        // Sending SIGUSR1 to ourselves should succeed (default handler may
        // terminate the process, so we install a no-op handler first).
        unsafe {
            libc::signal(libc::SIGUSR1, libc::SIG_IGN);
        }
        let notifier = UnixDaemonNotifier;
        let pid = std::process::id();
        let result = notifier.notify(pid);
        assert!(result.is_ok());
    }
}
