//! Platform-specific implementations and factory functions.
//!
//! This module provides [`default_shell_executor`] and [`default_daemon_notifier`]
//! factory functions that return the appropriate implementation for the current platform.

#[cfg(unix)]
pub mod unix;
pub mod windows;

use belt_core::platform::{DaemonNotifier, ShellExecutor};

/// Return the default [`ShellExecutor`] for the current platform.
///
/// - **Unix**: returns [`unix::UnixShellExecutor`] (uses `sh -c`)
/// - **Windows**: returns [`windows::WindowsShellExecutor`] (uses `cmd.exe /C`)
pub fn default_shell_executor() -> Box<dyn ShellExecutor> {
    #[cfg(unix)]
    {
        Box::new(unix::UnixShellExecutor::new())
    }
    #[cfg(windows)]
    {
        Box::new(windows::WindowsShellExecutor::new())
    }
}

/// Return the default [`DaemonNotifier`] for the current platform.
///
/// - **Unix**: returns [`unix::UnixDaemonNotifier`] (uses SIGUSR1)
/// - **Windows**: returns [`windows::WindowsDaemonNotifier`] (not yet supported, returns error)
pub fn default_daemon_notifier() -> Box<dyn DaemonNotifier> {
    #[cfg(unix)]
    {
        Box::new(unix::UnixDaemonNotifier::new())
    }
    #[cfg(windows)]
    {
        Box::new(windows::WindowsDaemonNotifier::new())
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
    #[tokio::test]
    async fn default_daemon_notifier_rejects_invalid_pid() {
        let notifier = default_daemon_notifier();
        let result = notifier.notify(999_999_999).await;
        assert!(result.is_err());
    }
}
