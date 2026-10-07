//! Cancel flow of a Running item, shared by `belt queue skip` and the
//! dashboard `x` key.
//!
//! The request is recorded first; then either a live daemon is woken and
//! given [`CANCEL_WAIT_LIMIT`] to close it, or -- when no daemon is there or
//! it never accepts -- the request is carried out directly.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::bail;
use belt_core::phase::QueuePhase;
use belt_core::platform::ProcessKiller;
use belt_core::transition::Actor;
use belt_infra::db::{Database, DirectCancelOutcome, RequestCancelOutcome, transition_kind};
use belt_infra::ipc::{DaemonSignal, notify_daemon};

/// How long a caller waits for the daemon to close a request it was woken for.
pub const CANCEL_WAIT_LIMIT: Duration = Duration::from_secs(10);
/// Interval between checks of the request while waiting.
pub const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The link to a daemon that may be running.
pub trait DaemonLink {
    /// Wake the daemon to handle open cancel requests now. `false` when no
    /// daemon is there to answer.
    fn wake(&self) -> bool;
}

/// The daemon of a `BELT_HOME`: alive when its pid file names a live process,
/// woken over the IPC port.
pub struct LocalDaemon {
    pub belt_home: PathBuf,
}

impl DaemonLink for LocalDaemon {
    fn wake(&self) -> bool {
        let Some(pid) = std::fs::read_to_string(self.belt_home.join("daemon.pid"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        else {
            return false;
        };
        process_alive(pid) && notify_daemon(&self.belt_home, DaemonSignal::CancelRequested).is_ok()
    }
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

/// How a cancel of a Running item ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The daemon stopped the handler and moved the item to Skipped.
    Canceled,
    /// No daemon answered; the item was moved to Skipped directly.
    CanceledDirectly,
    /// The daemon accepted the request but did not close it in time.
    Accepted,
    /// The item had already left Running; `current` is its phase now.
    TooLate { current: QueuePhase },
}

impl CancelOutcome {
    /// The `result` value of the CLI contract.
    pub fn result(&self) -> &'static str {
        match self {
            Self::Canceled => "canceled",
            Self::CanceledDirectly => "canceled_directly",
            Self::Accepted => "accepted",
            Self::TooLate { .. } => "too_late",
        }
    }
}

/// Everything a cancel needs besides the item.
pub struct CancelFlow<'a> {
    pub db: &'a Database,
    pub daemon: &'a dyn DaemonLink,
    pub killer: &'a dyn ProcessKiller,
    /// The path the request comes through: [`Actor::Cli`] or [`Actor::Tui`].
    pub actor: Actor,
    /// Who asked, recorded on the request.
    pub requester: String,
    pub wait_limit: Duration,
    pub poll_interval: Duration,
}

impl CancelFlow<'_> {
    /// Cancel the Running item `work_id`.
    ///
    /// # Errors
    /// Store failures, and a request that closed with a result this flow
    /// does not know.
    pub fn cancel(&self, work_id: &str) -> anyhow::Result<CancelOutcome> {
        let baseline = self.last_closed_seq(work_id)?;
        let request_id = match self
            .db
            .request_cancel(work_id, &self.requester, &self.actor)?
        {
            RequestCancelOutcome::Opened { id } => id,
            RequestCancelOutcome::Existing(record) => record.id,
        };

        if self.daemon.wake() {
            let deadline = Instant::now() + self.wait_limit;
            let accepted = loop {
                let accepted = match self.db.open_cancel_request(work_id)? {
                    Some(open) if open.id == request_id => {
                        open.status == belt_infra::db::CancelStatus::Accepted
                    }
                    _ => return self.closed(work_id, baseline),
                };
                if Instant::now() >= deadline {
                    break accepted;
                }
                std::thread::sleep(self.poll_interval);
            };
            if accepted {
                return Ok(CancelOutcome::Accepted);
            }
        }

        match belt_daemon::cancel::cancel_directly(self.db, self.killer, request_id, &self.actor)? {
            DirectCancelOutcome::Canceled { .. } => Ok(CancelOutcome::CanceledDirectly),
            DirectCancelOutcome::TooLate { current } => Ok(CancelOutcome::TooLate { current }),
            // The daemon closed it between the last check and the direct try.
            DirectCancelOutcome::NotOpen => self.closed(work_id, baseline),
        }
    }

    /// Sequence number of the newest closed request of `work_id`.
    fn last_closed_seq(&self, work_id: &str) -> anyhow::Result<u64> {
        Ok(self
            .db
            .transitions_of(work_id)?
            .iter()
            .filter(|e| e.kind == transition_kind::CANCEL_CLOSED)
            .map(|e| e.seq)
            .max()
            .unwrap_or(0))
    }

    /// The outcome of the request closed after `baseline`.
    fn closed(&self, work_id: &str, baseline: u64) -> anyhow::Result<CancelOutcome> {
        let result = self
            .db
            .transitions_of(work_id)?
            .into_iter()
            .find(|e| e.kind == transition_kind::CANCEL_CLOSED && e.seq > baseline)
            .and_then(|e| e.reason);
        match result.as_deref() {
            Some("canceled") => Ok(CancelOutcome::Canceled),
            Some("canceled_directly") => Ok(CancelOutcome::CanceledDirectly),
            Some("too_late") => Ok(CancelOutcome::TooLate {
                current: self.db.get_item(work_id)?.phase(),
            }),
            Some(other) => {
                bail!("cancel request of {work_id} closed with unknown result '{other}'")
            }
            None => bail!("cancel request of {work_id} is closed but its result was not logged"),
        }
    }
}

/// Doubles shared by the tests of the cancel flow and its callers.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use belt_core::transition::{TransitionReason, TransitionRequest};
    use belt_infra::db::{CollectOutcome, NewItem};

    /// No daemon is there to wake.
    pub struct NoDaemon;
    impl DaemonLink for NoDaemon {
        fn wake(&self) -> bool {
            false
        }
    }

    /// Kills nothing.
    pub struct NoKill;
    impl ProcessKiller for NoKill {
        fn kill_group(&self, _pid: u32) -> Result<(), belt_core::error::BeltError> {
            Ok(())
        }
    }

    /// A new item walked to Running.
    pub fn running_item(db: &Database) -> String {
        let CollectOutcome::Inserted { work_id } = db
            .insert_collected(&NewItem {
                source_id: "github:org/repo#1".to_string(),
                workspace_id: "ws".to_string(),
                state: "implement".to_string(),
                title: None,
                actor: Actor::Daemon,
            })
            .unwrap()
        else {
            panic!("expected a new item");
        };
        let mut from = QueuePhase::Pending;
        for next in [QueuePhase::Ready, QueuePhase::Running] {
            db.transition(&TransitionRequest {
                work_id: work_id.clone(),
                expected_from: from,
                to: next,
                actor: Actor::Daemon,
                reason: TransitionReason::Manual,
                detail: None,
            })
            .unwrap();
            from = next;
        }
        work_id
    }

    /// A flow with a short wait limit.
    pub fn flow<'a>(
        db: &'a Database,
        daemon: &'a dyn DaemonLink,
        killer: &'a dyn ProcessKiller,
    ) -> CancelFlow<'a> {
        CancelFlow {
            db,
            daemon,
            killer,
            actor: Actor::Cli,
            requester: "alice".to_string(),
            wait_limit: Duration::from_millis(300),
            poll_interval: Duration::from_millis(10),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use std::sync::Arc;

    use belt_core::transition::{TransitionReason, TransitionRequest};
    use belt_infra::db::CancelResult;

    #[test]
    fn without_a_daemon_the_request_is_carried_out_directly() {
        let db = Database::open_in_memory().unwrap();
        let id = running_item(&db);

        let outcome = flow(&db, &NoDaemon, &NoKill).cancel(&id).unwrap();

        assert_eq!(outcome, CancelOutcome::CanceledDirectly);
        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Skipped);
    }

    /// What a woken daemon double does with the open request.
    #[derive(Clone, Copy)]
    enum Reaction {
        /// Never touches the request.
        Silent,
        /// Accepts it and stops there.
        AcceptOnly,
        /// Accepts it, moves the item, closes it `canceled`.
        Cancel,
        /// Closes it `too_late` after the handler finished first.
        TooLate,
    }

    struct FakeDaemon {
        db: Arc<Database>,
        reaction: Reaction,
    }

    impl DaemonLink for FakeDaemon {
        fn wake(&self) -> bool {
            let db = Arc::clone(&self.db);
            let reaction = self.reaction;
            std::thread::spawn(move || {
                let Some(open) = db.open_cancel_requests().unwrap().into_iter().next() else {
                    return;
                };
                let daemon = Actor::Daemon;
                match reaction {
                    Reaction::Silent => {}
                    Reaction::AcceptOnly => {
                        db.accept_cancel(open.id, &daemon).unwrap();
                    }
                    Reaction::Cancel => {
                        db.accept_cancel(open.id, &daemon).unwrap();
                        db.transition(&TransitionRequest {
                            work_id: open.work_id.clone(),
                            expected_from: QueuePhase::Running,
                            to: QueuePhase::Skipped,
                            actor: daemon.clone(),
                            reason: TransitionReason::Canceled,
                            detail: None,
                        })
                        .unwrap();
                        db.close_cancel(open.id, CancelResult::Canceled, &daemon)
                            .unwrap();
                    }
                    Reaction::TooLate => {
                        db.close_cancel(open.id, CancelResult::TooLate, &daemon)
                            .unwrap();
                    }
                }
            });
            true
        }
    }

    fn with_daemon(reaction: Reaction) -> (Arc<Database>, String, FakeDaemon) {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let id = running_item(&db);
        let daemon = FakeDaemon {
            db: Arc::clone(&db),
            reaction,
        };
        (db, id, daemon)
    }

    #[test]
    fn a_daemon_that_closes_the_request_decides_the_result() {
        let (db, id, daemon) = with_daemon(Reaction::Cancel);

        let outcome = flow(&db, &daemon, &NoKill).cancel(&id).unwrap();

        assert_eq!(outcome, CancelOutcome::Canceled);
        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Skipped);
    }

    #[test]
    fn a_request_closed_too_late_reports_the_current_phase() {
        let (db, id, daemon) = with_daemon(Reaction::TooLate);

        let outcome = flow(&db, &daemon, &NoKill).cancel(&id).unwrap();

        assert_eq!(
            outcome,
            CancelOutcome::TooLate {
                current: QueuePhase::Running
            }
        );
    }

    #[test]
    fn an_accepted_request_that_does_not_close_in_time_is_accepted() {
        let (db, id, daemon) = with_daemon(Reaction::AcceptOnly);

        let outcome = flow(&db, &daemon, &NoKill).cancel(&id).unwrap();

        assert_eq!(outcome, CancelOutcome::Accepted);
        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Running);
    }

    #[test]
    fn a_daemon_that_never_accepts_is_bypassed_by_the_direct_path() {
        let (db, id, daemon) = with_daemon(Reaction::Silent);

        let outcome = flow(&db, &daemon, &NoKill).cancel(&id).unwrap();

        assert_eq!(outcome, CancelOutcome::CanceledDirectly);
        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Skipped);
    }
}
