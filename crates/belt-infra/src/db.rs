//! SQLite persistence layer for Belt.
//!
//! Provides CRUD operations for queue items, history events, workspaces,
//! cron jobs, and token usage — all backed by a single SQLite database.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use belt_core::error::BeltError;
use belt_core::phase::QueuePhase;
use belt_core::queue::QueueItem;
use belt_core::runtime::TokenUsage;

/// History event for database persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEvent {
    pub work_id: String,
    pub source_id: String,
    pub state: String,
    pub status: String,
    pub attempt: u32,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
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
    pub last_run_at: Option<DateTime<Utc>>,
    /// When this job was created.
    pub created_at: DateTime<Utc>,
}

/// SQLite-backed persistence for Belt state.
pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open (or create) a database at the given path and initialize the schema.
    ///
    /// # Errors
    /// Returns `BeltError::Database` if the connection or schema creation fails.
    pub fn open(path: &str) -> Result<Self, BeltError> {
        let conn = Connection::open(path).map_err(|e| BeltError::Database(e.to_string()))?;
        let db = Self { conn };
        db.init()?;
        Ok(db)
    }

    /// Open an in-memory database — useful for testing.
    ///
    /// # Errors
    /// Returns `BeltError::Database` if schema creation fails.
    pub fn open_in_memory() -> Result<Self, BeltError> {
        let conn = Connection::open_in_memory().map_err(|e| BeltError::Database(e.to_string()))?;
        let db = Self { conn };
        db.init()?;
        Ok(db)
    }

    /// Create all tables if they do not already exist.
    fn init(&self) -> Result<(), BeltError> {
        self.conn
            .execute_batch(
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
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                work_id       TEXT NOT NULL,
                workspace     TEXT NOT NULL,
                runtime       TEXT NOT NULL,
                model         TEXT NOT NULL,
                input_tokens  INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                created_at    TEXT NOT NULL
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
        self.conn
            .execute(
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
        let rows = self
            .conn
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
        self.conn
            .query_row(
                "SELECT work_id, source_id, workspace_id, state, phase, title, created_at, updated_at
                 FROM queue_items WHERE work_id = ?1",
                params![work_id],
                |row| Ok(row_to_queue_item(row)),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    BeltError::ItemNotFound(work_id.to_string())
                }
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

        let mut stmt = self
            .conn
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
        self.conn
            .execute(
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
                    event.created_at.to_rfc3339(),
                ],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(())
    }

    /// Get all history events for a given `source_id`, ordered by creation time.
    pub fn get_history(&self, source_id: &str) -> Result<Vec<HistoryEvent>, BeltError> {
        let mut stmt = self
            .conn
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
                    created_at: parse_datetime(&row.get::<_, String>(7)?),
                })
            })
            .map_err(|e| BeltError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(events)
    }

    /// Count how many times a `source_id` has failed in a given `state`.
    pub fn count_failures(&self, source_id: &str, state: &str) -> Result<u32, BeltError> {
        let count: u32 = self
            .conn
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
        self.conn
            .execute(
                "INSERT INTO workspaces (name, config_path, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![name, config_path, now, now],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(())
    }

    /// List all registered workspaces as `(name, config_path, created_at)` tuples.
    pub fn list_workspaces(&self) -> Result<Vec<(String, String, DateTime<Utc>)>, BeltError> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, config_path, created_at FROM workspaces ORDER BY name")
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                let name: String = row.get(0)?;
                let config_path: String = row.get(1)?;
                let created_at: String = row.get(2)?;
                Ok((name, config_path, parse_datetime(&created_at)))
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
    pub fn get_workspace(&self, name: &str) -> Result<(String, String, DateTime<Utc>), BeltError> {
        self.conn
            .query_row(
                "SELECT name, config_path, created_at FROM workspaces WHERE name = ?1",
                params![name],
                |row| {
                    let n: String = row.get(0)?;
                    let cp: String = row.get(1)?;
                    let ca: String = row.get(2)?;
                    Ok((n, cp, parse_datetime(&ca)))
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    BeltError::WorkspaceNotFound(name.to_string())
                }
                other => BeltError::Database(other.to_string()),
            })
    }

    /// Remove a workspace by name.
    ///
    /// # Errors
    /// Returns `BeltError::WorkspaceNotFound` if no row was deleted.
    pub fn remove_workspace(&self, name: &str) -> Result<(), BeltError> {
        let rows = self
            .conn
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
        self.conn
            .execute(
                "INSERT INTO cron_jobs (name, schedule, workspace, enabled, created_at)
                 VALUES (?1, ?2, ?3, 1, ?4)",
                params![name, schedule, workspace, now],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(())
    }

    /// List all cron jobs.
    pub fn list_cron_jobs(&self) -> Result<Vec<CronJob>, BeltError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT name, schedule, workspace, enabled, last_run_at, created_at
                 FROM cron_jobs ORDER BY name",
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;

        let jobs = stmt
            .query_map([], |row| {
                let enabled_int: i32 = row.get(3)?;
                let last_run: Option<String> = row.get(4)?;
                let created: String = row.get(5)?;
                Ok(CronJob {
                    name: row.get(0)?,
                    schedule: row.get(1)?,
                    workspace: row.get(2)?,
                    enabled: enabled_int != 0,
                    last_run_at: last_run.as_deref().map(parse_datetime),
                    created_at: parse_datetime(&created),
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
        let rows = self
            .conn
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

    /// Enable or disable a cron job.
    pub fn toggle_cron_job(&self, name: &str, enabled: bool) -> Result<(), BeltError> {
        let rows = self
            .conn
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
        let rows = self
            .conn
            .execute("DELETE FROM cron_jobs WHERE name = ?1", params![name])
            .map_err(|e| BeltError::Database(e.to_string()))?;
        if rows == 0 {
            return Err(BeltError::ItemNotFound(name.to_string()));
        }
        Ok(())
    }

    // ---- Token Usage -------------------------------------------------------

    /// Record token usage for a completed runtime invocation.
    pub fn record_token_usage(
        &self,
        work_id: &str,
        workspace: &str,
        runtime: &str,
        model: &str,
        usage: &TokenUsage,
    ) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "INSERT INTO token_usage (work_id, workspace, runtime, model, input_tokens, output_tokens, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    work_id,
                    workspace,
                    runtime,
                    model,
                    usage.input_tokens as i64,
                    usage.output_tokens as i64,
                    now,
                ],
            )
            .map_err(|e| BeltError::Database(e.to_string()))?;
        Ok(())
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
/// Defaults to `Pending` for unrecognised values (should never happen with
/// validated data).
fn str_to_phase(s: &str) -> QueuePhase {
    match s {
        "pending" => QueuePhase::Pending,
        "ready" => QueuePhase::Ready,
        "running" => QueuePhase::Running,
        "completed" => QueuePhase::Completed,
        "done" => QueuePhase::Done,
        "hitl" => QueuePhase::Hitl,
        "failed" => QueuePhase::Failed,
        "skipped" => QueuePhase::Skipped,
        _ => QueuePhase::Pending,
    }
}

/// Parse an RFC 3339 timestamp string into `DateTime<Utc>`.
///
/// Falls back to `Utc::now()` on parse failure (should not happen with
/// well-formed data).
fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
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
        phase: str_to_phase(&phase_str),
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
            created_at: Utc::now(),
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
                created_at: Utc::now(),
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
            created_at: Utc::now(),
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

    // ---- Token Usage -------------------------------------------------------

    #[test]
    fn record_token_usage() {
        let db = test_db();
        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        db.record_token_usage("w1", "ws1", "claude", "opus-4", &usage)
            .unwrap();

        // Verify by raw query — no public read API for token_usage yet.
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM token_usage", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
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
            assert_eq!(str_to_phase(phase_to_str(p)), p);
        }
    }
}
