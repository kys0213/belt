//! Shared helpers for real E2E tests.
//!
//! Provides gh CLI wrappers, real daemon factory, and prerequisite checks.
//! These helpers interact with **real** GitHub (kys0213/belt) and Claude API.

use std::process::Command;
use std::sync::Arc;

use belt_core::runtime::RuntimeRegistry;
#[allow(unused_imports)]
use belt_core::workspace::WorkspaceConfig;
use belt_daemon::daemon::Daemon;
use belt_infra::db::Database;
use belt_infra::runtimes::claude::ClaudeRuntime;
use belt_infra::sources::github::GitHubDataSource;
use belt_infra::worktree::MockWorktreeManager;
use tempfile::TempDir;

const REPO: &str = "kys0213/belt";

// ─── Prerequisites ───────────────────────────────────────────────

/// Panics if `gh` or `claude` CLI are not available.
pub fn assert_prerequisites() {
    let gh = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .expect("gh CLI not found");
    assert!(
        gh.status.success(),
        "gh CLI not authenticated. Run `gh auth login` first."
    );

    let claude = Command::new("claude")
        .args(["--version"])
        .output()
        .expect("claude CLI not found");
    assert!(
        claude.status.success(),
        "claude CLI not available. Install it first."
    );
}

// ─── GitHub Issue Helpers ────────────────────────────────────────

/// Creates a test issue with the given title and label. Returns the issue number.
pub fn create_test_issue(title: &str, label: &str) -> i64 {
    let output = Command::new("gh")
        .args([
            "issue",
            "create",
            "--repo",
            REPO,
            "--title",
            title,
            "--body",
            "Automated E2E test issue. Safe to close.",
            "--label",
            label,
        ])
        .output()
        .expect("failed to create issue");

    assert!(
        output.status.success(),
        "gh issue create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // gh issue create outputs a URL like https://github.com/owner/repo/issues/123
    extract_issue_number(&stdout)
        .unwrap_or_else(|| panic!("could not parse issue number from: {stdout}"))
}

/// Closes a test issue.
pub fn close_test_issue(number: i64) {
    let _ = Command::new("gh")
        .args([
            "issue",
            "close",
            &number.to_string(),
            "--repo",
            REPO,
            "--comment",
            "[e2e] Test completed. Closing.",
        ])
        .output();
}

/// Returns the labels on an issue.
pub fn get_issue_labels(number: i64) -> Vec<String> {
    let output = Command::new("gh")
        .args([
            "issue",
            "view",
            &number.to_string(),
            "--repo",
            REPO,
            "--json",
            "labels",
        ])
        .output()
        .expect("failed to view issue");

    if !output.status.success() {
        return vec![];
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_default();
    val["labels"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l["name"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Returns comments on an issue (body text only).
pub fn get_issue_comments(number: i64) -> Vec<String> {
    let output = Command::new("gh")
        .args([
            "issue",
            "view",
            &number.to_string(),
            "--repo",
            REPO,
            "--json",
            "comments",
        ])
        .output()
        .expect("failed to view issue comments");

    if !output.status.success() {
        return vec![];
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_default();
    val["comments"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c["body"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Closes all open issues with e2e-test labels (cleanup utility).
pub fn cleanup_all_e2e_issues() {
    for label in &["e2e-test:analyze", "e2e-test:implement"] {
        let output = Command::new("gh")
            .args([
                "issue",
                "list",
                "--repo",
                REPO,
                "--label",
                label,
                "--state",
                "open",
                "--json",
                "number",
            ])
            .output();

        if let Ok(output) = output {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(issues) = serde_json::from_str::<Vec<serde_json::Value>>(&stdout) {
                for issue in issues {
                    if let Some(number) = issue["number"].as_i64() {
                        close_test_issue(number);
                    }
                }
            }
        }
    }
}

// ─── Daemon Factory ──────────────────────────────────────────────

/// Loads the E2E workspace config from tests/e2e-workspace.yaml.
pub fn e2e_workspace_config() -> WorkspaceConfig {
    let yaml = include_str!("../../../tests/e2e-workspace.yaml");
    serde_yaml::from_str(yaml).expect("failed to parse e2e-workspace.yaml")
}

/// Returns the path to a file-based DB inside the temp dir.
/// Both the Daemon and the test can open this path independently.
pub fn db_path(tmp: &TempDir) -> String {
    tmp.path().join("e2e-belt.db").to_string_lossy().to_string()
}

/// Opens a Database at the given path (for test assertions).
pub fn open_db(path: &str) -> Database {
    Database::open(path).expect("failed to open DB for assertions")
}

/// Creates a Daemon with real ClaudeRuntime + GitHubDataSource + file-based DB.
pub fn setup_real_daemon(tmp: &TempDir) -> Daemon {
    let config = e2e_workspace_config();

    let source = GitHubDataSource::new("https://github.com/kys0213/belt");
    let runtime = ClaudeRuntime::new(Some("claude-sonnet-4-20250514".to_string()));

    let mut registry = RuntimeRegistry::new("claude".to_string());
    registry.register(Arc::new(runtime));

    let worktree_mgr = MockWorktreeManager::new(tmp.path().to_path_buf());
    let db = Database::open(&db_path(tmp)).expect("failed to create DB");

    Daemon::new(
        config,
        vec![Box::new(source)],
        Arc::new(registry),
        Box::new(worktree_mgr),
        1,
    )
    .with_db(db)
    .with_belt_home(tmp.path().to_path_buf())
}

/// Creates a Daemon with real GitHubDataSource but MockRuntime (for failure tests).
pub fn setup_mock_runtime_daemon(tmp: &TempDir, exit_codes: Vec<i32>) -> Daemon {
    use belt_infra::runtimes::mock::MockRuntime;

    let config = e2e_workspace_config();

    let source = GitHubDataSource::new("https://github.com/kys0213/belt");
    let runtime = MockRuntime::new("claude", exit_codes);

    let mut registry = RuntimeRegistry::new("claude".to_string());
    registry.register(Arc::new(runtime));

    let worktree_mgr = MockWorktreeManager::new(tmp.path().to_path_buf());
    let db = Database::open(&db_path(tmp)).expect("failed to create DB");

    Daemon::new(
        config,
        vec![Box::new(source)],
        Arc::new(registry),
        Box::new(worktree_mgr),
        1,
    )
    .with_db(db)
    .with_belt_home(tmp.path().to_path_buf())
}

// ─── RAII Guard ──────────────────────────────────────────────────

/// RAII guard that closes a test issue on drop.
pub struct TestIssueGuard {
    pub number: i64,
}

impl Drop for TestIssueGuard {
    fn drop(&mut self) {
        close_test_issue(self.number);
    }
}

// ─── Internal Helpers ────────────────────────────────────────────

fn extract_issue_number(url: &str) -> Option<i64> {
    url.trim()
        .rsplit('/')
        .next()
        .and_then(|s| s.parse().ok())
}
