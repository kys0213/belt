//! Lineage scenarios: a failed handler derives the next item of the same
//! lineage (2), and a Failed item blocks re-collection (7).

use std::time::Duration;

use crate::harness::{E2eEnv, wait_until};
use crate::support::{
    STATE, TRIGGER, Workspace, first_work_id, on_fail_runs, phase_enters, progress_comments,
    progress_comments_any, rows_of, worktree_name,
};

/// Scenario 2: the first handler run fails, escalation level 1 (`retry`)
/// skips the item as `derived` and creates `…:2`, which inherits the
/// worktree, succeeds there and reaches Done.
pub fn handler_failure_retry_derives_item_that_inherits_worktree() {
    let env = E2eEnv::new();
    let issue = env.gh.seed_issue("Analyze", "body", &[TRIGGER]);
    let original = first_work_id(issue);
    let derived = format!("{original}:2");
    let runs_log = env.tmp_dir.join("runs.log");
    let on_fail_log = env.tmp_dir.join("on_fail.log");
    // Fails until `.attempt` exists in its worktree: the second run only
    // succeeds when it gets the same worktree back.
    let handler = format!(
        r#"echo "$WORK_ID $WORKTREE" >> "{log}"; if [ -f "$WORKTREE/.attempt" ]; then exit 0; fi; touch "$WORKTREE/.attempt"; exit 1"#,
        log = runs_log.display()
    );
    let mut ws = Workspace::new(handler);
    ws.on_fail_log = Some(on_fail_log.clone());
    let config = ws.install(&env);

    let mut daemon = env.start_daemon(&config, 1);
    wait_until(
        Duration::from_secs(20),
        "derived item to reach done",
        &mut daemon,
        || (env.phase_of(&derived).as_deref() == Some("done")).then_some(()),
    );
    // Done is committed before the inherited worktree is removed.
    wait_until(
        Duration::from_secs(10),
        "inherited worktree to be cleaned after done",
        &mut daemon,
        || (env.worktree_dirs().is_empty() && env.git_branches() == ["main"]).then_some(()),
    );
    wait_until(
        Duration::from_secs(10),
        "a started comment for each item",
        &mut daemon,
        || (progress_comments_any(&env, issue, "started") == 2).then_some(()),
    );

    // 1. The original was skipped as `derived` by the retry escalation.
    let orig = env.item(&original).expect("original item missing");
    assert_eq!(orig["phase"], "skipped");
    let orig_log = env.transitions(&original);
    let last = *phase_enters(&orig_log).last().unwrap();
    assert_eq!(last["from_phase"], "running", "{last}");
    assert_eq!(last["to_phase"], "skipped", "{last}");
    assert_eq!(last["reason"], "derived", "{last}");
    assert_eq!(last["detail"], "escalation: retry", "{last}");

    // 2. The derived item links back to it and shares its lineage.
    let item = env.item(&derived).expect("derived item missing");
    assert_eq!(item["derived_from"], original.as_str(), "{item}");
    assert_eq!(item["lineage_root"], original.as_str(), "{item}");
    assert_eq!(item["source_id"], orig["source_id"]);
    assert_eq!(item["state"], STATE);
    let derived_log = env.transitions(&derived);
    let created = rows_of(&derived_log, "item_created");
    assert_eq!(created.len(), 1, "{derived_log:#?}");
    assert_eq!(created[0]["reason"], "derived");
    assert_eq!(created[0]["detail"], original.as_str());
    // Exactly the two items; no third run.
    assert_eq!(env.queue_items().len(), 2, "{:#?}", env.queue_items());

    // 4. Worktree handover: both runs used the original item's worktree.
    let runs = std::fs::read_to_string(&runs_log).unwrap();
    let runs: Vec<(&str, &str)> = runs.lines().map(|l| l.split_once(' ').unwrap()).collect();
    let expected_wt = env
        .belt_home
        .join("worktrees")
        .join(worktree_name(&original));
    assert_eq!(
        runs,
        vec![
            (original.as_str(), expected_wt.to_str().unwrap()),
            (derived.as_str(), expected_wt.to_str().unwrap()),
        ]
    );

    // 6. `retry` runs no on_fail and announces neither failed nor skipped.
    let on_fail = on_fail_runs(&on_fail_log);
    assert!(on_fail.is_empty(), "on_fail ran: {on_fail:?}");
    assert_eq!(progress_comments(&env, issue, &original, "started"), 1);
    assert_eq!(progress_comments(&env, issue, &derived, "started"), 1);
    assert_eq!(progress_comments_any(&env, issue, "failed"), 0);
    assert_eq!(progress_comments_any(&env, issue, "skipped"), 0);
}

/// Scenario 7: a Failed item of `(source_id, state)` keeps the still
/// labeled issue from being collected again, across scans and a daemon
/// restart. Skipping it reopens the lineage as `…:2`.
pub fn failed_item_blocks_recollection_across_scans_and_restart() {
    let env = E2eEnv::new();
    let issue = env.gh.seed_issue("Analyze", "body", &[TRIGGER]);
    let work_id = first_work_id(issue);
    // Handler succeeds, the evaluator judges Done, on_done fails -> Failed.
    // The label is never removed.
    let mut ws = Workspace::new("true");
    ws.on_done = "exit 1".to_string();
    ws.scan_interval_secs = 1;
    let config = ws.install(&env);

    let count_items = || {
        env.queue_items()
            .iter()
            .filter(|i| i["source_id"] == format!("github:{}#{issue}", crate::harness::REPO))
            .filter(|i| i["state"] == STATE)
            .count()
    };
    let collect_calls = || {
        env.gh
            .calls()
            .iter()
            .filter(|c| c.line().starts_with("gh issue list"))
            .count()
    };

    let mut daemon = env.start_daemon(&config, 1);
    wait_until(
        Duration::from_secs(20),
        "item to fail on on_done",
        &mut daemon,
        || (env.phase_of(&work_id).as_deref() == Some("failed")).then_some(()),
    );
    let log = env.transitions(&work_id);
    let last = *phase_enters(&log).last().unwrap();
    assert_eq!(last["from_phase"], "completed", "{last}");
    assert_eq!(last["to_phase"], "failed", "{last}");
    assert!(
        env.worktree_dirs().contains(&worktree_name(&work_id)),
        "Failed must preserve the worktree: {:?}",
        env.worktree_dirs()
    );

    // Two more scans while Failed: still one item.
    let base = collect_calls();
    wait_until(
        Duration::from_secs(15),
        "two more collect scans",
        &mut daemon,
        || (collect_calls() >= base + 2).then_some(()),
    );
    assert_eq!(count_items(), 1, "{:#?}", env.queue_items());

    // A restart scans at once; the Failed item still blocks it.
    assert!(
        daemon.stop(Duration::from_secs(10)),
        "daemon ignored SIGINT"
    );
    drop(daemon);
    let base = collect_calls();
    let mut daemon = env.start_daemon(&config, 1);
    wait_until(
        Duration::from_secs(15),
        "two collect scans after restart",
        &mut daemon,
        || (collect_calls() >= base + 2).then_some(()),
    );
    assert_eq!(count_items(), 1, "{:#?}", env.queue_items());
    assert_eq!(env.phase_of(&work_id).as_deref(), Some("failed"));

    // Skipping the Failed item ends the lineage; the next scan starts `:2`.
    let (code, answer) = env.belt_answer(&["queue", "skip", &work_id, "--json"]);
    assert_eq!(code, 0, "{answer}");
    assert_eq!(answer["result"], "applied", "{answer}");
    assert_eq!(answer["phase"], "skipped", "{answer}");
    let log = env.transitions(&work_id);
    let last = *phase_enters(&log).last().unwrap();
    assert_eq!(last["from_phase"], "failed", "{last}");
    assert_eq!(last["actor"], "cli", "{last}");
    assert_eq!(last["reason"], "manual", "{last}");
    assert!(
        !env.worktree_dirs().contains(&worktree_name(&work_id)),
        "skip must clean the worktree: {:?}",
        env.worktree_dirs()
    );

    let next = format!("{work_id}:2");
    let item = wait_until(
        Duration::from_secs(15),
        "next lineage item after skip",
        &mut daemon,
        || env.item(&next),
    );
    assert!(item.get("derived_from").is_none(), "{item}");
    assert_eq!(item["lineage_root"], next.as_str(), "{item}");
    let created = env.transitions(&next);
    let created = rows_of(&created, "item_created");
    assert_eq!(created[0]["reason"], "collected", "{created:?}");
}

// E2E-FINDING: once a workspace with a GitHub source is registered
// (`belt workspace add`), the daemon panics on its first tick and exits:
// `PrReviewScanJob` bridges `collect_review_items` with
// `std::thread::scope(|_| { Runtime::new()...block_on(..) })`
// (belt-daemon/src/cron.rs:1715-1722). `thread::scope` runs the closure on
// the calling thread — no thread is spawned — and that thread already drives
// the daemon's tokio runtime, so `block_on` panics with "Cannot start a
// runtime from within a runtime" (cron.rs:1721).
/// A registered workspace must not take the daemon down.
pub fn finding_daemon_survives_a_registered_workspace() {
    let env = E2eEnv::new();
    let issue = env.gh.seed_issue("Analyze", "body", &[TRIGGER]);
    let work_id = first_work_id(issue);
    let config = Workspace::new("true").install(&env);
    crate::support::register_workspace(&env, &config);

    let mut daemon = env.start_daemon(&config, 1);
    // `wait_until` fails as soon as the daemon exits.
    wait_until(
        Duration::from_secs(20),
        "item to reach done with a registered workspace",
        &mut daemon,
        || (env.phase_of(&work_id).as_deref() == Some("done")).then_some(()),
    );
}
