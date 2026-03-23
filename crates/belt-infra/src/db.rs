//! SQLite persistence layer for Belt.
//!
//! Provides CRUD operations for queue items, history events, workspaces,
//! cron jobs, and token usage — all backed by a single SQLite database.

use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use belt_core::error::BeltError;
use belt_core::phase::QueuePhase;
use belt_core::queue::QueueItem;
use belt_core::runtime::TokenUsage;

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
                work_id      TEXT PRIMARY KEY,
                source_id    TEXT NOT NULL,
                workspace_id TEXT NOT NULL,
                state        TEXT NOT NULL,
                phase        TEXT NOT NULL,
                title        TEXT,
                created_at   TEXT NOT NULL,
                updated_at   TEXT NOT NULL
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
            "INSERT INTO queue_items (work_id, source_id, workspace_id, state, phase, title, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                item.work_id,
                item.source_id,
                item.workspace_id,
                item.state,
                phase_to_str(item.phase),
                item.title,
                item.created_at,
                item.updated_at,
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
            "SELECT work_id, source_id, workspace_id, state, phase, title, created_at, updated_at
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
            "SELECT work_id, source_id, workspace_id, state, phase, title, created_at, updated_at FROM queue_items WHERE 1=1",
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
            .map(|(wid, ws, rt, model, input, output, cache_read, cache_write, dur, created)| {
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
            })
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
            .map(|(wid, ws, rt, model, input, output, cache_read, cache_write, dur, created)| {
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
            })
            .collect()
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

/// Extract a `QueueItem` from a rusqlite `Row`.
///
/// Column order must match:
/// `work_id, source_id, workspace_id, state, phase, title, created_at, updated_at`
fn row_to_queue_item(row: &rusqlite::Row<'_>) -> Result<QueueItem, BeltError> {
    let phase_str: String = row.get(4).map_err(|e| BeltError::Database(e.to_string()))?;

    Ok(QueueItem {
        work_id: row.get(0).map_err(|e| BeltError::Database(e.to_string()))?,
        source_id: row.get(1).map_err(|e| BeltError::Database(e.to_string()))?,
        workspace_id: row.get(2).map_err(|e| BeltError::Database(e.to_string()))?,
        state: row.get(3).map_err(|e| BeltError::Database(e.to_string()))?,
        phase: str_to_phase(&phase_str)?,
        title: row.get(5).map_err(|e| BeltError::Database(e.to_string()))?,
        created_at: row.get(6).map_err(|e| BeltError::Database(e.to_string()))?,
        updated_at: row.get(7).map_err(|e| BeltError::Database(e.to_string()))?,
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
    fn database_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Database>();
    }
}
