use anyhow::Result;
use async_trait::async_trait;

use belt_core::context::{IssueContext, ItemContext, QueueContext, SourceContext};
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
    pub fn new(source_url: &str) -> Self {
        Self {
            source_url: source_url.to_string(),
            last_scan: None,
        }
    }

    fn extract_repo_name(url: &str) -> Option<String> {
        let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
        let parts: Vec<&str> = trimmed.split('/').collect();
        if parts.len() >= 2 {
            Some(format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1]))
        } else {
            None
        }
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
                .args(["issue", "list", "--label", label, "--json", "number,title", "-R", &repo_name])
                .output()
                .await;

            if let Ok(output) = output {
                if output.status.success() {
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
                            });
                        }
                    }
                }
            }
        }

        self.last_scan = Some(chrono::Utc::now());
        Ok(items)
    }

    async fn get_context(&self, item: &QueueItem) -> Result<ItemContext> {
        let issue_number = item
            .source_id
            .rsplit('#')
            .next()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);

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
                default_branch: Some("main".to_string()),
            },
            issue: Some(IssueContext {
                number: issue_number,
                title: item.title.clone().unwrap_or_default(),
                body: None,
                labels: vec![],
                author: String::new(),
            }),
            pr: None,
            history: vec![],
            worktree: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
