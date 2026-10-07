//! GitHub origin channel — progress and HITL messages as issue comments,
//! responses read back from the same issue.
//!
//! The channel never touches labels (the lifecycle hook owns them).
//!
//! Wire conventions:
//! - Every comment belt posts starts with [`OWN_MARKER`]; polling skips those,
//!   so belt's own messages are never read back as responses. An author check
//!   would not work because belt posts as whatever account `gh` is logged in to.
//! - A message tied to a HITL request carries a hidden `hitl_id` token line.
//! - Explicit responses: `/belt <done|retry|skip|replan> [hitl_id]` and
//!   `/belt confirm [hitl_id]`. Any other comment is natural language.
//! - A comment without a `hitl_id` belongs to the single HITL request polled
//!   for that issue; with several candidates it has no correlation clue.
//! - `external_id` and [`MessageRef`] are the comment URL, stable across polls.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, FixedOffset};
use serde::Deserialize;

use belt_core::hitl::{HitlAction, HitlId};
use belt_core::notification::{
    HitlRef, InboundBody, InboundResponse, MessageRef, NotificationChannel, ORIGIN_CHANNEL,
    OutboundMessage, PollTarget, ResponseInbox,
};
use belt_core::platform::ShellExecutor;

/// First line of every comment belt posts.
pub const OWN_MARKER: &str = "<!-- belt:origin -->";
const TOKEN_PREFIX: &str = "<!-- belt:hitl_id=";
const TOKEN_SUFFIX: &str = " -->";
const COMMAND_PREFIX: &str = "/belt";
const CONFIRM_WORD: &str = "confirm";

/// Configuration of the GitHub origin channel.
#[derive(Debug, Clone)]
pub struct GitHubChannelConfig {
    /// Repository in `owner/repo` format.
    pub repo: String,
}

impl GitHubChannelConfig {
    /// Create a config for the given `owner/repo`.
    pub fn new(repo: &str) -> Self {
        Self {
            repo: repo.to_string(),
        }
    }
}

/// Origin channel for GitHub issues, built on the `gh` CLI.
pub struct GitHubOriginChannel {
    config: GitHubChannelConfig,
    shell: Arc<dyn ShellExecutor>,
}

impl GitHubOriginChannel {
    /// Create the channel; `shell` runs the `gh` commands.
    pub fn new(config: GitHubChannelConfig, shell: Arc<dyn ShellExecutor>) -> Self {
        Self { config, shell }
    }

    /// Issue number of a `github:owner/repo#NUMBER[:state]` work_id of this
    /// channel's repository. `None` when the item belongs to another source.
    fn issue_number<'a>(&self, work_id: &'a str) -> Option<&'a str> {
        let rest = work_id.strip_prefix("github:")?;
        let (repo, after) = rest.split_once('#')?;
        if repo != self.config.repo {
            return None;
        }
        let number = after.split(':').next()?;
        (!number.is_empty() && number.chars().all(|c| c.is_ascii_digit())).then_some(number)
    }

    async fn gh(&self, command: &str) -> Result<String> {
        let output = self
            .shell
            .execute(command, Path::new("."), &HashMap::new())
            .await
            .context("failed to run gh")?;
        if !output.success() {
            let code = output.exit_code.unwrap_or(-1);
            bail!("gh command failed (exit {code}): {}", output.stderr);
        }
        Ok(output.stdout)
    }
}

/// Single-quote `s` for a POSIX shell.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn comment_body(msg: &OutboundMessage) -> String {
    let mut body = String::from(OWN_MARKER);
    body.push('\n');
    if let Some(id) = &msg.hitl_id {
        body.push_str(&format!("{TOKEN_PREFIX}{id}{TOKEN_SUFFIX}\n"));
    }
    body.push_str(&msg.text);
    body
}

#[async_trait]
impl NotificationChannel for GitHubOriginChannel {
    fn name(&self) -> &str {
        ORIGIN_CHANNEL
    }

    async fn notify(&self, msg: &OutboundMessage) -> Result<Option<MessageRef>> {
        let number = self.issue_number(&msg.work_id).with_context(|| {
            format!(
                "work_id is not a {} issue: {}",
                self.config.repo, msg.work_id
            )
        })?;
        let command = format!(
            "gh issue comment {number} --repo {repo} --body {body}",
            repo = shell_quote(&self.config.repo),
            body = shell_quote(&comment_body(msg)),
        );
        let stdout = self.gh(&command).await?;
        let url = stdout.trim();
        Ok((!url.is_empty()).then(|| MessageRef(url.to_string())))
    }

    fn inbox(&self) -> Option<&dyn ResponseInbox> {
        Some(self)
    }
}

#[derive(Deserialize)]
struct IssueComments {
    comments: Vec<Comment>,
}

#[derive(Deserialize)]
struct Comment {
    url: String,
    body: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    author: Option<Author>,
}

#[derive(Deserialize)]
struct Author {
    login: String,
}

/// A parsed `/belt ...` command.
enum Command {
    Action(HitlAction),
    Confirm,
}

/// `Some((command, explicit hitl_id))` when the whole comment is an explicit
/// command; `None` for natural language.
fn parse_command(body: &str) -> Option<(Command, Option<HitlId>)> {
    let tokens: Vec<&str> = body.split_whitespace().collect();
    if !(2..=3).contains(&tokens.len()) || tokens[0] != COMMAND_PREFIX {
        return None;
    }
    let command = if tokens[1] == CONFIRM_WORD {
        Command::Confirm
    } else {
        Command::Action(tokens[1].parse().ok()?)
    };
    Some((command, tokens.get(2).map(|id| HitlId::new(*id))))
}

fn parse_time(s: &str) -> Result<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(s).with_context(|| format!("invalid RFC 3339 timestamp: {s}"))
}

type IssueTargets<'a> = Vec<(&'a PollTarget, DateTime<FixedOffset>)>;

#[async_trait]
impl ResponseInbox for GitHubOriginChannel {
    async fn poll(&self, targets: &[PollTarget]) -> Result<Vec<InboundResponse>> {
        // Targets of other sources are not this channel's business.
        let mut by_issue: Vec<(&str, IssueTargets)> = Vec::new();
        for target in targets {
            let Some(number) = self.issue_number(&target.work_id) else {
                continue;
            };
            let since = parse_time(&target.since)?;
            match by_issue.iter_mut().find(|(n, _)| *n == number) {
                Some((_, group)) => group.push((target, since)),
                None => by_issue.push((number, vec![(target, since)])),
            }
        }

        let mut responses = Vec::new();
        for (number, group) in by_issue {
            let command = format!(
                "gh issue view {number} --repo {repo} --json comments",
                repo = shell_quote(&self.config.repo),
            );
            let stdout = self.gh(&command).await?;
            let issue: IssueComments = serde_json::from_str(&stdout)
                .with_context(|| format!("unexpected gh output for issue #{number}"))?;

            for comment in issue.comments {
                if comment.body.starts_with(OWN_MARKER) {
                    continue;
                }
                let created = parse_time(&comment.created_at)?;
                let candidates: Vec<&PollTarget> = group
                    .iter()
                    .filter(|(_, since)| *since <= created)
                    .map(|(t, _)| *t)
                    .collect();
                let text = comment.body.trim();
                if candidates.is_empty() || text.is_empty() {
                    continue;
                }
                let (body, explicit) = match parse_command(text) {
                    Some((Command::Action(a), id)) => (InboundBody::Action(a), id),
                    Some((Command::Confirm, id)) => (InboundBody::Confirm, id),
                    None => (InboundBody::Text(text.to_string()), None),
                };
                let hitl_ref = match explicit {
                    Some(id) => candidates
                        .iter()
                        .any(|t| t.hitl_id == id)
                        .then_some(HitlRef::Token(id)),
                    None if candidates.len() == 1 => {
                        Some(HitlRef::Token(candidates[0].hitl_id.clone()))
                    }
                    None => None,
                };
                responses.push(InboundResponse {
                    external_id: comment.url,
                    respondent: comment.author.map(|a| a.login).unwrap_or_default(),
                    hitl_ref,
                    body,
                });
            }
        }
        Ok(responses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::error::BeltError;
    use belt_core::notification::MessageKind;
    use belt_core::platform::ShellOutput;
    use std::sync::Mutex;

    /// Records every command and replies with one fixed output.
    struct RecordingShell {
        commands: Mutex<Vec<String>>,
        output: ShellOutput,
    }

    impl RecordingShell {
        fn ok(stdout: &str) -> Arc<Self> {
            Self::with(ShellOutput {
                exit_code: Some(0),
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        }

        fn with(output: ShellOutput) -> Arc<Self> {
            Arc::new(Self {
                commands: Mutex::new(Vec::new()),
                output,
            })
        }

        fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ShellExecutor for RecordingShell {
        async fn execute(
            &self,
            command: &str,
            _working_dir: &Path,
            _env_vars: &HashMap<String, String>,
        ) -> std::result::Result<ShellOutput, BeltError> {
            self.commands.lock().unwrap().push(command.to_string());
            Ok(self.output.clone())
        }
    }

    fn channel(shell: &Arc<RecordingShell>) -> GitHubOriginChannel {
        GitHubOriginChannel::new(GitHubChannelConfig::new("org/repo"), shell.clone())
    }

    fn msg(work_id: &str, hitl: Option<&str>, text: &str) -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Reply,
            work_id: work_id.to_string(),
            hitl_id: hitl.map(HitlId::new),
            text: text.to_string(),
        }
    }

    fn target(hitl: &str, work_id: &str, since: &str) -> PollTarget {
        PollTarget {
            hitl_id: HitlId::new(hitl),
            work_id: work_id.to_string(),
            message_ref: None,
            since: since.to_string(),
        }
    }

    fn comments_json(comments: &[(&str, &str, &str, &str)]) -> String {
        let items: Vec<serde_json::Value> = comments
            .iter()
            .map(|(n, login, body, at)| {
                serde_json::json!({
                    "id": format!("IC_{n}"),
                    "url": format!("https://github.com/org/repo/issues/42#issuecomment-{n}"),
                    "author": {"login": login},
                    "body": body,
                    "createdAt": at,
                })
            })
            .collect();
        serde_json::json!({ "comments": items }).to_string()
    }

    const WID: &str = "github:org/repo#42:implement";
    const SINCE: &str = "2026-10-07T00:00:00Z";

    #[tokio::test]
    async fn notify_comments_on_issue_with_token_and_returns_url() {
        let url = "https://github.com/org/repo/issues/42#issuecomment-1";
        let shell = RecordingShell::ok(&format!("{url}\n"));
        let r = channel(&shell)
            .notify(&msg(WID, Some("h-1"), "needs a human"))
            .await
            .unwrap();
        assert_eq!(r, Some(MessageRef(url.to_string())));
        let cmds = shell.commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].starts_with("gh issue comment 42 --repo 'org/repo' --body '"));
        assert!(cmds[0].contains(OWN_MARKER));
        assert!(cmds[0].contains("<!-- belt:hitl_id=h-1 -->"));
        assert!(cmds[0].contains("needs a human"));
    }

    #[tokio::test]
    async fn notify_without_hitl_id_has_no_token_and_never_edits_labels() {
        let shell = RecordingShell::ok("");
        let r = channel(&shell)
            .notify(&msg(WID, None, "started"))
            .await
            .unwrap();
        assert_eq!(r, None);
        let cmd = &shell.commands()[0];
        assert!(!cmd.contains("hitl_id="));
        assert!(!cmd.contains("--add-label") && !cmd.contains("issue edit"));
    }

    #[tokio::test]
    async fn notify_quotes_single_quotes_in_text() {
        let shell = RecordingShell::ok("u");
        channel(&shell)
            .notify(&msg(WID, None, "it's $(unsafe)"))
            .await
            .unwrap();
        assert!(shell.commands()[0].contains(r"it'\''s $(unsafe)"));
    }

    #[tokio::test]
    async fn notify_fails_for_foreign_work_id_without_calling_gh() {
        let shell = RecordingShell::ok("");
        let err = channel(&shell)
            .notify(&msg("github:other/repo#1:x", None, "t"))
            .await;
        assert!(err.is_err());
        assert!(shell.commands().is_empty());
    }

    #[tokio::test]
    async fn gh_failure_surfaces_as_err() {
        let shell = RecordingShell::with(ShellOutput {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "boom".to_string(),
        });
        let c = channel(&shell);
        let e = c.notify(&msg(WID, None, "t")).await.unwrap_err();
        assert!(e.to_string().contains("boom"));
        let e = c.poll(&[target("h-1", WID, SINCE)]).await.unwrap_err();
        assert!(e.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn poll_normalizes_explicit_actions_text_and_confirm() {
        let json = comments_json(&[
            ("1", "alice", "/belt done", "2026-10-07T01:00:00Z"),
            ("2", "bob", "/belt retry h-1", "2026-10-07T02:00:00Z"),
            ("3", "carol", "skip it please", "2026-10-07T03:00:00Z"),
            ("4", "alice", "/belt confirm", "2026-10-07T04:00:00Z"),
            ("5", "alice", "/belt bogus", "2026-10-07T05:00:00Z"),
        ]);
        let shell = RecordingShell::ok(&json);
        let rs = channel(&shell)
            .poll(&[target("h-1", WID, SINCE)])
            .await
            .unwrap();
        assert_eq!(
            shell.commands(),
            vec!["gh issue view 42 --repo 'org/repo' --json comments"]
        );
        let tok = Some(HitlRef::Token(HitlId::new("h-1")));
        assert_eq!(rs.len(), 5);
        assert_eq!(rs[0].respondent, "alice");
        assert_eq!(rs[0].body, InboundBody::Action(HitlAction::Done));
        assert_eq!(rs[0].hitl_ref, tok);
        assert_eq!(rs[1].body, InboundBody::Action(HitlAction::Retry));
        assert_eq!(rs[1].hitl_ref, tok);
        assert_eq!(rs[2].body, InboundBody::Text("skip it please".to_string()));
        assert_eq!(rs[3].body, InboundBody::Confirm);
        assert_eq!(rs[4].body, InboundBody::Text("/belt bogus".to_string()));
    }

    #[tokio::test]
    async fn poll_filters_by_since_and_skips_own_comments() {
        let own = format!("{OWN_MARKER}\n{TOKEN_PREFIX}h-1{TOKEN_SUFFIX}\nplease respond");
        let json = comments_json(&[
            ("1", "alice", "/belt done", "2026-10-06T23:59:59Z"),
            ("2", "me", &own, "2026-10-07T01:00:00Z"),
            ("3", "alice", "/belt skip", "2026-10-07T08:59:59+09:00"),
            ("4", "alice", "/belt skip", "2026-10-07T00:00:00Z"),
        ]);
        let shell = RecordingShell::ok(&json);
        let rs = channel(&shell)
            .poll(&[target("h-1", WID, SINCE)])
            .await
            .unwrap();
        let ids: Vec<&str> = rs.iter().map(|r| r.external_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["https://github.com/org/repo/issues/42#issuecomment-4"]
        );
    }

    #[tokio::test]
    async fn poll_external_id_is_stable_across_polls() {
        let json = comments_json(&[("7", "alice", "/belt done", "2026-10-07T01:00:00Z")]);
        let shell = RecordingShell::ok(&json);
        let c = channel(&shell);
        let t = [target("h-1", WID, SINCE)];
        let a = c.poll(&t).await.unwrap();
        let b = c.poll(&t).await.unwrap();
        assert_eq!(a, b);
        assert!(a[0].external_id.ends_with("issuecomment-7"));
    }

    #[tokio::test]
    async fn poll_with_several_targets_on_one_issue_needs_explicit_id() {
        let json = comments_json(&[
            ("1", "alice", "/belt done", "2026-10-07T01:00:00Z"),
            ("2", "alice", "/belt done h-2", "2026-10-07T01:00:00Z"),
            ("3", "alice", "/belt done h-9", "2026-10-07T01:00:00Z"),
        ]);
        let shell = RecordingShell::ok(&json);
        let rs = channel(&shell)
            .poll(&[target("h-1", WID, SINCE), target("h-2", WID, SINCE)])
            .await
            .unwrap();
        assert_eq!(shell.commands().len(), 1);
        assert_eq!(rs[0].hitl_ref, None);
        assert_eq!(rs[1].hitl_ref, Some(HitlRef::Token(HitlId::new("h-2"))));
        assert_eq!(rs[2].hitl_ref, None);
    }

    #[tokio::test]
    async fn poll_ignores_other_repo_targets_and_rejects_bad_output() {
        let shell = RecordingShell::ok("not json");
        let c = channel(&shell);
        assert!(
            c.poll(&[target("h-1", "github:other/repo#1:x", SINCE)])
                .await
                .unwrap()
                .is_empty()
        );
        assert!(shell.commands().is_empty());
        assert!(c.poll(&[target("h-1", WID, SINCE)]).await.is_err());
    }

    #[test]
    fn exposes_origin_name_and_inbox() {
        let shell = RecordingShell::ok("");
        let c = channel(&shell);
        assert_eq!(c.name(), "origin");
        assert!(c.inbox().is_some());
        assert!(crate::channels::SUPPORTED_CHANNEL_TYPES.is_empty());
    }
}
