//! Cross-platform IPC for daemon process communication.
//!
//! On Unix, the daemon uses SIGUSR1 for notifications. Windows lacks Unix
//! signals, so this module provides a lightweight TCP-loopback-based IPC
//! mechanism that works on **all** platforms.
//!
//! ## Protocol
//!
//! 1. The daemon binds a TCP listener on `127.0.0.1:0` (OS-assigned port).
//! 2. The assigned port is written to `$BELT_HOME/daemon.ipc`.
//! 3. A client (e.g. `belt cron trigger`) reads the port file and sends a
//!    single-line JSON command: `{"signal":"CronSync"}\n`.
//! 4. The daemon reads the command, acts on it, and closes the connection.
//!
//! The IPC file is removed when the listener is dropped.

use std::path::{Path, PathBuf};

use tokio::io::AsyncBufReadExt;
use tokio::net::TcpListener;

/// Signals that can be sent to the daemon via IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "signal")]
pub enum DaemonSignal {
    /// Request the daemon to synchronize cron jobs from the database and
    /// perform an immediate tick.
    CronSync,
}

/// A TCP-based IPC listener that the daemon uses to receive cross-platform
/// signals from CLI commands.
pub struct IpcListener {
    listener: TcpListener,
    ipc_path: PathBuf,
}

impl IpcListener {
    /// Bind a TCP listener on the loopback address and write the port to
    /// `<belt_home>/daemon.ipc`.
    ///
    /// # Errors
    ///
    /// Returns an error if binding or writing the port file fails.
    pub async fn bind(belt_home: &Path) -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let ipc_path = belt_home.join("daemon.ipc");
        std::fs::write(&ipc_path, port.to_string())?;
        tracing::info!(port, path = %ipc_path.display(), "IPC listener started");
        Ok(Self { listener, ipc_path })
    }

    /// Accept one incoming connection, read the signal, and return it.
    ///
    /// This is cancel-safe and designed to be used inside `tokio::select!`.
    pub async fn recv(&self) -> Option<DaemonSignal> {
        let (stream, _addr) = match self.listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("IPC accept error: {e}");
                return None;
            }
        };
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                tracing::debug!("IPC connection closed without data");
                None
            }
            Ok(_) => match serde_json::from_str::<DaemonSignal>(line.trim()) {
                Ok(signal) => {
                    tracing::debug!(?signal, "received IPC signal");
                    Some(signal)
                }
                Err(e) => {
                    tracing::warn!("invalid IPC message: {e}");
                    None
                }
            },
            Err(e) => {
                tracing::warn!("IPC read error: {e}");
                None
            }
        }
    }
}

impl Drop for IpcListener {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.ipc_path) {
            tracing::debug!("failed to remove IPC file: {e}");
        }
    }
}

/// Send a [`DaemonSignal`] to the running daemon.
///
/// Reads the IPC port from `<belt_home>/daemon.ipc` and sends the signal
/// over a TCP connection to the loopback address.
///
/// # Errors
///
/// Returns an error if the port file is missing, the daemon is not
/// listening, or the write fails.
pub fn notify_daemon(belt_home: &Path, signal: DaemonSignal) -> anyhow::Result<()> {
    let ipc_path = belt_home.join("daemon.ipc");
    let port_str = std::fs::read_to_string(&ipc_path).map_err(|e| {
        anyhow::anyhow!(
            "could not read IPC port file at {}: {} (is the daemon running?)",
            ipc_path.display(),
            e
        )
    })?;
    let port: u16 = port_str
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid port in {}: {}", ipc_path.display(), e))?;

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
    let msg = serde_json::to_string(&signal)? + "\n";
    std::io::Write::write_all(&mut stream, msg.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn ipc_round_trip() {
        let tmp = TempDir::new().unwrap();
        let listener = IpcListener::bind(tmp.path()).await.unwrap();

        // Verify port file was written.
        let ipc_path = tmp.path().join("daemon.ipc");
        assert!(ipc_path.exists());

        // Send a signal from another task.
        let home = tmp.path().to_path_buf();
        let sender = tokio::task::spawn_blocking(move || {
            notify_daemon(&home, DaemonSignal::CronSync).unwrap();
        });

        let signal = listener.recv().await;
        assert_eq!(signal, Some(DaemonSignal::CronSync));
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn ipc_file_cleaned_up_on_drop() {
        let tmp = TempDir::new().unwrap();
        let ipc_path = tmp.path().join("daemon.ipc");
        {
            let _listener = IpcListener::bind(tmp.path()).await.unwrap();
            assert!(ipc_path.exists());
        }
        assert!(!ipc_path.exists());
    }

    #[test]
    fn notify_daemon_missing_file() {
        let tmp = TempDir::new().unwrap();
        let result = notify_daemon(tmp.path(), DaemonSignal::CronSync);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("could not read IPC port file"),
        );
    }

    #[test]
    fn daemon_signal_serde_cron_sync() {
        let signal = DaemonSignal::CronSync;
        let json = serde_json::to_string(&signal).unwrap();
        // Verify the internally-tagged representation.
        assert_eq!(json, r#"{"signal":"CronSync"}"#);
        // Round-trip back to the original variant.
        let deserialized: DaemonSignal = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, DaemonSignal::CronSync);
    }

    #[tokio::test]
    async fn recv_returns_signal_on_valid_connection() {
        let tmp = TempDir::new().unwrap();
        let listener = IpcListener::bind(tmp.path()).await.unwrap();

        // Spawn a blocking sender that writes via notify_daemon.
        let home = tmp.path().to_path_buf();
        let sender = tokio::task::spawn_blocking(move || {
            notify_daemon(&home, DaemonSignal::CronSync).unwrap();
        });

        let signal = listener.recv().await;
        assert!(signal.is_some(), "recv must return Some on valid message");
        assert_eq!(signal.unwrap(), DaemonSignal::CronSync);
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn notify_daemon_success_writes_and_receives() {
        let tmp = TempDir::new().unwrap();
        let listener = IpcListener::bind(tmp.path()).await.unwrap();

        // Verify the port file content matches the listener's actual port.
        let ipc_path = tmp.path().join("daemon.ipc");
        let port_str = std::fs::read_to_string(&ipc_path).unwrap();
        let port: u16 = port_str.trim().parse().unwrap();
        assert_eq!(port, listener.listener.local_addr().unwrap().port());

        // notify_daemon should succeed and the listener should receive the signal.
        let home = tmp.path().to_path_buf();
        let sender = tokio::task::spawn_blocking(move || {
            let result = notify_daemon(&home, DaemonSignal::CronSync);
            assert!(result.is_ok(), "notify_daemon should succeed: {result:?}");
        });

        let signal = listener.recv().await;
        assert_eq!(signal, Some(DaemonSignal::CronSync));
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn notify_daemon_multiple_signals_in_order() {
        let tmp = TempDir::new().unwrap();
        let listener = IpcListener::bind(tmp.path()).await.unwrap();

        let home = tmp.path().to_path_buf();

        // Send 3 signals sequentially from a blocking task.
        let sender = tokio::task::spawn_blocking(move || {
            for _ in 0..3 {
                notify_daemon(&home, DaemonSignal::CronSync).unwrap();
            }
        });

        // Verify all 3 signals arrive in order.
        for i in 0..3 {
            let signal = listener.recv().await;
            assert_eq!(
                signal,
                Some(DaemonSignal::CronSync),
                "signal {i} should be CronSync",
            );
        }

        sender.await.unwrap();
    }
}
