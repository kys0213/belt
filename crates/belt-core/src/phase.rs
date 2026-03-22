use serde::{Deserialize, Serialize};

/// Queue item phase — the 8-state machine from spec v5.
///
/// ```text
/// Pending → Ready → Running → Completed → Done | HITL | Failed | Skipped
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuePhase {
    /// DataSource.collect() detected, waiting in queue.
    Pending,
    /// Ready to run (auto-transition from Pending).
    Ready,
    /// Worktree created, handlers executing.
    Running,
    /// All handlers succeeded, awaiting evaluate.
    Completed,
    /// Evaluate judged done + on_done script succeeded.
    Done,
    /// Evaluate or escalation determined human review needed.
    Hitl,
    /// on_done script failed or infrastructure error.
    Failed,
    /// Escalation skip or preflight failure.
    Skipped,
}

impl QueuePhase {
    /// Returns whether this phase is terminal (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Skipped)
    }

    /// Validate whether a transition from `self` to `to` is allowed.
    pub fn can_transition_to(&self, to: &QueuePhase) -> bool {
        matches!(
            (self, to),
            (Self::Pending, Self::Ready)
                | (Self::Ready, Self::Running)
                | (Self::Running, Self::Completed)
                | (Self::Running, Self::Failed)
                | (Self::Completed, Self::Done)
                | (Self::Completed, Self::Hitl)
                | (Self::Done, Self::Done) // idempotent
                | (Self::Hitl, Self::Done)
                | (Self::Hitl, Self::Skipped)
                | (Self::Hitl, Self::Pending) // retry from HITL
        )
    }
}

impl std::fmt::Display for QueuePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Pending => "Pending",
            Self::Ready => "Ready",
            Self::Running => "Running",
            Self::Completed => "Completed",
            Self::Done => "Done",
            Self::Hitl => "HITL",
            Self::Failed => "Failed",
            Self::Skipped => "Skipped",
        };
        write!(f, "{s}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_transitions() {
        let path = [
            QueuePhase::Pending,
            QueuePhase::Ready,
            QueuePhase::Running,
            QueuePhase::Completed,
            QueuePhase::Done,
        ];
        for window in path.windows(2) {
            assert!(
                window[0].can_transition_to(&window[1]),
                "{:?} -> {:?} should be valid",
                window[0],
                window[1]
            );
        }
    }

    #[test]
    fn invalid_backward_transition() {
        assert!(!QueuePhase::Completed.can_transition_to(&QueuePhase::Running));
        assert!(!QueuePhase::Done.can_transition_to(&QueuePhase::Pending));
    }

    #[test]
    fn terminal_phases() {
        assert!(QueuePhase::Done.is_terminal());
        assert!(QueuePhase::Skipped.is_terminal());
        assert!(!QueuePhase::Failed.is_terminal());
        assert!(!QueuePhase::Hitl.is_terminal());
    }
}
