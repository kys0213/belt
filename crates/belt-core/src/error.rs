use thiserror::Error;

use crate::phase::QueuePhase;

#[derive(Debug, Error)]
pub enum BeltError {
    #[error("queue item not found: {0}")]
    ItemNotFound(String),

    #[error("invalid phase transition: {from} -> {to}")]
    InvalidTransition { from: QueuePhase, to: QueuePhase },

    #[error("workspace not found: {0}")]
    WorkspaceNotFound(String),

    #[error("datasource error: {0}")]
    DataSource(String),

    #[error("runtime error: {0}")]
    Runtime(String),

    #[error("database error: {0}")]
    Database(String),

    #[error("worktree error: {0}")]
    Worktree(String),

    #[error("spec not found: {0}")]
    SpecNotFound(String),

    #[error("invalid spec transition: {from} -> {to}")]
    InvalidSpecTransition { from: String, to: String },

    #[error("replan limit exceeded for {work_id}: {count} attempts (max {max})")]
    ReplanLimitExceeded {
        work_id: String,
        count: u32,
        max: u32,
    },

    #[error("stagnation error: {0}")]
    Stagnation(String),
}
