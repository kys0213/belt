//! Platform-specific implementations of [`ShellExecutor`] and [`DaemonNotifier`].
//!
//! [`ShellExecutor`]: belt_core::platform::ShellExecutor
//! [`DaemonNotifier`]: belt_core::platform::DaemonNotifier

#[cfg(unix)]
pub mod unix;

#[cfg(windows)]
pub mod windows;

use belt_core::platform::{DaemonNotifier, ShellExecutor};

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
    #[test]
    fn default_shell_executor_can_run_command() {
        let executor = default_shell_executor();
        let tmp = tempfile::tempdir().unwrap();
        let env = std::collections::HashMap::new();

        let output = executor
            .execute("echo factory_test", tmp.path(), &env)
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
