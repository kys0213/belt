//! Shared pieces of scenarios 2-7: a workspace yaml builder around one
//! label-triggered `analyze` state, and small readers over `belt` JSON.

use serde_json::Value;

use crate::harness::{E2eEnv, REPO};

pub const TRIGGER: &str = "belt:analyze";
pub const STATE: &str = "analyze";

/// `on_done` that removes the trigger label of the item's issue, so a
/// finished issue is not collected again.
pub fn remove_label_script() -> String {
    format!(
        r#"n="${{WORK_ID#*#}}"; gh issue edit "${{n%%:*}}" --repo {REPO} --remove-label "{TRIGGER}""#
    )
}

/// One-state workspace. Every field maps to one yaml key; see
/// `qa-expectations.md` §0.2 for why each default is what it is.
pub struct Workspace {
    pub handler: String,
    pub on_done: String,
    /// When set, on_fail appends `$WORK_ID` as one line to this file
    /// (a work_id holds `/`, so it cannot be a file name).
    pub on_fail_log: Option<std::path::PathBuf>,
    /// Lines under `escalation:`, e.g. `"1: retry\n2: hitl"`.
    pub escalation: &'static str,
    pub scan_interval_secs: u64,
    /// `notifications.origin.respond.allow`; empty disables reply polling.
    pub respond_allow: Vec<&'static str>,
}

impl Workspace {
    pub fn new(handler: impl Into<String>) -> Self {
        Self {
            handler: handler.into(),
            on_done: remove_label_script(),
            on_fail_log: None,
            escalation: "1: retry\n2: retry_with_comment\n3: hitl",
            scan_interval_secs: 3600,
            respond_allow: Vec::new(),
        }
    }

    pub fn yaml(&self) -> String {
        let indent = |text: &str, n: usize| {
            text.lines()
                .map(|l| format!("{}{l}\n", " ".repeat(n)))
                .collect::<String>()
        };
        let on_fail = match &self.on_fail_log {
            Some(log) => format!(
                "        on_fail:\n          - script: {:?}\n",
                format!("echo \"$WORK_ID\" >> \"{}\"", log.display())
            ),
            None => String::new(),
        };
        let notifications = if self.respond_allow.is_empty() {
            String::new()
        } else {
            format!(
                "notifications:\n  origin:\n    respond:\n      allow: [{}]\n",
                self.respond_allow
                    .iter()
                    .map(|u| format!("{u:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        format!(
            r#"name: e2e-fake
concurrency: 1
sources:
  github:
    url: https://github.com/{REPO}
    scan_interval_secs: {scan}
    states:
      {STATE}:
        trigger:
          label: "{TRIGGER}"
        handlers:
          - script: {handler:?}
        on_done:
          - script: {on_done:?}
{on_fail}    escalation:
{escalation}      terminal: skip
runtime:
  default: claude
{notifications}"#,
            scan = self.scan_interval_secs,
            handler = self.handler,
            on_done = self.on_done,
            escalation = indent(self.escalation, 6),
        )
    }

    /// Write the yaml; returns the config path. The workspace is not
    /// registered with `belt workspace add` (see [`register_workspace`]).
    pub fn install(&self, env: &E2eEnv) -> std::path::PathBuf {
        env.write_workspace(&self.yaml())
    }
}

/// `belt workspace add --config <config>`. Only the CLI's `queue done`,
/// `queue hitl` and `context` need it. Registering also makes the daemon's
/// `pr_review_scan` cron scan it, which currently crashes the daemon (see
/// `scenarios_lineage::finding_daemon_survives_a_registered_workspace`), so
/// scenarios that need no registration leave it out.
pub fn register_workspace(env: &E2eEnv, config: &std::path::Path) {
    let out = env.belt(&["workspace", "add", "--config", config.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "belt workspace add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `github:<repo>#<issue>:analyze`, the first item of the issue's lineage.
pub fn first_work_id(issue: u64) -> String {
    format!("github:{REPO}#{issue}:{STATE}")
}

/// Worktree directory name of `work_id` (non-alphanumerics but `-`/`_`
/// become `_`).
pub fn worktree_name(work_id: &str) -> String {
    work_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The `phase_enter` rows of a transition list.
pub fn phase_enters(transitions: &[Value]) -> Vec<&Value> {
    transitions
        .iter()
        .filter(|t| t["kind"] == "phase_enter")
        .collect()
}

/// Rows of one `kind`.
pub fn rows_of<'a>(transitions: &'a [Value], kind: &str) -> Vec<&'a Value> {
    transitions.iter().filter(|t| t["kind"] == kind).collect()
}

/// Number of progress comments `belt: \`<work_id>\` <event>` on `issue`.
pub fn progress_comments(env: &E2eEnv, issue: u64, work_id: &str, event: &str) -> usize {
    let needle = format!("belt: `{work_id}` {event}");
    env.gh
        .issue_comments(issue)
        .iter()
        .filter(|c| c.body.lines().any(|l| l == needle))
        .count()
}

/// Every progress comment of `event` on `issue`, whatever the item.
pub fn progress_comments_any(env: &E2eEnv, issue: u64, event: &str) -> usize {
    let suffix = format!("` {event}");
    env.gh
        .issue_comments(issue)
        .iter()
        .filter(|c| {
            c.body
                .lines()
                .any(|l| l.starts_with("belt: `") && l.ends_with(&suffix))
        })
        .count()
}

/// Work ids on_fail ran for, in order (see [`Workspace::on_fail_log`]).
pub fn on_fail_runs(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default()
}
