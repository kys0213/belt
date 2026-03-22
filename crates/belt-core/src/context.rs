use serde::{Deserialize, Serialize};

/// Append-only history entry.
///
/// 모든 상태 변화를 기록하며, failure_count는 history에서 계산한다.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    pub state: String,
    pub status: HistoryStatus,
    pub attempt: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
}

/// History entry의 결과 상태.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryStatus {
    Running,
    Done,
    Failed,
    Skipped,
    Hitl,
}

impl HistoryStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            HistoryStatus::Running => "running",
            HistoryStatus::Done => "done",
            HistoryStatus::Failed => "failed",
            HistoryStatus::Skipped => "skipped",
            HistoryStatus::Hitl => "hitl",
        }
    }
}

impl std::str::FromStr for HistoryStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "running" => Ok(HistoryStatus::Running),
            "done" => Ok(HistoryStatus::Done),
            "failed" => Ok(HistoryStatus::Failed),
            "skipped" => Ok(HistoryStatus::Skipped),
            "hitl" => Ok(HistoryStatus::Hitl),
            "completed" => Ok(HistoryStatus::Done),
            _ => Err(format!("invalid history status: {s}")),
        }
    }
}

impl std::fmt::Display for HistoryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 큐 아이템의 전체 컨텍스트. `belt context` CLI의 JSON 출력 스키마.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemContext {
    pub work_id: String,
    pub workspace: String,
    pub queue: QueueContext,
    pub source: SourceContext,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue: Option<IssueContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrContext>,
    pub history: Vec<HistoryEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueContext {
    pub phase: String,
    pub state: String,
    pub source_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceContext {
    #[serde(rename = "type")]
    pub source_type: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueContext {
    pub number: i64,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub labels: Vec<String>,
    pub author: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrContext {
    pub number: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_branch: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub review_comments: Vec<String>,
}

impl ItemContext {
    /// history에서 특정 state의 failure 횟수를 계산한다.
    pub fn failure_count(&self, state: &str) -> u32 {
        self.history
            .iter()
            .filter(|h| h.state == state && h.status == HistoryStatus::Failed)
            .count() as u32
    }

    /// history에서 특정 state의 최대 attempt를 반환한다.
    pub fn max_attempt(&self, state: &str) -> u32 {
        self.history
            .iter()
            .filter(|h| h.state == state)
            .map(|h| h.attempt)
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_history_entry(state: &str, status: HistoryStatus, attempt: u32) -> HistoryEntry {
        HistoryEntry {
            state: state.to_string(),
            status,
            attempt,
            summary: None,
            error: None,
            created_at: "2026-03-22T00:00:00Z".to_string(),
        }
    }

    fn make_context(history: Vec<HistoryEntry>) -> ItemContext {
        ItemContext {
            work_id: "github:org/repo#42:implement".to_string(),
            workspace: "test-ws".to_string(),
            queue: QueueContext {
                phase: "running".to_string(),
                state: "implement".to_string(),
                source_id: "github:org/repo#42".to_string(),
            },
            source: SourceContext {
                source_type: "github".to_string(),
                url: "https://github.com/org/repo".to_string(),
                default_branch: Some("main".to_string()),
            },
            issue: Some(IssueContext {
                number: 42,
                title: "JWT middleware".to_string(),
                body: Some("Implement JWT".to_string()),
                labels: vec!["autodev:implement".to_string()],
                author: "irene".to_string(),
            }),
            pr: None,
            history,
            worktree: Some("/tmp/belt/test-ws-42".to_string()),
        }
    }

    #[test]
    fn json_roundtrip() {
        let ctx = make_context(vec![
            make_history_entry("analyze", HistoryStatus::Done, 1),
            make_history_entry("implement", HistoryStatus::Failed, 1),
            make_history_entry("implement", HistoryStatus::Running, 2),
        ]);
        let json = serde_json::to_string_pretty(&ctx).unwrap();
        let parsed: ItemContext = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.work_id, ctx.work_id);
        assert_eq!(parsed.history.len(), 3);
    }

    #[test]
    fn failure_count_filters_by_state_and_status() {
        let ctx = make_context(vec![
            make_history_entry("analyze", HistoryStatus::Done, 1),
            make_history_entry("implement", HistoryStatus::Failed, 1),
            make_history_entry("implement", HistoryStatus::Failed, 2),
            make_history_entry("implement", HistoryStatus::Running, 3),
            make_history_entry("review", HistoryStatus::Failed, 1),
        ]);
        assert_eq!(ctx.failure_count("implement"), 2);
        assert_eq!(ctx.failure_count("analyze"), 0);
        assert_eq!(ctx.failure_count("review"), 1);
        assert_eq!(ctx.failure_count("nonexistent"), 0);
    }

    #[test]
    fn max_attempt() {
        let ctx = make_context(vec![
            make_history_entry("implement", HistoryStatus::Failed, 1),
            make_history_entry("implement", HistoryStatus::Failed, 2),
            make_history_entry("implement", HistoryStatus::Running, 3),
        ]);
        assert_eq!(ctx.max_attempt("implement"), 3);
        assert_eq!(ctx.max_attempt("analyze"), 0);
    }

    #[test]
    fn history_status_roundtrip() {
        let statuses = [
            HistoryStatus::Running,
            HistoryStatus::Done,
            HistoryStatus::Failed,
            HistoryStatus::Skipped,
            HistoryStatus::Hitl,
        ];
        for status in statuses {
            let s = status.to_string();
            let parsed: HistoryStatus = s.parse().unwrap();
            assert_eq!(status, parsed);
        }
    }

    #[test]
    fn source_context_type_field_is_renamed() {
        let source = SourceContext {
            source_type: "github".to_string(),
            url: "https://github.com/org/repo".to_string(),
            default_branch: None,
        };
        let json = serde_json::to_string(&source).unwrap();
        assert!(json.contains("\"type\":\"github\""));
        assert!(!json.contains("source_type"));
    }
}
