use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::phase::QueuePhase;

/// A single item flowing through the conveyor belt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    /// Unique identifier (e.g. "github:org/repo#42:implement").
    pub work_id: String,
    /// Links items from the same external entity (e.g. "github:org/repo#42").
    pub source_id: String,
    /// Workspace this item belongs to.
    pub workspace: String,
    /// Current workflow state name (e.g. "analyze", "implement", "review").
    pub state: String,
    /// Current phase in the state machine.
    pub phase: QueuePhase,
    /// Worktree path (set when Running, preserved on retry/HITL).
    pub worktree: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Append-only history event for lineage tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEvent {
    pub work_id: String,
    pub source_id: String,
    pub state: String,
    pub status: String,
    pub attempt: u32,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}
