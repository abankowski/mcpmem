//! Offline, operator-gated checks and repair for legacy physical relation rows.
//!
//! This module deliberately opens the database directly instead of through
//! `GraphHandle`: auditing must not bootstrap or mutate a legacy database, and
//! repair is never part of normal server startup.

use std::fs::OpenOptions;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, backup::Backup};
use serde::Serialize;

use crate::errors::{MCSError, Result};
use crate::events::sql_error;
use crate::graph::TxGuard;

/// Aggregate relation-integrity state. It contains counts only, never names or
/// observation bodies, so it can safely be emitted by the maintenance CLI.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AuditReport {
    pub duplicate_groups: i64,
    pub duplicate_rows: i64,
    pub dangling_relation_rows: i64,
    pub drift: DriftReport,
}

/// Denormalized counters whose stored values differ from physical graph rows.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DriftReport {
    pub graph_stat_relations: CounterDrift,
    pub type_dict_count: i64,
    pub entity_out_deg: i64,
    pub entity_in_deg: i64,
}

/// A stored counter is optional because a damaged legacy database may be
/// missing the `relations` statistic altogether.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CounterDrift {
    pub stored: Option<i64>,
    pub actual: i64,
}

/// The two audits that bracket a successful repair.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepairReport {
    pub before: AuditReport,
    pub after: AuditReport,
}

impl AuditReport {
    /// Whether the observable relation rows and their derived counters agree.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.duplicate_groups == 0
            && self.duplicate_rows == 0
            && self.dangling_relation_rows == 0
            && self.drift.graph_stat_relations.stored
                == Some(self.drift.graph_stat_relations.actual)
            && self.drift.type_dict_count == 0
            && self.drift.entity_out_deg == 0
            && self.drift.entity_in_deg == 0
    }
}

/// Audit an existing database in one deferred read transaction.
///
/// The connection has `query_only` enabled and is opened without `CREATE`, so
/// this command cannot create or modify a user database.
pub fn audit(database: &Path) -> Result<AuditReport> {
    let conn = open_existing(database)?;
    conn.execute_batch("PRAGMA query_only = ON;")
        .map_err(sql_error)?;
    read_transaction(&conn, audit_current)
}

/// Back up, preflight, and repair a legacy relation table.
///
/// A backup destination is reserved atomically before SQLite's online backup
/// API opens it. Any failed preflight happens before a source-database write;
/// once the write lock is held, `TxGuard` rolls back every later failure.
pub fn repair(database: &Path, backup: Option<&Path>, confirmed: bool) -> Result<RepairReport> {
    if !confirmed {
        return Err(MCSError::InvalidParams(
            "relation repair requires --confirm".into(),
        ));
    }
    let backup = backup.ok_or_else(|| {
        MCSError::InvalidParams("relation repair requires a --backup path".into())
    })?;
    let source = open_existing(database)?;
    reserve_backup(backup)?;
    backup_and_verify(&source, backup)?;

    let tx = TxGuard::begin(&source)?;
    validate_source(&source)?;
    let before = audit_current(&source)?;
    if before.dangling_relation_rows != 0 {
        return Err(MCSError::MemoryError(
            "relation repair refused: dangling relation rows require explicit remediation".into(),
        ));
    }

    source
        .execute_batch(
            "DELETE FROM relation
             WHERE rowid IN (
               SELECT rowid FROM (
                 SELECT rowid,
                        ROW_NUMBER() OVER (
                          PARTITION BY from_id, to_id, type_id
                          ORDER BY created_us ASC, rowid ASC
                        ) AS ordinal
                 FROM relation
               ) WHERE ordinal > 1
             );",
        )
        .map_err(sql_error)?;
    rebuild_relation_caches(&source)?;
    // This is intentionally last: putting the index in bootstrap for an
    // existing legacy relation table would prevent this operator repair.
    ensure_relation_unique_index(&source)?;

    let after = audit_current(&source)?;
    if !after.is_clean() {
        return Err(MCSError::MemoryError(
            "relation repair did not produce a clean audit".into(),
        ));
    }
    tx.commit()?;
    Ok(RepairReport { before, after })
}

fn open_existing(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(sql_error)
}

fn reserve_backup(path: &Path) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(drop)
        .map_err(MCSError::IoError)
}

fn backup_and_verify(source: &Connection, destination_path: &Path) -> Result<()> {
    let mut destination = Connection::open_with_flags(
        destination_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(sql_error)?;
    {
        let backup = Backup::new(source, &mut destination).map_err(sql_error)?;
        backup
            .run_to_completion(128, Duration::from_millis(10), None)
            .map_err(sql_error)?;
    }
    drop(destination);

    let verified = open_existing(destination_path)?;
    require_integrity_check(&verified, "backup")
}

fn validate_source(conn: &Connection) -> Result<()> {
    require_integrity_check(conn, "source")?;
    let mut statement = conn
        .prepare("PRAGMA foreign_key_check")
        .map_err(sql_error)?;
    let mut rows = statement.query([]).map_err(sql_error)?;
    if rows.next().map_err(sql_error)?.is_some() {
        return Err(MCSError::MemoryError(
            "relation repair refused: foreign_key_check returned violations".into(),
        ));
    }
    Ok(())
}

fn require_integrity_check(conn: &Connection, label: &str) -> Result<()> {
    let mut statement = conn.prepare("PRAGMA integrity_check").map_err(sql_error)?;
    let results = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(sql_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_error)?;
    if results.as_slice() != ["ok"] {
        return Err(MCSError::MemoryError(format!(
            "{label} database failed PRAGMA integrity_check"
        )));
    }
    Ok(())
}

fn read_transaction<T>(
    conn: &Connection,
    read: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    conn.execute_batch("BEGIN DEFERRED").map_err(sql_error)?;
    match read(conn) {
        Ok(value) => {
            conn.execute_batch("COMMIT").map_err(sql_error)?;
            Ok(value)
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn audit_current(conn: &Connection) -> Result<AuditReport> {
    let mut duplicate_statement = conn
        .prepare(
            "SELECT COUNT(*)
             FROM relation
             GROUP BY from_id, to_id, type_id
             HAVING COUNT(*) > 1
             ORDER BY from_id, to_id, type_id",
        )
        .map_err(sql_error)?;
    let duplicate_sizes = duplicate_statement
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(sql_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_error)?;
    let duplicate_groups = duplicate_sizes.len() as i64;
    let duplicate_rows = duplicate_sizes.into_iter().map(|count| count - 1).sum();
    let dangling_relation_rows = conn
        .query_row(
            "SELECT COUNT(*)
             FROM relation r
             LEFT JOIN entity source ON source.id = r.from_id
             LEFT JOIN entity destination ON destination.id = r.to_id
             LEFT JOIN type_dict relation_type
               ON relation_type.id = r.type_id AND relation_type.kind = 1
             WHERE source.id IS NULL
                OR destination.id IS NULL
                OR relation_type.id IS NULL",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    let relation_count = conn
        .query_row("SELECT COUNT(*) FROM relation", [], |row| row.get(0))
        .map_err(sql_error)?;
    let stored_relation_count = conn
        .query_row(
            "SELECT value FROM graph_stat WHERE key = 'relations'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    let type_dict_count = conn
        .query_row(
            "SELECT COUNT(*)
             FROM type_dict dictionary
             WHERE dictionary.count != CASE dictionary.kind
               WHEN 0 THEN (
                 SELECT COUNT(*) FROM entity
                 WHERE entity.type_id = dictionary.id AND entity.flags = 0
               )
               WHEN 1 THEN (
                 SELECT COUNT(*) FROM relation
                 WHERE relation.type_id = dictionary.id
               )
               ELSE 0
             END",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    let entity_out_deg = conn
        .query_row(
            "SELECT COUNT(*) FROM entity source
             WHERE source.out_deg != (
               SELECT COUNT(*) FROM relation WHERE from_id = source.id
             )",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    let entity_in_deg = conn
        .query_row(
            "SELECT COUNT(*) FROM entity destination
             WHERE destination.in_deg != (
               SELECT COUNT(*) FROM relation WHERE to_id = destination.id
             )",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;

    Ok(AuditReport {
        duplicate_groups,
        duplicate_rows,
        dangling_relation_rows,
        drift: DriftReport {
            graph_stat_relations: CounterDrift {
                stored: stored_relation_count,
                actual: relation_count,
            },
            type_dict_count,
            entity_out_deg,
            entity_in_deg,
        },
    })
}

fn rebuild_relation_caches(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "INSERT INTO graph_stat(key, value)
         VALUES ('relations', (SELECT COUNT(*) FROM relation))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value;

         UPDATE type_dict
         SET count = CASE kind
           WHEN 0 THEN (
             SELECT COUNT(*) FROM entity
             WHERE entity.type_id = type_dict.id AND entity.flags = 0
           )
           WHEN 1 THEN (
             SELECT COUNT(*) FROM relation
             WHERE relation.type_id = type_dict.id
           )
           ELSE 0
         END;

         UPDATE entity
         SET out_deg = (SELECT COUNT(*) FROM relation WHERE from_id = entity.id),
             in_deg = (SELECT COUNT(*) FROM relation WHERE to_id = entity.id);",
    )
    .map_err(sql_error)
}

fn ensure_relation_unique_index(conn: &Connection) -> Result<()> {
    let index_attributes: Option<(bool, bool)> = conn
        .query_row(
            "SELECT \"unique\" = 1, \"partial\" = 0
             FROM pragma_index_list('relation')
             WHERE name = 'relation_unique_triple'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    match index_attributes {
        None => conn
            .execute_batch(
                "CREATE UNIQUE INDEX relation_unique_triple
                 ON relation(from_id, to_id, type_id);",
            )
            .map_err(sql_error),
        Some((true, true)) => {
            let columns = conn
                .prepare(
                    "SELECT name FROM pragma_index_info('relation_unique_triple') ORDER BY seqno",
                )
                .map_err(sql_error)?
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_error)?;
            if columns == ["from_id", "to_id", "type_id"] {
                Ok(())
            } else {
                Err(MCSError::MemoryError(
                    "relation_unique_triple does not enforce the relation triple".into(),
                ))
            }
        }
        Some((true, false)) => Err(MCSError::MemoryError(
            "relation_unique_triple must not be a partial index".into(),
        )),
        Some((false, _)) => Err(MCSError::MemoryError(
            "relation_unique_triple exists but is not unique".into(),
        )),
    }
}
