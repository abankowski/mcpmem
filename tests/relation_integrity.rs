use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use mcpmem_core::graph::GraphHandle;
use mcpmem_core::relation_integrity::{audit, repair};
use mcpmem_core::schema::initialize_database;
use mcpmem_core::storage::{Durability, SqliteTuning};
use rusqlite::Connection;
use serde_json::{Value, json};

// Construct the relation table before bootstrap to model a genuine legacy DB.
fn legacy_fixture(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE relation(from_id INTEGER NOT NULL, to_id INTEGER NOT NULL,
         type_id INTEGER NOT NULL, created_us INTEGER NOT NULL) STRICT;",
    )
    .unwrap();
    initialize_database(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO type_dict(id,kind,name,count) VALUES
         (1,0,'private entity type',99),(2,1,'private relation type',99),
         (3,1,'unused',99),(4,7,'unknown kind',99);
         INSERT INTO entity(id,name_hash,name,type_id,created_us,updated_us,flags,out_deg,in_deg)
         VALUES (1,1,'secret alpha',1,0,0,0,99,99),
         (2,2,'secret beta',1,0,0,0,99,99),(3,3,'archived',1,0,0,1,99,99);
         INSERT INTO relation(rowid,from_id,to_id,type_id,created_us) VALUES
         (1,1,2,2,30),(2,1,2,2,10),(3,1,2,2,20),
         (4,2,1,2,10),(5,2,1,2,10),(6,1,1,2,5);
         UPDATE graph_stat SET value=99 WHERE key='relations';",
    )
    .unwrap();
    conn
}

fn cli(args: &[&str], database: &Path) -> Output {
    let binary = option_env!("CARGO_BIN_EXE_mcpmem-maintenance")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/debug/mcpmem-maintenance"));
    Command::new(binary)
        .args(args)
        .arg("--database")
        .arg(database)
        .output()
        .expect("maintenance binary must exist")
}

fn snapshot(conn: &Connection) -> Vec<String> {
    [
        "SELECT rowid,* FROM relation ORDER BY rowid",
        "SELECT * FROM type_dict ORDER BY id",
        "SELECT * FROM entity ORDER BY id",
        "SELECT * FROM graph_stat ORDER BY key",
        "SELECT * FROM sqlite_schema ORDER BY name",
    ]
    .iter()
    .flat_map(|sql| {
        let mut stmt = conn.prepare(sql).unwrap();
        let columns = stmt.column_count();
        stmt.query_map([], |row| {
            Ok((0..columns)
                .map(|i| format!("{:?}", row.get_ref(i).unwrap()))
                .collect::<Vec<_>>()
                .join("|"))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    })
    .collect()
}

#[test]
fn audit_counts_physical_rows_and_drift_without_disclosing_content_or_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let conn = legacy_fixture(&path);
    let before = snapshot(&conn);
    let bytes = std::fs::read(&path).unwrap();
    let actual = serde_json::to_value(audit(&path).unwrap()).unwrap();
    assert_eq!(
        actual,
        json!({
            "duplicate_groups": 2,
            "duplicate_rows": 3,
            "dangling_relation_rows": 0,
            "drift": {
                "graph_stat_relations": {"stored": 99, "actual": 6},
                "type_dict_count": 4,
                "entity_out_deg": 3,
                "entity_in_deg": 3
            }
        })
    );
    assert_eq!(audit(&path).unwrap(), audit(&path).unwrap());
    assert_eq!(snapshot(&conn), before);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn repair_preserves_deterministic_keepers_verifies_backup_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let backup = dir.path().join("backup.db");
    let conn = legacy_fixture(&path);
    // Keep a live WAL open: copying only the main file would miss this fixture.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; UPDATE graph_stat SET value=98 WHERE key='relations';",
    )
    .unwrap();
    let before = snapshot(&conn);
    let result = repair(&path, Some(&backup), true).unwrap();
    assert_eq!(result.before.duplicate_rows, 3);
    assert!(result.after.is_clean());
    let restored = Connection::open(&backup).unwrap();
    assert_eq!(snapshot(&restored), before);
    assert_eq!(
        restored
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    drop(conn);
    let reopened = Connection::open(&path).unwrap();
    let keepers: Vec<i64> = reopened
        .prepare("SELECT rowid FROM relation ORDER BY rowid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(keepers, [2, 4, 6]);
    assert_eq!(
        reopened
            .query_row("SELECT count FROM type_dict WHERE id=1", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        reopened
            .query_row("SELECT out_deg,in_deg FROM entity WHERE id=1", [], |r| Ok(
                (r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)
            ))
            .unwrap(),
        (2, 2)
    );
    assert!(
        reopened
            .execute("INSERT INTO relation VALUES(1,2,2,999,0)", [])
            .is_err()
    );
    let after = snapshot(&reopened);
    let second = repair(&path, Some(&dir.path().join("second.db")), true).unwrap();
    assert!(second.before.is_clean());
    assert!(second.after.is_clean());
    assert_eq!(snapshot(&reopened), after);
}

#[test]
fn all_dangling_variants_refuse_without_changing_the_source() {
    for row in [
        "(99,2,2,0,0)",
        "(1,99,2,0,0)",
        "(1,2,99,0,0)",
        "(1,2,1,0,0)",
        "(1,2,4,0,0)",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        let conn = legacy_fixture(&path);
        conn.execute_batch(&format!("INSERT INTO relation VALUES{row}"))
            .unwrap();
        assert_eq!(audit(&path).unwrap().dangling_relation_rows, 1);
        let before = snapshot(&conn);
        let bytes = std::fs::read(&path).unwrap();
        assert!(repair(&path, Some(&dir.path().join("backup.db")), true).is_err());
        assert_eq!(snapshot(&conn), before, "{row}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "{row}");
    }
}

#[test]
fn missing_confirmation_backup_and_existing_target_refuse_before_source_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let conn = legacy_fixture(&path);
    let before = snapshot(&conn);
    let bytes = std::fs::read(&path).unwrap();
    let backup = dir.path().join("backup.db");
    assert!(repair(&path, Some(&backup), false).is_err());
    assert!(!backup.exists());
    assert!(repair(&path, None, true).is_err());
    std::fs::write(&backup, b"do not clobber").unwrap();
    assert!(repair(&path, Some(&backup), true).is_err());
    assert_eq!(std::fs::read(&backup).unwrap(), b"do not clobber");
    assert!(repair(&path, Some(&path), true).is_err());
    assert_eq!(snapshot(&conn), before);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn repair_recomputes_every_cache_even_without_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let conn = legacy_fixture(&path);
    conn.execute_batch("DELETE FROM relation; DELETE FROM graph_stat WHERE key='relations';")
        .unwrap();
    assert_eq!(audit(&path).unwrap().duplicate_rows, 0);
    let report = repair(&path, Some(&dir.path().join("backup.db")), true).unwrap();
    assert!(report.after.is_clean());
    assert_eq!(
        conn.query_row("SELECT sum(out_deg+in_deg) FROM entity", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row("SELECT sum(count) FROM type_dict WHERE kind!=0", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap(),
        0
    );
}

#[test]
fn foreign_key_failure_refuses_and_late_failure_rolls_back() {
    for corruption in [
        "CREATE TABLE fk_parent(id INTEGER PRIMARY KEY); CREATE TABLE fk_child(parent_id INTEGER REFERENCES fk_parent(id)); INSERT INTO fk_child VALUES(99);",
        "CREATE TRIGGER refuse_stats BEFORE UPDATE ON graph_stat BEGIN SELECT RAISE(ABORT,'fixture refusal'); END;",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        let conn = legacy_fixture(&path);
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute_batch(corruption).unwrap();
        let before = snapshot(&conn);
        let bytes = std::fs::read(&path).unwrap();
        assert!(repair(&path, Some(&dir.path().join("backup.db")), true).is_err());
        assert_eq!(snapshot(&conn), before);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn partial_triple_unique_index_refuses_and_preserves_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let conn = legacy_fixture(&path);
    // All fixture rows have non-negative timestamps, so this accepts the
    // duplicates while still looking like the named triple index to a shallow
    // index-list inspection.
    conn.execute_batch(
        "CREATE UNIQUE INDEX relation_unique_triple
         ON relation(from_id, to_id, type_id)
         WHERE created_us < 0;",
    )
    .unwrap();
    let before = snapshot(&conn);
    let bytes = std::fs::read(&path).unwrap();

    assert!(repair(&path, Some(&dir.path().join("backup.db")), true).is_err());

    assert_eq!(snapshot(&conn), before);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn bootstrap_enforces_fresh_uniqueness_but_server_open_never_repairs_legacy() {
    let fresh = Connection::open_in_memory().unwrap();
    initialize_database(&fresh).unwrap();
    fresh
        .execute("INSERT INTO relation VALUES(1,2,3,0,0)", [])
        .unwrap();
    assert!(
        fresh
            .execute("INSERT INTO relation VALUES(1,2,3,1,0)", [])
            .is_err()
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let conn = legacy_fixture(&path);
    let graph = GraphHandle::new(
        &path,
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .unwrap();
    assert_eq!(graph.get_relation_count().unwrap(), 99);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM relation", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        6
    );
    assert_eq!(
        conn.query_row(
            "SELECT value FROM graph_stat WHERE key='relations'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        99
    );
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE type='index' AND name='relation_unique_triple'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    assert_eq!(audit(&path).unwrap().duplicate_rows, 3);
}

#[test]
fn cli_emits_json_and_refuses_missing_operator_gates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let conn = legacy_fixture(&path);
    let before = snapshot(&conn);
    let bytes = std::fs::read(&path).unwrap();
    let output = cli(&["relation-audit", "--format", "json"], &path);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value, serde_json::to_value(audit(&path).unwrap()).unwrap());
    let backup = dir.path().join("backup.db");
    for args in [
        vec!["relation-repair", "--backup", backup.to_str().unwrap()],
        vec!["relation-repair", "--confirm"],
    ] {
        let refused = cli(&args, &path);
        assert!(!refused.status.success());
        assert!(!refused.stderr.is_empty());
        assert!(refused.stdout.is_empty());
        assert!(!backup.exists());
        assert_eq!(snapshot(&conn), before);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    let output = cli(
        &[
            "relation-repair",
            "--backup",
            backup.to_str().unwrap(),
            "--confirm",
        ],
        &path,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["before"]["duplicate_rows"], 3);
    assert_eq!(value["after"]["duplicate_rows"], 0);
    let output = cli(
        &[
            "relation-repair",
            "--backup",
            backup.to_str().unwrap(),
            "--confirm",
        ],
        &path,
    );
    assert!(!output.status.success());
}

#[test]
fn missing_source_is_not_created_by_audit_or_repair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.db");
    assert!(audit(&path).is_err());
    assert!(repair(&path, Some(&dir.path().join("backup.db")), true).is_err());
    assert!(!path.exists());
}
