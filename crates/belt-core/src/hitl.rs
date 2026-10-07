//! HITL 요청 인스턴스·응답·취소 결과 타입.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::escalation::EscalationAction;
use crate::phase::QueuePhase;
use crate::queue::HitlRespondAction;

/// HITL 요청 인스턴스 식별자. 전역에서 고유하다.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HitlId(String);

impl HitlId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HitlId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// HITL 응답 액션. [`HitlRespondAction`]을 대체할 타입이며 그 전까지 `From`으로 공존한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HitlAction {
    Done,
    Retry,
    Skip,
    Replan,
}

impl From<HitlRespondAction> for HitlAction {
    fn from(action: HitlRespondAction) -> Self {
        match action {
            HitlRespondAction::Done => HitlAction::Done,
            HitlRespondAction::Retry => HitlAction::Retry,
            HitlRespondAction::Skip => HitlAction::Skip,
            HitlRespondAction::Replan => HitlAction::Replan,
        }
    }
}

impl From<HitlAction> for HitlRespondAction {
    fn from(action: HitlAction) -> Self {
        match action {
            HitlAction::Done => HitlRespondAction::Done,
            HitlAction::Retry => HitlRespondAction::Retry,
            HitlAction::Skip => HitlRespondAction::Skip,
            HitlAction::Replan => HitlRespondAction::Replan,
        }
    }
}

impl fmt::Display for HitlAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HitlAction::Done => f.write_str("done"),
            HitlAction::Retry => f.write_str("retry"),
            HitlAction::Skip => f.write_str("skip"),
            HitlAction::Replan => f.write_str("replan"),
        }
    }
}

/// timeout 만료 시 terminal action에 대응되는 HITL 응답.
///
/// terminal은 `skip`·`replan`만 허용된다. 그 밖의 값이면 `None`이고,
/// 호출자는 만료를 확정하지 않고 `invalid_action`으로 거절한다.
pub fn expiry_action(terminal: EscalationAction) -> Option<HitlAction> {
    match terminal {
        EscalationAction::Skip => Some(HitlAction::Skip),
        EscalationAction::Replan => Some(HitlAction::Replan),
        EscalationAction::Retry | EscalationAction::RetryWithComment | EscalationAction::Hitl => {
            None
        }
    }
}

/// HITL 요청의 상태.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HitlStatus {
    Open,
    Resolved,
    Expired,
}

/// 응답이 확정된 경로.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmPath {
    /// 사람이 액션을 직접 지정했다.
    Direct,
    /// 자연어 응답의 LLM 제안을 사람이 확인했다.
    NaturalLanguage,
}

/// 확정된 HITL 응답. `already_handled` 거절에 그대로 실린다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HitlResolution {
    pub action: HitlAction,
    /// 응답자.
    pub by: String,
    /// 응답이 들어온 경로 (예: cli, tui, github).
    pub via: String,
    /// 확정 시각 (RFC3339).
    pub at: String,
    pub path: ConfirmPath,
}

/// HITL 응답 시도의 결과. 거절도 오류가 아니라 값이다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RespondOutcome {
    Won { hitl_id: HitlId },
    AlreadyHandled(HitlResolution),
    NotFound,
    InvalidAction,
    Unauthorized,
}

/// 실행 중 취소 요청의 결과.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    Canceled,
    CanceledDirectly,
    TooLate { current: QueuePhase },
    Accepted,
    Busy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_converts_both_ways_without_loss() {
        for legacy in [
            HitlRespondAction::Done,
            HitlRespondAction::Retry,
            HitlRespondAction::Skip,
            HitlRespondAction::Replan,
        ] {
            let action = HitlAction::from(legacy);
            assert_eq!(action.to_string(), legacy.to_string());
            assert_eq!(HitlRespondAction::from(action), legacy);
        }
    }

    #[test]
    fn action_serde_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&HitlAction::Replan).unwrap(),
            "\"replan\""
        );
        let parsed: HitlAction = serde_json::from_str("\"retry\"").unwrap();
        assert_eq!(parsed, HitlAction::Retry);
    }

    #[test]
    fn expiry_action_accepts_only_terminal_actions() {
        assert_eq!(
            expiry_action(EscalationAction::Skip),
            Some(HitlAction::Skip)
        );
        assert_eq!(
            expiry_action(EscalationAction::Replan),
            Some(HitlAction::Replan)
        );
        for level in [
            EscalationAction::Retry,
            EscalationAction::RetryWithComment,
            EscalationAction::Hitl,
        ] {
            assert_eq!(expiry_action(level), None, "{level:?}");
        }
    }

    #[test]
    fn hitl_id_roundtrips_as_plain_string() {
        let id = HitlId::new("h-1");
        assert_eq!(id.as_str(), "h-1");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"h-1\"");
    }

    #[test]
    fn resolution_serde_roundtrip() {
        let resolution = HitlResolution {
            action: HitlAction::Skip,
            by: "irene".into(),
            via: "cli".into(),
            at: "2026-10-07T00:00:00Z".into(),
            path: ConfirmPath::NaturalLanguage,
        };
        let json = serde_json::to_string(&resolution).unwrap();
        assert!(json.contains("\"path\":\"natural_language\""));
        let parsed: HitlResolution = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resolution);
    }
}
