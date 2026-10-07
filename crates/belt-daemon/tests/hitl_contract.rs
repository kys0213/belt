//! HITL single contract: first response wins, expiry races responses, the
//! phase stays Hitl until post-processing, and `on_hitl_opened` fires once per
//! open request.

use std::sync::{Arc, Barrier, Mutex};

use async_trait::async_trait;
use belt_core::escalation::EscalationAction;
use belt_core::hitl::{ConfirmPath, HitlAction, HitlId, HitlStatus, RespondOutcome};
use belt_core::lifecycle::{HookContext, LifecycleHook};
use belt_core::phase::QueuePhase;
use belt_core::queue::HitlReason;
use belt_core::queue::testing::test_item;
use belt_core::runtime::RuntimeRegistry;
use belt_core::transition::{Actor, TransitionOutcome, TransitionReason, TransitionRequest};
use belt_core::workspace::WorkspaceConfig;
use belt_daemon::daemon::Daemon;
use belt_daemon::hitl::{HitlExpiry, HitlResponse, HitlService};
use belt_infra::db::{
    CollectOutcome, Database, HitlTarget, NewItem, OpenHitlOutcome, OpenHitlRequest,
    transition_kind,
};
use belt_infra::runtimes::mock::MockRuntime;
use belt_infra::sources::mock::MockDataSource;
use belt_infra::worktree::MockWorktreeManager;
use tempfile::TempDir;

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
    let outcome = db
        .insert_collected(&NewItem {
            source_id: format!("github:org/repo#{source}"),
            workspace_id: "test-ws".to_string(),
            state: "analyze".to_string(),
            title: None,
            actor: Actor::Daemon,
        })
        .unwrap();
    let CollectOutcome::Inserted { work_id } = outcome else {
        panic!("expected a new item, got {outcome:?}");
    };
    work_id
}

fn running_item(db: &Database, source: &str) -> String {
    let id = collect(db, source);
    move_item(db, &id, QueuePhase::Pending, QueuePhase::Ready);
    move_item(db, &id, QueuePhase::Ready, QueuePhase::Running);
    id
}

fn open_request(work_id: &str) -> OpenHitlRequest {
    OpenHitlRequest {
        work_id: work_id.to_string(),
        expected_from: QueuePhase::Running,
        reason: HitlReason::EvaluateFailure,
        notes: None,
        actor: Actor::Daemon,
        transition_reason: TransitionReason::Escalation(EscalationAction::Hitl),
        timeout_at: None,
        terminal_action: None,
    }
}

fn open(service: &HitlService, work_id: &str) -> HitlId {
    match service.open(&open_request(work_id)).unwrap() {
        OpenHitlOutcome::Opened { hitl_id, .. } => hitl_id,
        other => panic!("expected Opened, got {other:?}"),
    }
}

fn response(target: HitlTarget, action: HitlAction, via: &str) -> HitlResponse {
    HitlResponse {
        target,
        action,
        by: format!("{via}-user"),
        via: via.to_string(),
        path: ConfirmPath::Direct,
        notes: None,
    }
}

fn file_db(dir: &TempDir) -> String {
    dir.path().join("belt.db").to_str().unwrap().to_string()
}

fn rejections(db: &Database, work_id: &str) -> Vec<(Option<String>, Option<String>)> {
    db.transitions_of(work_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == transition_kind::HITL_RESPONSE_REJECTED)
        .map(|e| (e.reason, Some(e.actor)))
        .collect()
}

#[test]
fn concurrent_responses_have_one_winner_and_the_loser_is_recorded() {
    let dir = TempDir::new().unwrap();
    let path = file_db(&dir);
    let setup = HitlService::new(Arc::new(Database::open(&path).unwrap()));

    for round in 0..5 {
        let work_id = running_item(setup.database(), &format!("r{round}"));
        let hitl_id = open(&setup, &work_id);
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = [("cli", HitlAction::Done), ("github", HitlAction::Skip)]
            .into_iter()
            .map(|(via, action)| {
                let path = path.clone();
                let hitl_id = hitl_id.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let service = HitlService::new(Arc::new(Database::open(&path).unwrap()));
                    barrier.wait();
                    service
                        .respond(&response(HitlTarget::Id(hitl_id), action, via))
                        .unwrap()
                })
            })
            .collect();
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let won = outcomes
            .iter()
            .filter(|o| matches!(o, RespondOutcome::Won { .. }))
            .count();
        let lost = outcomes
            .iter()
            .filter(|o| matches!(o, RespondOutcome::AlreadyHandled(_)))
            .count();
        assert_eq!((won, lost), (1, 1), "round {round}: {outcomes:?}");

        let logged = rejections(setup.database(), &work_id);
        assert_eq!(logged.len(), 1, "round {round}: one rejected response");
        assert_eq!(logged[0].0.as_deref(), Some("already_handled"));
    }
}

#[test]
fn expire_and_respond_race_has_one_winner() {
    let dir = TempDir::new().unwrap();
    let path = file_db(&dir);
    let setup = HitlService::new(Arc::new(Database::open(&path).unwrap()));

    for round in 0..5 {
        let work_id = running_item(setup.database(), &format!("e{round}"));
        let hitl_id = open(&setup, &work_id);
        let barrier = Arc::new(Barrier::new(2));

        let responder = {
            let (path, hitl_id, barrier) = (path.clone(), hitl_id.clone(), Arc::clone(&barrier));
            std::thread::spawn(move || {
                let service = HitlService::new(Arc::new(Database::open(&path).unwrap()));
                barrier.wait();
                service
                    .respond(&response(HitlTarget::Id(hitl_id), HitlAction::Retry, "cli"))
                    .unwrap()
            })
        };
        let expirer = {
            let (path, hitl_id, barrier) = (path.clone(), hitl_id.clone(), Arc::clone(&barrier));
            std::thread::spawn(move || {
                let service = HitlService::new(Arc::new(Database::open(&path).unwrap()));
                barrier.wait();
                service.expire(&hitl_id, EscalationAction::Skip).unwrap()
            })
        };
        let responded = responder.join().unwrap();
        let expired = expirer.join().unwrap();

        let stored = setup.database().hitl_request(&hitl_id).unwrap().unwrap();
        match (&responded, &expired) {
            (RespondOutcome::Won { .. }, RespondOutcome::AlreadyHandled(winner)) => {
                assert_eq!(stored.status, HitlStatus::Resolved, "round {round}");
                assert_eq!(winner.action, HitlAction::Retry);
                assert!(rejections(setup.database(), &work_id).is_empty());
            }
            (RespondOutcome::AlreadyHandled(winner), RespondOutcome::Won { .. }) => {
                assert_eq!(stored.status, HitlStatus::Expired, "round {round}");
                assert_eq!(winner.via, "timeout");
                assert_eq!(rejections(setup.database(), &work_id).len(), 1);
            }
            other => panic!("round {round}: exactly one side wins, got {other:?}"),
        }
    }
}

#[test]
fn a_won_response_keeps_the_item_in_hitl_for_post_processing() {
    let service = HitlService::new(Arc::new(Database::open_in_memory().unwrap()));
    let work_id = running_item(service.database(), "1");
    let hitl_id = open(&service, &work_id);

    let outcome = service
        .respond(&response(
            HitlTarget::Item(work_id.clone()),
            HitlAction::Done,
            "cli",
        ))
        .unwrap();
    assert_eq!(
        outcome,
        RespondOutcome::Won {
            hitl_id: hitl_id.clone()
        }
    );

    let item = service.database().get_item(&work_id).unwrap();
    assert_eq!(item.phase(), QueuePhase::Hitl);
    assert_eq!(
        service.database().pending_post_processing().unwrap().len(),
        1,
        "the confirmed request waits for the daemon"
    );
}

#[test]
fn a_previous_request_id_never_closes_the_reopened_request() {
    let service = HitlService::new(Arc::new(Database::open_in_memory().unwrap()));
    let db = service.database();
    let work_id = running_item(db, "1");
    let first = open(&service, &work_id);
    service
        .respond(&response(
            HitlTarget::Id(first.clone()),
            HitlAction::Retry,
            "cli",
        ))
        .unwrap();
    db.complete_post_processing(
        &first,
        &TransitionRequest {
            work_id: work_id.clone(),
            expected_from: QueuePhase::Hitl,
            to: QueuePhase::Pending,
            actor: Actor::Daemon,
            reason: TransitionReason::PostProcessing(HitlAction::Retry),
            detail: None,
        },
    )
    .unwrap();
    move_item(db, &work_id, QueuePhase::Pending, QueuePhase::Ready);
    move_item(db, &work_id, QueuePhase::Ready, QueuePhase::Running);
    let second = open(&service, &work_id);
    assert_ne!(first, second);

    let stale = service
        .respond(&response(
            HitlTarget::Id(first.clone()),
            HitlAction::Skip,
            "github",
        ))
        .unwrap();
    assert!(
        matches!(&stale, RespondOutcome::AlreadyHandled(r) if r.action == HitlAction::Retry),
        "{stale:?}"
    );
    assert_eq!(
        db.hitl_request(&second).unwrap().unwrap().status,
        HitlStatus::Open
    );
    assert_eq!(rejections(db, &work_id).len(), 1);

    let fresh = service
        .respond(&response(
            HitlTarget::Id(second.clone()),
            HitlAction::Skip,
            "cli",
        ))
        .unwrap();
    assert_eq!(fresh, RespondOutcome::Won { hitl_id: second });
}

#[test]
fn unauthorized_responses_are_recorded_as_rejected() {
    let service = HitlService::new(Arc::new(Database::open_in_memory().unwrap()));
    let work_id = running_item(service.database(), "1");
    let hitl_id = open(&service, &work_id);

    service
        .reject_unauthorized(&HitlTarget::Id(hitl_id.clone()), "mallory", "github")
        .unwrap();

    let logged = rejections(service.database(), &work_id);
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0].0.as_deref(), Some("unauthorized"));
    assert_eq!(
        service
            .database()
            .hitl_request(&hitl_id)
            .unwrap()
            .unwrap()
            .status,
        HitlStatus::Open,
        "a rejected response leaves the request open"
    );
}

#[test]
fn due_requests_are_the_open_ones_past_their_deadline() {
    let service = HitlService::new(Arc::new(Database::open_in_memory().unwrap()));
    let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
    let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let mut ids = Vec::new();
    for (source, timeout_at) in [("a", Some(past)), ("b", Some(future)), ("c", None)] {
        let work_id = running_item(service.database(), source);
        let mut req = open_request(&work_id);
        req.timeout_at = timeout_at;
        req.terminal_action = Some(EscalationAction::Skip);
        let OpenHitlOutcome::Opened { hitl_id, .. } = service.open(&req).unwrap() else {
            panic!("expected Opened");
        };
        ids.push(hitl_id);
    }

    let due = service.due_for_expiry(chrono::Utc::now()).unwrap();
    assert_eq!(
        due.iter().map(|r| r.hitl_id.clone()).collect::<Vec<_>>(),
        vec![ids[0].clone()]
    );
    assert_eq!(service.open_requests().unwrap().len(), 3);
}

// ---- on_hitl_opened -------------------------------------------------------

struct OpenedHook {
    opened: Mutex<Vec<String>>,
    fail: bool,
}

impl OpenedHook {
    fn new(fail: bool) -> Arc<Self> {
        Arc::new(Self {
            opened: Mutex::new(Vec::new()),
            fail,
        })
    }

    fn opened(&self) -> Vec<String> {
        self.opened.lock().unwrap().clone()
    }
}

#[async_trait]
impl LifecycleHook for OpenedHook {
    async fn on_enter(&self, _ctx: &HookContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn on_done(&self, _ctx: &HookContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn on_fail(&self, _ctx: &HookContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn on_escalation(
        &self,
        _ctx: &HookContext,
        _action: EscalationAction,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn on_hitl_opened(&self, ctx: &HookContext) -> anyhow::Result<()> {
        self.opened.lock().unwrap().push(ctx.work_id.clone());
        if self.fail {
            anyhow::bail!("label api down");
        }
        Ok(())
    }
}

fn workspace_config() -> WorkspaceConfig {
    let yaml = r#"
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
      terminal: replan
"#;
    serde_yaml::from_str(yaml).unwrap()
}

fn daemon(tmp: &TempDir, source: MockDataSource, exit_codes: Vec<i32>) -> Daemon {
    let mut registry = RuntimeRegistry::new("mock".to_string());
    registry.register(Arc::new(MockRuntime::new("mock", exit_codes)));
    Daemon::new(
        workspace_config(),
        vec![Box::new(source)],
        Arc::new(registry),
        Box::new(MockWorktreeManager::new(tmp.path().to_path_buf())),
        4,
        Database::open_in_memory().unwrap(),
    )
}

#[tokio::test]
async fn on_hitl_opened_runs_once_per_open_request() {
    let tmp = TempDir::new().unwrap();
    let hook = OpenedHook::new(false);
    let mut daemon = daemon(&tmp, MockDataSource::new("github"), vec![]).with_hook(hook.clone());
    let service = HitlService::new(Arc::clone(daemon.database()));

    let first = running_item(daemon.db(), "1");
    let second = running_item(daemon.db(), "2");
    open(&service, &first);
    open(&service, &second);

    assert_eq!(daemon.observe_hitl_opened().await.unwrap(), 2);
    assert_eq!(hook.opened(), vec![first.clone(), second.clone()]);

    assert_eq!(daemon.observe_hitl_opened().await.unwrap(), 0);
    assert_eq!(hook.opened().len(), 2, "re-observing does not call again");

    // A request opened later is observed on its own.
    let third = running_item(daemon.db(), "3");
    open(&service, &third);
    assert_eq!(daemon.observe_hitl_opened().await.unwrap(), 1);
    assert_eq!(hook.opened(), vec![first, second, third]);
}

#[tokio::test]
async fn on_hitl_opened_skips_requests_confirmed_before_observation() {
    let tmp = TempDir::new().unwrap();
    let hook = OpenedHook::new(false);
    let mut daemon = daemon(&tmp, MockDataSource::new("github"), vec![]).with_hook(hook.clone());
    let service = HitlService::new(Arc::clone(daemon.database()));

    let work_id = running_item(daemon.db(), "1");
    let hitl_id = open(&service, &work_id);
    service
        .respond(&response(HitlTarget::Id(hitl_id), HitlAction::Skip, "cli"))
        .unwrap();

    assert_eq!(daemon.observe_hitl_opened().await.unwrap(), 0);
    assert!(hook.opened().is_empty());
}

#[tokio::test]
async fn a_failing_on_hitl_opened_keeps_the_transition_and_is_not_retried() {
    let tmp = TempDir::new().unwrap();
    let hook = OpenedHook::new(true);
    let mut daemon = daemon(&tmp, MockDataSource::new("github"), vec![]).with_hook(hook.clone());
    let service = HitlService::new(Arc::clone(daemon.database()));

    let work_id = running_item(daemon.db(), "1");
    let hitl_id = open(&service, &work_id);

    daemon.observe_hitl_opened().await.unwrap();
    daemon.observe_hitl_opened().await.unwrap();

    assert_eq!(hook.opened().len(), 1, "no retry after a failure");
    assert_eq!(
        daemon.db().get_item(&work_id).unwrap().phase(),
        QueuePhase::Hitl
    );
    assert_eq!(
        daemon.db().hitl_request(&hitl_id).unwrap().unwrap().status,
        HitlStatus::Open
    );
}

// ---- daemon-opened requests carry their expiry --------------------------------

fn assert_expiry(daemon: &Daemon) {
    let open = HitlService::new(Arc::clone(daemon.database()))
        .open_requests()
        .unwrap();
    assert_eq!(open.len(), 1);
    let request = &open[0];
    assert_eq!(
        request.terminal_action,
        Some(EscalationAction::Replan),
        "the workspace terminal"
    );
    let timeout_at = chrono::DateTime::parse_from_rfc3339(
        request.timeout_at.as_deref().expect("timeout_at is set"),
    )
    .unwrap()
    .with_timezone(&chrono::Utc);
    let until = timeout_at - chrono::Utc::now();
    assert!(
        until > chrono::Duration::hours(23) && until <= chrono::Duration::hours(24),
        "the default HITL timeout applies, got {until}"
    );
}

#[tokio::test]
async fn escalation_hitl_carries_timeout_and_terminal() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));
    let mut daemon = daemon(&tmp, source, vec![1, 1, 1]);

    daemon.collect().await.unwrap();
    for _ in 0..3 {
        daemon.advance();
        daemon.execute_running().await;
    }
    assert_expiry(&daemon);
}

#[tokio::test]
async fn evaluate_hitl_carries_timeout_and_terminal() {
    let tmp = TempDir::new().unwrap();
    let mut source = MockDataSource::new("github");
    source.add_item(test_item("github:org/repo#1", "analyze"));
    let mut daemon = daemon(&tmp, source, vec![0]);

    daemon.collect().await.unwrap();
    daemon.advance();
    daemon.execute_running().await;
    let work_id = daemon
        .items_in_phase(QueuePhase::Completed)
        .first()
        .map(|i| i.work_id.clone())
        .expect("handler success leaves a Completed item");
    daemon
        .mark_hitl(&work_id, HitlReason::EvaluateFailure, None)
        .unwrap();

    assert_expiry(&daemon);
}

#[test]
fn expiry_terms_start_after_the_timeout() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-07T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let expiry = HitlExpiry::after_hours(24, Some(EscalationAction::Skip), now);
    assert_eq!(
        expiry.timeout_at.as_deref(),
        Some("2026-10-08T00:00:00+00:00")
    );
    assert_eq!(expiry.terminal_action, Some(EscalationAction::Skip));
}
