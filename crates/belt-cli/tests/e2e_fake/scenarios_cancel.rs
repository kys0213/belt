//! Cancel scenarios: `belt queue skip` on a Running item, through a live
//! daemon (4) and directly when the daemon is gone or unresponsive (5),
//! plus the skip of an item that already left Running or never started.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::harness::{DaemonGuard, E2eEnv, OS_USER, process_gone, read_pid, wait_for, wait_until};
use crate::support::{
    TRIGGER, Workspace, first_work_id, on_fail_runs, phase_enters, progress_comments_any, rows_of,
    worktree_name,
};

/// Pid files of [`HandlerPids::sleeping_handler`]. The group leader goes to
/// the harness registry, so the guard kills the group even if the scenario
/// fails before the cancel; the child only goes to `tmp/` for assertions
/// (the registry takes group leaders only).
struct HandlerPids {
    leader: PathBuf,
    child: PathBuf,
}

impl HandlerPids {
    fn of(env: &E2eEnv) -> Self {
        Self {
            leader: env.handler_pid_dir().join("leader"),
            child: env.tmp_dir.join("child"),
        }
    }

    /// Handler that records its group leader and a sleeping child, then blocks.
    fn sleeping_handler(&self) -> String {
        format!(
            r#"echo $$ > "{}"; sleep 300 & echo $! > "{}"; wait"#,
            self.leader.display(),
            self.child.display()
        )
    }

    /// Both pids are written.
    fn started(&self) -> bool {
        self.child.exists()
    }

    /// `(leader, child)`.
    fn read(&self) -> (u32, u32) {
        (read_pid(&self.leader), read_pid(&self.child))
    }
}

struct Running {
    // Declared first so it drops first: the daemon must stop before the
    // sandbox it runs in is removed.
    daemon: DaemonGuard,
    env: E2eEnv,
    issue: u64,
    work_id: String,
    leader: u32,
    child: u32,
    on_fail_log: PathBuf,
    config: PathBuf,
}

/// A daemon with one item whose sleeping handler is running.
fn running_item(tick_secs: u64) -> Running {
    let env = E2eEnv::new();
    let issue = env.gh.seed_issue("Analyze", "body", &[TRIGGER]);
    let work_id = first_work_id(issue);
    let pids = HandlerPids::of(&env);
    let on_fail_log = env.tmp_dir.join("on_fail.log");
    let mut ws = Workspace::new(pids.sleeping_handler());
    ws.on_fail_log = Some(on_fail_log.clone());
    let config = ws.install(&env);

    let mut daemon = env.start_daemon(&config, tick_secs);
    wait_until(
        Duration::from_secs(20),
        "handler to be running",
        &mut daemon,
        || (pids.started() && env.queue_show(&work_id)["processing"] == "handler").then_some(()),
    );
    let (leader, child) = pids.read();
    assert!(!process_gone(leader) && !process_gone(child));
    Running {
        daemon,
        env,
        issue,
        work_id,
        leader,
        child,
        on_fail_log,
        config,
    }
}

fn wait_processes_gone(pids: &[u32]) {
    wait_for(
        Duration::from_secs(5),
        "handler process group to die",
        || pids.iter().all(|p| process_gone(*p)).then_some(()),
    );
}

/// Statuses `belt context --json` reports in the attempt history of
/// `work_id`. The history spans the whole source, so other items' rows are
/// filtered out.
fn history_statuses(env: &E2eEnv, work_id: &str) -> Vec<String> {
    let context = env.belt_json(&["context", work_id, "--json"]);
    context["history"]
        .as_array()
        .unwrap_or_else(|| panic!("context has no history array: {context:#}"))
        .iter()
        .filter(|h| h["work_id"] == work_id)
        .map(|h| h["status"].as_str().unwrap().to_string())
        .collect()
}

/// Scenario 4: skipping a Running item through a live daemon kills the
/// handler's process group and ends as `canceled`, without waiting for the
/// next tick.
pub fn skip_running_item_cancels_through_daemon_and_kills_handler_group() {
    // A 30s tick: the cancel must be handled on the IPC wake, not a tick.
    let mut r = running_item(30);

    let started = Instant::now();
    let (code, answer) = r.env.belt_answer(&["queue", "skip", &r.work_id, "--json"]);
    let took = started.elapsed();
    assert_eq!(code, 0, "{answer}");
    assert_eq!(answer["success"], true, "{answer}");
    assert_eq!(answer["result"], "canceled", "{answer}");
    assert_eq!(answer["phase"], "skipped", "{answer}");
    assert_eq!(answer["work_id"], r.work_id.as_str(), "{answer}");
    assert!(took < Duration::from_secs(10), "cancel took {took:?}");

    wait_processes_gone(&[r.leader, r.child]);
    assert_eq!(r.env.phase_of(&r.work_id).as_deref(), Some("skipped"));

    let log = r.env.transitions(&r.work_id);
    let kinds: Vec<&str> = log
        .iter()
        .filter_map(|t| t["kind"].as_str())
        .filter(|k| k.starts_with("cancel_") || *k == "phase_enter")
        .collect();
    let tail: Vec<&str> = kinds.iter().rev().take(4).rev().copied().collect();
    assert_eq!(
        tail,
        [
            "cancel_requested",
            "cancel_accepted",
            "phase_enter",
            "cancel_closed"
        ],
        "{log:#?}"
    );
    let requested = rows_of(&log, "cancel_requested")[0];
    assert_eq!(requested["actor"], "cli", "{requested}");
    assert_eq!(requested["detail"], OS_USER, "{requested}");
    let accepted = rows_of(&log, "cancel_accepted")[0];
    assert_eq!(accepted["actor"], "daemon", "{accepted}");
    let skipped = *phase_enters(&log).last().unwrap();
    assert_eq!(skipped["from_phase"], "running", "{skipped}");
    assert_eq!(skipped["to_phase"], "skipped", "{skipped}");
    assert_eq!(skipped["actor"], "daemon", "{skipped}");
    assert_eq!(skipped["reason"], "canceled", "{skipped}");
    assert!(
        skipped["detail"]
            .as_str()
            .is_some_and(|d| d.starts_with("cancel request ")),
        "{skipped}"
    );
    let closed = rows_of(&log, "cancel_closed")[0];
    assert_eq!(closed["reason"], "canceled", "{closed}");

    // Skipped cleans the worktree.
    let (env, name) = (&r.env, worktree_name(&r.work_id));
    wait_until(
        Duration::from_secs(10),
        "worktree cleaned after cancel",
        &mut r.daemon,
        || (!env.worktree_dirs().contains(&name)).then_some(()),
    );

    // The canceled run fires no on_fail, no escalation and no comment.
    assert!(on_fail_runs(&r.on_fail_log).is_empty());
    let derived: Vec<_> = r
        .env
        .queue_items()
        .into_iter()
        .filter(|i| i["derived_from"] == r.work_id.as_str())
        .collect();
    assert!(derived.is_empty(), "{derived:#?}");
    assert_eq!(history_statuses(&r.env, &r.work_id), ["skipped"]);
    assert_eq!(progress_comments_any(&r.env, r.issue, "skipped"), 0);
}

/// Crash the daemon of `r`, leaving the handler group and `daemon.pid`.
fn crash_daemon(r: &mut Running) {
    r.daemon.kill_hard();
    assert!(r.env.belt_home.join("daemon.pid").exists());
    assert!(!process_gone(r.leader), "handler died with the daemon");
}

/// Scenario 5: with the daemon gone, the CLI cancels the Running item
/// itself, kills the leftover handler group and answers `canceled_directly`.
pub fn skip_running_item_without_daemon_cancels_directly() {
    let mut r = running_item(1);
    crash_daemon(&mut r);

    let (code, answer) = r.env.belt_answer(&["queue", "skip", &r.work_id, "--json"]);
    assert_eq!(code, 0, "{answer}");
    assert_eq!(answer["success"], true, "{answer}");
    assert_eq!(answer["result"], "canceled_directly", "{answer}");
    assert_eq!(answer["phase"], "skipped", "{answer}");
    wait_processes_gone(&[r.leader, r.child]);

    let log = r.env.transitions(&r.work_id);
    assert!(rows_of(&log, "cancel_accepted").is_empty(), "{log:#?}");
    let requested = rows_of(&log, "cancel_requested");
    assert_eq!(requested.len(), 1, "{log:#?}");
    assert_eq!(requested[0]["actor"], "cli");
    let skipped = *phase_enters(&log).last().unwrap();
    assert_eq!(skipped["from_phase"], "running", "{skipped}");
    assert_eq!(skipped["to_phase"], "skipped", "{skipped}");
    assert_eq!(skipped["actor"], "cli", "{skipped}");
    assert_eq!(skipped["reason"], "canceled", "{skipped}");
    assert!(
        skipped["detail"]
            .as_str()
            .is_some_and(|d| d.ends_with(": canceled directly")),
        "{skipped}"
    );
    let closed = rows_of(&log, "cancel_closed");
    assert_eq!(closed.len(), 1, "{log:#?}");
    assert_eq!(closed[0]["reason"], "canceled_directly");
    let seq = |row: &serde_json::Value| row["seq"].as_i64().unwrap();
    assert!(seq(requested[0]) < seq(skipped) && seq(skipped) < seq(closed[0]));

    // A non-derived item owns its worktree under its own key.
    assert!(
        !r.env.worktree_dirs().contains(&worktree_name(&r.work_id)),
        "{:?}",
        r.env.worktree_dirs()
    );

    // A restart neither rolls the item back nor reruns it. The label is
    // removed first: a restart scans at once and would start `:2`.
    r.env
        .gh
        .update(|s| s.issues.get_mut(&r.issue).unwrap().labels.clear());
    let collects = || {
        r.env
            .gh
            .calls()
            .iter()
            .filter(|c| c.line().starts_with("gh issue list"))
            .count()
    };
    let base = collects();
    r.daemon = r.env.start_daemon(&r.config, 1);
    let (env, daemon) = (&r.env, &mut r.daemon);
    // Startup restore (where a rollback would happen) precedes the first
    // tick, whose scan is the one collect call after a restart.
    wait_until(
        Duration::from_secs(10),
        "the first tick's collect scan after restart",
        daemon,
        || (collects() > base && env.daemon_log().contains("belt daemon started")).then_some(()),
    );
    assert_eq!(r.env.phase_of(&r.work_id).as_deref(), Some("skipped"));
    let log = r.env.transitions(&r.work_id);
    let last = *phase_enters(&log).last().unwrap();
    assert_eq!(
        last["to_phase"], "skipped",
        "rolled back after restart: {log:#?}"
    );
    assert_eq!(r.env.queue_items().len(), 1);
}

/// D6: a direct cancel records the run as `skipped` in the attempt history,
/// like a cancel through the daemon. (spec: queue-state-machine.md:268,
/// data-model.md:79)
pub fn direct_cancel_records_skipped_attempt() {
    let mut r = running_item(1);
    crash_daemon(&mut r);
    let (code, answer) = r.env.belt_answer(&["queue", "skip", &r.work_id, "--json"]);
    assert_eq!(
        (code, answer["result"].as_str()),
        (0, Some("canceled_directly"))
    );
    assert_eq!(history_statuses(&r.env, &r.work_id), ["skipped"]);
}

/// D5: directly canceling a derived item cleans the worktree it inherited
/// (the owner's key), not one named after its own work_id.
/// (spec: queue-state-machine.md:318, 324)
pub fn direct_cancel_of_derived_item_cleans_inherited_worktree() {
    let env = E2eEnv::new();
    let issue = env.gh.seed_issue("Analyze", "body", &[TRIGGER]);
    let original = first_work_id(issue);
    let derived = format!("{original}:2");
    let pids = HandlerPids::of(&env);
    // First run fails (retry derives `:2` into the same worktree); the
    // second run, seeing `.attempt`, sleeps.
    let handler = format!(
        r#"if [ ! -f "$WORKTREE/.attempt" ]; then touch "$WORKTREE/.attempt"; exit 1; fi; {}"#,
        pids.sleeping_handler()
    );
    let mut ws = Workspace::new(handler);
    ws.escalation = "1: retry\n2: hitl";
    let config = ws.install(&env);

    let mut daemon = env.start_daemon(&config, 1);
    wait_until(
        Duration::from_secs(20),
        "derived item's handler to be running",
        &mut daemon,
        || (pids.started() && env.queue_show(&derived)["processing"] == "handler").then_some(()),
    );
    let (leader, child) = pids.read();
    daemon.kill_hard();

    let (code, answer) = env.belt_answer(&["queue", "skip", &derived, "--json"]);
    assert_eq!(
        (code, answer["result"].as_str()),
        (0, Some("canceled_directly"))
    );
    wait_processes_gone(&[leader, child]);
    let inherited = worktree_name(&original);
    assert!(
        !env.worktree_dirs().contains(&inherited),
        "inherited worktree {inherited} left behind: {:?}",
        env.worktree_dirs()
    );
    // The git worktree registration goes too, not only the directory.
    assert_eq!(
        env.git_worktrees(),
        vec![env.repo_dir.display().to_string()]
    );
    assert_eq!(history_statuses(&env, &derived), ["skipped"]);
}

/// A `belt` CLI process running in the background; killed on drop, so a
/// failing scenario does not leave it waiting out its cancel limit.
struct BackgroundBelt(Option<std::process::Child>);

impl BackgroundBelt {
    fn spawn(env: &E2eEnv, args: &[&str]) -> Self {
        let child = env
            .command(env!("CARGO_BIN_EXE_belt"))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn belt");
        Self(Some(child))
    }

    /// Wait for it to exit; `(exit code, JSON stdout)`.
    fn answer(mut self) -> (i32, serde_json::Value) {
        let out = self.0.take().unwrap().wait_with_output().unwrap();
        let json = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "belt printed non-JSON ({e}): stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code().unwrap_or(-1), json)
    }
}

impl Drop for BackgroundBelt {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Scenario 4 edge: the handler ends after the CLI saw the item Running but
/// before the cancel is carried out. The skip is refused `too_late` with the
/// phase the item reached, and the finished run stands.
pub fn skip_after_handler_finished_is_too_late() {
    let env = E2eEnv::new();
    let issue = env.gh.seed_issue("Analyze", "body", &[TRIGGER]);
    let work_id = first_work_id(issue);
    let started = env.tmp_dir.join("started");
    let release = env.tmp_dir.join("release");
    let handler = format!(
        r#"echo $$ > "{}/leader"; touch "{}"; while [ ! -f "{}" ]; do sleep 0.1; done"#,
        env.handler_pid_dir().display(),
        started.display(),
        release.display()
    );
    let on_fail_log = env.tmp_dir.join("on_fail.log");
    let mut ws = Workspace::new(handler);
    ws.on_fail_log = Some(on_fail_log.clone());
    let config = ws.install(&env);

    // A 120s tick: after the first tick only the handler's own end moves the
    // item (Completed is evaluated on a tick, two minutes away, leaving a
    // wide margin on a slow CI runner).
    let mut daemon = env.start_daemon(&config, 120);
    wait_until(
        Duration::from_secs(20),
        "handler to be running",
        &mut daemon,
        || (started.exists() && env.queue_show(&work_id)["processing"] == "handler").then_some(()),
    );
    // Without its IPC port file the CLI cannot wake the live daemon, so it
    // waits out its limit and then tries the cancel itself.
    std::fs::remove_file(env.belt_home.join("daemon.ipc")).expect("remove daemon.ipc");
    let skip = BackgroundBelt::spawn(&env, &["queue", "skip", &work_id, "--json"]);
    wait_until(
        Duration::from_secs(10),
        "cancel request recorded",
        &mut daemon,
        || (!rows_of(&env.transitions(&work_id), "cancel_requested").is_empty()).then_some(()),
    );
    std::fs::write(&release, "").unwrap();
    wait_until(
        Duration::from_secs(10),
        "handler result applied as completed",
        &mut daemon,
        || (env.phase_of(&work_id).as_deref() == Some("completed")).then_some(()),
    );

    let (code, answer) = skip.answer();
    assert_eq!(code, 1, "{answer}");
    assert_eq!(answer["success"], false, "{answer}");
    assert_eq!(answer["reason"], "too_late", "{answer}");
    assert_eq!(answer["current"], "completed", "{answer}");
    assert_eq!(answer["work_id"], work_id.as_str(), "{answer}");

    assert_eq!(env.phase_of(&work_id).as_deref(), Some("completed"));
    let log = env.transitions(&work_id);
    let closed = rows_of(&log, "cancel_closed");
    assert_eq!(closed.len(), 1, "{log:#?}");
    assert_eq!(closed[0]["reason"], "too_late", "{}", closed[0]);
    assert!(
        !phase_enters(&log)
            .iter()
            .any(|t| t["to_phase"] == "skipped"),
        "{log:#?}"
    );
    assert!(on_fail_runs(&on_fail_log).is_empty());
}

/// Scenario 4 edge: skipping an item that waits for a slot (Pending or
/// Ready) applies at once, as the CLI, without touching the running one.
pub fn skip_of_queued_item_is_applied() {
    let env = E2eEnv::new();
    let ids = [
        first_work_id(env.gh.seed_issue("First", "body", &[TRIGGER])),
        first_work_id(env.gh.seed_issue("Second", "body", &[TRIGGER])),
    ];
    let pids = HandlerPids::of(&env);
    let config = Workspace::new(pids.sleeping_handler()).install(&env);

    // concurrency 1: while one handler sleeps the other item waits.
    let mut daemon = env.start_daemon(&config, 1);
    let (running, queued) = wait_until(
        Duration::from_secs(20),
        "one item running and the other waiting",
        &mut daemon,
        || {
            if !pids.started() {
                return None;
            }
            let phase = |id: &str| env.phase_of(id).unwrap_or_default();
            let waiting = |p: &str| p == "pending" || p == "ready";
            let (a, b) = (phase(&ids[0]), phase(&ids[1]));
            if a == "running" && waiting(&b) {
                Some((ids[0].clone(), ids[1].clone()))
            } else if b == "running" && waiting(&a) {
                Some((ids[1].clone(), ids[0].clone()))
            } else {
                None
            }
        },
    );

    let (code, answer) = env.belt_answer(&["queue", "skip", &queued, "--json"]);
    assert_eq!(code, 0, "{answer}");
    assert_eq!(answer["success"], true, "{answer}");
    assert_eq!(answer["result"], "applied", "{answer}");
    assert_eq!(answer["phase"], "skipped", "{answer}");

    let log = env.transitions(&queued);
    let last = *phase_enters(&log).last().unwrap();
    assert!(
        matches!(last["from_phase"].as_str(), Some("pending" | "ready")),
        "{last}"
    );
    assert_eq!(last["to_phase"], "skipped", "{last}");
    assert_eq!(last["actor"], "cli", "{last}");
    assert_eq!(last["reason"], "manual", "{last}");
    assert!(rows_of(&log, "cancel_requested").is_empty(), "{log:#?}");
    assert_eq!(env.phase_of(&queued).as_deref(), Some("skipped"));
    assert_eq!(env.queue_show(&running)["processing"], "handler");
}

/// Scenario 5 edge: a daemon that is alive but unresponsive (SIGSTOP) makes
/// the CLI wait its cancel limit and then cancel directly. Once the daemon
/// resumes, its late result for the killed handler is discarded as a
/// conflict: no on_fail, no escalation, the item stays Skipped.
pub fn skip_with_unresponsive_daemon_cancels_directly_and_late_result_conflicts() {
    let mut r = running_item(1);
    r.daemon.pause();

    let started = Instant::now();
    let (code, answer) = r.env.belt_answer(&["queue", "skip", &r.work_id, "--json"]);
    let took = started.elapsed();
    assert_eq!(code, 0, "{answer}");
    assert_eq!(answer["result"], "canceled_directly", "{answer}");
    assert_eq!(answer["phase"], "skipped", "{answer}");
    // The CLI waited for the live daemon before going direct (limit 10s).
    assert!(took >= Duration::from_secs(9), "answered after {took:?}");
    wait_processes_gone(&[r.leader, r.child]);

    let log = r.env.transitions(&r.work_id);
    assert!(rows_of(&log, "cancel_accepted").is_empty(), "{log:#?}");
    let skipped = *phase_enters(&log).last().unwrap();
    assert_eq!(skipped["from_phase"], "running", "{skipped}");
    assert_eq!(skipped["to_phase"], "skipped", "{skipped}");
    assert_eq!(skipped["actor"], "cli", "{skipped}");
    let closed = rows_of(&log, "cancel_closed");
    assert_eq!(closed.len(), 1, "{log:#?}");
    assert_eq!(closed[0]["reason"], "canceled_directly");

    r.daemon.resume();
    // The log line only marks when the late result has been handled; the
    // assertions below check the outcome through the store and the CLI.
    let discarded = format!("discarded: {} (stored phase is skipped)", r.work_id);
    let env = &r.env;
    wait_until(
        Duration::from_secs(10),
        "the resumed daemon to discard its late result",
        &mut r.daemon,
        || env.daemon_log().contains(&discarded).then_some(()),
    );
    assert_eq!(r.env.phase_of(&r.work_id).as_deref(), Some("skipped"));
    let log = r.env.transitions(&r.work_id);
    let last = *phase_enters(&log).last().unwrap();
    assert_eq!(last["actor"], "cli", "late result moved the item: {last}");
    assert!(on_fail_runs(&r.on_fail_log).is_empty());
    let derived: Vec<_> = r
        .env
        .queue_items()
        .into_iter()
        .filter(|i| i["derived_from"] == r.work_id.as_str())
        .collect();
    assert!(derived.is_empty(), "{derived:#?}");
    assert_eq!(progress_comments_any(&r.env, r.issue, "failed"), 0);
}
