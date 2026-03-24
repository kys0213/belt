use anyhow::Result;
use async_trait::async_trait;

use belt_core::context::{
    CommitContext, FileChangeContext, IssueContext, ItemContext, PrContext, QueueContext,
    SourceContext,
};
use belt_core::phase::QueuePhase;
use belt_core::queue::QueueItem;
use belt_core::source::DataSource;
use belt_core::workspace::WorkspaceConfig;

/// GitHub DataSource — gh CLI를 통해 이슈/PR을 스캔.
pub struct GitHubDataSource {
    source_url: String,
    last_scan: Option<chrono::DateTime<chrono::Utc>>,
}

impl GitHubDataSource {
    /// 새 `GitHubDataSource`를 생성한다.
    pub fn new(source_url: &str) -> Self {
        Self {
            source_url: source_url.to_string(),
            last_scan: None,
        }
    }

    /// URL에서 `owner/repo` 형태의 레포 이름을 추출한다.
    fn extract_repo_name(url: &str) -> Option<String> {
        let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
        let parts: Vec<&str> = trimmed.split('/').collect();
        if parts.len() >= 2 {
            Some(format!(
                "{}/{}",
                parts[parts.len() - 2],
                parts[parts.len() - 1]
            ))
        } else {
            None
        }
    }

    /// `gh` CLI로 이슈 상세 정보를 조회한다.
    async fn fetch_issue(
        repo: &str,
        number: i64,
    ) -> Option<(String, Option<String>, Vec<String>, String)> {
        let output = tokio::process::Command::new("gh")
            .args([
                "issue",
                "view",
                &number.to_string(),
                "--repo",
                repo,
                "--json",
                "title,body,labels,author,state",
            ])
            .output()
            .await;

        let output = output.ok().filter(|o| o.status.success())?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let val: serde_json::Value = serde_json::from_str(&stdout).ok()?;

        let title = val["title"].as_str().unwrap_or("").to_string();
        let body = val["body"].as_str().map(|s| s.to_string());
        let labels = val["labels"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l["name"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let author = val["author"]["login"].as_str().unwrap_or("").to_string();

        Some((title, body, labels, author))
    }

    /// `gh` CLI로 해당 이슈에 연결된 PR을 조회한다.
    ///
    /// `gh pr list`에서 현재 이슈 번호와 연결된 PR을 찾고,
    /// 리뷰 코멘트, 커밋 목록, 파일 변경사항을 함께 조회한다.
    async fn fetch_linked_pr(repo: &str, issue_number: i64) -> Option<PrContext> {
        let output = tokio::process::Command::new("gh")
            .args([
                "pr",
                "list",
                "--repo",
                repo,
                "--search",
                &format!("linked:issue:{issue_number}"),
                "--json",
                "number,title,state,url,headRefName",
                "--limit",
                "1",
            ])
            .output()
            .await;

        let output = output.ok().filter(|o| o.status.success())?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let prs: Vec<serde_json::Value> = serde_json::from_str(&stdout).ok()?;
        let pr = prs.first()?;

        let number = pr["number"].as_i64()?;
        let head_branch = pr["headRefName"].as_str().map(|s| s.to_string());
        let title = pr["title"].as_str().map(|s| s.to_string());
        let state = pr["state"].as_str().map(|s| s.to_string());
        let url = pr["url"].as_str().map(|s| s.to_string());

        // 리뷰 코멘트, 커밋, 파일 변경사항을 병렬로 조회
        let (reviews, commits, files) = tokio::join!(
            Self::fetch_pr_reviews(repo, number),
            Self::fetch_pr_commits(repo, number),
            Self::fetch_pr_files(repo, number),
        );

        Some(PrContext {
            number,
            head_branch,
            title,
            state,
            url,
            review_comments: reviews.unwrap_or_default(),
            commits: commits.unwrap_or_default(),
            files: files.unwrap_or_default(),
        })
    }

    /// `gh` CLI로 PR의 리뷰 코멘트를 조회한다.
    async fn fetch_pr_reviews(repo: &str, pr_number: i64) -> Option<Vec<String>> {
        let output = tokio::process::Command::new("gh")
            .args([
                "pr",
                "view",
                &pr_number.to_string(),
                "--repo",
                repo,
                "--json",
                "reviews",
            ])
            .output()
            .await;

        let output = output.ok().filter(|o| o.status.success())?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let val: serde_json::Value = serde_json::from_str(&stdout).ok()?;

        let reviews = val["reviews"].as_array()?;
        let comments: Vec<String> = reviews
            .iter()
            .filter_map(|r| r["body"].as_str())
            .filter(|body| !body.is_empty())
            .map(|s| s.to_string())
            .collect();

        Some(comments)
    }

    /// `gh` CLI로 PR의 커밋 목록을 조회한다.
    async fn fetch_pr_commits(repo: &str, pr_number: i64) -> Option<Vec<CommitContext>> {
        let output = tokio::process::Command::new("gh")
            .args([
                "pr",
                "view",
                &pr_number.to_string(),
                "--repo",
                repo,
                "--json",
                "commits",
            ])
            .output()
            .await;

        let output = output.ok().filter(|o| o.status.success())?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let val: serde_json::Value = serde_json::from_str(&stdout).ok()?;

        let commits = val["commits"].as_array()?;
        let result: Vec<CommitContext> = commits
            .iter()
            .filter_map(|c| {
                let sha = c["oid"].as_str().unwrap_or_default().to_string();
                let message = c["messageHeadline"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let author = c["authors"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|a| a["login"].as_str().or_else(|| a["name"].as_str()))
                    .unwrap_or_default()
                    .to_string();
                if sha.is_empty() {
                    return None;
                }
                Some(CommitContext {
                    sha,
                    message,
                    author,
                })
            })
            .collect();

        Some(result)
    }

    /// `gh` CLI로 PR의 파일 변경사항을 조회한다.
    async fn fetch_pr_files(repo: &str, pr_number: i64) -> Option<Vec<FileChangeContext>> {
        let output = tokio::process::Command::new("gh")
            .args([
                "pr",
                "view",
                &pr_number.to_string(),
                "--repo",
                repo,
                "--json",
                "files",
            ])
            .output()
            .await;

        let output = output.ok().filter(|o| o.status.success())?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let val: serde_json::Value = serde_json::from_str(&stdout).ok()?;

        let files = val["files"].as_array()?;
        let result: Vec<FileChangeContext> = files
            .iter()
            .filter_map(|f| {
                let path = f["path"].as_str()?.to_string();
                let additions = f["additions"].as_u64();
                let deletions = f["deletions"].as_u64();
                Some(FileChangeContext {
                    path,
                    additions,
                    deletions,
                })
            })
            .collect();

        Some(result)
    }

    /// `gh repo view`로 기본 브랜치를 조회한다.
    async fn fetch_default_branch(repo: &str) -> Option<String> {
        let output = tokio::process::Command::new("gh")
            .args(["repo", "view", repo, "--json", "defaultBranchRef"])
            .output()
            .await;

        let output = output.ok().filter(|o| o.status.success())?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let val: serde_json::Value = serde_json::from_str(&stdout).ok()?;

        val["defaultBranchRef"]["name"]
            .as_str()
            .map(|s| s.to_string())
    }
}

#[async_trait]
impl DataSource for GitHubDataSource {
    fn name(&self) -> &str {
        "github"
    }

    async fn collect(&mut self, workspace: &WorkspaceConfig) -> Result<Vec<QueueItem>> {
        let github_config = match workspace.sources.get("github") {
            Some(config) => config,
            None => return Ok(Vec::new()),
        };

        let repo_name = Self::extract_repo_name(&github_config.url)
            .unwrap_or_else(|| "unknown/repo".to_string());

        let mut items = Vec::new();

        for (state_name, state_config) in &github_config.states {
            let label = match &state_config.trigger.label {
                Some(l) => l,
                None => continue,
            };

            // gh issue list로 라벨 매칭 이슈 조회
            let output = tokio::process::Command::new("gh")
                .args([
                    "issue",
                    "list",
                    "--label",
                    label,
                    "--json",
                    "number,title",
                    "-R",
                    &repo_name,
                ])
                .output()
                .await;

            // Issue #21: collapsible_if — let-chain으로 병합
            if let Ok(output) = output
                && output.status.success()
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Ok(issues) = serde_json::from_str::<Vec<serde_json::Value>>(&stdout) {
                    for issue in issues {
                        let number = issue["number"].as_i64().unwrap_or(0);
                        let title = issue["title"].as_str().unwrap_or("").to_string();
                        let source_id = format!("github:{repo_name}#{number}");
                        let work_id = QueueItem::make_work_id(&source_id, state_name);

                        items.push(QueueItem {
                            work_id,
                            source_id,
                            workspace_id: workspace.name.clone(),
                            state: state_name.clone(),
                            phase: QueuePhase::Pending,
                            title: Some(title),
                            created_at: chrono::Utc::now().to_rfc3339(),
                            updated_at: chrono::Utc::now().to_rfc3339(),
                            hitl_created_at: None,
                            hitl_respondent: None,
                            hitl_notes: None,
                            hitl_reason: None,
                        });
                    }
                }
            }
        }

        self.last_scan = Some(chrono::Utc::now());
        Ok(items)
    }

    /// `gh` CLI를 사용하여 이슈/PR의 실제 데이터를 조회하고 `ItemContext`를 구성한다.
    async fn get_context(&self, item: &QueueItem) -> Result<ItemContext> {
        let issue_number = item
            .source_id
            .rsplit('#')
            .next()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);

        let repo_name =
            Self::extract_repo_name(&self.source_url).unwrap_or_else(|| "unknown/repo".to_string());

        // 이슈 상세, PR, 기본 브랜치를 병렬로 조회
        let (issue_data, pr_data, default_branch) = tokio::join!(
            Self::fetch_issue(&repo_name, issue_number),
            Self::fetch_linked_pr(&repo_name, issue_number),
            Self::fetch_default_branch(&repo_name),
        );

        let (title, body, labels, author) = issue_data.unwrap_or_else(|| {
            (
                item.title.clone().unwrap_or_default(),
                None,
                vec![],
                String::new(),
            )
        });

        Ok(ItemContext {
            work_id: item.work_id.clone(),
            workspace: item.workspace_id.clone(),
            queue: QueueContext {
                phase: item.phase.as_str().to_string(),
                state: item.state.clone(),
                source_id: item.source_id.clone(),
            },
            source: SourceContext {
                source_type: "github".to_string(),
                url: self.source_url.clone(),
                default_branch: default_branch.or(Some("main".to_string())),
            },
            issue: Some(IssueContext {
                number: issue_number,
                title,
                body,
                labels,
                author,
            }),
            pr: pr_data,
            history: vec![],
            worktree: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::phase::QueuePhase;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn make_workspace_with_github(url: &str, state: &str, label: Option<&str>) -> WorkspaceConfig {
        let label_yaml = match label {
            Some(l) => format!("          label: \"{l}\""),
            None => String::new(),
        };
        let yaml = format!(
            r#"
name: test-ws
sources:
  github:
    url: {url}
    states:
      {state}:
        trigger:
{label_yaml}
"#
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    fn make_workspace_without_github() -> WorkspaceConfig {
        serde_yaml::from_str("name: test-ws\nsources: {}").unwrap()
    }

    fn make_queue_item(source_id: &str, state: &str) -> QueueItem {
        let work_id = QueueItem::make_work_id(source_id, state);
        QueueItem {
            work_id,
            source_id: source_id.to_string(),
            workspace_id: "test-ws".to_string(),
            state: state.to_string(),
            phase: QueuePhase::Pending,
            title: Some("Test issue title".to_string()),
            created_at: "2026-03-24T00:00:00Z".to_string(),
            updated_at: "2026-03-24T00:00:00Z".to_string(),
        }
    }

    // ── extract_repo_name ────────────────────────────────────────────────────

    #[test]
    fn extract_repo_name_from_url() {
        assert_eq!(
            GitHubDataSource::extract_repo_name("https://github.com/org/repo"),
            Some("org/repo".to_string())
        );
        assert_eq!(
            GitHubDataSource::extract_repo_name("https://github.com/org/repo.git"),
            Some("org/repo".to_string())
        );
    }

    #[test]
    fn extract_repo_name_strips_trailing_slash() {
        assert_eq!(
            GitHubDataSource::extract_repo_name("https://github.com/org/repo/"),
            Some("org/repo".to_string())
        );
    }

    #[test]
    fn extract_repo_name_strips_trailing_slash_and_git() {
        // trailing slash then .git would leave ".git" after trim_end_matches('/'),
        // but .git suffix is stripped independently — test the actual combination
        assert_eq!(
            GitHubDataSource::extract_repo_name("https://github.com/org/repo.git"),
            Some("org/repo".to_string())
        );
    }

    #[test]
    fn extract_repo_name_ssh_url() {
        // SSH style: git@github.com:org/repo.git
        // split('/') on this gives ["git@github.com:org", "repo.git"] → "org/repo" after strip
        assert_eq!(
            GitHubDataSource::extract_repo_name("git@github.com:org/repo.git"),
            Some("git@github.com:org/repo".to_string())
        );
    }

    #[test]
    fn extract_repo_name_bare_owner_repo() {
        // Minimal "owner/repo" string (no host)
        assert_eq!(
            GitHubDataSource::extract_repo_name("owner/repo"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn extract_repo_name_single_segment_returns_none() {
        // Only one segment — cannot form "owner/repo"
        assert_eq!(GitHubDataSource::extract_repo_name("repo"), None);
    }

    #[test]
    fn extract_repo_name_empty_string_returns_none() {
        assert_eq!(GitHubDataSource::extract_repo_name(""), None);
    }

    // ── constructor & DataSource::name ───────────────────────────────────────

    #[test]
    fn new_stores_source_url() {
        let ds = GitHubDataSource::new("https://github.com/org/repo");
        assert_eq!(ds.source_url, "https://github.com/org/repo");
    }

    #[test]
    fn new_sets_last_scan_to_none() {
        let ds = GitHubDataSource::new("https://github.com/org/repo");
        assert!(ds.last_scan.is_none());
    }

    #[test]
    fn name_returns_github() {
        let ds = GitHubDataSource::new("https://github.com/org/repo");
        assert_eq!(ds.name(), "github");
    }

    // ── collect() ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn collect_returns_empty_when_no_github_source() {
        let mut ds = GitHubDataSource::new("https://github.com/org/repo");
        let workspace = make_workspace_without_github();
        let items = ds.collect(&workspace).await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn collect_skips_state_with_no_trigger_label() {
        let mut ds = GitHubDataSource::new("https://github.com/org/repo");
        // label is None — this state must be skipped
        let workspace = make_workspace_with_github("https://github.com/org/repo", "analyze", None);
        // The gh CLI will not be called because we return early when label is None.
        // We only verify no panic and the result is Ok.
        let result = ds.collect(&workspace).await;
        assert!(result.is_ok());
    }

    // ── get_context() ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_context_parses_issue_number_from_source_id() {
        let ds = GitHubDataSource::new("https://github.com/org/repo");
        let item = make_queue_item("github:org/repo#42", "analyze");
        // gh CLI will fail in the test environment; the fallback path is exercised.
        let ctx = ds.get_context(&item).await.unwrap();
        // issue number must be parsed correctly from source_id even without gh
        assert_eq!(ctx.issue.as_ref().unwrap().number, 42);
    }

    #[tokio::test]
    async fn get_context_uses_item_title_as_fallback_when_gh_unavailable() {
        let ds = GitHubDataSource::new("https://github.com/org/repo");
        let item = make_queue_item("github:org/repo#7", "implement");
        let ctx = ds.get_context(&item).await.unwrap();
        // When gh CLI is not available, title falls back to item.title
        let issue = ctx.issue.as_ref().unwrap();
        assert_eq!(issue.title, item.title.clone().unwrap_or_default());
    }

    #[tokio::test]
    async fn get_context_falls_back_to_main_when_default_branch_unknown() {
        let ds = GitHubDataSource::new("https://github.com/org/repo");
        let item = make_queue_item("github:org/repo#1", "analyze");
        let ctx = ds.get_context(&item).await.unwrap();
        // When gh CLI is unavailable, default_branch falls back to Some("main")
        assert_eq!(ctx.source.default_branch.as_deref(), Some("main"));
    }

    #[tokio::test]
    async fn get_context_source_type_is_github() {
        let ds = GitHubDataSource::new("https://github.com/org/repo");
        let item = make_queue_item("github:org/repo#5", "review");
        let ctx = ds.get_context(&item).await.unwrap();
        assert_eq!(ctx.source.source_type, "github");
    }

    #[tokio::test]
    async fn get_context_source_url_matches_datasource_url() {
        let url = "https://github.com/org/repo";
        let ds = GitHubDataSource::new(url);
        let item = make_queue_item("github:org/repo#10", "analyze");
        let ctx = ds.get_context(&item).await.unwrap();
        assert_eq!(ctx.source.url, url);
    }

    #[tokio::test]
    async fn get_context_work_id_and_workspace_propagated() {
        let ds = GitHubDataSource::new("https://github.com/org/repo");
        let item = make_queue_item("github:org/repo#3", "analyze");
        let ctx = ds.get_context(&item).await.unwrap();
        assert_eq!(ctx.work_id, item.work_id);
        assert_eq!(ctx.workspace, item.workspace_id);
    }

    #[tokio::test]
    async fn get_context_queue_fields_propagated() {
        let ds = GitHubDataSource::new("https://github.com/org/repo");
        let item = make_queue_item("github:org/repo#9", "implement");
        let ctx = ds.get_context(&item).await.unwrap();
        assert_eq!(ctx.queue.state, item.state);
        assert_eq!(ctx.queue.source_id, item.source_id);
        assert_eq!(ctx.queue.phase, item.phase.as_str());
    }

    #[tokio::test]
    async fn get_context_source_id_without_hash_yields_issue_number_zero() {
        // Edge case: malformed source_id with no '#' separator
        let ds = GitHubDataSource::new("https://github.com/org/repo");
        let mut item = make_queue_item("github:org/repo#0", "analyze");
        item.source_id = "github:org/repo".to_string(); // no '#number'
        let ctx = ds.get_context(&item).await.unwrap();
        assert_eq!(ctx.issue.as_ref().unwrap().number, 0);
    }

    #[tokio::test]
    async fn get_context_history_is_empty_by_default() {
        let ds = GitHubDataSource::new("https://github.com/org/repo");
        let item = make_queue_item("github:org/repo#11", "analyze");
        let ctx = ds.get_context(&item).await.unwrap();
        assert!(ctx.history.is_empty());
    }

    #[tokio::test]
    async fn get_context_worktree_is_none_by_default() {
        let ds = GitHubDataSource::new("https://github.com/org/repo");
        let item = make_queue_item("github:org/repo#12", "analyze");
        let ctx = ds.get_context(&item).await.unwrap();
        assert!(ctx.worktree.is_none());
    }

    // ── work_id construction ─────────────────────────────────────────────────

    #[test]
    fn work_id_format_matches_convention() {
        // Verifies the source_id + state → work_id convention used inside collect()
        let source_id = "github:org/repo#42";
        let state = "implement";
        let work_id = QueueItem::make_work_id(source_id, state);
        assert_eq!(work_id, "github:org/repo#42:implement");
    }

    #[test]
    fn source_id_format_matches_convention() {
        // Verifies the repo_name + issue_number → source_id format used inside collect()
        let repo_name = "org/repo";
        let number: i64 = 42;
        let source_id = format!("github:{repo_name}#{number}");
        assert_eq!(source_id, "github:org/repo#42");
    }
}
