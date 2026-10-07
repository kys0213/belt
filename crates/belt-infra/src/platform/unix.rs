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

/// Unix side of [`super::probe_handler`].
pub(crate) fn probe_handler(
    pid: u32,
    running_since: chrono::DateTime<chrono::Utc>,
) -> super::HandlerProbe {
    use super::HandlerProbe;

    let Ok(target) = libc::pid_t::try_from(pid) else {
        return HandlerProbe::Unknown(format!("pid {pid} is out of range"));
    };
    if target <= 1 {
        return HandlerProbe::Reused(format!("pid {pid} cannot lead a handler group"));
    }
    // Safety: getpgid only reads the process table.
    let group = unsafe { libc::getpgid(target) };
    if group == -1 {
        let err = std::io::Error::last_os_error();
        return if err.raw_os_error() == Some(libc::ESRCH) {
            HandlerProbe::Gone
        } else {
            HandlerProbe::Unknown(format!("getpgid({pid}) failed: {err}"))
        };
    }
    if group != target {
        return HandlerProbe::Reused(format!("pid {pid} is not a process group leader"));
    }
    let elapsed = match process_elapsed(pid) {
        Ok(Some(elapsed)) => elapsed,
        Ok(None) => return HandlerProbe::Gone,
        Err(e) => return HandlerProbe::Unknown(e),
    };
    // `ps` truncates the elapsed time, so this is never earlier than the real start.
    let started_at = chrono::Utc::now() - elapsed;
    if started_at < running_since - chrono::Duration::seconds(1) {
        return HandlerProbe::Reused(format!(
            "pid {pid} started at {started_at}, before its item entered Running at {running_since}"
        ));
    }
    HandlerProbe::Handler
}

/// Time since `pid` started, from `ps -o etime=`; `None` when no such process.
fn process_elapsed(pid: u32) -> Result<Option<chrono::Duration>, String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("failed to run ps: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    parse_etime(text)
        .map(Some)
        .ok_or_else(|| format!("unreadable ps elapsed time '{text}'"))
}

/// Parse the `[[dd-]hh:]mm:ss` format of `ps -o etime`.
fn parse_etime(text: &str) -> Option<chrono::Duration> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<i64>().ok()?, clock),
        None => (0, text),
    };
    let parts = clock
        .split(':')
        .map(|p| p.parse::<i64>().ok())
        .collect::<Option<Vec<_>>>()?;
    let (hours, minutes, seconds) = match parts.as_slice() {
        [m, s] => (0, *m, *s),
        [h, m, s] => (*h, *m, *s),
        _ => return None,
    };
    Some(chrono::Duration::seconds(
        ((days * 24 + hours) * 60 + minutes) * 60 + seconds,
    ))
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
        assert_eq!(
            sink.exits(),
            sink.pids(),
            "the exit is reported once waited for"
        );
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
    fn parse_etime_reads_every_ps_format() {
        assert_eq!(parse_etime("00:07"), Some(chrono::Duration::seconds(7)));
        assert_eq!(
            parse_etime("01:02:03"),
            Some(chrono::Duration::seconds(3723))
        );
        assert_eq!(
            parse_etime("2-01:02:03"),
            Some(chrono::Duration::seconds(2 * 86_400 + 3723))
        );
        assert_eq!(parse_etime("garbage"), None);
    }

    #[test]
    fn probe_tells_a_handler_from_a_gone_or_reused_pid() {
        use super::super::HandlerProbe;
        use std::os::unix::process::CommandExt;

        let before = chrono::Utc::now() - chrono::Duration::seconds(5);
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();

        assert_eq!(probe_handler(pid, before), HandlerProbe::Handler);
        let later = chrono::Utc::now() + chrono::Duration::seconds(30);
        assert!(
            matches!(probe_handler(pid, later), HandlerProbe::Reused(_)),
            "a process older than the Running entry is not the handler"
        );
        let own = std::process::id();
        if unsafe { libc::getpgid(0) } != own as libc::pid_t {
            assert!(matches!(
                probe_handler(own, before),
                HandlerProbe::Reused(_)
            ));
        }

        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(probe_handler(pid, before), HandlerProbe::Gone);
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
