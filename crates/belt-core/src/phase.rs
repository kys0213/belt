use std::fmt;

use serde::{Deserialize, Serialize};

/// Queue item phase lifecycle (8 phases).
///
/// ```text
/// Pending → Ready → Running → Completed → Done | HITL | Failed | Skipped
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueuePhase {
    Pending,
    Ready,
    Running,
    Completed,
    Done,
    Hitl,
    Failed,
    Skipped,
}

impl QueuePhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            QueuePhase::Pending => "pending",
            QueuePhase::Ready => "ready",
            QueuePhase::Running => "running",
            QueuePhase::Completed => "completed",
            QueuePhase::Done => "done",
            QueuePhase::Hitl => "hitl",
            QueuePhase::Failed => "failed",
            QueuePhase::Skipped => "skipped",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, QueuePhase::Done | QueuePhase::Skipped)
    }

    pub fn needs_human(&self) -> bool {
        matches!(self, QueuePhase::Hitl | QueuePhase::Failed)
    }
}

impl std::str::FromStr for QueuePhase {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(QueuePhase::Pending),
            "ready" => Ok(QueuePhase::Ready),
            "running" => Ok(QueuePhase::Running),
            "completed" => Ok(QueuePhase::Completed),
            "done" => Ok(QueuePhase::Done),
            "hitl" => Ok(QueuePhase::Hitl),
            "failed" => Ok(QueuePhase::Failed),
            "skipped" => Ok(QueuePhase::Skipped),
            _ => Err(format!("invalid queue phase: {s}")),
        }
    }
}

impl fmt::Display for QueuePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_phases_roundtrip() {
        let phases = [
            QueuePhase::Pending,
            QueuePhase::Ready,
            QueuePhase::Running,
            QueuePhase::Completed,
            QueuePhase::Done,
            QueuePhase::Hitl,
            QueuePhase::Failed,
            QueuePhase::Skipped,
        ];
        for phase in phases {
            let s = phase.to_string();
            let parsed: QueuePhase = s.parse().unwrap();
            assert_eq!(phase, parsed);
        }
    }

    #[test]
    fn terminal_phases() {
        assert!(QueuePhase::Done.is_terminal());
        assert!(QueuePhase::Skipped.is_terminal());
        assert!(!QueuePhase::Failed.is_terminal());
        assert!(!QueuePhase::Hitl.is_terminal());
    }

    #[test]
    fn needs_human_phases() {
        assert!(QueuePhase::Hitl.needs_human());
        assert!(QueuePhase::Failed.needs_human());
        assert!(!QueuePhase::Done.needs_human());
    }

    #[test]
    fn serde_json_roundtrip() {
        let phase = QueuePhase::Completed;
        let json = serde_json::to_string(&phase).unwrap();
        assert_eq!(json, "\"completed\"");
        let parsed: QueuePhase = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, phase);
    }

    #[test]
    fn phase_count_is_eight() {
        let all = [
            QueuePhase::Pending,
            QueuePhase::Ready,
            QueuePhase::Running,
            QueuePhase::Completed,
            QueuePhase::Done,
            QueuePhase::Hitl,
            QueuePhase::Failed,
            QueuePhase::Skipped,
        ];
        assert_eq!(all.len(), 8);
    }
}
