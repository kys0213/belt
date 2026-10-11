//! Regression cases for reviewed fixes: shutdown signals observed during a
//! tick and draining a Running handler on SIGTERM.

use std::process::{Command, Stdio};
use std::time::Duration;

use crate::harness::{E2eEnv, process_gone, read_pid, wait_for, wait_until};
use crate::support::{TRIGGER, Workspace, first_work_id, remove_label_script};
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
