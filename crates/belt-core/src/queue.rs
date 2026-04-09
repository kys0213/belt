use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::BeltError;
use crate::escalation::EscalationAction;
use crate::phase::QueuePhase;

/// HITL 생성 경로 — 어떤 원인으로 HITL에 진입했는지 기록한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HitlReason {
    /// evaluate 실패 (partial result).
    EvaluateFailure,
    /// retry 최대 횟수 초과.
    RetryMaxExceeded,
    /// 실행 timeout.
    Timeout,
    /// 수동 escalation (사용자 요청).
    ManualEscalation,
    /// Spec 충돌 감지 — 파일 수준 overlap으로 사람의 판단 필요.
    SpecConflict,
    /// 정체 감지 — 에이전트가 반복/진동/무진전 등의 stagnation 패턴을 보임.
    StagnationDetected,
    /// spec Completing 단계 최종 확인 (gap-detection 통과 후 HITL 승인 대기).
    SpecCompletionReview,
    /// Claw 에이전트가 제안한 스펙 수정.
    SpecModificationProposed,
}

impl fmt::Display for HitlReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HitlReason::EvaluateFailure => f.write_str("evaluate_failure"),
            HitlReason::RetryMaxExceeded => f.write_str("retry_max_exceeded"),
            HitlReason::Timeout => f.write_str("timeout"),
            HitlReason::ManualEscalation => f.write_str("manual_escalation"),
            HitlReason::SpecConflict => f.write_str("spec_conflict"),
            HitlReason::StagnationDetected => f.write_str("stagnation_detected"),
            HitlReason::SpecCompletionReview => f.write_str("spec_completion_review"),
            HitlReason::SpecModificationProposed => f.write_str("spec_modification_proposed"),
        }
    }
}

/// HITL 응답 액션 — 사용자가 HITL 아이템에 대해 취할 수 있는 행동.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HitlRespondAction {
    /// 완료 처리 (HITL → Done).
    Done,
    /// 재시도 (HITL → Pending).
    Retry,
    /// 건너뛰기 (HITL → Skipped).
    Skip,
    /// 재계획 (HITL → Failed, replan 트리거).
    Replan,
}

impl std::str::FromStr for HitlRespondAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "done" => Ok(HitlRespondAction::Done),
            "retry" => Ok(HitlRespondAction::Retry),
            "skip" => Ok(HitlRespondAction::Skip),
            "replan" => Ok(HitlRespondAction::Replan),
            _ => Err(format!(
                "invalid HITL respond action: {s} (expected: done, retry, skip, replan)"
            )),
        }
    }
}

impl fmt::Display for HitlRespondAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HitlRespondAction::Done => f.write_str("done"),
            HitlRespondAction::Retry => f.write_str("retry"),
            HitlRespondAction::Skip => f.write_str("skip"),
            HitlRespondAction::Replan => f.write_str("replan"),
        }
    }
}

/// HITL timeout 기본값 (24시간).
pub const HITL_TIMEOUT_HOURS: u64 = 24;

/// A history event recorded whenever a noteworthy phase transition or failure occurs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEvent {
    /// The work_id of the queue item.
    pub work_id: String,
    /// The source_id of the queue item.
    pub source_id: String,
    /// The workflow state.
    pub state: String,
    /// Status string (e.g. "failed", "completed").
    pub status: String,
    /// Attempt number.
    pub attempt: u32,
    /// Optional summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Optional error message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When this event was recorded.
    pub created_at: DateTime<Utc>,
}

/// 큐 아이템 — 컨베이어 벨트 위의 단일 작업 단위.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    /// 고유 식별자 (e.g. "github:org/repo#42:implement")
    pub work_id: String,
    /// 외부 엔티티 식별자 (e.g. "github:org/repo#42")
    pub source_id: String,
    /// 워크스페이스 식별자
    pub workspace_id: String,
    /// DataSource 정의 워크플로우 상태 (e.g. "analyze", "implement", "review")
    pub state: String,
    /// 큐 phase (Pending → Ready → Running → ...)
    pub phase: QueuePhase,
    /// 아이템 제목
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 생성 시각 (RFC3339)
    pub created_at: String,
    /// 마지막 업데이트 시각 (RFC3339)
    pub updated_at: String,
    /// HITL 진입 시각 (RFC3339). HITL phase 진입 시 설정된다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hitl_created_at: Option<String>,
    /// HITL 응답자 (e.g. 사용자 이름).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hitl_respondent: Option<String>,
    /// HITL 관련 메모 (사유, 응답 내용 등).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hitl_notes: Option<String>,
    /// HITL 생성 경로.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hitl_reason: Option<HitlReason>,
    /// HITL timeout 만료 시각 (RFC3339). `belt hitl timeout` 으로 설정된다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hitl_timeout_at: Option<String>,
    /// HITL timeout 만료 시 적용할 terminal action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hitl_terminal_action: Option<EscalationAction>,
    /// Worktree가 보존되었는지 여부.
    ///
    /// HITL 또는 Failed 전이 시 `true`로 설정되어 worktree가
    /// cleanup 없이 보존되었음을 명시적으로 기록한다.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub worktree_preserved: bool,
    /// 이전 아이템(보존된)의 worktree 경로.
    ///
    /// Retry 아이템 생성 시 실패한 원본 아이템의 worktree 경로를 저장하여
    /// `WorktreeManager::create_or_reuse`가 기존 worktree를 재사용할 수 있게 한다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_worktree_path: Option<String>,
    /// Replan 시도 횟수. 무한 루프 방지를 위해 최대 3회로 제한한다.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub replan_count: u32,
    /// Lateral plan (serialized JSON) — stagnation 감지 시 LateralAnalyzer가 생성한 계획.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lateral_plan: Option<String>,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

impl QueueItem {
    pub fn new(work_id: String, source_id: String, workspace_id: String, state: String) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            work_id,
            source_id,
            workspace_id,
            state,
            phase: QueuePhase::Pending,
            title: None,
            created_at: now.clone(),
            updated_at: now,
            hitl_created_at: None,
            hitl_respondent: None,
            hitl_notes: None,
            hitl_reason: None,
            hitl_timeout_at: None,
            hitl_terminal_action: None,
            worktree_preserved: false,
            previous_worktree_path: None,
            replan_count: 0,
            lateral_plan: None,
        }
    }

    /// work_id를 규약에 따라 생성한다.
    /// format: "{source_id}:{state}"
    pub fn make_work_id(source_id: &str, state: &str) -> String {
        format!("{source_id}:{state}")
    }

    /// Read-only accessor for the current phase.
    pub fn phase(&self) -> QueuePhase {
        self.phase
    }

    /// Validate and perform a phase transition.
    ///
    /// Uses `QueuePhase::can_transition_to` to enforce state-machine invariants.
    /// On success, updates `updated_at` and returns the previous phase.
    pub fn transit(&mut self, to: QueuePhase) -> Result<QueuePhase, BeltError> {
        let from = self.phase;
        if !from.can_transition_to(&to) {
            return Err(BeltError::InvalidTransition { from, to });
        }
        self.phase = to;
        self.updated_at = chrono::Utc::now().to_rfc3339();
        Ok(from)
    }
}

/// DB row 표현.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItemRow {
    pub work_id: String,
    pub source_id: String,
    pub workspace_id: String,
    pub state: String,
    pub phase: String,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub hitl_created_at: Option<String>,
    pub hitl_respondent: Option<String>,
    pub hitl_notes: Option<String>,
    pub hitl_reason: Option<String>,
    pub hitl_timeout_at: Option<String>,
    pub hitl_terminal_action: Option<String>,
    pub worktree_preserved: bool,
    pub previous_worktree_path: Option<String>,
    pub replan_count: u32,
    pub lateral_plan: Option<String>,
}

impl QueueItem {
    pub fn to_row(&self) -> QueueItemRow {
        QueueItemRow {
            work_id: self.work_id.clone(),
            source_id: self.source_id.clone(),
            workspace_id: self.workspace_id.clone(),
            state: self.state.clone(),
            phase: self.phase.as_str().to_string(),
            title: self.title.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            hitl_created_at: self.hitl_created_at.clone(),
            hitl_respondent: self.hitl_respondent.clone(),
            hitl_notes: self.hitl_notes.clone(),
            hitl_reason: self.hitl_reason.map(|r| r.to_string()),
            hitl_timeout_at: self.hitl_timeout_at.clone(),
            hitl_terminal_action: self.hitl_terminal_action.map(|a| a.to_string()),
            worktree_preserved: self.worktree_preserved,
            previous_worktree_path: self.previous_worktree_path.clone(),
            replan_count: self.replan_count,
            lateral_plan: self.lateral_plan.clone(),
        }
    }

    pub fn from_row(row: &QueueItemRow) -> Result<Self, String> {
        let phase: QueuePhase = row.phase.parse()?;
        let hitl_reason = row
            .hitl_reason
            .as_deref()
            .map(|s| match s {
                "evaluate_failure" => Ok(HitlReason::EvaluateFailure),
                "retry_max_exceeded" => Ok(HitlReason::RetryMaxExceeded),
                "timeout" => Ok(HitlReason::Timeout),
                "manual_escalation" => Ok(HitlReason::ManualEscalation),
                "spec_conflict" => Ok(HitlReason::SpecConflict),
                "spec_completion_review" => Ok(HitlReason::SpecCompletionReview),
                "spec_modification_proposed" => Ok(HitlReason::SpecModificationProposed),
                "stagnation_detected" => Ok(HitlReason::StagnationDetected),
                other => Err(format!("invalid hitl_reason: {other}")),
            })
            .transpose()?;
        let hitl_terminal_action = row
            .hitl_terminal_action
            .as_deref()
            .map(|s| s.parse::<EscalationAction>())
            .transpose()?;
        Ok(Self {
            work_id: row.work_id.clone(),
            source_id: row.source_id.clone(),
            workspace_id: row.workspace_id.clone(),
            state: row.state.clone(),
            phase,
            title: row.title.clone(),
            created_at: row.created_at.clone(),
            updated_at: row.updated_at.clone(),
            hitl_created_at: row.hitl_created_at.clone(),
            hitl_respondent: row.hitl_respondent.clone(),
            hitl_notes: row.hitl_notes.clone(),
            hitl_reason,
            hitl_timeout_at: row.hitl_timeout_at.clone(),
            hitl_terminal_action,
            worktree_preserved: row.worktree_preserved,
            previous_worktree_path: row.previous_worktree_path.clone(),
            replan_count: row.replan_count,
            lateral_plan: row.lateral_plan.clone(),
        })
    }

    /// Mark that the worktree is preserved (not cleaned up).
    ///
    /// Called when transitioning to HITL or Failed to record that the
    /// worktree remains on disk for debugging or human review.
    pub fn mark_worktree_preserved(&mut self) {
        self.worktree_preserved = true;
    }
}

/// 테스트 팩토리.
pub mod testing {
    use super::*;

    pub fn test_item(source_id: &str, state: &str) -> QueueItem {
        let work_id = QueueItem::make_work_id(source_id, state);
        QueueItem {
            work_id,
            source_id: source_id.to_string(),
            workspace_id: "test-ws".to_string(),
            state: state.to_string(),
            phase: QueuePhase::Pending,
            title: Some(format!("Test item: {state}")),
            created_at: "2026-03-22T00:00:00Z".to_string(),
            updated_at: "2026-03-22T00:00:00Z".to_string(),
            hitl_created_at: None,
            hitl_respondent: None,
            hitl_notes: None,
            hitl_reason: None,
            hitl_timeout_at: None,
            hitl_terminal_action: None,
            worktree_preserved: false,
            previous_worktree_path: None,
            replan_count: 0,
            lateral_plan: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn make_work_id_format() {
        let id = QueueItem::make_work_id("github:org/repo#42", "implement");
        assert_eq!(id, "github:org/repo#42:implement");
    }

    #[test]
    fn new_creates_pending() {
        let item = QueueItem::new(
            "wid".to_string(),
            "sid".to_string(),
            "ws".to_string(),
            "analyze".to_string(),
        );
        assert_eq!(item.phase, QueuePhase::Pending);
    }

    #[test]
    fn to_row_roundtrip() {
        let item = test_item("github:org/repo#42", "implement");
        let row = item.to_row();
        assert_eq!(row.phase, "pending");
        let restored = QueueItem::from_row(&row).unwrap();
        assert_eq!(restored.work_id, item.work_id);
        assert_eq!(restored.phase, item.phase);
    }

    #[test]
    fn source_id_connects_lineage() {
        let a = test_item("github:org/repo#42", "analyze");
        let i = test_item("github:org/repo#42", "implement");
        assert_eq!(a.source_id, i.source_id);
        assert_ne!(a.work_id, i.work_id);
    }

    #[test]
    fn json_roundtrip() {
        let item = test_item("github:org/repo#42", "analyze");
        let json = serde_json::to_string(&item).unwrap();
        let parsed: QueueItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.work_id, item.work_id);
        assert_eq!(parsed.phase, item.phase);
    }

    #[test]
    fn hitl_metadata_json_roundtrip() {
        let mut item = test_item("github:org/repo#42", "analyze");
        item.hitl_created_at = Some("2026-03-24T00:00:00Z".to_string());
        item.hitl_respondent = Some("irene".to_string());
        item.hitl_notes = Some("needs manual review".to_string());
        item.hitl_reason = Some(HitlReason::RetryMaxExceeded);

        let json = serde_json::to_string(&item).unwrap();
        let parsed: QueueItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.hitl_created_at, item.hitl_created_at);
        assert_eq!(parsed.hitl_respondent, item.hitl_respondent);
        assert_eq!(parsed.hitl_notes, item.hitl_notes);
        assert_eq!(parsed.hitl_reason, item.hitl_reason);
    }

    #[test]
    fn hitl_respond_action_roundtrip() {
        let actions = ["done", "retry", "skip", "replan"];
        for s in actions {
            let action: HitlRespondAction = s.parse().unwrap();
            assert_eq!(action.to_string(), s);
        }
    }

    #[test]
    fn hitl_respond_action_invalid() {
        assert!("invalid".parse::<HitlRespondAction>().is_err());
    }

    #[test]
    fn hitl_reason_display() {
        assert_eq!(HitlReason::EvaluateFailure.to_string(), "evaluate_failure");
        assert_eq!(
            HitlReason::RetryMaxExceeded.to_string(),
            "retry_max_exceeded"
        );
        assert_eq!(HitlReason::Timeout.to_string(), "timeout");
        assert_eq!(
            HitlReason::ManualEscalation.to_string(),
            "manual_escalation"
        );
        assert_eq!(HitlReason::SpecConflict.to_string(), "spec_conflict");
        assert_eq!(
            HitlReason::SpecModificationProposed.to_string(),
            "spec_modification_proposed"
        );
    }

    #[test]
    fn worktree_preserved_default_false() {
        let item = test_item("s1", "analyze");
        assert!(!item.worktree_preserved);
    }

    #[test]
    fn mark_worktree_preserved_sets_flag() {
        let mut item = test_item("s1", "analyze");
        item.mark_worktree_preserved();
        assert!(item.worktree_preserved);
    }

    #[test]
    fn worktree_preserved_skipped_in_json_when_false() {
        let item = test_item("s1", "analyze");
        let json = serde_json::to_string(&item).unwrap();
        assert!(!json.contains("worktree_preserved"));
    }

    #[test]
    fn worktree_preserved_present_in_json_when_true() {
        let mut item = test_item("s1", "analyze");
        item.mark_worktree_preserved();
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"worktree_preserved\":true"));
    }

    #[test]
    fn worktree_preserved_row_roundtrip() {
        let mut item = test_item("s1", "analyze");
        item.mark_worktree_preserved();
        let row = item.to_row();
        assert!(row.worktree_preserved);
        let restored = QueueItem::from_row(&row).unwrap();
        assert!(restored.worktree_preserved);
    }

    #[test]
    fn previous_worktree_path_default_none() {
        let item = test_item("s1", "analyze");
        assert!(item.previous_worktree_path.is_none());
    }

    #[test]
    fn previous_worktree_path_skipped_in_json_when_none() {
        let item = test_item("s1", "analyze");
        let json = serde_json::to_string(&item).unwrap();
        assert!(!json.contains("previous_worktree_path"));
    }

    #[test]
    fn previous_worktree_path_present_in_json_when_set() {
        let mut item = test_item("s1", "analyze");
        item.previous_worktree_path = Some("/tmp/worktrees/old-item".to_string());
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"previous_worktree_path\":\"/tmp/worktrees/old-item\""));
    }

    #[test]
    fn previous_worktree_path_row_roundtrip() {
        let mut item = test_item("s1", "analyze");
        item.previous_worktree_path = Some("/tmp/worktrees/old-item".to_string());
        let row = item.to_row();
        assert_eq!(
            row.previous_worktree_path.as_deref(),
            Some("/tmp/worktrees/old-item")
        );
        let restored = QueueItem::from_row(&row).unwrap();
        assert_eq!(
            restored.previous_worktree_path.as_deref(),
            Some("/tmp/worktrees/old-item")
        );
    }

    #[test]
    fn replan_count_default_zero() {
        let item = test_item("s1", "analyze");
        assert_eq!(item.replan_count, 0);
    }

    #[test]
    fn replan_count_skipped_in_json_when_zero() {
        let item = test_item("s1", "analyze");
        let json = serde_json::to_string(&item).unwrap();
        assert!(!json.contains("replan_count"));
    }

    #[test]
    fn replan_count_present_in_json_when_nonzero() {
        let mut item = test_item("s1", "analyze");
        item.replan_count = 2;
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"replan_count\":2"));
    }

    #[test]
    fn replan_count_row_roundtrip() {
        let mut item = test_item("s1", "analyze");
        item.replan_count = 3;
        let row = item.to_row();
        assert_eq!(row.replan_count, 3);
        let restored = QueueItem::from_row(&row).unwrap();
        assert_eq!(restored.replan_count, 3);
    }

    #[test]
    fn spec_modification_proposed_reason_roundtrip() {
        let mut item = test_item("s1", "analyze");
        item.hitl_reason = Some(HitlReason::SpecModificationProposed);
        let row = item.to_row();
        assert_eq!(
            row.hitl_reason.as_deref(),
            Some("spec_modification_proposed")
        );
        let restored = QueueItem::from_row(&row).unwrap();
        assert_eq!(
            restored.hitl_reason,
            Some(HitlReason::SpecModificationProposed)
        );
    }
}
