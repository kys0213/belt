use std::path::PathBuf;

use async_trait::async_trait;

use crate::context::ItemContext;
use crate::escalation::EscalationAction;
use crate::queue::QueueItem;

/// Context passed to lifecycle hook methods.
///
/// Provides the same information as `belt context` CLI, structured for
/// programmatic consumption by hook implementations.
#[derive(Debug, Clone)]
pub struct HookContext {
    /// Unique work identifier (e.g. "github:org/repo#42:implement").
    pub work_id: String,
    /// Worktree path assigned to this item.
    pub worktree: PathBuf,
    /// The queue item undergoing a phase transition.
    pub item: QueueItem,
    /// Full context from `DataSource::get_context()`.
    pub item_context: ItemContext,
    /// Number of prior failures for this item's state.
    pub failure_count: u32,
}

/// Lifecycle hook for reacting to phase transitions.
///
/// Each `DataSource` type provides its own implementation (e.g.
/// `GitHubLifecycleHook`, `JiraLifecycleHook`).  The Daemon triggers
/// hook methods at transition points without knowing the concrete
/// reaction — this satisfies OCP: adding a new DataSource type only
/// requires a new `LifecycleHook` impl, zero core changes.
///
/// # Error handling policy
///
/// | hook             | on failure                              |
/// |------------------|-----------------------------------------|
/// | `on_enter`       | skip handler, enter escalation path     |
/// | `on_done`        | transition to Failed                    |
/// | `on_fail`        | log only, do not interrupt flow         |
/// | `on_escalation`  | log only, escalation proceeds           |
#[async_trait]
pub trait LifecycleHook: Send + Sync {
    /// Called after entering Running, before handler execution.
    ///
    /// A failure here skips the handler and enters the escalation path.
    async fn on_enter(&self, ctx: &HookContext) -> anyhow::Result<()>;

    /// Called after the evaluator judges the item as Done.
    ///
    /// A failure here transitions the item to Failed.
    async fn on_done(&self, ctx: &HookContext) -> anyhow::Result<()>;

    /// Called on handler or `on_enter` failure (except silent retry).
    ///
    /// Failures are logged but do not interrupt the flow.
    async fn on_fail(&self, ctx: &HookContext) -> anyhow::Result<()>;

    /// Called after an escalation decision is made.
    ///
    /// The `action` parameter tells the hook which escalation path was
    /// chosen so it can react accordingly (e.g. add a HITL label,
    /// post a retry comment).
    async fn on_escalation(
        &self,
        ctx: &HookContext,
        action: EscalationAction,
    ) -> anyhow::Result<()>;
}

/// A no-op lifecycle hook that does nothing.
///
/// Useful as a default when no hook behavior is configured, and as a
/// test double.
pub struct NoopLifecycleHook;

#[async_trait]
impl LifecycleHook for NoopLifecycleHook {
    async fn on_enter(&self, _ctx: &HookContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn on_done(&self, _ctx: &HookContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn on_fail(&self, _ctx: &HookContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn on_escalation(
        &self,
        _ctx: &HookContext,
        _action: EscalationAction,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ItemContext, QueueContext, SourceContext};
    use crate::queue::testing::test_item;

    fn make_hook_context() -> HookContext {
        let item = test_item("github:org/repo#42", "implement");
        HookContext {
            work_id: item.work_id.clone(),
            worktree: PathBuf::from("/tmp/belt/test-ws-42"),
            item,
            item_context: ItemContext {
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
                issue: None,
                pr: None,
                history: vec![],
                worktree: Some("/tmp/belt/test-ws-42".to_string()),
                source_data: serde_json::Value::Null,
            },
            failure_count: 0,
        }
    }

    #[test]
    fn hook_context_fields_accessible() {
        let ctx = make_hook_context();
        assert_eq!(ctx.work_id, "github:org/repo#42:implement");
        assert_eq!(ctx.worktree, PathBuf::from("/tmp/belt/test-ws-42"));
        assert_eq!(ctx.item.state, "implement");
        assert_eq!(ctx.failure_count, 0);
    }

    #[tokio::test]
    async fn noop_hook_succeeds() {
        let hook = NoopLifecycleHook;
        let ctx = make_hook_context();

        hook.on_enter(&ctx).await.unwrap();
        hook.on_done(&ctx).await.unwrap();
        hook.on_fail(&ctx).await.unwrap();
        hook.on_escalation(&ctx, EscalationAction::Retry)
            .await
            .unwrap();
        hook.on_escalation(&ctx, EscalationAction::Hitl)
            .await
            .unwrap();
    }

    #[test]
    fn noop_hook_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NoopLifecycleHook>();
    }

    #[test]
    fn lifecycle_hook_is_object_safe() {
        // Verify LifecycleHook can be used as a trait object.
        fn _accepts_boxed(_hook: Box<dyn LifecycleHook>) {}
    }
}
