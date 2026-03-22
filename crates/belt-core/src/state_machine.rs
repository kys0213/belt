use crate::phase::QueuePhase;

/// 상태 전이 규칙 (12개 유효 전이).
pub fn is_valid_transition(from: QueuePhase, to: QueuePhase) -> bool {
    use QueuePhase::*;
    matches!(
        (from, to),
        (Pending, Ready)
            | (Ready, Running)
            | (Running, Completed)
            | (Running, Skipped)
            | (Completed, Done)
            | (Completed, Hitl)
            | (Completed, Failed)
            | (Hitl, Done)
            | (Hitl, Skipped)
            | (Hitl, Failed)
            | (Failed, Done)
            | (Failed, Skipped)
    )
}

/// 상태 전이를 시도하고 유효하지 않으면 에러를 반환한다.
pub fn transit(from: QueuePhase, to: QueuePhase) -> Result<(), TransitionError> {
    if from == to {
        return Err(TransitionError::SamePhase(from));
    }
    if is_valid_transition(from, to) {
        Ok(())
    } else {
        Err(TransitionError::Invalid { from, to })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionError {
    SamePhase(QueuePhase),
    Invalid { from: QueuePhase, to: QueuePhase },
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransitionError::SamePhase(p) => write!(f, "cannot transit to same phase: {p}"),
            TransitionError::Invalid { from, to } => {
                write!(f, "invalid transition: {from} → {to}")
            }
        }
    }
}

impl std::error::Error for TransitionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use QueuePhase::*;

    #[test]
    fn happy_path_pending_to_done() {
        assert!(transit(Pending, Ready).is_ok());
        assert!(transit(Ready, Running).is_ok());
        assert!(transit(Running, Completed).is_ok());
        assert!(transit(Completed, Done).is_ok());
    }

    #[test]
    fn completed_branches() {
        assert!(transit(Completed, Done).is_ok());
        assert!(transit(Completed, Hitl).is_ok());
        assert!(transit(Completed, Failed).is_ok());
    }

    #[test]
    fn hitl_exits() {
        assert!(transit(Hitl, Done).is_ok());
        assert!(transit(Hitl, Skipped).is_ok());
        assert!(transit(Hitl, Failed).is_ok());
    }

    #[test]
    fn backward_transitions_rejected() {
        assert!(transit(Ready, Pending).is_err());
        assert!(transit(Running, Ready).is_err());
        assert!(transit(Done, Pending).is_err());
    }

    #[test]
    fn terminal_cannot_transition() {
        assert!(transit(Done, Pending).is_err());
        assert!(transit(Skipped, Ready).is_err());
    }

    #[test]
    fn same_phase_rejected() {
        let phases = [Pending, Ready, Running, Completed, Done, Hitl, Failed, Skipped];
        for phase in phases {
            assert_eq!(transit(phase, phase).unwrap_err(), TransitionError::SamePhase(phase));
        }
    }

    #[test]
    fn exhaustive_transition_count() {
        let phases = [Pending, Ready, Running, Completed, Done, Hitl, Failed, Skipped];
        let valid_count = phases
            .iter()
            .flat_map(|&from| phases.iter().map(move |&to| (from, to)))
            .filter(|&(from, to)| is_valid_transition(from, to))
            .count();
        assert_eq!(valid_count, 12);
    }
}
