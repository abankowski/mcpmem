//! Durable change log and delivery leases. No network work occurs here.
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::errors::{MCSError, Result};
use crate::graph::TxGuard;
use crate::mutation::{CommittedChangeSet, EntityChange, MutationContext};

pub fn sql_error(error: rusqlite::Error) -> MCSError {
    MCSError::IoError(std::io::Error::other(error))
}

pub fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(i64::MAX as u128) as i64
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Method, normalized path and exact ingress bytes are length-delimited so
/// neither separators inside a body nor JSON reserialization can collide.
pub fn request_fingerprint(method: &str, normalized_path: &str, raw_body: &[u8]) -> String {
    let mut hash = Sha256::new();
    for part in [method.as_bytes(), normalized_path.as_bytes(), raw_body] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    format!("{hash:x}", hash = hash.finalize())
}

/// The ordered migration set, embedded at compile time.
///
/// Public because a test that builds a historical database needs the exact SQL
/// and its checksum. It must not read the `.sql` file itself: the files belong
/// to this crate, and `cargo package` copies only the files under one crate
/// root, so an `include_str!` from another crate ships a crate that cannot
/// compile. That is how the `v1.0.0-rc.1` release failed.
pub const MIGRATIONS: [(i64, &str); 15] = [
    (1, include_str!("../migrations/0001_change_events.sql")),
    (
        2,
        include_str!("../migrations/0002_webhook_subscriptions.sql"),
    ),
    (
        3,
        include_str!("../migrations/0003_observation_metadata.sql"),
    ),
    (4, include_str!("../migrations/0004_oauth.sql")),
    (5, include_str!("../migrations/0005_principals.sql")),
    (6, include_str!("../migrations/0006_code_repos.sql")),
    (7, include_str!("../migrations/0007_taxonomy_index.sql")),
    (8, include_str!("../migrations/0008_type_descriptions.sql")),
    (9, include_str!("../migrations/0009_chunked_embeddings.sql")),
    (10, include_str!("../migrations/0010_embedding_cleanup.sql")),
    (
        11,
        include_str!("../migrations/0011_relation_observations_and_attributes.sql"),
    ),
    (
        12,
        include_str!("../migrations/0012_rel_obs_fts_delete_fix.sql"),
    ),
    (
        13,
        include_str!("../migrations/0013_quiet_events_share_revision.sql"),
    ),
    (14, include_str!("../migrations/0014_workspace_marker.sql")),
    (15, include_str!("../migrations/0015_attachments.sql")),
];

#[cfg(test)]
mod migration_inventory {
    /// The one inventory anchor for the migration set. Every other test derives
    /// its expectation from [`super::MIGRATIONS`], so a registry entry deleted
    /// by a bad merge, misnumbered, or edited in place would otherwise pass the
    /// whole suite. Checksums are what production verifies at every startup, so
    /// pinning them here pins count, order and content together.
    #[test]
    fn every_migration_version_and_checksum_is_pinned() {
        let inventory: Vec<(i64, String)> = super::MIGRATIONS
            .iter()
            .map(|(version, sql)| (*version, super::sha256(sql.as_bytes())))
            .collect();
        assert_eq!(
            inventory,
            vec![
                (
                    1,
                    "a48def8b25e9ecf813d3fa27a785893ba5de346a8b82f012cc543af2fecd5af2".to_string()
                ),
                (
                    2,
                    "2c267315d89203d5223895a845b275d8add904310d2c0a5a7a5b0d1873b984ec".to_string()
                ),
                (
                    3,
                    "0b82809aca1b4e90edfea4796e33a9c32963e055b7b57b8524b86dcb580bd454".to_string()
                ),
                (
                    4,
                    "5d18e23d999661dda33bfd2358659530760afec0a079c3a1b96689b537af28a2".to_string()
                ),
                (
                    5,
                    "9e40e5c633bef1facda4361563d4c7952f4e1a8d86d11a4c20844bf1ff63b412".to_string()
                ),
                (
                    6,
                    "f57f6103c82269627ada7de2cea989a2494aabdb066eb6f9775e3ea6d32ed564".to_owned()
                ),
                (
                    7,
                    "4949f6c6f73c22bce5de31219d2d5d52739cde05967363c2fc89e88b35c044e8".to_string()
                ),
                (
                    8,
                    "9e0721309bf535e79ccf8561c7556f663dda3b6ffd8eb176973630ebf7d39a54".to_string()
                ),
                (
                    9,
                    "22810c6a00ba60b3360b4ada74c3e33519a654d1e5f94b890718dd057e373acb".to_string()
                ),
                (
                    10,
                    "fc42f71ef3edc7e6ccb7a695456d519e394c9ab92de8f2078bdc3b60c4f4bca2".to_string()
                ),
                (
                    11,
                    "11456cb24b55e78a80c3e05ff37f4fd3411f9d16c5898132f7b783109f5474a8".to_string()
                ),
                (
                    12,
                    "2a713d3eb83bc75449063b94cf7089b2361b3b92612065407ee063f9468f8a2c".to_string()
                ),
                (
                    13,
                    "ad6117ba4176acbd113696338b026e9753d5a7bfd61868accc9309be60e48268".to_string()
                ),
                (
                    14,
                    "7eb71e14f5978792748beb04a81601daf4ccbc0cbaa92e60b3189fa91d2021d7".to_string()
                ),
                (
                    15,
                    "b0bf821aea63c30d0eaf1397398c46196afa12b51dc8c82ff0037f359c3772a9".to_string()
                ),
            ],
            "a migration was added, removed, renumbered or edited"
        );
    }
}

/// Apply all pending ordered migrations in one transaction, including their
/// ledger entries. Any failure rolls the entire pending set back; historical
/// checksums are verified even when no migrations remain to apply.
///
/// Startup callers use [`crate::schema::initialize_database`] to establish the
/// legacy graph tables and statistics before these migrations can reference them.
pub fn migrate(conn: &Connection) -> Result<()> {
    let tx = TxGuard::begin(conn)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_migration(version INTEGER PRIMARY KEY, checksum TEXT NOT NULL, applied_at_us INTEGER NOT NULL) STRICT;").map_err(sql_error)?;
    let migrations = MIGRATIONS;
    let newest: i64 = conn
        .query_row(
            "SELECT coalesce(max(version),0) FROM schema_migration",
            [],
            |r| r.get(0),
        )
        .map_err(sql_error)?;
    if newest > migrations.last().map_or(0, |(version, _)| *version) {
        return Err(MCSError::MemoryError(
            "database schema is newer than this binary".into(),
        ));
    }
    for (version, sql) in migrations {
        let checksum = sha256(sql.as_bytes());
        let existing: Option<String> = conn
            .query_row(
                "SELECT checksum FROM schema_migration WHERE version=?1",
                [version],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        match existing {
            Some(existing) if existing != checksum => {
                return Err(MCSError::MemoryError(format!(
                    "migration {version} checksum mismatch"
                )));
            }
            Some(_) => {}
            None => {
                conn.execute_batch(sql).map_err(sql_error)?;
                conn.execute(
                    "INSERT INTO schema_migration VALUES(?1,?2,?3)",
                    params![version, checksum, now_us()],
                )
                .map_err(sql_error)?;
            }
        }
    }
    tx.commit()
}

#[cfg(test)]
mod attachment_migration_tests {
    use rusqlite::{Connection, params, types::Value};

    use super::{MIGRATIONS, migrate, sha256};

    // Build the graph that the version-14 binary leaves on disk. Apply its
    // actual migrations and record their checksums, not a copy of their SQL.
    fn graph_at_14() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE entity(id INTEGER PRIMARY KEY, name_hash INTEGER NOT NULL, name TEXT NOT NULL, type_id INTEGER NOT NULL,
                obs_count INTEGER NOT NULL DEFAULT 0, out_deg INTEGER NOT NULL DEFAULT 0, in_deg INTEGER NOT NULL DEFAULT 0,
                created_us INTEGER NOT NULL, updated_us INTEGER NOT NULL, flags INTEGER NOT NULL DEFAULT 0) STRICT;
             CREATE TABLE observation(id INTEGER PRIMARY KEY, entity_id INTEGER NOT NULL, idx INTEGER NOT NULL, body TEXT NOT NULL, created_us INTEGER NOT NULL) STRICT;
             CREATE TABLE relation(from_id INTEGER NOT NULL, to_id INTEGER NOT NULL, type_id INTEGER NOT NULL, created_us INTEGER NOT NULL) STRICT;
             CREATE TABLE type_dict(id INTEGER PRIMARY KEY, kind INTEGER NOT NULL, name TEXT NOT NULL, count INTEGER NOT NULL DEFAULT 0) STRICT;
             CREATE TABLE graph_stat(key TEXT NOT NULL PRIMARY KEY, value INTEGER NOT NULL) STRICT, WITHOUT ROWID;
             INSERT INTO graph_stat VALUES
                 ('entities',0),('relations',0),('observations',0),('entity_seq',0),('obs_seq',0);
             CREATE VIRTUAL TABLE obs_fts USING fts5(body, content='observation', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
             CREATE TRIGGER obs_fts_bd BEFORE DELETE ON observation BEGIN
               INSERT INTO obs_fts(obs_fts, rowid, body) VALUES ('delete', old.id, old.body);
             END;
             CREATE TABLE schema_migration(version INTEGER PRIMARY KEY, checksum TEXT NOT NULL, applied_at_us INTEGER NOT NULL) STRICT;",
        )
        .unwrap();
        for &(version, sql) in MIGRATIONS.iter().filter(|(version, _)| *version <= 14) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migration VALUES(?1,?2,1)",
                params![version, sha256(sql.as_bytes())],
            )
            .unwrap();
        }
        assert_eq!(
            conn.query_row("SELECT max(version) FROM schema_migration", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            14
        );
        conn
    }

    fn rows(conn: &Connection, table: &str) -> Vec<Vec<Value>> {
        let mut statement = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let column_count = statement.column_count();
        statement
            .query_map([], |row| {
                (0..column_count)
                    .map(|index| row.get::<_, Value>(index))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn index_sql(conn: &Connection, name: &str) -> String {
        conn.query_row(
            "SELECT sql FROM sqlite_schema WHERE type='index' AND name=?1",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn index_columns(conn: &Connection, name: &str) -> Vec<String> {
        conn.prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")
            .unwrap()
            .query_map([name], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn migration_15_preserves_vector_bytes_jobs_keys_and_named_indexes() {
        let conn = graph_at_14();
        conn.execute_batch(
            "INSERT INTO chunk_vector VALUES
                ('profile','identity','entity',10,0,4,2,x'00ff80deadbeef',101,'old-entity'),
                ('profile','relation','relation',20,1,5,3,x'0102030004',102,'old-relation');
             INSERT INTO chunk_index_job
                (profile_id,owner_kind,owner_id,owner_revision,operation,state,lease_token,lease_epoch,lease_until_us,next_attempt_us,attempts,last_error)
             VALUES
                ('profile','entity',10,2,'upsert','leased','old-lease',7,500,400,3,'retry'),
                ('profile','relation',20,3,'delete','held',NULL,0,0,900,1,NULL);",
        )
        .unwrap();
        let vectors = rows(&conn, "chunk_vector");
        let jobs = rows(&conn, "chunk_index_job");
        assert_eq!(vectors.len(), 2, "the historical fixture must have vectors");
        assert_eq!(jobs.len(), 2, "the historical fixture must have jobs");
        let index_names = [
            "chunk_vector_owner",
            "chunk_vector_type",
            "chunk_index_job_due",
            "chunk_index_job_owner",
        ];
        let indexes: Vec<_> = index_names
            .iter()
            .map(|name| index_sql(&conn, name))
            .collect();

        migrate(&conn).unwrap();

        assert_eq!(rows(&conn, "chunk_vector"), vectors);
        assert_eq!(rows(&conn, "chunk_index_job"), jobs);
        for (name, sql) in index_names.iter().zip(indexes) {
            assert_eq!(index_sql(&conn, name), sql, "{name}");
        }
        assert!(
            conn.execute(
                "INSERT INTO chunk_vector VALUES ('profile','identity','entity',10,0,4,2,x'42',1,'duplicate')",
                [],
            )
            .is_err(),
            "the original composite vector key must remain unique"
        );
        assert!(
            conn.execute(
                "INSERT INTO chunk_index_job(profile_id,owner_kind,owner_id,owner_revision,operation)
                 VALUES ('profile','entity',10,2,'upsert')",
                [],
            )
            .is_err(),
            "the original composite job key must remain unique"
        );
        conn.execute(
            "INSERT INTO chunk_vector VALUES ('profile','attachment','attachment',30,0,4,1,x'ff00',103,'new-file')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chunk_index_job(profile_id,owner_kind,owner_id,owner_revision,operation)
             VALUES ('profile','attachment',30,1,'upsert')",
            [],
        )
        .unwrap();
        for (kind, owner_kind) in [("invalid", "attachment"), ("attachment", "invalid")] {
            assert!(
                conn.execute(
                    "INSERT INTO chunk_vector VALUES ('profile',?1,?2,31,0,4,1,x'01',1,'invalid')",
                    params![kind, owner_kind],
                )
                .is_err(),
                "invalid vector kind or owner must fail"
            );
        }
        assert!(
            conn.execute(
                "INSERT INTO chunk_index_job(profile_id,owner_kind,owner_id,owner_revision,operation)
                 VALUES ('profile','invalid',31,1,'upsert')",
                [],
            )
            .is_err(),
            "an unrelated job owner must fail"
        );
        assert!(
            conn.execute(
                "INSERT INTO chunk_index_job(profile_id,owner_kind,owner_id,owner_revision,operation)
                 VALUES ('profile','attachment',31,1,'invalid')",
                [],
            )
            .is_err(),
            "the old operation check must remain"
        );
        assert!(
            conn.execute(
                "INSERT INTO chunk_index_job(profile_id,owner_kind,owner_id,owner_revision,operation,state)
                 VALUES ('profile','attachment',31,1,'upsert','invalid')",
                [],
            )
            .is_err(),
            "the old job-state check must remain"
        );
        assert!(
            conn.execute(
                "INSERT INTO chunk_index_job(profile_id,owner_kind,owner_id,owner_revision,operation,last_error)
                 VALUES ('profile','attachment',31,1,'upsert',?1)",
                ["x".repeat(2049)],
            )
            .is_err(),
            "the old last-error length check must remain"
        );
        migrate(&conn).unwrap();
        assert_eq!(rows(&conn, "chunk_vector").len(), 3);
        assert_eq!(rows(&conn, "chunk_index_job").len(), 3);
    }

    #[test]
    fn migration_15_creates_six_strict_graph_local_tables_with_valid_states() {
        let conn = graph_at_14();
        migrate(&conn).unwrap();
        for name in [
            "attachment",
            "attachment_text",
            "attachment_chunk",
            "attachment_job",
            "attachment_upload",
            "attachment_upload_chunk",
        ] {
            let strict: i64 = conn
                .query_row(
                    "SELECT strict FROM pragma_table_list WHERE name=?1",
                    [name],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(strict, 1, "{name} must be STRICT");
            let has_workspace_id: bool = conn
                .prepare(&format!("PRAGMA table_info({name})"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .any(|column| column.unwrap() == "workspace_id");
            assert!(!has_workspace_id, "{name} is already in a workspace graph");
        }
        for (index, expected_columns) in [
            ("attachment_entity_filename", vec!["entity_id", "filename"]),
            ("attachment_entity", vec!["entity_id"]),
            (
                "attachment_job_due",
                vec!["state", "next_attempt_us", "lease_until_us"],
            ),
            ("attachment_upload_expires", vec!["expires_us"]),
        ] {
            assert_eq!(index_columns(&conn, index), expected_columns, "{index}");
        }
        conn.execute_batch(
            "INSERT INTO attachment VALUES (1,10,'notes.txt','text/plain',2,zeroblob(32),x'6869','uploaded',0,NULL,NULL,1);
             INSERT INTO attachment VALUES (2,11,'notes.txt','text/plain',2,zeroblob(32),x'6869','ready',1,NULL,NULL,1);
             INSERT INTO attachment_text VALUES (2,1,'hi',2);
             INSERT INTO attachment_chunk VALUES (2,0,1,0,'hi');
             INSERT INTO attachment_job VALUES (1,'pending',NULL,0,0,0,0,NULL);
             INSERT INTO attachment_upload VALUES ('upload','owner',10,'draft.txt','text/plain',2,zeroblob(32),2,1,100,NULL);
             INSERT INTO attachment_upload_chunk VALUES ('upload',0,x'6869');",
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO attachment VALUES (3,10,'notes.txt','text/plain',2,zeroblob(32),x'6869','uploaded',0,NULL,NULL,1)",
                [],
            )
            .is_err(),
            "the filename must be unique within one entity"
        );
        assert!(
            conn.execute("UPDATE attachment SET status='unknown' WHERE id=1", [])
                .is_err()
        );
        assert!(
            conn.execute("UPDATE attachment SET error_stage='unknown' WHERE id=1", [])
                .is_err()
        );
        assert!(
            conn.execute("UPDATE attachment SET error_stage='decode' WHERE id=1", [])
                .is_err(),
            "an upload cannot have a failed extraction stage"
        );
        conn.execute(
            "UPDATE attachment SET status='extracting',error_stage='decode',last_error='invalid UTF-8' WHERE id=1",
            [],
        )
        .unwrap();
        conn.execute("UPDATE attachment SET status='error' WHERE id=1", [])
            .unwrap();
        conn.execute(
            "UPDATE attachment SET status='ready',error_stage=NULL,last_error=NULL WHERE id=1",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO attachment_text VALUES (2,0,'invalid page',12)",
                [],
            )
            .is_err(),
            "page numbers start at one"
        );
        assert!(
            conn.execute(
                "UPDATE attachment_job SET state='unknown' WHERE attachment_id=1",
                []
            )
            .is_err()
        );
        assert!(
            conn.execute(
                "INSERT INTO attachment_chunk VALUES (2,1,1,0,'duplicate segment')",
                [],
            )
            .is_err(),
            "one page segment cannot map to two vector indexes"
        );
        assert!(
            conn.execute(
                "INSERT INTO attachment_chunk VALUES (2,0,1,1,'duplicate vector index')",
                [],
            )
            .is_err(),
            "one vector index maps to one segment"
        );
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeEvent {
    pub event_id: Uuid,
    pub transaction_id: Uuid,
    pub entity_id: i64,
    pub entity_revision: i64,
    pub occurred_at_us: i64,
    pub change: EntityChange,
    pub provenance: MutationContext,
}

pub(crate) fn persist_changes(
    conn: &Connection,
    changes: &CommittedChangeSet,
    context: &MutationContext,
    quiet: bool,
) -> Result<()> {
    for change in &changes.changes {
        let snapshot = change
            .after
            .as_ref()
            .or(change.before.as_ref())
            .ok_or_else(|| MCSError::MemoryError("empty entity change".into()))?;
        let deleted = change.after.is_none();
        // Quiet changes (attribute writes, relation observation writes) carry
        // the current structural revision without bumping it; consecutive
        // quiet events on one entity may share a revision. The event row and
        // the subscription outbox row are the point; the revision bump and
        // the index enqueue stay off (REQ-ATTR-OFFLINE).
        let quiet_change = quiet || change.relation_change.is_some();
        let revision: i64 = if quiet_change {
            let current: Option<i64> = conn
                .query_row(
                    "SELECT revision FROM entity_revision WHERE entity_id=?1",
                    [snapshot.entity_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            match current {
                Some(revision) => revision,
                None => {
                    conn.execute(
                        "INSERT INTO entity_revision(entity_id, revision, deleted) VALUES(?1, 1, 0)",
                        [snapshot.entity_id],
                    )
                    .map_err(sql_error)?;
                    1
                }
            }
        } else {
            conn.query_row("INSERT INTO entity_revision VALUES(?1,1,?2) ON CONFLICT(entity_id) DO UPDATE SET revision=revision+1, deleted=excluded.deleted RETURNING revision", params![snapshot.entity_id, deleted], |r| r.get(0)).map_err(sql_error)?
        };
        let event = ChangeEvent {
            event_id: Uuid::new_v4(),
            transaction_id: changes.transaction_id,
            entity_id: snapshot.entity_id,
            entity_revision: revision,
            occurred_at_us: now_us(),
            change: change.clone(),
            provenance: context.clone(),
        };
        conn.execute(
            "INSERT INTO change_event VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                event.event_id.to_string(),
                event.transaction_id.to_string(),
                event.entity_id,
                revision,
                event.occurred_at_us,
                serde_json::to_string(&event)?
            ],
        )
        .map_err(sql_error)?;
        if !quiet_change {
            crate::jobs::enqueue_change(conn, snapshot.entity_id, revision, deleted)?;
        }
        crate::subscriptions::SubscriptionRepository::new(conn).enqueue_matching(&event)?;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub token: Uuid,
    pub epoch: i64,
    pub until_us: i64,
}

pub(crate) fn lease_until(now: i64, duration_us: i64) -> Result<i64> {
    if duration_us <= 0 || duration_us > 3_600_000_000 {
        return Err(MCSError::InvalidParams(
            "lease duration must be 1us..1h".into(),
        ));
    }
    now.checked_add(duration_us)
        .ok_or_else(|| MCSError::InvalidParams("lease time overflow".into()))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventDelivery {
    pub delivery_id: Uuid,
    pub subscription_id: Uuid,
    pub event: ChangeEvent,
    pub lease: Lease,
    pub attempts: i64,
}

pub struct EventRepository<'a> {
    conn: &'a Connection,
}

impl<'a> EventRepository<'a> {
    pub const fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn get(&self, event_id: Uuid) -> Result<Option<ChangeEvent>> {
        let payload: Option<String> = self
            .conn
            .query_row(
                "SELECT payload FROM change_event WHERE event_id=?1",
                [event_id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        payload
            .map(|text| serde_json::from_str(&text).map_err(Into::into))
            .transpose()
    }

    pub fn enqueue_delivery(&self, event_id: Uuid, subscription_id: Uuid) -> Result<()> {
        let tx = TxGuard::begin(self.conn)?;
        if self.get(event_id)?.is_none() {
            return Err(MCSError::InvalidParams("unknown event".into()));
        }
        self.conn.execute("INSERT INTO event_outbox(delivery_id,event_id,subscription_id) VALUES(?1,?2,?3) ON CONFLICT(event_id,subscription_id) DO NOTHING", params![Uuid::new_v4().to_string(), event_id.to_string(), subscription_id.to_string()]).map_err(sql_error)?;
        tx.commit()
    }

    /// At most one active delivery per subscription, including claims made by
    /// other processes. Expired tokens are superseded by a strictly newer epoch.
    pub fn claim_due(&self, now: i64, duration_us: i64) -> Result<Option<EventDelivery>> {
        let until = lease_until(now, duration_us)?;
        let tx = TxGuard::begin(self.conn)?;
        let row: Option<(String,String,String,i64,i64)> = self.conn.query_row("SELECT delivery_id,subscription_id,event_id,lease_epoch,attempts FROM event_outbox e WHERE ((state='pending' AND next_attempt_us<=?1) OR (state='leased' AND lease_until_us<=?1)) AND NOT EXISTS(SELECT 1 FROM event_outbox busy WHERE busy.subscription_id=e.subscription_id AND busy.state='leased' AND busy.lease_until_us>?1) ORDER BY next_attempt_us,delivery_id LIMIT 1", [now], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(sql_error)?;
        let result = row.map(|(delivery, subscription, event, epoch, attempts)| -> Result<EventDelivery> {
            let token = Uuid::new_v4();
            self.conn.execute("UPDATE event_outbox SET state='leased',lease_token=?2,lease_epoch=lease_epoch+1,lease_until_us=?3,attempts=attempts+1 WHERE delivery_id=?1", params![delivery,token.to_string(),until]).map_err(sql_error)?;
            Ok(EventDelivery { delivery_id: parse_uuid(&delivery)?, subscription_id: parse_uuid(&subscription)?, event: self.get(parse_uuid(&event)?)?.ok_or_else(|| MCSError::MemoryError("delivery event missing".into()))?, lease: Lease { token,epoch:epoch+1,until_us:until }, attempts:attempts+1 })
        }).transpose()?;
        tx.commit()?;
        Ok(result)
    }

    pub fn complete(&self, delivery: &EventDelivery, now: i64) -> Result<bool> {
        let tx = TxGuard::begin(self.conn)?;
        let changed = self.conn.execute("UPDATE event_outbox SET state='done' WHERE delivery_id=?1 AND lease_token=?2 AND lease_epoch=?3 AND (state='done' OR (state='leased' AND lease_until_us>?4))", params![delivery.delivery_id.to_string(),delivery.lease.token.to_string(),delivery.lease.epoch,now]).map_err(sql_error)?;
        tx.commit()?;
        Ok(changed == 1)
    }

    pub fn retry(
        &self,
        delivery: &EventDelivery,
        now: i64,
        next_attempt_us: i64,
        error: &str,
        dead: bool,
    ) -> Result<bool> {
        let tx = TxGuard::begin(self.conn)?;
        let error: String = error.chars().take(2048).collect();
        let changed = self.conn.execute("UPDATE event_outbox SET state=?5,next_attempt_us=?6,last_error=?7 WHERE delivery_id=?1 AND lease_token=?2 AND lease_epoch=?3 AND state='leased' AND lease_until_us>?4", params![delivery.delivery_id.to_string(),delivery.lease.token.to_string(),delivery.lease.epoch,now,if dead {"dead"} else {"pending"},next_attempt_us,error]).map_err(sql_error)?;
        tx.commit()?;
        Ok(changed == 1)
    }
}

pub(crate) fn parse_uuid(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value)
        .map_err(|error| MCSError::MemoryError(format!("invalid persisted UUID: {error}")))
}
