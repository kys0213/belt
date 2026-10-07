//! SQLite persistence layer for Belt.
//!
//! Provides CRUD operations for queue items, history events, workspaces,
//! cron jobs, and token usage — all backed by a single SQLite database.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use belt_core::error::BeltError;
use belt_core::escalation::EscalationAction;
use belt_core::hitl::{
    ConfirmPath, HitlAction, HitlId, HitlResolution, HitlStatus, RespondOutcome,
};
use belt_core::lineage::{AttemptStatus, CollectDecision, collect_decision, count_since_reset};
use belt_core::phase::QueuePhase;
use belt_core::queue::{HitlReason, HitlRespondAction, QueueItem};
use belt_core::runtime::TokenUsage;
use belt_core::transition::{
    Actor, GuardDecision, ItemSnapshot, Processing, TransitionOutcome, TransitionReason,
    TransitionRequest, guard,
};

use crate::db_migrations;

/// An immutable history event recording an attempt on a work item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEvent {
    /// The work item this event belongs to.
    pub work_id: String,
    /// External source entity identifier.
    pub source_id: String,
    /// Workflow state when the event occurred.
    pub state: String,
    /// Outcome status (e.g. "success", "failed").
    pub status: String,
    /// Attempt number.
    pub attempt: i32,
    /// Optional summary of the result.
    pub summary: Option<String>,
    /// Optional error description.
    pub error: Option<String>,
    /// Timestamp when this event was created (RFC 3339).
    pub created_at: String,
}

/// A scheduled cron job definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    /// Unique name for the cron job.
    pub name: String,
    /// Cron schedule expression (e.g. "*/5 * * * *").
    pub schedule: String,
    /// Path to the script to execute.
    pub script: String,
    /// Optional workspace scope; `None` means global.
    pub workspace: Option<String>,
    /// Whether this job is currently enabled.
    pub enabled: bool,
    /// Timestamp of the last successful run, if any.
    pub last_run_at: Option<String>,
    /// When this job was created (RFC 3339).
    pub created_at: String,
    /// When this job was last updated (RFC 3339).
    pub updated_at: String,
}

/// A row from the `knowledge_base` table.
///
/// Stores extracted knowledge from merged PRs — decisions, patterns,
/// domain knowledge, and review feedback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeEntry {
    /// Auto-incremented row ID (populated on read, ignored on insert).
    pub id: Option<i64>,
    /// The workspace this knowledge belongs to.
    pub workspace: String,
    /// Source reference (e.g. "PR #42", "gh:org/repo#42").
    pub source_ref: String,
    /// Category of knowledge: "decision", "pattern", "domain", "review_feedback".
    pub category: String,
    /// The extracted knowledge content.
    pub content: String,
    /// When this entry was created (RFC 3339).
    pub created_at: String,
}

/// A row from the `token_usage` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsageRow {
    /// The work item ID this usage belongs to.
    pub work_id: String,
    /// The workspace scope.
    pub workspace: String,
    /// Name of the runtime that was invoked.
    pub runtime: String,
    /// Model identifier used for the invocation.
    pub model: String,
    /// Number of input tokens consumed.
    pub input_tokens: u64,
    /// Number of output tokens produced.
    pub output_tokens: u64,
    /// Number of cache-read tokens, if applicable.
    pub cache_read_tokens: Option<u64>,
    /// Number of cache-write tokens, if applicable.
    pub cache_write_tokens: Option<u64>,
    /// Wall-clock duration of the invocation in milliseconds, if recorded.
    pub duration_ms: Option<u64>,
    /// Timestamp when the usage was recorded.
    pub created_at: DateTime<Utc>,
}

/// A transition event recording a state change for a queue item.
///
/// Schema aligned with spec `05-monitoring.md` section 6.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionEvent {
    /// Unique event identifier.
    pub id: String,
    /// The queue item this transition belongs to (spec: `work_id`).
    pub work_id: String,
    /// External source entity identifier for lineage tracking (spec: `source_id`).
    pub source_id: String,
    /// Type of event (e.g. "phase_enter", "handler", "evaluate", "on_done", "on_fail").
    pub event_type: String,
    /// The phase entered (spec: `phase`).
    pub phase: Option<String>,
    /// The phase before the transition, for timeline rendering.
    pub from_phase: Option<String>,
    /// Human-readable detail: script exit code, prompt result, error message (spec: `detail`).
    pub detail: Option<String>,
    /// When this transition occurred (RFC 3339) (spec: `created_at`).
    pub created_at: String,
}

/// Per-model aggregated statistics from the `token_usage` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStats {
    /// Model identifier.
    pub model: String,
    /// Total input tokens consumed by this model.
    pub input_tokens: u64,
    /// Total output tokens produced by this model.
    pub output_tokens: u64,
    /// Combined input + output tokens for this model.
    pub total_tokens: u64,
    /// Number of invocations recorded for this model.
    pub executions: u64,
    /// Average wall-clock duration in milliseconds (only from rows that have a value).
    pub avg_duration_ms: Option<f64>,
}

/// Per-script (state) execution statistics aggregated from the `history` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptExecStats {
    /// Script/state name (e.g. "analyze", "implement").
    pub state: String,
    /// Total number of executions for this state.
    pub total_runs: u64,
    /// Number of successful executions.
    pub success_count: u64,
    /// Number of failed executions.
    pub fail_count: u64,
    /// Success rate as a percentage (0.0 - 100.0).
    pub success_rate: f64,
    /// Average duration in milliseconds (from `token_usage`), if available.
    pub avg_duration_ms: Option<f64>,
}

/// Aggregated runtime statistics across all models.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStats {
    /// Total input tokens across all models.
    pub total_tokens_input: u64,
    /// Total output tokens across all models.
    pub total_tokens_output: u64,
    /// Grand total of input + output tokens.
    pub total_tokens: u64,
    /// Total number of runtime invocations.
    pub executions: u64,
    /// Average wall-clock duration in milliseconds across all invocations.
    pub avg_duration_ms: Option<f64>,
    /// Per-model breakdown.
    pub by_model: HashMap<String, ModelStats>,
}

/// Column list shared by all `queue_items` SELECT and INSERT statements.
///
/// Keeping this in one place avoids drift when columns are added or reordered.
const QUEUE_ITEM_COLUMNS: &str = "work_id, source_id, workspace_id, state, phase, title, created_at, updated_at, hitl_created_at, hitl_respondent, hitl_notes, hitl_reason, hitl_timeout_at, hitl_terminal_action, replan_count, worktree_preserved, previous_worktree_path, derived_from, lineage_root";

/// Shorthand for extracting a column value and mapping the error to `BeltError::Database`.
fn col<T: rusqlite::types::FromSql>(row: &rusqlite::Row<'_>, idx: usize) -> Result<T, BeltError> {
    row.get(idx).map_err(|e| BeltError::Database(e.to_string()))
}

/// SQLite-backed persistence for Belt state.
///
/// The inner connection is wrapped in a [`Mutex`] so that `Database` is
/// `Send + Sync` and can be shared across async tasks.
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// Open (or create) a database at the given path and migrate it to the
    /// current schema version.
    ///
    /// A database created by an older binary is backed up next to `path`
    /// (`{path}.bak-v{old}`) before it is migrated. Migration is forward-only:
    /// a migrated database cannot be opened by an older binary.
    ///
    /// # Errors
    /// Returns `BeltError::Database` if the connection fails, the database was
    /// written by a newer binary, or the migration fails (it is rolled back).
    pub fn open(path: &str) -> Result<Self, BeltError> {
        let mut conn = Connection::open(path).map_err(|e| BeltError::Database(e.to_string()))?;
        db_migrations::migrate(&mut conn, Some(Path::new(path)))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open an in-memory database at the current schema version — useful for testing.
    ///
    /// # Errors
    /// Returns `BeltError::Database` if schema creation fails.
    pub fn open_in_memory() -> Result<Self, BeltError> {
        let mut conn =
            Connection::open_in_memory().map_err(|e| BeltError::Database(e.to_string()))?;
        db_migrations::migrate(&mut conn, None)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // ---- Queue CRUD --------------------------------------------------------

    /// Insert a new queue item.
    ///
    /// Writes no transition log row and issues no `work_id`; collection goes
    /// through [`Database::insert_collected`].
    ///
    /// # Errors
    /// Returns `BeltError::Database` on constraint violation, I/O error, or an
    /// empty `lineage_root`.
    pub fn insert_item(&self, item: &QueueItem) -> Result<(), BeltError> {
        let conn = self.lock_conn()?;
        insert_queue_row(&conn, item)
    }

    /// Update the phase of an existing queue item.
    ///
    /// Also refreshes `updated_at` to the current UTC time.
    ///
    /// Scheduled to be replaced by [`Database::transition`], which applies the
    /// transition contract and records the transition log. This method
    /// overwrites the phase without either.
    ///
    /// # Errors
    /// Returns `BeltError::ItemNotFound` if no row matches the given `work_id`.
    pub fn update_phase(&self, work_id: &str, phase: QueuePhase) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE queue_items SET phase = ?1, updated_at = ?2 WHERE work_id = ?3",
                params![phase_to_str(phase), now, work_id],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::ItemNotFound(work_id.to_string()));
        }
        Ok(())
    }

    /// Persist worktree preservation state for an item.
    ///
    /// Sets `worktree_preserved`, `previous_worktree_path`, and `phase`,
    /// and refreshes `updated_at`. Used during daemon shutdown rollback
    /// to ensure worktree reuse information survives restart.
    ///
    /// # Errors
    /// Returns `BeltError::ItemNotFound` if no row matches the given `work_id`.
    pub fn update_item_worktree_state(
        &self,
        work_id: &str,
        phase: QueuePhase,
        worktree_preserved: bool,
        previous_worktree_path: Option<&str>,
    ) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE queue_items SET phase = ?1, worktree_preserved = ?2, previous_worktree_path = ?3, updated_at = ?4 WHERE work_id = ?5",
                params![phase_to_str(phase), worktree_preserved, previous_worktree_path, now, work_id],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::ItemNotFound(work_id.to_string()));
        }
        Ok(())
    }

    /// Escalate an item to HITL phase with metadata.
    ///
    /// Sets `phase` to `Hitl`, records `hitl_created_at`, `hitl_reason`, and
    /// `hitl_notes`, and refreshes `updated_at`.
    ///
    /// # Errors
    /// Returns `BeltError::ItemNotFound` if no row matches the given `work_id`.
    pub fn escalate_to_hitl(
        &self,
        work_id: &str,
        reason: &str,
        notes: &str,
    ) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE queue_items SET phase = 'hitl', updated_at = ?1, hitl_created_at = ?2, hitl_reason = ?3, hitl_notes = ?4 WHERE work_id = ?5",
                params![now, now, reason, notes, work_id],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::ItemNotFound(work_id.to_string()));
        }
        Ok(())
    }

    /// Increment the `replan_count` for a queue item.
    ///
    /// Used by `EvaluateJob` to track per-item evaluate failure counts.
    /// Also refreshes `updated_at`.
    ///
    /// # Errors
    /// Returns `BeltError::ItemNotFound` if no row matches the given `work_id`.
    pub fn increment_replan_count(&self, work_id: &str) -> Result<u32, BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE queue_items SET replan_count = replan_count + 1, updated_at = ?1 WHERE work_id = ?2",
                params![now, work_id],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::ItemNotFound(work_id.to_string()));
        }
        // Return updated count.
        let count: u32 = conn
            .query_row(
                "SELECT replan_count FROM queue_items WHERE work_id = ?1",
                params![work_id],
                |row| row.get(0),
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(count)
    }

    /// Update HITL metadata when responding to a HITL item.
    ///
    /// Sets `hitl_respondent`, `hitl_notes`, phase, and refreshes `updated_at`.
    ///
    /// Scheduled to be replaced by the HITL request API, where the first
    /// response wins and the item leaves Hitl only through daemon
    /// post-processing. This method decides nothing about who wins.
    ///
    /// # Errors
    /// Returns `BeltError::ItemNotFound` if no row matches the given `work_id`.
    pub fn respond_hitl(
        &self,
        work_id: &str,
        phase: QueuePhase,
        respondent: Option<&str>,
        notes: Option<&str>,
    ) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE queue_items SET phase = ?1, updated_at = ?2, hitl_respondent = ?3, hitl_notes = COALESCE(?4, hitl_notes) WHERE work_id = ?5",
                params![phase_to_str(phase), now, respondent, notes, work_id],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::ItemNotFound(work_id.to_string()));
        }
        Ok(())
    }

    /// List queue items in HITL phase that have exceeded the timeout threshold.
    ///
    /// Returns work_ids of HITL items where `hitl_created_at` is older than
    /// `timeout_hours` from now.
    pub fn list_expired_hitl_items(&self, timeout_hours: u64) -> Result<Vec<String>, BeltError> {
        let cutoff = (Utc::now() - chrono::Duration::hours(timeout_hours as i64)).to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT work_id FROM queue_items WHERE phase = 'hitl' AND hitl_created_at IS NOT NULL AND hitl_created_at < ?1",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let work_ids = stmt
            .query_map(params![cutoff], |row| row.get::<_, String>(0))
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(work_ids)
    }

    /// Set HITL timeout on a queue item.
    ///
    /// Stores `hitl_timeout_at` (the absolute expiry time) and an optional
    /// `terminal_action` (skip/failed/replan) to apply when the timeout fires.
    ///
    /// # Errors
    /// Returns `BeltError::ItemNotFound` if no row matches the given `work_id`.
    pub fn set_hitl_timeout(
        &self,
        work_id: &str,
        timeout_at: &str,
        terminal_action: Option<&EscalationAction>,
    ) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let action_str = terminal_action.map(|a| a.to_string());
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE queue_items SET hitl_timeout_at = ?1, hitl_terminal_action = ?2, updated_at = ?3 WHERE work_id = ?4",
                params![timeout_at, action_str, now, work_id],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::ItemNotFound(work_id.to_string()));
        }
        Ok(())
    }

    /// List HITL items that have a timeout set and are pending expiry.
    ///
    /// Returns items where `hitl_timeout_at` is set and the item is still in HITL phase.
    pub fn list_hitl_items_with_timeout(&self) -> Result<Vec<QueueItem>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                &format!("SELECT {QUEUE_ITEM_COLUMNS} FROM queue_items WHERE phase = 'hitl' AND hitl_timeout_at IS NOT NULL"),
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let items = stmt
            .query_map([], |row| Ok(row_to_queue_item(row)))
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        items.into_iter().collect::<Result<Vec<_>, _>>()
    }

    /// Retrieve a single queue item by `work_id`.
    ///
    /// # Errors
    /// Returns `BeltError::ItemNotFound` if no row matches.
    pub fn get_item(&self, work_id: &str) -> Result<QueueItem, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        conn.query_row(
            &format!("SELECT {QUEUE_ITEM_COLUMNS} FROM queue_items WHERE work_id = ?1"),
            params![work_id],
            |row| Ok(row_to_queue_item(row)),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => BeltError::ItemNotFound(work_id.to_string()),
            other => BeltError::Database(other.to_string()),
        })?
    }

    /// List queue items with optional phase and workspace filters.
    ///
    /// When both filters are `None`, all items are returned.
    pub fn list_items(
        &self,
        phase: Option<QueuePhase>,
        workspace: Option<&str>,
    ) -> Result<Vec<QueueItem>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut sql = format!("SELECT {QUEUE_ITEM_COLUMNS} FROM queue_items WHERE 1=1");
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(p) = phase {
            sql.push_str(" AND phase = ?");
            param_values.push(Box::new(phase_to_str(p).to_string()));
        }
        if let Some(ws) = workspace {
            sql.push_str(" AND workspace_id = ?");
            param_values.push(Box::new(ws.to_string()));
        }

        let params_ref: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let items = stmt
            .query_map(params_ref.as_slice(), |row| Ok(row_to_queue_item(row)))
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;

        // Unwrap the inner Results produced by row_to_queue_item.
        items.into_iter().collect::<Result<Vec<_>, _>>()
    }

    /// Check whether any non-terminal queue items exist for the given `source_id`.
    ///
    /// Non-terminal phases are those where `is_terminal()` returns `false`
    /// (i.e. everything except `Done` and `Skipped`).  This is used as a
    /// deduplication guard — e.g. the gap-detection job skips issue creation
    /// when an open item for the same spec already exists.
    pub fn has_open_items_for_source(&self, source_id: &str) -> Result<bool, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM queue_items WHERE source_id = ?1 AND phase NOT IN ('done', 'skipped')",
                rusqlite::params![source_id],
                |row| row.get(0),
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(count > 0)
    }

    /// Count queue items grouped by phase.
    ///
    /// Returns a list of `(phase_string, count)` tuples.
    pub fn count_items_by_phase(&self) -> Result<Vec<(String, u32)>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare("SELECT phase, COUNT(*) FROM queue_items GROUP BY phase ORDER BY phase")
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                let phase: String = row.get(0)?;
                let count: u32 = row.get(1)?;
                Ok((phase, count))
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(rows)
    }

    // ---- History -----------------------------------------------------------

    /// Append an immutable history event.
    pub fn append_history(&self, event: &HistoryEvent) -> Result<(), BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        conn.execute(
            "INSERT INTO history (work_id, source_id, state, status, attempt, summary, error, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                event.work_id,
                event.source_id,
                event.state,
                event.status,
                event.attempt,
                event.summary,
                event.error,
                event.created_at,
            ],
        )
        .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(())
    }

    /// Get all history events for a given `source_id`, ordered by creation time.
    pub fn get_history(&self, source_id: &str) -> Result<Vec<HistoryEvent>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT work_id, source_id, state, status, attempt, summary, error, created_at
                 FROM history WHERE source_id = ?1 ORDER BY created_at ASC",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let events = stmt
            .query_map(params![source_id], |row| {
                Ok(HistoryEvent {
                    work_id: row.get(0)?,
                    source_id: row.get(1)?,
                    state: row.get(2)?,
                    status: row.get(3)?,
                    attempt: row.get(4)?,
                    summary: row.get(5)?,
                    error: row.get(6)?,
                    created_at: row.get(7)?,
                })
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(events)
    }

    /// Count how many times a `source_id` has failed in a given `state`.
    pub fn count_failures(&self, source_id: &str, state: &str) -> Result<u32, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM history WHERE source_id = ?1 AND state = ?2 AND status = 'failed'",
                params![source_id, state],
                |row| row.get(0),
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(count)
    }

    // ---- Workspaces --------------------------------------------------------

    /// Register a new workspace.
    pub fn add_workspace(&self, name: &str, config_path: &str) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        conn.execute(
            "INSERT INTO workspaces (name, config_path, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4)",
            params![name, config_path, now, now],
        )
        .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(())
    }

    /// Update the `config_path` of an existing workspace.
    ///
    /// Also refreshes `updated_at` to the current UTC time.
    ///
    /// # Errors
    /// Returns `BeltError::WorkspaceNotFound` if no workspace matches the given `name`.
    pub fn update_workspace(&self, name: &str, config_path: &str) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE workspaces SET config_path = ?1, updated_at = ?2 WHERE name = ?3",
                params![config_path, now, name],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::WorkspaceNotFound(name.to_string()));
        }
        Ok(())
    }

    /// List all registered workspaces as `(name, config_path, created_at)` tuples.
    pub fn list_workspaces(&self) -> Result<Vec<(String, String, String)>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare("SELECT name, config_path, created_at FROM workspaces ORDER BY name")
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                let name: String = row.get(0)?;
                let config_path: String = row.get(1)?;
                let created_at: String = row.get(2)?;
                Ok((name, config_path, created_at))
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(rows)
    }

    /// Get a single workspace by name.
    ///
    /// # Errors
    /// Returns `BeltError::WorkspaceNotFound` if no such workspace exists.
    pub fn get_workspace(&self, name: &str) -> Result<(String, String, String), BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        conn.query_row(
            "SELECT name, config_path, created_at FROM workspaces WHERE name = ?1",
            params![name],
            |row| {
                let n: String = row.get(0)?;
                let cp: String = row.get(1)?;
                let ca: String = row.get(2)?;
                Ok((n, cp, ca))
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => BeltError::WorkspaceNotFound(name.to_string()),
            other => BeltError::Database(other.to_string()),
        })
    }

    /// Get a single workspace by name, including `updated_at` for cache invalidation.
    ///
    /// Returns `(name, config_path, updated_at)`.
    ///
    /// # Errors
    /// Returns `BeltError::WorkspaceNotFound` if no such workspace exists.
    pub fn get_workspace_with_updated_at(
        &self,
        name: &str,
    ) -> Result<(String, String, String), BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        conn.query_row(
            "SELECT name, config_path, updated_at FROM workspaces WHERE name = ?1",
            params![name],
            |row| {
                let n: String = row.get(0)?;
                let cp: String = row.get(1)?;
                let ua: String = row.get(2)?;
                Ok((n, cp, ua))
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => BeltError::WorkspaceNotFound(name.to_string()),
            other => BeltError::Database(other.to_string()),
        })
    }

    /// Remove a workspace by name.
    ///
    /// # Errors
    /// Returns `BeltError::WorkspaceNotFound` if no row was deleted.
    pub fn remove_workspace(&self, name: &str) -> Result<(), BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute("DELETE FROM workspaces WHERE name = ?1", params![name])
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::WorkspaceNotFound(name.to_string()));
        }
        Ok(())
    }

    // ---- Cron Jobs ---------------------------------------------------------

    /// Add a new cron job.
    pub fn add_cron_job(
        &self,
        name: &str,
        schedule: &str,
        script: &str,
        workspace: Option<&str>,
    ) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        conn.execute(
            "INSERT INTO cron_jobs (name, schedule, script, workspace, enabled, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)",
            params![name, schedule, script, workspace, now],
        )
        .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(())
    }

    /// List all cron jobs.
    pub fn list_cron_jobs(&self) -> Result<Vec<CronJob>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT name, schedule, script, workspace, enabled, last_run_at, created_at, updated_at
                 FROM cron_jobs ORDER BY name",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let jobs = stmt
            .query_map([], |row| {
                let enabled_int: i32 = row.get(4)?;
                Ok(CronJob {
                    name: row.get(0)?,
                    schedule: row.get(1)?,
                    script: row.get(2)?,
                    workspace: row.get(3)?,
                    enabled: enabled_int != 0,
                    last_run_at: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(jobs)
    }

    /// Get a cron job by name.
    ///
    /// # Errors
    /// Returns `BeltError::ItemNotFound` if no cron job matches the given `name`.
    pub fn get_cron_job(&self, name: &str) -> Result<CronJob, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT name, schedule, script, workspace, enabled, last_run_at, created_at, updated_at
                 FROM cron_jobs WHERE name = ?1",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;

        stmt.query_row(params![name], |row| {
            let enabled_int: i32 = row.get(4)?;
            Ok(CronJob {
                name: row.get(0)?,
                schedule: row.get(1)?,
                script: row.get(2)?,
                workspace: row.get(3)?,
                enabled: enabled_int != 0,
                last_run_at: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => BeltError::ItemNotFound(name.to_string()),
            _ => BeltError::Database(e.to_string()),
        })
    }

    /// Reset the `last_run_at` timestamp of a cron job to `NULL`.
    ///
    /// This causes the cron engine to treat the job as never-run, so it will
    /// fire on the next tick regardless of schedule.
    ///
    /// # Errors
    /// Returns `BeltError::ItemNotFound` if no cron job matches the given `name`.
    pub fn reset_cron_last_run(&self, name: &str) -> Result<(), BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE cron_jobs SET last_run_at = NULL WHERE name = ?1",
                params![name],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::ItemNotFound(name.to_string()));
        }
        Ok(())
    }

    /// Update the `last_run_at` timestamp of a cron job to now.
    pub fn update_cron_last_run(&self, name: &str) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE cron_jobs SET last_run_at = ?1 WHERE name = ?2",
                params![now, name],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::ItemNotFound(name.to_string()));
        }
        Ok(())
    }

    /// Update the schedule expression of an existing cron job.
    ///
    /// # Errors
    /// Returns `BeltError::ItemNotFound` if no cron job matches the given `name`.
    pub fn update_cron_schedule(&self, name: &str, schedule: &str) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE cron_jobs SET schedule = ?1, updated_at = ?3 WHERE name = ?2",
                params![schedule, name, now],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::ItemNotFound(name.to_string()));
        }
        Ok(())
    }

    /// Update the script path of an existing cron job.
    ///
    /// # Errors
    /// Returns `BeltError::ItemNotFound` if no cron job matches the given `name`.
    pub fn update_cron_script(&self, name: &str, script: &str) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE cron_jobs SET script = ?1, updated_at = ?3 WHERE name = ?2",
                params![script, name, now],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::ItemNotFound(name.to_string()));
        }
        Ok(())
    }

    /// Enable or disable a cron job.
    pub fn toggle_cron_job(&self, name: &str, enabled: bool) -> Result<(), BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE cron_jobs SET enabled = ?1 WHERE name = ?2",
                params![enabled as i32, name],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::ItemNotFound(name.to_string()));
        }
        Ok(())
    }

    /// Remove a cron job by name.
    pub fn remove_cron_job(&self, name: &str) -> Result<(), BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute("DELETE FROM cron_jobs WHERE name = ?1", params![name])
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::ItemNotFound(name.to_string()));
        }
        Ok(())
    }

    // ---- Knowledge Base ----------------------------------------------------

    /// Insert a new knowledge entry extracted from a merged PR.
    ///
    /// # Errors
    /// Returns `BeltError::Database` on constraint violation or I/O error.
    pub fn insert_knowledge(&self, entry: &KnowledgeEntry) -> Result<(), BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        conn.execute(
            "INSERT INTO knowledge_base (workspace, source_ref, category, content, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                entry.workspace,
                entry.source_ref,
                entry.category,
                entry.content,
                entry.created_at,
            ],
        )
        .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(())
    }

    /// List knowledge entries, optionally filtered by workspace and/or category.
    pub fn list_knowledge(
        &self,
        workspace: Option<&str>,
        category: Option<&str>,
    ) -> Result<Vec<KnowledgeEntry>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let mut sql = String::from(
            "SELECT id, workspace, source_ref, category, content, created_at FROM knowledge_base WHERE 1=1",
        );
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(ws) = workspace {
            sql.push_str(" AND workspace = ?");
            param_values.push(Box::new(ws.to_string()));
        }
        if let Some(cat) = category {
            sql.push_str(" AND category = ?");
            param_values.push(Box::new(cat.to_string()));
        }
        sql.push_str(" ORDER BY created_at DESC");

        let params_ref: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let entries = stmt
            .query_map(params_ref.as_slice(), |row| {
                Ok(KnowledgeEntry {
                    id: row.get(0)?,
                    workspace: row.get(1)?,
                    source_ref: row.get(2)?,
                    category: row.get(3)?,
                    content: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(entries)
    }

    /// Get knowledge entries for a specific source reference (e.g. a PR).
    pub fn get_knowledge_by_source(
        &self,
        source_ref: &str,
    ) -> Result<Vec<KnowledgeEntry>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, workspace, source_ref, category, content, created_at
                 FROM knowledge_base WHERE source_ref = ?1 ORDER BY created_at DESC",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let entries = stmt
            .query_map(params![source_ref], |row| {
                Ok(KnowledgeEntry {
                    id: row.get(0)?,
                    workspace: row.get(1)?,
                    source_ref: row.get(2)?,
                    category: row.get(3)?,
                    content: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(entries)
    }

    // ---- Transition Events -------------------------------------------------

    /// Record a transition event.
    pub fn insert_transition_event(&self, event: &TransitionEvent) -> Result<(), BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        conn.execute(
            "INSERT INTO transition_events (id, work_id, source_id, event_type, phase, from_phase, detail, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                event.id,
                event.work_id,
                event.source_id,
                event.event_type,
                event.phase,
                event.from_phase,
                event.detail,
                event.created_at,
            ],
        )
        .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(())
    }

    /// List transition events for a given item, ordered by created_at ascending.
    pub fn list_transition_events(&self, work_id: &str) -> Result<Vec<TransitionEvent>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, work_id, source_id, event_type, phase, from_phase, detail, created_at
                 FROM transition_events WHERE work_id = ?1 ORDER BY created_at ASC",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let events = stmt
            .query_map(params![work_id], |row| {
                Ok(TransitionEvent {
                    id: row.get(0)?,
                    work_id: row.get(1)?,
                    source_id: row.get(2)?,
                    event_type: row.get(3)?,
                    phase: row.get(4)?,
                    from_phase: row.get(5)?,
                    detail: row.get(6)?,
                    created_at: row.get(7)?,
                })
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(events)
    }

    /// List the most recent transition events across all items.
    ///
    /// Returns up to `limit` events ordered by created_at descending.
    pub fn list_recent_transition_events(
        &self,
        limit: u32,
    ) -> Result<Vec<TransitionEvent>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, work_id, source_id, event_type, phase, from_phase, detail, created_at
                 FROM transition_events ORDER BY created_at DESC LIMIT ?1",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let events = stmt
            .query_map(params![limit], |row| {
                Ok(TransitionEvent {
                    id: row.get(0)?,
                    work_id: row.get(1)?,
                    source_id: row.get(2)?,
                    event_type: row.get(3)?,
                    phase: row.get(4)?,
                    from_phase: row.get(5)?,
                    detail: row.get(6)?,
                    created_at: row.get(7)?,
                })
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(events)
    }

    // ---- Queue Dependencies ------------------------------------------------

    /// Add a dependency between two queue items.
    ///
    /// Declares that `work_id` depends on (must run after) `depends_on_id`.
    /// Before inserting, performs a DFS cycle check to prevent circular
    /// dependencies that could cause runtime deadlocks.
    ///
    /// # Errors
    /// - Returns `BeltError::CircularDependency` if adding this edge would
    ///   create a cycle.
    /// - Returns `BeltError::Database` on constraint violation or I/O error.
    pub fn add_queue_dependency(
        &self,
        work_id: &str,
        depends_on_id: &str,
    ) -> Result<(), BeltError> {
        // Self-dependency is a trivial cycle.
        if work_id == depends_on_id {
            return Err(BeltError::CircularDependency(format!(
                "{work_id} -> {work_id}"
            )));
        }

        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;

        // Check for circular dependency using DFS before inserting.
        // We want to verify that `depends_on_id` does not transitively
        // depend on `work_id`. Starting from `depends_on_id`, follow
        // the depends_on edges; if we reach `work_id`, a cycle exists.
        Self::detect_cycle(&conn, work_id, depends_on_id)?;

        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT OR IGNORE INTO queue_dependencies (work_id, depends_on, created_at)
                 VALUES (?1, ?2, ?3)",
            params![work_id, depends_on_id, now],
        )
        .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(())
    }

    /// DFS cycle detection: starting from `start`, follow depends_on edges
    /// in the existing graph. If we reach `target`, a cycle would be formed.
    fn detect_cycle(
        conn: &rusqlite::Connection,
        target: &str,
        start: &str,
    ) -> Result<(), BeltError> {
        use std::collections::HashSet;

        let mut visited = HashSet::new();
        let mut stack = vec![start.to_string()];

        while let Some(node) = stack.pop() {
            if node == target {
                return Err(BeltError::CircularDependency(format!(
                    "{target} -> ... -> {node}"
                )));
            }
            if !visited.insert(node.clone()) {
                continue;
            }
            // Find all nodes that `node` depends on (outgoing edges).
            let mut stmt = conn
                .prepare("SELECT depends_on FROM queue_dependencies WHERE work_id = ?1")
                .map_err(|e| BeltError::Database(e.to_string()))?;
            let deps: Vec<String> = stmt
                .query_map(params![node], |row| row.get(0))
                .map_err(|e| BeltError::Database(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| BeltError::Database(e.to_string()))?;
            for dep in deps {
                if !visited.contains(&dep) {
                    stack.push(dep);
                }
            }
        }
        Ok(())
    }

    /// Remove a dependency between two queue items.
    ///
    /// # Errors
    /// Returns `BeltError::ItemNotFound` if the dependency does not exist.
    pub fn remove_queue_dependency(
        &self,
        work_id: &str,
        depends_on_id: &str,
    ) -> Result<(), BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "DELETE FROM queue_dependencies WHERE work_id = ?1 AND depends_on = ?2",
                params![work_id, depends_on_id],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::ItemNotFound(format!(
                "dependency {work_id} -> {depends_on_id}"
            )));
        }
        Ok(())
    }

    /// List all dependencies for a given queue item.
    ///
    /// Returns the `work_id` values that the given item depends on.
    pub fn list_queue_dependencies(&self, work_id: &str) -> Result<Vec<String>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT depends_on FROM queue_dependencies WHERE work_id = ?1 ORDER BY created_at ASC",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let deps = stmt
            .query_map(params![work_id], |row| row.get::<_, String>(0))
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(deps)
    }

    /// List all dependency pairs across all queue items.
    ///
    /// Returns `(work_id, depends_on)` tuples ordered by creation time.
    pub fn list_all_queue_dependencies(&self) -> Result<Vec<(String, String)>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare("SELECT work_id, depends_on FROM queue_dependencies ORDER BY created_at ASC")
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let deps = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(deps)
    }

    // ---- Token Usage -------------------------------------------------------

    /// Record token usage for a completed runtime invocation.
    ///
    /// The optional `duration_ms` parameter captures the wall-clock duration
    /// of the runtime invocation in milliseconds.
    pub fn record_token_usage(
        &self,
        work_id: &str,
        workspace: &str,
        runtime: &str,
        model: &str,
        usage: &TokenUsage,
        duration_ms: Option<u64>,
    ) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        conn.execute(
            "INSERT INTO token_usage (work_id, workspace, runtime, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, duration_ms, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                work_id,
                workspace,
                runtime,
                model,
                usage.input_tokens as i64,
                usage.output_tokens as i64,
                usage.cache_read_tokens.map(|v| v as i64),
                usage.cache_write_tokens.map(|v| v as i64),
                duration_ms.map(|d| d as i64),
                now,
            ],
        )
        .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(())
    }

    /// Retrieve all token usage records for a given `work_id`.
    ///
    /// Results are ordered by `created_at` ascending.
    pub fn get_token_usage_by_work_id(
        &self,
        work_id: &str,
    ) -> Result<Vec<TokenUsageRow>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT work_id, workspace, runtime, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, duration_ms, created_at
                 FROM token_usage WHERE work_id = ?1 ORDER BY created_at ASC",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(params![work_id], |row| {
                let created_str: String = row.get(9)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    created_str,
                ))
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;

        rows.into_iter()
            .map(
                |(wid, ws, rt, model, input, output, cache_read, cache_write, dur, created)| {
                    Ok(TokenUsageRow {
                        work_id: wid,
                        workspace: ws,
                        runtime: rt,
                        model,
                        input_tokens: input as u64,
                        output_tokens: output as u64,
                        cache_read_tokens: cache_read.map(|v| v as u64),
                        cache_write_tokens: cache_write.map(|v| v as u64),
                        duration_ms: dur.map(|d| d as u64),
                        created_at: parse_datetime(&created)?,
                    })
                },
            )
            .collect()
    }

    /// Retrieve all token usage records for a given workspace.
    ///
    /// Results are ordered by `created_at` ascending.
    pub fn get_token_usage_by_workspace(
        &self,
        workspace: &str,
    ) -> Result<Vec<TokenUsageRow>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT work_id, workspace, runtime, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, duration_ms, created_at
                 FROM token_usage WHERE workspace = ?1 ORDER BY created_at ASC",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(params![workspace], |row| {
                let created_str: String = row.get(9)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    created_str,
                ))
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;

        rows.into_iter()
            .map(
                |(wid, ws, rt, model, input, output, cache_read, cache_write, dur, created)| {
                    Ok(TokenUsageRow {
                        work_id: wid,
                        workspace: ws,
                        runtime: rt,
                        model,
                        input_tokens: input as u64,
                        output_tokens: output as u64,
                        cache_read_tokens: cache_read.map(|v| v as u64),
                        cache_write_tokens: cache_write.map(|v| v as u64),
                        duration_ms: dur.map(|d| d as u64),
                        created_at: parse_datetime(&created)?,
                    })
                },
            )
            .collect()
    }

    /// Aggregate runtime statistics from the last 24 hours, grouped by model.
    ///
    /// Returns overall totals and a per-model breakdown of token usage,
    /// execution count, and average duration.
    pub fn get_runtime_stats(&self) -> Result<RuntimeStats, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let cutoff = (Utc::now() - chrono::Duration::hours(24)).to_rfc3339();

        let mut stmt = conn
            .prepare(
                "SELECT model,
                        SUM(input_tokens)  AS total_input,
                        SUM(output_tokens) AS total_output,
                        COUNT(*)           AS exec_count,
                        AVG(duration_ms)   AS avg_dur
                 FROM token_usage
                 WHERE created_at >= ?1
                 GROUP BY model
                 ORDER BY model",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let model_rows = stmt
            .query_map(params![cutoff], |row| {
                let model: String = row.get(0)?;
                let input: i64 = row.get(1)?;
                let output: i64 = row.get(2)?;
                let count: i64 = row.get(3)?;
                let avg_dur: Option<f64> = row.get(4)?;
                Ok((model, input, output, count, avg_dur))
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let mut total_input: u64 = 0;
        let mut total_output: u64 = 0;
        let mut total_executions: u64 = 0;
        let mut duration_sum: f64 = 0.0;
        let mut duration_count: u64 = 0;
        let mut by_model = HashMap::new();

        for (model, input, output, count, avg_dur) in model_rows {
            let inp = input as u64;
            let out = output as u64;
            let cnt = count as u64;
            total_input += inp;
            total_output += out;
            total_executions += cnt;

            if let Some(d) = avg_dur {
                duration_sum += d * cnt as f64;
                duration_count += cnt;
            }

            by_model.insert(
                model.clone(),
                ModelStats {
                    model,
                    input_tokens: inp,
                    output_tokens: out,
                    total_tokens: inp + out,
                    executions: cnt,
                    avg_duration_ms: avg_dur,
                },
            );
        }

        let avg_duration_ms = if duration_count > 0 {
            Some(duration_sum / duration_count as f64)
        } else {
            None
        };

        Ok(RuntimeStats {
            total_tokens_input: total_input,
            total_tokens_output: total_output,
            total_tokens: total_input + total_output,
            executions: total_executions,
            avg_duration_ms,
            by_model,
        })
    }

    /// Aggregate token usage since a given cutoff timestamp, grouped by model.
    ///
    /// Returns a vec of `(model, input_tokens, output_tokens, executions)` tuples
    /// ordered by total tokens descending.  The caller decides the cutoff (e.g.
    /// 24 hours, 7 days, 30 days) so that daily/weekly/monthly views share a
    /// single query path.
    pub fn get_token_usage_since(
        &self,
        since: &DateTime<Utc>,
    ) -> Result<Vec<(String, u64, u64, u64)>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let cutoff = since.to_rfc3339();

        let mut stmt = conn
            .prepare(
                "SELECT model,
                        SUM(input_tokens)  AS total_input,
                        SUM(output_tokens) AS total_output,
                        COUNT(*)           AS exec_count
                 FROM token_usage
                 WHERE created_at >= ?1
                 GROUP BY model
                 ORDER BY (SUM(input_tokens) + SUM(output_tokens)) DESC",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(params![cutoff], |row| {
                let model: String = row.get(0)?;
                let input: i64 = row.get(1)?;
                let output: i64 = row.get(2)?;
                let count: i64 = row.get(3)?;
                Ok((model, input as u64, output as u64, count as u64))
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;

        Ok(rows)
    }

    /// Aggregate script execution statistics from the `history` table, grouped by state.
    ///
    /// Returns per-state (script) totals for success/failure counts and rates.
    /// Average duration is joined from the `token_usage` table when available.
    /// Results are ordered by total runs descending.
    pub fn get_script_execution_stats(&self) -> Result<Vec<ScriptExecStats>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;

        // Aggregate success/fail counts per state from history.
        let mut stmt = conn
            .prepare(
                "SELECT state,
                        COUNT(*)                                    AS total,
                        SUM(CASE WHEN status = 'success' THEN 1 ELSE 0 END) AS successes,
                        SUM(CASE WHEN status = 'failed'  THEN 1 ELSE 0 END) AS failures
                 FROM history
                 GROUP BY state
                 ORDER BY total DESC",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                let state: String = row.get(0)?;
                let total: i64 = row.get(1)?;
                let successes: i64 = row.get(2)?;
                let failures: i64 = row.get(3)?;
                Ok((state, total, successes, failures))
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;

        // Look up average duration per work_id's state from token_usage.
        // We join on work_id since history and token_usage both reference the same work_id.
        let avg_dur_by_state: HashMap<String, f64> = {
            let mut dur_stmt = conn
                .prepare(
                    "SELECT h.state, AVG(t.duration_ms)
                     FROM history h
                     JOIN token_usage t ON h.work_id = t.work_id
                     WHERE t.duration_ms IS NOT NULL
                     GROUP BY h.state",
                )
                .map_err(|e| BeltError::Database(e.to_string()))?;

            dur_stmt
                .query_map([], |row| {
                    let state: String = row.get(0)?;
                    let avg_dur: f64 = row.get(1)?;
                    Ok((state, avg_dur))
                })
                .map_err(|e| BeltError::Database(e.to_string()))?
                .filter_map(|r| r.ok())
                .collect()
        };

        let stats = rows
            .into_iter()
            .map(|(state, total, successes, failures)| {
                let total_u = total as u64;
                let success_u = successes as u64;
                let fail_u = failures as u64;
                let rate = if total_u > 0 {
                    (success_u as f64 / total_u as f64) * 100.0
                } else {
                    0.0
                };
                ScriptExecStats {
                    avg_duration_ms: avg_dur_by_state.get(&state).copied(),
                    state,
                    total_runs: total_u,
                    success_count: success_u,
                    fail_count: fail_u,
                    success_rate: rate,
                }
            })
            .collect();

        Ok(stats)
    }

    /// Retrieve the most recent history events across all scripts, ordered by
    /// `created_at` descending.
    ///
    /// Returns at most `limit` entries.
    pub fn get_recent_script_executions(
        &self,
        limit: usize,
    ) -> Result<Vec<HistoryEvent>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT work_id, source_id, state, status, attempt, summary, error, created_at
                 FROM history
                 ORDER BY created_at DESC
                 LIMIT ?1",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let events = stmt
            .query_map(params![limit as i64], |row| {
                Ok(HistoryEvent {
                    work_id: row.get(0)?,
                    source_id: row.get(1)?,
                    state: row.get(2)?,
                    status: row.get(3)?,
                    attempt: row.get(4)?,
                    summary: row.get(5)?,
                    error: row.get(6)?,
                    created_at: row.get(7)?,
                })
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(events)
    }
}

// ---- Transition / lineage store API ----------------------------------------

/// Values of `transition_log.kind` written by this binary.
///
/// Rows copied from the legacy `transition_events` table keep their original
/// `event_type` (`handler`, `evaluate`, `on_done`, ...) and are marked by the
/// `legacy` actor, so readers must treat `kind` as an open vocabulary. The
/// legacy phase kind is the same word as [`PHASE_ENTER`], which keeps phase
/// history uniform across the migration.
pub mod transition_kind {
    /// An item entered a phase (`from_phase` → `to_phase`).
    pub const PHASE_ENTER: &str = "phase_enter";
    /// An item was created: collected, or derived (origin in `detail`).
    pub const ITEM_CREATED: &str = "item_created";
    /// A transition was refused with `busy` because the item is being processed.
    pub const TRANSITION_REJECTED: &str = "transition_rejected";
    /// A transition found a phase other than the expected one.
    pub const TRANSITION_CONFLICT: &str = "transition_conflict";
    /// A HITL response was refused; `reason` is `already_handled` or
    /// `unauthorized`. The phase columns are empty: no transition happened.
    pub const HITL_RESPONSE_REJECTED: &str = "hitl_response_rejected";
}

/// One row of the append-only `transition_log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionLogEntry {
    /// Global, strictly increasing, never reused.
    pub seq: u64,
    pub work_id: String,
    pub source_id: String,
    /// See [`transition_kind`]; legacy rows carry their original event type.
    pub kind: String,
    /// Phase before the event; `None` for creation and non-phase events.
    pub from_phase: Option<String>,
    pub to_phase: Option<String>,
    /// `daemon`, `cli`, `tui`, `cron`, an external channel name, or `legacy`.
    pub actor: String,
    pub reason: Option<String>,
    pub detail: Option<String>,
    /// RFC 3339.
    pub created_at: String,
}

/// An item to create from collection. The `work_id` is issued by the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewItem {
    pub source_id: String,
    pub workspace_id: String,
    pub state: String,
    pub title: Option<String>,
    pub actor: Actor,
}

/// Result of [`Database::insert_collected`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectOutcome {
    /// A new Pending item (first item of a new lineage) was created.
    Inserted { work_id: String },
    /// The same `(source_id, state)` still has an item that is not Done or Skipped.
    Duplicate,
}

/// Why an item is being derived. Decides worktree and failure-count handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeriveKind {
    /// escalation retry: the origin's worktree is handed over to the derived item.
    EscalationRetry,
    /// replan: the derived item gets a new worktree and a failure-count reset point.
    Replan,
}

/// Request to end `work_id` as Skipped and continue the work in a derived item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeriveRequest {
    pub work_id: String,
    pub expected_from: QueuePhase,
    pub kind: DeriveKind,
    pub actor: Actor,
    pub reason: TransitionReason,
    pub detail: Option<String>,
}

/// Result of [`Database::derive`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeriveOutcome {
    Derived {
        work_id: String,
    },
    /// The origin's transition was refused; nothing was derived.
    Rejected(TransitionOutcome),
}

/// Request to enter Hitl and open a HITL request for `work_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenHitlRequest {
    pub work_id: String,
    pub expected_from: QueuePhase,
    pub reason: HitlReason,
    pub notes: Option<String>,
    pub actor: Actor,
    /// Recorded on the `X -> Hitl` transition (e.g. `Escalation(Hitl)`).
    pub transition_reason: TransitionReason,
    /// Absolute expiry time (RFC 3339), if the request can expire.
    pub timeout_at: Option<String>,
    /// Action applied on expiry; `skip` or `replan`.
    pub terminal_action: Option<EscalationAction>,
}

/// Result of [`Database::open_hitl`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenHitlOutcome {
    Opened {
        hitl_id: HitlId,
        /// `transition_log.seq` of the `X -> Hitl` transition.
        seq: u64,
    },
    /// The `X -> Hitl` transition was refused; no request was created.
    Rejected(TransitionOutcome),
}

/// Which request a response addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitlTarget {
    /// One specific request instance.
    Id(HitlId),
    /// The current request of an item: its open request, else its latest one.
    Item(String),
}

/// One row of `hitl_requests`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitlRequest {
    pub hitl_id: HitlId,
    pub work_id: String,
    pub status: HitlStatus,
    pub reason: Option<HitlReason>,
    pub notes: Option<String>,
    /// RFC 3339.
    pub opened_at: String,
    pub timeout_at: Option<String>,
    pub terminal_action: Option<EscalationAction>,
    /// The winning response or expiry; `None` while open.
    pub resolution: Option<HitlResolution>,
    pub resolution_notes: Option<String>,
    pub post_processed_at: Option<String>,
    pub post_processing_failures: u32,
}

/// Result of [`Database::complete_post_processing`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompleteOutcome {
    Completed {
        /// `transition_log.seq` of the result transition.
        seq: u64,
    },
    /// The request is still open or was already post-processed.
    NotPending,
    /// The result transition was refused; the request stays pending.
    Rejected(TransitionOutcome),
}

/// `resolution.by` / `resolution.via` recorded when a request expires.
const REJECT_ALREADY_HANDLED: &str = "already_handled";
const REJECT_UNAUTHORIZED: &str = "unauthorized";
const EXPIRY_BY: &str = "system";
const EXPIRY_VIA: &str = "timeout";

const HITL_COLUMNS: &str = "hitl_id, work_id, status, reason, notes, opened_at, timeout_at, \
     terminal_action, action, respondent, via, confirm_path, resolved_at, resolution_notes, \
     post_processed_at, post_processing_failures";

/// What a transition attempt inside a transaction decided.
enum Step {
    Applied {
        seq: u64,
    },
    Rejected(TransitionOutcome),
    /// The request maps to a HITL response; the HITL API owns that race.
    HitlResponse,
}

/// Fields of a `transition_log` row to append.
struct LogRecord<'a> {
    work_id: &'a str,
    source_id: &'a str,
    kind: &'a str,
    from_phase: Option<&'a str>,
    to_phase: Option<&'a str>,
    actor: &'a str,
    reason: Option<&'a str>,
    detail: Option<&'a str>,
}

impl Database {
    /// Attempt a phase transition through the transition contract.
    ///
    /// One `BEGIN IMMEDIATE` transaction: read the current state, ask
    /// [`belt_core::transition::guard`], then (only on `Proceed`) change the
    /// phase and append a `phase_enter` row. `busy` and `conflict` refusals
    /// are committed as `transition_rejected` / `transition_conflict` rows;
    /// `invalid_action` leaves no row. Refusals are values, not errors.
    ///
    /// A request that leaves Hitl without being a daemon post-processing
    /// transition corresponds to a HITL response. This method does not decide
    /// that race: it reports `InvalidAction { current: Hitl }` and changes
    /// nothing; the caller maps its target phase with
    /// [`belt_core::transition::hitl_response_for`] and, when that yields an
    /// action, answers through [`Database::resolve_hitl`]. The
    /// `InvalidAction` result alone never justifies a HITL response: the caller
    /// branches with `hitl_response_for` first, because the same value also
    /// means a request with no matching response and a daemon post-processing
    /// attempt without a confirmed request.
    ///
    /// A daemon post-processing transition leaves Hitl only while a confirmed
    /// request awaits post-processing; with only an open request it is
    /// `InvalidAction { current: Hitl }`.
    ///
    /// # Errors
    /// `BeltError::ItemNotFound` for an unknown `work_id`, `BeltError::Database`
    /// on I/O failure.
    pub fn transition(&self, req: &TransitionRequest) -> Result<TransitionOutcome, BeltError> {
        self.write_tx(|tx| {
            let (_, step) = transition_in_tx(tx, req)?;
            Ok(match step {
                Step::Applied { seq } => TransitionOutcome::Applied { seq },
                Step::Rejected(outcome) => outcome,
                Step::HitlResponse => TransitionOutcome::InvalidAction {
                    current: QueuePhase::Hitl,
                },
            })
        })
    }

    /// Create the first item of a new lineage from collection.
    ///
    /// One transaction: read the phases of the same `(source_id, state)` and
    /// the highest issued sequence, apply
    /// [`belt_core::lineage::collect_decision`], then insert the Pending item
    /// (`lineage_root` = its own `work_id`, no origin) and an `item_created` row.
    ///
    /// # Errors
    /// `BeltError::Database` on I/O failure or unreadable stored phases.
    pub fn insert_collected(&self, new: &NewItem) -> Result<CollectOutcome, BeltError> {
        self.write_tx(|tx| {
            let (phases, max_seq) = read_series(tx, &new.source_id, &new.state)?;
            let work_id = match collect_decision(&phases, max_seq) {
                CollectDecision::Duplicate => return Ok(CollectOutcome::Duplicate),
                CollectDecision::New { seq } => issue_work_id(&new.source_id, &new.state, seq),
            };
            let mut item = QueueItem::new(
                work_id.clone(),
                new.source_id.clone(),
                new.workspace_id.clone(),
                new.state.clone(),
            );
            item.title = new.title.clone();
            insert_queue_row(tx, &item)?;
            append_log(
                tx,
                &LogRecord {
                    work_id: &work_id,
                    source_id: &new.source_id,
                    kind: transition_kind::ITEM_CREATED,
                    from_phase: None,
                    to_phase: Some(phase_to_str(QueuePhase::Pending)),
                    actor: &actor_str(&new.actor),
                    reason: Some("collected"),
                    detail: None,
                },
            )?;
            Ok(CollectOutcome::Inserted { work_id })
        })
    }

    /// End an item as Skipped and continue its work in a derived Pending item.
    ///
    /// One transaction: transition the origin to Skipped through the same
    /// contract as [`Database::transition`], then insert the derived item
    /// (next sequence, `derived_from` = origin, same `lineage_root`, inherited
    /// `replan_count`) with an `item_created` row naming the origin.
    /// [`DeriveKind::EscalationRetry`] hands the worktree over
    /// (`worktree_owner` = the origin's owner, or the origin itself);
    /// [`DeriveKind::Replan`] leaves the owner empty and records a
    /// failure-count reset point. If the origin's transition is refused,
    /// nothing is derived.
    ///
    /// # Errors
    /// `BeltError::ItemNotFound` for an unknown origin; `BeltError::Database`
    /// on I/O failure or when the `(source_id, state)` already has another open item.
    pub fn derive(&self, req: &DeriveRequest) -> Result<DeriveOutcome, BeltError> {
        self.write_tx(|tx| {
            let skip = TransitionRequest {
                work_id: req.work_id.clone(),
                expected_from: req.expected_from,
                to: QueuePhase::Skipped,
                actor: req.actor.clone(),
                reason: req.reason.clone(),
                detail: req.detail.clone(),
            };
            let (origin, step) = transition_in_tx(tx, &skip)?;
            match step {
                Step::Applied { .. } => {}
                Step::Rejected(outcome) => return Ok(DeriveOutcome::Rejected(outcome)),
                Step::HitlResponse => {
                    return Ok(DeriveOutcome::Rejected(TransitionOutcome::InvalidAction {
                        current: QueuePhase::Hitl,
                    }));
                }
            }

            let (phases, max_seq) = read_series(tx, &origin.item.source_id, &origin.item.state)?;
            let seq = match collect_decision(&phases, max_seq) {
                CollectDecision::New { seq: Some(n) } => n,
                CollectDecision::New { seq: None } | CollectDecision::Duplicate => {
                    return Err(BeltError::Database(format!(
                        "cannot derive from {}: another item of ({}, {}) is still open",
                        req.work_id, origin.item.source_id, origin.item.state
                    )));
                }
            };
            let work_id = issue_work_id(&origin.item.source_id, &origin.item.state, Some(seq));
            let mut derived = QueueItem::new(
                work_id.clone(),
                origin.item.source_id.clone(),
                origin.item.workspace_id.clone(),
                origin.item.state.clone(),
            );
            derived.title = origin.item.title.clone();
            derived.replan_count = origin.item.replan_count;
            derived.derived_from = Some(origin.item.work_id.clone());
            derived.lineage_root = origin.item.lineage_root.clone();
            insert_queue_row(tx, &derived)?;

            // Replan leaves Hitl as post-processing of a confirmed request;
            // completing it here keeps it out of `pending_post_processing`.
            tx.execute(
                "UPDATE hitl_requests SET post_processed_at = ?1
                 WHERE work_id = ?2 AND status IN ('resolved', 'expired')
                   AND post_processed_at IS NULL",
                params![Utc::now().to_rfc3339(), origin.item.work_id],
            )
            .map_err(sql_err)?;

            match req.kind {
                DeriveKind::EscalationRetry => {
                    let owner = origin
                        .worktree_owner
                        .as_deref()
                        .unwrap_or(&origin.item.work_id);
                    tx.execute(
                        "UPDATE queue_items SET worktree_owner = ?1 WHERE work_id = ?2",
                        params![owner, work_id],
                    )
                    .map_err(sql_err)?;
                }
                DeriveKind::Replan => insert_reset_row(
                    tx,
                    &origin.item.source_id,
                    &origin.item.state,
                    &origin.item.work_id,
                )?,
            }

            append_log(
                tx,
                &LogRecord {
                    work_id: &work_id,
                    source_id: &origin.item.source_id,
                    kind: transition_kind::ITEM_CREATED,
                    from_phase: None,
                    to_phase: Some(phase_to_str(QueuePhase::Pending)),
                    actor: &actor_str(&req.actor),
                    reason: Some("derived"),
                    detail: Some(&origin.item.work_id),
                },
            )?;
            Ok(DeriveOutcome::Derived { work_id })
        })
    }

    /// Failures of the lineage `work_id` belongs to since its last reset point.
    ///
    /// Collects the attempt history of every item sharing `work_id`'s
    /// `lineage_root` in insertion order and applies
    /// [`belt_core::lineage::count_since_reset`]. A re-collected item starts a
    /// new lineage and does not inherit earlier lineages' failures.
    ///
    /// # Errors
    /// `BeltError::ItemNotFound` for an unknown `work_id`.
    /// `BeltError::Database` on I/O failure or a history status that maps to
    /// no attempt status (`failed`, `reset`, `running`, `completed`/`done`/`success`,
    /// `skipped`, `hitl`).
    pub fn failure_count(&self, work_id: &str) -> Result<u32, BeltError> {
        let conn = self.lock_conn()?;
        let root = lineage_root_of(&conn, work_id)?;
        let mut stmt = conn
            .prepare(
                "SELECT h.status FROM history h
                 JOIN queue_items q ON q.work_id = h.work_id
                 WHERE q.lineage_root = ?1 ORDER BY h.id",
            )
            .map_err(sql_err)?;
        let attempts = stmt
            .query_map(params![root], |row| row.get::<_, String>(0))
            .map_err(sql_err)?
            .map(|status| attempt_status(&status.map_err(sql_err)?))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(count_since_reset(&attempts))
    }

    /// Record a failure-count reset point on the lineage of `work_id`
    /// (the HITL-retried item).
    ///
    /// # Errors
    /// `BeltError::ItemNotFound` for an unknown `work_id`.
    /// `BeltError::Database` on I/O failure.
    pub fn record_reset(&self, work_id: &str) -> Result<(), BeltError> {
        self.write_tx(|tx| {
            let (source_id, state): (String, String) = tx
                .query_row(
                    "SELECT source_id, state FROM queue_items WHERE work_id = ?1",
                    params![work_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(sql_err)?
                .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))?;
            insert_reset_row(tx, &source_id, &state, work_id)
        })
    }

    /// Transition log rows with `seq` greater than the cursor, oldest first.
    ///
    /// # Errors
    /// `BeltError::Database` on I/O failure.
    pub fn transitions_since(&self, seq: u64) -> Result<Vec<TransitionLogEntry>, BeltError> {
        let cursor = i64::try_from(seq)
            .map_err(|_| BeltError::Database(format!("seq cursor out of range: {seq}")))?;
        self.query_log("WHERE seq > ?1", params![cursor])
    }

    /// All transition log rows of one item, oldest first.
    ///
    /// # Errors
    /// `BeltError::Database` on I/O failure.
    pub fn transitions_of(&self, work_id: &str) -> Result<Vec<TransitionLogEntry>, BeltError> {
        self.query_log("WHERE work_id = ?1", params![work_id])
    }

    /// The most recently created item of the lineage `work_id` belongs to.
    ///
    /// Creation order is the `item_created` log sequence, which survives row
    /// re-insertion and `VACUUM`; legacy items without an `item_created` row
    /// sort oldest and fall back to `created_at`.
    ///
    /// # Errors
    /// `BeltError::ItemNotFound` for an unknown `work_id`.
    pub fn latest_in_lineage(&self, work_id: &str) -> Result<QueueItem, BeltError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {QUEUE_ITEM_COLUMNS} FROM queue_items
                 WHERE lineage_root = (SELECT lineage_root FROM queue_items WHERE work_id = ?1)
                 ORDER BY (SELECT MAX(t.seq) FROM transition_log t
                           WHERE t.work_id = queue_items.work_id AND t.kind = '{kind}')
                          DESC NULLS LAST,
                          created_at DESC, rowid DESC
                 LIMIT 1",
                kind = transition_kind::ITEM_CREATED
            ))
            .map_err(sql_err)?;
        let mut rows = stmt.query(params![work_id]).map_err(sql_err)?;
        match rows.next().map_err(sql_err)? {
            Some(row) => row_to_queue_item(row),
            None => Err(BeltError::ItemNotFound(work_id.to_string())),
        }
    }

    /// The key of the worktree `work_id` works in.
    ///
    /// An item handed a worktree by escalation retry works in the worktree
    /// of the item it was first created for (its `worktree_owner`); any
    /// other item works in its own, keyed by its `work_id`.
    ///
    /// # Errors
    /// `BeltError::ItemNotFound` for an unknown `work_id`.
    pub fn worktree_key(&self, work_id: &str) -> Result<String, BeltError> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT COALESCE(worktree_owner, work_id) FROM queue_items WHERE work_id = ?1",
            params![work_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_err)?
        .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))
    }

    /// The `work_id` of the item that currently owns the worktree keyed `key`.
    ///
    /// The owner is the most recently created item the worktree was handed
    /// to; a worktree never handed over is owned by the item `key` names.
    /// Only the owner's phase decides whether the worktree may be cleaned up.
    ///
    /// # Errors
    /// `BeltError::Database` on I/O failure.
    pub fn worktree_holder(&self, key: &str) -> Result<String, BeltError> {
        let conn = self.lock_conn()?;
        let handed: Option<String> = conn
            .query_row(
                &format!(
                    "SELECT work_id FROM queue_items WHERE worktree_owner = ?1
                     ORDER BY (SELECT MAX(t.seq) FROM transition_log t
                               WHERE t.work_id = queue_items.work_id AND t.kind = '{kind}')
                              DESC NULLS LAST,
                              created_at DESC, rowid DESC
                     LIMIT 1",
                    kind = transition_kind::ITEM_CREATED
                ),
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        Ok(handed.unwrap_or_else(|| key.to_string()))
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, BeltError> {
        self.conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))
    }

    /// Run `f` in a `BEGIN IMMEDIATE` transaction; commit when it returns `Ok`.
    fn write_tx<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, BeltError>,
    ) -> Result<T, BeltError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let value = f(&tx)?;
        tx.commit().map_err(sql_err)?;
        Ok(value)
    }

    fn query_log(
        &self,
        filter: &str,
        args: impl rusqlite::Params,
    ) -> Result<Vec<TransitionLogEntry>, BeltError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT seq, work_id, source_id, kind, from_phase, to_phase, actor, reason, detail, created_at
                 FROM transition_log {filter} ORDER BY seq"
            ))
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(args, |row| {
                Ok(TransitionLogEntry {
                    seq: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                    work_id: row.get(1)?,
                    source_id: row.get(2)?,
                    kind: row.get(3)?,
                    from_phase: row.get(4)?,
                    to_phase: row.get(5)?,
                    actor: row.get(6)?,
                    reason: row.get(7)?,
                    detail: row.get(8)?,
                    created_at: row.get(9)?,
                })
            })
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        Ok(rows)
    }
}

impl Database {
    /// Enter Hitl and open a HITL request, in one transaction.
    ///
    /// Runs the `expected_from -> Hitl` transition through the transition
    /// contract and, only if it is applied, inserts the open request. A
    /// request cannot exist without the phase change or vice versa. An item
    /// that already has an open request is in Hitl, so a second open is
    /// refused by the transition contract (`InvalidAction { Hitl }`).
    ///
    /// The `hitl_id` is `hitl-{seq}` with the `transition_log.seq` of the
    /// entering transition: unique for the lifetime of the database and
    /// disjoint from migrated `hitl-legacy-{n}` ids.
    ///
    /// # Errors
    /// `BeltError::ItemNotFound` for an unknown `work_id`; `BeltError::Database`
    /// on I/O failure or a violated request constraint (everything is rolled back).
    pub fn open_hitl(&self, req: &OpenHitlRequest) -> Result<OpenHitlOutcome, BeltError> {
        self.write_tx(|tx| {
            let entering = TransitionRequest {
                work_id: req.work_id.clone(),
                expected_from: req.expected_from,
                to: QueuePhase::Hitl,
                actor: req.actor.clone(),
                reason: req.transition_reason.clone(),
                detail: Some(req.reason.to_string()),
            };
            let (_, step) = transition_in_tx(tx, &entering)?;
            let seq = match step {
                Step::Applied { seq } => seq,
                Step::Rejected(outcome) => return Ok(OpenHitlOutcome::Rejected(outcome)),
                Step::HitlResponse => {
                    return Ok(OpenHitlOutcome::Rejected(
                        TransitionOutcome::InvalidAction {
                            current: QueuePhase::Hitl,
                        },
                    ));
                }
            };
            let hitl_id = HitlId::new(format!("hitl-{seq}"));
            tx.execute(
                "INSERT INTO hitl_requests
                 (hitl_id, work_id, status, reason, notes, opened_at, timeout_at, terminal_action)
                 VALUES (?1, ?2, 'open', ?3, ?4, ?5, ?6, ?7)",
                params![
                    hitl_id.as_str(),
                    req.work_id,
                    req.reason.to_string(),
                    req.notes,
                    Utc::now().to_rfc3339(),
                    req.timeout_at,
                    req.terminal_action.map(|a| a.to_string()),
                ],
            )
            .map_err(sql_err)?;
            Ok(OpenHitlOutcome::Opened { hitl_id, seq })
        })
    }

    /// Confirm a response, first response wins.
    ///
    /// Only an open request can be resolved. A request that is already
    /// resolved or expired yields `AlreadyHandled` carrying the winning
    /// response, so a loser is a value and never a database error. The item's
    /// phase is untouched: it leaves Hitl only through
    /// [`Database::complete_post_processing`].
    ///
    /// [`HitlTarget::Item`] addresses only the item's current request (open,
    /// or confirmed and not yet post-processed); otherwise `NotFound`.
    ///
    /// # Errors
    /// `BeltError::Database` on I/O failure or an inconsistent stored request.
    pub fn resolve_hitl(
        &self,
        target: &HitlTarget,
        resolution: &HitlResolution,
        notes: Option<&str>,
    ) -> Result<RespondOutcome, BeltError> {
        self.write_tx(|tx| confirm_in_tx(tx, target, resolution, notes))
    }

    /// Record a response of `by` through `via` that was refused as
    /// unauthorized, in the `transition_log`. The request is left untouched.
    ///
    /// Returns `false` when `target` names no request (nothing to attribute
    /// the rejection to).
    ///
    /// # Errors
    /// `BeltError::Database` on I/O failure or an inconsistent stored request.
    pub fn record_unauthorized_response(
        &self,
        target: &HitlTarget,
        by: &str,
        via: &str,
    ) -> Result<bool, BeltError> {
        self.write_tx(|tx| {
            let Some(request) = find_hitl(tx, target)? else {
                return Ok(false);
            };
            append_rejection(tx, &request, REJECT_UNAUTHORIZED, by, via, None)?;
            Ok(true)
        })
    }

    /// Every open request, oldest first.
    ///
    /// # Errors
    /// `BeltError::Database` on I/O failure or an inconsistent stored request.
    pub fn open_hitl_requests(&self) -> Result<Vec<HitlRequest>, BeltError> {
        let conn = self.lock_conn()?;
        read_open_requests(&conn)
    }

    /// Claim the `on_hitl_opened` notification of every open request that
    /// has not been claimed yet, and return those requests.
    ///
    /// The claim is stored before the hook runs, so each request is handed
    /// out exactly once: a failing hook is not retried, and a request that
    /// was confirmed before it was observed is never handed out.
    ///
    /// # Errors
    /// `BeltError::Database` on I/O failure or an inconsistent stored request.
    pub fn claim_opened_hooks(&self) -> Result<Vec<HitlRequest>, BeltError> {
        self.write_tx(|tx| {
            let now = Utc::now().to_rfc3339();
            let mut claimed = Vec::new();
            for request in read_open_requests(tx)? {
                let changed = tx
                    .execute(
                        "UPDATE hitl_requests SET opened_hook_done_at = ?1
                         WHERE hitl_id = ?2 AND status = 'open' AND opened_hook_done_at IS NULL",
                        params![now, request.hitl_id.as_str()],
                    )
                    .map_err(sql_err)?;
                if changed == 1 {
                    claimed.push(request);
                }
            }
            Ok(claimed)
        })
    }

    /// Expire an open request by timeout, applying `terminal`.
    ///
    /// Races with [`Database::resolve_hitl`] on the same open-state
    /// compare-and-set: whichever is confirmed first wins and the other gets
    /// `AlreadyHandled`. The expiry is recorded as a resolution by `system`
    /// via `timeout` with the terminal action. Only `skip` and `replan` are
    /// terminal actions; any other value is `InvalidAction`.
    ///
    /// # Errors
    /// `BeltError::Database` on I/O failure or an inconsistent stored request.
    pub fn expire_hitl(
        &self,
        hitl_id: &HitlId,
        terminal: EscalationAction,
    ) -> Result<RespondOutcome, BeltError> {
        let Some(action) = belt_core::hitl::expiry_action(terminal) else {
            return Ok(RespondOutcome::InvalidAction);
        };
        let expiry = HitlResolution {
            action,
            by: EXPIRY_BY.to_string(),
            via: EXPIRY_VIA.to_string(),
            at: Utc::now().to_rfc3339(),
            path: ConfirmPath::Direct,
        };
        self.write_tx(|tx| {
            let Some(current) = find_hitl(tx, &HitlTarget::Id(hitl_id.clone()))? else {
                return Ok(RespondOutcome::NotFound);
            };
            match current.status {
                HitlStatus::Open => {
                    confirm_open(tx, &current.hitl_id, HitlStatus::Expired, &expiry, None)?;
                    Ok(RespondOutcome::Won {
                        hitl_id: current.hitl_id,
                    })
                }
                HitlStatus::Resolved | HitlStatus::Expired => {
                    Ok(RespondOutcome::AlreadyHandled(stored_resolution(&current)?))
                }
            }
        })
    }

    /// One request by id, or `None`.
    ///
    /// # Errors
    /// `BeltError::Database` on I/O failure or an inconsistent stored request.
    pub fn hitl_request(&self, hitl_id: &HitlId) -> Result<Option<HitlRequest>, BeltError> {
        let conn = self.lock_conn()?;
        find_hitl(&conn, &HitlTarget::Id(hitl_id.clone()))
    }

    /// Requests that are confirmed (resolved or expired) and not yet
    /// post-processed, oldest confirmation first. The daemon works through
    /// these; each one holds its item in Hitl as "processing".
    ///
    /// # Errors
    /// `BeltError::Database` on I/O failure or an inconsistent stored request.
    pub fn pending_post_processing(&self) -> Result<Vec<HitlRequest>, BeltError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {HITL_COLUMNS} FROM hitl_requests
                 WHERE status IN ('resolved', 'expired') AND post_processed_at IS NULL
                 ORDER BY resolved_at, rowid"
            ))
            .map_err(sql_err)?;
        let mut rows = stmt.query([]).map_err(sql_err)?;
        let mut pending = Vec::new();
        while let Some(row) = rows.next().map_err(sql_err)? {
            pending.push(row_to_hitl_request(row)?);
        }
        Ok(pending)
    }

    /// Apply the post-processing result transition and mark the request
    /// processed, in one transaction (crash-safe: both or neither).
    ///
    /// `result.work_id` must be the request's item. If the transition is
    /// refused the request stays pending.
    ///
    /// # Errors
    /// `BeltError::ItemNotFound` for an unknown `hitl_id`; `BeltError::Database`
    /// when `result` names another item, or on I/O failure.
    pub fn complete_post_processing(
        &self,
        hitl_id: &HitlId,
        result: &TransitionRequest,
    ) -> Result<CompleteOutcome, BeltError> {
        self.write_tx(|tx| {
            let Some(request) = find_hitl(tx, &HitlTarget::Id(hitl_id.clone()))? else {
                return Err(BeltError::ItemNotFound(hitl_id.to_string()));
            };
            if request.work_id != result.work_id {
                return Err(BeltError::Database(format!(
                    "post-processing result for {} does not belong to HITL request {hitl_id} of {}",
                    result.work_id, request.work_id
                )));
            }
            if request.status == HitlStatus::Open || request.post_processed_at.is_some() {
                return Ok(CompleteOutcome::NotPending);
            }
            let (_, step) = transition_in_tx(tx, result)?;
            let seq = match step {
                Step::Applied { seq } => seq,
                Step::Rejected(outcome) => return Ok(CompleteOutcome::Rejected(outcome)),
                Step::HitlResponse => {
                    return Ok(CompleteOutcome::Rejected(
                        TransitionOutcome::InvalidAction {
                            current: QueuePhase::Hitl,
                        },
                    ));
                }
            };
            let changed = tx
                .execute(
                    "UPDATE hitl_requests SET post_processed_at = ?1
                     WHERE hitl_id = ?2 AND post_processed_at IS NULL",
                    params![Utc::now().to_rfc3339(), hitl_id.as_str()],
                )
                .map_err(sql_err)?;
            if changed != 1 {
                return Err(BeltError::Database(format!(
                    "post-processing mark on {hitl_id} changed {changed} rows under an immediate transaction"
                )));
            }
            Ok(CompleteOutcome::Completed { seq })
        })
    }

    /// Count one more failed post-processing attempt and return the total.
    ///
    /// # Errors
    /// `BeltError::Database` when the request does not exist or is not
    /// awaiting post-processing (open, or already processed), or on I/O failure.
    pub fn record_post_processing_failure(&self, hitl_id: &HitlId) -> Result<u32, BeltError> {
        self.write_tx(|tx| {
            let changed = tx
                .execute(
                    "UPDATE hitl_requests
                     SET post_processing_failures = post_processing_failures + 1
                     WHERE hitl_id = ?1 AND status IN ('resolved', 'expired')
                       AND post_processed_at IS NULL",
                    params![hitl_id.as_str()],
                )
                .map_err(sql_err)?;
            if changed != 1 {
                return Err(BeltError::Database(format!(
                    "HITL request {hitl_id} is not awaiting post-processing"
                )));
            }
            tx.query_row(
                "SELECT post_processing_failures FROM hitl_requests WHERE hitl_id = ?1",
                params![hitl_id.as_str()],
                |r| r.get(0),
            )
            .map_err(sql_err)
        })
    }
}

/// Decide a response against the target request inside the caller's transaction.
fn confirm_in_tx(
    tx: &Transaction<'_>,
    target: &HitlTarget,
    resolution: &HitlResolution,
    notes: Option<&str>,
) -> Result<RespondOutcome, BeltError> {
    let Some(current) = find_hitl(tx, target)? else {
        return Ok(RespondOutcome::NotFound);
    };
    match current.status {
        HitlStatus::Open => {
            confirm_open(
                tx,
                &current.hitl_id,
                HitlStatus::Resolved,
                resolution,
                notes,
            )?;
            Ok(RespondOutcome::Won {
                hitl_id: current.hitl_id,
            })
        }
        HitlStatus::Resolved | HitlStatus::Expired => {
            let winner = stored_resolution(&current)?;
            append_rejection(
                tx,
                &current,
                REJECT_ALREADY_HANDLED,
                &resolution.by,
                &resolution.via,
                Some(resolution.action),
            )?;
            Ok(RespondOutcome::AlreadyHandled(winner))
        }
    }
}

/// Record a response of `by` through `via` refused against `request` in the
/// `transition_log`, inside the caller's transaction. `action` is `None`
/// when the response was refused before its action was read.
fn append_rejection(
    tx: &Transaction<'_>,
    request: &HitlRequest,
    reason: &str,
    by: &str,
    via: &str,
    action: Option<HitlAction>,
) -> Result<(), BeltError> {
    let source_id: String = tx
        .query_row(
            "SELECT source_id FROM queue_items WHERE work_id = ?1",
            params![request.work_id],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    let detail = serde_json::json!({
        "hitl_id": request.hitl_id.as_str(),
        "by": by,
        "via": via,
        "action": action.map(|a| a.to_string()),
        "winner": request.resolution.as_ref().map(|w| serde_json::json!({
            "by": w.by,
            "via": w.via,
            "action": w.action.to_string(),
            "at": w.at,
        })),
    })
    .to_string();
    append_log(
        tx,
        &LogRecord {
            work_id: &request.work_id,
            source_id: &source_id,
            kind: transition_kind::HITL_RESPONSE_REJECTED,
            from_phase: None,
            to_phase: None,
            actor: via,
            reason: Some(reason),
            detail: Some(&detail),
        },
    )?;
    Ok(())
}

/// Open -> `status` compare-and-set recording the winning resolution.
fn confirm_open(
    tx: &Transaction<'_>,
    hitl_id: &HitlId,
    status: HitlStatus,
    resolution: &HitlResolution,
    notes: Option<&str>,
) -> Result<(), BeltError> {
    let status = match status {
        HitlStatus::Resolved => "resolved",
        HitlStatus::Expired => "expired",
        HitlStatus::Open => {
            return Err(BeltError::Database(
                "a request cannot be confirmed into the open status".to_string(),
            ));
        }
    };
    let path = match resolution.path {
        ConfirmPath::Direct => "direct",
        ConfirmPath::NaturalLanguage => "natural_language",
    };
    let changed = tx
        .execute(
            "UPDATE hitl_requests
             SET status = ?1, action = ?2, respondent = ?3, via = ?4, confirm_path = ?5,
                 resolved_at = ?6, resolution_notes = ?7
             WHERE hitl_id = ?8 AND status = 'open'",
            params![
                status,
                resolution.action.to_string(),
                resolution.by,
                resolution.via,
                path,
                resolution.at,
                notes,
                hitl_id.as_str(),
            ],
        )
        .map_err(sql_err)?;
    if changed != 1 {
        return Err(BeltError::Database(format!(
            "open-state compare-and-set on {hitl_id} changed {changed} rows under an immediate transaction"
        )));
    }
    Ok(())
}

/// The request a target names. An item names only its current request: the
/// open one, or a confirmed one still awaiting post-processing. Requests
/// already post-processed belong to the item's past and are not found.
fn find_hitl(conn: &Connection, target: &HitlTarget) -> Result<Option<HitlRequest>, BeltError> {
    let (filter, key) = match target {
        HitlTarget::Id(id) => ("hitl_id = ?1", id.as_str()),
        HitlTarget::Item(work_id) => (
            "work_id = ?1 AND (status = 'open' OR post_processed_at IS NULL)",
            work_id.as_str(),
        ),
    };
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {HITL_COLUMNS} FROM hitl_requests WHERE {filter}
             ORDER BY (status = 'open') DESC, rowid DESC LIMIT 1"
        ))
        .map_err(sql_err)?;
    let mut rows = stmt.query(params![key]).map_err(sql_err)?;
    rows.next()
        .map_err(sql_err)?
        .map(row_to_hitl_request)
        .transpose()
}

fn read_open_requests(conn: &Connection) -> Result<Vec<HitlRequest>, BeltError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {HITL_COLUMNS} FROM hitl_requests WHERE status = 'open' ORDER BY rowid"
        ))
        .map_err(sql_err)?;
    let mut rows = stmt.query([]).map_err(sql_err)?;
    let mut open = Vec::new();
    while let Some(row) = rows.next().map_err(sql_err)? {
        open.push(row_to_hitl_request(row)?);
    }
    Ok(open)
}

/// The recorded winner of a confirmed request.
fn stored_resolution(request: &HitlRequest) -> Result<HitlResolution, BeltError> {
    request.resolution.clone().ok_or_else(|| {
        BeltError::Database(format!(
            "HITL request {} is {:?} but has no recorded resolution",
            request.hitl_id, request.status
        ))
    })
}

fn row_to_hitl_request(row: &rusqlite::Row<'_>) -> Result<HitlRequest, BeltError> {
    let hitl_id = HitlId::new(col::<String>(row, 0)?);
    let status = match col::<String>(row, 2)?.as_str() {
        "open" => HitlStatus::Open,
        "resolved" => HitlStatus::Resolved,
        "expired" => HitlStatus::Expired,
        other => {
            return Err(BeltError::Database(format!(
                "unknown hitl status of {hitl_id}: {other}"
            )));
        }
    };
    let action: Option<String> = col(row, 8)?;
    let resolution = match status {
        HitlStatus::Open => None,
        HitlStatus::Resolved | HitlStatus::Expired => {
            let missing = |field: &str| {
                BeltError::Database(format!("confirmed HITL request {hitl_id} has no {field}"))
            };
            let action = action.ok_or_else(|| missing("action"))?;
            let path = col::<Option<String>>(row, 11)?.ok_or_else(|| missing("confirm_path"))?;
            Some(HitlResolution {
                action: action
                    .parse::<HitlRespondAction>()
                    .map(HitlAction::from)
                    .map_err(BeltError::Database)?,
                by: col::<Option<String>>(row, 9)?.ok_or_else(|| missing("respondent"))?,
                via: col::<Option<String>>(row, 10)?.ok_or_else(|| missing("via"))?,
                at: col::<Option<String>>(row, 12)?.ok_or_else(|| missing("resolved_at"))?,
                path: match path.as_str() {
                    "direct" => ConfirmPath::Direct,
                    "natural_language" => ConfirmPath::NaturalLanguage,
                    other => {
                        return Err(BeltError::Database(format!(
                            "unknown confirm_path of {hitl_id}: {other}"
                        )));
                    }
                },
            })
        }
    };
    Ok(HitlRequest {
        work_id: col(row, 1)?,
        status,
        reason: col::<Option<String>>(row, 3)?
            .as_deref()
            .map(parse_hitl_reason)
            .transpose()?,
        notes: col(row, 4)?,
        opened_at: col(row, 5)?,
        timeout_at: col(row, 6)?,
        terminal_action: col::<Option<String>>(row, 7)?
            .as_deref()
            .map(|s| s.parse::<EscalationAction>().map_err(BeltError::Database))
            .transpose()?,
        resolution,
        resolution_notes: col(row, 13)?,
        post_processed_at: col(row, 14)?,
        post_processing_failures: col(row, 15)?,
        hitl_id,
    })
}

fn sql_err(e: rusqlite::Error) -> BeltError {
    BeltError::Database(e.to_string())
}

/// An item as read inside a transition transaction.
struct StoredItem {
    item: QueueItem,
    worktree_owner: Option<String>,
    snapshot: ItemSnapshot,
}

fn read_stored_item(tx: &Transaction<'_>, work_id: &str) -> Result<StoredItem, BeltError> {
    let mut stmt = tx
        .prepare(&format!(
            "SELECT {QUEUE_ITEM_COLUMNS}, worktree_owner FROM queue_items WHERE work_id = ?1"
        ))
        .map_err(sql_err)?;
    let mut rows = stmt.query(params![work_id]).map_err(sql_err)?;
    let Some(row) = rows.next().map_err(sql_err)? else {
        return Err(BeltError::ItemNotFound(work_id.to_string()));
    };
    let item = row_to_queue_item(row)?;
    let worktree_owner = col(row, 19)?;
    let processing = processing_of(tx, &item)?;
    let snapshot = ItemSnapshot {
        phase: item.phase(),
        processing,
    };
    Ok(StoredItem {
        item,
        worktree_owner,
        snapshot,
    })
}

/// Running means the daemon's handler owns the item; Hitl means a resolved or
/// expired request is waiting for the daemon's post-processing.
fn processing_of(tx: &Transaction<'_>, item: &QueueItem) -> Result<Option<Processing>, BeltError> {
    match item.phase() {
        QueuePhase::Running => Ok(Some(Processing::Handler)),
        QueuePhase::Hitl => {
            let waiting: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM hitl_requests
                     WHERE work_id = ?1 AND status IN ('resolved', 'expired') AND post_processed_at IS NULL)",
                    params![item.work_id],
                    |row| row.get(0),
                )
                .map_err(sql_err)?;
            Ok(waiting.then_some(Processing::PostProcessing))
        }
        QueuePhase::Pending
        | QueuePhase::Ready
        | QueuePhase::Completed
        | QueuePhase::Done
        | QueuePhase::Failed
        | QueuePhase::Skipped => Ok(None),
    }
}

/// Guard, then CAS and log, inside the caller's transaction.
fn transition_in_tx(
    tx: &Transaction<'_>,
    req: &TransitionRequest,
) -> Result<(StoredItem, Step), BeltError> {
    let stored = read_stored_item(tx, &req.work_id)?;
    let actor = actor_str(&req.actor);
    let current = phase_to_str(stored.snapshot.phase);
    let step = match guard(&stored.snapshot, req) {
        GuardDecision::Proceed => {
            let now = Utc::now().to_rfc3339();
            let changed = tx
                .execute(
                    "UPDATE queue_items SET phase = ?1, updated_at = ?2 WHERE work_id = ?3 AND phase = ?4",
                    params![phase_to_str(req.to), now, req.work_id, current],
                )
                .map_err(sql_err)?;
            if changed != 1 {
                return Err(BeltError::Database(format!(
                    "phase compare-and-set on {} changed {changed} rows under an immediate transaction",
                    req.work_id
                )));
            }
            let reason = reason_str(&req.reason);
            let seq = append_log(
                tx,
                &LogRecord {
                    work_id: &req.work_id,
                    source_id: &stored.item.source_id,
                    kind: transition_kind::PHASE_ENTER,
                    from_phase: Some(current),
                    to_phase: Some(phase_to_str(req.to)),
                    actor: &actor,
                    reason: Some(&reason),
                    detail: req.detail.as_deref(),
                },
            )?;
            Step::Applied { seq }
        }
        GuardDecision::Reject(outcome) => {
            let kind = match outcome {
                TransitionOutcome::Busy { .. } => Some(transition_kind::TRANSITION_REJECTED),
                TransitionOutcome::Conflict { .. } => Some(transition_kind::TRANSITION_CONFLICT),
                TransitionOutcome::InvalidAction { .. } | TransitionOutcome::Applied { .. } => None,
            };
            if let Some(kind) = kind {
                append_log(
                    tx,
                    &LogRecord {
                        work_id: &req.work_id,
                        source_id: &stored.item.source_id,
                        kind,
                        from_phase: Some(current),
                        to_phase: Some(phase_to_str(req.to)),
                        actor: &actor,
                        reason: None,
                        detail: req.detail.as_deref(),
                    },
                )?;
            }
            Step::Rejected(outcome)
        }
        GuardDecision::ConvertToHitlResponse(_) => Step::HitlResponse,
    };
    Ok((stored, step))
}

fn append_log(tx: &Transaction<'_>, rec: &LogRecord<'_>) -> Result<u64, BeltError> {
    tx.execute(
        "INSERT INTO transition_log
         (work_id, source_id, kind, from_phase, to_phase, actor, reason, detail, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            rec.work_id,
            rec.source_id,
            rec.kind,
            rec.from_phase,
            rec.to_phase,
            rec.actor,
            rec.reason,
            rec.detail,
            Utc::now().to_rfc3339(),
        ],
    )
    .map_err(sql_err)?;
    u64::try_from(tx.last_insert_rowid())
        .map_err(|_| BeltError::Database("transition_log seq out of range".to_string()))
}

/// Phases of every item of `(source_id, state)` and the highest sequence ever
/// issued for it. The base `work_id` counts as 1. Issued ids are also read
/// from the transition log, so a removed item's `work_id` is never reissued.
fn read_series(
    tx: &Transaction<'_>,
    source_id: &str,
    state: &str,
) -> Result<(Vec<QueuePhase>, Option<u32>), BeltError> {
    let phases = {
        let mut stmt = tx
            .prepare("SELECT phase FROM queue_items WHERE source_id = ?1 AND state = ?2")
            .map_err(sql_err)?;
        stmt.query_map(params![source_id, state], |row| row.get::<_, String>(0))
            .map_err(sql_err)?
            .map(|phase| str_to_phase(&phase.map_err(sql_err)?))
            .collect::<Result<Vec<_>, _>>()?
    };

    let base = QueueItem::make_work_id(source_id, state);
    let prefix = format!("{base}:");
    let issued = {
        let mut stmt = tx
            .prepare(
                "SELECT work_id FROM queue_items WHERE source_id = ?1 AND state = ?2
                 UNION
                 SELECT work_id FROM transition_log WHERE source_id = ?1 AND substr(work_id, 1, ?3) = ?4",
            )
            .map_err(sql_err)?;
        let prefix_len = i64::try_from(base.chars().count())
            .map_err(|_| BeltError::Database("work_id prefix too long".to_string()))?;
        stmt.query_map(params![source_id, state, prefix_len, base], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?
    };
    let max_seq = issued
        .iter()
        .filter_map(|id| {
            if *id == base {
                Some(1)
            } else {
                id.strip_prefix(&prefix)?.parse::<u32>().ok()
            }
        })
        .max();
    Ok((phases, max_seq))
}

fn issue_work_id(source_id: &str, state: &str, seq: Option<u32>) -> String {
    match seq {
        None => QueueItem::make_work_id(source_id, state),
        Some(n) => QueueItem::make_derived_work_id(source_id, state, n),
    }
}

/// Insert a `queue_items` row. An empty `lineage_root` (serde default for
/// items deserialized without one) is rejected: every item belongs to a lineage.
fn insert_queue_row(conn: &Connection, item: &QueueItem) -> Result<(), BeltError> {
    if item.lineage_root.is_empty() {
        return Err(BeltError::Database(format!(
            "queue item {} has an empty lineage_root",
            item.work_id
        )));
    }
    conn.execute(
        &format!(
            "INSERT INTO queue_items ({QUEUE_ITEM_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)"
        ),
        params![
            item.work_id,
            item.source_id,
            item.workspace_id,
            item.state,
            phase_to_str(item.phase()),
            item.title,
            item.created_at,
            item.updated_at,
            item.hitl_created_at,
            item.hitl_respondent,
            item.hitl_notes,
            item.hitl_reason.map(|r| r.to_string()),
            item.hitl_timeout_at,
            item.hitl_terminal_action.map(|a| a.to_string()),
            item.replan_count,
            item.worktree_preserved,
            item.previous_worktree_path,
            item.derived_from,
            item.lineage_root,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn lineage_root_of(conn: &Connection, work_id: &str) -> Result<String, BeltError> {
    conn.query_row(
        "SELECT lineage_root FROM queue_items WHERE work_id = ?1",
        params![work_id],
        |r| r.get(0),
    )
    .optional()
    .map_err(sql_err)?
    .ok_or_else(|| BeltError::ItemNotFound(work_id.to_string()))
}

fn insert_reset_row(
    tx: &Transaction<'_>,
    source_id: &str,
    state: &str,
    work_id: &str,
) -> Result<(), BeltError> {
    tx.execute(
        "INSERT INTO history (work_id, source_id, state, status, attempt, summary, error, created_at)
         VALUES (?1, ?2, ?3, ?4, 0, NULL, NULL, ?5)",
        params![work_id, source_id, state, HISTORY_STATUS_RESET, Utc::now().to_rfc3339()],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// `history.status` of a failure-count reset point.
const HISTORY_STATUS_RESET: &str = "reset";

fn attempt_status(status: &str) -> Result<AttemptStatus, BeltError> {
    match status {
        "failed" => Ok(AttemptStatus::Failed),
        HISTORY_STATUS_RESET => Ok(AttemptStatus::Reset),
        "running" => Ok(AttemptStatus::Running),
        "completed" | "done" | "success" => Ok(AttemptStatus::Done),
        "skipped" => Ok(AttemptStatus::Skipped),
        "hitl" => Ok(AttemptStatus::Hitl),
        other => Err(BeltError::Database(format!(
            "unknown history status: {other}"
        ))),
    }
}

fn actor_str(actor: &Actor) -> String {
    match actor {
        Actor::Daemon => "daemon".to_string(),
        Actor::Cli => "cli".to_string(),
        Actor::Tui => "tui".to_string(),
        Actor::Cron => "cron".to_string(),
        Actor::Channel(name) => name.clone(),
    }
}

fn reason_str(reason: &TransitionReason) -> String {
    match reason {
        TransitionReason::Manual => "manual".to_string(),
        TransitionReason::Advance => "advance".to_string(),
        TransitionReason::Derived => "derived".to_string(),
        TransitionReason::Canceled => "canceled".to_string(),
        TransitionReason::Rollback => "rollback".to_string(),
        TransitionReason::Escalation(action) => format!("escalation:{action}"),
        TransitionReason::PostProcessing(action) => format!("post_processing:{action}"),
    }
}

// ---- Helpers ---------------------------------------------------------------

/// Convert a `QueuePhase` to its database string representation.
fn phase_to_str(phase: QueuePhase) -> &'static str {
    match phase {
        QueuePhase::Pending => "pending",
        QueuePhase::Ready => "ready",
        QueuePhase::Running => "running",
        QueuePhase::Completed => "completed",
        QueuePhase::Done => "done",
        QueuePhase::Hitl => "hitl",
        QueuePhase::Failed => "failed",
        QueuePhase::Skipped => "skipped",
    }
}

/// Parse a database phase string back into a `QueuePhase`.
///
/// # Errors
/// Returns `BeltError::Database` for unrecognised phase values.
fn str_to_phase(s: &str) -> Result<QueuePhase, BeltError> {
    match s {
        "pending" => Ok(QueuePhase::Pending),
        "ready" => Ok(QueuePhase::Ready),
        "running" => Ok(QueuePhase::Running),
        "completed" => Ok(QueuePhase::Completed),
        "done" => Ok(QueuePhase::Done),
        "hitl" => Ok(QueuePhase::Hitl),
        "failed" => Ok(QueuePhase::Failed),
        "skipped" => Ok(QueuePhase::Skipped),
        _ => Err(BeltError::Database(format!("unknown phase: {s}"))),
    }
}

/// Parse an RFC 3339 timestamp string into `DateTime<Utc>`.
///
/// # Errors
/// Returns `BeltError::Database` if the string cannot be parsed.
fn parse_datetime(s: &str) -> Result<DateTime<Utc>, BeltError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| BeltError::Database(format!("invalid datetime: {s}")))
}

fn parse_hitl_reason(s: &str) -> Result<HitlReason, BeltError> {
    match s {
        "evaluate_failure" => Ok(HitlReason::EvaluateFailure),
        "retry_max_exceeded" => Ok(HitlReason::RetryMaxExceeded),
        "timeout" => Ok(HitlReason::Timeout),
        "manual_escalation" => Ok(HitlReason::ManualEscalation),
        "spec_conflict" => Ok(HitlReason::SpecConflict),
        "spec_completion_review" => Ok(HitlReason::SpecCompletionReview),
        "spec_modification_proposed" => Ok(HitlReason::SpecModificationProposed),
        "stagnation_detected" => Ok(HitlReason::StagnationDetected),
        other => Err(BeltError::Database(format!("unknown hitl_reason: {other}"))),
    }
}

/// Extract a `QueueItem` from a rusqlite `Row`.
///
/// Column order must match [`QUEUE_ITEM_COLUMNS`].
fn row_to_queue_item(row: &rusqlite::Row<'_>) -> Result<QueueItem, BeltError> {
    let phase_str: String = col(row, 4)?;
    let hitl_reason_str: Option<String> = col(row, 11)?;
    let hitl_reason = hitl_reason_str
        .as_deref()
        .map(parse_hitl_reason)
        .transpose()?;

    let mut item = QueueItem::new(col(row, 0)?, col(row, 1)?, col(row, 2)?, col(row, 3)?);
    item.set_phase_unchecked(str_to_phase(&phase_str)?);
    item.title = col(row, 5)?;
    item.created_at = col(row, 6)?;
    item.updated_at = col(row, 7)?;
    item.hitl_created_at = col(row, 8)?;
    item.hitl_respondent = col(row, 9)?;
    item.hitl_notes = col(row, 10)?;
    item.hitl_reason = hitl_reason;
    item.hitl_timeout_at = col(row, 12)?;
    item.hitl_terminal_action = col::<Option<String>>(row, 13)?
        .as_deref()
        .map(|s| s.parse::<EscalationAction>().map_err(BeltError::Database))
        .transpose()?;
    item.replan_count = row.get::<_, u32>(14).unwrap_or(0);
    item.worktree_preserved = col(row, 15)?;
    item.previous_worktree_path = col(row, 16)?;
    item.derived_from = col(row, 17)?;
    item.lineage_root = col(row, 18)?;
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        Database::open_in_memory().expect("in-memory DB should open")
    }

    fn sample_item() -> QueueItem {
        QueueItem::new(
            "gh:org/repo#1:implement".to_string(),
            "gh:org/repo#1".to_string(),
            "my-ws".to_string(),
            "implement".to_string(),
        )
    }

    // ---- Queue CRUD --------------------------------------------------------

    #[test]
    fn insert_and_get_item() {
        let db = test_db();
        let item = sample_item();
        db.insert_item(&item).unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(fetched.work_id, item.work_id);
        assert_eq!(fetched.source_id, item.source_id);
        assert_eq!(fetched.phase(), QueuePhase::Pending);
    }

    #[test]
    fn get_item_not_found() {
        let db = test_db();
        let err = db.get_item("nonexistent").unwrap_err();
        assert!(matches!(err, BeltError::ItemNotFound(_)));
    }

    #[test]
    fn update_phase() {
        let db = test_db();
        let item = sample_item();
        db.insert_item(&item).unwrap();
        db.update_phase(&item.work_id, QueuePhase::Ready).unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(fetched.phase(), QueuePhase::Ready);
    }

    #[test]
    fn update_phase_not_found() {
        let db = test_db();
        let err = db
            .update_phase("nonexistent", QueuePhase::Ready)
            .unwrap_err();
        assert!(matches!(err, BeltError::ItemNotFound(_)));
    }

    #[test]
    fn escalate_to_hitl_sets_metadata() {
        let db = test_db();
        let item = sample_item();
        db.insert_item(&item).unwrap();

        db.escalate_to_hitl(&item.work_id, "evaluate_failure", "failed 3 times")
            .unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(fetched.phase(), QueuePhase::Hitl);
        assert!(fetched.hitl_created_at.is_some());
        assert_eq!(fetched.hitl_notes.as_deref(), Some("failed 3 times"));
    }

    #[test]
    fn escalate_to_hitl_not_found() {
        let db = test_db();
        let err = db
            .escalate_to_hitl("nonexistent", "evaluate_failure", "error")
            .unwrap_err();
        assert!(matches!(err, BeltError::ItemNotFound(_)));
    }

    #[test]
    fn increment_replan_count_returns_new_value() {
        let db = test_db();
        let item = sample_item();
        db.insert_item(&item).unwrap();

        let count = db.increment_replan_count(&item.work_id).unwrap();
        assert_eq!(count, 1);

        let count = db.increment_replan_count(&item.work_id).unwrap();
        assert_eq!(count, 2);

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(fetched.replan_count, 2);
    }

    #[test]
    fn increment_replan_count_not_found() {
        let db = test_db();
        let err = db.increment_replan_count("nonexistent").unwrap_err();
        assert!(matches!(err, BeltError::ItemNotFound(_)));
    }

    #[test]
    fn list_items_no_filter() {
        let db = test_db();
        let item = sample_item();
        db.insert_item(&item).unwrap();

        let items = db.list_items(None, None).unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn list_items_filter_by_phase() {
        let db = test_db();
        let item = sample_item();
        db.insert_item(&item).unwrap();

        let items = db.list_items(Some(QueuePhase::Pending), None).unwrap();
        assert_eq!(items.len(), 1);

        let items = db.list_items(Some(QueuePhase::Running), None).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn list_items_filter_by_workspace() {
        let db = test_db();
        let item = sample_item();
        db.insert_item(&item).unwrap();

        let items = db.list_items(None, Some("my-ws")).unwrap();
        assert_eq!(items.len(), 1);

        let items = db.list_items(None, Some("other")).unwrap();
        assert!(items.is_empty());
    }

    // ---- History -----------------------------------------------------------

    #[test]
    fn append_and_get_history() {
        let db = test_db();
        let event = HistoryEvent {
            work_id: "w1".to_string(),
            source_id: "s1".to_string(),
            state: "implement".to_string(),
            status: "success".to_string(),
            attempt: 1,
            summary: Some("all good".to_string()),
            error: None,
            created_at: Utc::now().to_rfc3339(),
        };
        db.append_history(&event).unwrap();

        let history = db.get_history("s1").unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].work_id, "w1");
        assert_eq!(history[0].attempt, 1);
    }

    #[test]
    fn count_failures() {
        let db = test_db();
        for i in 0..3 {
            let event = HistoryEvent {
                work_id: format!("w{i}"),
                source_id: "s1".to_string(),
                state: "implement".to_string(),
                status: "failed".to_string(),
                attempt: i + 1,
                summary: None,
                error: Some("boom".to_string()),
                created_at: Utc::now().to_rfc3339(),
            };
            db.append_history(&event).unwrap();
        }
        // One success
        let ok_event = HistoryEvent {
            work_id: "w_ok".to_string(),
            source_id: "s1".to_string(),
            state: "implement".to_string(),
            status: "success".to_string(),
            attempt: 4,
            summary: None,
            error: None,
            created_at: Utc::now().to_rfc3339(),
        };
        db.append_history(&ok_event).unwrap();

        assert_eq!(db.count_failures("s1", "implement").unwrap(), 3);
    }

    #[test]
    fn get_history_returns_full_history_across_states_for_source_id() {
        let db = test_db();
        let source_id = "github:org/repo#42";

        // Simulate a source_id progressing through multiple states with different work_ids.
        let events = vec![
            HistoryEvent {
                work_id: "github:org/repo#42:analyze".to_string(),
                source_id: source_id.to_string(),
                state: "analyze".to_string(),
                status: "done".to_string(),
                attempt: 1,
                summary: Some("analysis complete".to_string()),
                error: None,
                created_at: "2026-03-20T01:00:00Z".to_string(),
            },
            HistoryEvent {
                work_id: "github:org/repo#42:implement".to_string(),
                source_id: source_id.to_string(),
                state: "implement".to_string(),
                status: "failed".to_string(),
                attempt: 1,
                summary: None,
                error: Some("compile error".to_string()),
                created_at: "2026-03-20T02:00:00Z".to_string(),
            },
            HistoryEvent {
                work_id: "github:org/repo#42:implement".to_string(),
                source_id: source_id.to_string(),
                state: "implement".to_string(),
                status: "done".to_string(),
                attempt: 2,
                summary: Some("implemented".to_string()),
                error: None,
                created_at: "2026-03-20T03:00:00Z".to_string(),
            },
            HistoryEvent {
                work_id: "github:org/repo#42:review".to_string(),
                source_id: source_id.to_string(),
                state: "review".to_string(),
                status: "done".to_string(),
                attempt: 1,
                summary: Some("review passed".to_string()),
                error: None,
                created_at: "2026-03-20T04:00:00Z".to_string(),
            },
        ];

        for event in &events {
            db.append_history(event).unwrap();
        }

        let history = db.get_history(source_id).unwrap();
        assert_eq!(
            history.len(),
            4,
            "should return all 4 events for the source_id"
        );

        // Verify chronological order (ORDER BY created_at ASC).
        assert_eq!(history[0].state, "analyze");
        assert_eq!(history[1].state, "implement");
        assert_eq!(history[1].status, "failed");
        assert_eq!(history[2].state, "implement");
        assert_eq!(history[2].status, "done");
        assert_eq!(history[2].attempt, 2);
        assert_eq!(history[3].state, "review");

        // Verify all work_ids are distinct (different phases produce different work_ids).
        assert_eq!(history[0].work_id, "github:org/repo#42:analyze");
        assert_eq!(history[1].work_id, "github:org/repo#42:implement");
        assert_eq!(history[3].work_id, "github:org/repo#42:review");

        // Verify optional fields are preserved.
        assert_eq!(history[0].summary.as_deref(), Some("analysis complete"));
        assert_eq!(history[1].error.as_deref(), Some("compile error"));
        assert!(history[0].error.is_none());
    }

    #[test]
    fn get_history_isolates_by_source_id() {
        let db = test_db();

        // Insert history for two different source_ids.
        let events = vec![
            HistoryEvent {
                work_id: "github:org/repo#42:analyze".to_string(),
                source_id: "github:org/repo#42".to_string(),
                state: "analyze".to_string(),
                status: "done".to_string(),
                attempt: 1,
                summary: None,
                error: None,
                created_at: "2026-03-20T01:00:00Z".to_string(),
            },
            HistoryEvent {
                work_id: "github:org/repo#42:implement".to_string(),
                source_id: "github:org/repo#42".to_string(),
                state: "implement".to_string(),
                status: "done".to_string(),
                attempt: 1,
                summary: None,
                error: None,
                created_at: "2026-03-20T02:00:00Z".to_string(),
            },
            HistoryEvent {
                work_id: "github:org/repo#99:analyze".to_string(),
                source_id: "github:org/repo#99".to_string(),
                state: "analyze".to_string(),
                status: "failed".to_string(),
                attempt: 1,
                summary: None,
                error: Some("timeout".to_string()),
                created_at: "2026-03-20T01:30:00Z".to_string(),
            },
        ];

        for event in &events {
            db.append_history(event).unwrap();
        }

        let history_42 = db.get_history("github:org/repo#42").unwrap();
        assert_eq!(
            history_42.len(),
            2,
            "source #42 should have exactly 2 events"
        );
        assert!(
            history_42
                .iter()
                .all(|h| h.source_id == "github:org/repo#42"),
            "all events must belong to source #42"
        );

        let history_99 = db.get_history("github:org/repo#99").unwrap();
        assert_eq!(
            history_99.len(),
            1,
            "source #99 should have exactly 1 event"
        );
        assert_eq!(history_99[0].status, "failed");

        let history_none = db.get_history("github:org/repo#999").unwrap();
        assert!(
            history_none.is_empty(),
            "non-existent source should return empty"
        );
    }

    // ---- Workspaces --------------------------------------------------------

    #[test]
    fn workspace_crud() {
        let db = test_db();
        db.add_workspace("ws1", "/path/to/config.yaml").unwrap();

        let list = db.list_workspaces().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "ws1");

        let (name, path, _) = db.get_workspace("ws1").unwrap();
        assert_eq!(name, "ws1");
        assert_eq!(path, "/path/to/config.yaml");

        db.remove_workspace("ws1").unwrap();
        assert!(db.get_workspace("ws1").is_err());
    }

    #[test]
    fn get_workspace_not_found() {
        let db = test_db();
        let err = db.get_workspace("nope").unwrap_err();
        assert!(matches!(err, BeltError::WorkspaceNotFound(_)));
    }

    #[test]
    fn remove_workspace_not_found() {
        let db = test_db();
        let err = db.remove_workspace("nope").unwrap_err();
        assert!(matches!(err, BeltError::WorkspaceNotFound(_)));
    }

    #[test]
    fn update_workspace_changes_config_path() {
        let db = test_db();
        db.add_workspace("ws1", "/old/path.yaml").unwrap();
        db.update_workspace("ws1", "/new/path.yaml").unwrap();

        let (_, path, _) = db.get_workspace("ws1").unwrap();
        assert_eq!(path, "/new/path.yaml");
    }

    #[test]
    fn update_workspace_not_found() {
        let db = test_db();
        let err = db.update_workspace("nope", "/any").unwrap_err();
        assert!(matches!(err, BeltError::WorkspaceNotFound(_)));
    }

    // ---- Cron Jobs ---------------------------------------------------------

    #[test]
    fn cron_job_crud() {
        let db = test_db();
        db.add_cron_job(
            "sync-issues",
            "*/5 * * * *",
            "/usr/local/bin/sync.sh",
            Some("ws1"),
        )
        .unwrap();

        let jobs = db.list_cron_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "sync-issues");
        assert_eq!(jobs[0].script, "/usr/local/bin/sync.sh");
        assert!(jobs[0].enabled);
        assert!(jobs[0].last_run_at.is_none());

        db.update_cron_last_run("sync-issues").unwrap();
        let jobs = db.list_cron_jobs().unwrap();
        assert!(jobs[0].last_run_at.is_some());

        db.toggle_cron_job("sync-issues", false).unwrap();
        let jobs = db.list_cron_jobs().unwrap();
        assert!(!jobs[0].enabled);

        db.remove_cron_job("sync-issues").unwrap();
        let jobs = db.list_cron_jobs().unwrap();
        assert!(jobs.is_empty());
    }

    #[test]
    fn cron_job_global_scope() {
        let db = test_db();
        db.add_cron_job("global-job", "0 * * * *", "/bin/run.sh", None)
            .unwrap();
        let jobs = db.list_cron_jobs().unwrap();
        assert!(jobs[0].workspace.is_none());
    }

    #[test]
    fn update_cron_schedule_changes_schedule() {
        let db = test_db();
        db.add_cron_job("my-job", "*/5 * * * *", "/bin/run.sh", None)
            .unwrap();
        db.update_cron_schedule("my-job", "0 */2 * * *").unwrap();

        let jobs = db.list_cron_jobs().unwrap();
        assert_eq!(jobs[0].schedule, "0 */2 * * *");
    }

    #[test]
    fn update_cron_schedule_not_found() {
        let db = test_db();
        let err = db.update_cron_schedule("nope", "* * * * *").unwrap_err();
        assert!(matches!(err, BeltError::ItemNotFound(_)));
    }

    #[test]
    fn update_cron_script_changes_script() {
        let db = test_db();
        db.add_cron_job("my-job", "*/5 * * * *", "/bin/old.sh", None)
            .unwrap();
        db.update_cron_script("my-job", "/bin/new.sh").unwrap();

        let job = db.get_cron_job("my-job").unwrap();
        assert_eq!(job.script, "/bin/new.sh");
    }

    #[test]
    fn update_cron_script_not_found() {
        let db = test_db();
        let err = db.update_cron_script("nope", "/bin/run.sh").unwrap_err();
        assert!(matches!(err, BeltError::ItemNotFound(_)));
    }

    #[test]
    fn get_cron_job_by_name() {
        let db = test_db();
        db.add_cron_job("my-job", "*/5 * * * *", "/bin/run.sh", Some("ws1"))
            .unwrap();

        let job = db.get_cron_job("my-job").unwrap();
        assert_eq!(job.name, "my-job");
        assert_eq!(job.schedule, "*/5 * * * *");
        assert_eq!(job.script, "/bin/run.sh");
        assert_eq!(job.workspace.as_deref(), Some("ws1"));
        assert!(job.enabled);
    }

    #[test]
    fn get_cron_job_not_found() {
        let db = test_db();
        let err = db.get_cron_job("nope").unwrap_err();
        assert!(matches!(err, BeltError::ItemNotFound(_)));
    }

    #[test]
    fn reset_cron_last_run_clears_timestamp() {
        let db = test_db();
        db.add_cron_job("reset-job", "*/5 * * * *", "/bin/run.sh", None)
            .unwrap();

        // Set last_run_at to now.
        db.update_cron_last_run("reset-job").unwrap();
        let job = db.get_cron_job("reset-job").unwrap();
        assert!(job.last_run_at.is_some());

        // Reset should clear it.
        db.reset_cron_last_run("reset-job").unwrap();
        let job = db.get_cron_job("reset-job").unwrap();
        assert!(job.last_run_at.is_none());
    }

    #[test]
    fn reset_cron_last_run_not_found() {
        let db = test_db();
        let err = db.reset_cron_last_run("nope").unwrap_err();
        assert!(matches!(err, BeltError::ItemNotFound(_)));
    }

    // ---- Knowledge Base ----------------------------------------------------

    #[test]
    fn insert_and_list_knowledge() {
        let db = test_db();
        let entry = KnowledgeEntry {
            id: None,
            workspace: "ws1".to_string(),
            source_ref: "PR #42".to_string(),
            category: "decision".to_string(),
            content: "Chose SQLite over Postgres for simplicity".to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        db.insert_knowledge(&entry).unwrap();

        let entries = db.list_knowledge(None, None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source_ref, "PR #42");
        assert_eq!(entries[0].category, "decision");
        assert!(entries[0].id.is_some());
    }

    #[test]
    fn list_knowledge_filter_by_workspace() {
        let db = test_db();
        for (ws, cat) in &[("ws1", "decision"), ("ws1", "pattern"), ("ws2", "domain")] {
            db.insert_knowledge(&KnowledgeEntry {
                id: None,
                workspace: ws.to_string(),
                source_ref: "PR #1".to_string(),
                category: cat.to_string(),
                content: "some knowledge".to_string(),
                created_at: Utc::now().to_rfc3339(),
            })
            .unwrap();
        }

        let ws1 = db.list_knowledge(Some("ws1"), None).unwrap();
        assert_eq!(ws1.len(), 2);

        let ws2 = db.list_knowledge(Some("ws2"), None).unwrap();
        assert_eq!(ws2.len(), 1);
    }

    #[test]
    fn list_knowledge_filter_by_category() {
        let db = test_db();
        for cat in &["decision", "pattern", "decision"] {
            db.insert_knowledge(&KnowledgeEntry {
                id: None,
                workspace: "ws1".to_string(),
                source_ref: "PR #1".to_string(),
                category: cat.to_string(),
                content: "content".to_string(),
                created_at: Utc::now().to_rfc3339(),
            })
            .unwrap();
        }

        let decisions = db.list_knowledge(None, Some("decision")).unwrap();
        assert_eq!(decisions.len(), 2);
    }

    #[test]
    fn get_knowledge_by_source() {
        let db = test_db();
        db.insert_knowledge(&KnowledgeEntry {
            id: None,
            workspace: "ws1".to_string(),
            source_ref: "PR #42".to_string(),
            category: "pattern".to_string(),
            content: "use builder pattern".to_string(),
            created_at: Utc::now().to_rfc3339(),
        })
        .unwrap();
        db.insert_knowledge(&KnowledgeEntry {
            id: None,
            workspace: "ws1".to_string(),
            source_ref: "PR #99".to_string(),
            category: "domain".to_string(),
            content: "other PR".to_string(),
            created_at: Utc::now().to_rfc3339(),
        })
        .unwrap();

        let entries = db.get_knowledge_by_source("PR #42").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "use builder pattern");
    }

    // ---- Token Usage -------------------------------------------------------

    #[test]
    fn record_token_usage() {
        let db = test_db();
        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: Some(200),
            cache_write_tokens: Some(100),
        };
        db.record_token_usage("w1", "ws1", "claude", "opus-4", &usage, Some(1234))
            .unwrap();

        let rows = db.get_token_usage_by_work_id("w1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, 1000);
        assert_eq!(rows[0].output_tokens, 500);
        assert_eq!(rows[0].cache_read_tokens, Some(200));
        assert_eq!(rows[0].cache_write_tokens, Some(100));
        assert_eq!(rows[0].duration_ms, Some(1234));
    }

    #[test]
    fn record_token_usage_no_duration() {
        let db = test_db();
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        };
        db.record_token_usage("w2", "ws1", "claude", "opus-4", &usage, None)
            .unwrap();

        let rows = db.get_token_usage_by_work_id("w2").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].duration_ms.is_none());
        assert!(rows[0].cache_read_tokens.is_none());
        assert!(rows[0].cache_write_tokens.is_none());
    }

    #[test]
    fn get_token_usage_by_workspace() {
        let db = test_db();
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        };
        db.record_token_usage("w1", "ws1", "claude", "opus-4", &usage, None)
            .unwrap();
        db.record_token_usage("w2", "ws1", "claude", "opus-4", &usage, Some(500))
            .unwrap();
        db.record_token_usage("w3", "ws2", "claude", "opus-4", &usage, None)
            .unwrap();

        let rows = db.get_token_usage_by_workspace("ws1").unwrap();
        assert_eq!(rows.len(), 2);

        let rows = db.get_token_usage_by_workspace("ws2").unwrap();
        assert_eq!(rows.len(), 1);
    }

    // ---- Helpers -----------------------------------------------------------

    #[test]
    fn phase_roundtrip() {
        let phases = [
            QueuePhase::Pending,
            QueuePhase::Ready,
            QueuePhase::Running,
            QueuePhase::Completed,
            QueuePhase::Done,
            QueuePhase::Hitl,
            QueuePhase::Failed,
            QueuePhase::Skipped,
        ];
        for p in phases {
            assert_eq!(str_to_phase(phase_to_str(p)).unwrap(), p);
        }
    }

    #[test]
    fn str_to_phase_unknown_returns_error() {
        let err = str_to_phase("bogus").unwrap_err();
        assert!(matches!(err, BeltError::Database(_)));
    }

    #[test]
    fn parse_datetime_invalid_returns_error() {
        let err = parse_datetime("not-a-date").unwrap_err();
        assert!(matches!(err, BeltError::Database(_)));
    }

    #[test]
    fn get_runtime_stats_empty() {
        let db = test_db();
        let stats = db.get_runtime_stats().unwrap();
        assert_eq!(stats.total_tokens, 0);
        assert_eq!(stats.executions, 0);
        assert!(stats.avg_duration_ms.is_none());
        assert!(stats.by_model.is_empty());
    }

    #[test]
    fn get_runtime_stats_aggregates_by_model() {
        let db = test_db();
        let usage_a = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            ..Default::default()
        };
        let usage_b = TokenUsage {
            input_tokens: 200,
            output_tokens: 100,
            ..Default::default()
        };
        db.record_token_usage("w1", "ws1", "claude", "opus-4", &usage_a, Some(2000))
            .unwrap();
        db.record_token_usage("w2", "ws1", "claude", "opus-4", &usage_a, Some(3000))
            .unwrap();
        db.record_token_usage("w3", "ws1", "claude", "sonnet-4", &usage_b, Some(500))
            .unwrap();

        let stats = db.get_runtime_stats().unwrap();
        assert_eq!(stats.total_tokens_input, 2200);
        assert_eq!(stats.total_tokens_output, 1100);
        assert_eq!(stats.total_tokens, 3300);
        assert_eq!(stats.executions, 3);
        assert!(stats.avg_duration_ms.is_some());

        let opus = stats.by_model.get("opus-4").unwrap();
        assert_eq!(opus.executions, 2);
        assert_eq!(opus.input_tokens, 2000);
        assert_eq!(opus.total_tokens, 3000);

        let sonnet = stats.by_model.get("sonnet-4").unwrap();
        assert_eq!(sonnet.executions, 1);
        assert_eq!(sonnet.total_tokens, 300);
    }

    #[test]
    fn database_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Database>();
    }

    // ---- Legacy spec tables ------------------------------------------------

    fn table_exists(db: &Database, name: &str) -> bool {
        let conn = db.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    }

    #[test]
    fn new_database_has_no_spec_tables() {
        let db = test_db();
        assert!(!table_exists(&db, "specs"));
        assert!(!table_exists(&db, "spec_links"));
    }

    #[test]
    fn existing_database_with_spec_tables_opens_and_keeps_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        let path = path.to_str().unwrap();
        {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch(
                "CREATE TABLE specs (id TEXT PRIMARY KEY, name TEXT NOT NULL);
                 CREATE TABLE spec_links (id TEXT PRIMARY KEY, spec_id TEXT NOT NULL);
                 INSERT INTO specs (id, name) VALUES ('s1', 'legacy');",
            )
            .unwrap();
        }

        let db = Database::open(path).unwrap();
        assert!(table_exists(&db, "specs"));
        assert!(table_exists(&db, "spec_links"));
        db.insert_item(&sample_item()).unwrap();
        let count: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM specs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    // ---- HITL metadata --------------------------------------------------------

    #[test]
    fn insert_and_get_item_with_hitl_metadata() {
        let db = test_db();
        let mut item = sample_item();
        item.set_phase_unchecked(QueuePhase::Hitl);
        item.hitl_created_at = Some(Utc::now().to_rfc3339());
        item.hitl_reason = Some(belt_core::queue::HitlReason::RetryMaxExceeded);
        item.hitl_notes = Some("max retries".to_string());
        db.insert_item(&item).unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(fetched.phase(), QueuePhase::Hitl);
        assert!(fetched.hitl_created_at.is_some());
        assert_eq!(
            fetched.hitl_reason,
            Some(belt_core::queue::HitlReason::RetryMaxExceeded)
        );
        assert_eq!(fetched.hitl_notes.as_deref(), Some("max retries"));
    }

    #[test]
    fn respond_hitl_updates_metadata() {
        let db = test_db();
        let mut item = sample_item();
        item.set_phase_unchecked(QueuePhase::Hitl);
        item.hitl_created_at = Some(Utc::now().to_rfc3339());
        db.insert_item(&item).unwrap();

        db.respond_hitl(
            &item.work_id,
            QueuePhase::Done,
            Some("irene"),
            Some("looks good"),
        )
        .unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(fetched.phase(), QueuePhase::Done);
        assert_eq!(fetched.hitl_respondent.as_deref(), Some("irene"));
        assert_eq!(fetched.hitl_notes.as_deref(), Some("looks good"));
    }

    #[test]
    fn list_expired_hitl_items_returns_old_items() {
        let db = test_db();
        // Item with hitl_created_at 25 hours ago
        let mut old_item = sample_item();
        old_item.set_phase_unchecked(QueuePhase::Hitl);
        old_item.hitl_created_at = Some((Utc::now() - chrono::Duration::hours(25)).to_rfc3339());
        db.insert_item(&old_item).unwrap();
        db.update_phase(&old_item.work_id, QueuePhase::Hitl)
            .unwrap();

        // Item with hitl_created_at 1 hour ago (not expired)
        let mut new_item = sample_item();
        new_item.work_id = "gh:org/repo#2:implement".to_string();
        new_item.set_phase_unchecked(QueuePhase::Hitl);
        new_item.hitl_created_at = Some((Utc::now() - chrono::Duration::hours(1)).to_rfc3339());
        db.insert_item(&new_item).unwrap();
        db.update_phase(&new_item.work_id, QueuePhase::Hitl)
            .unwrap();

        let expired = db.list_expired_hitl_items(24).unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0], old_item.work_id);
    }

    #[test]
    fn set_hitl_timeout_updates_item() {
        let db = test_db();
        let mut item = sample_item();
        item.set_phase_unchecked(QueuePhase::Hitl);
        db.insert_item(&item).unwrap();

        let timeout_at = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        db.set_hitl_timeout(&item.work_id, &timeout_at, Some(&EscalationAction::Skip))
            .unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(
            fetched.hitl_timeout_at.as_deref(),
            Some(timeout_at.as_str())
        );
        assert_eq!(fetched.hitl_terminal_action, Some(EscalationAction::Skip));
    }

    #[test]
    fn set_hitl_timeout_not_found() {
        let db = test_db();
        let result = db.set_hitl_timeout("nonexistent", "2026-01-01T00:00:00Z", None);
        assert!(result.is_err());
    }

    #[test]
    fn list_hitl_items_with_timeout_returns_matching() {
        let db = test_db();

        // Item with timeout set.
        let mut item1 = sample_item();
        item1.work_id = "w-timeout".to_string();
        item1.set_phase_unchecked(QueuePhase::Hitl);
        item1.hitl_timeout_at = Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339());
        item1.hitl_terminal_action = Some(EscalationAction::Skip);
        db.insert_item(&item1).unwrap();

        // Item without timeout.
        let mut item2 = sample_item();
        item2.work_id = "w-no-timeout".to_string();
        item2.set_phase_unchecked(QueuePhase::Hitl);
        db.insert_item(&item2).unwrap();

        let items = db.list_hitl_items_with_timeout().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].work_id, "w-timeout");
    }

    // ---- Transition Events tests -------------------------------------------

    #[test]
    fn insert_and_list_transition_events() {
        let db = test_db();
        let ev = TransitionEvent {
            id: "ev1".to_string(),
            work_id: "w1".to_string(),
            source_id: "github:org/repo#1".to_string(),
            event_type: "phase_enter".to_string(),
            phase: Some("running".to_string()),
            from_phase: Some("pending".to_string()),
            detail: None,
            created_at: Utc::now().to_rfc3339(),
        };
        db.insert_transition_event(&ev).unwrap();

        let events = db.list_transition_events("w1").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "ev1");
        assert_eq!(events[0].work_id, "w1");
        assert_eq!(events[0].source_id, "github:org/repo#1");
        assert_eq!(events[0].from_phase.as_deref(), Some("pending"));
        assert_eq!(events[0].phase.as_deref(), Some("running"));
    }

    #[test]
    fn list_recent_transition_events_respects_limit() {
        let db = test_db();
        for i in 0..5 {
            let ev = TransitionEvent {
                id: format!("ev{i}"),
                work_id: format!("w{i}"),
                source_id: format!("github:org/repo#{i}"),
                event_type: "phase_enter".to_string(),
                phase: Some("running".to_string()),
                from_phase: Some("pending".to_string()),
                detail: None,
                created_at: Utc::now().to_rfc3339(),
            };
            db.insert_transition_event(&ev).unwrap();
        }
        let recent = db.list_recent_transition_events(3).unwrap();
        assert_eq!(recent.len(), 3);
    }

    #[test]
    fn count_items_by_phase_empty() {
        let db = test_db();
        let counts = db.count_items_by_phase().unwrap();
        assert!(counts.is_empty());
    }

    #[test]
    fn count_items_by_phase_with_items() {
        let db = test_db();
        let mut item1 = sample_item();
        item1.work_id = "w1".to_string();
        item1.set_phase_unchecked(QueuePhase::Pending);
        db.insert_item(&item1).unwrap();

        let mut item2 = sample_item();
        item2.work_id = "w2".to_string();
        item2.set_phase_unchecked(QueuePhase::Running);
        db.insert_item(&item2).unwrap();

        let mut item3 = sample_item();
        item3.work_id = "w3".to_string();
        item3.set_phase_unchecked(QueuePhase::Pending);
        db.insert_item(&item3).unwrap();

        let counts = db.count_items_by_phase().unwrap();
        let pending = counts.iter().find(|(p, _)| p == "pending").map(|(_, c)| *c);
        let running = counts.iter().find(|(p, _)| p == "running").map(|(_, c)| *c);
        assert_eq!(pending, Some(2));
        assert_eq!(running, Some(1));
    }

    // ---- Queue Dependencies tests ------------------------------------------

    #[test]
    fn add_and_list_queue_dependency() {
        let db = test_db();
        db.add_queue_dependency("item-a", "item-b").unwrap();
        db.add_queue_dependency("item-a", "item-c").unwrap();

        let deps = db.list_queue_dependencies("item-a").unwrap();
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&"item-b".to_string()));
        assert!(deps.contains(&"item-c".to_string()));
    }

    #[test]
    fn add_duplicate_dependency_is_ignored() {
        let db = test_db();
        db.add_queue_dependency("item-a", "item-b").unwrap();
        db.add_queue_dependency("item-a", "item-b").unwrap();

        let deps = db.list_queue_dependencies("item-a").unwrap();
        assert_eq!(deps.len(), 1);
    }

    #[test]
    fn remove_queue_dependency() {
        let db = test_db();
        db.add_queue_dependency("item-a", "item-b").unwrap();
        db.add_queue_dependency("item-a", "item-c").unwrap();

        db.remove_queue_dependency("item-a", "item-b").unwrap();

        let deps = db.list_queue_dependencies("item-a").unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0], "item-c");
    }

    #[test]
    fn remove_nonexistent_dependency_returns_error() {
        let db = test_db();
        let result = db.remove_queue_dependency("item-a", "item-b");
        assert!(result.is_err());
    }

    #[test]
    fn list_queue_dependencies_empty() {
        let db = test_db();
        let deps = db.list_queue_dependencies("item-a").unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn circular_dependency_self_loop() {
        let db = test_db();
        let result = db.add_queue_dependency("item-a", "item-a");
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), BeltError::CircularDependency(_)),
            "self-loop should be detected as circular dependency"
        );
    }

    #[test]
    fn circular_dependency_direct_cycle() {
        let db = test_db();
        // A depends on B
        db.add_queue_dependency("item-a", "item-b").unwrap();
        // B depends on A => cycle: B -> A -> B
        let result = db.add_queue_dependency("item-b", "item-a");
        assert!(
            matches!(result, Err(BeltError::CircularDependency(_))),
            "direct cycle should be detected"
        );
    }

    #[test]
    fn circular_dependency_indirect_cycle() {
        let db = test_db();
        // A -> B -> C (A depends on B, B depends on C)
        db.add_queue_dependency("item-a", "item-b").unwrap();
        db.add_queue_dependency("item-b", "item-c").unwrap();
        // C -> A => cycle: C -> A -> B -> C
        let result = db.add_queue_dependency("item-c", "item-a");
        assert!(
            matches!(result, Err(BeltError::CircularDependency(_))),
            "indirect cycle (3 nodes) should be detected"
        );
    }

    #[test]
    fn circular_dependency_long_chain() {
        let db = test_db();
        // A -> B -> C -> D
        db.add_queue_dependency("item-a", "item-b").unwrap();
        db.add_queue_dependency("item-b", "item-c").unwrap();
        db.add_queue_dependency("item-c", "item-d").unwrap();
        // D -> A => cycle through 4 nodes
        let result = db.add_queue_dependency("item-d", "item-a");
        assert!(
            matches!(result, Err(BeltError::CircularDependency(_))),
            "long indirect cycle (4 nodes) should be detected"
        );
        // Non-cyclic addition should still work
        db.add_queue_dependency("item-d", "item-e").unwrap();
    }

    // ---- Script Execution Stats ---------------------------------------------

    fn insert_history(db: &Database, work_id: &str, state: &str, status: &str, time: &str) {
        db.append_history(&HistoryEvent {
            work_id: work_id.to_string(),
            source_id: format!("src-{work_id}"),
            state: state.to_string(),
            status: status.to_string(),
            attempt: 1,
            summary: None,
            error: None,
            created_at: time.to_string(),
        })
        .unwrap();
    }

    #[test]
    fn get_script_execution_stats_empty() {
        let db = test_db();
        let stats = db.get_script_execution_stats().unwrap();
        assert!(stats.is_empty());
    }

    #[test]
    fn get_script_execution_stats_aggregates_by_state() {
        let db = test_db();
        insert_history(&db, "w1", "analyze", "success", "2026-03-25T01:00:00Z");
        insert_history(&db, "w2", "analyze", "success", "2026-03-25T02:00:00Z");
        insert_history(&db, "w3", "analyze", "failed", "2026-03-25T03:00:00Z");
        insert_history(&db, "w4", "implement", "success", "2026-03-25T04:00:00Z");

        let stats = db.get_script_execution_stats().unwrap();
        assert_eq!(stats.len(), 2);

        // Ordered by total_runs descending: analyze(3) > implement(1).
        assert_eq!(stats[0].state, "analyze");
        assert_eq!(stats[0].total_runs, 3);
        assert_eq!(stats[0].success_count, 2);
        assert_eq!(stats[0].fail_count, 1);
        assert!((stats[0].success_rate - 66.666).abs() < 1.0);

        assert_eq!(stats[1].state, "implement");
        assert_eq!(stats[1].total_runs, 1);
        assert_eq!(stats[1].success_count, 1);
        assert_eq!(stats[1].fail_count, 0);
        assert!((stats[1].success_rate - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn get_recent_script_executions_returns_most_recent() {
        let db = test_db();
        insert_history(&db, "w1", "analyze", "success", "2026-03-25T01:00:00Z");
        insert_history(&db, "w2", "implement", "failed", "2026-03-25T02:00:00Z");
        insert_history(&db, "w3", "review", "success", "2026-03-25T03:00:00Z");

        let recent = db.get_recent_script_executions(2).unwrap();
        assert_eq!(recent.len(), 2);
        // Most recent first.
        assert_eq!(recent[0].work_id, "w3");
        assert_eq!(recent[1].work_id, "w2");
    }

    #[test]
    fn get_recent_script_executions_empty() {
        let db = test_db();
        let recent = db.get_recent_script_executions(10).unwrap();
        assert!(recent.is_empty());
    }

    // ---- worktree_preserved ------------------------------------------------

    #[test]
    fn insert_item_with_worktree_preserved_true() {
        let db = test_db();
        let mut item = sample_item();
        item.worktree_preserved = true;
        db.insert_item(&item).unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert!(fetched.worktree_preserved);
    }

    #[test]
    fn insert_item_worktree_preserved_defaults_to_false() {
        let db = test_db();
        let item = sample_item();
        assert!(!item.worktree_preserved);
        db.insert_item(&item).unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert!(!fetched.worktree_preserved);
    }

    #[test]
    fn worktree_preserved_survives_phase_update() {
        let db = test_db();
        let mut item = sample_item();
        item.worktree_preserved = true;
        db.insert_item(&item).unwrap();

        db.update_phase(&item.work_id, QueuePhase::Running).unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert!(fetched.worktree_preserved);
        assert_eq!(fetched.phase(), QueuePhase::Running);
    }

    #[test]
    fn worktree_preserved_visible_in_list_items() {
        let db = test_db();

        let mut item1 = sample_item();
        item1.work_id = "w-preserved".to_string();
        item1.worktree_preserved = true;
        db.insert_item(&item1).unwrap();

        let mut item2 = sample_item();
        item2.work_id = "w-not-preserved".to_string();
        item2.worktree_preserved = false;
        db.insert_item(&item2).unwrap();

        let items = db.list_items(None, None).unwrap();
        assert_eq!(items.len(), 2);

        let preserved: Vec<_> = items.iter().filter(|i| i.worktree_preserved).collect();
        assert_eq!(preserved.len(), 1);
        assert_eq!(preserved[0].work_id, "w-preserved");
    }

    // ---- previous_worktree_path persistence ---------------------------------

    #[test]
    fn insert_item_with_previous_worktree_path() {
        let db = test_db();
        let mut item = sample_item();
        item.previous_worktree_path = Some("/tmp/worktrees/old".to_string());
        db.insert_item(&item).unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(
            fetched.previous_worktree_path.as_deref(),
            Some("/tmp/worktrees/old")
        );
    }

    #[test]
    fn insert_item_previous_worktree_path_defaults_to_none() {
        let db = test_db();
        let item = sample_item();
        db.insert_item(&item).unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert!(fetched.previous_worktree_path.is_none());
    }

    #[test]
    fn previous_worktree_path_survives_phase_update() {
        let db = test_db();
        let mut item = sample_item();
        item.previous_worktree_path = Some("/tmp/wt/old".to_string());
        db.insert_item(&item).unwrap();

        db.update_phase(&item.work_id, QueuePhase::Running).unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(
            fetched.previous_worktree_path.as_deref(),
            Some("/tmp/wt/old")
        );
    }

    #[test]
    fn update_item_worktree_state_persists_path() {
        let db = test_db();
        let item = sample_item();
        db.insert_item(&item).unwrap();

        db.update_item_worktree_state(
            &item.work_id,
            QueuePhase::Pending,
            true,
            Some("/tmp/wt/preserved"),
        )
        .unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(fetched.phase(), QueuePhase::Pending);
        assert!(fetched.worktree_preserved);
        assert_eq!(
            fetched.previous_worktree_path.as_deref(),
            Some("/tmp/wt/preserved")
        );
    }

    #[test]
    fn update_item_worktree_state_clears_path_when_none() {
        let db = test_db();
        let mut item = sample_item();
        item.previous_worktree_path = Some("/tmp/wt/old".to_string());
        db.insert_item(&item).unwrap();

        db.update_item_worktree_state(&item.work_id, QueuePhase::Running, false, None)
            .unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert!(fetched.previous_worktree_path.is_none());
        assert!(!fetched.worktree_preserved);
    }

    #[test]
    fn update_item_worktree_state_not_found() {
        let db = test_db();
        let err = db
            .update_item_worktree_state("nonexistent", QueuePhase::Pending, true, None)
            .unwrap_err();
        assert!(matches!(err, BeltError::ItemNotFound(_)));
    }

    // ---- reset_cron_last_run (additional) ----------------------------------

    #[test]
    fn reset_cron_last_run_idempotent_on_null() {
        let db = test_db();
        db.add_cron_job("idem-job", "*/5 * * * *", "/bin/run.sh", None)
            .unwrap();

        // last_run_at is already NULL after creation.
        let job = db.get_cron_job("idem-job").unwrap();
        assert!(job.last_run_at.is_none());

        // Resetting again should succeed without error.
        db.reset_cron_last_run("idem-job").unwrap();
        let job = db.get_cron_job("idem-job").unwrap();
        assert!(job.last_run_at.is_none());
    }

    #[test]
    fn reset_cron_last_run_does_not_affect_other_fields() {
        let db = test_db();
        db.add_cron_job("field-job", "*/10 * * * *", "/bin/check.sh", Some("ws1"))
            .unwrap();
        db.update_cron_last_run("field-job").unwrap();

        db.reset_cron_last_run("field-job").unwrap();

        let job = db.get_cron_job("field-job").unwrap();
        assert!(job.last_run_at.is_none());
        // Other fields remain intact.
        assert_eq!(job.schedule, "*/10 * * * *");
        assert_eq!(job.script, "/bin/check.sh");
        assert_eq!(job.workspace.as_deref(), Some("ws1"));
        assert!(job.enabled);
    }

    // ---- QUEUE_ITEM_COLUMNS consistency ------------------------------------

    #[test]
    fn queue_item_columns_count_matches_schema() {
        let col_count = QUEUE_ITEM_COLUMNS.split(',').count();
        // QueueItem maps 19 queue_items columns:
        // work_id, source_id, workspace_id, state, phase, title,
        // created_at, updated_at, hitl_created_at, hitl_respondent,
        // hitl_notes, hitl_reason, hitl_timeout_at, hitl_terminal_action,
        // replan_count, worktree_preserved, previous_worktree_path,
        // derived_from, lineage_root.
        // handler_pid and worktree_owner are not part of QueueItem.
        assert_eq!(col_count, 19);
    }

    #[test]
    fn queue_item_columns_contains_worktree_preserved() {
        assert!(
            QUEUE_ITEM_COLUMNS.contains("worktree_preserved"),
            "QUEUE_ITEM_COLUMNS must include the worktree_preserved column"
        );
    }

    #[test]
    fn queue_item_columns_all_expected_columns_present() {
        let expected = [
            "work_id",
            "source_id",
            "workspace_id",
            "state",
            "phase",
            "title",
            "created_at",
            "updated_at",
            "hitl_created_at",
            "hitl_respondent",
            "hitl_notes",
            "hitl_reason",
            "hitl_timeout_at",
            "hitl_terminal_action",
            "replan_count",
            "worktree_preserved",
            "previous_worktree_path",
            "derived_from",
            "lineage_root",
        ];
        let columns: Vec<&str> = QUEUE_ITEM_COLUMNS.split(',').map(|s| s.trim()).collect();
        for col_name in &expected {
            assert!(
                columns.contains(col_name),
                "QUEUE_ITEM_COLUMNS is missing column: {col_name}"
            );
        }
        assert_eq!(columns.len(), expected.len());
    }

    // ---- respond_hitl additional tests -------------------------------------

    #[test]
    fn respond_hitl_not_found() {
        let db = test_db();
        let err = db
            .respond_hitl("nonexistent", QueuePhase::Done, Some("irene"), None)
            .unwrap_err();
        assert!(matches!(err, BeltError::ItemNotFound(_)));
    }

    #[test]
    fn respond_hitl_with_none_respondent_and_notes() {
        let db = test_db();
        let mut item = sample_item();
        item.set_phase_unchecked(QueuePhase::Hitl);
        item.hitl_created_at = Some(Utc::now().to_rfc3339());
        item.hitl_notes = Some("original notes".to_string());
        db.insert_item(&item).unwrap();

        db.respond_hitl(&item.work_id, QueuePhase::Pending, None, None)
            .unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(fetched.phase(), QueuePhase::Pending);
        assert!(fetched.hitl_respondent.is_none());
        // When notes is NULL, COALESCE preserves original notes
        assert_eq!(fetched.hitl_notes.as_deref(), Some("original notes"));
    }

    #[test]
    fn respond_hitl_overwrites_notes_when_provided() {
        let db = test_db();
        let mut item = sample_item();
        item.set_phase_unchecked(QueuePhase::Hitl);
        item.hitl_created_at = Some(Utc::now().to_rfc3339());
        item.hitl_notes = Some("old notes".to_string());
        db.insert_item(&item).unwrap();

        db.respond_hitl(
            &item.work_id,
            QueuePhase::Done,
            Some("bob"),
            Some("new notes"),
        )
        .unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(fetched.hitl_notes.as_deref(), Some("new notes"));
    }

    // ---- toggle_cron_job tests ---------------------------------------------

    #[test]
    fn toggle_cron_job_disable_and_enable() {
        let db = test_db();
        db.add_cron_job("my-job", "*/5 * * * *", "run.sh", None)
            .unwrap();

        // Jobs start enabled (enabled=1 in INSERT)
        let job = db.get_cron_job("my-job").unwrap();
        assert!(job.enabled);

        // Disable
        db.toggle_cron_job("my-job", false).unwrap();
        let job = db.get_cron_job("my-job").unwrap();
        assert!(!job.enabled);

        // Re-enable
        db.toggle_cron_job("my-job", true).unwrap();
        let job = db.get_cron_job("my-job").unwrap();
        assert!(job.enabled);
    }

    #[test]
    fn toggle_cron_job_not_found() {
        let db = test_db();
        let err = db.toggle_cron_job("nonexistent", true).unwrap_err();
        assert!(matches!(err, BeltError::ItemNotFound(_)));
    }

    // ---- has_open_items_for_source tests ------------------------------------

    #[test]
    fn has_open_items_for_source_returns_false_when_empty() {
        let db = test_db();
        let result = db.has_open_items_for_source("gh:org/repo#99").unwrap();
        assert!(!result);
    }

    #[test]
    fn has_open_items_for_source_returns_true_for_pending() {
        let db = test_db();
        let item = sample_item(); // phase = Pending
        db.insert_item(&item).unwrap();

        let result = db.has_open_items_for_source(&item.source_id).unwrap();
        assert!(result);
    }

    #[test]
    fn has_open_items_for_source_returns_false_for_done() {
        let db = test_db();
        let mut item = sample_item();
        item.set_phase_unchecked(QueuePhase::Done);
        db.insert_item(&item).unwrap();

        let result = db.has_open_items_for_source(&item.source_id).unwrap();
        assert!(!result);
    }

    #[test]
    fn has_open_items_for_source_returns_false_for_skipped() {
        let db = test_db();
        let mut item = sample_item();
        item.set_phase_unchecked(QueuePhase::Skipped);
        db.insert_item(&item).unwrap();

        let result = db.has_open_items_for_source(&item.source_id).unwrap();
        assert!(!result);
    }

    #[test]
    fn has_open_items_for_source_different_source_ids() {
        let db = test_db();
        let item = sample_item(); // source_id = "gh:org/repo#1"
        db.insert_item(&item).unwrap();

        // Same source_id should find it
        assert!(db.has_open_items_for_source("gh:org/repo#1").unwrap());
        // Different source_id should not
        assert!(!db.has_open_items_for_source("gh:org/repo#999").unwrap());
    }

    // ---- update_cron_last_run tests ----------------------------------------

    #[test]
    fn update_cron_last_run_sets_timestamp() {
        let db = test_db();
        db.add_cron_job("runner", "0 * * * *", "script.sh", None)
            .unwrap();

        // Initially last_run_at is None
        let job = db.get_cron_job("runner").unwrap();
        assert!(job.last_run_at.is_none());

        // After update it should have a timestamp
        db.update_cron_last_run("runner").unwrap();
        let job = db.get_cron_job("runner").unwrap();
        assert!(job.last_run_at.is_some());
    }

    #[test]
    fn update_cron_last_run_not_found() {
        let db = test_db();
        let err = db.update_cron_last_run("nonexistent").unwrap_err();
        assert!(matches!(err, BeltError::ItemNotFound(_)));
    }

    // ---- set_hitl_timeout additional tests ----------------------------------

    #[test]
    fn set_hitl_timeout_with_no_terminal_action() {
        let db = test_db();
        let mut item = sample_item();
        item.set_phase_unchecked(QueuePhase::Hitl);
        db.insert_item(&item).unwrap();

        let timeout_at = (Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        db.set_hitl_timeout(&item.work_id, &timeout_at, None)
            .unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(
            fetched.hitl_timeout_at.as_deref(),
            Some(timeout_at.as_str())
        );
        assert!(fetched.hitl_terminal_action.is_none());
    }

    // ---- list_cron_jobs tests -----------------------------------------------

    #[test]
    fn list_cron_jobs_empty() {
        let db = test_db();
        let jobs = db.list_cron_jobs().unwrap();
        assert!(jobs.is_empty());
    }

    #[test]
    fn list_cron_jobs_returns_all_sorted_by_name() {
        let db = test_db();
        db.add_cron_job("zebra-job", "0 * * * *", "z.sh", None)
            .unwrap();
        db.add_cron_job("alpha-job", "*/10 * * * *", "a.sh", Some("ws1"))
            .unwrap();
        db.add_cron_job("mid-job", "*/5 * * * *", "m.sh", None)
            .unwrap();

        let jobs = db.list_cron_jobs().unwrap();
        assert_eq!(jobs.len(), 3);
        // Sorted by name ascending
        assert_eq!(jobs[0].name, "alpha-job");
        assert_eq!(jobs[1].name, "mid-job");
        assert_eq!(jobs[2].name, "zebra-job");
    }

    #[test]
    fn list_cron_jobs_reflects_fields() {
        let db = test_db();
        db.add_cron_job("test-job", "*/5 * * * *", "run.sh", Some("ws1"))
            .unwrap();

        let jobs = db.list_cron_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert_eq!(job.name, "test-job");
        assert_eq!(job.schedule, "*/5 * * * *");
        assert_eq!(job.script, "run.sh");
        assert_eq!(job.workspace.as_deref(), Some("ws1"));
        assert!(job.enabled);
        assert!(job.last_run_at.is_none());
    }

    #[test]
    fn list_cron_jobs_reflects_toggle_state() {
        let db = test_db();
        db.add_cron_job("toggled", "0 * * * *", "t.sh", None)
            .unwrap();
        db.toggle_cron_job("toggled", false).unwrap();

        let jobs = db.list_cron_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(!jobs[0].enabled);
    }

    #[test]
    fn get_token_usage_since_empty() {
        let db = test_db();
        let since = Utc::now() - chrono::Duration::hours(24);
        let rows = db.get_token_usage_since(&since).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn get_token_usage_since_groups_by_model() {
        let db = test_db();
        let usage = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            ..Default::default()
        };
        db.record_token_usage("w1", "ws1", "claude", "opus", &usage, None)
            .unwrap();
        db.record_token_usage("w2", "ws1", "claude", "sonnet", &usage, None)
            .unwrap();
        db.record_token_usage("w3", "ws1", "claude", "opus", &usage, Some(200))
            .unwrap();

        let since = Utc::now() - chrono::Duration::hours(1);
        let rows = db.get_token_usage_since(&since).unwrap();

        assert_eq!(rows.len(), 2);
        // Sorted by total tokens desc: opus (3000) > sonnet (1500)
        assert_eq!(rows[0].0, "opus");
        assert_eq!(rows[0].1, 2_000); // input
        assert_eq!(rows[0].2, 1_000); // output
        assert_eq!(rows[0].3, 2); // executions
        assert_eq!(rows[1].0, "sonnet");
        assert_eq!(rows[1].3, 1);
    }

    // ---- Transition / lineage store API ------------------------------------

    use belt_core::transition::{Actor, Processing, TransitionOutcome, TransitionReason};

    fn new_item(source_id: &str, state: &str) -> NewItem {
        NewItem {
            source_id: source_id.to_string(),
            workspace_id: "ws".to_string(),
            state: state.to_string(),
            title: None,
            actor: Actor::Daemon,
        }
    }

    fn collect(db: &Database, source_id: &str, state: &str) -> CollectOutcome {
        db.insert_collected(&new_item(source_id, state)).unwrap()
    }

    fn inserted_id(outcome: CollectOutcome) -> String {
        match outcome {
            CollectOutcome::Inserted { work_id } => work_id,
            CollectOutcome::Duplicate => panic!("expected Inserted"),
        }
    }

    fn request(work_id: &str, from: QueuePhase, to: QueuePhase, actor: Actor) -> TransitionRequest {
        TransitionRequest {
            work_id: work_id.to_string(),
            expected_from: from,
            to,
            actor,
            reason: TransitionReason::Manual,
            detail: None,
        }
    }

    fn step(db: &Database, work_id: &str, from: QueuePhase, to: QueuePhase) {
        let outcome = db
            .transition(&request(work_id, from, to, Actor::Daemon))
            .unwrap();
        assert!(
            matches!(outcome, TransitionOutcome::Applied { .. }),
            "{from:?}->{to:?} gave {outcome:?}"
        );
    }

    fn run_to_running(db: &Database, work_id: &str) {
        step(db, work_id, QueuePhase::Pending, QueuePhase::Ready);
        step(db, work_id, QueuePhase::Ready, QueuePhase::Running);
    }

    fn history(db: &Database, work_id: &str, source_id: &str, state: &str, status: &str) {
        db.append_history(&HistoryEvent {
            work_id: work_id.to_string(),
            source_id: source_id.to_string(),
            state: state.to_string(),
            status: status.to_string(),
            attempt: 1,
            summary: None,
            error: None,
            created_at: Utc::now().to_rfc3339(),
        })
        .unwrap();
    }

    #[test]
    fn transition_applies_and_logs_phase_enter_in_same_commit() {
        let db = test_db();
        let id = inserted_id(collect(&db, "s1", "analyze"));

        let outcome = db
            .transition(&request(
                &id,
                QueuePhase::Pending,
                QueuePhase::Ready,
                Actor::Cron,
            ))
            .unwrap();

        let TransitionOutcome::Applied { seq } = outcome else {
            panic!("expected Applied, got {outcome:?}");
        };
        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Ready);
        let log = db.transitions_of(&id).unwrap();
        let last = log.last().unwrap();
        assert_eq!(last.seq, seq);
        assert_eq!(last.kind, transition_kind::PHASE_ENTER);
        assert_eq!(last.from_phase.as_deref(), Some("pending"));
        assert_eq!(last.to_phase.as_deref(), Some("ready"));
        assert_eq!(last.actor, "cron");
        assert_eq!(last.reason.as_deref(), Some("manual"));
    }

    #[test]
    fn transition_conflict_returns_current_phase_and_logs_conflict() {
        let db = test_db();
        let id = inserted_id(collect(&db, "s1", "analyze"));
        step(&db, &id, QueuePhase::Pending, QueuePhase::Ready);

        let outcome = db
            .transition(&request(
                &id,
                QueuePhase::Pending,
                QueuePhase::Ready,
                Actor::Cli,
            ))
            .unwrap();

        assert_eq!(
            outcome,
            TransitionOutcome::Conflict {
                current: QueuePhase::Ready
            }
        );
        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Ready);
        let last = db.transitions_of(&id).unwrap().pop().unwrap();
        assert_eq!(last.kind, transition_kind::TRANSITION_CONFLICT);
        assert_eq!(last.actor, "cli");
    }

    #[test]
    fn transition_on_running_item_is_busy_for_non_owner_and_logged() {
        let db = test_db();
        let id = inserted_id(collect(&db, "s1", "analyze"));
        run_to_running(&db, &id);

        let outcome = db
            .transition(&request(
                &id,
                QueuePhase::Running,
                QueuePhase::Skipped,
                Actor::Cli,
            ))
            .unwrap();

        assert_eq!(
            outcome,
            TransitionOutcome::Busy {
                processing: Processing::Handler
            }
        );
        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Running);
        let last = db.transitions_of(&id).unwrap().pop().unwrap();
        assert_eq!(last.kind, transition_kind::TRANSITION_REJECTED);
        // The owner is never blocked by its own lock.
        step(&db, &id, QueuePhase::Running, QueuePhase::Completed);
    }

    #[test]
    fn hitl_item_in_post_processing_is_busy_for_non_owner() {
        let db = test_db();
        let id = inserted_id(collect(&db, "s1", "analyze"));
        run_to_running(&db, &id);
        step(&db, &id, QueuePhase::Running, QueuePhase::Hitl);
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO hitl_requests (hitl_id, work_id, status, opened_at, resolved_at, action)
                 VALUES ('h1', ?1, 'resolved', 't', 't', 'retry')",
                params![id],
            )
            .unwrap();
        }

        let outcome = db
            .transition(&request(
                &id,
                QueuePhase::Hitl,
                QueuePhase::Skipped,
                Actor::Cli,
            ))
            .unwrap();

        assert_eq!(
            outcome,
            TransitionOutcome::Busy {
                processing: Processing::PostProcessing
            }
        );
    }

    #[test]
    fn hitl_exit_without_matching_action_is_invalid_and_not_applied() {
        let db = test_db();
        let id = inserted_id(collect(&db, "s1", "analyze"));
        run_to_running(&db, &id);
        step(&db, &id, QueuePhase::Running, QueuePhase::Hitl);

        let outcome = db
            .transition(&request(
                &id,
                QueuePhase::Hitl,
                QueuePhase::Pending,
                Actor::Cli,
            ))
            .unwrap();

        assert_eq!(
            outcome,
            TransitionOutcome::InvalidAction {
                current: QueuePhase::Hitl
            }
        );
        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Hitl);
    }

    #[test]
    fn transition_outside_state_machine_is_invalid_action_without_log() {
        let db = test_db();
        let id = inserted_id(collect(&db, "s1", "analyze"));
        let before = db.transitions_of(&id).unwrap().len();

        let outcome = db
            .transition(&request(
                &id,
                QueuePhase::Pending,
                QueuePhase::Done,
                Actor::Cli,
            ))
            .unwrap();

        assert_eq!(
            outcome,
            TransitionOutcome::InvalidAction {
                current: QueuePhase::Pending
            }
        );
        assert_eq!(db.transitions_of(&id).unwrap().len(), before);
    }

    #[test]
    fn transition_of_unknown_item_is_item_not_found() {
        let db = test_db();
        let err = db
            .transition(&request(
                "nope",
                QueuePhase::Pending,
                QueuePhase::Ready,
                Actor::Daemon,
            ))
            .unwrap_err();
        assert!(matches!(err, BeltError::ItemNotFound(_)));
    }

    #[test]
    fn concurrent_transitions_on_one_item_apply_once_and_conflict_once() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("belt.db");
        let path = path.to_str().unwrap().to_string();
        let setup = Database::open(&path).unwrap();

        for round in 0..10 {
            let id = inserted_id(collect(&setup, &format!("s{round}"), "analyze"));
            let barrier = Arc::new(Barrier::new(2));
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let path = path.clone();
                    let id = id.clone();
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        let db = Database::open(&path).unwrap();
                        barrier.wait();
                        db.transition(&request(
                            &id,
                            QueuePhase::Pending,
                            QueuePhase::Ready,
                            Actor::Cli,
                        ))
                    })
                })
                .collect();
            let outcomes: Vec<_> = handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap()
                        .expect("no database error under contention")
                })
                .collect();

            let applied = outcomes
                .iter()
                .filter(|o| matches!(o, TransitionOutcome::Applied { .. }))
                .count();
            let conflicts = outcomes
                .iter()
                .filter(|o| {
                    matches!(
                        o,
                        TransitionOutcome::Conflict {
                            current: QueuePhase::Ready
                        }
                    )
                })
                .count();
            assert_eq!((applied, conflicts), (1, 1), "round {round}: {outcomes:?}");
        }

        let seqs: Vec<u64> = setup
            .transitions_since(0)
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        let unique: std::collections::BTreeSet<_> = seqs.iter().collect();
        assert_eq!(unique.len(), seqs.len(), "duplicate transition_log seq");
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn transition_log_seq_is_strictly_increasing_across_items() {
        let db = test_db();
        let a = inserted_id(collect(&db, "s1", "analyze"));
        let b = inserted_id(collect(&db, "s2", "analyze"));
        step(&db, &a, QueuePhase::Pending, QueuePhase::Ready);
        step(&db, &b, QueuePhase::Pending, QueuePhase::Ready);
        step(&db, &a, QueuePhase::Ready, QueuePhase::Running);

        let all = db.transitions_since(0).unwrap();
        assert!(all.windows(2).all(|w| w[0].seq < w[1].seq));
        let cursor = all[1].seq;
        let rest = db.transitions_since(cursor).unwrap();
        assert_eq!(rest.len(), all.len() - 2);
        assert!(rest.iter().all(|e| e.seq > cursor));
    }

    #[test]
    fn insert_collected_first_item_uses_base_work_id_and_logs_creation() {
        let db = test_db();
        let id = inserted_id(collect(&db, "gh:o/r#1", "implement"));

        assert_eq!(id, "gh:o/r#1:implement");
        let item = db.get_item(&id).unwrap();
        assert_eq!(item.lineage_root, id);
        assert_eq!(item.derived_from, None);
        let log = db.transitions_of(&id).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].kind, transition_kind::ITEM_CREATED);
        assert_eq!(log[0].from_phase, None);
        assert_eq!(log[0].to_phase.as_deref(), Some("pending"));
    }

    #[test]
    fn insert_collected_is_duplicate_while_an_item_is_open() {
        let db = test_db();
        let id = inserted_id(collect(&db, "s1", "implement"));
        assert_eq!(collect(&db, "s1", "implement"), CollectOutcome::Duplicate);

        // Failed is not terminal for collection (C-26).
        run_to_running(&db, &id);
        step(&db, &id, QueuePhase::Running, QueuePhase::Failed);
        assert_eq!(collect(&db, "s1", "implement"), CollectOutcome::Duplicate);
        assert_eq!(db.list_items(None, None).unwrap().len(), 1);
    }

    #[test]
    fn insert_collected_after_terminal_items_takes_next_sequence() {
        let db = test_db();
        let first = inserted_id(collect(&db, "s1", "implement"));
        step(&db, &first, QueuePhase::Pending, QueuePhase::Skipped);

        let second = inserted_id(collect(&db, "s1", "implement"));
        assert_eq!(second, "s1:implement:2");
        step(&db, &second, QueuePhase::Pending, QueuePhase::Skipped);

        let third = inserted_id(collect(&db, "s1", "implement"));
        assert_eq!(third, "s1:implement:3");
        // A recollected item starts a new lineage with no origin.
        let item = db.get_item(&third).unwrap();
        assert_eq!(item.lineage_root, third);
        assert_eq!(item.derived_from, None);
        // Another state of the same source is independent.
        assert_eq!(inserted_id(collect(&db, "s1", "analyze")), "s1:analyze");
    }

    #[test]
    fn insert_collected_never_reuses_a_work_id_of_a_removed_item() {
        let db = test_db();
        let first = inserted_id(collect(&db, "s1", "implement"));
        step(&db, &first, QueuePhase::Pending, QueuePhase::Skipped);
        let second = inserted_id(collect(&db, "s1", "implement"));
        step(&db, &second, QueuePhase::Pending, QueuePhase::Skipped);
        {
            let conn = db.conn.lock().unwrap();
            conn.execute("DELETE FROM queue_items", []).unwrap();
        }

        assert_eq!(
            inserted_id(collect(&db, "s1", "implement")),
            "s1:implement:3"
        );
    }

    #[test]
    fn insert_item_rejects_empty_lineage_root() {
        let db = test_db();
        let mut item = sample_item();
        item.lineage_root = String::new();
        let err = db.insert_item(&item).unwrap_err();
        assert!(matches!(err, BeltError::Database(_)));
        assert!(matches!(
            db.get_item(&item.work_id),
            Err(BeltError::ItemNotFound(_))
        ));
    }

    fn derive_request(work_id: &str, from: QueuePhase, kind: DeriveKind) -> DeriveRequest {
        DeriveRequest {
            work_id: work_id.to_string(),
            expected_from: from,
            kind,
            actor: Actor::Daemon,
            reason: TransitionReason::Derived,
            detail: None,
        }
    }

    #[test]
    fn derive_retry_skips_origin_and_creates_pending_item_with_origin_recorded() {
        let db = test_db();
        let first = inserted_id(collect(&db, "s1", "implement"));
        run_to_running(&db, &first);

        let outcome = db
            .derive(&derive_request(
                &first,
                QueuePhase::Running,
                DeriveKind::EscalationRetry,
            ))
            .unwrap();

        let DeriveOutcome::Derived { work_id } = outcome else {
            panic!("expected Derived, got {outcome:?}");
        };
        assert_eq!(work_id, "s1:implement:2");
        assert_eq!(db.get_item(&first).unwrap().phase(), QueuePhase::Skipped);
        let derived = db.get_item(&work_id).unwrap();
        assert_eq!(derived.phase(), QueuePhase::Pending);
        assert_eq!(derived.derived_from.as_deref(), Some(first.as_str()));
        assert_eq!(derived.lineage_root, first);
        let created = db.transitions_of(&work_id).unwrap();
        assert_eq!(created[0].kind, transition_kind::ITEM_CREATED);
        assert_eq!(created[0].reason.as_deref(), Some("derived"));
        assert_eq!(created[0].detail.as_deref(), Some(first.as_str()));
        let origin_log = db.transitions_of(&first).unwrap();
        let ended = origin_log.last().unwrap();
        assert_eq!(ended.to_phase.as_deref(), Some("skipped"));
        assert_eq!(ended.reason.as_deref(), Some("derived"));
    }

    #[test]
    fn derive_retry_hands_worktree_over_and_keeps_failure_count() {
        let db = test_db();
        let first = inserted_id(collect(&db, "s1", "implement"));
        run_to_running(&db, &first);
        history(&db, &first, "s1", "implement", "failed");

        let DeriveOutcome::Derived { work_id: second } = db
            .derive(&derive_request(
                &first,
                QueuePhase::Running,
                DeriveKind::EscalationRetry,
            ))
            .unwrap()
        else {
            panic!("expected Derived");
        };
        run_to_running(&db, &second);
        let DeriveOutcome::Derived { work_id: third } = db
            .derive(&derive_request(
                &second,
                QueuePhase::Running,
                DeriveKind::EscalationRetry,
            ))
            .unwrap()
        else {
            panic!("expected Derived");
        };

        let owner = |id: &str| -> Option<String> {
            let conn = db.conn.lock().unwrap();
            conn.query_row(
                "SELECT worktree_owner FROM queue_items WHERE work_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(owner(&first), None);
        assert_eq!(owner(&second).as_deref(), Some(first.as_str()));
        assert_eq!(owner(&third).as_deref(), Some(first.as_str()));
        assert_eq!(db.failure_count(&first).unwrap(), 1);
    }

    #[test]
    fn worktree_key_and_holder_follow_the_handover() {
        let db = test_db();
        let first = inserted_id(collect(&db, "s1", "implement"));
        assert_eq!(db.worktree_key(&first).unwrap(), first);
        assert_eq!(db.worktree_holder(&first).unwrap(), first);

        run_to_running(&db, &first);
        let DeriveOutcome::Derived { work_id: second } = db
            .derive(&derive_request(
                &first,
                QueuePhase::Running,
                DeriveKind::EscalationRetry,
            ))
            .unwrap()
        else {
            panic!("expected Derived");
        };
        assert_eq!(db.worktree_key(&second).unwrap(), first);
        assert_eq!(db.worktree_holder(&first).unwrap(), second);

        run_to_running(&db, &second);
        let DeriveOutcome::Derived { work_id: third } = db
            .derive(&derive_request(
                &second,
                QueuePhase::Running,
                DeriveKind::EscalationRetry,
            ))
            .unwrap()
        else {
            panic!("expected Derived");
        };
        assert_eq!(db.worktree_key(&third).unwrap(), first);
        assert_eq!(db.worktree_holder(&first).unwrap(), third);
        assert!(matches!(
            db.worktree_key("missing"),
            Err(BeltError::ItemNotFound(_))
        ));
    }

    #[test]
    fn derive_replan_resets_failure_count_and_starts_a_new_worktree() {
        let db = test_db();
        let first = inserted_id(collect(&db, "s1", "implement"));
        run_to_running(&db, &first);
        history(&db, &first, "s1", "implement", "failed");
        history(&db, &first, "s1", "implement", "failed");
        // Replan leaves Hitl only as post-processing of a confirmed request.
        let hitl_id = opened(&db, &first);
        db.resolve_hitl(
            &HitlTarget::Id(hitl_id),
            &resolution(HitlAction::Replan, "irene", "cli"),
            None,
        )
        .unwrap();
        assert_eq!(db.failure_count(&first).unwrap(), 2);

        let outcome = db
            .derive(&DeriveRequest {
                reason: TransitionReason::PostProcessing(belt_core::hitl::HitlAction::Replan),
                ..derive_request(&first, QueuePhase::Hitl, DeriveKind::Replan)
            })
            .unwrap();

        let DeriveOutcome::Derived { work_id } = outcome else {
            panic!("expected Derived, got {outcome:?}");
        };
        assert_eq!(db.failure_count(&work_id).unwrap(), 0);
        history(&db, &work_id, "s1", "implement", "failed");
        assert_eq!(db.failure_count(&first).unwrap(), 1);
        let conn = db.conn.lock().unwrap();
        let owner: Option<String> = conn
            .query_row(
                "SELECT worktree_owner FROM queue_items WHERE work_id = ?1",
                params![work_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, None);
    }

    #[test]
    fn derive_inherits_replan_count() {
        let db = test_db();
        let first = inserted_id(collect(&db, "s1", "implement"));
        db.increment_replan_count(&first).unwrap();
        run_to_running(&db, &first);

        let DeriveOutcome::Derived { work_id } = db
            .derive(&derive_request(
                &first,
                QueuePhase::Running,
                DeriveKind::EscalationRetry,
            ))
            .unwrap()
        else {
            panic!("expected Derived");
        };
        assert_eq!(db.get_item(&work_id).unwrap().replan_count, 1);
    }

    #[test]
    fn derive_rejected_by_guard_leaves_everything_untouched() {
        let db = test_db();
        let first = inserted_id(collect(&db, "s1", "implement"));
        run_to_running(&db, &first);

        let outcome = db
            .derive(&DeriveRequest {
                actor: Actor::Cli,
                ..derive_request(&first, QueuePhase::Running, DeriveKind::EscalationRetry)
            })
            .unwrap();
        assert_eq!(
            outcome,
            DeriveOutcome::Rejected(TransitionOutcome::Busy {
                processing: Processing::Handler
            })
        );

        let outcome = db
            .derive(&derive_request(
                &first,
                QueuePhase::Ready,
                DeriveKind::EscalationRetry,
            ))
            .unwrap();
        assert_eq!(
            outcome,
            DeriveOutcome::Rejected(TransitionOutcome::Conflict {
                current: QueuePhase::Running
            })
        );
        assert_eq!(db.get_item(&first).unwrap().phase(), QueuePhase::Running);
        assert_eq!(db.list_items(None, None).unwrap().len(), 1);
    }

    #[test]
    fn failure_count_counts_only_failures_after_the_last_reset() {
        let db = test_db();
        let first = inserted_id(collect(&db, "s1", "implement"));
        let other = inserted_id(collect(&db, "s1", "other"));
        let foreign = inserted_id(collect(&db, "s2", "implement"));
        for status in ["failed", "done", "failed"] {
            history(&db, &first, "s1", "implement", status);
        }
        history(&db, &other, "s1", "other", "failed");
        history(&db, &foreign, "s2", "implement", "failed");
        assert_eq!(db.failure_count(&first).unwrap(), 2);

        db.record_reset(&first).unwrap();
        assert_eq!(db.failure_count(&first).unwrap(), 0);

        history(&db, &first, "s1", "implement", "failed");
        assert_eq!(db.failure_count(&first).unwrap(), 1);
        // Other lineages are untouched by the reset.
        assert_eq!(db.failure_count(&other).unwrap(), 1);
        assert_eq!(db.failure_count(&foreign).unwrap(), 1);
    }

    #[test]
    fn failure_count_does_not_carry_over_to_a_recollected_lineage() {
        let db = test_db();
        let a = inserted_id(collect(&db, "s1", "implement"));
        run_to_running(&db, &a);
        history(&db, &a, "s1", "implement", "failed");
        history(&db, &a, "s1", "implement", "failed");
        step(&db, &a, QueuePhase::Running, QueuePhase::Completed);
        step(&db, &a, QueuePhase::Completed, QueuePhase::Done);
        assert_eq!(db.failure_count(&a).unwrap(), 2);

        let b = inserted_id(collect(&db, "s1", "implement"));
        assert_ne!(a, b);
        assert_eq!(db.failure_count(&b).unwrap(), 0);
        history(&db, &b, "s1", "implement", "failed");
        assert_eq!(db.failure_count(&b).unwrap(), 1);
        assert_eq!(db.failure_count(&a).unwrap(), 2);
    }

    #[test]
    fn failure_count_accumulates_across_escalation_retry_derivation() {
        let db = test_db();
        let first = inserted_id(collect(&db, "s1", "implement"));
        run_to_running(&db, &first);
        history(&db, &first, "s1", "implement", "failed");
        let DeriveOutcome::Derived { work_id: second } = db
            .derive(&derive_request(
                &first,
                QueuePhase::Running,
                DeriveKind::EscalationRetry,
            ))
            .unwrap()
        else {
            panic!("expected Derived");
        };
        history(&db, &second, "s1", "implement", "failed");
        assert_eq!(db.failure_count(&second).unwrap(), 2);
        assert_eq!(db.failure_count(&first).unwrap(), 2);
    }

    #[test]
    fn failure_count_and_record_reset_reject_unknown_work_id() {
        let db = test_db();
        assert!(matches!(
            db.failure_count("nope"),
            Err(BeltError::ItemNotFound(_))
        ));
        assert!(matches!(
            db.record_reset("nope"),
            Err(BeltError::ItemNotFound(_))
        ));
    }

    #[test]
    fn failure_count_treats_legacy_completed_history_as_a_non_failure() {
        let db = test_db();
        let id = inserted_id(collect(&db, "s1", "implement"));
        history(&db, &id, "s1", "implement", "completed");
        history(&db, &id, "s1", "implement", "failed");
        history(&db, &id, "s1", "implement", "completed");
        assert_eq!(db.failure_count(&id).unwrap(), 1);
    }

    #[test]
    fn latest_in_lineage_follows_creation_order_not_rowid() {
        let db = test_db();
        let first = inserted_id(collect(&db, "s1", "implement"));
        run_to_running(&db, &first);
        let second = match db.derive(&derive_request(
            &first,
            QueuePhase::Running,
            DeriveKind::EscalationRetry,
        )) {
            Ok(DeriveOutcome::Derived { work_id }) => work_id,
            other => panic!("expected Derived, got {other:?}"),
        };
        // Re-insert the newest row so that it gets the lowest rowid.
        {
            let conn = db.conn.lock().unwrap();
            let rowid: i64 = conn
                .query_row("SELECT MIN(rowid) FROM queue_items", [], |r| r.get(0))
                .unwrap();
            conn.execute(
                "UPDATE queue_items SET rowid = ?1 WHERE work_id = ?2",
                params![rowid - 1, second],
            )
            .unwrap();
        }
        assert_eq!(db.latest_in_lineage(&first).unwrap().work_id, second);
    }

    #[test]
    fn derive_replan_marks_the_confirmed_request_post_processed() {
        let db = test_db();
        let first = inserted_id(collect(&db, "s1", "implement"));
        run_to_running(&db, &first);
        let hitl_id = opened(&db, &first);
        db.resolve_hitl(
            &HitlTarget::Id(hitl_id.clone()),
            &resolution(HitlAction::Replan, "irene", "cli"),
            None,
        )
        .unwrap();
        assert_eq!(db.pending_post_processing().unwrap().len(), 1);

        let outcome = db
            .derive(&DeriveRequest {
                reason: TransitionReason::PostProcessing(HitlAction::Replan),
                ..derive_request(&first, QueuePhase::Hitl, DeriveKind::Replan)
            })
            .unwrap();

        assert!(
            matches!(outcome, DeriveOutcome::Derived { .. }),
            "{outcome:?}"
        );
        assert!(db.pending_post_processing().unwrap().is_empty());
        let stored = db.hitl_request(&hitl_id).unwrap().unwrap();
        assert!(stored.post_processed_at.is_some());
    }

    #[test]
    fn derive_from_running_has_no_hitl_request_to_mark() {
        let db = test_db();
        let first = inserted_id(collect(&db, "s1", "implement"));
        run_to_running(&db, &first);
        let outcome = db
            .derive(&derive_request(
                &first,
                QueuePhase::Running,
                DeriveKind::EscalationRetry,
            ))
            .unwrap();
        assert!(matches!(outcome, DeriveOutcome::Derived { .. }));
        let conn = db.conn.lock().unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM hitl_requests", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn concurrent_collection_of_one_series_inserts_once_and_reports_duplicate_once() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("belt.db");
        let path = path.to_str().unwrap().to_string();
        let _migrated = Database::open(&path).unwrap();

        for round in 0..10 {
            let source = format!("s{round}");
            let barrier = Arc::new(Barrier::new(2));
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let (path, source, barrier) =
                        (path.clone(), source.clone(), Arc::clone(&barrier));
                    std::thread::spawn(move || {
                        let db = Database::open(&path).unwrap();
                        barrier.wait();
                        db.insert_collected(&new_item(&source, "implement"))
                    })
                })
                .collect();
            let outcomes: Vec<_> = handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap()
                        .expect("no database error under contention")
                })
                .collect();
            let inserted = outcomes
                .iter()
                .filter(|o| matches!(o, CollectOutcome::Inserted { .. }))
                .count();
            let duplicates = outcomes
                .iter()
                .filter(|o| matches!(o, CollectOutcome::Duplicate))
                .count();
            assert_eq!(
                (inserted, duplicates),
                (1, 1),
                "round {round}: {outcomes:?}"
            );
        }
    }

    #[test]
    fn file_database_connection_uses_wal_and_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("belt.db");
        let db = Database::open(path.to_str().unwrap()).unwrap();
        let conn = db.conn.lock().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        let timeout: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        assert!(timeout > 0, "busy_timeout = {timeout}");
    }

    #[test]
    fn failure_count_rejects_unknown_history_status() {
        let db = test_db();
        let id = inserted_id(collect(&db, "s1", "implement"));
        history(&db, &id, "s1", "implement", "weird");
        assert!(matches!(db.failure_count(&id), Err(BeltError::Database(_))));
    }

    #[test]
    fn latest_in_lineage_follows_derivation_and_ignores_other_lineages() {
        let db = test_db();
        let first = inserted_id(collect(&db, "s1", "implement"));
        run_to_running(&db, &first);
        let DeriveOutcome::Derived { work_id: second } = db
            .derive(&derive_request(
                &first,
                QueuePhase::Running,
                DeriveKind::EscalationRetry,
            ))
            .unwrap()
        else {
            panic!("expected Derived");
        };

        assert_eq!(db.latest_in_lineage(&first).unwrap().work_id, second);
        assert_eq!(db.latest_in_lineage(&second).unwrap().work_id, second);

        // A recollected item is a different lineage.
        step(&db, &second, QueuePhase::Pending, QueuePhase::Skipped);
        let third = inserted_id(collect(&db, "s1", "implement"));
        assert_eq!(db.latest_in_lineage(&first).unwrap().work_id, second);
        assert_eq!(db.latest_in_lineage(&third).unwrap().work_id, third);
        assert!(matches!(
            db.latest_in_lineage("missing"),
            Err(BeltError::ItemNotFound(_))
        ));
    }

    // ---- HITL request store --------------------------------------------------

    use belt_core::hitl::{
        ConfirmPath, HitlAction, HitlId, HitlResolution, HitlStatus, RespondOutcome,
    };
    use belt_core::queue::HitlReason;

    fn open_req(work_id: &str) -> OpenHitlRequest {
        OpenHitlRequest {
            work_id: work_id.to_string(),
            expected_from: QueuePhase::Running,
            reason: HitlReason::EvaluateFailure,
            notes: Some("needs review".to_string()),
            actor: Actor::Daemon,
            transition_reason: TransitionReason::Escalation(EscalationAction::Hitl),
            timeout_at: Some("2099-01-01T00:00:00Z".to_string()),
            terminal_action: Some(EscalationAction::Skip),
        }
    }

    fn running_item(db: &Database, source: &str) -> String {
        let id = inserted_id(collect(db, source, "analyze"));
        run_to_running(db, &id);
        id
    }

    fn opened(db: &Database, work_id: &str) -> HitlId {
        match db.open_hitl(&open_req(work_id)).unwrap() {
            OpenHitlOutcome::Opened { hitl_id, .. } => hitl_id,
            other => panic!("expected Opened, got {other:?}"),
        }
    }

    fn resolution(action: HitlAction, by: &str, via: &str) -> HitlResolution {
        HitlResolution {
            action,
            by: by.to_string(),
            via: via.to_string(),
            at: Utc::now().to_rfc3339(),
            path: ConfirmPath::Direct,
        }
    }

    fn post_processing(work_id: &str, to: QueuePhase, action: HitlAction) -> TransitionRequest {
        TransitionRequest {
            work_id: work_id.to_string(),
            expected_from: QueuePhase::Hitl,
            to,
            actor: Actor::Daemon,
            reason: TransitionReason::PostProcessing(action),
            detail: None,
        }
    }

    fn hitl_rows(db: &Database, work_id: &str) -> i64 {
        let conn = db.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM hitl_requests WHERE work_id = ?1",
            params![work_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn open_hitl_enters_hitl_and_creates_open_request_together() {
        let db = test_db();
        let id = running_item(&db, "s1");

        let OpenHitlOutcome::Opened { hitl_id, seq } = db.open_hitl(&open_req(&id)).unwrap() else {
            panic!("expected Opened");
        };

        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Hitl);
        let stored = db.hitl_request(&hitl_id).unwrap().expect("request row");
        assert_eq!(stored.work_id, id);
        assert_eq!(stored.status, HitlStatus::Open);
        assert_eq!(stored.reason, Some(HitlReason::EvaluateFailure));
        assert_eq!(stored.notes.as_deref(), Some("needs review"));
        assert_eq!(stored.terminal_action, Some(EscalationAction::Skip));
        assert_eq!(stored.timeout_at.as_deref(), Some("2099-01-01T00:00:00Z"));
        assert_eq!(stored.resolution, None);
        assert_eq!(stored.post_processed_at, None);
        assert_eq!(stored.post_processing_failures, 0);

        let log = db.transitions_of(&id).unwrap();
        let last = log.last().unwrap();
        assert_eq!(last.seq, seq);
        assert_eq!(last.kind, transition_kind::PHASE_ENTER);
        assert_eq!(last.to_phase.as_deref(), Some("hitl"));
        assert_eq!(last.reason.as_deref(), Some("escalation:hitl"));
    }

    #[test]
    fn hitl_id_is_unique_and_disjoint_from_legacy_ids() {
        let db = test_db();
        let a = running_item(&db, "s1");
        let b = running_item(&db, "s2");
        let first = opened(&db, &a);
        let second = opened(&db, &b);

        assert_ne!(first, second);
        for id in [&first, &second] {
            assert!(id.as_str().starts_with("hitl-"));
            assert!(!id.as_str().starts_with("hitl-legacy-"));
        }
    }

    #[test]
    fn open_hitl_failing_request_insert_rolls_back_the_phase_change() {
        let db = test_db();
        let id = running_item(&db, "s1");
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO hitl_requests (hitl_id, work_id, status, opened_at)
                 VALUES ('stray', ?1, 'open', 't')",
                params![id],
            )
            .unwrap();
        }

        assert!(db.open_hitl(&open_req(&id)).is_err());

        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Running);
        assert_eq!(hitl_rows(&db, &id), 1);
        assert!(
            db.transitions_of(&id)
                .unwrap()
                .iter()
                .all(|e| e.to_phase.as_deref() != Some("hitl"))
        );
    }

    #[test]
    fn second_open_hitl_on_an_open_item_is_rejected() {
        let db = test_db();
        let id = running_item(&db, "s1");
        opened(&db, &id);

        for from in [QueuePhase::Running, QueuePhase::Hitl] {
            let mut again = open_req(&id);
            again.expected_from = from;
            assert_eq!(
                db.open_hitl(&again).unwrap(),
                OpenHitlOutcome::Rejected(TransitionOutcome::InvalidAction {
                    current: QueuePhase::Hitl
                })
            );
        }
        assert_eq!(hitl_rows(&db, &id), 1);
    }

    #[test]
    fn open_hitl_by_non_owner_on_running_item_is_busy_and_creates_no_request() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let mut req = open_req(&id);
        req.actor = Actor::Cli;

        assert_eq!(
            db.open_hitl(&req).unwrap(),
            OpenHitlOutcome::Rejected(TransitionOutcome::Busy {
                processing: Processing::Handler
            })
        );
        assert_eq!(hitl_rows(&db, &id), 0);
        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Running);
    }

    #[test]
    fn open_hitl_unknown_item_is_item_not_found() {
        let db = test_db();
        assert!(matches!(
            db.open_hitl(&open_req("nope")),
            Err(BeltError::ItemNotFound(_))
        ));
    }

    #[test]
    fn resolve_hitl_first_response_wins_and_second_sees_the_winner() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let hitl_id = opened(&db, &id);

        let winner = resolution(HitlAction::Retry, "irene", "cli");
        assert_eq!(
            db.resolve_hitl(&HitlTarget::Id(hitl_id.clone()), &winner, Some("again"))
                .unwrap(),
            RespondOutcome::Won {
                hitl_id: hitl_id.clone()
            }
        );

        let late = resolution(HitlAction::Skip, "bob", "github");
        assert_eq!(
            db.resolve_hitl(&HitlTarget::Item(id.clone()), &late, None)
                .unwrap(),
            RespondOutcome::AlreadyHandled(winner.clone())
        );
        assert_eq!(
            db.resolve_hitl(&HitlTarget::Id(hitl_id.clone()), &late, None)
                .unwrap(),
            RespondOutcome::AlreadyHandled(winner.clone())
        );

        let stored = db.hitl_request(&hitl_id).unwrap().unwrap();
        assert_eq!(stored.status, HitlStatus::Resolved);
        assert_eq!(stored.resolution, Some(winner));
        assert_eq!(stored.resolution_notes.as_deref(), Some("again"));
        // The phase changes only through post-processing.
        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Hitl);
    }

    #[test]
    fn resolve_hitl_unknown_target_is_not_found() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let any = resolution(HitlAction::Done, "irene", "cli");

        assert_eq!(
            db.resolve_hitl(&HitlTarget::Id(HitlId::new("missing")), &any, None)
                .unwrap(),
            RespondOutcome::NotFound
        );
        assert_eq!(
            db.resolve_hitl(&HitlTarget::Item(id), &any, None).unwrap(),
            RespondOutcome::NotFound
        );
    }

    #[test]
    fn resolve_hitl_by_item_targets_the_current_request_not_an_old_one() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let first = opened(&db, &id);
        let old = resolution(HitlAction::Retry, "irene", "cli");
        db.resolve_hitl(&HitlTarget::Id(first.clone()), &old, None)
            .unwrap();
        db.complete_post_processing(
            &first,
            &post_processing(&id, QueuePhase::Pending, HitlAction::Retry),
        )
        .unwrap();
        run_to_running(&db, &id);
        let second = opened(&db, &id);
        assert_ne!(first, second);

        let fresh = resolution(HitlAction::Done, "bob", "tui");
        assert_eq!(
            db.resolve_hitl(&HitlTarget::Item(id.clone()), &fresh, None)
                .unwrap(),
            RespondOutcome::Won { hitl_id: second }
        );
        // A late answer addressed to the old request does not touch the new one.
        assert_eq!(
            db.resolve_hitl(&HitlTarget::Id(first), &fresh, None)
                .unwrap(),
            RespondOutcome::AlreadyHandled(old)
        );
    }

    #[test]
    fn concurrent_resolve_hitl_has_one_winner_and_the_loser_sees_it() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("belt.db");
        let path = path.to_str().unwrap().to_string();
        let setup = Database::open(&path).unwrap();

        for round in 0..10 {
            let id = running_item(&setup, &format!("s{round}"));
            let hitl_id = opened(&setup, &id);
            let barrier = Arc::new(Barrier::new(2));
            let handles: Vec<_> = [("cli", HitlAction::Done), ("github", HitlAction::Skip)]
                .into_iter()
                .map(|(via, action)| {
                    let path = path.clone();
                    let hitl_id = hitl_id.clone();
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        let db = Database::open(&path).unwrap();
                        let r = resolution(action, via, via);
                        barrier.wait();
                        (
                            r.clone(),
                            db.resolve_hitl(&HitlTarget::Id(hitl_id), &r, None),
                        )
                    })
                })
                .collect();
            let results: Vec<_> = handles
                .into_iter()
                .map(|h| {
                    let (r, outcome) = h.join().unwrap();
                    (r, outcome.expect("no database error under contention"))
                })
                .collect();

            let winners: Vec<_> = results
                .iter()
                .filter(|(_, o)| matches!(o, RespondOutcome::Won { .. }))
                .collect();
            assert_eq!(winners.len(), 1, "round {round}: {results:?}");
            let winning = &winners[0].0;
            let loser = results
                .iter()
                .find(|(_, o)| matches!(o, RespondOutcome::AlreadyHandled(_)))
                .unwrap_or_else(|| panic!("round {round}: no loser in {results:?}"));
            assert_eq!(loser.1, RespondOutcome::AlreadyHandled(winning.clone()));
        }
    }

    #[test]
    fn expire_hitl_then_resolve_reports_the_expiry_as_already_handled() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let hitl_id = opened(&db, &id);

        assert_eq!(
            db.expire_hitl(&hitl_id, EscalationAction::Skip).unwrap(),
            RespondOutcome::Won {
                hitl_id: hitl_id.clone()
            }
        );

        let late = resolution(HitlAction::Done, "irene", "cli");
        let RespondOutcome::AlreadyHandled(winner) = db
            .resolve_hitl(&HitlTarget::Id(hitl_id.clone()), &late, None)
            .unwrap()
        else {
            panic!("expected AlreadyHandled");
        };
        assert_eq!(winner.action, HitlAction::Skip);
        assert_eq!(winner.via, "timeout");
        assert_eq!(
            db.hitl_request(&hitl_id).unwrap().unwrap().status,
            HitlStatus::Expired
        );
    }

    #[test]
    fn resolve_then_expire_keeps_the_response() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let hitl_id = opened(&db, &id);
        let human = resolution(HitlAction::Retry, "irene", "cli");
        db.resolve_hitl(&HitlTarget::Id(hitl_id.clone()), &human, None)
            .unwrap();

        assert_eq!(
            db.expire_hitl(&hitl_id, EscalationAction::Replan).unwrap(),
            RespondOutcome::AlreadyHandled(human.clone())
        );
        let stored = db.hitl_request(&hitl_id).unwrap().unwrap();
        assert_eq!(stored.status, HitlStatus::Resolved);
        assert_eq!(stored.resolution, Some(human));
    }

    #[test]
    fn expire_hitl_rejects_non_terminal_actions_and_unknown_requests() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let hitl_id = opened(&db, &id);

        for action in [
            EscalationAction::Retry,
            EscalationAction::RetryWithComment,
            EscalationAction::Hitl,
        ] {
            assert_eq!(
                db.expire_hitl(&hitl_id, action).unwrap(),
                RespondOutcome::InvalidAction
            );
        }
        assert_eq!(
            db.hitl_request(&hitl_id).unwrap().unwrap().status,
            HitlStatus::Open
        );
        assert_eq!(
            db.expire_hitl(&HitlId::new("missing"), EscalationAction::Skip)
                .unwrap(),
            RespondOutcome::NotFound
        );
    }

    #[test]
    fn concurrent_expire_and_resolve_let_exactly_one_win() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("belt.db");
        let path = path.to_str().unwrap().to_string();
        let setup = Database::open(&path).unwrap();

        for round in 0..10 {
            let id = running_item(&setup, &format!("s{round}"));
            let hitl_id = opened(&setup, &id);
            let barrier = Arc::new(Barrier::new(2));

            let expirer = {
                let (path, hitl_id, barrier) =
                    (path.clone(), hitl_id.clone(), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    let db = Database::open(&path).unwrap();
                    barrier.wait();
                    db.expire_hitl(&hitl_id, EscalationAction::Skip)
                })
            };
            let responder = {
                let (path, hitl_id, barrier) =
                    (path.clone(), hitl_id.clone(), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    let db = Database::open(&path).unwrap();
                    barrier.wait();
                    db.resolve_hitl(
                        &HitlTarget::Id(hitl_id),
                        &resolution(HitlAction::Done, "irene", "cli"),
                        None,
                    )
                })
            };
            let expired = expirer.join().unwrap().expect("no database error");
            let resolved = responder.join().unwrap().expect("no database error");

            let stored = setup.hitl_request(&hitl_id).unwrap().unwrap();
            match stored.status {
                HitlStatus::Expired => {
                    assert!(
                        matches!(expired, RespondOutcome::Won { .. }),
                        "round {round}"
                    );
                    assert!(matches!(resolved, RespondOutcome::AlreadyHandled(_)));
                }
                HitlStatus::Resolved => {
                    assert!(
                        matches!(resolved, RespondOutcome::Won { .. }),
                        "round {round}"
                    );
                    assert!(matches!(expired, RespondOutcome::AlreadyHandled(_)));
                }
                HitlStatus::Open => panic!("round {round}: nobody won"),
            }
        }
    }

    #[test]
    fn confirmed_request_makes_the_item_busy_for_non_owners() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let hitl_id = opened(&db, &id);

        // Open: a CLI skip maps to a HITL response, not to busy.
        assert_eq!(
            db.transition(&request(
                &id,
                QueuePhase::Hitl,
                QueuePhase::Skipped,
                Actor::Cli
            ))
            .unwrap(),
            TransitionOutcome::InvalidAction {
                current: QueuePhase::Hitl
            }
        );

        db.resolve_hitl(
            &HitlTarget::Id(hitl_id),
            &resolution(HitlAction::Skip, "irene", "cli"),
            None,
        )
        .unwrap();

        assert_eq!(
            db.transition(&request(
                &id,
                QueuePhase::Hitl,
                QueuePhase::Skipped,
                Actor::Cli
            ))
            .unwrap(),
            TransitionOutcome::Busy {
                processing: Processing::PostProcessing
            }
        );
    }

    #[test]
    fn daemon_post_processing_cannot_leave_hitl_while_the_request_is_open() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let hitl_id = opened(&db, &id);

        for (to, action) in [
            (QueuePhase::Done, HitlAction::Done),
            (QueuePhase::Skipped, HitlAction::Skip),
            (QueuePhase::Pending, HitlAction::Retry),
            (QueuePhase::Failed, HitlAction::Replan),
        ] {
            assert_eq!(
                db.transition(&post_processing(&id, to, action)).unwrap(),
                TransitionOutcome::InvalidAction {
                    current: QueuePhase::Hitl
                },
                "to {to:?}"
            );
        }
        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Hitl);
        assert_eq!(
            db.hitl_request(&hitl_id).unwrap().unwrap().status,
            HitlStatus::Open
        );
    }

    #[test]
    fn cli_skip_on_open_hitl_maps_to_a_response_through_the_core_mapping() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let hitl_id = opened(&db, &id);
        let skip = request(&id, QueuePhase::Hitl, QueuePhase::Skipped, Actor::Cli);

        let outcome = db.transition(&skip).unwrap();
        assert_eq!(
            outcome,
            TransitionOutcome::InvalidAction {
                current: QueuePhase::Hitl
            }
        );
        let action = belt_core::transition::hitl_response_for(skip.to)
            .expect("queue skip on Hitl is a HITL response");
        assert_eq!(
            db.resolve_hitl(
                &HitlTarget::Item(id.clone()),
                &resolution(action, "irene", "cli"),
                None
            )
            .unwrap(),
            RespondOutcome::Won { hitl_id }
        );
    }

    #[test]
    fn resolve_hitl_by_item_ignores_requests_already_post_processed() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let hitl_id = opened(&db, &id);
        db.resolve_hitl(
            &HitlTarget::Id(hitl_id.clone()),
            &resolution(HitlAction::Retry, "irene", "cli"),
            None,
        )
        .unwrap();
        db.complete_post_processing(
            &hitl_id,
            &post_processing(&id, QueuePhase::Pending, HitlAction::Retry),
        )
        .unwrap();

        // The item left Hitl; its old request is history, not a target.
        assert_eq!(
            db.resolve_hitl(
                &HitlTarget::Item(id.clone()),
                &resolution(HitlAction::Done, "bob", "cli"),
                None
            )
            .unwrap(),
            RespondOutcome::NotFound
        );
        // Addressed by id, the old request still reports its winner.
        assert!(matches!(
            db.resolve_hitl(
                &HitlTarget::Id(hitl_id),
                &resolution(HitlAction::Done, "bob", "cli"),
                None
            )
            .unwrap(),
            RespondOutcome::AlreadyHandled(_)
        ));
    }

    #[test]
    fn expired_request_also_makes_the_item_busy() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let hitl_id = opened(&db, &id);
        db.expire_hitl(&hitl_id, EscalationAction::Replan).unwrap();

        assert_eq!(
            db.transition(&request(
                &id,
                QueuePhase::Hitl,
                QueuePhase::Skipped,
                Actor::Tui
            ))
            .unwrap(),
            TransitionOutcome::Busy {
                processing: Processing::PostProcessing
            }
        );
    }

    #[test]
    fn pending_post_processing_lists_only_confirmed_unprocessed_requests() {
        let db = test_db();
        let open_item = running_item(&db, "s-open");
        let resolved_item = running_item(&db, "s-resolved");
        let expired_item = running_item(&db, "s-expired");
        let _open = opened(&db, &open_item);
        let resolved = opened(&db, &resolved_item);
        let expired = opened(&db, &expired_item);
        db.resolve_hitl(
            &HitlTarget::Id(resolved.clone()),
            &resolution(HitlAction::Done, "irene", "cli"),
            None,
        )
        .unwrap();
        db.expire_hitl(&expired, EscalationAction::Skip).unwrap();

        let pending = db.pending_post_processing().unwrap();

        let ids: Vec<_> = pending.iter().map(|r| r.hitl_id.clone()).collect();
        assert_eq!(ids, vec![resolved, expired]);
        assert_eq!(pending[0].status, HitlStatus::Resolved);
        assert_eq!(pending[1].status, HitlStatus::Expired);
    }

    #[test]
    fn complete_post_processing_applies_result_transition_and_marks_done_together() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let hitl_id = opened(&db, &id);
        db.resolve_hitl(
            &HitlTarget::Id(hitl_id.clone()),
            &resolution(HitlAction::Done, "irene", "cli"),
            None,
        )
        .unwrap();

        let outcome = db
            .complete_post_processing(
                &hitl_id,
                &post_processing(&id, QueuePhase::Done, HitlAction::Done),
            )
            .unwrap();

        assert!(
            matches!(outcome, CompleteOutcome::Completed { .. }),
            "{outcome:?}"
        );
        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Done);
        assert!(db.pending_post_processing().unwrap().is_empty());
        let stored = db.hitl_request(&hitl_id).unwrap().unwrap();
        assert!(stored.post_processed_at.is_some());
        assert_eq!(
            db.complete_post_processing(
                &hitl_id,
                &post_processing(&id, QueuePhase::Done, HitlAction::Done),
            )
            .unwrap(),
            CompleteOutcome::NotPending
        );
    }

    #[test]
    fn complete_post_processing_before_confirmation_is_not_pending() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let hitl_id = opened(&db, &id);

        assert_eq!(
            db.complete_post_processing(
                &hitl_id,
                &post_processing(&id, QueuePhase::Done, HitlAction::Done),
            )
            .unwrap(),
            CompleteOutcome::NotPending
        );
        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Hitl);
    }

    #[test]
    fn rejected_result_transition_leaves_the_request_pending() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let hitl_id = opened(&db, &id);
        db.expire_hitl(&hitl_id, EscalationAction::Skip).unwrap();

        // Not a daemon post-processing transition: the guard refuses.
        let mut by_cli = post_processing(&id, QueuePhase::Skipped, HitlAction::Skip);
        by_cli.actor = Actor::Cli;
        assert_eq!(
            db.complete_post_processing(&hitl_id, &by_cli).unwrap(),
            CompleteOutcome::Rejected(TransitionOutcome::Busy {
                processing: Processing::PostProcessing
            })
        );
        // A transition outside the state machine is refused as well.
        let invalid = post_processing(&id, QueuePhase::Hitl, HitlAction::Skip);
        assert!(matches!(
            db.complete_post_processing(&hitl_id, &invalid).unwrap(),
            CompleteOutcome::Rejected(TransitionOutcome::InvalidAction { .. })
        ));

        assert_eq!(db.get_item(&id).unwrap().phase(), QueuePhase::Hitl);
        assert_eq!(db.pending_post_processing().unwrap().len(), 1);
    }

    #[test]
    fn complete_post_processing_for_another_item_is_an_error() {
        let db = test_db();
        let a = running_item(&db, "s1");
        let b = running_item(&db, "s2");
        let hitl_a = opened(&db, &a);
        opened(&db, &b);
        db.expire_hitl(&hitl_a, EscalationAction::Skip).unwrap();

        assert!(
            db.complete_post_processing(
                &hitl_a,
                &post_processing(&b, QueuePhase::Skipped, HitlAction::Skip),
            )
            .is_err()
        );
    }

    #[test]
    fn post_processing_failures_accumulate_until_completion() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let hitl_id = opened(&db, &id);
        db.expire_hitl(&hitl_id, EscalationAction::Skip).unwrap();

        assert_eq!(db.record_post_processing_failure(&hitl_id).unwrap(), 1);
        assert_eq!(db.record_post_processing_failure(&hitl_id).unwrap(), 2);
        assert_eq!(
            db.hitl_request(&hitl_id)
                .unwrap()
                .unwrap()
                .post_processing_failures,
            2
        );

        db.complete_post_processing(
            &hitl_id,
            &post_processing(&id, QueuePhase::Skipped, HitlAction::Skip),
        )
        .unwrap();
        assert!(db.record_post_processing_failure(&hitl_id).is_err());
    }

    #[test]
    fn post_processing_failure_on_open_or_unknown_request_is_an_error() {
        let db = test_db();
        let id = running_item(&db, "s1");
        let hitl_id = opened(&db, &id);

        assert!(db.record_post_processing_failure(&hitl_id).is_err());
        assert!(
            db.record_post_processing_failure(&HitlId::new("missing"))
                .is_err()
        );
    }
}
