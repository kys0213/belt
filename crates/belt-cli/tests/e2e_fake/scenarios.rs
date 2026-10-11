//! Fake-environment E2E scenarios: the real `belt` binary against fake
//! GitHub and a fake runtime. Assertions use observable surfaces only —
//! `belt` CLI output, the fake GitHub state and call log, the filesystem.

use std::time::Duration;

use crate::harness::{E2eEnv, REPO, wait_until};

const TRIGGER: &str = "belt:implement";

/// Workspace with one label-triggered state whose handler is `handler`.
/// `on_done` removes the trigger label, as a real workspace does, so the
/// finished issue is not collected again.
fn workspace_yaml(handler: &str) -> String {
    format!(
        r#"name: e2e-fake
concurrency: 1
sources:
  github:
    url: https://github.com/{REPO}
    scan_interval_secs: 1
    states:
      implement:
        trigger:
          label: "{TRIGGER}"
        handlers:
          - script: {handler:?}
        on_done:
          - script: 'n="${{WORK_ID#*#}}"; gh issue edit "${{n%%:*}}" --repo {REPO} --remove-label "{TRIGGER}"'
    escalation:
      1: retry
      2: hitl
      terminal: skip
runtime:
  default: claude
"#
    )
}

/// Scenario 1: a labeled issue is collected, its script handler succeeds,
/// the item reaches Done and its worktree is removed.
pub fn smoke_labeled_issue_reaches_done_and_worktree_is_cleaned() {
    let env = E2eEnv::new();
    let issue = env
        .gh
        .seed_issue("Add a greeting", "Please add hello.txt", &[TRIGGER]);
    let work_id = format!("github:{REPO}#{issue}:implement");
    // The handler leaves a marker outside the worktree, proving it ran there.
    let marker = env.tmp_dir.join("handler-ran");
    let workspace = env.write_workspace(&workspace_yaml(&format!(
        "echo hello > hello.txt && pwd > {}",
        marker.display()
    )));

    let mut daemon = env.start_daemon(&workspace, 1);

    wait_until(
        Duration::from_secs(20),
        "item to reach done",
        &mut daemon,
        || (env.phase_of(&work_id).as_deref() == Some("done")).then_some(()),
    );
    // The store commits Done before the worktree is removed; wait for the
    // cleanup to finish instead of racing it with the shutdown.
    wait_until(
        Duration::from_secs(10),
        "worktree branch to be deleted after done",
        &mut daemon,
        || (env.git_branches() == ["main"]).then_some(()),
    );
    let clean_exit = daemon.stop(Duration::from_secs(10));

    // The handler ran inside a belt worktree, not in the target checkout.
    let handler_cwd = std::fs::read_to_string(&marker).expect("handler never ran");
    let worktrees_root = env.belt_home.join("worktrees");
    assert!(
        handler_cwd
            .trim()
            .starts_with(worktrees_root.to_str().unwrap()),
        "handler ran in {handler_cwd:?}, expected under {}",
        worktrees_root.display()
    );

    // Transition history, as `belt queue show` reports it.
    let shown = env.belt_json(&["queue", "show", &work_id, "--json"]);
    let phases: Vec<&str> = shown["transitions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["to_phase"].as_str())
        .collect();
    for phase in ["pending", "ready", "running", "completed", "done"] {
        assert!(
            phases.contains(&phase),
            "missing {phase} in history {phases:?}"
        );
    }

    // Exactly one item: the label removal stopped re-collection.
    assert_eq!(env.queue_items().len(), 1, "issue collected more than once");
    assert!(env.gh.issue_labels(issue).is_empty());

    // Worktree removed from disk, from git, and its branch deleted.
    assert!(
        env.worktree_dirs().is_empty(),
        "worktree left behind: {:?}",
        env.worktree_dirs()
    );
    assert_eq!(
        env.git_worktrees(),
        vec![env.repo_dir.display().to_string()]
    );
    assert_eq!(env.git_branches(), vec!["main".to_string()]);

    // Only emulated gh commands were used; the evaluator ran on the fake runtime.
    let calls = env.gh.calls();
    let unsupported: Vec<String> = calls
        .iter()
        .filter(|c| c.exit_code == 2)
        .map(|c| c.line())
        .collect();
    assert!(
        unsupported.is_empty(),
        "unsupported gh calls: {unsupported:#?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.line().starts_with("gh issue list --label belt:implement")),
        "no collect call in {calls:#?}"
    );
    assert!(
        !env.claude.calls().is_empty(),
        "evaluator never called the runtime"
    );
    assert!(clean_exit, "daemon ignored SIGINT:\n{}", daemon.log());
}

/// `belt stop` must shut the daemon down through the graceful path.
pub fn finding_belt_stop_shuts_daemon_down_gracefully() {
    let env = E2eEnv::new();
    let workspace = env.write_workspace(&workspace_yaml("true"));
    let mut daemon = env.start_daemon(&workspace, 1);
    let pid_file = env.belt_home.join("daemon.pid");
    wait_until(
        Duration::from_secs(10),
        "daemon to enter its run loop",
        &mut daemon,
        || (pid_file.exists() && env.daemon_log().contains("belt daemon started")).then_some(()),
    );

    let out = env.belt(&["stop"]);
    assert!(
        out.status.success(),
        "belt stop failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        daemon.wait_exit(Duration::from_secs(10)),
        "daemon still running 10s after belt stop"
    );
    assert!(
        daemon.log().contains("belt daemon stopped"),
        "daemon did not go through graceful shutdown; log:\n{}",
        daemon.log()
    );
    assert!(
        !pid_file.exists(),
        "graceful shutdown removes daemon.pid, but it is still there"
    );
}
