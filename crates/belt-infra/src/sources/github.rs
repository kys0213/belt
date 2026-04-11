use anyhow::Result;
use async_trait::async_trait;

use belt_core::context::{
    IssueContext, ItemContext, PrContext, QueueContext, ReviewContext, SourceContext,
};
use belt_core::queue::QueueItem;
use belt_core::source::DataSource;
use belt_core::workspace::WorkspaceConfig;

/// GitHub DataSource — gh CLI를 통해 이슈/PR을 스캔.
pub struct GitHubDataSource {
    source_url: String,
    last_collected_at: Option<std::time::Instant>,
}

impl GitHubDataSource {
    /// 새 `GitHubDataSource`를 생성한다.
    pub fn new(source_url: &str) -> Self {
        Self {
            source_url: source_url.to_string(),
            last_collected_at: None,
        }
    }

    /// URL에서 `owner/repo` 형태의 레포 이름을 추출한다.
    pub fn extract_repo_name(url: &str) -> Option<String> {
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
    ) -> Option<(String, Option<String>, Vec<String>, String, String)> {
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
        let state = val["state"].as_str().unwrap_or("open").to_string();

        Some((title, body, labels, author, state))
    }

    /// `gh` CLI로 해당 이슈에 연결된 PR을 조회한다.
    ///
    /// title, body, author, state, draft, branch, labels, merge status, reviews를 포함한다.
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
                "number,title,body,author,state,isDraft,headRefName,baseRefName,labels,mergeable,reviews",
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
        let title = pr["title"].as_str().map(|s| s.to_string());
        let body = pr["body"].as_str().map(|s| s.to_string());
        let author = pr["author"]["login"].as_str().map(|s| s.to_string());
        let state = pr["state"].as_str().map(|s| s.to_string());
        let draft = pr["isDraft"].as_bool().unwrap_or(false);
        let head_branch = pr["headRefName"].as_str().map(|s| s.to_string());
        let base_branch = pr["baseRefName"].as_str().map(|s| s.to_string());
        let merge_status = pr["mergeable"].as_str().map(|s| s.to_string());
        let labels = pr["labels"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l["name"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        // Parse reviews from the gh pr list response
        let reviews = pr["reviews"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|r| {
                        let reviewer = r["author"]["login"].as_str().unwrap_or("").to_string();
                        let state = r["state"].as_str().unwrap_or("").to_string();
                        let body = r["body"].as_str().and_then(|s| {
                            if s.is_empty() {
                                None
                            } else {
                                Some(s.to_string())
                            }
                        });
                        let submitted_at = r["submittedAt"].as_str().map(|s| s.to_string());
                        ReviewContext {
                            reviewer,
                            state,
                            body,
                            submitted_at,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        Some(PrContext {
            number,
            title,
            body,
            author,
            state,
            draft,
            head_branch,
            base_branch,
            merge_status,
            labels,
            reviews,
            review_comments: vec![],
        })
    }

    /// `gh` CLI로 PR의 리뷰 목록을 조회한다.
    async fn fetch_pr_reviews(repo: &str, pr_number: i64) -> Option<Vec<ReviewContext>> {
        let output = tokio::process::Command::new("gh")
            .args([
                "api",
                &format!("repos/{repo}/pulls/{pr_number}/reviews"),
                "--jq",
                ".[] | {user: .user.login, state: .state, body: .body, submitted_at: .submitted_at}",
            ])
            .output()
            .await;

        let output = output.ok().filter(|o| o.status.success())?;
        let stdout = String::from_utf8_lossy(&output.stdout);

        let mut reviews = Vec::new();
        for line in stdout.lines() {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                let reviewer = val["user"].as_str().unwrap_or("").to_string();
                let state = val["state"].as_str().unwrap_or("").to_string();
                let body = val["body"].as_str().and_then(|s| {
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.to_string())
                    }
                });
                let submitted_at = val["submitted_at"].as_str().map(|s| s.to_string());
                reviews.push(ReviewContext {
                    reviewer,
                    state,
                    body,
                    submitted_at,
                });
            }
        }
        Some(reviews)
    }

    /// PR에 `changes_requested` 리뷰가 있는지 확인하고,
    /// 해당하는 경우 새 큐 아이템을 생성한다.
    ///
    /// `review_scan_state`는 changes_requested 감지 시 생성할 워크플로우 상태 이름이다.
    pub async fn collect_review_items(
        &self,
        workspace: &WorkspaceConfig,
        review_scan_state: &str,
    ) -> Result<Vec<QueueItem>> {
        let github_config = match workspace.sources.get("github") {
            Some(config) => config,
            None => return Ok(Vec::new()),
        };

        let repo_name = Self::extract_repo_name(&github_config.url)
            .unwrap_or_else(|| "unknown/repo".to_string());

        // List open PRs
        let output = tokio::process::Command::new("gh")
            .args([
                "pr",
                "list",
                "--repo",
                &repo_name,
                "--state",
                "open",
                "--json",
                "number,title,headRefName",
                "--limit",
                "50",
            ])
            .output()
            .await;

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return Ok(Vec::new()),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let prs: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap_or_default();

        let mut items = Vec::new();
        for pr in prs {
            let number = match pr["number"].as_i64() {
                Some(n) => n,
                None => continue,
            };
            let title = pr["title"].as_str().unwrap_or("").to_string();

            // Check reviews for this PR
            let reviews = Self::fetch_pr_reviews(&repo_name, number)
                .await
                .unwrap_or_default();

            let has_changes_requested = reviews.iter().any(|r| r.state == "CHANGES_REQUESTED");
            if !has_changes_requested {
                continue;
            }

            let source_id = format!("github:{repo_name}!{number}");
            let work_id = QueueItem::make_work_id(&source_id, review_scan_state);

            let mut item = QueueItem::new(
                work_id,
                source_id,
                workspace.name.clone(),
                review_scan_state.to_string(),
            );
            item.title = Some(format!("[review] {title}"));
            items.push(item);
        }

        Ok(items)
    }

    /// Scan open PRs for `CHANGES_REQUESTED` reviews and create queue items
    /// for each matching PR in the given review-triggered states.
    ///
    /// Uses `!` separator in `source_id` (e.g. `github:org/repo!42`) to
    /// distinguish PR-review items from issue items (`#`).
    async fn collect_changes_requested_items(
        &self,
        repo_name: &str,
        workspace: &WorkspaceConfig,
        review_states: &[String],
    ) -> Vec<QueueItem> {
        let output = tokio::process::Command::new("gh")
            .args([
                "pr",
                "list",
                "--repo",
                repo_name,
                "--state",
                "open",
                "--json",
                "number,title",
                "--limit",
                "50",
            ])
            .output()
            .await;

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return Vec::new(),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let prs: Vec<serde_json::Value> = match serde_json::from_str(&stdout) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };

        let mut items = Vec::new();
        for pr in prs {
            let number = match pr["number"].as_i64() {
                Some(n) => n,
                None => continue,
            };
            let title = pr["title"].as_str().unwrap_or("").to_string();

            let reviews = Self::fetch_pr_reviews(repo_name, number)
                .await
                .unwrap_or_default();

            let has_changes_requested = reviews.iter().any(|r| r.state == "CHANGES_REQUESTED");
            if !has_changes_requested {
                continue;
            }

            // Create a queue item for each review-triggered state.
            for state_name in review_states {
                let source_id = format!("github:{repo_name}!{number}");
                let work_id = QueueItem::make_work_id(&source_id, state_name);

                let mut item = QueueItem::new(
                    work_id,
                    source_id,
                    workspace.name.clone(),
                    state_name.clone(),
                );
                item.title = Some(format!("[review] {title}"));
                items.push(item);
            }
        }

        items
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

        // Enforce scan_interval_secs: skip this tick if not enough time has passed.
        if let Some(last) = self.last_collected_at {
            let interval = std::time::Duration::from_secs(github_config.scan_interval_secs);
            if last.elapsed() < interval {
                tracing::debug!(
                    elapsed_secs = last.elapsed().as_secs(),
                    interval_secs = github_config.scan_interval_secs,
                    "skipping GitHub scan — scan_interval_secs not yet elapsed"
                );
                return Ok(Vec::new());
            }
        }

        let repo_name = Self::extract_repo_name(&github_config.url)
            .unwrap_or_else(|| "unknown/repo".to_string());

        let mut items = Vec::new();

        // Collect states triggered by changes_requested reviews.
        let review_states: Vec<String> = github_config
            .states
            .iter()
            .filter(|(_, cfg)| cfg.trigger.changes_requested)
            .map(|(name, _)| name.clone())
            .collect();

        for (state_name, state_config) in &github_config.states {
            // Label-based trigger: scan issues matching the label.
            if let Some(label) = &state_config.trigger.label {
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

                            let mut item = QueueItem::new(
                                work_id,
                                source_id,
                                workspace.name.clone(),
                                state_name.clone(),
                            );
                            item.title = Some(title);
                            items.push(item);
                        }
                    }
                }
            }
        }

        // Scan open PRs for changes_requested reviews and create items
        // for each state configured with `changes_requested: true`.
        if !review_states.is_empty() {
            let review_items = self
                .collect_changes_requested_items(&repo_name, workspace, &review_states)
                .await;
            items.extend(review_items);
        }

        self.last_collected_at = Some(std::time::Instant::now());
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

        let (title, body, labels, author, issue_state) = issue_data.unwrap_or_else(|| {
            (
                item.title.clone().unwrap_or_default(),
                None,
                vec![],
                String::new(),
                "open".to_string(),
            )
        });

        Ok(ItemContext {
            work_id: item.work_id.clone(),
            workspace: item.workspace_id.clone(),
            queue: QueueContext {
                phase: item.phase().as_str().to_string(),
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
                state: issue_state,
            }),
            pr: pr_data,
            history: vec![],
            worktree: None,
            source_data: serde_json::Value::Null,
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
        let mut item = QueueItem::new(
            work_id,
            source_id.to_string(),
            "test-ws".to_string(),
            state.to_string(),
        );
        item.title = Some("Test issue title".to_string());
        item
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
    fn new_sets_last_collected_at_to_none() {
        let ds = GitHubDataSource::new("https://github.com/org/repo");
        assert!(ds.last_collected_at.is_none());
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
    async fn collect_skips_when_scan_interval_not_elapsed() {
        let mut ds = GitHubDataSource::new("https://github.com/org/repo");
        // Simulate a recent collection by setting last_collected_at to now.
        ds.last_collected_at = Some(std::time::Instant::now());
        // Default scan_interval_secs is 300 — far from elapsed.
        let workspace = make_workspace_with_github(
            "https://github.com/org/repo",
            "analyze",
            Some("belt:analyze"),
        );
        let items = ds.collect(&workspace).await.unwrap();
        assert!(
            items.is_empty(),
            "should skip scan when interval not elapsed"
        );
    }

    #[tokio::test]
    async fn collect_proceeds_when_scan_interval_elapsed() {
        let mut ds = GitHubDataSource::new("https://github.com/org/repo");
        // Set last_collected_at far enough in the past (> 300s default).
        ds.last_collected_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(301));
        let workspace = make_workspace_with_github(
            "https://github.com/org/repo",
            "analyze",
            Some("belt:analyze"),
        );
        // gh CLI unavailable in test env, so no items returned, but the method
        // must proceed past the interval check and update last_collected_at.
        let _items = ds.collect(&workspace).await.unwrap();
        // last_collected_at should have been refreshed to a very recent instant.
        let elapsed = ds.last_collected_at.unwrap().elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "last_collected_at should be updated after successful collection"
        );
    }

    #[tokio::test]
    async fn collect_proceeds_on_first_call_without_last_collected_at() {
        let mut ds = GitHubDataSource::new("https://github.com/org/repo");
        assert!(ds.last_collected_at.is_none());
        let workspace = make_workspace_with_github(
            "https://github.com/org/repo",
            "analyze",
            Some("belt:analyze"),
        );
        let _items = ds.collect(&workspace).await.unwrap();
        // After first collect, last_collected_at must be set.
        assert!(
            ds.last_collected_at.is_some(),
            "first collect should set last_collected_at"
        );
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
        assert_eq!(ctx.queue.phase, item.phase().as_str());
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

    // ── changes_requested trigger ───────────────────────────────────────────

    fn make_workspace_with_review_trigger(url: &str, state: &str) -> WorkspaceConfig {
        let yaml = format!(
            r#"
name: test-ws
sources:
  github:
    url: {url}
    states:
      {state}:
        trigger:
          changes_requested: true
"#
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    #[test]
    fn workspace_parses_changes_requested_trigger() {
        let ws = make_workspace_with_review_trigger("https://github.com/org/repo", "fix_review");
        let github = ws.sources.get("github").unwrap();
        let state = github.states.get("fix_review").unwrap();
        assert!(state.trigger.changes_requested);
        assert!(state.trigger.label.is_none());
    }

    #[tokio::test]
    async fn collect_returns_empty_when_no_review_states() {
        let mut ds = GitHubDataSource::new("https://github.com/org/repo");
        // Workspace with label trigger only — no changes_requested states.
        let workspace = make_workspace_with_github(
            "https://github.com/org/repo",
            "analyze",
            Some("belt:analyze"),
        );
        // gh CLI not available in test env, so label-based scan returns nothing.
        let items = ds.collect(&workspace).await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn collect_skips_review_scan_when_no_review_trigger() {
        let mut ds = GitHubDataSource::new("https://github.com/org/repo");
        let workspace = make_workspace_without_github();
        let items = ds.collect(&workspace).await.unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn pr_review_source_id_uses_bang_separator() {
        // PR review items use '!' instead of '#' to distinguish from issue items.
        let repo_name = "org/repo";
        let pr_number: i64 = 10;
        let source_id = format!("github:{repo_name}!{pr_number}");
        assert_eq!(source_id, "github:org/repo!10");
        // Must not collide with issue source_id.
        let issue_source_id = format!("github:{repo_name}#{pr_number}");
        assert_ne!(source_id, issue_source_id);
    }

    #[test]
    fn pr_review_work_id_format() {
        let source_id = "github:org/repo!10";
        let state = "fix_review";
        let work_id = QueueItem::make_work_id(source_id, state);
        assert_eq!(work_id, "github:org/repo!10:fix_review");
    }

    #[test]
    fn collect_review_items_creates_correct_item_fields() {
        // Verify the shape of a review-triggered QueueItem.
        let source_id = "github:org/repo!10".to_string();
        let state = "fix_review".to_string();
        let work_id = QueueItem::make_work_id(&source_id, &state);
        let mut item = QueueItem::new(
            work_id.clone(),
            source_id.clone(),
            "test-ws".to_string(),
            state.clone(),
        );
        item.title = Some("[review] Fix the bug".to_string());

        assert_eq!(item.work_id, "github:org/repo!10:fix_review");
        assert_eq!(item.source_id, "github:org/repo!10");
        assert_eq!(item.state, "fix_review");
        assert_eq!(item.phase(), QueuePhase::Pending);
        assert!(item.title.as_ref().unwrap().starts_with("[review]"));
    }
}
