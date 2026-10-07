//! Notifier — the daemon's side of `NotificationChannel`: HITL request
//! delivery, response polling and progress notifications (tick steps 4–6).
//!
//! The notifier owns the sending and receiving policy; channels only carry
//! messages. Every send, delivery and response ends in a classified value the
//! caller can show; only store failures are errors.
//!
//! - Progress notifications are best-effort: a failed send is recorded as
//!   `notification_failed` and never retried. The cursor lives in memory and
//!   starts at the log head, so transitions made while the daemon was down are
//!   never announced.
//! - HITL request delivery is tracked per channel and retried every round
//!   until the store's attempt cap marks it `failed`.
//! - Responses are processed once per `(channel, external_id)`, correlated to
//!   a request, checked against the channel's `respond.allow`, and settled
//!   through [`HitlService`]. Natural language goes through an LLM proposal
//!   that only a confirmation turns into a response.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use belt_core::error::BeltError;
use belt_core::escalation::EscalationAction;
use belt_core::hitl::{
    ConfirmPath, HitlAction, HitlId, HitlResolution, HitlStatus, RespondOutcome,
};
use belt_core::notification::{
    ChannelEvent, HitlRef, InboundBody, InboundResponse, MessageKind, MessageRef,
    NotificationChannel, NotificationsConfig, NotifyOutcome, ORIGIN_CHANNEL, OutboundMessage,
    PollTarget, route,
};
use belt_core::runtime::{AgentRuntime, RuntimeRequest, StructuredOutputConfig};
use belt_core::transition::Actor;
use belt_infra::db::{
    ConfirmProposalOutcome, Database, DeliveryAttempt, DeliveryStatus, EventRecord,
    ExternalResponseOutcome, HitlRequest, HitlTarget, NewProposal, ProposalStatus,
    TransitionLogEntry, UpsertProposalOutcome, transition_kind,
};
use chrono::{Duration, Utc};

use crate::hitl::{HitlResponse, HitlService};

/// How long a confirmed request keeps being polled, so a response that
/// arrives late (or while the daemon was down) is still answered
/// `already_handled` instead of going unanswered.
pub const LATE_RESPONSE_WINDOW_HOURS: i64 = 24;

/// Explicit response forms a respondent can use, shown in requests and replies.
const RESPONSE_HELP: &str = "/belt <done|retry|skip|replan>";
const CONFIRM_HELP: &str = "/belt confirm";

/// `reason` of a `notification_failed` event for a reply.
const REPLY_LABEL: &str = "reply";

// ---- result values ---------------------------------------------------------

/// Result of one send to one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelSend {
    Sent,
    /// The send failed; recorded as `notification_failed`.
    Failed {
        error: String,
    },
    /// The route names a channel without an implementation (dashboard only).
    NoImplementation,
    /// The channel has no address for the item (e.g. an item of another
    /// source). Not a failure: nothing is recorded.
    NoAddress,
}

/// One progress notification attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressNotice {
    /// `transition_log.seq` of the transition.
    pub seq: u64,
    pub work_id: String,
    pub event: ChannelEvent,
    pub channel: String,
    pub result: ChannelSend,
}

/// Where one HITL request delivery stands after an attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryResult {
    Sent,
    /// Failed; tried again next round.
    Retrying {
        attempts: u32,
    },
    /// Failed for the last time; the delivery is `failed` from now on.
    GaveUp {
        attempts: u32,
    },
    /// The channel has no address for the request's item. Not a failure:
    /// nothing is recorded and the delivery stays `pending`.
    NoAddress,
}

/// One HITL request delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryReport {
    pub hitl_id: HitlId,
    pub channel: String,
    pub result: DeliveryResult,
}

/// What happened to one external response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseOutcome {
    /// This `(channel, external_id)` was processed before; skipped.
    Duplicate,
    /// No request matches the response's correlation hint.
    NotFound,
    /// The respondent is not in the channel's allowlist. Recorded, no reply.
    Unauthorized { hitl_id: HitlId },
    /// The response confirmed the request.
    Won {
        hitl_id: HitlId,
        action: HitlAction,
        path: ConfirmPath,
    },
    /// Another response confirmed the request first.
    AlreadyHandled {
        hitl_id: HitlId,
        resolution: HitlResolution,
    },
    /// The action is not allowed for this request.
    InvalidAction { hitl_id: HitlId },
    /// Natural language was turned into a proposal awaiting confirmation.
    Proposed {
        hitl_id: HitlId,
        proposal_id: i64,
        action: HitlAction,
    },
    /// Natural language could not be turned into an action; no proposal.
    NotInterpreted { hitl_id: HitlId, reason: String },
    /// A confirmation found no pending proposal of the respondent.
    NoPendingProposal { hitl_id: HitlId },
}

/// One external response and the reply sent for it, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseReport {
    pub external_id: String,
    pub respondent: String,
    pub outcome: ResponseOutcome,
    /// `None` when the outcome takes no reply.
    pub reply: Option<ChannelSend>,
}

/// Result of polling one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollResult {
    Polled(Vec<ResponseReport>),
    /// The channel could not be read; nothing was processed and the next
    /// round polls again.
    Failed {
        error: String,
    },
}

/// One channel's polling round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollReport {
    pub channel: String,
    pub result: PollResult,
}

// ---- natural-language interpretation ---------------------------------------

/// What the LLM made of a natural-language response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interpretation {
    Proposal {
        action: HitlAction,
        summary: Option<String>,
    },
    /// The model answered but named no allowed action.
    NoAction { reason: String },
    /// The model could not be run or its output could not be read.
    Failed { error: String },
}

/// Turns a natural-language response into a proposed action with the
/// workspace's default AgentRuntime.
pub struct NlInterpreter {
    runtime: Arc<dyn AgentRuntime>,
    working_dir: PathBuf,
}

impl NlInterpreter {
    /// `working_dir` is where the runtime process runs; the interpretation
    /// reads no files.
    pub fn new(runtime: Arc<dyn AgentRuntime>, working_dir: PathBuf) -> Self {
        Self {
            runtime,
            working_dir,
        }
    }

    /// Ask for one of the allowed actions for `request` given `text`.
    pub async fn interpret(&self, request: &HitlRequest, text: &str) -> Interpretation {
        let response = self
            .runtime
            .invoke(RuntimeRequest {
                working_dir: self.working_dir.clone(),
                prompt: interpretation_prompt(request, text),
                model: None,
                system_prompt: None,
                session_id: None,
                structured_output: Some(StructuredOutputConfig {
                    schema: interpretation_schema(),
                    name: Some("hitl_action_proposal".to_string()),
                }),
            })
            .await;
        if !response.success() {
            return Interpretation::Failed {
                error: format!(
                    "runtime exited with {}: {}",
                    response.exit_code,
                    response.stderr.trim()
                ),
            };
        }
        parse_interpretation(&response.stdout)
    }
}

fn interpretation_prompt(request: &HitlRequest, text: &str) -> String {
    let reason = request
        .reason
        .map(|r| r.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let notes = request.notes.as_deref().unwrap_or("(none)");
    format!(
        "A work item `{work_id}` is waiting for a human decision.\n\
         Reason: {reason}\nNotes: {notes}\n\n\
         The allowed actions are:\n\
         - done: accept the current result as complete\n\
         - retry: run the same step again\n\
         - skip: stop working on this item\n\
         - replan: start over with a new plan\n\n\
         A person answered:\n\"\"\"\n{text}\n\"\"\"\n\n\
         Reply with only a JSON object {{\"action\": \"done|retry|skip|replan|none\", \"summary\": \"<one sentence>\"}}. \
         Use \"none\" when the answer does not clearly ask for one of the allowed actions.",
        work_id = request.work_id,
    )
}

fn interpretation_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": ["done", "retry", "skip", "replan", "none"]},
            "summary": {"type": "string"}
        },
        "required": ["action"]
    })
}

fn parse_interpretation(stdout: &str) -> Interpretation {
    let body = strip_code_fence(stdout.trim());
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(e) => {
            return Interpretation::Failed {
                error: format!("output is not the requested JSON object: {e}"),
            };
        }
    };
    let summary = value
        .get("summary")
        .and_then(|s| s.as_str())
        .map(str::to_string);
    let Some(action) = value.get("action").and_then(|a| a.as_str()) else {
        return Interpretation::Failed {
            error: "output has no string `action`".to_string(),
        };
    };
    if action == "none" {
        return Interpretation::NoAction {
            reason: summary.unwrap_or_else(|| "no action was asked for".to_string()),
        };
    }
    match action.parse::<HitlAction>() {
        Ok(action) => Interpretation::Proposal { action, summary },
        Err(e) => Interpretation::NoAction { reason: e },
    }
}

/// The body of a fenced ```` ```json ```` block, or `text` itself.
fn strip_code_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

// ---- progress events -------------------------------------------------------

/// The channel event each phase transition of `batch` produces, in order.
///
/// A Skipped that a derived item continues (its `item_created` row, written in
/// the same transaction, is in the same batch) is not `skipped`; it is
/// `failed` only for `retry_with_comment`, the escalation that runs `on_fail`.
/// Entering Hitl by an escalation that runs `on_fail` is `failed`.
fn progress_events(batch: &[TransitionLogEntry]) -> Vec<(&TransitionLogEntry, ChannelEvent)> {
    let derived_origins: HashSet<&str> = batch
        .iter()
        .filter(|e| {
            e.kind == transition_kind::ITEM_CREATED && e.reason.as_deref() == Some("derived")
        })
        .filter_map(|e| e.detail.as_deref())
        .collect();
    let commented_retry = format!("escalation: {}", EscalationAction::RetryWithComment);
    batch
        .iter()
        .filter(|e| e.kind == transition_kind::PHASE_ENTER)
        .filter_map(|e| {
            let event = match e.to_phase.as_deref()? {
                "running" => ChannelEvent::Started,
                "done" => ChannelEvent::Done,
                "failed" => ChannelEvent::Failed,
                "skipped" if !derived_origins.contains(e.work_id.as_str()) => ChannelEvent::Skipped,
                "skipped" if e.detail.as_deref() == Some(commented_retry.as_str()) => {
                    ChannelEvent::Failed
                }
                "hitl" if escalation_runs_on_fail(e.reason.as_deref()) => ChannelEvent::Failed,
                _ => return None,
            };
            Some((e, event))
        })
        .collect()
}

fn escalation_runs_on_fail(reason: Option<&str>) -> bool {
    reason
        .and_then(|r| r.strip_prefix("escalation:"))
        .and_then(|a| a.parse::<EscalationAction>().ok())
        .is_some_and(|a| a.should_run_on_fail())
}

fn event_label(event: ChannelEvent) -> &'static str {
    match event {
        ChannelEvent::Started => "started",
        ChannelEvent::Done => "done",
        ChannelEvent::Failed => "failed",
        ChannelEvent::Skipped => "skipped",
        ChannelEvent::HitlRequested => "hitl_requested",
    }
}

// ---- message texts ---------------------------------------------------------

fn progress_text(work_id: &str, event: ChannelEvent) -> String {
    format!("belt: `{work_id}` {}", event_label(event))
}

fn hitl_request_text(request: &HitlRequest) -> String {
    let reason = request
        .reason
        .map(|r| r.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let mut text = format!(
        "belt: `{}` needs a human decision ({reason}).",
        request.work_id
    );
    if let Some(notes) = request.notes.as_deref() {
        text.push_str(&format!("\n\n{notes}"));
    }
    text.push_str(&format!(
        "\n\nReply `{RESPONSE_HELP} {}`, or describe what to do in your own words.",
        request.hitl_id
    ));
    text
}

fn already_handled_text(resolution: &HitlResolution) -> String {
    format!(
        "Already handled: `{}` by {} via {} at {}.",
        resolution.action, resolution.by, resolution.via, resolution.at
    )
}

fn invalid_action_text(hitl_id: &HitlId) -> String {
    format!("That action is not allowed here. Reply `{RESPONSE_HELP} {hitl_id}`.")
}

fn proposal_text(hitl_id: &HitlId, action: HitlAction, summary: Option<&str>) -> String {
    let because = summary.map(|s| format!(" ({s})")).unwrap_or_default();
    format!(
        "Proposed action: `{action}`{because}. Reply `{CONFIRM_HELP} {hitl_id}` to apply it, \
         or `{RESPONSE_HELP} {hitl_id}` to choose another."
    )
}

fn not_interpreted_text(hitl_id: &HitlId, reason: &str) -> String {
    format!("Could not turn this into an action ({reason}). Reply `{RESPONSE_HELP} {hitl_id}`.")
}

fn no_proposal_text(hitl_id: &HitlId) -> String {
    format!(
        "There is no pending proposal of yours to confirm. Reply `{RESPONSE_HELP} {hitl_id}`, \
         or describe what to do in your own words."
    )
}

// ---- notifier --------------------------------------------------------------

/// Sends progress notifications and HITL requests and processes responses.
///
/// The daemon calls, each tick: [`Notifier::register_deliveries`] for every
/// newly opened request, [`Notifier::deliver_due`], [`Notifier::poll_responses`],
/// then [`Notifier::notify_progress`].
pub struct Notifier {
    db: Arc<Database>,
    hitl: HitlService,
    config: NotificationsConfig,
    channels: HashMap<String, Arc<dyn NotificationChannel>>,
    interpreter: NlInterpreter,
    /// Last `transition_log.seq` handled by [`Notifier::notify_progress`].
    cursor: u64,
}

impl Notifier {
    /// Build a notifier over `channels`, starting the progress cursor at the
    /// current log head (nothing earlier is announced).
    ///
    /// # Errors
    /// Two channels share a name, a non-origin channel has no entry in
    /// `config.channels`, or the store fails.
    pub fn new(
        db: Arc<Database>,
        config: NotificationsConfig,
        channels: Vec<Arc<dyn NotificationChannel>>,
        interpreter: NlInterpreter,
    ) -> anyhow::Result<Self> {
        let mut by_name = HashMap::new();
        for channel in channels {
            let name = channel.name().to_string();
            if name != ORIGIN_CHANNEL && !config.channels.iter().any(|c| c.name == name) {
                anyhow::bail!(
                    "channel implementation `{name}` has no notifications.channels entry"
                );
            }
            if by_name.insert(name.clone(), channel).is_some() {
                anyhow::bail!("two channel implementations are named `{name}`");
            }
        }
        let cursor = db.latest_transition_seq()?;
        Ok(Self {
            hitl: HitlService::new(db.clone()),
            db,
            config,
            channels: by_name,
            interpreter,
            cursor,
        })
    }

    /// Register the delivery of a newly opened request on every channel the
    /// `hitl_requested` route names and that has an implementation; returns
    /// those channels. A routed name without an implementation is logged and
    /// skipped (dashboard only). Idempotent.
    ///
    /// # Errors
    /// `BeltError` when the store fails.
    pub fn register_deliveries(&self, hitl_id: &HitlId) -> Result<Vec<String>, BeltError> {
        let mut registered = Vec::new();
        for name in route(ChannelEvent::HitlRequested, &self.config) {
            if !self.channels.contains_key(&name) {
                tracing::warn!(
                    channel = %name,
                    %hitl_id,
                    "no implementation for notification channel; HITL request shown on the dashboard only"
                );
                continue;
            }
            self.db.ensure_delivery(hitl_id, &name)?;
            registered.push(name);
        }
        Ok(registered)
    }

    /// Try every pending delivery of an open request once (tick step 4).
    ///
    /// # Errors
    /// `BeltError` when the store fails.
    pub async fn deliver_due(&self) -> Result<Vec<DeliveryReport>, BeltError> {
        let mut reports = Vec::new();
        for delivery in self.db.deliveries_due()? {
            let Some(request) = self.db.hitl_request(&delivery.hitl_id)? else {
                return Err(BeltError::Database(format!(
                    "delivery of unknown HITL request {}",
                    delivery.hitl_id
                )));
            };
            let attempt = match self.channels.get(&delivery.channel) {
                None => DeliveryAttempt::Failed {
                    error: format!("no implementation for channel `{}`", delivery.channel),
                },
                Some(channel) => {
                    let message = OutboundMessage {
                        kind: MessageKind::Event(ChannelEvent::HitlRequested),
                        work_id: request.work_id.clone(),
                        hitl_id: Some(request.hitl_id.clone()),
                        text: hitl_request_text(&request),
                    };
                    match channel.notify(&message).await {
                        Ok(NotifyOutcome::Sent(message_ref)) => DeliveryAttempt::Sent {
                            message_ref: message_ref.map(|m| m.0),
                        },
                        Ok(NotifyOutcome::NoAddress) => {
                            reports.push(DeliveryReport {
                                hitl_id: delivery.hitl_id,
                                channel: delivery.channel,
                                result: DeliveryResult::NoAddress,
                            });
                            continue;
                        }
                        Err(e) => DeliveryAttempt::Failed {
                            error: format!("{e:#}"),
                        },
                    }
                }
            };
            if let DeliveryAttempt::Failed { error } = &attempt {
                self.record_failure(
                    &request.work_id,
                    event_label(ChannelEvent::HitlRequested),
                    &delivery.channel,
                    error,
                )?;
            }
            let stored = self
                .db
                .mark_delivery(&delivery.hitl_id, &delivery.channel, &attempt)?
                .ok_or_else(|| {
                    BeltError::Database(format!(
                        "delivery {} on {} vanished",
                        delivery.hitl_id, delivery.channel
                    ))
                })?;
            let result = match stored.status {
                DeliveryStatus::Sent => DeliveryResult::Sent,
                DeliveryStatus::Pending => DeliveryResult::Retrying {
                    attempts: stored.attempts,
                },
                DeliveryStatus::Failed => DeliveryResult::GaveUp {
                    attempts: stored.attempts,
                },
            };
            reports.push(DeliveryReport {
                hitl_id: delivery.hitl_id,
                channel: delivery.channel,
                result,
            });
        }
        Ok(reports)
    }

    /// Poll every channel that receives responses and process what it
    /// returns (tick step 5). A channel whose `respond.allow` is empty is not
    /// polled.
    ///
    /// # Errors
    /// `BeltError` when the store fails.
    pub async fn poll_responses(&self) -> Result<Vec<PollReport>, BeltError> {
        let cutoff = (Utc::now() - Duration::hours(LATE_RESPONSE_WINDOW_HOURS)).to_rfc3339();
        let requests = self.db.hitl_requests_to_poll(&cutoff)?;
        let mut reports = Vec::new();
        for (name, channel) in self.channels_in_config_order() {
            let Some(inbox) = channel.inbox() else {
                continue;
            };
            if self.allowlist(name).is_empty() {
                continue;
            }
            let targets = self.poll_targets(name, &requests)?;
            if targets.is_empty() {
                continue;
            }
            let result = match inbox.poll(&targets).await {
                Err(e) => {
                    let error = format!("{e:#}");
                    tracing::warn!(channel = %name, %error, "polling responses failed; retrying next tick");
                    PollResult::Failed { error }
                }
                Ok(responses) => {
                    let mut processed = Vec::new();
                    for response in responses {
                        processed.push(
                            self.process_response(name, channel.as_ref(), &targets, response)
                                .await?,
                        );
                    }
                    PollResult::Polled(processed)
                }
            };
            reports.push(PollReport {
                channel: name.to_string(),
                result,
            });
        }
        Ok(reports)
    }

    /// Announce the transitions since the last call (tick step 6).
    ///
    /// # Errors
    /// `BeltError` when the store fails.
    pub async fn notify_progress(&mut self) -> Result<Vec<ProgressNotice>, BeltError> {
        let batch = self.db.transitions_since(self.cursor)?;
        let mut notices = Vec::new();
        for (entry, event) in progress_events(&batch) {
            for name in route(event, &self.config) {
                let result = match self.channels.get(&name) {
                    None => {
                        tracing::warn!(
                            channel = %name,
                            work_id = %entry.work_id,
                            "no implementation for notification channel; progress shown on the dashboard only"
                        );
                        ChannelSend::NoImplementation
                    }
                    Some(channel) => {
                        let message = OutboundMessage {
                            kind: MessageKind::Event(event),
                            work_id: entry.work_id.clone(),
                            hitl_id: None,
                            text: progress_text(&entry.work_id, event),
                        };
                        send(channel.as_ref(), &message).await
                    }
                };
                if let ChannelSend::Failed { error } = &result {
                    self.record_failure(&entry.work_id, event_label(event), &name, error)?;
                }
                notices.push(ProgressNotice {
                    seq: entry.seq,
                    work_id: entry.work_id.clone(),
                    event,
                    channel: name,
                    result,
                });
            }
        }
        if let Some(last) = batch.last() {
            self.cursor = last.seq;
        }
        Ok(notices)
    }

    // ---- responses ---------------------------------------------------------

    async fn process_response(
        &self,
        channel_name: &str,
        channel: &dyn NotificationChannel,
        targets: &[PollTarget],
        response: InboundResponse,
    ) -> Result<ResponseReport, BeltError> {
        let request = self.correlate(response.hitl_ref.as_ref(), targets)?;
        let recorded = self.db.record_external_response(
            channel_name,
            &response.external_id,
            request.as_ref().map(|r| &r.hitl_id),
            Some(&response.respondent),
        )?;
        let (outcome, reply_text) = match (recorded, request) {
            (ExternalResponseOutcome::Duplicate, _) => (ResponseOutcome::Duplicate, None),
            (ExternalResponseOutcome::Recorded, None) => {
                tracing::warn!(
                    channel = %channel_name,
                    external_id = %response.external_id,
                    "external response matches no HITL request (not_found)"
                );
                (ResponseOutcome::NotFound, None)
            }
            (ExternalResponseOutcome::Recorded, Some(request)) => {
                if self
                    .allowlist(channel_name)
                    .iter()
                    .any(|allowed| allowed == &response.respondent)
                {
                    self.settle(channel_name, &request, &response).await?
                } else {
                    self.hitl.reject_unauthorized(
                        &HitlTarget::Id(request.hitl_id.clone()),
                        &response.respondent,
                        channel_name,
                    )?;
                    (
                        ResponseOutcome::Unauthorized {
                            hitl_id: request.hitl_id,
                        },
                        None,
                    )
                }
            }
        };
        let reply = match reply_text {
            None => None,
            Some((request, text)) => Some(self.reply(channel_name, channel, &request, text).await?),
        };
        Ok(ResponseReport {
            external_id: response.external_id,
            respondent: response.respondent,
            outcome,
            reply,
        })
    }

    /// The request a response addresses, or `None` (`not_found`).
    fn correlate(
        &self,
        hint: Option<&HitlRef>,
        targets: &[PollTarget],
    ) -> Result<Option<HitlRequest>, BeltError> {
        match hint {
            None => Ok(None),
            Some(HitlRef::Token(hitl_id)) => self.db.hitl_request(hitl_id),
            Some(HitlRef::ReplyTo(message_ref)) => {
                match targets
                    .iter()
                    .find(|t| t.message_ref.as_ref() == Some(message_ref))
                {
                    None => Ok(None),
                    Some(target) => self.db.hitl_request(&target.hitl_id),
                }
            }
        }
    }

    /// Apply an authorized response. Returns the outcome and the reply to
    /// send, if any.
    async fn settle(
        &self,
        channel_name: &str,
        request: &HitlRequest,
        response: &InboundResponse,
    ) -> Result<(ResponseOutcome, Option<(HitlRequest, String)>), BeltError> {
        let hitl_id = request.hitl_id.clone();
        match &response.body {
            InboundBody::Action(action) => self.respond(
                channel_name,
                request,
                &response.respondent,
                *action,
                ConfirmPath::Direct,
            ),
            InboundBody::Text(text) => {
                if request.status != HitlStatus::Open {
                    return already_handled(request);
                }
                match self.interpreter.interpret(request, text).await {
                    Interpretation::Proposal { action, summary } => {
                        let proposal = NewProposal {
                            hitl_id: hitl_id.clone(),
                            respondent: response.respondent.clone(),
                            channel: channel_name.to_string(),
                            action,
                            summary: summary.clone(),
                        };
                        match self.db.upsert_proposal(&proposal)? {
                            UpsertProposalOutcome::Recorded(stored) => Ok((
                                ResponseOutcome::Proposed {
                                    hitl_id: hitl_id.clone(),
                                    proposal_id: stored.id,
                                    action,
                                },
                                Some((
                                    request.clone(),
                                    proposal_text(&hitl_id, action, summary.as_deref()),
                                )),
                            )),
                            UpsertProposalOutcome::HitlConfirmed(confirmed) => {
                                already_handled(&confirmed)
                            }
                            UpsertProposalOutcome::NotFound => {
                                Ok((ResponseOutcome::NotFound, None))
                            }
                        }
                    }
                    Interpretation::NoAction { reason }
                    | Interpretation::Failed { error: reason } => {
                        tracing::warn!(%hitl_id, %reason, "natural-language response not interpreted");
                        let text = not_interpreted_text(&hitl_id, &reason);
                        Ok((
                            ResponseOutcome::NotInterpreted {
                                hitl_id: hitl_id.clone(),
                                reason,
                            },
                            Some((request.clone(), text)),
                        ))
                    }
                }
            }
            InboundBody::Confirm => {
                let latest = self.db.latest_proposal(&hitl_id, &response.respondent)?;
                let pending = latest.filter(|p| p.status == ProposalStatus::Pending);
                let Some(proposal) = pending else {
                    if request.status != HitlStatus::Open {
                        return already_handled(request);
                    }
                    return Ok((
                        ResponseOutcome::NoPendingProposal {
                            hitl_id: hitl_id.clone(),
                        },
                        Some((request.clone(), no_proposal_text(&hitl_id))),
                    ));
                };
                match self.db.confirm_proposal(proposal.id)? {
                    ConfirmProposalOutcome::Confirmed(confirmed) => self.respond(
                        channel_name,
                        request,
                        &response.respondent,
                        confirmed.action,
                        ConfirmPath::NaturalLanguage,
                    ),
                    ConfirmProposalOutcome::NotPending(_) => {
                        // Closed between the read and the confirmation: the
                        // request was confirmed by someone else.
                        match self.db.hitl_request(&hitl_id)? {
                            Some(current) if current.status != HitlStatus::Open => {
                                already_handled(&current)
                            }
                            _ => Ok((
                                ResponseOutcome::NoPendingProposal {
                                    hitl_id: hitl_id.clone(),
                                },
                                Some((request.clone(), no_proposal_text(&hitl_id))),
                            )),
                        }
                    }
                }
            }
        }
    }

    fn respond(
        &self,
        channel_name: &str,
        request: &HitlRequest,
        respondent: &str,
        action: HitlAction,
        path: ConfirmPath,
    ) -> Result<(ResponseOutcome, Option<(HitlRequest, String)>), BeltError> {
        let hitl_id = request.hitl_id.clone();
        let outcome = self.hitl.respond(&HitlResponse {
            target: HitlTarget::Id(hitl_id.clone()),
            action,
            by: respondent.to_string(),
            via: channel_name.to_string(),
            path,
            notes: None,
        })?;
        Ok(match outcome {
            RespondOutcome::Won { hitl_id } => (
                ResponseOutcome::Won {
                    hitl_id,
                    action,
                    path,
                },
                None,
            ),
            RespondOutcome::AlreadyHandled(resolution) => {
                let text = already_handled_text(&resolution);
                (
                    ResponseOutcome::AlreadyHandled {
                        hitl_id,
                        resolution,
                    },
                    Some((request.clone(), text)),
                )
            }
            RespondOutcome::NotFound => (ResponseOutcome::NotFound, None),
            RespondOutcome::InvalidAction => (
                ResponseOutcome::InvalidAction {
                    hitl_id: hitl_id.clone(),
                },
                Some((request.clone(), invalid_action_text(&hitl_id))),
            ),
            RespondOutcome::Unauthorized => (ResponseOutcome::Unauthorized { hitl_id }, None),
        })
    }

    /// Reply on the channel the response came from, regardless of its event
    /// filter. A failed reply is recorded and changes no decision.
    async fn reply(
        &self,
        channel_name: &str,
        channel: &dyn NotificationChannel,
        request: &HitlRequest,
        text: String,
    ) -> Result<ChannelSend, BeltError> {
        let message = OutboundMessage {
            kind: MessageKind::Reply,
            work_id: request.work_id.clone(),
            hitl_id: Some(request.hitl_id.clone()),
            text,
        };
        let result = send(channel, &message).await;
        if let ChannelSend::Failed { error } = &result {
            self.record_failure(&request.work_id, REPLY_LABEL, channel_name, error)?;
        }
        Ok(result)
    }

    // ---- helpers -----------------------------------------------------------

    /// Channels in configuration order: origin first, then `channels`.
    fn channels_in_config_order(&self) -> Vec<(&str, &Arc<dyn NotificationChannel>)> {
        std::iter::once(ORIGIN_CHANNEL)
            .chain(self.config.channels.iter().map(|c| c.name.as_str()))
            .filter_map(|name| self.channels.get(name).map(|c| (name, c)))
            .collect()
    }

    fn allowlist(&self, channel_name: &str) -> &[String] {
        if channel_name == ORIGIN_CHANNEL {
            return &self.config.origin.respond.allow;
        }
        self.config
            .channels
            .iter()
            .find(|c| c.name == channel_name)
            .map(|c| c.respond.allow.as_slice())
            .unwrap_or(&[])
    }

    fn poll_targets(
        &self,
        channel_name: &str,
        requests: &[HitlRequest],
    ) -> Result<Vec<PollTarget>, BeltError> {
        let mut targets = Vec::with_capacity(requests.len());
        for request in requests {
            let message_ref = self
                .db
                .delivery(&request.hitl_id, channel_name)?
                .and_then(|d| d.message_ref)
                .map(MessageRef);
            targets.push(PollTarget {
                hitl_id: request.hitl_id.clone(),
                work_id: request.work_id.clone(),
                message_ref,
                since: request.opened_at.clone(),
            });
        }
        Ok(targets)
    }

    fn record_failure(
        &self,
        work_id: &str,
        label: &str,
        channel_name: &str,
        error: &str,
    ) -> Result<(), BeltError> {
        tracing::warn!(%work_id, channel = %channel_name, what = %label, %error, "notification failed");
        let detail = format!("{channel_name}: {error}");
        self.db.record_event(&EventRecord {
            work_id,
            kind: transition_kind::NOTIFICATION_FAILED,
            actor: Actor::Daemon,
            reason: Some(label),
            detail: Some(&detail),
        })?;
        Ok(())
    }
}

/// Send one best-effort message; a failure is a value.
async fn send(channel: &dyn NotificationChannel, message: &OutboundMessage) -> ChannelSend {
    match channel.notify(message).await {
        Ok(NotifyOutcome::Sent(_)) => ChannelSend::Sent,
        Ok(NotifyOutcome::NoAddress) => ChannelSend::NoAddress,
        Err(e) => ChannelSend::Failed {
            error: format!("{e:#}"),
        },
    }
}

/// `already_handled` for a confirmed request, with the reply carrying the winner.
fn already_handled(
    request: &HitlRequest,
) -> Result<(ResponseOutcome, Option<(HitlRequest, String)>), BeltError> {
    let resolution = request.resolution.clone().ok_or_else(|| {
        BeltError::Database(format!(
            "HITL request {} is {:?} but has no resolution",
            request.hitl_id, request.status
        ))
    })?;
    let text = already_handled_text(&resolution);
    Ok((
        ResponseOutcome::AlreadyHandled {
            hitl_id: request.hitl_id.clone(),
            resolution,
        },
        Some((request.clone(), text)),
    ))
}
