//! Fake `gh` CLI: emulates exactly the subcommands belt invokes, backed by a
//! JSON state file, and logs every invocation.
//!
//! Emulated surface (anything else exits 2 with `fake gh: unsupported`, so a
//! new `gh` call in belt shows up as a test failure instead of a silent no-op):
//!
//! | command | used by |
//! |---------|---------|
//! | `issue list --label L --json F -R R [--state S] [--limit N]` | `GitHubDataSource::collect` |
//! | `issue view N --repo R --json F` | `get_context`, origin channel poll |
//! | `issue comment N --repo R --body-file F` | origin channel notify |
//! | `issue edit N --repo R --add-label L` / `--remove-label L` | lifecycle hook, scripts |
//! | `pr list --repo R [--state S] [--search linked:issue:N] --json F [--limit N]` | source |
//! | `pr view N --repo R --json F` | origin channel poll |
//! | `pr comment N --repo R --body-file F` | origin channel notify |
//! | `repo view R --json defaultBranchRef` | `get_context` |
//! | `api repos/R/pulls/N/reviews --jq ...` | changes-requested scan |

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const STATE_FILE: &str = "gh_state.json";
const CALL_LOG: &str = "gh_calls.jsonl";
const LOCK_DIR: &str = "gh_state.lock";

/// Whole fake GitHub: one repository with its issues and pull requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GhState {
    pub repo: String,
    pub default_branch: String,
    /// Issues and pull requests share one number space, like on GitHub.
    pub next_number: u64,
    pub next_comment_id: u64,
    pub issues: BTreeMap<u64, Issue>,
    pub prs: BTreeMap<u64, PullRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    /// `OPEN` or `CLOSED`, as `gh` reports it.
    pub state: String,
    pub author: String,
    pub labels: Vec<String>,
    pub comments: Vec<Comment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub author: String,
    pub head: String,
    pub base: String,
    pub draft: bool,
    pub labels: Vec<String>,
    /// Issue this PR closes; matched by `--search linked:issue:N`.
    pub linked_issue: Option<u64>,
    pub comments: Vec<Comment>,
    pub reviews: Vec<Review>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub url: String,
    pub body: String,
    pub author: String,
    pub created_at: String,
    pub edited: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub author: String,
    /// e.g. `APPROVED`, `CHANGES_REQUESTED`.
    pub state: String,
    pub body: String,
    pub submitted_at: String,
}

/// One logged `gh` invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GhCall {
    pub argv: Vec<String>,
    pub cwd: String,
    pub exit_code: i32,
}

impl GhCall {
    /// The invocation as one space-joined line, for `contains` assertions.
    pub fn line(&self) -> String {
        self.argv.join(" ")
    }
}

/// Handle on a fake GitHub state directory, shared by the test process and
/// every fake `gh` process (which find it through `BELT_E2E_FAKE_DIR`).
#[derive(Debug, Clone)]
pub struct FakeGitHub {
    dir: PathBuf,
}

impl FakeGitHub {
    /// Create a fresh state for `repo` (`owner/name`) in `dir`.
    pub fn init(dir: &Path, repo: &str) -> Self {
        let fake = Self {
            dir: dir.to_path_buf(),
        };
        let state = GhState {
            repo: repo.to_string(),
            default_branch: "main".to_string(),
            next_number: 1,
            next_comment_id: 1,
            issues: BTreeMap::new(),
            prs: BTreeMap::new(),
        };
        fake.save(&state);
        fake
    }

    /// Open an existing state directory.
    pub fn open(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// Run `f` on the state under the cross-process lock and persist it.
    pub fn update<R>(&self, f: impl FnOnce(&mut GhState) -> R) -> R {
        let _lock = StateLock::acquire(&self.dir.join(LOCK_DIR));
        let mut state = self.load();
        let result = f(&mut state);
        self.save(&state);
        result
    }

    /// A consistent snapshot of the state.
    pub fn snapshot(&self) -> GhState {
        let _lock = StateLock::acquire(&self.dir.join(LOCK_DIR));
        self.load()
    }

    /// Seed an open issue; returns its number.
    pub fn seed_issue(&self, title: &str, body: &str, labels: &[&str]) -> u64 {
        self.update(|s| {
            let number = s.next_number;
            s.next_number += 1;
            s.issues.insert(
                number,
                Issue {
                    number,
                    title: title.to_string(),
                    body: body.to_string(),
                    state: "OPEN".to_string(),
                    author: "reporter".to_string(),
                    labels: labels.iter().map(|l| l.to_string()).collect(),
                    comments: Vec::new(),
                },
            );
            number
        })
    }

    /// Add a comment on issue `number` as `author` (e.g. a human `/belt done`).
    #[allow(dead_code)] // harness API for the HITL scenarios
    pub fn add_issue_comment(&self, number: u64, author: &str, body: &str) -> String {
        self.update(|s| {
            let comment = new_comment(s, "issues", number, author, body);
            let url = comment.url.clone();
            s.issues
                .get_mut(&number)
                .unwrap_or_else(|| panic!("no fake issue #{number}"))
                .comments
                .push(comment);
            url
        })
    }

    /// Current labels of issue `number`.
    pub fn issue_labels(&self, number: u64) -> Vec<String> {
        self.snapshot().issues[&number].labels.clone()
    }

    /// Current comments of issue `number`.
    #[allow(dead_code)] // harness API for the notification scenarios
    pub fn issue_comments(&self, number: u64) -> Vec<Comment> {
        self.snapshot().issues[&number].comments.clone()
    }

    /// Every `gh` invocation so far, in order.
    pub fn calls(&self) -> Vec<GhCall> {
        let _lock = StateLock::acquire(&self.dir.join(LOCK_DIR));
        let Ok(text) = std::fs::read_to_string(self.dir.join(CALL_LOG)) else {
            return Vec::new();
        };
        text.lines()
            .map(|l| serde_json::from_str(l).expect("malformed gh call log line"))
            .collect()
    }

    fn load(&self) -> GhState {
        let text = std::fs::read_to_string(self.dir.join(STATE_FILE))
            .expect("fake gh state missing: FakeGitHub::init was not called");
        serde_json::from_str(&text).expect("fake gh state is not valid JSON")
    }

    /// Write to a temp file and rename, so a reader never sees a partial file.
    fn save(&self, state: &GhState) {
        let tmp = self
            .dir
            .join(format!("{STATE_FILE}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(state).unwrap()).unwrap();
        std::fs::rename(&tmp, self.dir.join(STATE_FILE)).unwrap();
    }

    fn log_call(&self, call: &GhCall) {
        let _lock = StateLock::acquire(&self.dir.join(LOCK_DIR));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(CALL_LOG))
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(call).unwrap()).unwrap();
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn new_comment(s: &mut GhState, kind: &str, number: u64, author: &str, body: &str) -> Comment {
    let id = s.next_comment_id;
    s.next_comment_id += 1;
    Comment {
        id,
        url: format!(
            "https://github.com/{}/{kind}/{number}#issuecomment-{id}",
            s.repo
        ),
        body: body.to_string(),
        author: author.to_string(),
        created_at: now_rfc3339(),
        edited: false,
    }
}

/// Cross-process mutex: `mkdir` is atomic on every POSIX filesystem.
struct StateLock(PathBuf);

impl StateLock {
    fn acquire(path: &Path) -> Self {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match std::fs::create_dir(path) {
                Ok(()) => return Self(path.to_path_buf()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    assert!(
                        Instant::now() < deadline,
                        "fake gh state lock held for 10s: {}",
                        path.display()
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("cannot create lock {}: {e}", path.display()),
            }
        }
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

// ---------------------------------------------------------------------------
// CLI emulation
// ---------------------------------------------------------------------------

/// Entry point when this binary runs as `gh`. Returns the exit code.
pub fn main(state_dir: &Path, args: Vec<String>) -> i32 {
    let fake = FakeGitHub::open(state_dir);
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let (code, stdout, stderr) = match run(&fake, &args) {
        Ok(out) => (0, out, String::new()),
        Err(GhError { code, message }) => (code, String::new(), message),
    };
    fake.log_call(&GhCall {
        argv: std::iter::once("gh".to_string()).chain(args).collect(),
        cwd,
        exit_code: code,
    });
    print!("{stdout}");
    if !stderr.is_empty() {
        eprintln!("{stderr}");
    }
    code
}

struct GhError {
    code: i32,
    message: String,
}

fn unsupported(args: &[String]) -> GhError {
    GhError {
        code: 2,
        message: format!("fake gh: unsupported invocation: gh {}", args.join(" ")),
    }
}

fn fail(message: impl Into<String>) -> GhError {
    GhError {
        code: 1,
        message: message.into(),
    }
}

/// Flags that take a value. Any other flag is rejected as unsupported.
const VALUE_FLAGS: &[&str] = &[
    "--repo",
    "-R",
    "--json",
    "--label",
    "--state",
    "--limit",
    "--search",
    "--body-file",
    "--add-label",
    "--remove-label",
    "--jq",
];

struct Parsed {
    positional: Vec<String>,
    flags: BTreeMap<String, Vec<String>>,
}

impl Parsed {
    fn one(&self, flag: &str) -> Option<&str> {
        self.flags
            .get(flag)
            .and_then(|v| v.last())
            .map(|s| s.as_str())
    }

    fn all(&self, flag: &str) -> Vec<&str> {
        self.flags
            .get(flag)
            .map(|v| v.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default()
    }

    fn repo(&self) -> Option<&str> {
        self.one("--repo").or_else(|| self.one("-R"))
    }
}

fn parse(args: &[String]) -> Result<Parsed, GhError> {
    let mut positional = Vec::new();
    let mut flags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg.starts_with('-') {
            let (name, inline) = match arg.split_once('=') {
                Some((n, v)) => (n.to_string(), Some(v.to_string())),
                None => (arg.clone(), None),
            };
            if !VALUE_FLAGS.contains(&name.as_str()) {
                return Err(unsupported(args));
            }
            let value = match inline {
                Some(v) => v,
                None => iter.next().cloned().ok_or_else(|| unsupported(args))?,
            };
            flags.entry(name).or_default().push(value);
        } else {
            positional.push(arg.clone());
        }
    }
    Ok(Parsed { positional, flags })
}

fn run(fake: &FakeGitHub, args: &[String]) -> Result<String, GhError> {
    let p = parse(args)?;
    let pos: Vec<&str> = p.positional.iter().map(|s| s.as_str()).collect();
    match pos.as_slice() {
        ["issue", "list"] => {
            let s = fake.snapshot();
            check_repo(&s, p.repo(), args)?;
            let state = p.one("--state").unwrap_or("open");
            let labels = p.all("--label");
            let fields = p.one("--json").ok_or_else(|| unsupported(args))?;
            let limit = limit(&p)?;
            let rows: Vec<Value> = s
                .issues
                .values()
                .filter(|i| state_matches(&i.state, state))
                .filter(|i| labels.iter().all(|l| i.labels.iter().any(|x| x == l)))
                .take(limit)
                .map(|i| select(issue_json(&s, i), fields))
                .collect::<Result<_, _>>()?;
            Ok(to_json(&Value::Array(rows)))
        }
        ["issue", "view", number] => {
            let s = fake.snapshot();
            check_repo(&s, p.repo(), args)?;
            let fields = p.one("--json").ok_or_else(|| unsupported(args))?;
            let issue = s.issues.get(&number_arg(number)?).ok_or_else(|| {
                fail(format!("GraphQL: Could not resolve to an issue ({number})"))
            })?;
            Ok(to_json(&select(issue_json(&s, issue), fields)?))
        }
        ["issue", "comment", number] => {
            let number = number_arg(number)?;
            let body = body_file(&p, args)?;
            fake.update(|s| {
                check_repo(s, p.repo(), args)?;
                if !s.issues.contains_key(&number) {
                    return Err(fail(format!("no issue #{number}")));
                }
                let comment = new_comment(s, "issues", number, "belt-bot", &body);
                let url = comment.url.clone();
                s.issues.get_mut(&number).unwrap().comments.push(comment);
                Ok(format!("{url}\n"))
            })
        }
        ["issue", "edit", number] => {
            let number = number_arg(number)?;
            let add = split_labels(&p.all("--add-label"));
            let remove = split_labels(&p.all("--remove-label"));
            if add.is_empty() && remove.is_empty() {
                return Err(unsupported(args));
            }
            fake.update(|s| {
                check_repo(s, p.repo(), args)?;
                let url = format!("https://github.com/{}/issues/{number}", s.repo);
                let issue = s
                    .issues
                    .get_mut(&number)
                    .ok_or_else(|| fail(format!("no issue #{number}")))?;
                for l in add {
                    if !issue.labels.contains(&l) {
                        issue.labels.push(l);
                    }
                }
                issue.labels.retain(|l| !remove.contains(l));
                Ok(format!("{url}\n"))
            })
        }
        ["pr", "list"] => {
            let s = fake.snapshot();
            check_repo(&s, p.repo(), args)?;
            let state = p.one("--state").unwrap_or("open");
            let fields = p.one("--json").ok_or_else(|| unsupported(args))?;
            let linked = match p.one("--search") {
                None => None,
                Some(q) => Some(
                    q.strip_prefix("linked:issue:")
                        .and_then(|n| n.parse::<u64>().ok())
                        .ok_or_else(|| unsupported(args))?,
                ),
            };
            let limit = limit(&p)?;
            let rows: Vec<Value> = s
                .prs
                .values()
                .filter(|pr| state_matches(&pr.state, state))
                .filter(|pr| linked.is_none() || pr.linked_issue == linked)
                .take(limit)
                .map(|pr| select(pr_json(&s, pr), fields))
                .collect::<Result<_, _>>()?;
            Ok(to_json(&Value::Array(rows)))
        }
        ["pr", "view", number] => {
            let s = fake.snapshot();
            check_repo(&s, p.repo(), args)?;
            let fields = p.one("--json").ok_or_else(|| unsupported(args))?;
            let pr = s
                .prs
                .get(&number_arg(number)?)
                .ok_or_else(|| fail(format!("no pull request #{number}")))?;
            Ok(to_json(&select(pr_json(&s, pr), fields)?))
        }
        ["pr", "comment", number] => {
            let number = number_arg(number)?;
            let body = body_file(&p, args)?;
            fake.update(|s| {
                check_repo(s, p.repo(), args)?;
                if !s.prs.contains_key(&number) {
                    return Err(fail(format!("no pull request #{number}")));
                }
                let comment = new_comment(s, "pull", number, "belt-bot", &body);
                let url = comment.url.clone();
                s.prs.get_mut(&number).unwrap().comments.push(comment);
                Ok(format!("{url}\n"))
            })
        }
        ["repo", "view", repo] => {
            let s = fake.snapshot();
            check_repo(&s, Some(repo), args)?;
            if p.one("--json") != Some("defaultBranchRef") {
                return Err(unsupported(args));
            }
            Ok(to_json(
                &json!({ "defaultBranchRef": { "name": s.default_branch } }),
            ))
        }
        ["api", path] => {
            // belt passes one fixed `--jq` projection; emit its output shape.
            let s = fake.snapshot();
            let rest = path
                .strip_prefix(&format!("repos/{}/pulls/", s.repo))
                .and_then(|r| r.strip_suffix("/reviews"))
                .ok_or_else(|| unsupported(args))?;
            if p.one("--jq").is_none() {
                return Err(unsupported(args));
            }
            let pr = s
                .prs
                .get(&number_arg(rest)?)
                .ok_or_else(|| fail("HTTP 404: Not Found"))?;
            Ok(pr
                .reviews
                .iter()
                .map(|r| {
                    format!(
                        "{}\n",
                        json!({
                            "user": r.author,
                            "state": r.state,
                            "body": r.body,
                            "submitted_at": r.submitted_at,
                        })
                    )
                })
                .collect())
        }
        _ => Err(unsupported(args)),
    }
}

fn check_repo(s: &GhState, repo: Option<&str>, args: &[String]) -> Result<(), GhError> {
    match repo {
        // belt always names the repository; relying on the cwd remote would
        // be a behavior change worth failing on.
        None => Err(unsupported(args)),
        Some(r) if r == s.repo => Ok(()),
        Some(r) => Err(fail(format!(
            "GraphQL: Could not resolve to a Repository with the name '{r}'."
        ))),
    }
}

fn number_arg(s: &str) -> Result<u64, GhError> {
    s.parse()
        .map_err(|_| fail(format!("invalid issue/PR number: {s}")))
}

fn limit(p: &Parsed) -> Result<usize, GhError> {
    match p.one("--limit") {
        None => Ok(30),
        Some(n) => n.parse().map_err(|_| fail(format!("invalid --limit {n}"))),
    }
}

fn state_matches(actual: &str, wanted: &str) -> bool {
    wanted == "all" || actual.eq_ignore_ascii_case(wanted)
}

fn split_labels(values: &[&str]) -> Vec<String> {
    values
        .iter()
        .flat_map(|v| v.split(','))
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// `--body-file` relative to the cwd, as `gh` resolves it.
fn body_file(p: &Parsed, args: &[String]) -> Result<String, GhError> {
    let path = p.one("--body-file").ok_or_else(|| unsupported(args))?;
    std::fs::read_to_string(path).map_err(|e| fail(format!("cannot read {path}: {e}")))
}

fn to_json(v: &Value) -> String {
    format!("{}\n", serde_json::to_string(v).unwrap())
}

/// Keep only the requested `--json` fields; unknown ones fail like real `gh`.
fn select(full: Value, fields: &str) -> Result<Value, GhError> {
    let obj = full.as_object().unwrap();
    let mut out = serde_json::Map::new();
    for f in fields.split(',') {
        let v = obj
            .get(f)
            .ok_or_else(|| fail(format!("Unknown JSON field: \"{f}\"")))?;
        out.insert(f.to_string(), v.clone());
    }
    Ok(Value::Object(out))
}

fn labels_json(labels: &[String]) -> Value {
    labels.iter().map(|l| json!({ "name": l })).collect()
}

fn comments_json(comments: &[Comment]) -> Value {
    comments
        .iter()
        .map(|c| {
            json!({
                "id": c.id.to_string(),
                "url": c.url,
                "body": c.body,
                "createdAt": c.created_at,
                "author": { "login": c.author },
                "includesCreatedEdit": c.edited,
            })
        })
        .collect()
}

fn issue_json(s: &GhState, i: &Issue) -> Value {
    json!({
        "number": i.number,
        "title": i.title,
        "body": i.body,
        "state": i.state,
        "author": { "login": i.author },
        "labels": labels_json(&i.labels),
        "comments": comments_json(&i.comments),
        "url": format!("https://github.com/{}/issues/{}", s.repo, i.number),
    })
}

fn pr_json(s: &GhState, pr: &PullRequest) -> Value {
    json!({
        "number": pr.number,
        "title": pr.title,
        "body": pr.body,
        "state": pr.state,
        "author": { "login": pr.author },
        "isDraft": pr.draft,
        "headRefName": pr.head,
        "baseRefName": pr.base,
        "labels": labels_json(&pr.labels),
        "mergeable": "MERGEABLE",
        "reviews": pr.reviews.iter().map(|r| json!({
            "author": { "login": r.author },
            "state": r.state,
            "body": r.body,
            "submittedAt": r.submitted_at,
        })).collect::<Vec<_>>(),
        "comments": comments_json(&pr.comments),
        "url": format!("https://github.com/{}/pull/{}", s.repo, pr.number),
    })
}
