//! Platform-specific implementations of [`ShellExecutor`] and [`DaemonNotifier`].
//!
//! [`ShellExecutor`]: belt_core::platform::ShellExecutor
//! [`DaemonNotifier`]: belt_core::platform::DaemonNotifier

#[cfg(unix)]
pub mod unix;

#[cfg(windows)]
pub mod windows;

use std::process::{Output, Stdio};

use belt_core::platform::{DaemonNotifier, ProcessKiller, ProcessSink, ShellExecutor};
use tokio::process::Command;

/// Returns the platform-appropriate [`ShellExecutor`].
///
/// - **Unix**: returns [`unix::UnixShellExecutor`] (uses `bash -c`)
/// - **Windows**: returns [`windows::WindowsShellExecutor`] (uses `cmd.exe /C`)
pub fn default_shell_executor() -> Box<dyn ShellExecutor> {
    #[cfg(unix)]
    {
        Box::new(unix::UnixShellExecutor)
    }
    #[cfg(windows)]
    {
        Box::new(windows::WindowsShellExecutor)
    }
}

/// Returns the platform-appropriate [`DaemonNotifier`].
///
/// - **Unix**: returns [`unix::UnixDaemonNotifier`] (uses SIGUSR1)
/// - **Windows**: returns [`windows::WindowsDaemonNotifier`] (uses named pipe)
pub fn default_daemon_notifier() -> Box<dyn DaemonNotifier> {
    #[cfg(unix)]
    {
        Box::new(unix::UnixDaemonNotifier)
    }
    #[cfg(windows)]
    {
        Box::new(windows::WindowsDaemonNotifier)
    }
}

/// Returns the platform-appropriate [`ProcessKiller`].
///
/// - **Unix**: returns [`unix::UnixProcessKiller`] (uses `killpg`)
/// - **Windows**: returns [`windows::WindowsProcessKiller`] (uses `taskkill /T /F`)
pub fn default_process_killer() -> Box<dyn ProcessKiller> {
    #[cfg(unix)]
    {
        Box::new(unix::UnixProcessKiller)
    }
    #[cfg(windows)]
    {
        Box::new(windows::WindowsProcessKiller)
    }
}

/// Spawns `cmd` as the leader of a new process group, reports its pid to
/// `sink` right after the spawn, and collects its output.
///
/// The dedicated group lets [`ProcessKiller::kill_group`] terminate the
/// process together with its children. Every handler spawn goes through this
/// function so that the pid is reported exactly once.
pub(crate) async fn output_in_new_group(
    cmd: &mut Command,
    sink: &dyn ProcessSink,
) -> std::io::Result<Output> {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn()?;
    if let Some(pid) = child.id() {
        sink.spawned(pid);
    }
    child.wait_with_output().await
}

/// Test doubles shared by the platform and runtime tests.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Mutex;

    use belt_core::platform::ProcessSink;

    /// Records every reported pid.
    #[derive(Default)]
    pub struct RecordingSink {
        pids: Mutex<Vec<u32>>,
    }

    impl RecordingSink {
        pub fn pids(&self) -> Vec<u32> {
            self.pids.lock().unwrap().clone()
        }
    }

    impl ProcessSink for RecordingSink {
        fn spawned(&self, pid: u32) {
            self.pids.lock().unwrap().push(pid);
        }
    }

    /// Writes an executable stand-in for an LLM CLI that prints `{}` and exits.
    #[cfg(unix)]
    pub fn fake_cli(dir: &std::path::Path) -> String {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("fake-cli");
        std::fs::write(&path, "#!/bin/sh\necho '{}'\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shell_executor_returns_trait_object() {
        let executor = default_shell_executor();
        // Verify the factory returns a valid trait object.
        // We cannot downcast without Any, but we can confirm it's constructed.
        let _ = executor;
    }

    #[test]
    fn default_daemon_notifier_returns_trait_object() {
        let notifier = default_daemon_notifier();
        let _ = notifier;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn default_shell_executor_can_run_command() {
        let executor = default_shell_executor();
        let tmp = tempfile::tempdir().unwrap();
        let env = std::collections::HashMap::new();

        let output = executor
            .execute("echo factory_test", tmp.path(), &env)
            .await
            .unwrap();
        assert!(output.success());
        assert!(output.stdout.contains("factory_test"));
    }

    #[cfg(unix)]
    #[test]
    fn default_daemon_notifier_rejects_invalid_pid() {
        let notifier = default_daemon_notifier();
        let result = notifier.notify(999_999_999);
        assert!(result.is_err());
    }
}
