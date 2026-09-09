//! Portable, non-destructive graph bootstrap shared by every runtime role.
use rusqlite::Connection;

use crate::errors::Result;
use crate::events::sql_error;
use crate::graph::TxGuard;

/// Establish the legacy graph prerequisites before applying ordered migrations.
///
/// The caller owns connection tuning and durability; this sets no PRAGMAs.
/// Bootstrap never clears existing rows or rebuilds derived data. Its transaction
/// serializes the initial statistics seed; all pending versioned migrations then
/// run together in their own transaction, so a failure publishes no partial
/// migration version. Call before readers or workers start using the database.
pub fn initialize_database(conn: &Connection) -> Result<()> {
    let tx = TxGuard::begin(conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS entity (
             id          INTEGER PRIMARY KEY,
             name_hash   INTEGER NOT NULL,
             name        TEXT    NOT NULL,
             type_id     INTEGER NOT NULL,
             obs_count   INTEGER NOT NULL DEFAULT 0,
             out_deg     INTEGER NOT NULL DEFAULT 0,
             in_deg      INTEGER NOT NULL DEFAULT 0,
             created_us  INTEGER NOT NULL,
             updated_us  INTEGER NOT NULL,
             flags       INTEGER NOT NULL DEFAULT 0
         ) STRICT;

         CREATE INDEX IF NOT EXISTS entity_by_hash
             ON entity(name_hash, type_id, obs_count, out_deg, in_deg)
             WHERE flags = 0;

         CREATE INDEX IF NOT EXISTS entity_name_ci
             ON entity(lower(name))
             WHERE flags = 0;

         CREATE TABLE IF NOT EXISTS observation (
             id          INTEGER PRIMARY KEY,
             entity_id   INTEGER NOT NULL,
             idx         INTEGER NOT NULL,
             body        TEXT    NOT NULL,
             created_us  INTEGER NOT NULL
         ) STRICT;

         CREATE INDEX IF NOT EXISTS obs_by_entity
             ON observation(entity_id, idx);

         CREATE TABLE IF NOT EXISTS relation (
             from_id     INTEGER NOT NULL,
             to_id       INTEGER NOT NULL,
             type_id     INTEGER NOT NULL,
             created_us  INTEGER NOT NULL
         ) STRICT;

         CREATE INDEX IF NOT EXISTS rel_out
             ON relation(from_id, type_id, to_id);

         CREATE INDEX IF NOT EXISTS rel_in
             ON relation(to_id, type_id, from_id);

         CREATE VIRTUAL TABLE IF NOT EXISTS name_fts
             USING fts5(name, content='entity', content_rowid='id',
                        tokenize='unicode61 remove_diacritics 2');

         CREATE VIRTUAL TABLE IF NOT EXISTS obs_fts
             USING fts5(body, content='observation', content_rowid='id',
                        tokenize='unicode61 remove_diacritics 2');

         CREATE TRIGGER IF NOT EXISTS obs_fts_ai AFTER INSERT ON observation BEGIN
           INSERT INTO obs_fts(rowid, body) VALUES (new.id, new.body);
         END;

         CREATE TRIGGER IF NOT EXISTS obs_fts_bd BEFORE DELETE ON observation BEGIN
           INSERT INTO obs_fts(obs_fts, rowid, body) VALUES ('delete', old.id, '');
         END;

         CREATE TABLE IF NOT EXISTS type_dict (
             id     INTEGER PRIMARY KEY,
             kind   INTEGER NOT NULL,
             name   TEXT    NOT NULL,
             count  INTEGER NOT NULL DEFAULT 0
         ) STRICT;

         CREATE INDEX IF NOT EXISTS type_by_name
             ON type_dict(kind, name);

         CREATE TABLE IF NOT EXISTS graph_stat (
             key    TEXT NOT NULL PRIMARY KEY,
             value  INTEGER NOT NULL
         ) STRICT, WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS hub_degree (
             entity_id INTEGER PRIMARY KEY,
             out_deg   INTEGER NOT NULL,
             in_deg    INTEGER NOT NULL
         ) STRICT;

         CREATE TABLE IF NOT EXISTS partition_map (
             table_name TEXT NOT NULL PRIMARY KEY,
             role       INTEGER NOT NULL,
             type_id    INTEGER,
             row_count  INTEGER NOT NULL DEFAULT 0
         ) STRICT, WITHOUT ROWID;",
    )
    .map_err(sql_error)?;
    let has_stat: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM graph_stat)", [], |row| {
            row.get(0)
        })
        .map_err(sql_error)?;
    if !has_stat {
        conn.execute_batch(
            "INSERT INTO graph_stat(key, value) VALUES
             ('entities', 0), ('relations', 0), ('observations', 0),
             ('entity_seq', 0), ('obs_seq', 0);",
        )
        .map_err(sql_error)?;
    }
    tx.commit()?;
    crate::events::migrate(conn)
}
