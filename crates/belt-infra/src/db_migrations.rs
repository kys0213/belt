//! Forward-only schema migrations for the Belt SQLite database.
//!
//! The schema version lives in `PRAGMA user_version`. A database that predates
//! versioning reports `0`; if it already holds tables it is treated as v1.

use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use belt_core::error::BeltError;

/// Schema version this binary reads and writes.
pub(crate) const CURRENT_VERSION: u32 = 2;

/// How long a writer waits for another process's lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Bring the database up to [`CURRENT_VERSION`] and configure the connection.
///
/// `db_path` is the database file; when given, a database written by an older
/// binary is backed up next to it before any change. In-memory databases pass
/// `None`. All schema and data changes run in one `BEGIN IMMEDIATE`
/// transaction, so a failure leaves the database exactly as it was.
pub(crate) fn migrate(conn: &mut Connection, db_path: Option<&Path>) -> Result<(), BeltError> {
    conn.busy_timeout(BUSY_TIMEOUT).map_err(db_err)?;

    let found = detect_version(conn)?;
    if found > CURRENT_VERSION {
        return Err(BeltError::Database(format!(
            "database schema v{found} is newer than this binary supports (v{CURRENT_VERSION}); downgrade is not supported"
        )));
    }
    if found < CURRENT_VERSION {
        // An empty database (v0) has nothing to lose, so only v1+ is backed up.
        if found >= 1 {
            // A migration that is certain to fail must not leave a backup behind
            // on every start.
            v2_data::precheck(conn)?;
            if let Some(path) = db_path {
                backup(conn, path, found)?;
            }
        }
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_err)?;
        // Another process may have migrated between the check above and the lock.
        let locked = detect_version(&tx)?;
        // An unversioned database may hold only some v1 tables, so v1 also
        // runs for a detected v1 database; its statements are idempotent.
        if locked <= 1 {
            apply_v1(&tx)?;
        }
        if locked < 2 {
            apply_v2(&tx)?;
        }
        tx.pragma_update(None, "user_version", CURRENT_VERSION)
            .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
    }

    // In-memory databases answer "memory"; only file databases switch to WAL.
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))
        .map_err(db_err)?;
    Ok(())
}

/// `PRAGMA user_version`, except that an unversioned database that already has
/// tables is the v1 schema written before versioning existed.
fn detect_version(conn: &Connection) -> Result<u32, BeltError> {
    let version: u32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(db_err)?;
    if version > 0 {
        return Ok(version);
    }
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    Ok(if tables > 0 { 1 } else { 0 })
}

/// Copy the database to `{path}.bak-v{version}` (or `.bak-v{version}.{n}` when
/// that name is taken). An existing backup is never overwritten.
///
/// The name is claimed atomically by creating an empty file with
/// `create_new`, so processes starting at the same time never pick the same
/// name. `VACUUM INTO` accepts an existing empty file as its target.
fn backup(conn: &Connection, path: &Path, version: u32) -> Result<(), BeltError> {
    let base = format!("{}.bak-v{version}", path.display());
    let candidates = std::iter::once(PathBuf::from(&base))
        .chain((2..).map(|n| PathBuf::from(format!("{base}.{n}"))));
    for target in candidates {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(_) => return fill_backup(conn, &target, version),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(BeltError::Database(format!(
                    "cannot create backup file {}: {e}",
                    target.display()
                )));
            }
        }
    }
    unreachable!("an unbounded candidate sequence always has a free name")
}

/// `VACUUM INTO` the claimed empty file; on failure the empty claim is removed.
fn fill_backup(conn: &Connection, target: &Path, version: u32) -> Result<(), BeltError> {
    let vacuum = target
        .to_str()
        .ok_or_else(|| {
            BeltError::Database(format!("backup path is not UTF-8: {}", target.display()))
        })
        .and_then(|target_str| {
            // VACUUM INTO reads through the connection, so pages still in a WAL file are included.
            conn.execute("VACUUM INTO ?1", params![target_str])
                .map_err(|e| BeltError::Database(format!("backup to {target_str} failed: {e}")))
        });
    if let Err(e) = vacuum {
        if let Err(remove) = std::fs::remove_file(target) {
            tracing::warn!(backup = %target.display(), error = %remove, "could not remove the unfinished backup file");
        }
        return Err(e);
    }
    tracing::info!(backup = %target.display(), from_version = version, "database backed up before migration");
    Ok(())
}

fn db_err(e: rusqlite::Error) -> BeltError {
    BeltError::Database(e.to_string())
}

/// The v1 schema: every table as it existed before versioning was introduced.
///
/// Frozen. A legacy database may already hold any subset of these tables,
/// and a table created by an older release may lack later v1 columns
/// (v0.1.0 through v0.1.6 have no `queue_items.previous_worktree_path`).
/// `CREATE TABLE IF NOT EXISTS` does not add those, so they are added here.
fn apply_v1(conn: &Connection) -> Result<(), BeltError> {
    conn.execute_batch(V1_SCHEMA).map_err(db_err)?;
    add_missing_v1_columns(conn)
}

/// A column as `pragma_table_info` describes it.
struct ColumnDef {
    name: String,
    decl_type: String,
    not_null: bool,
    default: Option<String>,
    primary_key: bool,
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<ColumnDef>, BeltError> {
    let mut stmt = conn
        .prepare("SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1)")
        .map_err(db_err)?;
    stmt.query_map(params![table], |r| {
        Ok(ColumnDef {
            name: r.get(0)?,
            decl_type: r.get(1)?,
            not_null: r.get(2)?,
            default: r.get(3)?,
            primary_key: r.get::<_, i64>(4)? > 0,
        })
    })
    .map_err(db_err)?
    .collect::<Result<_, _>>()
    .map_err(db_err)
}

/// Add every v1 column a v1 table lacks, with its v1 type, nullability and
/// default. The v1 definitions are read from [`V1_SCHEMA`] applied to a
/// scratch in-memory database, so they cannot drift from the frozen schema.
fn add_missing_v1_columns(conn: &Connection) -> Result<(), BeltError> {
    let reference = Connection::open_in_memory().map_err(db_err)?;
    reference.execute_batch(V1_SCHEMA).map_err(db_err)?;
    let tables: Vec<String> = {
        let mut stmt = reference
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .map_err(db_err)?;
        stmt.query_map([], |r| r.get(0))
            .map_err(db_err)?
            .collect::<Result<_, _>>()
            .map_err(db_err)?
    };
    for table in tables {
        let present: Vec<String> = table_columns(conn, &table)?
            .into_iter()
            .map(|c| c.name)
            .collect();
        for column in table_columns(&reference, &table)? {
            if present.contains(&column.name) {
                continue;
            }
            if column.primary_key {
                return Err(BeltError::Database(format!(
                    "migration v1: table {table} lacks its primary key column {}",
                    column.name
                )));
            }
            let mut ddl = format!(
                "ALTER TABLE {table} ADD COLUMN {} {}",
                column.name, column.decl_type
            );
            if column.not_null {
                ddl.push_str(" NOT NULL");
            }
            if let Some(default) = &column.default {
                ddl.push_str(&format!(" DEFAULT {default}"));
            }
            conn.execute_batch(&ddl)
                .map_err(|e| BeltError::Database(format!("migration v1: `{ddl}` failed: {e}")))?;
            tracing::info!(table = %table, column = %column.name, "added missing v1 column");
        }
    }
    Ok(())
}

/// v2: SQLite owns queue state. Adds lineage and handler columns, the
/// transition log, HITL requests and their delivery, response, proposal and
/// cancel records, then converts legacy rows. Nothing is dropped.
fn apply_v2(tx: &Transaction<'_>) -> Result<(), BeltError> {
    tx.execute_batch(V2_SCHEMA).map_err(db_err)?;
    v2_data::convert(tx)
}

/// Legacy data conversion for v2. The value lists are frozen copies of what
/// v1 binaries wrote; they must not follow later changes to core enums.
mod v2_data {
    use chrono::Utc;
    use rusqlite::{Connection, Transaction, params};

    use belt_core::error::BeltError;

    use super::db_err;

    const ACTOR_LEGACY: &str = "legacy";
    const KIND_PHASE: &str = "phase_enter";

    const V1_PHASES: &[&str] = &[
        "pending",
        "ready",
        "running",
        "completed",
        "done",
        "hitl",
        "failed",
        "skipped",
    ];
    const V1_REASONS: &[&str] = &[
        "evaluate_failure",
        "retry_max_exceeded",
        "timeout",
        "manual_escalation",
        "stagnation_detected",
        "spec_conflict",
        "spec_completion_review",
        "spec_modification_proposed",
    ];
    /// Spec-lifecycle reasons whose HITL items have no meaningful response left.
    const CLOSED_SPEC_REASONS: &[&str] = &["spec_completion_review", "spec_modification_proposed"];
    /// Every spec-lifecycle reason; rewritten to `manual_escalation`.
    const SPEC_REASONS_SQL: &str =
        "('spec_conflict', 'spec_completion_review', 'spec_modification_proposed')";

    /// SQL expression appending `suffix` to `hitl_notes` on its own line.
    fn append_note(suffix: &str) -> String {
        format!(
            "CASE WHEN hitl_notes IS NULL OR hitl_notes = '' THEN {suffix} ELSE hitl_notes || char(10) || {suffix} END"
        )
    }

    pub(super) fn convert(tx: &Transaction<'_>) -> Result<(), BeltError> {
        let now = Utc::now().to_rfc3339();
        reject_unknown_values(tx)?;
        copy_transition_events(tx)?;
        start_lineages(tx)?;
        clear_unsupported_terminal_actions(tx)?;
        skip_closed_spec_hitl(tx, &now)?;
        rename_spec_reasons(tx)?;
        open_requests_for_hitl_items(tx)
    }

    /// Read-only check, run before the backup, for data the conversion is
    /// certain to reject. [`convert`] repeats it under the write lock.
    pub(super) fn precheck(conn: &Connection) -> Result<(), BeltError> {
        let has_queue: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'queue_items')",
                [],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if has_queue {
            reject_unknown_values(conn)?;
        }
        Ok(())
    }

    /// Fail Fast on values no v1 binary wrote: they cannot be mapped safely.
    fn reject_unknown_values(conn: &Connection) -> Result<(), BeltError> {
        let mut stmt = conn
            .prepare("SELECT work_id, phase, hitl_reason FROM queue_items ORDER BY work_id")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(db_err)?;
        for row in rows {
            let (work_id, phase, reason) = row.map_err(db_err)?;
            if !V1_PHASES.contains(&phase.as_str()) {
                return Err(BeltError::Database(format!(
                    "migration v2: queue item {work_id} has unknown phase '{phase}'"
                )));
            }
            if let Some(reason) = reason
                && !V1_REASONS.contains(&reason.as_str())
            {
                return Err(BeltError::Database(format!(
                    "migration v2: queue item {work_id} has unknown hitl_reason '{reason}'"
                )));
            }
        }
        Ok(())
    }

    /// `transition_events` ids carry no order, so the global order is `(created_at, id)`.
    fn copy_transition_events(tx: &Transaction<'_>) -> Result<(), BeltError> {
        let mut select = tx
            .prepare(
                "SELECT id, work_id, source_id, event_type, from_phase, phase, detail, created_at
                 FROM transition_events ORDER BY created_at, id",
            )
            .map_err(db_err)?;
        let mut insert = tx
            .prepare(
                "INSERT INTO transition_log
                 (legacy_event_id, work_id, source_id, kind, from_phase, to_phase, detail, created_at, actor)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .map_err(db_err)?;
        let mut rows = select.query([]).map_err(db_err)?;
        while let Some(r) = rows.next().map_err(db_err)? {
            insert
                .execute(params![
                    r.get::<_, String>(0).map_err(db_err)?,
                    r.get::<_, String>(1).map_err(db_err)?,
                    r.get::<_, String>(2).map_err(db_err)?,
                    r.get::<_, String>(3).map_err(db_err)?,
                    r.get::<_, Option<String>>(4).map_err(db_err)?,
                    r.get::<_, Option<String>>(5).map_err(db_err)?,
                    r.get::<_, Option<String>>(6).map_err(db_err)?,
                    r.get::<_, String>(7).map_err(db_err)?,
                    ACTOR_LEGACY,
                ])
                .map_err(db_err)?;
        }
        Ok(())
    }

    /// Every existing item is the first item of its own lineage.
    fn start_lineages(tx: &Transaction<'_>) -> Result<(), BeltError> {
        tx.execute(
            "UPDATE queue_items SET lineage_root = work_id, derived_from = NULL",
            [],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Per-item terminal actions may only be `skip` or `replan`; anything else
    /// falls back to the workspace terminal, with the old value kept in notes.
    fn clear_unsupported_terminal_actions(tx: &Transaction<'_>) -> Result<(), BeltError> {
        let note = append_note("'[legacy terminal: ' || hitl_terminal_action || ']'");
        tx.execute(
            &format!(
                "UPDATE queue_items SET hitl_notes = {note}, hitl_terminal_action = NULL
                 WHERE hitl_terminal_action IS NOT NULL AND hitl_terminal_action NOT IN ('skip', 'replan')"
            ),
            [],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Spec completion and spec modification HITL items are closed as Skipped
    /// without running hooks; no response to them means anything any more.
    fn skip_closed_spec_hitl(tx: &Transaction<'_>, now: &str) -> Result<(), BeltError> {
        let mut select = tx
            .prepare(
                "SELECT work_id, source_id FROM queue_items
                 WHERE phase = 'hitl' AND hitl_reason = ?1 ORDER BY work_id",
            )
            .map_err(db_err)?;
        for reason in CLOSED_SPEC_REASONS {
            let items = select
                .query_map(params![reason], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_err)?;
            for (work_id, source_id) in items {
                tx.execute(
                    "UPDATE queue_items SET phase = 'skipped', updated_at = ?1 WHERE work_id = ?2",
                    params![now, work_id],
                )
                .map_err(db_err)?;
                tx.execute(
                    "INSERT INTO transition_log
                     (work_id, source_id, kind, from_phase, to_phase, actor, reason, detail, created_at)
                     VALUES (?1, ?2, ?3, 'hitl', 'skipped', ?4, ?4, ?5, ?6)",
                    params![work_id, source_id, KIND_PHASE, ACTOR_LEGACY, reason, now],
                )
                .map_err(db_err)?;
            }
        }
        Ok(())
    }

    fn rename_spec_reasons(tx: &Transaction<'_>) -> Result<(), BeltError> {
        let note = append_note("'[legacy reason: ' || hitl_reason || ']'");
        tx.execute(
            &format!(
                "UPDATE queue_items SET hitl_notes = {note}, hitl_reason = 'manual_escalation'
                 WHERE hitl_reason IN {SPEC_REASONS_SQL}"
            ),
            [],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Each item still in Hitl gets one open request carrying its HITL columns.
    ///
    /// Legacy ids use their own `hitl-legacy-` prefix so they cannot collide
    /// with ids issued later. A row without `hitl_created_at` (written without
    /// escalation metadata) opens its request at the item's `updated_at`.
    fn open_requests_for_hitl_items(tx: &Transaction<'_>) -> Result<(), BeltError> {
        let mut select = tx
            .prepare(
                "SELECT work_id, COALESCE(hitl_created_at, updated_at), hitl_reason, hitl_notes,
                        hitl_timeout_at, hitl_terminal_action
                 FROM queue_items WHERE phase = 'hitl'
                 ORDER BY COALESCE(hitl_created_at, updated_at), work_id",
            )
            .map_err(db_err)?;
        let mut insert = tx
            .prepare(
                "INSERT INTO hitl_requests
                 (hitl_id, work_id, opened_at, reason, notes, timeout_at, terminal_action, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'open')",
            )
            .map_err(db_err)?;
        let mut rows = select.query([]).map_err(db_err)?;
        let mut n = 0u64;
        while let Some(r) = rows.next().map_err(db_err)? {
            n += 1;
            insert
                .execute(params![
                    format!("hitl-legacy-{n}"),
                    r.get::<_, String>(0).map_err(db_err)?,
                    r.get::<_, String>(1).map_err(db_err)?,
                    r.get::<_, Option<String>>(2).map_err(db_err)?,
                    r.get::<_, Option<String>>(3).map_err(db_err)?,
                    r.get::<_, Option<String>>(4).map_err(db_err)?,
                    r.get::<_, Option<String>>(5).map_err(db_err)?,
                ])
                .map_err(db_err)?;
        }
        Ok(())
    }
}

const V1_SCHEMA: &str = "
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
        hitl_reason          TEXT,
        hitl_timeout_at      TEXT,
        hitl_terminal_action TEXT,
        replan_count         INTEGER NOT NULL DEFAULT 0,
        worktree_preserved   INTEGER NOT NULL DEFAULT 0,
        previous_worktree_path TEXT
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
        script      TEXT NOT NULL DEFAULT '',
        workspace   TEXT,
        enabled     INTEGER NOT NULL DEFAULT 1,
        last_run_at TEXT,
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL DEFAULT ''
    );

    CREATE TABLE IF NOT EXISTS knowledge_base (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        workspace  TEXT NOT NULL,
        source_ref TEXT NOT NULL,
        category   TEXT NOT NULL,
        content    TEXT NOT NULL,
        created_at TEXT NOT NULL
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

    CREATE TABLE IF NOT EXISTS transition_events (
        id         TEXT PRIMARY KEY,
        work_id    TEXT NOT NULL,
        source_id  TEXT NOT NULL,
        event_type TEXT NOT NULL,
        phase      TEXT,
        from_phase TEXT,
        detail     TEXT,
        created_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS queue_dependencies (
        work_id    TEXT NOT NULL,
        depends_on TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (work_id, depends_on)
    );
";

const V2_SCHEMA: &str = "
    ALTER TABLE queue_items ADD COLUMN derived_from TEXT;
    ALTER TABLE queue_items ADD COLUMN lineage_root TEXT NOT NULL DEFAULT '';
    ALTER TABLE queue_items ADD COLUMN handler_pid INTEGER;
    ALTER TABLE queue_items ADD COLUMN worktree_owner TEXT;
    CREATE INDEX idx_queue_items_source_state ON queue_items (source_id, state);

    CREATE TABLE transition_log (
        seq             INTEGER PRIMARY KEY AUTOINCREMENT,
        work_id         TEXT NOT NULL,
        source_id       TEXT NOT NULL,
        kind            TEXT NOT NULL,
        from_phase      TEXT,
        to_phase        TEXT,
        actor           TEXT NOT NULL,
        reason          TEXT,
        detail          TEXT,
        created_at      TEXT NOT NULL,
        legacy_event_id TEXT
    );
    CREATE INDEX idx_transition_log_work_id ON transition_log (work_id);
    -- Work-id issuance reads every id a (source_id, state) series ever had under the write lock.
    CREATE INDEX idx_transition_log_source ON transition_log (source_id, work_id);
    CREATE INDEX idx_queue_items_lineage_root ON queue_items (lineage_root);
    CREATE INDEX idx_history_work_id ON history (work_id);

    CREATE TABLE hitl_requests (
        hitl_id                  TEXT PRIMARY KEY,
        work_id                  TEXT NOT NULL,
        status                   TEXT NOT NULL CHECK (status IN ('open', 'resolved', 'expired')),
        reason                   TEXT,
        notes                    TEXT,
        opened_at                TEXT NOT NULL,
        timeout_at               TEXT,
        terminal_action          TEXT,
        opened_hook_done_at      TEXT,
        action                   TEXT,
        respondent               TEXT,
        via                      TEXT,
        confirm_path             TEXT,
        resolved_at              TEXT,
        resolution_notes         TEXT,
        post_processed_at        TEXT,
        post_processing_failures INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX idx_hitl_requests_work_id ON hitl_requests (work_id);
    CREATE UNIQUE INDEX idx_hitl_requests_one_open ON hitl_requests (work_id) WHERE status = 'open';

    CREATE TABLE hitl_deliveries (
        hitl_id     TEXT NOT NULL,
        channel     TEXT NOT NULL,
        status      TEXT NOT NULL CHECK (status IN ('pending', 'sent', 'failed')),
        attempts    INTEGER NOT NULL DEFAULT 0,
        message_ref TEXT,
        last_error  TEXT,
        updated_at  TEXT NOT NULL,
        PRIMARY KEY (hitl_id, channel)
    );

    CREATE TABLE external_responses (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        channel     TEXT NOT NULL,
        external_id TEXT NOT NULL,
        hitl_id     TEXT,
        respondent  TEXT,
        received_at TEXT NOT NULL,
        UNIQUE (channel, external_id)
    );

    CREATE TABLE nl_proposals (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        hitl_id    TEXT NOT NULL,
        respondent TEXT NOT NULL,
        channel    TEXT NOT NULL,
        action     TEXT NOT NULL,
        summary    TEXT,
        status     TEXT NOT NULL CHECK (status IN ('pending', 'confirmed', 'superseded')),
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE UNIQUE INDEX idx_nl_proposals_one_pending ON nl_proposals (hitl_id, respondent) WHERE status = 'pending';

    CREATE TABLE cancel_requests (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        work_id      TEXT NOT NULL,
        requester    TEXT NOT NULL,
        via          TEXT NOT NULL,
        status       TEXT NOT NULL CHECK (status IN ('requested', 'accepted', 'closed')),
        result       TEXT CHECK (result IS NULL OR result IN ('canceled', 'canceled_directly', 'too_late')),
        requested_at TEXT NOT NULL,
        accepted_at  TEXT,
        closed_at    TEXT
    );
    CREATE UNIQUE INDEX idx_cancel_requests_one_open ON cancel_requests (work_id) WHERE status IN ('requested', 'accepted');
";

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use belt_core::phase::QueuePhase;
    use belt_core::queue::HitlReason;
    use rusqlite::{Connection, params};

    use super::{CURRENT_VERSION, apply_v1};
    use crate::db::Database;

    // ---- helpers ----------------------------------------------------------

    fn user_version(conn: &Connection) -> u32 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    }

    fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        names.iter().any(|n| n == column)
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// Row shape of a legacy `queue_items` row in the fixture.
    struct LegacyItem<'a> {
        work_id: &'a str,
        phase: &'a str,
        reason: Option<&'a str>,
        notes: Option<&'a str>,
        terminal: Option<&'a str>,
    }

    fn insert_legacy(conn: &Connection, item: &LegacyItem<'_>) {
        conn.execute(
            "INSERT INTO queue_items (work_id, source_id, workspace_id, state, phase, created_at, updated_at, hitl_created_at, hitl_notes, hitl_reason, hitl_timeout_at, hitl_terminal_action)
             VALUES (?1, 'gh:org/repo#1', 'ws', 'implement', ?2, '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z', ?3, ?4, ?5, ?6, ?7)",
            params![
                item.work_id,
                item.phase,
                (item.phase == "hitl").then_some("2026-01-03T00:00:00Z"),
                item.notes,
                item.reason,
                (item.phase == "hitl").then_some("2026-01-04T00:00:00Z"),
                item.terminal,
            ],
        )
        .unwrap();
    }

    const LEGACY_ITEMS: &[LegacyItem<'static>] = &[
        LegacyItem {
            work_id: "plain",
            phase: "pending",
            reason: None,
            notes: None,
            terminal: None,
        },
        LegacyItem {
            work_id: "hitl-eval",
            phase: "hitl",
            reason: Some("evaluate_failure"),
            notes: Some("needs review"),
            terminal: Some("skip"),
        },
        LegacyItem {
            work_id: "hitl-null",
            phase: "hitl",
            reason: None,
            notes: None,
            terminal: None,
        },
        LegacyItem {
            work_id: "hitl-conflict",
            phase: "hitl",
            reason: Some("spec_conflict"),
            notes: Some("overlap"),
            terminal: None,
        },
        LegacyItem {
            work_id: "spec-completion:sp1:hitl",
            phase: "hitl",
            reason: Some("spec_completion_review"),
            notes: None,
            terminal: None,
        },
        LegacyItem {
            work_id: "gh:org/repo#1:implement:replan-1",
            phase: "hitl",
            reason: Some("spec_modification_proposed"),
            notes: Some("proposal"),
            terminal: None,
        },
        LegacyItem {
            work_id: "done-conflict",
            phase: "done",
            reason: Some("spec_conflict"),
            notes: None,
            terminal: None,
        },
        LegacyItem {
            work_id: "hitl-retry",
            phase: "hitl",
            reason: Some("retry_max_exceeded"),
            notes: None,
            terminal: Some("retry"),
        },
    ];

    /// Build a pre-versioning database with the v1 schema and one row per legacy case.
    fn legacy_db(dir: &Path) -> PathBuf {
        let path = dir.join("belt.db");
        let conn = Connection::open(&path).unwrap();
        apply_v1(&conn).unwrap();
        for item in LEGACY_ITEMS {
            insert_legacy(&conn, item);
        }
        // Inserted out of order on purpose: the copy must follow (created_at, id).
        conn.execute_batch(
            "INSERT INTO transition_events VALUES ('e2', 'plain', 'gh:org/repo#1', 'phase_enter', 'ready', 'pending', NULL, '2026-01-02T00:00:00Z');
             INSERT INTO transition_events VALUES ('e1', 'plain', 'gh:org/repo#1', 'phase_enter', 'pending', NULL, 'created', '2026-01-01T00:00:00Z');
             INSERT INTO transition_events VALUES ('e3', 'plain', 'gh:org/repo#1', 'handler', NULL, NULL, 'exit 0', '2026-01-02T00:00:00Z');",
        )
        .unwrap();
        assert_eq!(user_version(&conn), 0);
        path
    }

    fn open(path: &Path) -> Database {
        Database::open(path.to_str().unwrap()).unwrap()
    }

    fn raw(path: &Path) -> Connection {
        Connection::open(path).unwrap()
    }

    fn item_field(conn: &Connection, work_id: &str, column: &str) -> Option<String> {
        conn.query_row(
            &format!("SELECT {column} FROM queue_items WHERE work_id = ?1"),
            params![work_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn requests_for(conn: &Connection, work_id: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM hitl_requests WHERE work_id = ?1",
            params![work_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    const V2_TABLES: &[&str] = &[
        "transition_log",
        "hitl_requests",
        "hitl_deliveries",
        "external_responses",
        "nl_proposals",
        "cancel_requests",
    ];

    const V2_QUEUE_COLUMNS: &[&str] = &[
        "derived_from",
        "lineage_root",
        "handler_pid",
        "worktree_owner",
    ];

    // ---- empty database ---------------------------------------------------

    #[test]
    fn empty_database_is_created_at_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("belt.db");
        drop(open(&path));

        let conn = raw(&path);
        assert_eq!(user_version(&conn), CURRENT_VERSION);
        for table in V2_TABLES {
            assert!(table_exists(&conn, table), "missing table {table}");
        }
        for column in V2_QUEUE_COLUMNS {
            assert!(
                column_exists(&conn, "queue_items", column),
                "missing column {column}"
            );
        }
        assert!(!table_exists(&conn, "specs"));
    }

    #[test]
    fn empty_database_is_not_backed_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("belt.db");
        drop(open(&path));

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.contains(".bak-"))
            .collect();
        assert!(entries.is_empty(), "unexpected backups: {entries:?}");
    }

    #[test]
    fn in_memory_database_works_at_current_version() {
        let db = Database::open_in_memory().unwrap();
        let mut item = belt_core::queue::testing::test_item("s1", "analyze");
        item.derived_from = Some("s1:analyze:0".to_string());
        item.lineage_root = "s1:analyze:root".to_string();
        db.insert_item(&item).unwrap();

        let fetched = db.get_item(&item.work_id).unwrap();
        assert_eq!(fetched.derived_from.as_deref(), Some("s1:analyze:0"));
        assert_eq!(fetched.lineage_root, "s1:analyze:root");
    }

    // ---- legacy (v1) database ---------------------------------------------

    #[test]
    fn legacy_database_is_migrated_to_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        drop(open(&path));

        let conn = raw(&path);
        assert_eq!(user_version(&conn), CURRENT_VERSION);
        for table in V2_TABLES {
            assert!(table_exists(&conn, table), "missing table {table}");
        }
        for column in V2_QUEUE_COLUMNS {
            assert!(
                column_exists(&conn, "queue_items", column),
                "missing column {column}"
            );
        }
        // Legacy tables are kept.
        assert!(table_exists(&conn, "transition_events"));
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM transition_events"), 3);
    }

    #[test]
    fn legacy_database_is_backed_up_before_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        drop(open(&path));

        let backup = dir.path().join("belt.db.bak-v1");
        assert!(backup.exists(), "backup file missing");
        let conn = raw(&backup);
        assert_eq!(user_version(&conn), 0);
        assert!(!table_exists(&conn, "transition_log"));
        assert!(!column_exists(&conn, "queue_items", "lineage_root"));
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM queue_items"),
            LEGACY_ITEMS.len() as i64
        );
        assert_eq!(
            item_field(&conn, "hitl-conflict", "hitl_reason").as_deref(),
            Some("spec_conflict")
        );
    }

    #[test]
    fn existing_backup_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        let first = dir.path().join("belt.db.bak-v1");
        std::fs::write(&first, b"keep me").unwrap();

        drop(open(&path));

        assert_eq!(std::fs::read(&first).unwrap(), b"keep me");
        let second = dir.path().join("belt.db.bak-v1.2");
        assert!(second.exists(), "fallback backup missing");
        assert_eq!(user_version(&raw(&second)), 0);
    }

    #[test]
    fn legacy_items_become_lineage_roots() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        drop(open(&path));

        let conn = raw(&path);
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM queue_items WHERE lineage_root = work_id AND derived_from IS NULL"
            ),
            LEGACY_ITEMS.len() as i64
        );
    }

    #[test]
    fn legacy_transition_events_are_copied_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        drop(open(&path));

        let conn = raw(&path);
        let mut stmt = conn
            .prepare(
                "SELECT legacy_event_id, actor, kind, from_phase, to_phase, detail FROM transition_log
                 WHERE legacy_event_id IS NOT NULL ORDER BY seq",
            )
            .unwrap();
        type Row = (
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let rows: Vec<Row> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(ids, ["e1", "e2", "e3"]);
        assert!(rows.iter().all(|r| r.1 == "legacy"));
        assert_eq!(rows[1].2, "phase_enter");
        assert_eq!(rows[1].3.as_deref(), Some("pending"));
        assert_eq!(rows[1].4.as_deref(), Some("ready"));
        assert_eq!(rows[0].5.as_deref(), Some("created"));
    }

    #[test]
    fn legacy_hitl_item_gets_one_open_request_with_its_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        drop(open(&path));

        let conn = raw(&path);
        let field = |column: &str| -> Option<String> {
            conn.query_row(
                &format!("SELECT {column} FROM hitl_requests WHERE work_id = 'hitl-eval'"),
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(!field("hitl_id").unwrap().is_empty());
        assert_eq!(field("status").as_deref(), Some("open"));
        assert_eq!(field("reason").as_deref(), Some("evaluate_failure"));
        assert_eq!(field("notes").as_deref(), Some("needs review"));
        assert_eq!(field("opened_at").as_deref(), Some("2026-01-03T00:00:00Z"));
        assert_eq!(field("timeout_at").as_deref(), Some("2026-01-04T00:00:00Z"));
        assert_eq!(field("terminal_action").as_deref(), Some("skip"));
        assert_eq!(field("resolved_at"), None);
        assert_eq!(field("post_processed_at"), None);

        // NULL reason is still an open HITL.
        assert_eq!(requests_for(&conn, "hitl-null"), 1);
        assert_eq!(
            item_field(&conn, "hitl-null", "phase").as_deref(),
            Some("hitl")
        );
    }

    #[test]
    fn hitl_ids_are_unique() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        drop(open(&path));

        let conn = raw(&path);
        assert_eq!(
            count(&conn, "SELECT COUNT(DISTINCT hitl_id) FROM hitl_requests"),
            count(&conn, "SELECT COUNT(*) FROM hitl_requests"),
        );
        // Every Hitl item has exactly one open request, and nothing else has one.
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM hitl_requests WHERE status = 'open'"
            ),
            count(
                &conn,
                "SELECT COUNT(*) FROM queue_items WHERE phase = 'hitl'"
            ),
        );
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM hitl_requests r JOIN queue_items q ON q.work_id = r.work_id WHERE q.phase != 'hitl'"
            ),
            0
        );
    }

    #[test]
    fn spec_conflict_hitl_becomes_open_manual_escalation() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        drop(open(&path));

        let conn = raw(&path);
        assert_eq!(
            item_field(&conn, "hitl-conflict", "phase").as_deref(),
            Some("hitl")
        );
        assert_eq!(
            item_field(&conn, "hitl-conflict", "hitl_reason").as_deref(),
            Some("manual_escalation")
        );
        assert_eq!(
            item_field(&conn, "hitl-conflict", "hitl_notes").as_deref(),
            Some("overlap\n[legacy reason: spec_conflict]")
        );
        let (status, reason): (String, String) = conn
            .query_row(
                "SELECT status, reason FROM hitl_requests WHERE work_id = 'hitl-conflict'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "open");
        assert_eq!(reason, "manual_escalation");
    }

    fn assert_skipped_as_legacy(conn: &Connection, work_id: &str, original_reason: &str) {
        assert_eq!(
            item_field(conn, work_id, "phase").as_deref(),
            Some("skipped")
        );
        assert_eq!(requests_for(conn, work_id), 0, "no request for {work_id}");
        let (from, to, actor, reason, detail): (String, String, String, String, String) = conn
            .query_row(
                "SELECT from_phase, to_phase, actor, reason, detail FROM transition_log
                 WHERE work_id = ?1 AND legacy_event_id IS NULL",
                params![work_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!((from.as_str(), to.as_str()), ("hitl", "skipped"));
        assert_eq!(actor, "legacy");
        assert_eq!(reason, "legacy");
        assert_eq!(detail, original_reason);
        // The spec reason is gone from the row but kept in the notes.
        assert_eq!(
            item_field(conn, work_id, "hitl_reason").as_deref(),
            Some("manual_escalation")
        );
        let notes = item_field(conn, work_id, "hitl_notes").unwrap();
        assert!(
            notes.ends_with(&format!("[legacy reason: {original_reason}]")),
            "notes: {notes}"
        );
    }

    #[test]
    fn spec_completion_review_hitl_is_skipped_without_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        drop(open(&path));

        assert_skipped_as_legacy(
            &raw(&path),
            "spec-completion:sp1:hitl",
            "spec_completion_review",
        );
    }

    #[test]
    fn spec_modification_proposed_hitl_is_skipped_without_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        drop(open(&path));

        let conn = raw(&path);
        let work_id = "gh:org/repo#1:implement:replan-1";
        assert_skipped_as_legacy(&conn, work_id, "spec_modification_proposed");
        assert_eq!(
            item_field(&conn, work_id, "hitl_notes").as_deref(),
            Some("proposal\n[legacy reason: spec_modification_proposed]")
        );
    }

    #[test]
    fn spec_reason_on_non_hitl_row_is_renamed_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        drop(open(&path));

        let conn = raw(&path);
        assert_eq!(
            item_field(&conn, "done-conflict", "phase").as_deref(),
            Some("done")
        );
        assert_eq!(
            item_field(&conn, "done-conflict", "hitl_reason").as_deref(),
            Some("manual_escalation")
        );
        assert_eq!(
            item_field(&conn, "done-conflict", "hitl_notes").as_deref(),
            Some("[legacy reason: spec_conflict]")
        );
        assert_eq!(requests_for(&conn, "done-conflict"), 0);
    }

    #[test]
    fn unsupported_terminal_action_is_cleared_with_note() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        drop(open(&path));

        let conn = raw(&path);
        assert_eq!(
            item_field(&conn, "hitl-retry", "hitl_terminal_action"),
            None
        );
        assert_eq!(
            item_field(&conn, "hitl-retry", "hitl_notes").as_deref(),
            Some("[legacy terminal: retry]")
        );
        let request_terminal: Option<String> = conn
            .query_row(
                "SELECT terminal_action FROM hitl_requests WHERE work_id = 'hitl-retry'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(request_terminal, None);

        // Supported values are kept as is.
        assert_eq!(
            item_field(&conn, "hitl-eval", "hitl_terminal_action").as_deref(),
            Some("skip")
        );
    }

    #[test]
    fn every_row_is_readable_after_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        let db = open(&path);

        let items = db.list_items(None, None).unwrap();
        assert_eq!(items.len(), LEGACY_ITEMS.len());
        let conflict = db.get_item("hitl-conflict").unwrap();
        assert_eq!(conflict.phase(), QueuePhase::Hitl);
        assert_eq!(conflict.hitl_reason, Some(HitlReason::ManualEscalation));
        assert_eq!(conflict.lineage_root, "hitl-conflict");
        assert_eq!(conflict.derived_from, None);
        let retry = db.get_item("hitl-retry").unwrap();
        assert_eq!(retry.hitl_terminal_action, None);
    }

    // ---- idempotency, failure, version guard --------------------------------

    #[test]
    fn reopening_a_migrated_database_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        drop(open(&path));

        let snapshot = |conn: &Connection| {
            (
                user_version(conn),
                count(conn, "SELECT COUNT(*) FROM transition_log"),
                count(conn, "SELECT COUNT(*) FROM hitl_requests"),
                conn.query_row(
                    "SELECT group_concat(work_id || '|' || phase || '|' || IFNULL(hitl_notes, ''), ';') FROM (SELECT * FROM queue_items ORDER BY work_id)",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .unwrap(),
            )
        };
        let before = snapshot(&raw(&path));

        drop(open(&path));

        assert_eq!(snapshot(&raw(&path)), before);
        assert!(!dir.path().join("belt.db.bak-v2").exists());
        assert!(!dir.path().join("belt.db.bak-v1.2").exists());
    }

    #[test]
    fn failed_migration_rolls_back_every_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        insert_legacy(
            &raw(&path),
            &LegacyItem {
                work_id: "broken",
                phase: "hitl",
                reason: Some("no_such_reason"),
                notes: None,
                terminal: Some("retry"),
            },
        );

        let err = Database::open(path.to_str().unwrap())
            .err()
            .expect("migration must fail");
        assert!(err.to_string().contains("no_such_reason"), "error: {err}");

        let conn = raw(&path);
        assert_eq!(user_version(&conn), 0);
        assert!(!table_exists(&conn, "transition_log"));
        assert!(!table_exists(&conn, "hitl_requests"));
        assert!(!column_exists(&conn, "queue_items", "lineage_root"));
        assert_eq!(
            item_field(&conn, "hitl-retry", "hitl_terminal_action").as_deref(),
            Some("retry")
        );
        assert_eq!(
            item_field(&conn, "spec-completion:sp1:hitl", "phase").as_deref(),
            Some("hitl")
        );
    }

    #[test]
    fn unknown_phase_fails_the_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        insert_legacy(
            &raw(&path),
            &LegacyItem {
                work_id: "weird",
                phase: "paused",
                reason: None,
                notes: None,
                terminal: None,
            },
        );

        let err = Database::open(path.to_str().unwrap())
            .err()
            .expect("migration must fail");
        assert!(err.to_string().contains("paused"), "error: {err}");
        assert_eq!(user_version(&raw(&path)), 0);
    }

    fn backups_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.contains(".bak-"))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn migration_that_is_certain_to_fail_leaves_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        insert_legacy(
            &raw(&path),
            &LegacyItem {
                work_id: "weird",
                phase: "paused",
                reason: None,
                notes: None,
                terminal: None,
            },
        );

        for _ in 0..2 {
            assert!(Database::open(path.to_str().unwrap()).is_err());
        }
        assert_eq!(backups_in(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn concurrent_first_opens_of_a_legacy_database_all_succeed() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path());
        let path_str = path.to_str().unwrap().to_string();
        let barrier = Arc::new(Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let path = path_str.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    Database::open(&path).map(drop)
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().expect("every concurrent open succeeds");
        }

        let conn = raw(&path);
        assert_eq!(user_version(&conn), CURRENT_VERSION);
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM hitl_requests"),
            count(
                &conn,
                "SELECT COUNT(*) FROM queue_items WHERE phase = 'hitl'"
            )
        );
        // Every backup is a complete, readable database.
        let backups = backups_in(dir.path());
        assert!(!backups.is_empty());
        for name in backups {
            let backup = raw(&dir.path().join(&name));
            assert_eq!(
                count(&backup, "SELECT COUNT(*) FROM queue_items"),
                LEGACY_ITEMS.len() as i64,
                "{name}"
            );
        }
    }

    /// `queue_items` as created by v0.1.0 through v0.1.6, before
    /// `previous_worktree_path` existed (`git show v0.1.0:crates/belt-infra/src/db.rs`).
    const V0_1_0_QUEUE_ITEMS: &str = "
        CREATE TABLE queue_items (
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
            hitl_reason          TEXT,
            hitl_timeout_at      TEXT,
            hitl_terminal_action TEXT,
            replan_count         INTEGER NOT NULL DEFAULT 0,
            worktree_preserved   INTEGER NOT NULL DEFAULT 0
        );
    ";

    #[test]
    fn database_from_a_release_without_previous_worktree_path_is_migrated_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("belt.db");
        {
            let conn = raw(&path);
            conn.execute_batch(V0_1_0_QUEUE_ITEMS).unwrap();
            // The remaining v1 tables had their final shape already.
            conn.execute_batch(super::V1_SCHEMA).unwrap();
            assert!(!column_exists(
                &conn,
                "queue_items",
                "previous_worktree_path"
            ));
            for item in LEGACY_ITEMS {
                insert_legacy(&conn, item);
            }
        }

        let db = open(&path);

        let items = db.list_items(None, None).unwrap();
        assert_eq!(items.len(), LEGACY_ITEMS.len());
        for item in &items {
            assert_eq!(item.previous_worktree_path, None);
            assert_eq!(db.get_item(&item.work_id).unwrap().work_id, item.work_id);
        }
        let conn = raw(&path);
        assert_eq!(user_version(&conn), CURRENT_VERSION);
        let (notnull, default): (i64, Option<String>) = conn
            .query_row(
                "SELECT \"notnull\", dflt_value FROM pragma_table_info('queue_items')
                 WHERE name = 'previous_worktree_path'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((notnull, default), (0, None));
    }

    #[test]
    fn missing_v1_column_keeps_its_v1_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("belt.db");
        {
            let conn = raw(&path);
            conn.execute_batch(
                "CREATE TABLE cron_jobs (name TEXT PRIMARY KEY, schedule TEXT NOT NULL, created_at TEXT NOT NULL);
                 INSERT INTO cron_jobs VALUES ('job', '* * * * *', '2026-01-01T00:00:00Z');",
            )
            .unwrap();
        }

        drop(open(&path));

        let conn = raw(&path);
        let (script, enabled, updated_at): (String, i64, String) = conn
            .query_row(
                "SELECT script, enabled, updated_at FROM cron_jobs WHERE name = 'job'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((script.as_str(), enabled, updated_at.as_str()), ("", 1, ""));
    }

    #[test]
    fn v2_indexes_cover_lineage_and_series_lookups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("belt.db");
        drop(open(&path));

        let conn = raw(&path);
        for (table, index) in [
            ("transition_log", "idx_transition_log_source"),
            ("queue_items", "idx_queue_items_lineage_root"),
            ("history", "idx_history_work_id"),
        ] {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1 AND name = ?2",
                    params![table, index],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "missing index {index} on {table}");
        }
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("belt.db");
        {
            let conn = raw(&path);
            apply_v1(&conn).unwrap();
            conn.pragma_update(None, "user_version", CURRENT_VERSION + 1)
                .unwrap();
        }

        let err = Database::open(path.to_str().unwrap())
            .err()
            .expect("must refuse");
        assert!(err.to_string().contains("newer"), "error: {err}");
        let conn = raw(&path);
        assert_eq!(user_version(&conn), CURRENT_VERSION + 1);
        assert!(!table_exists(&conn, "transition_log"));
        assert!(!dir.path().join("belt.db.bak-v3").exists());
    }
}
