use async_trait::async_trait;

use crate::error::BeltError;
use crate::queue::QueueItem;

/// External system abstraction.
///
/// Each DataSource knows how to:
/// 1. **collect** — detect new items matching trigger conditions
/// 2. **get_context** — retrieve external system context for a given item
#[async_trait]
pub trait DataSource: Send + Sync {
    /// DataSource name (e.g. "github", "jira").
    fn name(&self) -> &str;

    /// Detect new items from the external system.
    async fn collect(&mut self) -> Result<Vec<QueueItem>, BeltError>;

    /// Retrieve context for a queue item (called by `belt context` CLI).
    async fn get_context(&self, item: &QueueItem) -> Result<serde_json::Value, BeltError>;
}
