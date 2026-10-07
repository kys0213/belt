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

use std::sync::{Arc, Mutex, MutexGuard};

use belt_core::error::BeltError;
use belt_core::platform::{ProcessKiller, ProcessSink};
use belt_core::transition::Actor;
use belt_infra::db::{Database, DirectCancelOutcome, HandlerProcess};
use belt_infra::platform::{HandlerProbe, probe_handler};

/// Why a running execution is being stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopReason {
    /// The cancel request `request_id` was accepted for it.
    Cancel { request_id: i64 },
    /// The daemon shuts down past its drain timeout.
    Shutdown,
}

#[derive(Debug, Default)]
struct ControlState {
    /// The last process the execution spawned, until the execution ends.
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
    /// process is reported. Returns `false` when it was already stopping.
    pub(crate) fn stop(&self, reason: StopReason) -> bool {
        let mut state = self.lock();
        if state.stop.is_some() {
            return false;
        }
        state.stop = Some(reason);
        if !state.finished
            && let Some(pid) = state.pid
        {
            self.kill(pid);
        }
        true
    }

    /// Whether a stop was requested; the execution must not start new steps.
    pub(crate) fn is_stopped(&self) -> bool {
        self.lock().stop.is_some()
    }

    /// The accepted cancel request, when the execution was canceled.
    pub(crate) fn canceled_request(&self) -> Option<i64> {
        match self.lock().stop {
            Some(StopReason::Cancel { request_id }) => Some(request_id),
            Some(StopReason::Shutdown) | None => None,
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
}

/// What happened to a handler process a previous daemon left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeftoverHandler {
    /// It was the recorded handler and its group was killed.
    Killed,
    /// It had already exited.
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
        HandlerProbe::Gone => LeftoverHandler::Gone,
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
