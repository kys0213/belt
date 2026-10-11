//! Scenario 3: a HITL request opened by escalation is answered on GitHub
//! (`/belt done` by an allowed user); a later CLI answer is refused as
//! `already_handled`.

use std::time::Duration;

use serde_json::Value;

use crate::harness::{DaemonGuard, E2eEnv, wait_until};
use crate::support::{
    TRIGGER, Workspace, first_work_id, on_fail_runs, progress_comments, remove_label_script,
    rows_of,
};

const RESPONDER: &str = "octo";
/// A GitHub user outside `respond.allow`.
const OUTSIDER: &str = "mallory";
const NEEDS_HUMAN: &str = "belt:needs-human";

/// A daemon whose only item failed into Hitl, with its open request id.
struct HitlSetup {
    // Declared first so it drops first: the daemon must stop before the
    // sandbox it runs in is removed.
    daemon: DaemonGuard,
    env: E2eEnv,
    issue: u64,
    work_id: String,
    hitl_id: String,
    on_fail_log: std::path::PathBuf,
}

fn open_hitl() -> HitlSetup {
    open_hitl_with(|_, _| {})
}

/// [`open_hitl`] with the workspace adjusted by `tweak` before install.
fn open_hitl_with(tweak: impl FnOnce(&E2eEnv, &mut Workspace)) -> HitlSetup {
    let env = E2eEnv::new();
    let issue = env.gh.seed_issue("Analyze", "body", &[TRIGGER]);
    let work_id = first_work_id(issue);
    let on_fail_log = env.tmp_dir.join("on_fail.log");
    let mut ws = Workspace::new("exit 1");
    ws.escalation = "1: hitl";
    ws.respond_allow = vec![RESPONDER];
    ws.on_fail_log = Some(on_fail_log.clone());
    tweak(&env, &mut ws);
    let config = ws.install(&env);

    let mut daemon = env.start_daemon(&config, 1);
    let request = wait_until(
        Duration::from_secs(20),
        "an open HITL request for the item",
        &mut daemon,
        || open_request_of(&env, &work_id),
    );
    let hitl_id = request["hitl_id"].as_str().unwrap().to_string();
    HitlSetup {
        daemon,
        env,
        issue,
        work_id,
        hitl_id,
        on_fail_log,
    }
}

fn open_request_of(env: &E2eEnv, work_id: &str) -> Option<Value> {
    match env.belt_json(&["hitl", "list", "--format", "json"]) {
        Value::Array(rows) => rows.into_iter().find(|r| r["work_id"] == work_id),
        other => panic!("hitl list is not an array: {other}"),
    }
}

/// Wait for the HITL request comment the origin channel posts on the issue.
fn wait_hitl_comment(s: &mut HitlSetup) -> String {
    let marker = format!("<!-- belt:hitl_id={} -->", s.hitl_id);
    let (env, issue) = (&s.env, s.issue);
    wait_until(
        Duration::from_secs(15),
        "HITL request comment on the issue",
        &mut s.daemon,
        || {
            env.gh
                .issue_comments(issue)
                .into_iter()
                .find(|c| c.body.lines().nth(1) == Some(marker.as_str()))
                .map(|c| c.body)
        },
    )
}

/// Post `/belt done` as the allowed user and wait for the item to be Done
/// and its worktree cleaned.
fn answer_done_on_github(s: &mut HitlSetup) {
    s.env.gh.add_issue_comment(s.issue, RESPONDER, "/belt done");
    let (env, work_id) = (&s.env, s.work_id.as_str());
    wait_until(
        Duration::from_secs(20),
        "item to reach done after the GitHub answer",
        &mut s.daemon,
        || (env.phase_of(work_id).as_deref() == Some("done")).then_some(()),
    );
    wait_until(
        Duration::from_secs(10),
        "worktree to be cleaned after done",
        &mut s.daemon,
        || (env.worktree_dirs().is_empty() && env.git_branches() == ["main"]).then_some(()),
    );
}

/// `(status, action, respondent, via, post_processed_at)` of the request.
fn stored_request(env: &E2eEnv, hitl_id: &str) -> (String, String, String, String, Option<String>) {
    env.db()
        .query_row(
            "SELECT status, action, respondent, via, post_processed_at FROM hitl_requests WHERE hitl_id = ?1",
            [hitl_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .expect("hitl request row")
}

pub fn hitl_resolved_on_github_then_late_cli_answer_is_already_handled() {
    let mut s = open_hitl();

    // 1. Hitl with one open request opened by the escalation.
    assert_eq!(s.env.phase_of(&s.work_id).as_deref(), Some("hitl"));
    let request = open_request_of(&s.env, &s.work_id).unwrap();
    assert_eq!(request["status"], "open", "{request}");
    assert_eq!(request["reason"], "retry_max_exceeded", "{request}");

    // 2. The origin channel posts the request and the `failed` progress.
    let body = wait_hitl_comment(&mut s);
    assert_eq!(body.lines().next(), Some("<!-- belt:origin -->"), "{body}");
    assert!(
        body.contains("needs a human decision (retry_max_exceeded)"),
        "{body}"
    );
    assert!(body.contains(&s.hitl_id), "{body}");
    let (env, issue, work_id) = (&s.env, s.issue, s.work_id.clone());
    wait_until(
        Duration::from_secs(10),
        "failed progress comment",
        &mut s.daemon,
        || (progress_comments(env, issue, &work_id, "failed") == 1).then_some(()),
    );
    let shown = s
        .env
        .belt_json(&["hitl", "show", &s.work_id, "--format", "json"]);
    let origin = shown["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["channel"] == "origin")
        .cloned();
    assert_eq!(
        origin.as_ref().map(|d| d["status"].clone()),
        Some(Value::from("sent")),
        "{shown}"
    );
    // escalation `hitl` runs on_fail.
    let (log, work_id) = (&s.on_fail_log, s.work_id.clone());
    wait_until(
        Duration::from_secs(10),
        "on_fail to run on escalation hitl",
        &mut s.daemon,
        || (on_fail_runs(log) == [work_id.clone()]).then_some(()),
    );

    // 4. An allowed `/belt done` wins; post-processing finishes the item.
    answer_done_on_github(&mut s);
    let (status, action, by, via, post_processed) = stored_request(&s.env, &s.hitl_id);
    assert_eq!(
        (status.as_str(), action.as_str(), by.as_str(), via.as_str()),
        ("resolved", "done", RESPONDER, "origin")
    );
    assert!(post_processed.is_some(), "post_processed_at not recorded");

    // 5. A late CLI answer loses and is recorded as rejected.
    let (code, answer) = s.env.belt_answer(&[
        "hitl",
        "respond",
        "--hitl-id",
        &s.hitl_id,
        "--action",
        "skip",
        "--json",
    ]);
    assert_eq!(code, 1, "{answer}");
    assert_eq!(answer["success"], false, "{answer}");
    assert_eq!(answer["reason"], "already_handled", "{answer}");
    assert_eq!(answer["by"], RESPONDER, "{answer}");
    assert_eq!(answer["via"], "origin", "{answer}");
    assert_eq!(answer["action"], "done", "{answer}");
    assert!(answer["at"].is_string(), "{answer}");
    assert_eq!(answer["work_id"], s.work_id.as_str(), "{answer}");
    assert_eq!(s.env.phase_of(&s.work_id).as_deref(), Some("done"));
    let log = s.env.transitions(&s.work_id);
    let rejected = rows_of(&log, "hitl_response_rejected");
    assert_eq!(rejected.len(), 1, "{log:#?}");
    assert_eq!(rejected[0]["reason"], "already_handled");
    assert_eq!(rejected[0]["actor"], "cli");
}

/// D1: the lifecycle hook labels the issue `belt:needs-human` when the
/// request opens and removes the label once it is resolved.
/// (spec: lifecycle-hook.md:110, 144, 166, 245, 248)
pub fn hitl_needs_human_label_added_on_open_and_removed_on_resolve() {
    let mut s = open_hitl();
    let (env, issue) = (&s.env, s.issue);
    wait_until(
        Duration::from_secs(10),
        "belt:needs-human label on the issue",
        &mut s.daemon,
        || {
            env.gh
                .issue_labels(issue)
                .iter()
                .any(|l| l == NEEDS_HUMAN)
                .then_some(())
        },
    );
    answer_done_on_github(&mut s);
    let (env, issue) = (&s.env, s.issue);
    wait_until(
        Duration::from_secs(10),
        "belt:needs-human label removed after resolve",
        &mut s.daemon,
        || (!env.gh.issue_labels(issue).iter().any(|l| l == NEEDS_HUMAN)).then_some(()),
    );
}

/// D2: the winning answer is logged as a `hitl_resolved` transition row.
/// (spec: data-model.md:66)
pub fn hitl_resolution_is_logged_as_hitl_resolved_transition() {
    let mut s = open_hitl();
    wait_hitl_comment(&mut s);
    answer_done_on_github(&mut s);
    let log = s.env.transitions(&s.work_id);
    let resolved = rows_of(&log, "hitl_resolved");
    assert_eq!(resolved.len(), 1, "no hitl_resolved row: {log:#?}");
    assert_eq!(resolved[0]["actor"], "origin", "{}", resolved[0]);
    assert_eq!(resolved[0]["reason"], "done", "{}", resolved[0]);
}

/// Comments the origin channel itself posted on `issue`.
fn origin_comments(env: &E2eEnv, issue: u64) -> usize {
    env.gh
        .issue_comments(issue)
        .iter()
        .filter(|c| c.body.lines().next() == Some("<!-- belt:origin -->"))
        .count()
}

/// `(status, post_processed_at)` of the request; readable while it is open.
fn request_progress(env: &E2eEnv, hitl_id: &str) -> (String, Option<String>) {
    env.db()
        .query_row(
            "SELECT status, post_processed_at FROM hitl_requests WHERE hitl_id = ?1",
            [hitl_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("hitl request row")
}

/// Times the origin channel polled `issue` for responses.
fn comment_polls(env: &E2eEnv, issue: u64) -> usize {
    let prefix = format!("gh issue view {issue} ");
    env.gh
        .calls()
        .iter()
        .map(|c| c.line())
        .filter(|l| l.starts_with(&prefix) && l.contains("comments"))
        .count()
}

/// `hitl_response_rejected` rows of `work_id` as `(actor, reason)`.
fn rejections(env: &E2eEnv, work_id: &str) -> Vec<(String, String)> {
    rows_of(&env.transitions(work_id), "hitl_response_rejected")
        .iter()
        .map(|r| {
            (
                r["actor"].as_str().unwrap_or_default().to_string(),
                r["reason"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Scenario 3 edges: an answer from outside the allowlist is rejected once
/// (re-polling the same comment adds nothing) and gets no reply; the
/// allowed answer wins once; `belt hitl respond <work_id>` sees the
/// confirmed request as `already_handled` until it is post-processed and
/// `not_found` after.
pub fn hitl_unauthorized_answer_is_rejected_once_and_work_id_answer_follows_post_processing() {
    const RELEASE: &str = "release_on_done";
    let mut s = open_hitl_with(|env, ws| {
        // on_done holds post-processing until the test releases it, so the
        // "confirmed, not yet post-processed" window is as long as needed.
        // The wait registers itself for the guard's cleanup and gives up
        // after ~120s, so a failing test leaks no endless loop.
        ws.on_done = format!(
            r#"echo $$ > "{pids}/on_done"; i=0; while [ ! -f "{release}" ] && [ $i -lt 1200 ]; do sleep 0.1; i=$((i+1)); done; {remove}"#,
            pids = env.handler_pid_dir().display(),
            release = env.tmp_dir.join(RELEASE).display(),
            remove = remove_label_script()
        );
    });
    let release = s.env.tmp_dir.join(RELEASE);
    wait_hitl_comment(&mut s);
    let (env, issue, work_id) = (&s.env, s.issue, s.work_id.clone());
    wait_until(
        Duration::from_secs(10),
        "failed progress comment",
        &mut s.daemon,
        || (progress_comments(env, issue, &work_id, "failed") == 1).then_some(()),
    );
    let posted = origin_comments(env, issue);

    // 1. An outsider's `/belt done` is rejected `unauthorized`, once.
    s.env.gh.add_issue_comment(s.issue, OUTSIDER, "/belt done");
    let (env, work_id) = (&s.env, s.work_id.as_str());
    wait_until(
        Duration::from_secs(15),
        "unauthorized rejection of the outsider's answer",
        &mut s.daemon,
        || (!rejections(env, work_id).is_empty()).then_some(()),
    );
    let polled = comment_polls(env, issue);
    wait_until(
        Duration::from_secs(15),
        "two more polls that see the outsider's comment again",
        &mut s.daemon,
        || (comment_polls(env, issue) >= polled + 2).then_some(()),
    );
    assert_eq!(
        rejections(&s.env, &s.work_id),
        [("origin".to_string(), "unauthorized".to_string())]
    );
    let request = open_request_of(&s.env, &s.work_id).expect("request no longer open");
    assert_eq!(request["status"], "open", "{request}");
    assert_eq!(s.env.phase_of(&s.work_id).as_deref(), Some("hitl"));
    assert_eq!(
        origin_comments(&s.env, s.issue),
        posted,
        "a reply was posted"
    );

    // 2. The allowed answer wins; post-processing waits on on_done.
    s.env.gh.add_issue_comment(s.issue, RESPONDER, "/belt done");
    let (env, hitl_id) = (&s.env, s.hitl_id.as_str());
    wait_until(
        Duration::from_secs(15),
        "the allowed answer to resolve the request",
        &mut s.daemon,
        || (request_progress(env, hitl_id).0 == "resolved").then_some(()),
    );
    let (_, post_processed) = request_progress(&s.env, &s.hitl_id);
    assert!(
        post_processed.is_none(),
        "post-processed before on_done ran"
    );

    // 3. Confirmed but not post-processed: the item still names it.
    let (code, answer) = s
        .env
        .belt_answer(&["hitl", "respond", &s.work_id, "--action", "skip", "--json"]);
    assert_eq!(code, 1, "{answer}");
    assert_eq!(answer["reason"], "already_handled", "{answer}");
    assert_eq!(answer["by"], RESPONDER, "{answer}");
    assert_eq!(answer["via"], "origin", "{answer}");
    assert_eq!(answer["action"], "done", "{answer}");

    // 4. Post-processed: the request is the item's past, not found.
    std::fs::write(&release, "").unwrap();
    let (env, work_id, hitl_id) = (&s.env, s.work_id.as_str(), s.hitl_id.as_str());
    wait_until(
        Duration::from_secs(15),
        "item done and request post-processed",
        &mut s.daemon,
        || {
            (env.phase_of(work_id).as_deref() == Some("done")
                && request_progress(env, hitl_id).1.is_some())
            .then_some(())
        },
    );
    let (code, answer) = s
        .env
        .belt_answer(&["hitl", "respond", &s.work_id, "--action", "skip", "--json"]);
    assert_eq!(code, 1, "{answer}");
    assert_eq!(answer["reason"], "not_found", "{answer}");

    // The winning comment was settled once: the only origin rejection is
    // the outsider's; the CLI's late answer added its own.
    assert_eq!(
        rejections(&s.env, &s.work_id),
        [
            ("origin".to_string(), "unauthorized".to_string()),
            ("cli".to_string(), "already_handled".to_string()),
        ]
    );
    assert_eq!(s.env.phase_of(&s.work_id).as_deref(), Some("done"));
}
