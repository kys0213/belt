use serde::{Deserialize, Serialize};

use crate::phase::QueuePhase;

/// 큐 아이템 — 컨베이어 벨트 위의 단일 작업 단위.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    /// 고유 식별자 (e.g. "github:org/repo#42:implement")
    pub work_id: String,
    /// 외부 엔티티 식별자 (e.g. "github:org/repo#42")
    pub source_id: String,
    /// 워크스페이스 식별자
    pub workspace_id: String,
    /// DataSource 정의 워크플로우 상태 (e.g. "analyze", "implement", "review")
    pub state: String,
    /// 큐 phase (Pending → Ready → Running → ...)
    pub phase: QueuePhase,
    /// 아이템 제목
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 생성 시각 (RFC3339)
    pub created_at: String,
    /// 마지막 업데이트 시각 (RFC3339)
    pub updated_at: String,
}

impl QueueItem {
    pub fn new(work_id: String, source_id: String, workspace_id: String, state: String) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            work_id,
            source_id,
            workspace_id,
            state,
            phase: QueuePhase::Pending,
            title: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// work_id를 규약에 따라 생성한다.
    /// format: "{source_id}:{state}"
    pub fn make_work_id(source_id: &str, state: &str) -> String {
        format!("{source_id}:{state}")
    }
}

/// DB row 표현.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItemRow {
    pub work_id: String,
    pub source_id: String,
    pub workspace_id: String,
    pub state: String,
    pub phase: String,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl QueueItem {
    pub fn to_row(&self) -> QueueItemRow {
        QueueItemRow {
            work_id: self.work_id.clone(),
            source_id: self.source_id.clone(),
            workspace_id: self.workspace_id.clone(),
            state: self.state.clone(),
            phase: self.phase.as_str().to_string(),
            title: self.title.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        }
    }

    pub fn from_row(row: &QueueItemRow) -> Result<Self, String> {
        let phase: QueuePhase = row.phase.parse()?;
        Ok(Self {
            work_id: row.work_id.clone(),
            source_id: row.source_id.clone(),
            workspace_id: row.workspace_id.clone(),
            state: row.state.clone(),
            phase,
            title: row.title.clone(),
            created_at: row.created_at.clone(),
            updated_at: row.updated_at.clone(),
        })
    }
}

/// 테스트 팩토리.
pub mod testing {
    use super::*;

    pub fn test_item(source_id: &str, state: &str) -> QueueItem {
        let work_id = QueueItem::make_work_id(source_id, state);
        QueueItem {
            work_id,
            source_id: source_id.to_string(),
            workspace_id: "test-ws".to_string(),
            state: state.to_string(),
            phase: QueuePhase::Pending,
            title: Some(format!("Test item: {state}")),
            created_at: "2026-03-22T00:00:00Z".to_string(),
            updated_at: "2026-03-22T00:00:00Z".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn make_work_id_format() {
        let id = QueueItem::make_work_id("github:org/repo#42", "implement");
        assert_eq!(id, "github:org/repo#42:implement");
    }

    #[test]
    fn new_creates_pending() {
        let item = QueueItem::new(
            "wid".to_string(),
            "sid".to_string(),
            "ws".to_string(),
            "analyze".to_string(),
        );
        assert_eq!(item.phase, QueuePhase::Pending);
    }

    #[test]
    fn to_row_roundtrip() {
        let item = test_item("github:org/repo#42", "implement");
        let row = item.to_row();
        assert_eq!(row.phase, "pending");
        let restored = QueueItem::from_row(&row).unwrap();
        assert_eq!(restored.work_id, item.work_id);
        assert_eq!(restored.phase, item.phase);
    }

    #[test]
    fn source_id_connects_lineage() {
        let a = test_item("github:org/repo#42", "analyze");
        let i = test_item("github:org/repo#42", "implement");
        assert_eq!(a.source_id, i.source_id);
        assert_ne!(a.work_id, i.work_id);
    }

    #[test]
    fn json_roundtrip() {
        let item = test_item("github:org/repo#42", "analyze");
        let json = serde_json::to_string(&item).unwrap();
        let parsed: QueueItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.work_id, item.work_id);
        assert_eq!(parsed.phase, item.phase);
    }
}
