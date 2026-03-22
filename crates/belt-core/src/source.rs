use anyhow::Result;
use async_trait::async_trait;

use crate::context::ItemContext;
use crate::queue::QueueItem;
use crate::workspace::WorkspaceConfig;

/// DataSource trait — 외부 시스템 추상화.
#[async_trait]
pub trait DataSource: Send + Sync {
    /// DataSource 이름 (e.g. "github")
    fn name(&self) -> &str;

    /// 외부 시스템을 스캔하여 새 큐 아이템을 수집한다.
    async fn collect(&mut self, workspace: &WorkspaceConfig) -> Result<Vec<QueueItem>>;

    /// 큐 아이템의 전체 컨텍스트를 구성한다.
    async fn get_context(&self, item: &QueueItem) -> Result<ItemContext>;
}
