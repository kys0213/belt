use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;

use belt_core::context::{HistoryEntry, ItemContext, QueueContext, SourceContext};
use belt_core::queue::QueueItem;
use belt_core::source::DataSource;
use belt_core::workspace::WorkspaceConfig;

/// 테스트용 MockDataSource.
pub struct MockDataSource {
    source_name: String,
    items: Vec<QueueItem>,
    contexts: HashMap<String, ItemContext>,
}

impl MockDataSource {
    pub fn new(name: &str) -> Self {
        Self {
            source_name: name.to_string(),
            items: Vec::new(),
            contexts: HashMap::new(),
        }
    }

    pub fn add_item(&mut self, item: QueueItem) {
        self.items.push(item);
    }

    pub fn set_context(&mut self, work_id: &str, context: ItemContext) {
        self.contexts.insert(work_id.to_string(), context);
    }

    pub fn default_context(item: &QueueItem) -> ItemContext {
        ItemContext {
            work_id: item.work_id.clone(),
            workspace: item.workspace_id.clone(),
            queue: QueueContext {
                phase: item.phase.as_str().to_string(),
                state: item.state.clone(),
                source_id: item.source_id.clone(),
            },
            source: SourceContext {
                source_type: "mock".to_string(),
                url: "https://mock.example.com".to_string(),
                default_branch: Some("main".to_string()),
            },
            issue: None,
            pr: None,
            history: Vec::new(),
            worktree: None,
        }
    }

    pub fn context_with_history(item: &QueueItem, history: Vec<HistoryEntry>) -> ItemContext {
        let mut ctx = Self::default_context(item);
        ctx.history = history;
        ctx
    }
}

#[async_trait]
impl DataSource for MockDataSource {
    fn name(&self) -> &str {
        &self.source_name
    }

    async fn collect(&mut self, _workspace: &WorkspaceConfig) -> Result<Vec<QueueItem>> {
        Ok(std::mem::take(&mut self.items))
    }

    async fn get_context(&self, item: &QueueItem) -> Result<ItemContext> {
        match self.contexts.get(&item.work_id) {
            Some(ctx) => Ok(ctx.clone()),
            None => Ok(Self::default_context(item)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::queue::testing::test_item;

    #[tokio::test]
    async fn collect_returns_added_items() {
        let mut source = MockDataSource::new("github");
        source.add_item(test_item("github:org/repo#1", "analyze"));
        source.add_item(test_item("github:org/repo#2", "implement"));

        let config: WorkspaceConfig = serde_yaml::from_str("name: test\nsources: {}").unwrap();
        let items = source.collect(&config).await.unwrap();
        assert_eq!(items.len(), 2);

        let items = source.collect(&config).await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn get_context_default() {
        let source = MockDataSource::new("github");
        let item = test_item("github:org/repo#1", "analyze");
        let ctx = source.get_context(&item).await.unwrap();
        assert_eq!(ctx.work_id, item.work_id);
        assert_eq!(ctx.source.source_type, "mock");
    }
}
