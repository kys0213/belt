//! Running-item cancellation.
//!
//! A cancel request never changes a phase by itself; whoever owns the item
//! acts on it:
//!
//! | path | who | steps |
//! |---|---|---|
//! | daemon | the daemon that runs the handler | accept → stop the handler group → Running→Skipped (`canceled`) → close `canceled` |
//! | daemon, not yet spawned | the daemon, before the handler spawns | accept → Running→Skipped without spawning → close `canceled` |
//! | restart | the starting daemon | Running→Skipped → close `canceled`; an item no longer Running closes `too_late` |
//! | direct | CLI/TUI when no daemon answers | [`cancel_directly`] |
//!
//! [`HandlerControl`] ties a running execution to its process group: it is
//! the [`ProcessSink`] the handler reports its pid to, and the switch a
//! cancel or a shutdown flips. Both go through one lock, so a stop that
//! arrives before the pid is reported kills the process as soon as it is.
//!
//! The controls live in [`InFlight`], shared with the IPC wake task: a
//! cancel wake is accepted and its handler stopped by
//! [`accept_in_flight_cancels`] even while a tick is busy (an evaluator
//! call, a hook). The item's result transition stays with the tick loop,
//! which applies the stopped execution when its handler returns.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use belt_core::error::BeltError;
use belt_core::platform::{ProcessKiller, ProcessSink};
use belt_core::transition::Actor;
use belt_infra::db::{
    CancelRequestRecord, CancelStatus, Database, DirectCancelOutcome, HandlerProcess,
};
use belt_infra::platform::{HandlerProbe, probe_handler};

/// Why a running execution is being stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopReason {
    /// The cancel request `request_id` was accepted for it.
    Cancel { request_id: i64 },
    /// The daemon shuts down past its drain timeout.
    Shutdown,
    /// The stored row left Running without this daemon (the direct cancel
    /// path): the execution no longer owns the item.
    Superseded,
}

#[derive(Debug, Default)]
struct ControlState {
    /// The process the execution runs now; cleared as soon as it exits.
    pid: Option<u32>,
    stop: Option<StopReason>,
    /// The execution returned; its processes have all been waited for.
    finished: bool,
}

/// The process-group handle of one in-flight execution.
pub(crate) struct HandlerControl {
    work_id: String,
    db: Arc<Database>,
    killer: Arc<dyn ProcessKiller>,
    state: Mutex<ControlState>,
}

impl HandlerControl {
    pub(crate) fn new(work_id: &str, db: Arc<Database>, killer: Arc<dyn ProcessKiller>) -> Self {
        Self {
            work_id: work_id.to_string(),
            db,
            killer,
            state: Mutex::new(ControlState::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ControlState> {
        // A panic while holding the lock leaves plain data behind; the
        // state stays meaningful, so keep using it.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stop the execution: kill its process group now, or as soon as a
    /// process is reported. Returns `false` when it was already stopping or
    /// already returned: a finished execution's result stands, so the stop
    /// is not recorded.
    pub(crate) fn stop(&self, reason: StopReason) -> bool {
        let mut state = self.lock();
        if state.stop.is_some() || state.finished {
            return false;
        }
        state.stop = Some(reason);
        if let Some(pid) = state.pid {
            self.kill(pid);
        }
        true
    }

    /// Accept `request` for this execution and stop it, as one step under
    /// the control's lock: a finishing execution either sees the stop or
    /// refuses the accept, so an accepted request always ends `canceled`.
    ///
    /// Returns `false`, accepting nothing, when the execution already
    /// returned or is stopping for another reason, or when the request can
    /// no longer be accepted (closed meanwhile, or the store failed).
    pub(crate) fn cancel(&self, request: &CancelRequestRecord) -> bool {
        let mut state = self.lock();
        if state.finished || state.stop.is_some() || !accept_request(&self.db, request) {
            return false;
        }
        state.stop = Some(StopReason::Cancel {
            request_id: request.id,
        });
        if let Some(pid) = state.pid {
            self.kill(pid);
        }
        true
    }

    /// Whether a stop was requested; the execution must not start new steps.
    pub(crate) fn is_stopped(&self) -> bool {
        self.lock().stop.is_some()
    }

    /// Why the execution was stopped, if it was.
    pub(crate) fn stop_reason(&self) -> Option<StopReason> {
        self.lock().stop
    }

    /// The accepted cancel request, when the execution was canceled.
    pub(crate) fn canceled_request(&self) -> Option<i64> {
        match self.stop_reason() {
            Some(StopReason::Cancel { request_id }) => Some(request_id),
            Some(StopReason::Shutdown | StopReason::Superseded) | None => None,
        }
    }

    /// The execution returned: its pid may be reused from now on, so a
    /// later stop must not signal it.
    pub(crate) fn finish(&self) {
        let mut state = self.lock();
        state.finished = true;
        state.pid = None;
    }

    fn kill(&self, pid: u32) {
        match self.killer.kill_group(pid) {
            Ok(()) => tracing::info!(work_id = %self.work_id, pid, "handler process group killed"),
            Err(e) => {
                tracing::warn!(work_id = %self.work_id, pid, "handler process group kill failed: {e}")
            }
        }
    }
}

impl ProcessSink for HandlerControl {
    fn spawned(&self, pid: u32) {
        let mut state = self.lock();
        state.pid = Some(pid);
        match self.db.set_handler_process(&self.work_id, pid) {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                work_id = %self.work_id,
                pid,
                "handler pid not recorded: the item is no longer Running"
            ),
            Err(e) => {
                tracing::error!(work_id = %self.work_id, pid, "handler pid not recorded: {e}")
            }
        }
        if state.stop.is_some() {
            self.kill(pid);
        }
    }

    fn exited(&self, pid: u32) {
        let mut state = self.lock();
        // Only the current process: an older one's exit must not erase it.
        if state.pid != Some(pid) {
            return;
        }
        state.pid = None;
        if let Err(e) = self.db.clear_handler_process(&self.work_id) {
            tracing::error!(work_id = %self.work_id, pid, "exited handler pid not cleared: {e}");
        }
    }
}

/// Mark `request` accepted by the daemon. `false` when it can no longer be
/// acted on (closed meanwhile, or the store failed).
pub(crate) fn accept_request(db: &Database, request: &CancelRequestRecord) -> bool {
    match request.status {
        CancelStatus::Accepted => return true,
        CancelStatus::Closed => return false,
        CancelStatus::Requested => {}
    }
    match db.accept_cancel(request.id, &Actor::Daemon) {
        Ok(accepted) => accepted,
        Err(e) => {
            tracing::error!(work_id = %request.work_id, "cancel request not accepted: {e}");
            false
        }
    }
}

/// The controls of the executions in flight, by work_id.
///
/// Owned by the daemon and shared with the IPC wake task; every method
/// takes the lock for its own duration only.
#[derive(Default)]
pub(crate) struct InFlight {
    controls: Mutex<HashMap<String, Arc<HandlerControl>>>,
}

impl InFlight {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Arc<HandlerControl>>> {
        // The map holds plain handles; a panic elsewhere leaves it usable.
        self.controls.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn insert(&self, work_id: &str, control: Arc<HandlerControl>) {
        self.lock().insert(work_id.to_string(), control);
    }

    pub(crate) fn remove(&self, work_id: &str) -> Option<Arc<HandlerControl>> {
        self.lock().remove(work_id)
    }

    pub(crate) fn get(&self, work_id: &str) -> Option<Arc<HandlerControl>> {
        self.lock().get(work_id).cloned()
    }

    pub(crate) fn contains(&self, work_id: &str) -> bool {
        self.lock().contains_key(work_id)
    }

    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Stop every execution in flight.
    pub(crate) fn stop_all(&self, reason: StopReason) {
        for control in self.lock().values() {
            control.stop(reason);
        }
    }

    /// Take every control out.
    pub(crate) fn drain(&self) -> Vec<(String, Arc<HandlerControl>)> {
        self.lock().drain().collect()
    }
}

/// Accept the open cancel requests of the executions in `in_flight` and
/// stop their handlers, without waiting for a tick.
///
/// Requests for anything else (an item not Running, a Running item with no
/// handler in flight) are left to the next tick's cancel step. Returns how
/// many executions were stopped.
///
/// # Errors
/// When the open requests cannot be listed.
pub(crate) fn accept_in_flight_cancels(
    db: &Database,
    in_flight: &InFlight,
) -> Result<usize, BeltError> {
    let mut stopped = 0;
    for request in db.open_cancel_requests()? {
        let Some(control) = in_flight.get(&request.work_id) else {
            continue;
        };
        if control.canceled_request().is_none() && control.cancel(&request) {
            tracing::info!(work_id = %request.work_id, request_id = request.id, "cancel accepted, handler stopped");
            stopped += 1;
        }
    }
    Ok(stopped)
}

/// What happened to a handler process a previous daemon left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeftoverHandler {
    /// It was the recorded handler and its group was killed (or, with the
    /// leader already gone, what was left of the group).
    Killed,
    /// It had already exited and left nothing in its group.
    Gone,
    /// It was not killed; the reason says why (a reused pid, an unverifiable
    /// platform, or a failed kill).
    Spared(String),
}

/// Stop the recorded handler of a Running item whose daemon is gone.
///
/// The pid is killed only after [`probe_handler`] confirms it still names
/// that handler; a pid that may have been reused is left alone and the
/// reason is logged.
pub fn stop_leftover_handler(
    killer: &dyn ProcessKiller,
    work_id: &str,
    handler: &HandlerProcess,
) -> LeftoverHandler {
    let pid = handler.pid;
    let outcome = match probe_handler(pid, handler.running_since) {
        HandlerProbe::Handler => match killer.kill_group(pid) {
            Ok(()) => LeftoverHandler::Killed,
            Err(e) => LeftoverHandler::Spared(format!("kill failed: {e}")),
        },
        // The leader is gone but its children may still hold the group.
        // The group id cannot be handed to a new process while any member
        // lives, so the kill reaches only them; with none left it fails.
        HandlerProbe::Gone => match killer.kill_group(pid) {
            Ok(()) => LeftoverHandler::Killed,
            Err(_) => LeftoverHandler::Gone,
        },
        HandlerProbe::Reused(reason) | HandlerProbe::Unknown(reason) => {
            LeftoverHandler::Spared(reason)
        }
    };
    match &outcome {
        LeftoverHandler::Killed => {
            tracing::info!(work_id, pid, "leftover handler process group killed")
        }
        LeftoverHandler::Gone => tracing::debug!(work_id, pid, "leftover handler already exited"),
        LeftoverHandler::Spared(reason) => {
            tracing::warn!(work_id, pid, "leftover handler not killed: {reason}")
        }
    }
    outcome
}

/// Cancel the item of the open request `request_id` without a daemon.
///
/// The direct path, for when no daemon owns the item (absent or not
/// answering): the store moves a Running item to Skipped and closes the
/// request `canceled_directly` in one transaction
/// ([`Database::cancel_directly`]); then the handler process recorded
/// before the move is stopped, if it still is that handler. An item that
/// already left Running closes the request `too_late`.
///
/// # Errors
/// `BeltError` when the store cannot apply the cancel.
pub fn cancel_directly(
    db: &Database,
    killer: &dyn ProcessKiller,
    request_id: i64,
    actor: &Actor,
) -> Result<DirectCancelOutcome, BeltError> {
    let outcome = db.cancel_directly(request_id, actor)?;
    if let DirectCancelOutcome::Canceled {
        handler: Some(handler),
    } = &outcome
    {
        stop_leftover_handler(killer, &format!("cancel request {request_id}"), handler);
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::phase::QueuePhase;
    use belt_core::queue::QueueItem;

    /// Records every group it is asked to kill.
    #[derive(Default)]
    struct RecordingKiller {
        killed: Mutex<Vec<u32>>,
    }

    impl RecordingKiller {
        fn killed(&self) -> Vec<u32> {
            self.killed.lock().unwrap().clone()
        }
    }

    impl ProcessKiller for RecordingKiller {
        fn kill_group(&self, pid: u32) -> Result<(), BeltError> {
            self.killed.lock().unwrap().push(pid);
            Ok(())
        }
    }

    const WORK_ID: &str = "w-1";

    fn control() -> (HandlerControl, Arc<RecordingKiller>, Arc<Database>) {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let mut item = QueueItem::new(
            WORK_ID.to_string(),
            "github:org/repo#1".to_string(),
            "ws".to_string(),
            "implement".to_string(),
        );
        item.set_phase_unchecked(QueuePhase::Running);
        db.insert_item(&item).unwrap();
        let killer = Arc::new(RecordingKiller::default());
        let control = HandlerControl::new(
            WORK_ID,
            Arc::clone(&db),
            Arc::clone(&killer) as Arc<dyn ProcessKiller>,
        );
        (control, killer, db)
    }

    const CANCEL: StopReason = StopReason::Cancel { request_id: 7 };

    #[test]
    fn a_stop_before_the_spawn_kills_the_process_as_soon_as_it_is_reported() {
        let (control, killer, _db) = control();

        assert!(control.stop(CANCEL));
        assert!(killer.killed().is_empty(), "nothing to kill yet");
        control.spawned(42);

        assert_eq!(killer.killed(), vec![42]);
        assert_eq!(control.canceled_request(), Some(7));
    }

    #[test]
    fn a_stop_while_a_process_runs_kills_it_once() {
        let (control, killer, _db) = control();
        control.spawned(42);

        assert!(control.stop(CANCEL));
        assert!(!control.stop(StopReason::Shutdown), "already stopping");

        assert_eq!(killer.killed(), vec![42]);
        assert_eq!(control.canceled_request(), Some(7));
    }

    #[test]
    fn a_stop_after_the_execution_finished_is_refused() {
        let (control, killer, _db) = control();
        control.spawned(42);
        control.finish();

        assert!(!control.stop(CANCEL), "the result already stands");

        assert!(killer.killed().is_empty());
        assert_eq!(control.canceled_request(), None);
        assert!(!control.is_stopped());
    }

    #[test]
    fn an_exited_process_is_forgotten_in_memory_and_in_the_store() {
        let (control, killer, db) = control();
        control.spawned(42);
        assert_eq!(db.handler_process(WORK_ID).unwrap(), Some(42));

        control.exited(42);

        assert_eq!(db.handler_process(WORK_ID).unwrap(), None);
        assert!(control.stop(CANCEL), "the execution itself still runs");
        assert!(
            killer.killed().is_empty(),
            "an exited pid may be reused and is never signaled"
        );
    }

    #[test]
    fn the_exit_of_an_older_process_keeps_the_current_one() {
        let (control, killer, db) = control();
        control.spawned(42);
        control.spawned(43);

        control.exited(42);

        assert_eq!(db.handler_process(WORK_ID).unwrap(), Some(43));
        control.stop(CANCEL);
        assert_eq!(killer.killed(), vec![43]);
    }

    /// A handler whose leader exited but left a child in its group.
    #[cfg(unix)]
    #[test]
    fn a_leftover_group_without_its_leader_is_still_killed() {
        use std::os::unix::process::CommandExt;

        let alive = |pid: u32| {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let output = std::process::Command::new("bash")
            .args(["-c", "sleep 30 >/dev/null 2>&1 & echo $!"])
            .process_group(0)
            .output()
            .unwrap();
        let child: u32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap();
        // The leader was reaped by `output`; only its child holds the group.
        let ps = std::process::Command::new("ps")
            .args(["-o", "pgid=", "-p", &child.to_string()])
            .output()
            .unwrap();
        let leader = HandlerProcess {
            pid: String::from_utf8_lossy(&ps.stdout).trim().parse().unwrap(),
            running_since: chrono::Utc::now() - chrono::Duration::seconds(5),
        };
        assert!(alive(child));
        let killer = belt_infra::platform::default_process_killer();

        let outcome = stop_leftover_handler(killer.as_ref(), WORK_ID, &leader);

        let mut gone = false;
        for _ in 0..50 {
            if !alive(child) {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(gone, "the child left in the group is killed");
        assert_eq!(outcome, LeftoverHandler::Killed);
    }
}
