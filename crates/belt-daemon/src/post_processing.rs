//! HITL post-processing — the only exit from Hitl.
//!
//! A HITL request confirmed by a response or an expiry holds its item in
//! Hitl until the daemon applies the verdict here, whichever process
//! confirmed it. Each tick works through the confirmed requests that are not
//! yet post-processed (state-based, so a restart resumes them):
//!
//! | action | steps before the result transition | result | worktree |
//! |---|---|---|---|
//! | done | on_done → on_hitl_resolved | Done (Failed when an on_done script fails) | cleaned on Done, kept on Failed |
//! | retry | failure-count reset point → on_hitl_resolved | Pending, same item, instruction as lateral plan | kept |
//! | skip | on_hitl_resolved | Skipped | cleaned |
//! | replan | on_hitl_resolved → derive | origin Skipped + derived Pending with the failure context | origin's cleaned, derived gets a new one |
//! | replan at the lineage limit | on_hitl_resolved | Failed | kept |
//!
//! An expired request carries its terminal action (`skip` or `replan`) as
//! the verdict, so it follows the same rows.
//!
//! The result transition and the "post-processed" mark are one store
//! transaction (`complete_post_processing`, or `derive` for a replan), so
//! the work is at-least-once: a crash before it reruns every step, and
//! `on_done`/`on_hitl_resolved` may run twice. Every step that must not be
//! lost therefore runs before that transaction. Worktree cleanup runs only
//! after it commits, so an attempt that later gives up to Failed still has
//! its worktree; a cleanup lost to a crash right after the commit is left
//! to log-cleanup, which removes Done and Skipped worktrees.
//!
//! Hook and cleanup failures are non-fatal and recorded (`hook`,
//! `post_processing_error`). A failed result transition, or an on_done that
//! could not start because the store or checkout failed, is retried on the
//! next tick; after [`POST_PROCESSING_FAILURE_LIMIT`] failures
//! `on_hitl_resolved` runs (again, if an attempt already ran it) and the
//! item leaves Hitl for Failed with a `post_processing_failed` record.

use async_trait::async_trait;
use belt_core::error::BeltError;
use belt_core::hitl::{HitlAction, HitlId};
use belt_core::phase::QueuePhase;
use belt_core::queue::QueueItem;
use belt_core::transition::{Actor, TransitionReason, TransitionRequest};
use belt_infra::db::{
    CompleteOutcome, Database, DeriveKind, DeriveOutcome, DeriveRequest, EventRecord, HitlRequest,
    transition_kind,
};

/// Failed result transitions of one request after which post-processing
/// gives up and moves the item to Failed, so it never stays busy forever.
pub const POST_PROCESSING_FAILURE_LIMIT: u32 = 5;

/// Replans one lineage may go through. A replan confirmed for a lineage
/// that already has this many ends the item as Failed.
pub const REPLAN_LIMIT: u32 = 3;

/// What post-processing needs from the daemon that runs it.
#[async_trait]
pub(crate) trait PostProcessingEffects: Send {
    /// Whether this runner post-processes `item` (it knows the item's workspace).
    fn owns(&self, item: &QueueItem) -> bool;

    /// Run the on_done scripts of the item's state. `Err` means the scripts
    /// could not be started (store or checkout failure), not that they failed.
    async fn run_on_done(&mut self, item: &QueueItem) -> anyhow::Result<OnDoneRun>;

    /// Append the attempt history of an item that ended `done` or `failed`.
    fn record_attempt(&mut self, item: &QueueItem, status: &str, error: Option<&str>);

    /// Call the item's `on_hitl_resolved` hook.
    async fn on_hitl_resolved(&self, item: &QueueItem, action: HitlAction) -> anyhow::Result<()>;

    /// Remove the worktree the item works in.
    fn cleanup_worktree(&self, item: &QueueItem) -> Result<(), BeltError>;
}

/// What the on_done scripts did once they ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OnDoneRun {
    /// They succeeded, or none are configured.
    Passed,
    /// A script exited unsuccessfully.
    ScriptFailed,
    /// A script could not be executed.
    ScriptError(String),
}

/// A request whose result transition was committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Applied {
    /// The item left Hitl for `to`.
    Left {
        work_id: String,
        to: QueuePhase,
        /// Text the next run of the item must see (retry instruction).
        lateral_plan: Option<String>,
    },
    /// The item ended Skipped and the work continues in `derived`.
    Derived {
        work_id: String,
        derived: String,
        /// The failure context the derived item plans from.
        lateral_plan: Option<String>,
    },
}

/// Outcome of one attempt at a request.
enum Attempt {
    Applied(Applied),
    /// Someone else finished the request in the meantime.
    NotPending,
}

/// Post-process every confirmed request of an item `effects` owns.
///
/// Returns the requests whose result transition was committed in this run
/// (including those that gave up to Failed). A request whose attempt failed
/// below the limit stays pending for the next run.
///
/// # Errors
/// `BeltError` when the pending requests cannot be listed. Failures of a
/// single request are counted on that request instead.
pub(crate) async fn run<E: PostProcessingEffects>(
    db: &Database,
    effects: &mut E,
) -> Result<Vec<Applied>, BeltError> {
    let mut applied = Vec::new();
    for request in db.pending_post_processing()? {
        let item = match db.get_item(&request.work_id) {
            Ok(item) => item,
            Err(e) => {
                fail_attempt(db, &request, &format!("item unreadable: {e}"));
                continue;
            }
        };
        if !effects.owns(&item) {
            continue;
        }
        let Some(resolution) = request.resolution.as_ref() else {
            fail_attempt(db, &request, "confirmed request carries no resolution");
            continue;
        };
        let action = resolution.action;
        match attempt(db, effects, &request, &item, action).await {
            Ok(Attempt::Applied(done)) => {
                if ends_worktree(&done) {
                    cleanup(db, effects, &item);
                }
                applied.push(done);
            }
            Ok(Attempt::NotPending) => {}
            Err(error) => {
                let Some(failures) = count_failure(db, &request, &error) else {
                    continue;
                };
                // The verdict is final even when its steps keep failing:
                // resolve the HITL (e.g. drop its labels) before leaving it.
                // The hook is idempotent, so a call an attempt already made
                // may repeat.
                resolved(db, effects, &item, action).await;
                if let Some(gave_up) =
                    give_up(db, &request.hitl_id, &item, action, failures, &error)
                {
                    effects.record_attempt(&item, "failed", Some(&error));
                    applied.push(gave_up);
                }
            }
        }
    }
    Ok(applied)
}

/// Run the steps of `action` for `request`. `Err` describes a failed result
/// transition (or a store step it depends on).
async fn attempt<E: PostProcessingEffects>(
    db: &Database,
    effects: &mut E,
    request: &HitlRequest,
    item: &QueueItem,
    action: HitlAction,
) -> Result<Attempt, String> {
    match action {
        HitlAction::Done => {
            // A store or checkout failure is an attempt failure retried on the
            // next tick; only what the scripts themselves did can fail the item.
            let on_done = effects
                .run_on_done(item)
                .await
                .map_err(|e| format!("on_done could not run: {e}"))?;
            resolved(db, effects, item, action).await;
            let (to, detail) = match on_done {
                OnDoneRun::Passed => (QueuePhase::Done, None),
                OnDoneRun::ScriptFailed => (
                    QueuePhase::Failed,
                    Some("on_done script failed".to_string()),
                ),
                OnDoneRun::ScriptError(e) => {
                    (QueuePhase::Failed, Some(format!("on_done error: {e}")))
                }
            };
            let result = complete(db, request, action, to, detail.clone(), None);
            record_result(effects, item, &result, detail.as_deref());
            result
        }
        HitlAction::Retry => {
            db.record_reset(&item.work_id)
                .map_err(|e| format!("failure-count reset: {e}"))?;
            resolved(db, effects, item, action).await;
            complete(
                db,
                request,
                action,
                QueuePhase::Pending,
                None,
                retry_instruction(request),
            )
        }
        HitlAction::Skip => {
            resolved(db, effects, item, action).await;
            complete(db, request, action, QueuePhase::Skipped, None, None)
        }
        HitlAction::Replan if item.replan_count >= REPLAN_LIMIT => {
            resolved(db, effects, item, action).await;
            let detail = format!(
                "replan limit reached: the lineage was replanned {} times (max {REPLAN_LIMIT})",
                item.replan_count
            );
            let result = complete(
                db,
                request,
                action,
                QueuePhase::Failed,
                Some(detail.clone()),
                None,
            );
            record_result(effects, item, &result, Some(&detail));
            result
        }
        HitlAction::Replan => {
            let context = replan_context(db, request, item);
            resolved(db, effects, item, action).await;
            let outcome = db
                .derive(&DeriveRequest {
                    work_id: item.work_id.clone(),
                    expected_from: QueuePhase::Hitl,
                    kind: DeriveKind::Replan,
                    actor: Actor::Daemon,
                    reason: TransitionReason::PostProcessing(action),
                    detail: Some(format!(
                        "replan {} of {REPLAN_LIMIT}",
                        item.replan_count + 1
                    )),
                })
                .map_err(|e| format!("replan derivation: {e}"))?;
            match outcome {
                DeriveOutcome::Derived { work_id } => Ok(Attempt::Applied(Applied::Derived {
                    work_id: item.work_id.clone(),
                    derived: work_id,
                    lateral_plan: Some(context),
                })),
                DeriveOutcome::Rejected(refused) => {
                    Err(format!("replan derivation refused: {refused:?}"))
                }
            }
        }
    }
}

/// Append the attempt history of a committed Done or Failed result, with the
/// same statuses a normal completion records (`done`, `failed`).
fn record_result<E: PostProcessingEffects>(
    effects: &mut E,
    item: &QueueItem,
    result: &Result<Attempt, String>,
    error: Option<&str>,
) {
    if let Ok(Attempt::Applied(Applied::Left { to, .. })) = result {
        match to {
            QueuePhase::Done => effects.record_attempt(item, "done", None),
            QueuePhase::Failed => effects.record_attempt(item, "failed", error),
            _ => {}
        }
    }
}

/// Whether the committed result ends the item's worktree: Done and Skipped
/// (including a replanned origin, whose successor gets a new worktree) do;
/// Pending and Failed keep it for the next run or for investigation.
fn ends_worktree(applied: &Applied) -> bool {
    match applied {
        Applied::Derived { .. } => true,
        Applied::Left { to, .. } => match to {
            QueuePhase::Done | QueuePhase::Skipped => true,
            QueuePhase::Pending
            | QueuePhase::Ready
            | QueuePhase::Running
            | QueuePhase::Completed
            | QueuePhase::Hitl
            | QueuePhase::Failed => false,
        },
    }
}

/// Commit the result transition and the post-processed mark together.
fn complete(
    db: &Database,
    request: &HitlRequest,
    action: HitlAction,
    to: QueuePhase,
    detail: Option<String>,
    lateral_plan: Option<String>,
) -> Result<Attempt, String> {
    let outcome = db
        .complete_post_processing(
            &request.hitl_id,
            &TransitionRequest {
                work_id: request.work_id.clone(),
                expected_from: QueuePhase::Hitl,
                to,
                actor: Actor::Daemon,
                reason: TransitionReason::PostProcessing(action),
                detail,
            },
        )
        .map_err(|e| format!("result transition to {to}: {e}"))?;
    match outcome {
        CompleteOutcome::Completed { .. } => Ok(Attempt::Applied(Applied::Left {
            work_id: request.work_id.clone(),
            to,
            lateral_plan,
        })),
        CompleteOutcome::NotPending => Ok(Attempt::NotPending),
        CompleteOutcome::Rejected(refused) => {
            Err(format!("result transition to {to} refused: {refused:?}"))
        }
    }
}

/// Count a failed attempt of a request whose item or verdict is unknown.
///
/// A give-up transition needs both, so the attempt is only counted.
fn fail_attempt(db: &Database, request: &HitlRequest, error: &str) {
    if let Some(failures) = count_failure(db, request, error) {
        tracing::error!(hitl_id = %request.hitl_id, failures, "post-processing cannot give up without its item and verdict: {error}");
    }
}

/// Count a failed attempt. Returns the failure count once it reached
/// [`POST_PROCESSING_FAILURE_LIMIT`]: the caller then gives up.
fn count_failure(db: &Database, request: &HitlRequest, error: &str) -> Option<u32> {
    let hitl_id = &request.hitl_id;
    let failures = match db.record_post_processing_failure(hitl_id) {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(%hitl_id, "post-processing failure could not be counted: {e} (attempt error: {error})");
            return None;
        }
    };
    if failures < POST_PROCESSING_FAILURE_LIMIT {
        tracing::warn!(
            %hitl_id,
            work_id = %request.work_id,
            failures,
            limit = POST_PROCESSING_FAILURE_LIMIT,
            "post-processing retrying: {error}"
        );
        return None;
    }
    Some(failures)
}

/// Leave Hitl for Failed after repeated failures, and record why.
fn give_up(
    db: &Database,
    hitl_id: &HitlId,
    item: &QueueItem,
    action: HitlAction,
    failures: u32,
    error: &str,
) -> Option<Applied> {
    let detail = format!("post-processing failed {failures} times; last error: {error}");
    let outcome = db.complete_post_processing(
        hitl_id,
        &TransitionRequest {
            work_id: item.work_id.clone(),
            expected_from: QueuePhase::Hitl,
            to: QueuePhase::Failed,
            actor: Actor::Daemon,
            reason: TransitionReason::PostProcessing(action),
            detail: Some(detail.clone()),
        },
    );
    match outcome {
        Ok(CompleteOutcome::Completed { .. }) => {
            record(
                db,
                &item.work_id,
                transition_kind::POST_PROCESSING_FAILED,
                &action.to_string(),
                &detail,
            );
            tracing::error!(%hitl_id, work_id = %item.work_id, "post-processing gave up: {detail}");
            Some(Applied::Left {
                work_id: item.work_id.clone(),
                to: QueuePhase::Failed,
                lateral_plan: None,
            })
        }
        Ok(CompleteOutcome::NotPending) => None,
        Ok(CompleteOutcome::Rejected(refused)) => {
            tracing::error!(%hitl_id, work_id = %item.work_id, "post-processing give-up refused: {refused:?}");
            None
        }
        Err(e) => {
            tracing::error!(%hitl_id, work_id = %item.work_id, "post-processing give-up failed: {e}");
            None
        }
    }
}

/// Call `on_hitl_resolved`; a failure is recorded as a `hook` event.
async fn resolved<E: PostProcessingEffects>(
    db: &Database,
    effects: &E,
    item: &QueueItem,
    action: HitlAction,
) {
    if let Err(e) = effects.on_hitl_resolved(item, action).await {
        tracing::warn!(work_id = %item.work_id, "lifecycle hook on_hitl_resolved error (post-processing proceeds): {e}");
        record(
            db,
            &item.work_id,
            transition_kind::HOOK,
            "on_hitl_resolved",
            &e.to_string(),
        );
    }
}

/// Clean the item's worktree; a failure is a `post_processing_error`.
fn cleanup<E: PostProcessingEffects>(db: &Database, effects: &E, item: &QueueItem) {
    if let Err(e) = effects.cleanup_worktree(item) {
        tracing::warn!(work_id = %item.work_id, "worktree cleanup failed (post-processing proceeds): {e}");
        record(
            db,
            &item.work_id,
            transition_kind::POST_PROCESSING_ERROR,
            "worktree_cleanup",
            &e.to_string(),
        );
    }
}

/// Append a non-phase event; a failure to record it is only logged, since
/// the event reports a problem and must not become a new one.
fn record(db: &Database, work_id: &str, kind: &str, reason: &str, detail: &str) {
    if let Err(e) = db.record_event(&EventRecord {
        work_id,
        kind,
        actor: Actor::Daemon,
        reason: Some(reason),
        detail: Some(detail),
    }) {
        tracing::error!(
            work_id,
            kind,
            "event could not be recorded: {e} (event: {reason}: {detail})"
        );
    }
}

/// The respondent's instruction, injected into the retried item's prompt.
fn retry_instruction(request: &HitlRequest) -> Option<String> {
    let notes = request.resolution_notes.as_deref()?.trim();
    (!notes.is_empty()).then(|| format!("\n\n## Human Instruction (HITL retry)\n{notes}"))
}

/// What the replanned item must know: why its origin was stopped, the
/// respondent's instruction and the earlier failures of the same work.
///
/// An unreadable failure history leaves that section out and is recorded
/// as a `post_processing_error`; the replan itself proceeds.
fn replan_context(db: &Database, request: &HitlRequest, item: &QueueItem) -> String {
    let mut context = format!(
        "\n\n## Replan Context\n\
         The previous attempt ({}) was stopped for a new plan (replan {} of {REPLAN_LIMIT}). \
         Plan the work again from the start.\n",
        item.work_id,
        item.replan_count + 1
    );
    if let Some(notes) = request.notes.as_deref().filter(|n| !n.trim().is_empty()) {
        context.push_str(&format!("\n### HITL notes\n{notes}\n"));
    }
    if let Some(notes) = request
        .resolution_notes
        .as_deref()
        .filter(|n| !n.trim().is_empty())
    {
        context.push_str(&format!("\n### Instruction\n{notes}\n"));
    }
    match db.get_history(&item.source_id) {
        Ok(history) => {
            let failures: Vec<String> = history
                .into_iter()
                .filter(|h| h.state == item.state && h.status == "failed")
                .filter_map(|h| h.error)
                .collect();
            if !failures.is_empty() {
                context.push_str("\n### Previous failures\n");
                for error in failures {
                    context.push_str(&format!("- {error}\n"));
                }
            }
        }
        Err(e) => record(
            db,
            &item.work_id,
            transition_kind::POST_PROCESSING_ERROR,
            "replan_context",
            &e.to_string(),
        ),
    }
    context
}
