//! The notifier inside the daemon tick: HITL requests reach the origin
//! channel once, the channel's inbox answers them, and the confirmed request
//! is applied by the next tick's post-processing. Channel and runtime are
//! recording doubles; nothing leaves the process.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use belt_core::escalation::EscalationAction;
use belt_core::hitl::{HitlAction, HitlId};
use belt_core::notification::{
    ChannelEvent, HitlRef, InboundBody, InboundResponse, MessageKind, MessageRef,
    NotificationChannel, NotificationsConfig, NotifyOutcome, OutboundMessage, PollTarget,
    ResponseInbox,
};
use belt_core::phase::QueuePhase;
use belt_core::queue::HitlReason;
use belt_core::runtime::{
    AgentRuntime, RuntimeCapabilities, RuntimeRegistry, RuntimeRequest, RuntimeResponse,
};
use belt_core::transition::{Actor, TransitionOutcome, TransitionReason, TransitionRequest};
use belt_core::workspace::WorkspaceConfig;
use belt_daemon::daemon::Daemon;
use belt_daemon::notify::{NlInterpreter, Notifier};
use belt_infra::db::{
    CollectOutcome, Database, DeliveryStatus, NewItem, OpenHitlOutcome, OpenHitlRequest,
    transition_kind,
};
use belt_infra::runtimes::mock::MockRuntime;
use belt_infra::sources::mock::MockDataSource;
use belt_infra::worktree::MockWorktreeManager;
use tempfile::TempDir;

const SOURCE: &str = "github:org/repo#1";

struct RecordingChannel {
    sent: Mutex<Vec<OutboundMessage>>,
    inbox: Mutex<Vec<InboundResponse>>,
    send_fails: bool,
}

impl RecordingChannel {
    fn new(send_fails: bool) -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            inbox: Mutex::new(Vec::new()),
            send_fails,
        })
    }

    fn sent(&self) -> Vec<OutboundMessage> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl NotificationChannel for RecordingChannel {
    fn name(&self) -> &str {
        "origin"
    }

    async fn notify(&self, msg: &OutboundMessage) -> anyhow::Result<NotifyOutcome> {
        if self.send_fails {
            anyhow::bail!("channel down");
        }
        let mut sent = self.sent.lock().unwrap();
        sent.push(msg.clone());
        Ok(NotifyOutcome::Sent(Some(MessageRef(format!(
            "msg-{}",
            sent.len()
        )))))
    }

    fn inbox(&self) -> Option<&dyn ResponseInbox> {
        Some(self)
    }
}

#[async_trait]
impl ResponseInbox for RecordingChannel {
    async fn poll(&self, _targets: &[PollTarget]) -> anyhow::Result<Vec<InboundResponse>> {
        Ok(self.inbox.lock().unwrap().clone())
    }
}

struct NoLlm;

#[async_trait]
impl AgentRuntime for NoLlm {
    fn name(&self) -> &str {
        "no-llm"
    }

    async fn invoke(&self, _request: RuntimeRequest) -> RuntimeResponse {
        panic!("no natural-language response in this scenario");
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::default()
    }
}

fn workspace() -> WorkspaceConfig {
    serde_yaml::from_str(
        r#"
name: test-ws
concurrency: 2
sources:
  github:
    url: https://github.com/org/repo
    states:
      analyze:
        trigger:
          label: "belt:analyze"
        handlers:
          - prompt: "analyze this issue"
    escalation:
      1: retry
      2: retry_with_comment
      3: hitl
      terminal: skip
"#,
    )
    .unwrap()
}

fn daemon(tmp: &TempDir) -> Daemon {
    let mut registry = RuntimeRegistry::new("mock".to_string());
    registry.register(Arc::new(MockRuntime::new("mock", vec![])));
    Daemon::new(
        workspace(),
        vec![Box::new(MockDataSource::new("github"))],
        Arc::new(registry),
        Box::new(MockWorktreeManager::new(tmp.path().join("worktrees"))),
        4,
        Database::open_in_memory().unwrap(),
    )
}

fn with_origin(daemon: Daemon, channel: &Arc<RecordingChannel>) -> Daemon {
    let config: NotificationsConfig =
        serde_yaml::from_str("origin:\n  respond:\n    allow: [alice]\n").unwrap();
    let notifier = Notifier::new(
        Arc::clone(daemon.database()),
        config,
        vec![channel.clone() as Arc<dyn NotificationChannel>],
        NlInterpreter::new(Arc::new(NoLlm), PathBuf::from(".")),
    )
    .unwrap();
    daemon.with_notifier(notifier)
}

fn move_item(db: &Database, work_id: &str, from: QueuePhase, to: QueuePhase) {
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

fn running_item(db: &Database) -> String {
    let CollectOutcome::Inserted { work_id } = db
        .insert_collected(&NewItem {
            source_id: SOURCE.to_string(),
            workspace_id: "test-ws".to_string(),
            state: "analyze".to_string(),
            title: None,
            actor: Actor::Daemon,
        })
        .unwrap()
    else {
        panic!("expected a new item");
    };
    move_item(db, &work_id, QueuePhase::Pending, QueuePhase::Ready);
    move_item(db, &work_id, QueuePhase::Ready, QueuePhase::Running);
    work_id
}

fn open_hitl(daemon: &mut Daemon) -> (String, HitlId) {
    let work_id = running_item(daemon.database());
    let OpenHitlOutcome::Opened { hitl_id, .. } = daemon
        .hitl()
        .open(&OpenHitlRequest {
            work_id: work_id.clone(),
            expected_from: QueuePhase::Running,
            reason: HitlReason::RetryMaxExceeded,
            notes: Some("handler kept failing".to_string()),
            actor: Actor::Daemon,
            transition_reason: TransitionReason::Escalation(EscalationAction::Hitl),
            timeout_at: None,
            terminal_action: None,
        })
        .unwrap()
    else {
        panic!("expected an open request");
    };
    daemon.worktree_mgr().create_or_reuse(&work_id).unwrap();
    daemon.restore_from_store().unwrap();
    (work_id, hitl_id)
}

#[tokio::test]
async fn tick_delivers_a_hitl_request_once_and_applies_the_inbox_response() {
    let tmp = TempDir::new().unwrap();
    let channel = RecordingChannel::new(false);
    let mut daemon = with_origin(daemon(&tmp), &channel);
    let (work_id, hitl_id) = open_hitl(&mut daemon);

    daemon.tick().await.unwrap();
    daemon.tick().await.unwrap();

    let requests: Vec<_> = channel
        .sent()
        .into_iter()
        .filter(|m| m.kind == MessageKind::Event(ChannelEvent::HitlRequested))
        .collect();
    assert_eq!(requests.len(), 1, "sent exactly once across two ticks");
    assert_eq!(requests[0].hitl_id.as_ref(), Some(&hitl_id));
    let deliveries = daemon.database().deliveries_of(&hitl_id).unwrap();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].channel, "origin");
    assert_eq!(deliveries[0].status, DeliveryStatus::Sent);

    channel.inbox.lock().unwrap().push(InboundResponse {
        external_id: "c-1".to_string(),
        respondent: "alice".to_string(),
        hitl_ref: Some(HitlRef::Token(hitl_id.clone())),
        body: InboundBody::Action(HitlAction::Done),
    });
    // The poll confirms the request; the next tick's post-processing applies it.
    daemon.tick().await.unwrap();
    assert_eq!(
        daemon.database().get_item(&work_id).unwrap().phase(),
        QueuePhase::Hitl
    );
    daemon.tick().await.unwrap();
    assert_eq!(
        daemon.database().get_item(&work_id).unwrap().phase(),
        QueuePhase::Done
    );
}

#[tokio::test]
async fn tick_announces_started_to_the_origin_channel() {
    let tmp = TempDir::new().unwrap();
    let channel = RecordingChannel::new(false);
    let mut daemon = with_origin(daemon(&tmp), &channel);

    let work_id = running_item(daemon.database());
    daemon.tick().await.unwrap();

    let started: Vec<_> = channel
        .sent()
        .into_iter()
        .filter(|m| m.kind == MessageKind::Event(ChannelEvent::Started))
        .collect();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].work_id, work_id);
}

#[tokio::test]
async fn failed_progress_notification_is_recorded_and_does_not_stop_the_tick() {
    let tmp = TempDir::new().unwrap();
    let channel = RecordingChannel::new(true);
    let mut daemon = with_origin(daemon(&tmp), &channel);

    let work_id = running_item(daemon.database());
    daemon.tick().await.unwrap();

    let failures: Vec<_> = daemon
        .database()
        .transitions_of(&work_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == transition_kind::NOTIFICATION_FAILED)
        .collect();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].detail.as_deref(), Some("origin: channel down"));
}

#[tokio::test]
async fn daemon_without_a_notifier_ticks_as_before() {
    let tmp = TempDir::new().unwrap();
    let mut daemon = daemon(&tmp);
    let (_, hitl_id) = open_hitl(&mut daemon);
    daemon.tick().await.unwrap();
    assert!(
        daemon
            .database()
            .deliveries_of(&hitl_id)
            .unwrap()
            .is_empty()
    );
}
