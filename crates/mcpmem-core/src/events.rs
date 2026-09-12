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
pub const MIGRATIONS: [(i64, &str); 7] = [
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
) -> Result<()> {
    for change in &changes.changes {
        let snapshot = change
            .after
            .as_ref()
            .or(change.before.as_ref())
            .ok_or_else(|| MCSError::MemoryError("empty entity change".into()))?;
        let deleted = change.after.is_none();
        let revision: i64 = conn.query_row("INSERT INTO entity_revision VALUES(?1,1,?2) ON CONFLICT(entity_id) DO UPDATE SET revision=revision+1, deleted=excluded.deleted RETURNING revision", params![snapshot.entity_id, deleted], |r| r.get(0)).map_err(sql_error)?;
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
        crate::jobs::enqueue_change(conn, snapshot.entity_id, revision, deleted)?;
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
