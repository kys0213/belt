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
}
