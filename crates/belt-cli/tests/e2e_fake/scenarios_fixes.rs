//! Regression cases for reviewed fixes: shutdown signals observed during a
//! tick and draining a Running handler on SIGTERM, owner-key worktree
//! cleanup on `belt queue skip` / `belt queue done` of a derived item, and
//! the needs-human label on a registered workspace.

use std::process::{Command, Stdio};
use std::time::Duration;

use crate::harness::{E2eEnv, process_gone, read_pid, wait_for, wait_until};
use crate::support::{
    TRIGGER, Workspace, first_work_id, register_workspace, remove_label_script, worktree_name,
};

/// Send `kill -<signal>` to the daemon recorded in `daemon.pid`.
fn signal_daemon(env: &E2eEnv, signal: &str) {
    let pid_file = env.belt_home.join("daemon.pid");
    wait_for(Duration::from_secs(10), "daemon.pid", || {
        pid_file.exists().then_some(())
    });
    let pid = read_pid(&pid_file);
    let status = Command::new("kill")
        .args([format!("-{signal}"), pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "kill -{signal} {pid} failed");
}

/// A shutdown signal that arrives while a tick runs is still observed once
/// the tick returns; a per-iteration signal future would miss it and the
/// daemon would keep running. (spec: daemon.md "Graceful Shutdown")
pub fn sigint_during_a_tick_shuts_the_daemon_down() {
    let env = E2eEnv::new();
    env.gh.seed_issue("Analyze", "body", &[TRIGGER]);
    let started = env.tmp_dir.join("on_done.started");
    let mut ws = Workspace::new("exit 0");
    // on_done runs inside the tick's evaluate step, keeping the tick busy.
    ws.on_done = format!(
        r#"touch "{}"; sleep 3; {}"#,
        started.display(),
        remove_label_script()
    );
    let config = ws.install(&env);

    let mut daemon = env.start_daemon(&config, 1);
    wait_until(
        Duration::from_secs(20),
        "on_done to run inside a tick",
        &mut daemon,
        || started.exists().then_some(()),
    );
    signal_daemon(&env, "INT");

    assert!(
        daemon.wait_exit(Duration::from_secs(15)),
        "SIGINT sent during a tick was missed; log:\n{}",
        env.daemon_log()
    );
    let log = env.daemon_log();
    assert!(log.contains("received SIGINT"), "{log}");
    assert!(log.contains("belt daemon stopped"), "{log}");
}

/// SIGTERM (`belt stop`) drains like SIGINT: the daemon waits for the
/// Running handler, applies its result and exits, leaving no handler
/// behind. (spec: daemon.md "Graceful Shutdown", queue-state-machine.md:321)
pub fn sigterm_drains_a_running_handler() {
    let env = E2eEnv::new();
    let issue = env.gh.seed_issue("Analyze", "body", &[TRIGGER]);
    let work_id = first_work_id(issue);
    let pids = env.handler_pid_dir();
    let finished = env.tmp_dir.join("handler.finished");
    let handler = format!(
        r#"echo $$ > "{d}/leader"; sleep 3; touch "{f}""#,
        d = pids.display(),
        f = finished.display()
    );
    let config = Workspace::new(handler).install(&env);

    let mut daemon = env.start_daemon(&config, 1);
    wait_until(
        Duration::from_secs(20),
        "handler to be running",
        &mut daemon,
        || {
            (pids.join("leader").exists() && env.queue_show(&work_id)["processing"] == "handler")
                .then_some(())
        },
    );
    let leader = read_pid(&pids.join("leader"));
    signal_daemon(&env, "TERM");

    assert!(
        daemon.wait_exit(Duration::from_secs(20)),
        "daemon did not exit after SIGTERM; log:\n{}",
        env.daemon_log()
    );
    assert!(
        finished.exists(),
        "daemon exited before its handler finished; log:\n{}",
        env.daemon_log()
    );
    assert!(process_gone(leader), "handler outlived the daemon");
    assert_eq!(env.phase_of(&work_id).as_deref(), Some("completed"));
    let log = env.daemon_log();
    assert!(log.contains("received SIGTERM"), "{log}");
    assert!(log.contains("all running handlers drained"), "{log}");
}

/// Fails until `.attempt` exists in its worktree, so the item derived by
/// escalation retry succeeds only in the worktree it inherited.
const FAIL_ONCE_HANDLER: &str =
    r#"if [ -f "$WORKTREE/.attempt" ]; then exit 0; fi; touch "$WORKTREE/.attempt"; exit 1"#;

/// `belt queue skip <derived>` succeeds and removes the worktree the derived
/// item inherited from `original` (its owner key), directory and git
/// registration. (spec: queue-state-machine.md:111, 324)
fn skip_cleans_inherited_worktree(env: &E2eEnv, derived: &str, original: &str) {
    let inherited = worktree_name(original);
    assert!(
        env.worktree_dirs().contains(&inherited),
        "inherited worktree {inherited} missing before the skip: {:?}",
        env.worktree_dirs()
    );
    let (code, answer) = env.belt_answer(&["queue", "skip", derived, "--json"]);
    assert_eq!((code, &answer["success"]), (0, &true.into()), "{answer}");
    assert_eq!(env.phase_of(derived).as_deref(), Some("skipped"));
    assert!(
        !env.worktree_dirs().contains(&inherited),
        "inherited worktree {inherited} left behind: {:?}",
        env.worktree_dirs()
    );
    assert_eq!(
        env.git_worktrees(),
        vec![env.repo_dir.display().to_string()]
    );
}

/// D5: skipping a Pending derived item cleans the worktree it inherited.
pub fn skip_of_pending_derived_item_cleans_inherited_worktree() {
    let env = E2eEnv::new();
    let issue = env.gh.seed_issue("Analyze", "body", &[TRIGGER]);
    let original = first_work_id(issue);
    let derived = format!("{original}:2");
    let mut ws = Workspace::new(FAIL_ONCE_HANDLER);
    ws.escalation = "1: retry\n2: hitl";
    let config = ws.install(&env);

    // Only the first tick runs (at start): the derived item stays Pending.
    let mut daemon = env.start_daemon(&config, 3600);
    wait_until(
        Duration::from_secs(20),
        "derived item to be pending",
        &mut daemon,
        || (env.phase_of(&derived).as_deref() == Some("pending")).then_some(()),
    );
    assert!(daemon.stop(Duration::from_secs(10)), "daemon did not stop");

    skip_cleans_inherited_worktree(&env, &derived, &original);
}

/// D5: skipping a Failed derived item (its on_done failed) cleans the
/// worktree it inherited; Failed alone preserves it.
pub fn skip_of_failed_derived_item_cleans_inherited_worktree() {
    let env = E2eEnv::new();
    let issue = env.gh.seed_issue("Analyze", "body", &[TRIGGER]);
    let original = first_work_id(issue);
    let derived = format!("{original}:2");
    let mut ws = Workspace::new(FAIL_ONCE_HANDLER);
    ws.escalation = "1: retry\n2: hitl";
    ws.on_done = "exit 1".to_string();
    let config = ws.install(&env);

    let mut daemon = env.start_daemon(&config, 1);
    wait_until(
        Duration::from_secs(30),
        "derived item to fail on its on_done",
        &mut daemon,
        || (env.phase_of(&derived).as_deref() == Some("failed")).then_some(()),
    );
    assert!(daemon.stop(Duration::from_secs(10)), "daemon did not stop");

    skip_cleans_inherited_worktree(&env, &derived, &original);
}

/// D5: `belt queue done` on a Completed derived item runs on_done in the
/// worktree it inherited (the owner key) and cleans that worktree on Done.
/// (spec: queue-state-machine.md:111, 324)
pub fn done_of_completed_derived_item_uses_and_cleans_inherited_worktree() {
    let env = E2eEnv::new();
    let issue = env.gh.seed_issue("Analyze", "body", &[TRIGGER]);
    let original = first_work_id(issue);
    let derived = format!("{original}:2");
    let on_done_log = env.tmp_dir.join("on_done.log");
    let mut ws = Workspace::new(FAIL_ONCE_HANDLER);
    ws.escalation = "1: retry\n2: hitl";
    ws.on_done = format!(
        r#"echo "$WORK_ID $WORKTREE" >> "{}""#,
        on_done_log.display()
    );
    let config = ws.install(&env);

    // Ticks run only at start and on SIGUSR1, so the derived item is
    // claimed by the second tick and its success is never evaluated.
    let mut daemon = env.start_daemon(&config, 3600);
    wait_until(
        Duration::from_secs(20),
        "derived item to be pending",
        &mut daemon,
        || (env.phase_of(&derived).as_deref() == Some("pending")).then_some(()),
    );
    signal_daemon(&env, "USR1");
    wait_until(
        Duration::from_secs(20),
        "derived item to be completed",
        &mut daemon,
        || (env.phase_of(&derived).as_deref() == Some("completed")).then_some(()),
    );
    assert!(daemon.stop(Duration::from_secs(10)), "daemon did not stop");
    assert!(
        !on_done_log.exists(),
        "the daemon evaluated the derived item"
    );

    let inherited = worktree_name(&original);
    let inherited_path = env.belt_home.join("worktrees").join(&inherited);
    assert!(inherited_path.exists(), "{:?}", env.worktree_dirs());
    register_workspace(&env, &config);
    let (code, answer) = env.belt_answer(&["queue", "done", &derived, "--json"]);
    assert_eq!((code, &answer["success"]), (0, &true.into()), "{answer}");
    assert_eq!(env.phase_of(&derived).as_deref(), Some("done"));

    let runs = std::fs::read_to_string(&on_done_log).unwrap();
    assert_eq!(
        runs.lines().collect::<Vec<_>>(),
        [format!("{derived} {}", inherited_path.display())],
        "on_done must run once, in the inherited worktree"
    );
    assert!(
        !env.worktree_dirs().contains(&inherited),
        "inherited worktree {inherited} left behind: {:?}",
        env.worktree_dirs()
    );
    assert_eq!(
        env.git_worktrees(),
        vec![env.repo_dir.display().to_string()]
    );
}

const NEEDS_HUMAN: &str = "belt:needs-human";

/// The lifecycle hook of a workspace registered with `belt workspace add`
/// (resolved per trigger from the DB) labels the issue `belt:needs-human`
/// when a HITL request opens and removes the label once it is resolved.
/// (spec: lifecycle-hook.md "workspace 바인딩과 hook 선택", "on_hitl_opened")
pub fn registered_workspace_needs_human_label_added_and_removed() {
    let env = E2eEnv::new();
    let issue = env.gh.seed_issue("Analyze", "body", &[TRIGGER]);
    let work_id = first_work_id(issue);
    let mut ws = Workspace::new("exit 1");
    ws.escalation = "1: hitl";
    let config = ws.install(&env);
    register_workspace(&env, &config);

    let mut daemon = env.start_daemon(&config, 1);
    let has_label = || env.gh.issue_labels(issue).iter().any(|l| l == NEEDS_HUMAN);
    wait_until(
        Duration::from_secs(20),
        "belt:needs-human label on the issue",
        &mut daemon,
        || has_label().then_some(()),
    );
    assert_eq!(env.phase_of(&work_id).as_deref(), Some("hitl"));

    // Without the trigger label the skipped issue is not collected again
    // (a new item would reopen a HITL and re-add the label).
    env.gh.update(|s| {
        s.issues
            .get_mut(&issue)
            .unwrap()
            .labels
            .retain(|l| l != TRIGGER)
    });
    let (code, answer) = env.belt_answer(&["queue", "skip", &work_id, "--json"]);
    assert_eq!(code, 0, "{answer}");
    wait_until(
        Duration::from_secs(20),
        "belt:needs-human label removed after resolve",
        &mut daemon,
        || (!has_label()).then_some(()),
    );
    wait_until(
        Duration::from_secs(10),
        "item to be skipped by post-processing",
        &mut daemon,
        || (env.phase_of(&work_id).as_deref() == Some("skipped")).then_some(()),
    );
}
