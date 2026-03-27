//! Windows implementations of [`ShellExecutor`] and [`DaemonNotifier`].
//!
//! Shell commands are executed via `cmd.exe /C`. Daemon notifications use
//! a named pipe at `\\.\pipe\belt-daemon-{pid}`.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use belt_core::error::BeltError;
use belt_core::platform::{DaemonNotifier, ShellExecutor, ShellOutput};

/// Executes shell commands via `cmd.exe /C` on Windows systems.
#[derive(Debug, Default, Clone)]
pub struct WindowsShellExecutor;

impl ShellExecutor for WindowsShellExecutor {
    fn execute(
        &self,
        command: &str,
        working_dir: &Path,
        env_vars: &HashMap<String, String>,
    ) -> Result<ShellOutput, BeltError> {
        let output = Command::new("cmd.exe")
            .arg("/C")
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

/// Sends a wake-up notification to a daemon process via a named pipe on Windows.
///
/// The daemon is expected to listen on `\\.\pipe\belt-daemon-{pid}`.
#[derive(Debug, Default, Clone)]
pub struct WindowsDaemonNotifier;

impl DaemonNotifier for WindowsDaemonNotifier {
    fn notify(&self, pid: u32) -> Result<(), BeltError> {
        let pipe_name = format!(r"\\.\pipe\belt-daemon-{pid}");

        // Attempt to open the named pipe and write a wake-up byte.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&pipe_name)
            .map_err(|e| {
                BeltError::Runtime(format!(
                    "failed to open named pipe '{pipe_name}' for daemon pid {pid}: {e}"
                ))
            })?;

        use std::io::Write;
        file.write_all(b"wake").map_err(|e| {
            BeltError::Runtime(format!("failed to write to named pipe '{pipe_name}': {e}"))
        })?;

        Ok(())
    }
}
