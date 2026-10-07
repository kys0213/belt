//! Notifier: progress notifications, HITL request delivery, response polling
//! and natural-language proposals, against recording channel and runtime doubles.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use belt_core::escalation::EscalationAction;
use belt_core::hitl::{ConfirmPath, HitlAction, HitlId, HitlStatus, RespondOutcome};
use belt_core::notification::{
    ChannelEvent, HitlRef, InboundBody, InboundResponse, MessageKind, MessageRef,
    NotificationChannel, NotificationsConfig, NotifyOutcome, OutboundMessage, PollTarget,
    ResponseInbox,
};
use belt_core::phase::QueuePhase;
use belt_core::queue::HitlReason;
use belt_core::runtime::{AgentRuntime, RuntimeCapabilities, RuntimeRequest, RuntimeResponse};
use belt_core::transition::{Actor, TransitionOutcome, TransitionReason, TransitionRequest};
use belt_daemon::hitl::{HitlResponse, HitlService};
use belt_daemon::notify::{
    ChannelSend, DeliveryResult, NlInterpreter, Notifier, PollResult, ResponseOutcome,
    ResponseReport,
};
use belt_infra::db::{
    CollectOutcome, Database, DeliveryStatus, DeriveKind, DeriveOutcome, DeriveRequest, HitlTarget,
    NewItem, OpenHitlOutcome, OpenHitlRequest, transition_kind,
};

// ---- doubles ---------------------------------------------------------------

/// A channel that records what it sends and replays a fixed inbox on every
/// poll, the way a re-read of an issue returns the same comments again.
struct RecordingChannel {
    name: String,
    sent: Mutex<Vec<OutboundMessage>>,
    /// Sends that fail before sending succeeds again.
    failures_left: Mutex<u32>,
    inbox: Mutex<Vec<InboundResponse>>,
    polled: Mutex<Vec<Vec<PollTarget>>>,
    poll_fails: Mutex<bool>,
    receives: bool,
    /// Every item lacks an address on this channel.
    no_address: bool,
}

impl RecordingChannel {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            sent: Mutex::new(Vec::new()),
            failures_left: Mutex::new(0),
            inbox: Mutex::new(Vec::new()),
            polled: Mutex::new(Vec::new()),
            poll_fails: Mutex::new(false),
            receives: true,
            no_address: false,
        })
    }

    /// A channel on which no item has an address (e.g. items of another source).
    fn addressless(name: &str) -> Arc<Self> {
        let mut channel = Arc::try_unwrap(Self::new(name)).ok().unwrap();
        channel.no_address = true;
        Arc::new(channel)
    }

    fn failing(name: &str, times: u32) -> Arc<Self> {
        let channel = Self::new(name);
        *channel.failures_left.lock().unwrap() = times;
        channel
    }

    fn sent(&self) -> Vec<OutboundMessage> {
        self.sent.lock().unwrap().clone()
    }

    fn replies(&self) -> Vec<OutboundMessage> {
        self.sent()
            .into_iter()
            .filter(|m| m.kind == MessageKind::Reply)
            .collect()
    }

    fn receive(&self, response: InboundResponse) {
        self.inbox.lock().unwrap().push(response);
    }

    fn polls(&self) -> Vec<Vec<PollTarget>> {
        self.polled.lock().unwrap().clone()
    }
}

#[async_trait]
impl NotificationChannel for RecordingChannel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn notify(&self, msg: &OutboundMessage) -> anyhow::Result<NotifyOutcome> {
        if self.no_address {
            return Ok(NotifyOutcome::NoAddress);
        }
        {
            let mut left = self.failures_left.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                anyhow::bail!("channel down");
            }
        }
        let mut sent = self.sent.lock().unwrap();
        sent.push(msg.clone());
        Ok(NotifyOutcome::Sent(Some(MessageRef(format!(
            "msg-{}",
            sent.len()
        )))))
    }

    fn inbox(&self) -> Option<&dyn ResponseInbox> {
        self.receives.then_some(self as &dyn ResponseInbox)
    }
}

#[async_trait]
impl ResponseInbox for RecordingChannel {
    async fn poll(&self, targets: &[PollTarget]) -> anyhow::Result<Vec<InboundResponse>> {
        self.polled.lock().unwrap().push(targets.to_vec());
        if *self.poll_fails.lock().unwrap() {
            anyhow::bail!("gh api failed");
        }
        Ok(self.inbox.lock().unwrap().clone())
    }
}

/// A runtime that answers with scripted stdout and records the prompts.
struct ScriptedRuntime {
    outputs: Mutex<VecDeque<String>>,
    prompts: Mutex<Vec<String>>,
}

impl ScriptedRuntime {
    fn new(outputs: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            outputs: Mutex::new(outputs.iter().map(|s| s.to_string()).collect()),
            prompts: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }
}

#[async_trait]
impl AgentRuntime for ScriptedRuntime {
    fn name(&self) -> &str {
        "scripted"
    }

    async fn invoke(&self, request: RuntimeRequest) -> RuntimeResponse {
        self.prompts.lock().unwrap().push(request.prompt);
        let stdout = self
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected LLM call");
        RuntimeResponse {
            exit_code: 0,
            stdout,
            stderr: String::new(),
            duration: Duration::ZERO,
            token_usage: None,
            session_id: None,
        }
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::default()
    }
}

// ---- fixtures --------------------------------------------------------------

fn db() -> Arc<Database> {
    Arc::new(Database::open_in_memory().unwrap())
}

fn move_item(db: &Database, work_id: &str, from: QueuePhase, to: QueuePhase) {
    let outcome = db
        .transition(&TransitionRequest {
            work_id: work_id.to_string(),
            expected_from: from,
            to,
            actor: Actor::Daemon,
            reason: TransitionReason::Manual,
            detail: None,
        })
        .unwrap();
    assert!(
        matches!(outcome, TransitionOutcome::Applied { .. }),
        "{from:?} -> {to:?}: {outcome:?}"
    );
}

fn collect(db: &Database, source: &str) -> String {
    match db
        .insert_collected(&NewItem {
            source_id: format!("github:org/repo#{source}"),
            workspace_id: "ws".to_string(),
            state: "analyze".to_string(),
            title: None,
            actor: Actor::Daemon,
        })
        .unwrap()
    {
        CollectOutcome::Inserted { work_id } => work_id,
        other => panic!("expected a new item, got {other:?}"),
    }
}

fn running_item(db: &Database, source: &str) -> String {
    let id = collect(db, source);
    move_item(db, &id, QueuePhase::Pending, QueuePhase::Ready);
    move_item(db, &id, QueuePhase::Ready, QueuePhase::Running);
    id
}

fn open_hitl(db: &Arc<Database>, source: &str) -> (String, HitlId) {
    let work_id = running_item(db, source);
    let outcome = HitlService::new(db.clone())
        .open(&OpenHitlRequest {
            work_id: work_id.clone(),
            expected_from: QueuePhase::Running,
            reason: HitlReason::EvaluateFailure,
            notes: Some("tests keep failing".to_string()),
            actor: Actor::Daemon,
            transition_reason: TransitionReason::Escalation(EscalationAction::Hitl),
            timeout_at: None,
            terminal_action: None,
        })
        .unwrap();
    match outcome {
        OpenHitlOutcome::Opened { hitl_id, .. } => (work_id, hitl_id),
        other => panic!("expected Opened, got {other:?}"),
    }
}

fn config(yaml: &str) -> NotificationsConfig {
    serde_yaml::from_str(yaml).unwrap()
}

const ALLOW_ALICE: &str = "origin:\n  respond:\n    allow: [alice]\n";

fn notifier(
    db: &Arc<Database>,
    cfg: NotificationsConfig,
    channels: Vec<Arc<RecordingChannel>>,
    runtime: Arc<ScriptedRuntime>,
) -> Notifier {
    let channels = channels
        .into_iter()
        .map(|c| c as Arc<dyn NotificationChannel>)
        .collect();
    Notifier::new(
        db.clone(),
        cfg,
        channels,
        NlInterpreter::new(runtime, PathBuf::from(".")),
    )
    .unwrap()
}

fn response(external_id: &str, by: &str, hitl_id: &HitlId, body: InboundBody) -> InboundResponse {
    InboundResponse {
        external_id: external_id.to_string(),
        respondent: by.to_string(),
        hitl_ref: Some(HitlRef::Token(hitl_id.clone())),
        body,
    }
}

/// Every response report of one polling round on the only polled channel.
async fn poll(notifier: &Notifier) -> Vec<ResponseReport> {
    let mut reports = notifier.poll_responses().await.unwrap();
    assert_eq!(reports.len(), 1, "{reports:?}");
    match reports.remove(0).result {
        PollResult::Polled(responses) => responses,
        PollResult::Failed { error } => panic!("poll failed: {error}"),
    }
}

fn events_of(db: &Database, work_id: &str, kind: &str) -> Vec<Option<String>> {
    db.transitions_of(work_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.reason)
        .collect()
}

// ---- progress notifications ------------------------------------------------

#[tokio::test]
async fn default_config_sends_only_started_and_failed_to_origin() {
    let db = db();
    let earlier = running_item(&db, "1");
    let origin = RecordingChannel::new("origin");
    let mut notifier = notifier(
        &db,
        NotificationsConfig::default(),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );

    let done = running_item(&db, "2");
    move_item(&db, &done, QueuePhase::Running, QueuePhase::Completed);
    move_item(&db, &done, QueuePhase::Completed, QueuePhase::Done);
    let failed = running_item(&db, "3");
    move_item(&db, &failed, QueuePhase::Running, QueuePhase::Failed);

    let notices = notifier.notify_progress().await.unwrap();
    let sent: Vec<(String, MessageKind)> = origin
        .sent()
        .into_iter()
        .map(|m| (m.work_id, m.kind))
        .collect();
    assert_eq!(
        sent,
        vec![
            (done.clone(), MessageKind::Event(ChannelEvent::Started)),
            (failed.clone(), MessageKind::Event(ChannelEvent::Started)),
            (failed.clone(), MessageKind::Event(ChannelEvent::Failed)),
        ]
    );
    assert!(notices.iter().all(|n| n.result == ChannelSend::Sent));
    // The transitions before the notifier existed are never replayed.
    assert!(sent.iter().all(|(w, _)| w != &earlier));
    // A second round has nothing new.
    assert!(notifier.notify_progress().await.unwrap().is_empty());
}

fn derive_retry(db: &Database, work_id: &str, action: EscalationAction) -> String {
    match db
        .derive(&DeriveRequest {
            work_id: work_id.to_string(),
            expected_from: QueuePhase::Running,
            kind: DeriveKind::EscalationRetry,
            actor: Actor::Daemon,
            reason: TransitionReason::Derived,
            detail: Some(format!("escalation: {action}")),
        })
        .unwrap()
    {
        DeriveOutcome::Derived { work_id } => work_id,
        other => panic!("expected Derived, got {other:?}"),
    }
}

#[tokio::test]
async fn derived_skipped_is_not_a_skipped_event() {
    let db = db();
    let silent = running_item(&db, "1");
    let commented = running_item(&db, "2");
    let skipped = collect(&db, "3");
    let origin = RecordingChannel::new("origin");
    let mut notifier = notifier(
        &db,
        config("origin:\n  events: [skipped, failed]\n"),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );

    derive_retry(&db, &silent, EscalationAction::Retry);
    derive_retry(&db, &commented, EscalationAction::RetryWithComment);
    move_item(&db, &skipped, QueuePhase::Pending, QueuePhase::Skipped);

    notifier.notify_progress().await.unwrap();
    let sent: Vec<(String, MessageKind)> = origin
        .sent()
        .into_iter()
        .map(|m| (m.work_id, m.kind))
        .collect();
    assert_eq!(
        sent,
        vec![
            (commented, MessageKind::Event(ChannelEvent::Failed)),
            (skipped, MessageKind::Event(ChannelEvent::Skipped)),
        ]
    );
}

#[tokio::test]
async fn routes_by_event_filter_and_warns_for_missing_implementations() {
    let db = db();
    let chat = RecordingChannel::new("chat");
    let mut notifier = notifier(
        &db,
        config("channels:\n  - name: chat\n    type: chat\n    events: [started]\n"),
        vec![chat.clone()],
        ScriptedRuntime::new(&[]),
    );
    let work_id = running_item(&db, "1");
    move_item(&db, &work_id, QueuePhase::Running, QueuePhase::Failed);

    let notices = notifier.notify_progress().await.unwrap();
    let outcomes: Vec<(ChannelEvent, String, ChannelSend)> = notices
        .into_iter()
        .map(|n| (n.event, n.channel, n.result))
        .collect();
    assert_eq!(
        outcomes,
        vec![
            (
                ChannelEvent::Started,
                "origin".to_string(),
                ChannelSend::NoImplementation
            ),
            (ChannelEvent::Started, "chat".to_string(), ChannelSend::Sent),
            (
                ChannelEvent::Failed,
                "origin".to_string(),
                ChannelSend::NoImplementation
            ),
        ]
    );
    assert_eq!(chat.sent().len(), 1);
}

#[tokio::test]
async fn failed_progress_notification_is_recorded_and_not_retried() {
    let db = db();
    let origin = RecordingChannel::failing("origin", 1);
    let mut notifier = notifier(
        &db,
        NotificationsConfig::default(),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );
    let work_id = running_item(&db, "1");

    let notices = notifier.notify_progress().await.unwrap();
    assert!(matches!(notices[0].result, ChannelSend::Failed { .. }));
    assert_eq!(
        events_of(&db, &work_id, transition_kind::NOTIFICATION_FAILED),
        vec![Some("started".to_string())]
    );
    assert_eq!(db.get_item(&work_id).unwrap().phase(), QueuePhase::Running);
    assert!(notifier.notify_progress().await.unwrap().is_empty());
    assert!(origin.sent().is_empty());
}

#[tokio::test]
async fn item_without_an_address_on_the_channel_is_neither_failed_nor_retried() {
    let db = db();
    let origin = RecordingChannel::addressless("origin");
    let mut notifier = notifier(
        &db,
        NotificationsConfig::default(),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );
    let (work_id, hitl_id) = open_hitl(&db, "1");
    notifier.register_deliveries(&hitl_id).unwrap();

    let notices = notifier.notify_progress().await.unwrap();
    assert!(!notices.is_empty());
    assert!(notices.iter().all(|n| n.result == ChannelSend::NoAddress));

    for _ in 0..6 {
        let reports = notifier.deliver_due().await.unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].result, DeliveryResult::NoAddress);
    }
    let stored = db.delivery(&hitl_id, "origin").unwrap().unwrap();
    assert_eq!(stored.status, DeliveryStatus::Pending);
    assert_eq!(stored.attempts, 0);
    assert!(events_of(&db, &work_id, transition_kind::NOTIFICATION_FAILED).is_empty());
    assert!(origin.sent().is_empty());
}

// ---- HITL request delivery -------------------------------------------------

#[tokio::test]
async fn hitl_request_is_delivered_once_with_its_token() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let notifier = notifier(
        &db,
        NotificationsConfig::default(),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );
    let (work_id, hitl_id) = open_hitl(&db, "1");

    assert_eq!(
        notifier.register_deliveries(&hitl_id).unwrap(),
        vec!["origin".to_string()]
    );
    let reports = notifier.deliver_due().await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].result, DeliveryResult::Sent);
    let sent = origin.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].kind,
        MessageKind::Event(ChannelEvent::HitlRequested)
    );
    assert_eq!(sent[0].work_id, work_id);
    assert_eq!(sent[0].hitl_id.as_ref(), Some(&hitl_id));
    let stored = db.delivery(&hitl_id, "origin").unwrap().unwrap();
    assert_eq!(stored.status, DeliveryStatus::Sent);
    assert_eq!(stored.message_ref.as_deref(), Some("msg-1"));

    assert!(notifier.deliver_due().await.unwrap().is_empty());
    assert_eq!(origin.sent().len(), 1);
}

#[tokio::test]
async fn failing_delivery_is_retried_each_round_and_fails_at_the_cap() {
    let db = db();
    let origin = RecordingChannel::failing("origin", 100);
    let notifier = notifier(
        &db,
        NotificationsConfig::default(),
        vec![origin],
        ScriptedRuntime::new(&[]),
    );
    let (work_id, hitl_id) = open_hitl(&db, "1");
    notifier.register_deliveries(&hitl_id).unwrap();

    let mut results = Vec::new();
    for _ in 0..6 {
        results.extend(
            notifier
                .deliver_due()
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.result),
        );
    }
    assert_eq!(
        results,
        vec![
            DeliveryResult::Retrying { attempts: 1 },
            DeliveryResult::Retrying { attempts: 2 },
            DeliveryResult::Retrying { attempts: 3 },
            DeliveryResult::Retrying { attempts: 4 },
            DeliveryResult::GaveUp { attempts: 5 },
        ]
    );
    assert_eq!(
        db.delivery(&hitl_id, "origin").unwrap().unwrap().status,
        DeliveryStatus::Failed
    );
    assert_eq!(
        events_of(&db, &work_id, transition_kind::NOTIFICATION_FAILED).len(),
        5
    );
}

#[tokio::test]
async fn confirmed_request_is_not_delivered() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let notifier = notifier(
        &db,
        NotificationsConfig::default(),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );
    let (_, hitl_id) = open_hitl(&db, "1");
    notifier.register_deliveries(&hitl_id).unwrap();
    cli_responds(&db, &hitl_id, HitlAction::Done);

    assert!(notifier.deliver_due().await.unwrap().is_empty());
    assert!(origin.sent().is_empty());
}

#[tokio::test]
async fn delivery_is_registered_only_for_implemented_routes() {
    let db = db();
    let notifier = notifier(
        &db,
        NotificationsConfig::default(),
        vec![],
        ScriptedRuntime::new(&[]),
    );
    let (_, hitl_id) = open_hitl(&db, "1");
    assert!(notifier.register_deliveries(&hitl_id).unwrap().is_empty());
    assert!(db.deliveries_of(&hitl_id).unwrap().is_empty());
}

// ---- response polling ------------------------------------------------------

fn cli_responds(db: &Arc<Database>, hitl_id: &HitlId, action: HitlAction) {
    let outcome = HitlService::new(db.clone())
        .respond(&HitlResponse {
            target: HitlTarget::Id(hitl_id.clone()),
            action,
            by: "irene".to_string(),
            via: "cli".to_string(),
            path: ConfirmPath::Direct,
            notes: None,
        })
        .unwrap();
    assert!(matches!(outcome, RespondOutcome::Won { .. }), "{outcome:?}");
}

#[tokio::test]
async fn empty_allowlist_does_not_receive_responses() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let notifier = notifier(
        &db,
        NotificationsConfig::default(),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );
    let (_, hitl_id) = open_hitl(&db, "1");
    origin.receive(response(
        "c1",
        "alice",
        &hitl_id,
        InboundBody::Action(HitlAction::Done),
    ));

    assert!(notifier.poll_responses().await.unwrap().is_empty());
    assert!(origin.polls().is_empty());
    assert_eq!(
        db.hitl_request(&hitl_id).unwrap().unwrap().status,
        HitlStatus::Open
    );
}

#[tokio::test]
async fn explicit_action_wins_and_a_repoll_is_not_a_rejection() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let notifier = notifier(
        &db,
        config(ALLOW_ALICE),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );
    let (work_id, hitl_id) = open_hitl(&db, "1");
    origin.receive(response(
        "c1",
        "alice",
        &hitl_id,
        InboundBody::Action(HitlAction::Skip),
    ));

    let reports = poll(&notifier).await;
    assert_eq!(
        reports[0].outcome,
        ResponseOutcome::Won {
            hitl_id: hitl_id.clone(),
            action: HitlAction::Skip,
            path: ConfirmPath::Direct,
        }
    );
    let resolution = db
        .hitl_request(&hitl_id)
        .unwrap()
        .unwrap()
        .resolution
        .unwrap();
    assert_eq!(
        (resolution.by.as_str(), resolution.via.as_str()),
        ("alice", "origin")
    );

    // The same comment read again is skipped, not answered already_handled.
    let again = poll(&notifier).await;
    assert_eq!(again[0].outcome, ResponseOutcome::Duplicate);
    assert!(origin.replies().is_empty());
    assert!(events_of(&db, &work_id, transition_kind::HITL_RESPONSE_REJECTED).is_empty());
}

#[tokio::test]
async fn late_explicit_action_is_answered_already_handled_with_the_winner() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let notifier = notifier(
        &db,
        config(ALLOW_ALICE),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );
    let (work_id, hitl_id) = open_hitl(&db, "1");
    cli_responds(&db, &hitl_id, HitlAction::Retry);
    origin.receive(response(
        "c1",
        "alice",
        &hitl_id,
        InboundBody::Action(HitlAction::Done),
    ));

    let reports = poll(&notifier).await;
    match &reports[0].outcome {
        ResponseOutcome::AlreadyHandled { resolution, .. } => {
            assert_eq!(resolution.by, "irene");
            assert_eq!(resolution.action, HitlAction::Retry);
        }
        other => panic!("expected AlreadyHandled, got {other:?}"),
    }
    assert_eq!(reports[0].reply, Some(ChannelSend::Sent));
    let replies = origin.replies();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].work_id, work_id);
    assert!(replies[0].text.contains("irene"), "{}", replies[0].text);
    assert!(replies[0].text.contains("retry"), "{}", replies[0].text);
}

#[tokio::test]
async fn uncorrelated_response_is_recorded_not_found_without_reply() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let notifier = notifier(
        &db,
        config(ALLOW_ALICE),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );
    let (_, hitl_id) = open_hitl(&db, "1");
    origin.receive(InboundResponse {
        external_id: "c1".to_string(),
        respondent: "alice".to_string(),
        hitl_ref: None,
        body: InboundBody::Action(HitlAction::Done),
    });
    origin.receive(response(
        "c2",
        "alice",
        &HitlId::new("hitl-999"),
        InboundBody::Action(HitlAction::Done),
    ));

    let reports = poll(&notifier).await;
    let outcomes: Vec<&ResponseOutcome> = reports.iter().map(|r| &r.outcome).collect();
    assert_eq!(
        outcomes,
        vec![&ResponseOutcome::NotFound, &ResponseOutcome::NotFound]
    );
    assert!(origin.replies().is_empty());
    assert_eq!(
        db.hitl_request(&hitl_id).unwrap().unwrap().status,
        HitlStatus::Open
    );
    // Recorded: the next round skips them.
    let again = poll(&notifier).await;
    assert!(
        again
            .iter()
            .all(|r| r.outcome == ResponseOutcome::Duplicate)
    );
}

#[tokio::test]
async fn reply_to_the_delivered_message_is_correlated() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let notifier = notifier(
        &db,
        config(ALLOW_ALICE),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );
    let (_, hitl_id) = open_hitl(&db, "1");
    notifier.register_deliveries(&hitl_id).unwrap();
    notifier.deliver_due().await.unwrap();
    origin.receive(InboundResponse {
        external_id: "c1".to_string(),
        respondent: "alice".to_string(),
        hitl_ref: Some(HitlRef::ReplyTo(MessageRef("msg-1".to_string()))),
        body: InboundBody::Action(HitlAction::Done),
    });

    let reports = poll(&notifier).await;
    assert!(
        matches!(&reports[0].outcome, ResponseOutcome::Won { hitl_id: id, .. } if id == &hitl_id)
    );
}

#[tokio::test]
async fn respondent_outside_the_allowlist_is_unauthorized_without_reply() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let notifier = notifier(
        &db,
        config(ALLOW_ALICE),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );
    let (work_id, hitl_id) = open_hitl(&db, "1");
    origin.receive(response(
        "c1",
        "mallory",
        &hitl_id,
        InboundBody::Action(HitlAction::Done),
    ));

    let reports = poll(&notifier).await;
    assert_eq!(
        reports[0].outcome,
        ResponseOutcome::Unauthorized {
            hitl_id: hitl_id.clone()
        }
    );
    assert_eq!(reports[0].reply, None);
    assert!(origin.replies().is_empty());
    assert_eq!(
        events_of(&db, &work_id, transition_kind::HITL_RESPONSE_REJECTED),
        vec![Some("unauthorized".to_string())]
    );
    assert_eq!(
        db.hitl_request(&hitl_id).unwrap().unwrap().status,
        HitlStatus::Open
    );
    // Recorded once even though the comment is read again.
    poll(&notifier).await;
    assert_eq!(
        events_of(&db, &work_id, transition_kind::HITL_RESPONSE_REJECTED).len(),
        1
    );
}

#[tokio::test]
async fn poll_failure_is_a_value_and_retried_next_round() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let notifier = notifier(
        &db,
        config(ALLOW_ALICE),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );
    let (_, hitl_id) = open_hitl(&db, "1");
    origin.receive(response(
        "c1",
        "alice",
        &hitl_id,
        InboundBody::Action(HitlAction::Done),
    ));
    *origin.poll_fails.lock().unwrap() = true;

    let reports = notifier.poll_responses().await.unwrap();
    assert!(matches!(reports[0].result, PollResult::Failed { .. }));

    *origin.poll_fails.lock().unwrap() = false;
    let reports = poll(&notifier).await;
    assert!(matches!(reports[0].outcome, ResponseOutcome::Won { .. }));
}

#[tokio::test]
async fn responses_are_read_since_the_request_opened_also_after_a_restart() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let (_, hitl_id) = open_hitl(&db, "1");
    let opened_at = db.hitl_request(&hitl_id).unwrap().unwrap().opened_at;

    // While the daemon is down the CLI answers and a channel response arrives.
    cli_responds(&db, &hitl_id, HitlAction::Done);
    origin.receive(response(
        "c1",
        "alice",
        &hitl_id,
        InboundBody::Action(HitlAction::Skip),
    ));

    let restarted = notifier(
        &db,
        config(ALLOW_ALICE),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );
    let reports = poll(&restarted).await;
    let targets = &origin.polls()[0];
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].hitl_id, hitl_id);
    assert_eq!(targets[0].since, opened_at);
    assert!(matches!(
        reports[0].outcome,
        ResponseOutcome::AlreadyHandled { .. }
    ));
    assert_eq!(origin.replies().len(), 1);
}

// ---- natural-language proposals --------------------------------------------

#[tokio::test]
async fn natural_language_is_proposed_then_confirmed() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let runtime = ScriptedRuntime::new(&[r#"{"action": "skip", "summary": "not needed"}"#]);
    let notifier = notifier(
        &db,
        config(ALLOW_ALICE),
        vec![origin.clone()],
        runtime.clone(),
    );
    let (_, hitl_id) = open_hitl(&db, "1");
    origin.receive(response(
        "c1",
        "alice",
        &hitl_id,
        InboundBody::Text("this can be skipped".to_string()),
    ));

    let reports = poll(&notifier).await;
    assert!(matches!(
        reports[0].outcome,
        ResponseOutcome::Proposed {
            action: HitlAction::Skip,
            ..
        }
    ));
    assert_eq!(runtime.calls(), 1);
    let replies = origin.replies();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].hitl_id.as_ref(), Some(&hitl_id));
    assert!(replies[0].text.contains("skip"), "{}", replies[0].text);
    assert!(
        replies[0].text.contains("/belt confirm"),
        "{}",
        replies[0].text
    );
    // A proposal does not take part in the race.
    assert_eq!(
        db.hitl_request(&hitl_id).unwrap().unwrap().status,
        HitlStatus::Open
    );

    origin.receive(response("c2", "alice", &hitl_id, InboundBody::Confirm));
    let reports = poll(&notifier).await;
    assert_eq!(reports[0].outcome, ResponseOutcome::Duplicate);
    assert_eq!(
        reports[1].outcome,
        ResponseOutcome::Won {
            hitl_id: hitl_id.clone(),
            action: HitlAction::Skip,
            path: ConfirmPath::NaturalLanguage,
        }
    );
    let resolution = db
        .hitl_request(&hitl_id)
        .unwrap()
        .unwrap()
        .resolution
        .unwrap();
    assert_eq!(resolution.path, ConfirmPath::NaturalLanguage);
    assert_eq!(resolution.via, "origin");
}

#[tokio::test]
async fn newer_natural_language_response_replaces_the_proposal() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let runtime = ScriptedRuntime::new(&[
        r#"{"action": "skip", "summary": "drop it"}"#,
        r#"{"action": "retry", "summary": "try again"}"#,
    ]);
    let notifier = notifier(&db, config(ALLOW_ALICE), vec![origin.clone()], runtime);
    let (_, hitl_id) = open_hitl(&db, "1");
    origin.receive(response(
        "c1",
        "alice",
        &hitl_id,
        InboundBody::Text("skip it".to_string()),
    ));
    poll(&notifier).await;
    origin.receive(response(
        "c2",
        "alice",
        &hitl_id,
        InboundBody::Text("actually, try once more".to_string()),
    ));
    poll(&notifier).await;
    origin.receive(response("c3", "alice", &hitl_id, InboundBody::Confirm));

    let reports = poll(&notifier).await;
    assert_eq!(
        reports[2].outcome,
        ResponseOutcome::Won {
            hitl_id,
            action: HitlAction::Retry,
            path: ConfirmPath::NaturalLanguage,
        }
    );
}

#[tokio::test]
async fn confirmation_after_another_path_won_is_already_handled() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let runtime = ScriptedRuntime::new(&[r#"{"action": "skip", "summary": "drop it"}"#]);
    let notifier = notifier(&db, config(ALLOW_ALICE), vec![origin.clone()], runtime);
    let (_, hitl_id) = open_hitl(&db, "1");
    origin.receive(response(
        "c1",
        "alice",
        &hitl_id,
        InboundBody::Text("skip it".to_string()),
    ));
    poll(&notifier).await;
    cli_responds(&db, &hitl_id, HitlAction::Done);
    origin.receive(response("c2", "alice", &hitl_id, InboundBody::Confirm));

    let reports = poll(&notifier).await;
    match &reports[1].outcome {
        ResponseOutcome::AlreadyHandled { resolution, .. } => {
            assert_eq!(resolution.by, "irene");
            assert_eq!(resolution.action, HitlAction::Done);
        }
        other => panic!("expected AlreadyHandled, got {other:?}"),
    }
    let replies = origin.replies();
    assert_eq!(replies.len(), 2);
    assert!(replies[1].text.contains("irene"), "{}", replies[1].text);
}

#[tokio::test]
async fn natural_language_after_confirmation_skips_the_llm() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let runtime = ScriptedRuntime::new(&[]);
    let notifier = notifier(
        &db,
        config(ALLOW_ALICE),
        vec![origin.clone()],
        runtime.clone(),
    );
    let (_, hitl_id) = open_hitl(&db, "1");
    cli_responds(&db, &hitl_id, HitlAction::Done);
    origin.receive(response(
        "c1",
        "alice",
        &hitl_id,
        InboundBody::Text("skip it".to_string()),
    ));

    let reports = poll(&notifier).await;
    assert!(matches!(
        reports[0].outcome,
        ResponseOutcome::AlreadyHandled { .. }
    ));
    assert_eq!(runtime.calls(), 0);
}

#[tokio::test]
async fn uninterpretable_text_gets_a_clear_reply_and_no_proposal() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let runtime = ScriptedRuntime::new(&[
        r#"{"action": "none", "summary": "unclear"}"#,
        "I think you should skip",
    ]);
    let notifier = notifier(&db, config(ALLOW_ALICE), vec![origin.clone()], runtime);
    let (_, hitl_id) = open_hitl(&db, "1");
    origin.receive(response(
        "c1",
        "alice",
        &hitl_id,
        InboundBody::Text("hmm".to_string()),
    ));
    origin.receive(response(
        "c2",
        "alice",
        &hitl_id,
        InboundBody::Text("whatever".to_string()),
    ));

    let reports = poll(&notifier).await;
    assert!(
        reports
            .iter()
            .all(|r| matches!(r.outcome, ResponseOutcome::NotInterpreted { .. })),
        "{reports:?}"
    );
    assert_eq!(db.latest_proposal(&hitl_id, "alice").unwrap(), None);
    let replies = origin.replies();
    assert_eq!(replies.len(), 2);
    assert!(replies.iter().all(|r| r.text.contains("/belt")));
}

#[tokio::test]
async fn confirmation_without_a_proposal_is_answered() {
    let db = db();
    let origin = RecordingChannel::new("origin");
    let notifier = notifier(
        &db,
        config(ALLOW_ALICE),
        vec![origin.clone()],
        ScriptedRuntime::new(&[]),
    );
    let (_, hitl_id) = open_hitl(&db, "1");
    origin.receive(response("c1", "alice", &hitl_id, InboundBody::Confirm));

    let reports = poll(&notifier).await;
    assert_eq!(
        reports[0].outcome,
        ResponseOutcome::NoPendingProposal {
            hitl_id: hitl_id.clone()
        }
    );
    assert_eq!(origin.replies().len(), 1);
    assert_eq!(
        db.hitl_request(&hitl_id).unwrap().unwrap().status,
        HitlStatus::Open
    );
}

// ---- correlation through the real GitHub channel ---------------------------

/// A `gh` double: every command answers with the same stdout.
struct FixedGh {
    stdout: String,
}

#[async_trait]
impl belt_core::platform::ShellExecutor for FixedGh {
    async fn execute(
        &self,
        _command: &str,
        _working_dir: &std::path::Path,
        _env_vars: &std::collections::HashMap<String, String>,
    ) -> Result<belt_core::platform::ShellOutput, belt_core::error::BeltError> {
        Ok(belt_core::platform::ShellOutput {
            exit_code: Some(0),
            stdout: self.stdout.clone(),
            stderr: String::new(),
        })
    }
}

fn github_comment(n: u32, login: &str, body: &str) -> serde_json::Value {
    serde_json::json!({
        "id": format!("IC_{n}"),
        "url": format!("https://github.com/org/repo/issues/1#issuecomment-{n}"),
        "author": {"login": login},
        "body": body,
        "createdAt": "2099-01-01T00:00:00Z",
        "includesCreatedEdit": false,
    })
}

#[tokio::test]
async fn response_without_id_reaches_the_new_open_request_not_the_confirmed_one() {
    let db = db();
    // h-1 was confirmed and is still in the late-response window; h-2 is a
    // newer open request of another state of the same issue.
    let (_, old) = open_hitl(&db, "1");
    cli_responds(&db, &old, HitlAction::Retry);
    let second = match db
        .insert_collected(&NewItem {
            source_id: "github:org/repo#1".to_string(),
            workspace_id: "ws".to_string(),
            state: "implement".to_string(),
            title: None,
            actor: Actor::Daemon,
        })
        .unwrap()
    {
        CollectOutcome::Inserted { work_id } => work_id,
        other => panic!("expected a new item, got {other:?}"),
    };
    move_item(&db, &second, QueuePhase::Pending, QueuePhase::Ready);
    move_item(&db, &second, QueuePhase::Ready, QueuePhase::Running);
    let new = match HitlService::new(db.clone())
        .open(&OpenHitlRequest {
            work_id: second.clone(),
            expected_from: QueuePhase::Running,
            reason: HitlReason::EvaluateFailure,
            notes: None,
            actor: Actor::Daemon,
            transition_reason: TransitionReason::Escalation(EscalationAction::Hitl),
            timeout_at: None,
            terminal_action: None,
        })
        .unwrap()
    {
        OpenHitlOutcome::Opened { hitl_id, .. } => hitl_id,
        other => panic!("expected Opened, got {other:?}"),
    };

    let comments = serde_json::json!({ "comments": [
        github_comment(1, "alice", "/belt done"),
        github_comment(2, "alice", &format!("/belt done {old}")),
    ]})
    .to_string();
    let channel = belt_infra::channels::github::GitHubOriginChannel::new(
        belt_infra::channels::github::GitHubChannelConfig::new("org/repo"),
        Arc::new(FixedGh { stdout: comments }),
    );
    let notifier = Notifier::new(
        db.clone(),
        config(ALLOW_ALICE),
        vec![Arc::new(channel) as Arc<dyn NotificationChannel>],
        NlInterpreter::new(ScriptedRuntime::new(&[]), PathBuf::from(".")),
    )
    .unwrap();

    let reports = poll(&notifier).await;
    assert!(
        matches!(&reports[0].outcome, ResponseOutcome::Won { hitl_id, action: HitlAction::Done, .. } if hitl_id == &new),
        "{:?}",
        reports[0].outcome
    );
    // The explicit id of the confirmed request is answered `already_handled`.
    assert!(
        matches!(&reports[1].outcome, ResponseOutcome::AlreadyHandled { hitl_id, .. } if hitl_id == &old),
        "{:?}",
        reports[1].outcome
    );
    assert!(reports[1].reply.is_some());
    assert_eq!(
        db.hitl_request(&new).unwrap().unwrap().status,
        HitlStatus::Resolved
    );
}
