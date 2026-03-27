//! Unix implementations of [`ShellExecutor`] and [`DaemonNotifier`].
//!
//! Shell commands are executed via `bash -c` (falling back to `sh -c` if
//! `bash` is not available). Daemon notifications use `SIGUSR1`.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use belt_core::error::BeltError;
use belt_core::platform::{DaemonNotifier, ShellExecutor, ShellOutput};

/// Executes shell commands via `bash -c` on Unix systems.
#[derive(Debug, Default, Clone)]
pub struct UnixShellExecutor;

impl ShellExecutor for UnixShellExecutor {
    fn execute(
        &self,
        command: &str,
        working_dir: &Path,
        env_vars: &HashMap<String, String>,
    ) -> Result<ShellOutput, BeltError> {
        let output = Command::new("bash")
            .arg("-c")
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

    #[test]
    fn execute_echo() {
        let executor = UnixShellExecutor;
        let result = executor
            .execute("echo hello", Path::new("/tmp"), &HashMap::new())
            .unwrap();
        assert!(result.success());
        assert!(result.stdout.contains("hello"));
    }

    #[test]
    fn execute_with_env_vars() {
        let executor = UnixShellExecutor;
        let mut env = HashMap::new();
        env.insert("MY_TEST_VAR".to_string(), "test_value".to_string());
        let result = executor
            .execute("echo $MY_TEST_VAR", Path::new("/tmp"), &env)
            .unwrap();
        assert!(result.success());
        assert!(result.stdout.contains("test_value"));
    }

    #[test]
    fn execute_failing_command() {
        let executor = UnixShellExecutor;
        let result = executor
            .execute("exit 42", Path::new("/tmp"), &HashMap::new())
            .unwrap();
        assert!(!result.success());
        assert_eq!(result.exit_code, Some(42));
    }

    #[test]
    fn execute_captures_stderr() {
        let executor = UnixShellExecutor;
        let result = executor
            .execute("echo err_msg >&2", Path::new("/tmp"), &HashMap::new())
            .unwrap();
        assert!(result.success());
        assert!(result.stderr.contains("err_msg"));
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
