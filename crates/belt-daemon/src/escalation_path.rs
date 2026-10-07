//! Escalation path — commits the escalation decision of a failed run.
//!
//! The daemon decides the action from the lineage's failure count; this
//! module turns it into one store transition:
//! - `Retry`/`RetryWithComment`: the origin ends Running -> Skipped (derived)
//!   and a derived Pending item continues the work in the handed-over worktree
//! - `Hitl`/`Replan`: Running -> Hitl together with an open HITL request that
//!   carries the lineage's lateral thinking history
//! - `Skip`: Running -> Skipped
//!
//! Hooks react to an applied commit only; the daemon runs them afterwards so
//! a hook failure never undoes the committed state.

use belt_core::error::BeltError;
use belt_core::escalation::EscalationAction;
use belt_core::phase::QueuePhase;
use belt_core::queue::{HitlReason, QueueItem};
use belt_core::transition::{Actor, TransitionOutcome, TransitionReason, TransitionRequest};
use belt_infra::db::{
    Database, DeriveKind, DeriveOutcome, DeriveRequest, OpenHitlOutcome, OpenHitlRequest,
};

use crate::hitl::{HitlExpiry, HitlService};

/// Phase an escalated execution leaves: escalation decides the fate of a failed run.
const ESCALATION_FROM: QueuePhase = QueuePhase::Running;

/// Result of committing an escalation decision to the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EscalationCommit {
    /// The store applied the result transition.
    Applied(Committed),
    /// The stored phase is no longer Running. Nothing else may run.
    Conflict { current: QueuePhase },
    /// The transition contract refused the request for another reason.
    Rejected(TransitionOutcome),
}

/// What an applied escalation left in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Committed {
    /// The origin ended as Skipped (derived); the work continues in `work_id`.
    Derived { work_id: String },
    /// The item ended as Skipped.
    Skipped,
    /// The item entered Hitl with an open request.
    Hitl,
}

/// Commit the result transition that realizes `action` for the Running `work_id`.
///
/// `hitl_notes` and `expiry` are recorded on the HITL request when `action`
/// opens one.
///
/// # Errors
/// `BeltError` when the store fails (I/O, unknown `work_id`, another open
/// item of the same `(source_id, state)`).
pub(crate) fn commit(
    hitl: &HitlService,
    work_id: &str,
    action: EscalationAction,
    hitl_notes: Option<String>,
    expiry: &HitlExpiry,
) -> Result<EscalationCommit, BeltError> {
    let db = hitl.database();
    let reason = TransitionReason::Escalation(action);
    match action {
        EscalationAction::Retry | EscalationAction::RetryWithComment => {
            let outcome = db.derive(&DeriveRequest {
                work_id: work_id.to_string(),
                expected_from: ESCALATION_FROM,
                kind: DeriveKind::EscalationRetry,
                actor: Actor::Daemon,
                reason: TransitionReason::Derived,
                detail: Some(format!("escalation: {action}")),
            })?;
            match outcome {
                DeriveOutcome::Derived { work_id } => {
                    Ok(EscalationCommit::Applied(Committed::Derived { work_id }))
                }
                DeriveOutcome::Rejected(refused) => refusal(refused),
            }
        }
        EscalationAction::Hitl | EscalationAction::Replan => {
            let outcome = hitl.open(&OpenHitlRequest {
                work_id: work_id.to_string(),
                expected_from: ESCALATION_FROM,
                reason: HitlReason::RetryMaxExceeded,
                notes: hitl_notes,
                actor: Actor::Daemon,
                transition_reason: reason,
                timeout_at: expiry.timeout_at.clone(),
                terminal_action: expiry.terminal_action,
            })?;
            match outcome {
                OpenHitlOutcome::Opened { .. } => Ok(EscalationCommit::Applied(Committed::Hitl)),
                OpenHitlOutcome::Rejected(refused) => refusal(refused),
            }
        }
        EscalationAction::Skip => {
            let outcome = db.transition(&TransitionRequest {
                work_id: work_id.to_string(),
                expected_from: ESCALATION_FROM,
                to: QueuePhase::Skipped,
                actor: Actor::Daemon,
                reason,
                detail: Some(format!("escalation: {action}")),
            })?;
            match outcome {
                TransitionOutcome::Applied { .. } => {
                    Ok(EscalationCommit::Applied(Committed::Skipped))
                }
                refused @ (TransitionOutcome::Conflict { .. }
                | TransitionOutcome::Busy { .. }
                | TransitionOutcome::InvalidAction { .. }) => refusal(refused),
            }
        }
    }
}

/// Classify a refused result transition.
///
/// `InvalidAction` naming a phase other than Running means the row moved
/// on (a Hitl row refuses every non-post-processing exit this way), so the
/// stored phase wins just as for `Conflict`.
fn refusal(outcome: TransitionOutcome) -> Result<EscalationCommit, BeltError> {
    match outcome {
        TransitionOutcome::Conflict { current } => Ok(EscalationCommit::Conflict { current }),
        TransitionOutcome::InvalidAction { current } if current != ESCALATION_FROM => {
            Ok(EscalationCommit::Conflict { current })
        }
        refused @ (TransitionOutcome::InvalidAction { .. } | TransitionOutcome::Busy { .. }) => {
            Ok(EscalationCommit::Rejected(refused))
        }
        TransitionOutcome::Applied { seq } => Err(BeltError::Database(format!(
            "store reported an applied transition (seq {seq}) as a refusal"
        ))),
    }
}

/// HITL notes listing the lateral thinking history of `item`'s lineage.
///
/// Stagnation events of every item from the first of the lineage up to
/// `item` (following the derivation origins) are attached, so reviewers see
/// what automated approaches were already attempted. `None` when there is
/// neither a lateral plan nor a stagnation event.
///
/// # Errors
/// `BeltError` when the store cannot be read.
pub(crate) fn lineage_hitl_notes(
    db: &Database,
    item: &QueueItem,
    lateral_plan: Option<&str>,
) -> Result<Option<String>, BeltError> {
    let mut lineage = vec![item.work_id.clone()];
    let mut origin = item.derived_from.clone();
    while let Some(work_id) = origin {
        origin = db.get_item(&work_id)?.derived_from;
        lineage.push(work_id);
    }
    lineage.reverse();

    let mut stagnation_events = Vec::new();
    for work_id in &lineage {
        stagnation_events.extend(
            db.list_transition_events(work_id)?
                .into_iter()
                .filter(|e| e.event_type == "stagnation"),
        );
    }

    if lateral_plan.is_none() && stagnation_events.is_empty() {
        return Ok(None);
    }

    let mut notes = String::from("## Lateral Thinking History\n");
    if let Some(plan) = lateral_plan {
        notes.push_str(&format!("- Current lateral plan: {plan}\n"));
    }
    notes.push_str(&format!(
        "- Stagnation events: {}건\n",
        stagnation_events.len()
    ));

    // Extract pattern/confidence/persona from each stagnation event detail (JSON).
    for ev in &stagnation_events {
        if let Some(detail) = &ev.detail
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(detail)
        {
            let pattern = parsed
                .get("pattern_type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let confidence = parsed
                .get("confidence")
                .and_then(|v| v.as_f64())
                .map(|c| format!("{c:.2}"))
                .unwrap_or_else(|| "N/A".to_string());
            let persona = parsed
                .get("recommended_persona")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            notes.push_str(&format!(
                "- Item: {}\n- Pattern: {pattern} (confidence: {confidence})\n- Persona: {persona}\n",
                ev.work_id
            ));
        }
    }

    Ok(Some(notes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_infra::db::{CollectOutcome, NewItem, TransitionEvent};
    use std::sync::Arc;

    fn commit_on(
        db: &Arc<Database>,
        work_id: &str,
        action: EscalationAction,
        hitl_notes: Option<String>,
    ) -> Result<EscalationCommit, BeltError> {
        commit(
            &HitlService::new(Arc::clone(db)),
            work_id,
            action,
            hitl_notes,
            &HitlExpiry::default(),
        )
    }

    const SOURCE: &str = "src:1";
    const STATE: &str = "implement";

    fn collect(db: &Database) -> String {
        match db
            .insert_collected(&NewItem {
                source_id: SOURCE.to_string(),
                workspace_id: "ws".to_string(),
                state: STATE.to_string(),
                title: None,
                actor: Actor::Daemon,
            })
            .unwrap()
        {
            CollectOutcome::Inserted { work_id } => work_id,
            CollectOutcome::Duplicate => panic!("duplicate"),
        }
    }

    fn run(db: &Database, work_id: &str) {
        for (from, to) in [
            (QueuePhase::Pending, QueuePhase::Ready),
            (QueuePhase::Ready, QueuePhase::Running),
        ] {
            let outcome = db
                .transition(&TransitionRequest {
                    work_id: work_id.to_string(),
                    expected_from: from,
                    to,
                    actor: Actor::Daemon,
                    reason: TransitionReason::Advance,
                    detail: None,
                })
                .unwrap();
            assert!(matches!(outcome, TransitionOutcome::Applied { .. }));
        }
    }

    fn stagnation(db: &Database, work_id: &str, pattern: &str) {
        db.insert_transition_event(&TransitionEvent {
            id: format!("ev-{work_id}-{pattern}"),
            work_id: work_id.to_string(),
            source_id: SOURCE.to_string(),
            event_type: "stagnation".to_string(),
            phase: None,
            from_phase: None,
            detail: Some(
                serde_json::json!({
                    "pattern_type": pattern,
                    "confidence": 0.95,
                    "recommended_persona": "contrarian",
                })
                .to_string(),
            ),
            created_at: chrono::Utc::now().to_rfc3339(),
        })
        .unwrap();
    }

    #[test]
    fn retry_derives_a_pending_item_from_the_running_one() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let first = collect(&db);
        run(&db, &first);

        let commit = commit_on(&db, &first, EscalationAction::RetryWithComment, None).unwrap();

        let EscalationCommit::Applied(Committed::Derived { work_id }) = commit else {
            panic!("expected a derived item, got {commit:?}");
        };
        assert_eq!(db.get_item(&first).unwrap().phase(), QueuePhase::Skipped);
        let derived = db.get_item(&work_id).unwrap();
        assert_eq!(derived.phase(), QueuePhase::Pending);
        assert_eq!(derived.derived_from.as_deref(), Some(first.as_str()));
        assert_eq!(db.worktree_key(&work_id).unwrap(), first);
    }

    #[test]
    fn retry_on_a_moved_row_is_a_conflict() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let first = collect(&db);
        run(&db, &first);
        db.update_phase(&first, QueuePhase::Skipped).unwrap();

        let commit = commit_on(&db, &first, EscalationAction::Retry, None).unwrap();

        assert_eq!(
            commit,
            EscalationCommit::Conflict {
                current: QueuePhase::Skipped
            }
        );
    }

    #[test]
    fn retry_on_a_hitl_row_follows_the_stored_phase() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let first = collect(&db);
        run(&db, &first);
        db.update_phase(&first, QueuePhase::Hitl).unwrap();

        let commit = commit_on(&db, &first, EscalationAction::Retry, None).unwrap();

        assert_eq!(
            commit,
            EscalationCommit::Conflict {
                current: QueuePhase::Hitl
            }
        );
    }

    #[test]
    fn hitl_opens_a_request_with_the_notes() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let first = collect(&db);
        run(&db, &first);

        let commit = commit_on(
            &db,
            &first,
            EscalationAction::Hitl,
            Some("notes".to_string()),
        )
        .unwrap();

        assert_eq!(commit, EscalationCommit::Applied(Committed::Hitl));
        assert_eq!(db.get_item(&first).unwrap().phase(), QueuePhase::Hitl);
        let entering = db
            .transitions_of(&first)
            .unwrap()
            .into_iter()
            .find(|e| e.to_phase.as_deref() == Some("hitl"))
            .expect("running -> hitl is logged");
        assert_eq!(entering.reason.as_deref(), Some("escalation:hitl"));
        let request = db
            .hitl_request(&belt_core::hitl::HitlId::new(format!(
                "hitl-{}",
                entering.seq
            )))
            .unwrap()
            .expect("the request is opened with the transition");
        assert_eq!(request.notes.as_deref(), Some("notes"));
        assert_eq!(request.reason, Some(HitlReason::RetryMaxExceeded));
    }

    #[test]
    fn notes_are_none_without_plan_or_events() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let first = collect(&db);
        let item = db.get_item(&first).unwrap();

        assert_eq!(lineage_hitl_notes(&db, &item, None).unwrap(), None);
    }

    #[test]
    fn notes_include_the_plan() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let first = collect(&db);
        let item = db.get_item(&first).unwrap();

        let notes = lineage_hitl_notes(&db, &item, Some("try a different approach"))
            .unwrap()
            .expect("notes");
        assert!(notes.contains("## Lateral Thinking History"));
        assert!(notes.contains("try a different approach"));
        assert!(notes.contains("Stagnation events: 0"));
    }

    #[test]
    fn notes_cover_the_stagnation_events_of_the_whole_lineage() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let first = collect(&db);
        run(&db, &first);
        stagnation(&db, &first, "spinning");
        let EscalationCommit::Applied(Committed::Derived { work_id: second }) =
            commit_on(&db, &first, EscalationAction::Retry, None).unwrap()
        else {
            panic!("expected a derived item");
        };
        stagnation(&db, &second, "oscillation");
        let item = db.get_item(&second).unwrap();

        let notes = lineage_hitl_notes(&db, &item, None)
            .unwrap()
            .expect("notes");
        assert!(notes.contains("Stagnation events: 2"), "{notes}");
        assert!(notes.contains("Pattern: spinning (confidence: 0.95)"));
        assert!(notes.contains("Pattern: oscillation (confidence: 0.95)"));
        assert!(notes.contains("Persona: contrarian"));
        let spinning = notes.find("spinning").unwrap();
        let oscillation = notes.find("oscillation").unwrap();
        assert!(spinning < oscillation, "oldest item first");
    }
}
