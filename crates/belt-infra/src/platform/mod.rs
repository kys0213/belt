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
