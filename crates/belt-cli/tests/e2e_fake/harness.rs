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

/// `$USER` inside the sandbox: the CLI records it as the requester of a
/// cancel and the respondent of a HITL answer.
pub const OS_USER: &str = "e2e-operator";

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
            .env("USER", OS_USER)
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
            paused: false,
            log_path,
            belt_home: self.belt_home.clone(),
            handler_pid_dir: self.handler_pid_dir(),
            sandbox: self.root.clone(),
        }
    }

    /// Directory where scenario handlers register their process group leader
    /// pid (`$$`; one file per pid, any name). Register leaders only: on drop
    /// [`DaemonGuard`] kills each registered group that still runs inside
    /// this sandbox, so a handler that outlives its daemon never leaks.
    pub fn handler_pid_dir(&self) -> PathBuf {
        let dir = self.fake_dir.join("handler_pids");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `belt queue show <work_id> --json`.
    pub fn queue_show(&self, work_id: &str) -> Value {
        self.belt_json(&["queue", "show", work_id, "--json"])
    }

    /// Transition rows of `work_id`, oldest first.
    pub fn transitions(&self, work_id: &str) -> Vec<Value> {
        match &self.queue_show(work_id)["transitions"] {
            Value::Array(rows) => rows.clone(),
            other => panic!("queue show transitions is not an array: {other}"),
        }
    }

    /// The queue item `work_id` per `belt queue list`, if it exists.
    pub fn item(&self, work_id: &str) -> Option<Value> {
        self.queue_items()
            .into_iter()
            .find(|i| i["work_id"] == work_id)
    }

    /// Run a `belt` CLI command whose JSON answer is read regardless of the
    /// exit code (refusals exit non-zero). Returns `(exit code, JSON)`.
    pub fn belt_answer(&self, args: &[&str]) -> (i32, Value) {
        let out = self.belt(args);
        let json = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "belt {args:?} printed non-JSON ({e}): stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code().unwrap_or(-1), json)
    }

    /// Read-only connection to `$BELT_HOME/belt.db`, for state no CLI shows.
    pub fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open_with_flags(
            self.belt_home.join("belt.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open belt.db")
    }
}

/// Whether `pid` no longer names a live process. A zombie counts as gone:
/// it has exited and only waits for its parent to reap it.
pub fn process_gone(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    stat.is_empty() || stat.starts_with('Z')
}

/// The pid a handler wrote to `path`.
pub fn read_pid(path: &Path) -> u32 {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("pid file {} unreadable: {e}", path.display()));
    text.trim()
        .parse()
        .unwrap_or_else(|e| panic!("pid file {} holds {text:?}: {e}", path.display()))
}

/// Owns a running daemon. Dropping it stops the daemon (SIGINT, then
/// SIGKILL after a grace period), so a failing test leaves no process.
pub struct DaemonGuard {
    child: Option<Child>,
    /// SIGSTOPped by [`DaemonGuard::pause`] and not resumed yet.
    paused: bool,
    log_path: PathBuf,
    belt_home: PathBuf,
    handler_pid_dir: PathBuf,
    /// Sandbox root: only process groups running inside it are killed.
    sandbox: PathBuf,
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
        self.signal("-INT");
        self.wait_exit(grace)
    }

    /// Simulate an unresponsive daemon: SIGSTOP it. Its pid stays alive
    /// (`kill -0` succeeds) but it handles nothing until
    /// [`DaemonGuard::resume`]; dropping the guard resumes it first, so a
    /// panic never leaves it stopped.
    pub fn pause(&mut self) {
        self.signal("-STOP");
        self.paused = true;
    }

    /// SIGCONT a daemon stopped by [`DaemonGuard::pause`].
    pub fn resume(&mut self) {
        self.signal("-CONT");
        self.paused = false;
    }

    fn signal(&self, sig: &str) {
        if let Some(child) = &self.child {
            let _ = Command::new("kill")
                .args([sig, &child.id().to_string()])
                .stderr(Stdio::null())
                .status();
        }
    }

    /// Simulate a crash: SIGKILL the daemon with no drain. Its handler
    /// process groups and `daemon.pid` survive, as after a real crash.
    pub fn kill_hard(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Pids of every handler that may still run: the ones the store records
    /// for Running items and the ones scenario handlers registered.
    fn handler_pids(&self) -> Vec<u32> {
        let mut pids = Vec::new();
        if let Ok(conn) = rusqlite::Connection::open_with_flags(
            self.belt_home.join("belt.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) && let Ok(mut stmt) =
            conn.prepare("SELECT handler_pid FROM queue_items WHERE handler_pid IS NOT NULL")
            && let Ok(rows) = stmt.query_map([], |r| r.get::<_, i64>(0))
        {
            pids.extend(rows.flatten().filter_map(|p| u32::try_from(p).ok()));
        }
        if let Ok(rd) = std::fs::read_dir(&self.handler_pid_dir) {
            pids.extend(rd.flatten().filter_map(|entry| {
                std::fs::read_to_string(entry.path())
                    .ok()
                    .and_then(|t| t.trim().parse::<u32>().ok())
            }));
        }
        pids.sort_unstable();
        pids.dedup();
        pids
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
        if self.paused {
            self.resume();
        }
        // Snapshot before the stop: a drain rolls Running items back and
        // clears their recorded pid while the handler may live on.
        let handlers = self.handler_pids();
        self.stop(Duration::from_secs(5));
        // Handlers lead their own process group (pgid == pid), so the
        // daemon's death does not reach them; kill each group explicitly.
        for pgid in handlers {
            kill_group_in_sandbox(pgid, &self.sandbox);
        }
        if std::thread::panicking() {
            eprintln!("---- daemon log ----\n{}\n--------------------", self.log());
        }
    }
}

/// Poll `probe` every 100ms until it yields a value; panic after `timeout`
/// with `what`. For phases without a running daemon (it crashed, was
/// stopped, or never started); [`wait_until`] while one runs.
pub fn wait_for<T>(timeout: Duration, what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// [`wait_for`] that also fails fast, with the daemon log, once the daemon
/// has exited.
pub fn wait_until<T>(
    timeout: Duration,
    what: &str,
    daemon: &mut DaemonGuard,
    mut probe: impl FnMut() -> Option<T>,
) -> T {
    wait_for(timeout, what, || {
        let found = probe();
        if found.is_none() {
            daemon.assert_alive();
        }
        found
    })
}

/// SIGKILL the process group `pgid`, but only when one of its live members
/// has its working directory inside `sandbox`.
///
/// A recorded pid may name an unrelated process by now (pid reuse). A
/// group id is not handed out again while any member of the group lives,
/// so a member running inside this sandbox (handlers run in their worktree
/// under BELT_HOME) proves the group is ours. A group that cannot be
/// verified is left alone: a leaked test process beats killing someone
/// else's.
fn kill_group_in_sandbox(pgid: u32, sandbox: &Path) {
    if pgid <= 1 {
        return;
    }
    let ours = group_members(pgid)
        .into_iter()
        .any(|pid| process_cwd(pid).is_some_and(|cwd| cwd.starts_with(sandbox)));
    if ours {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{pgid}")])
            .stderr(Stdio::null())
            .status();
    }
}

/// Live pids whose process group is `pgid`.
fn group_members(pgid: u32) -> Vec<u32> {
    let Ok(out) = Command::new("ps").args(["-A", "-o", "pid=,pgid="]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace().map(|c| c.parse::<u32>().ok());
            match (cols.next().flatten(), cols.next().flatten()) {
                (Some(pid), Some(group)) if group == pgid => Some(pid),
                _ => None,
            }
        })
        .collect()
}

/// Working directory of `pid`, if it can be read.
#[cfg(target_os = "linux")]
fn process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// Working directory of `pid`, if it can be read (`lsof -Fn` prints it on
/// the line starting with `n`).
#[cfg(not(target_os = "linux"))]
fn process_cwd(pid: u32) -> Option<PathBuf> {
    let out = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n'))
        .map(PathBuf::from)
}
