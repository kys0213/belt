//! 전이 계약의 가드 정책.
//!
//! 저장소가 트랜잭션 안에서 현재 상태를 읽어 [`guard`]를 호출하고,
//! `Proceed`일 때만 phase 변경과 이력 기록을 한 트랜잭션으로 commit한다.

use crate::escalation::EscalationAction;
use crate::hitl::HitlAction;
use crate::phase::QueuePhase;
use crate::state_machine::is_valid_transition;

/// 전이를 요청한 행위자.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    Daemon,
    Cli,
    Tui,
    Cron,
    Channel(String),
}

/// 처리 중인 아이템의 처리 종류. 소유자는 항상 daemon이다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Processing {
    Handler,
    PostProcessing,
}

/// 전이 요청의 결과. `busy`·`conflict`·`invalid_action`은 오류가 아니라 값이다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionOutcome {
    Applied { seq: u64 },
    Busy { processing: Processing },
    Conflict { current: QueuePhase },
    InvalidAction { current: QueuePhase },
}

/// 전이 사유. 이력 기록용이며, 가드는 Hitl 출구에서 `PostProcessing`인지만 본다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionReason {
    /// 사람의 직접 조작 (`queue skip` 등).
    Manual,
    /// 파생 아이템으로 이어져 끝남.
    Derived,
    /// 실행 중 취소.
    Canceled,
    /// shutdown·재시작 롤백.
    Rollback,
    /// escalation 결정의 결과.
    Escalation(EscalationAction),
    /// HITL 확정 후처리의 결과.
    PostProcessing(HitlAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionRequest {
    pub work_id: String,
    pub expected_from: QueuePhase,
    pub to: QueuePhase,
    pub actor: Actor,
    pub reason: TransitionReason,
    pub detail: Option<String>,
}

/// 트랜잭션 안에서 읽은 아이템의 현재 상태.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemSnapshot {
    pub phase: QueuePhase,
    pub processing: Option<Processing>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardDecision {
    Proceed,
    Reject(TransitionOutcome),
    /// Hitl 출구 요청이 HITL 응답으로 대응된다. 첫 응답 승리 경합으로 넘긴다.
    ConvertToHitlResponse(HitlAction),
}

/// 가드 판정 (순수 함수). 순서는 `queue-state-machine` 전이 계약 흐름도를 따른다.
///
/// 1. 처리 중이고 행위자가 소유자(daemon)가 아니면 `Busy`
/// 2. Hitl 출구인데 daemon 후처리가 아니면 HITL 응답으로 대응되는지에 따라
///    `ConvertToHitlResponse` 또는 `InvalidAction`
/// 3. daemon 후처리라도 확정되고 후처리 전인 요청이 없으면(open 요청뿐이거나
///    요청이 없음) `InvalidAction`. 후처리는 open이 아닌 요청에서만 시작한다.
/// 4. 현재 phase가 기대 phase와 다르면 `Conflict`
/// 5. 허용된 전이 집합 밖이면 `InvalidAction`
pub fn guard(snapshot: &ItemSnapshot, req: &TransitionRequest) -> GuardDecision {
    let current = snapshot.phase;

    if let Some(processing) = snapshot.processing
        && req.actor != Actor::Daemon
    {
        return GuardDecision::Reject(TransitionOutcome::Busy { processing });
    }

    if current == QueuePhase::Hitl {
        let is_post_processing =
            req.actor == Actor::Daemon && matches!(req.reason, TransitionReason::PostProcessing(_));
        if !is_post_processing {
            return match hitl_response_for(req.to) {
                Some(action) => GuardDecision::ConvertToHitlResponse(action),
                None => GuardDecision::Reject(TransitionOutcome::InvalidAction { current }),
            };
        }
        // 확정 요청 없이 Hitl을 벗어나면 open 요청이 주인 없이 남는다.
        if snapshot.processing != Some(Processing::PostProcessing) {
            return GuardDecision::Reject(TransitionOutcome::InvalidAction { current });
        }
    }

    if current != req.expected_from {
        return GuardDecision::Reject(TransitionOutcome::Conflict { current });
    }

    if !is_valid_transition(current, req.to) {
        return GuardDecision::Reject(TransitionOutcome::InvalidAction { current });
    }

    GuardDecision::Proceed
}

/// Hitl 출구 요청의 목표 phase에 대응되는 HITL 응답. 대응이 없으면 `None`.
///
/// [`guard`]의 `ConvertToHitlResponse` 판정과 같은 매핑이다. 전이 요청이
/// `InvalidAction { current: Hitl }`로 돌아온 호출자(`queue skip`/`queue done`)는
/// 이 함수로 HITL 응답 액션을 얻어 첫 응답 승리 경합(`resolve_hitl`)으로 넘긴다.
pub fn hitl_response_for(to: QueuePhase) -> Option<HitlAction> {
    match to {
        QueuePhase::Done => Some(HitlAction::Done),
        QueuePhase::Skipped => Some(HitlAction::Skip),
        QueuePhase::Pending
        | QueuePhase::Ready
        | QueuePhase::Running
        | QueuePhase::Completed
        | QueuePhase::Hitl
        | QueuePhase::Failed => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use QueuePhase::*;

    fn snap(phase: QueuePhase, processing: Option<Processing>) -> ItemSnapshot {
        ItemSnapshot { phase, processing }
    }

    fn req(
        from: QueuePhase,
        to: QueuePhase,
        actor: Actor,
        reason: TransitionReason,
    ) -> TransitionRequest {
        TransitionRequest {
            work_id: "w".into(),
            expected_from: from,
            to,
            actor,
            reason,
            detail: None,
        }
    }

    fn manual(from: QueuePhase, to: QueuePhase) -> TransitionRequest {
        req(from, to, Actor::Cli, TransitionReason::Manual)
    }

    fn invalid(current: QueuePhase) -> GuardDecision {
        GuardDecision::Reject(TransitionOutcome::InvalidAction { current })
    }

    #[test]
    fn running_handler_rejects_cli_with_busy() {
        let d = guard(
            &snap(Running, Some(Processing::Handler)),
            &manual(Running, Skipped),
        );
        assert_eq!(
            d,
            GuardDecision::Reject(TransitionOutcome::Busy {
                processing: Processing::Handler
            })
        );
    }

    #[test]
    fn hitl_post_processing_rejects_cli_with_busy() {
        let d = guard(
            &snap(Hitl, Some(Processing::PostProcessing)),
            &manual(Hitl, Skipped),
        );
        assert_eq!(
            d,
            GuardDecision::Reject(TransitionOutcome::Busy {
                processing: Processing::PostProcessing
            })
        );
    }

    #[test]
    fn daemon_owns_processing_and_proceeds() {
        let r = req(Running, Completed, Actor::Daemon, TransitionReason::Manual);
        assert_eq!(
            guard(&snap(Running, Some(Processing::Handler)), &r),
            GuardDecision::Proceed
        );
    }

    #[test]
    fn open_hitl_cli_skip_converts_to_response() {
        let d = guard(&snap(Hitl, None), &manual(Hitl, Skipped));
        assert_eq!(d, GuardDecision::ConvertToHitlResponse(HitlAction::Skip));
    }

    #[test]
    fn open_hitl_cli_done_converts_to_response() {
        let d = guard(&snap(Hitl, None), &manual(Hitl, Done));
        assert_eq!(d, GuardDecision::ConvertToHitlResponse(HitlAction::Done));
    }

    #[test]
    fn hitl_to_hitl_is_invalid_action() {
        assert_eq!(guard(&snap(Hitl, None), &manual(Hitl, Hitl)), invalid(Hitl));
    }

    #[test]
    fn hitl_exit_without_matching_response_is_invalid_action() {
        for to in [Pending, Failed] {
            assert_eq!(guard(&snap(Hitl, None), &manual(Hitl, to)), invalid(Hitl));
        }
    }

    #[test]
    fn daemon_post_processing_leaves_hitl() {
        for (to, action) in [
            (Done, HitlAction::Done),
            (Skipped, HitlAction::Skip),
            (Pending, HitlAction::Retry),
            (Failed, HitlAction::Replan),
        ] {
            let r = req(
                Hitl,
                to,
                Actor::Daemon,
                TransitionReason::PostProcessing(action),
            );
            assert_eq!(
                guard(&snap(Hitl, Some(Processing::PostProcessing)), &r),
                GuardDecision::Proceed
            );
        }
    }

    #[test]
    fn daemon_post_processing_without_confirmed_request_cannot_leave_hitl() {
        // Hitl with no confirmed request (an open request or none at all):
        // leaving would orphan the open request.
        for (to, action) in [
            (Done, HitlAction::Done),
            (Skipped, HitlAction::Skip),
            (Pending, HitlAction::Retry),
            (Failed, HitlAction::Replan),
        ] {
            let r = req(
                Hitl,
                to,
                Actor::Daemon,
                TransitionReason::PostProcessing(action),
            );
            assert_eq!(guard(&snap(Hitl, None), &r), invalid(Hitl), "to {to:?}");
        }
    }

    #[test]
    fn hitl_response_for_maps_only_done_and_skipped() {
        assert_eq!(hitl_response_for(Done), Some(HitlAction::Done));
        assert_eq!(hitl_response_for(Skipped), Some(HitlAction::Skip));
        for to in [Pending, Ready, Running, Completed, Hitl, Failed] {
            assert_eq!(hitl_response_for(to), None, "to {to:?}");
        }
    }

    #[test]
    fn daemon_without_post_processing_reason_cannot_leave_hitl() {
        let r = req(Hitl, Failed, Actor::Daemon, TransitionReason::Manual);
        assert_eq!(guard(&snap(Hitl, None), &r), invalid(Hitl));
    }

    #[test]
    fn failed_to_done_is_invalid_action() {
        assert_eq!(
            guard(&snap(Failed, None), &manual(Failed, Done)),
            invalid(Failed)
        );
    }

    #[test]
    fn failed_to_skipped_proceeds() {
        assert_eq!(
            guard(&snap(Failed, None), &manual(Failed, Skipped)),
            GuardDecision::Proceed
        );
    }

    #[test]
    fn pending_ready_skip_and_running_hitl_are_allowed() {
        assert_eq!(
            guard(&snap(Pending, None), &manual(Pending, Skipped)),
            GuardDecision::Proceed
        );
        assert_eq!(
            guard(&snap(Ready, None), &manual(Ready, Skipped)),
            GuardDecision::Proceed
        );
        let r = req(
            Running,
            Hitl,
            Actor::Daemon,
            TransitionReason::Escalation(EscalationAction::Hitl),
        );
        assert_eq!(
            guard(&snap(Running, Some(Processing::Handler)), &r),
            GuardDecision::Proceed
        );
    }

    #[test]
    fn expected_phase_mismatch_is_conflict_with_current() {
        let d = guard(&snap(Running, None), &manual(Ready, Skipped));
        assert_eq!(
            d,
            GuardDecision::Reject(TransitionOutcome::Conflict { current: Running })
        );
    }

    #[test]
    fn same_phase_request_is_invalid_action() {
        assert_eq!(
            guard(&snap(Pending, None), &manual(Pending, Pending)),
            invalid(Pending)
        );
    }

    #[test]
    fn non_hitl_reasons_do_not_affect_decision() {
        for reason in [
            TransitionReason::Derived,
            TransitionReason::Canceled,
            TransitionReason::Rollback,
            TransitionReason::Escalation(EscalationAction::Retry),
        ] {
            let r = req(Running, Pending, Actor::Daemon, reason);
            assert_eq!(
                guard(&snap(Running, Some(Processing::Handler)), &r),
                GuardDecision::Proceed
            );
        }
    }
}
