//! HITL service — the single contract for opening, answering and expiring
//! HITL requests.
//!
//! CLI, TUI, cron, channels and the daemon go through this service; none of
//! them touches the request store directly. The service decides nothing about
//! phases beyond opening: a confirmed request holds its item in Hitl until the
//! daemon's post-processing applies the result transition.

use std::sync::Arc;

use belt_core::error::BeltError;
use belt_core::escalation::EscalationAction;
use belt_core::hitl::{ConfirmPath, HitlAction, HitlId, HitlResolution, RespondOutcome};
use belt_infra::db::{Database, HitlRequest, HitlTarget, OpenHitlOutcome, OpenHitlRequest};
use chrono::{DateTime, Duration, Utc};

/// Upper bound of a timeout (100 years) that keeps the date arithmetic in range.
const MAX_TIMEOUT_HOURS: u64 = 24 * 365 * 100;

/// Expiry terms a newly opened request carries: when it times out and which
/// terminal action (`skip` or `replan`) applies then.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HitlExpiry {
    /// RFC 3339.
    pub timeout_at: Option<String>,
    pub terminal_action: Option<EscalationAction>,
}

impl HitlExpiry {
    /// Expire `hours` after `now`.
    pub fn after_hours(
        hours: u64,
        terminal_action: Option<EscalationAction>,
        now: DateTime<Utc>,
    ) -> Self {
        let hours = i64::try_from(hours.min(MAX_TIMEOUT_HOURS)).unwrap_or(0);
        Self {
            timeout_at: Some((now + Duration::hours(hours)).to_rfc3339()),
            terminal_action,
        }
    }
}

/// A human (or channel) response to a HITL request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitlResponse {
    pub target: HitlTarget,
    pub action: HitlAction,
    /// Respondent.
    pub by: String,
    /// Path the response arrived through (`cli`, `tui`, a channel name).
    pub via: String,
    pub path: ConfirmPath,
    pub notes: Option<String>,
}

/// Opens, answers and expires HITL requests on top of the store.
///
/// First response wins: of any number of responses and expiries racing for
/// one request, the store confirms exactly one and returns the winner to
/// the others as `AlreadyHandled`.
#[derive(Clone)]
pub struct HitlService {
    db: Arc<Database>,
}

impl HitlService {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// The store this service works on.
    pub fn database(&self) -> &Database {
        &self.db
    }

    /// Enter Hitl and open a request for `req.work_id`, in one transaction.
    ///
    /// # Errors
    /// `BeltError` when the store fails or `req.work_id` is unknown. A refused
    /// transition is a value: [`OpenHitlOutcome::Rejected`].
    pub fn open(&self, req: &OpenHitlRequest) -> Result<OpenHitlOutcome, BeltError> {
        self.db.open_hitl(req)
    }

    /// Confirm a response; the first one wins.
    ///
    /// A loser gets [`RespondOutcome::AlreadyHandled`] with the winning
    /// response, and the refusal is recorded as `hitl_response_rejected`. The
    /// item stays in Hitl either way.
    ///
    /// # Errors
    /// `BeltError` when the store fails.
    pub fn respond(&self, response: &HitlResponse) -> Result<RespondOutcome, BeltError> {
        let resolution = HitlResolution {
            action: response.action,
            by: response.by.clone(),
            via: response.via.clone(),
            at: Utc::now().to_rfc3339(),
            path: response.path,
        };
        self.db
            .resolve_hitl(&response.target, &resolution, response.notes.as_deref())
    }

    /// Put an expiry in the race against responses, applying `terminal`
    /// (`skip` or `replan`; anything else is `InvalidAction`).
    ///
    /// # Errors
    /// `BeltError` when the store fails.
    pub fn expire(
        &self,
        hitl_id: &HitlId,
        terminal: EscalationAction,
    ) -> Result<RespondOutcome, BeltError> {
        self.db.expire_hitl(hitl_id, terminal)
    }

    /// Record that a response of `by` through `via` was refused as
    /// unauthorized (for example, a channel respondent who may not answer).
    /// The request stays open. Returns `false` when `target` names no request.
    ///
    /// # Errors
    /// `BeltError` when the store fails.
    pub fn reject_unauthorized(
        &self,
        target: &HitlTarget,
        by: &str,
        via: &str,
    ) -> Result<bool, BeltError> {
        self.db.record_unauthorized_response(target, by, via)
    }

    /// Every open request, oldest first.
    ///
    /// # Errors
    /// `BeltError` when the store fails.
    pub fn open_requests(&self) -> Result<Vec<HitlRequest>, BeltError> {
        self.db.open_hitl_requests()
    }

    /// Open requests whose `timeout_at` is at or before `now`. Requests
    /// without a deadline never come due.
    ///
    /// # Errors
    /// `BeltError` when the store fails or a stored deadline is not RFC 3339.
    pub fn due_for_expiry(&self, now: DateTime<Utc>) -> Result<Vec<HitlRequest>, BeltError> {
        let mut due = Vec::new();
        for request in self.open_requests()? {
            let Some(timeout_at) = request.timeout_at.as_deref() else {
                continue;
            };
            let deadline = DateTime::parse_from_rfc3339(timeout_at).map_err(|e| {
                BeltError::Database(format!(
                    "HITL request {} has an invalid timeout_at {timeout_at}: {e}",
                    request.hitl_id
                ))
            })?;
            if deadline.with_timezone(&Utc) <= now {
                due.push(request);
            }
        }
        Ok(due)
    }

    /// Hand out every open request whose `on_hitl_opened` has not run yet,
    /// each exactly once across all callers and restarts.
    ///
    /// # Errors
    /// `BeltError` when the store fails.
    pub fn claim_opened(&self) -> Result<Vec<HitlRequest>, BeltError> {
        self.db.claim_opened_hooks()
    }
}
