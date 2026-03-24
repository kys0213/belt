//! SQLite persistence layer for Belt.
//!
//! Provides CRUD operations for queue items, history events, workspaces,
//! cron jobs, and token usage — all backed by a single SQLite database.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use belt_core::error::BeltError;
use belt_core::phase::QueuePhase;
use belt_core::queue::QueueItem;
use belt_core::runtime::TokenUsage;
use belt_core::spec::{Spec, SpecStatus};

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
    /// Optional workspace scope; `None` means global.
    pub workspace: Option<String>,
    /// Whether this job is currently enabled.
    pub enabled: bool,
    /// Timestamp of the last successful run, if any.
    pub last_run_at: Option<String>,
    /// When this job was created (RFC 3339).
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

/// SQLite-backed persistence for Belt state.
///
/// The inner connection is wrapped in a [`Mutex`] so that `Database` is
/// `Send + Sync` and can be shared across async tasks.
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// Open (or create) a database at the given path and initialize the schema.
    ///
    /// # Errors
    /// Returns `BeltError::Database` if the connection or schema creation fails.
    pub fn open(path: &str) -> Result<Self, BeltError> {
        let conn = Connection::open(path).map_err(|e| BeltError::Database(e.to_string()))?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.init()?;
        Ok(db)
    }

    /// Open an in-memory database — useful for testing.
    ///
    /// # Errors
    /// Returns `BeltError::Database` if schema creation fails.
    pub fn open_in_memory() -> Result<Self, BeltError> {
        let conn = Connection::open_in_memory().map_err(|e| BeltError::Database(e.to_string()))?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.init()?;
        Ok(db)
    }

    /// Create all tables if they do not already exist.
    fn init(&self) -> Result<(), BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS queue_items (
                work_id          TEXT PRIMARY KEY,
                source_id        TEXT NOT NULL,
                workspace_id     TEXT NOT NULL,
                state            TEXT NOT NULL,
                phase            TEXT NOT NULL,
                title            TEXT,
                created_at       TEXT NOT NULL,
                updated_at       TEXT NOT NULL,
                hitl_created_at  TEXT,
                hitl_respondent  TEXT,
                hitl_notes       TEXT,
                hitl_reason      TEXT
            );

            CREATE TABLE IF NOT EXISTS history (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                work_id    TEXT NOT NULL,
                source_id  TEXT NOT NULL,
                state      TEXT NOT NULL,
                status     TEXT NOT NULL,
                attempt    INTEGER NOT NULL,
                summary    TEXT,
                error      TEXT,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS workspaces (
                name        TEXT PRIMARY KEY,
                config_path TEXT NOT NULL,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS cron_jobs (
                name        TEXT PRIMARY KEY,
                schedule    TEXT NOT NULL,
                workspace   TEXT,
                enabled     INTEGER NOT NULL DEFAULT 1,
                last_run_at TEXT,
                created_at  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS token_usage (
                id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                work_id            TEXT NOT NULL,
                workspace          TEXT NOT NULL,
                runtime            TEXT NOT NULL,
                model              TEXT NOT NULL,
                input_tokens       INTEGER NOT NULL,
                output_tokens      INTEGER NOT NULL,
                cache_read_tokens  INTEGER,
                cache_write_tokens INTEGER,
                duration_ms        INTEGER,
                created_at         TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS specs (
                id           TEXT PRIMARY KEY,
                workspace_id TEXT NOT NULL,
                name         TEXT NOT NULL,
                status       TEXT NOT NULL,
                content      TEXT NOT NULL,
                priority     INTEGER,
                labels       TEXT,
                depends_on   TEXT,
                created_at   TEXT NOT NULL,
                updated_at   TEXT NOT NULL
            );
            ",
        )
        .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(())
    }

    // ---- Queue CRUD --------------------------------------------------------

    /// Insert a new queue item.
    ///
    /// # Errors
    /// Returns `BeltError::Database` on constraint violation or I/O error.
    pub fn insert_item(&self, item: &QueueItem) -> Result<(), BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        conn.execute(
            "INSERT INTO queue_items (work_id, source_id, workspace_id, state, phase, title, created_at, updated_at, hitl_created_at, hitl_respondent, hitl_notes, hitl_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                item.work_id,
                item.source_id,
                item.workspace_id,
                item.state,
                phase_to_str(item.phase),
                item.title,
                item.created_at,
                item.updated_at,
                item.hitl_created_at,
                item.hitl_respondent,
                item.hitl_notes,
                item.hitl_reason.map(|r| r.to_string()),
            ],
        )
        .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(())
    }

    /// Update the phase of an existing queue item.
    ///
    /// Also refreshes `updated_at` to the current UTC time.
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

    /// Update HITL metadata when responding to a HITL item.
    ///
    /// Sets `hitl_respondent`, `hitl_notes`, phase, and refreshes `updated_at`.
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
            "SELECT work_id, source_id, workspace_id, state, phase, title, created_at, updated_at, hitl_created_at, hitl_respondent, hitl_notes, hitl_reason
                 FROM queue_items WHERE work_id = ?1",
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
        let mut sql = String::from(
            "SELECT work_id, source_id, workspace_id, state, phase, title, created_at, updated_at, hitl_created_at, hitl_respondent, hitl_notes, hitl_reason FROM queue_items WHERE 1=1",
        );
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
        workspace: Option<&str>,
    ) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        conn.execute(
            "INSERT INTO cron_jobs (name, schedule, workspace, enabled, created_at)
                 VALUES (?1, ?2, ?3, 1, ?4)",
            params![name, schedule, workspace, now],
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
                "SELECT name, schedule, workspace, enabled, last_run_at, created_at
                 FROM cron_jobs ORDER BY name",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let jobs = stmt
            .query_map([], |row| {
                let enabled_int: i32 = row.get(3)?;
                Ok(CronJob {
                    name: row.get(0)?,
                    schedule: row.get(1)?,
                    workspace: row.get(2)?,
                    enabled: enabled_int != 0,
                    last_run_at: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(jobs)
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
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE cron_jobs SET schedule = ?1 WHERE name = ?2",
                params![schedule, name],
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

    // ---- Specs -------------------------------------------------------------

    /// Insert a new spec.
    ///
    /// # Errors
    /// Returns `BeltError::Database` on constraint violation or I/O error.
    pub fn insert_spec(&self, spec: &Spec) -> Result<(), BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        conn.execute(
            "INSERT INTO specs (id, workspace_id, name, status, content, priority, labels, depends_on, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                spec.id,
                spec.workspace_id,
                spec.name,
                spec.status.as_str(),
                spec.content,
                spec.priority,
                spec.labels,
                spec.depends_on,
                spec.created_at,
                spec.updated_at,
            ],
        )
        .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(())
    }

    /// Retrieve a single spec by ID.
    ///
    /// # Errors
    /// Returns `BeltError::SpecNotFound` if no row matches.
    pub fn get_spec(&self, id: &str) -> Result<Spec, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        conn.query_row(
            "SELECT id, workspace_id, name, status, content, priority, labels, depends_on, created_at, updated_at
                 FROM specs WHERE id = ?1",
            params![id],
            |row| Ok(row_to_spec(row)),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => BeltError::SpecNotFound(id.to_string()),
            other => BeltError::Database(other.to_string()),
        })?
    }

    /// List all specs, optionally filtered by workspace and/or status.
    pub fn list_specs(
        &self,
        workspace: Option<&str>,
        status: Option<SpecStatus>,
    ) -> Result<Vec<Spec>, BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let mut sql = String::from(
            "SELECT id, workspace_id, name, status, content, priority, labels, depends_on, created_at, updated_at FROM specs WHERE 1=1",
        );
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(ws) = workspace {
            sql.push_str(" AND workspace_id = ?");
            param_values.push(Box::new(ws.to_string()));
        }
        if let Some(s) = status {
            sql.push_str(" AND status = ?");
            param_values.push(Box::new(s.as_str().to_string()));
        }

        sql.push_str(" ORDER BY created_at ASC");

        let params_ref: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let specs = stmt
            .query_map(params_ref.as_slice(), |row| Ok(row_to_spec(row)))
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;

        specs.into_iter().collect::<Result<Vec<_>, _>>()
    }

    /// Update a spec's name, content, priority, labels, and depends_on.
    ///
    /// # Errors
    /// Returns `BeltError::SpecNotFound` if no spec matches the given ID.
    pub fn update_spec(&self, spec: &Spec) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE specs SET name = ?1, content = ?2, priority = ?3, labels = ?4, depends_on = ?5, updated_at = ?6 WHERE id = ?7",
                params![
                    spec.name,
                    spec.content,
                    spec.priority,
                    spec.labels,
                    spec.depends_on,
                    now,
                    spec.id,
                ],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::SpecNotFound(spec.id.clone()));
        }
        Ok(())
    }

    /// Update the status of a spec.
    ///
    /// This method does NOT validate state machine transitions; the caller
    /// is responsible for checking `SpecStatus::can_transition_to` before
    /// calling this.
    ///
    /// # Errors
    /// Returns `BeltError::SpecNotFound` if no spec matches the given ID.
    pub fn update_spec_status(&self, id: &str, status: SpecStatus) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE specs SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status.as_str(), now, id],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::SpecNotFound(id.to_string()));
        }
        Ok(())
    }

    /// Remove a spec by ID.
    ///
    /// # Errors
    /// Returns `BeltError::SpecNotFound` if no row was deleted.
    pub fn remove_spec(&self, id: &str) -> Result<(), BeltError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        let rows = conn
            .execute("DELETE FROM specs WHERE id = ?1", params![id])
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::SpecNotFound(id.to_string()));
        }
        Ok(())
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

/// Parse a database status string back into a `SpecStatus`.
///
/// # Errors
/// Returns `BeltError::Database` for unrecognised status values.
fn str_to_spec_status(s: &str) -> Result<SpecStatus, BeltError> {
    s.parse::<SpecStatus>()
        .map_err(|_| BeltError::Database(format!("unknown spec status: {s}")))
}

/// Extract a `Spec` from a rusqlite `Row`.
///
/// Column order must match:
/// `id, workspace_id, name, status, content, priority, labels, depends_on, created_at, updated_at`
fn row_to_spec(row: &rusqlite::Row<'_>) -> Result<Spec, BeltError> {
    let status_str: String = row.get(3).map_err(|e| BeltError::Database(e.to_string()))?;

    Ok(Spec {
        id: row.get(0).map_err(|e| BeltError::Database(e.to_string()))?,
        workspace_id: row.get(1).map_err(|e| BeltError::Database(e.to_string()))?,
        name: row.get(2).map_err(|e| BeltError::Database(e.to_string()))?,
        status: str_to_spec_status(&status_str)?,
        content: row.get(4).map_err(|e| BeltError::Database(e.to_string()))?,
        priority: row.get(5).map_err(|e| BeltError::Database(e.to_string()))?,
        labels: row.get(6).map_err(|e| BeltError::Database(e.to_string()))?,
        depends_on: row.get(7).map_err(|e| BeltError::Database(e.to_string()))?,
        created_at: row.get(8).map_err(|e| BeltError::Database(e.to_string()))?,
        updated_at: row.get(9).map_err(|e| BeltError::Database(e.to_string()))?,
    })
}

/// Extract a `QueueItem` from a rusqlite `Row`.
///
/// Column order must match:
/// `work_id, source_id, workspace_id, state, phase, title, created_at, updated_at,
///  hitl_created_at, hitl_respondent, hitl_notes, hitl_reason`
fn row_to_queue_item(row: &rusqlite::Row<'_>) -> Result<QueueItem, BeltError> {
    let phase_str: String = row.get(4).map_err(|e| BeltError::Database(e.to_string()))?;
    let hitl_reason_str: Option<String> = row
        .get(11)
        .map_err(|e| BeltError::Database(e.to_string()))?;
    let hitl_reason = hitl_reason_str
        .as_deref()
        .map(|s| match s {
            "evaluate_failure" => Ok(belt_core::queue::HitlReason::EvaluateFailure),
            "retry_max_exceeded" => Ok(belt_core::queue::HitlReason::RetryMaxExceeded),
            "timeout" => Ok(belt_core::queue::HitlReason::Timeout),
            "manual_escalation" => Ok(belt_core::queue::HitlReason::ManualEscalation),
            other => Err(BeltError::Database(format!("unknown hitl_reason: {other}"))),
        })
        .transpose()?;

    Ok(QueueItem {
        work_id: row.get(0).map_err(|e| BeltError::Database(e.to_string()))?,
        source_id: row.get(1).map_err(|e| BeltError::Database(e.to_string()))?,
        workspace_id: row.get(2).map_err(|e| BeltError::Database(e.to_string()))?,
        state: row.get(3).map_err(|e| BeltError::Database(e.to_string()))?,
        phase: str_to_phase(&phase_str)?,
        title: row.get(5).map_err(|e| BeltError::Database(e.to_string()))?,
        created_at: row.get(6).map_err(|e| BeltError::Database(e.to_string()))?,
        updated_at: row.get(7).map_err(|e| BeltError::Database(e.to_string()))?,
        hitl_created_at: row.get(8).map_err(|e| BeltError::Database(e.to_string()))?,
        hitl_respondent: row.get(9).map_err(|e| BeltError::Database(e.to_string()))?,
        hitl_notes: row
            .get(10)
            .map_err(|e| BeltError::Database(e.to_string()))?,
        hitl_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        Database::open_in_memory().expect("in-memory DB should open")
    }

    fn sample_item() -> QueueItem {
        QueueItem {
            work_id: "gh:org/repo#1:implement".to_string(),
            source_id: "gh:org/repo#1".to_string(),
            workspace_id: "my-ws".to_string(),
            state: "implement".to_string(),
            phase: QueuePhase::Pending,
            title: None,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            hitl_created_at: None,
            hitl_respondent: None,
            hitl_notes: None,
            hitl_reason: None,
        }
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
        assert_eq!(fetched.phase, QueuePhase::Pending);
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
        assert_eq!(fetched.phase, QueuePhase::Ready);
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
        db.add_cron_job("sync-issues", "*/5 * * * *", Some("ws1"))
            .unwrap();

        let jobs = db.list_cron_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "sync-issues");
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
        db.add_cron_job("global-job", "0 * * * *", None).unwrap();
        let jobs = db.list_cron_jobs().unwrap();
        assert!(jobs[0].workspace.is_none());
    }

    #[test]
    fn update_cron_schedule_changes_schedule() {
        let db = test_db();
        db.add_cron_job("my-job", "*/5 * * * *", None).unwrap();
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

    // ---- Token Usage -------------------------------------------------------

    #[test]
    fn record_token_usage() {
        let db = test_db();
        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: Some(200),
            cache_write_tokens: Some(100),
            ..Default::default()
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

    // ---- Specs -------------------------------------------------------------

    fn sample_spec() -> Spec {
        Spec::new(
            "spec-1".to_string(),
            "ws-1".to_string(),
            "Test Spec".to_string(),
            "Some content".to_string(),
        )
    }

    #[test]
    fn insert_and_get_spec() {
        let db = test_db();
        let spec = sample_spec();
        db.insert_spec(&spec).unwrap();

        let fetched = db.get_spec(&spec.id).unwrap();
        assert_eq!(fetched.id, spec.id);
        assert_eq!(fetched.name, "Test Spec");
        assert_eq!(fetched.status, SpecStatus::Draft);
        assert_eq!(fetched.content, "Some content");
    }

    #[test]
    fn get_spec_not_found() {
        let db = test_db();
        let err = db.get_spec("nonexistent").unwrap_err();
        assert!(matches!(err, BeltError::SpecNotFound(_)));
    }

    #[test]
    fn list_specs_no_filter() {
        let db = test_db();
        db.insert_spec(&sample_spec()).unwrap();

        let specs = db.list_specs(None, None).unwrap();
        assert_eq!(specs.len(), 1);
    }

    #[test]
    fn list_specs_filter_by_workspace() {
        let db = test_db();
        db.insert_spec(&sample_spec()).unwrap();

        let specs = db.list_specs(Some("ws-1"), None).unwrap();
        assert_eq!(specs.len(), 1);

        let specs = db.list_specs(Some("other"), None).unwrap();
        assert!(specs.is_empty());
    }

    #[test]
    fn list_specs_filter_by_status() {
        let db = test_db();
        db.insert_spec(&sample_spec()).unwrap();

        let specs = db.list_specs(None, Some(SpecStatus::Draft)).unwrap();
        assert_eq!(specs.len(), 1);

        let specs = db.list_specs(None, Some(SpecStatus::Active)).unwrap();
        assert!(specs.is_empty());
    }

    #[test]
    fn update_spec_fields() {
        let db = test_db();
        let mut spec = sample_spec();
        db.insert_spec(&spec).unwrap();

        spec.name = "Updated Name".to_string();
        spec.content = "Updated content".to_string();
        spec.priority = Some(1);
        spec.labels = Some("urgent".to_string());
        db.update_spec(&spec).unwrap();

        let fetched = db.get_spec(&spec.id).unwrap();
        assert_eq!(fetched.name, "Updated Name");
        assert_eq!(fetched.content, "Updated content");
        assert_eq!(fetched.priority, Some(1));
        assert_eq!(fetched.labels.as_deref(), Some("urgent"));
    }

    #[test]
    fn update_spec_not_found() {
        let db = test_db();
        let spec = sample_spec();
        let err = db.update_spec(&spec).unwrap_err();
        assert!(matches!(err, BeltError::SpecNotFound(_)));
    }

    #[test]
    fn update_spec_status() {
        let db = test_db();
        let spec = sample_spec();
        db.insert_spec(&spec).unwrap();

        db.update_spec_status(&spec.id, SpecStatus::Active).unwrap();
        let fetched = db.get_spec(&spec.id).unwrap();
        assert_eq!(fetched.status, SpecStatus::Active);
    }

    #[test]
    fn update_spec_status_not_found() {
        let db = test_db();
        let err = db
            .update_spec_status("nonexistent", SpecStatus::Active)
            .unwrap_err();
        assert!(matches!(err, BeltError::SpecNotFound(_)));
    }

    #[test]
    fn remove_spec() {
        let db = test_db();
        let spec = sample_spec();
        db.insert_spec(&spec).unwrap();

        db.remove_spec(&spec.id).unwrap();
        assert!(db.get_spec(&spec.id).is_err());
    }

    #[test]
    fn remove_spec_not_found() {
        let db = test_db();
        let err = db.remove_spec("nonexistent").unwrap_err();
        assert!(matches!(err, BeltError::SpecNotFound(_)));
    }

    #[test]
    fn spec_with_optional_fields() {
        let db = test_db();
        let mut spec = sample_spec();
        spec.priority = Some(5);
        spec.labels = Some("bug,feature".to_string());
        spec.depends_on = Some("spec-0".to_string());
        db.insert_spec(&spec).unwrap();

        let fetched = db.get_spec(&spec.id).unwrap();
        assert_eq!(fetched.priority, Some(5));
        assert_eq!(fetched.labels.as_deref(), Some("bug,feature"));
        assert_eq!(fetched.depends_on.as_deref(), Some("spec-0"));
    }

    #[test]
    fn spec_status_roundtrip() {
        let statuses = [
            SpecStatus::Draft,
            SpecStatus::Active,
            SpecStatus::Paused,
            SpecStatus::Completed,
        ];
        for s in statuses {
            assert_eq!(str_to_spec_status(s.as_str()).unwrap(), s);
        }
    }

    // ---- HITL metadata --------------------------------------------------------

    #[test]
    fn insert_and_get_item_with_hitl_metadata() {
        let db = test_db();
        let mut item = sample_item();
        item.phase = QueuePhase::Hitl;
        item.hitl_created_at = Some(Utc::now().to_rfc3339());
        item.hitl_reason = Some(belt_core::queue::HitlReason::RetryMaxExceeded);
        item.hitl_notes = Some("max retries".to_string());
        db.insert_item(&item).unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(fetched.phase, QueuePhase::Hitl);
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
        item.phase = QueuePhase::Hitl;
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
        assert_eq!(fetched.phase, QueuePhase::Done);
        assert_eq!(fetched.hitl_respondent.as_deref(), Some("irene"));
        assert_eq!(fetched.hitl_notes.as_deref(), Some("looks good"));
    }

    #[test]
    fn list_expired_hitl_items_returns_old_items() {
        let db = test_db();
        // Item with hitl_created_at 25 hours ago
        let mut old_item = sample_item();
        old_item.phase = QueuePhase::Hitl;
        old_item.hitl_created_at = Some((Utc::now() - chrono::Duration::hours(25)).to_rfc3339());
        db.insert_item(&old_item).unwrap();
        db.update_phase(&old_item.work_id, QueuePhase::Hitl)
            .unwrap();

        // Item with hitl_created_at 1 hour ago (not expired)
        let mut new_item = QueueItem {
            work_id: "gh:org/repo#2:implement".to_string(),
            ..sample_item()
        };
        new_item.phase = QueuePhase::Hitl;
        new_item.hitl_created_at = Some((Utc::now() - chrono::Duration::hours(1)).to_rfc3339());
        db.insert_item(&new_item).unwrap();
        db.update_phase(&new_item.work_id, QueuePhase::Hitl)
            .unwrap();

        let expired = db.list_expired_hitl_items(24).unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0], old_item.work_id);
    }
}
