//! Per-test sandbox: temp HOME / BELT_HOME / target git repo, fake `gh` and
//! `claude` first on PATH, and a daemon guard that never leaves a process
//! behind.
//!
//! Layout under one temp root:
//!
//! ```text
//! bin/       gh, claude -> this test binary; belt -> the built belt binary
//! fake/      fake gh state + call logs, fake claude control + call log
//! home/      HOME (keeps ~/.gitconfig, ~/.belt and gh config out of reach)
//! belt/      BELT_HOME (belt.db, worktrees/, daemon.pid)
//! repo/      target git repository; the daemon runs here (worktrees need it)
//! tmp/       TMPDIR
//! ```

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

use crate::fake_claude::FakeClaude;
use crate::fake_gh::FakeGitHub;

/// Env var through which the fakes find their state directory.
pub const FAKE_DIR_ENV: &str = "BELT_E2E_FAKE_DIR";

/// Repository every scenario's GitHub source points at.
pub const REPO: &str = "acme/widgets";

const DAEMON_LOG: &str = "daemon.log";

pub struct E2eEnv {
    // Kept for its Drop: removes the whole sandbox.
    _root: TempDir,
    root: PathBuf,
    pub bin_dir: PathBuf,
    pub fake_dir: PathBuf,
    pub home: PathBuf,
    pub belt_home: PathBuf,
    pub repo_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub gh: FakeGitHub,
    pub claude: FakeClaude,
}

impl E2eEnv {
    pub fn new() -> Self {
        // Short prefix: the daemon's unix socket lives under BELT_HOME and
        // socket paths are limited to ~104 bytes on macOS.
        let root_dir = tempfile::Builder::new()
            .prefix("be2e")
            .tempdir()
            .expect("create sandbox");
        // Canonical path: macOS /var -> /private/var would otherwise make
        // paths reported by git differ from ours.
        let root = root_dir.path().canonicalize().unwrap();
        let sub = |name: &str| {
            let p = root.join(name);
            std::fs::create_dir_all(&p).unwrap();
            p
        };
        let env = Self {
            bin_dir: sub("bin"),
            fake_dir: sub("fake"),
            home: sub("home"),
            belt_home: sub("belt"),
            repo_dir: sub("repo"),
            tmp_dir: sub("tmp"),
            gh: FakeGitHub::init(&root.join("fake"), REPO),
            claude: FakeClaude::open(&root.join("fake")),
            root,
            _root: root_dir,
        };
        env.install_fakes();
        env.init_repo();
        env
    }

    fn install_fakes(&self) {
        let me = std::env::current_exe().expect("current_exe");
        for name in ["gh", "claude"] {
            std::os::unix::fs::symlink(&me, self.bin_dir.join(name)).unwrap();
        }
        // The evaluator falls back to spawning `belt` from PATH; it must be
        // the binary under test, never an installed one.
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_belt"), self.bin_dir.join("belt")).unwrap();
    }

    fn init_repo(&self) {
        std::fs::write(self.repo_dir.join("README.md"), "fake target repo\n").unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["add", "README.md"],
            &[
                "-c",
                "user.name=belt-e2e",
                "-c",
                "user.email=e2e@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        ] {
            let out = self.command("git").args(args).output().unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// A command running inside the sandbox: fakes first on PATH, isolated
    /// HOME/BELT_HOME/TMPDIR, cwd = target repo.
    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.bin_dir.clone()];
        paths.extend(std::env::split_paths(&path));
        let mut cmd = Command::new(program);
        cmd.current_dir(&self.repo_dir)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("HOME", &self.home)
            .env("BELT_HOME", &self.belt_home)
            .env("TMPDIR", &self.tmp_dir)
            .env(FAKE_DIR_ENV, &self.fake_dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("RUST_LOG", "info")
            .env("NO_COLOR", "1")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN");
        cmd
    }

    /// Write the workspace config; returns its path.
    pub fn write_workspace(&self, yaml: &str) -> PathBuf {
        let path = self.root.join("workspace.yaml");
        std::fs::write(&path, yaml).unwrap();
        path
    }

    /// Run a `belt` CLI command and return its output.
    pub fn belt(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_belt"))
            .args(args)
            .output()
            .expect("run belt")
    }

    /// Run a `belt` CLI command that must succeed and print JSON.
    pub fn belt_json(&self, args: &[&str]) -> Value {
        let out = self.belt(args);
        assert!(
            out.status.success(),
            "belt {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "belt {args:?} printed non-JSON ({e}): {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    /// `belt queue list --format json`.
    pub fn queue_items(&self) -> Vec<Value> {
        match self.belt_json(&["queue", "list", "--format", "json"]) {
            Value::Array(items) => items,
            other => panic!("queue list is not an array: {other}"),
        }
    }

    /// Phase of `work_id` per `belt queue list`, if the item exists.
    pub fn phase_of(&self, work_id: &str) -> Option<String> {
        self.queue_items()
            .into_iter()
            .find(|i| i["work_id"] == work_id)
            .and_then(|i| i["phase"].as_str().map(str::to_string))
    }

    /// Entries in `$BELT_HOME/worktrees`.
    pub fn worktree_dirs(&self) -> Vec<String> {
        match std::fs::read_dir(self.belt_home.join("worktrees")) {
            Ok(rd) => rd
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Worktree paths git knows for the target repo (main checkout included).
    pub fn git_worktrees(&self) -> Vec<String> {
        let out = self
            .command("git")
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix("worktree "))
            .map(str::to_string)
            .collect()
    }

    /// Local branch names of the target repo.
    pub fn git_branches(&self) -> Vec<String> {
        let out = self
            .command("git")
            .args(["branch", "--format=%(refname:short)"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Daemon stdout+stderr so far (usable while a `DaemonGuard` is borrowed).
    pub fn daemon_log(&self) -> String {
        std::fs::read_to_string(self.root.join(DAEMON_LOG)).unwrap_or_default()
    }

    /// Start `belt start --config <workspace> --tick <tick_secs>`.
    pub fn start_daemon(&self, workspace: &Path, tick_secs: u64) -> DaemonGuard {
        let log_path = self.root.join(DAEMON_LOG);
        let log = std::fs::File::create(&log_path).unwrap();
        let child = self
            .command(env!("CARGO_BIN_EXE_belt"))
            .args(["start", "--config"])
            .arg(workspace)
            .args(["--tick", &tick_secs.to_string()])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("spawn belt daemon");
        DaemonGuard {
            child: Some(child),
            log_path,
        }
    }
}

/// Owns a running daemon. Dropping it stops the daemon (SIGINT, then
/// SIGKILL after a grace period), so a failing test leaves no process.
pub struct DaemonGuard {
    child: Option<Child>,
    log_path: PathBuf,
}

impl DaemonGuard {
    /// Daemon stdout+stderr so far.
    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    /// Fail fast when the daemon died, with its log in the message.
    pub fn assert_alive(&mut self) {
        let child = self.child.as_mut().expect("daemon already stopped");
        if let Some(status) = child.try_wait().unwrap() {
            panic!("daemon exited early ({status}); log:\n{}", self.log());
        }
    }

    /// Graceful stop: SIGINT — the signal the daemon drains on — then wait
    /// up to `grace` and SIGKILL. Returns whether it exited within `grace`.
    pub fn stop(&mut self, grace: Duration) -> bool {
        if let Some(child) = &self.child {
            let _ = Command::new("kill")
                .args(["-INT", &child.id().to_string()])
                .status();
        }
        self.wait_exit(grace)
    }

    /// Wait up to `timeout` for the daemon to exit on its own (e.g. after
    /// `belt stop`), then SIGKILL. Returns whether it exited in time.
    pub fn wait_exit(&mut self, timeout: Duration) -> bool {
        let Some(mut child) = self.child.take() else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        false
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        self.stop(Duration::from_secs(5));
        if std::thread::panicking() {
            eprintln!("---- daemon log ----\n{}\n--------------------", self.log());
        }
    }
}

/// Poll `probe` every 100ms until it yields a value; panic after `timeout`
/// with `what` and the last probe description.
pub fn wait_until<T>(
    timeout: Duration,
    what: &str,
    daemon: &mut DaemonGuard,
    mut probe: impl FnMut() -> Option<T>,
) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = probe() {
            return v;
        }
        daemon.assert_alive();
        assert!(
            Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
