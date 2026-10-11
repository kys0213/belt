//! Cancel scenarios: `belt queue skip` on a Running item, through a live
//! daemon (4) and directly when the daemon is gone (5).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::harness::{DaemonGuard, E2eEnv, process_gone, read_pid, wait_for, wait_until};
use crate::support::{
    TRIGGER, Workspace, first_work_id, on_fail_runs, phase_enters, progress_comments_any, rows_of,
    worktree_name,
};

/// Handler that records its group leader and a sleeping child, then blocks.
/// Both pid files live in the harness pid registry, so the guard kills the
/// group even if the scenario fails before the cancel.
fn sleeping_handler(pids: &std::path::Path) -> String {
    format!(
        r#"echo $$ > "{d}/leader"; sleep 300 & echo $! > "{d}/child"; wait"#,
        d = pids.display()
    )
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
    let pids = env.handler_pid_dir();
    let on_fail_log = env.tmp_dir.join("on_fail.log");
    let mut ws = Workspace::new(sleeping_handler(&pids));
    ws.on_fail_log = Some(on_fail_log.clone());
    let config = ws.install(&env);

    let mut daemon = env.start_daemon(&config, tick_secs);
    wait_until(
        Duration::from_secs(20),
        "handler to be running",
        &mut daemon,
        || {
            (pids.join("child").exists() && env.queue_show(&work_id)["processing"] == "handler")
                .then_some(())
        },
    );
    let (leader, child) = (
        read_pid(&pids.join("leader")),
        read_pid(&pids.join("child")),
    );
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
    let pids = env.handler_pid_dir();
    // First run fails (retry derives `:2` into the same worktree); the
    // second run, seeing `.attempt`, sleeps.
    let handler = format!(
        r#"if [ ! -f "$WORKTREE/.attempt" ]; then touch "$WORKTREE/.attempt"; exit 1; fi; {}"#,
        sleeping_handler(&pids)
    );
    let mut ws = Workspace::new(handler);
    ws.escalation = "1: retry\n2: hitl";
    let config = ws.install(&env);

    let mut daemon = env.start_daemon(&config, 1);
    wait_until(
        Duration::from_secs(20),
        "derived item's handler to be running",
        &mut daemon,
        || {
            (pids.join("child").exists() && env.queue_show(&derived)["processing"] == "handler")
                .then_some(())
        },
    );
    let (leader, child) = (
        read_pid(&pids.join("leader")),
        read_pid(&pids.join("child")),
    );
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
