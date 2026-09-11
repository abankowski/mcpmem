//! Runtime principals and the approval waitlist, stored in the server's
//! SQLite database. Built-in entries from the principals file win on a
//! key collision; `oauth_routes` applies that rule. This store holds
//! everything else.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, params};

use crate::errors::{MCSError, Result};

/// The fixed size of the approval waitlist. The eviction order is by
/// activity: least-recently-seen first.
pub const WAITLIST_CAP: i64 = 25;

/// A runtime principal, stored in the server's database rather than in
/// the principals file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePrincipal {
    pub iss: String,
    pub sub: String,
    pub name: String,
    pub label: Option<String>,
    pub scopes: Vec<String>,
    pub created_us: i64,
    pub updated_us: i64,
}

/// A rejected would-be user awaiting admin approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitlistEntry {
    pub iss: String,
    pub sub: String,
    pub name: String,
    pub first_seen_us: i64,
    pub last_seen_us: i64,
}

/// SQLite-backed storage for runtime principals and the waitlist.
pub struct PrincipalsStore {
    conn: Connection,
    now_us: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl PrincipalsStore {
    /// Open the store against the server's SQLite file. The schema must
    /// already be migrated (build the graph first, which runs
    /// `initialize_database`), exactly as the OAuth store requires.
    pub fn open(db_path: &str, busy_timeout_ms: u64) -> Result<Self> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        Self::open_with_clock(db_path, busy_timeout_ms, Arc::new(move || now))
    }

    /// Test seam: the clock is injected, so TTL and eviction are
    /// deterministic.
    pub fn open_with_clock(
        db_path: &str,
        busy_timeout_ms: u64,
        now_us: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Result<Self> {
        let conn = Connection::open(db_path)
            .map_err(|e| MCSError::MemoryError(format!("failed to open the principals store: {e}")))?;
        conn.busy_timeout(std::time::Duration::from_millis(busy_timeout_ms))
            .map_err(|e| MCSError::MemoryError(format!("principals store busy_timeout: {e}")))?;
        // Admin writes are access-control changes. Never lose one to a
        // crash, whatever the graph's durability setting says.
        conn.execute_batch("PRAGMA synchronous = FULL")
            .map_err(|e| MCSError::MemoryError(format!("principals store pragma: {e}")))?;
        Ok(Self { conn, now_us })
    }

    /// Insert a runtime principal. Fails on a key collision with an
    /// existing runtime principal.
    pub fn create(&self, iss: &str, sub: &str, name: &str, label: Option<&str>, scopes: &[String]) -> Result<()> {
        let now = (self.now_us)();
        self.conn
            .execute(
                "INSERT INTO runtime_principal(iss, sub, name, label, scopes, created_us, updated_us)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![iss, sub, name, label, serde_json::to_string(scopes).map_err(|e| MCSError::JsonError(e))?, now],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    /// Update name/label/scopes. Returns false when no row carries the key.
    pub fn update(&self, iss: &str, sub: &str, name: &str, label: Option<&str>, scopes: &[String]) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "UPDATE runtime_principal
                 SET name = ?3, label = ?4, scopes = ?5, updated_us = ?6
                 WHERE iss = ?1 AND sub = ?2",
                params![iss, sub, name, label, serde_json::to_string(scopes).map_err(|e| MCSError::JsonError(e))?, (self.now_us)()],
            )
            .map_err(sql_error)?;
        Ok(changed > 0)
    }

    /// Delete a runtime principal. Returns false when no row carries the key.
    pub fn delete(&self, iss: &str, sub: &str) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "DELETE FROM runtime_principal WHERE iss = ?1 AND sub = ?2",
                params![iss, sub],
            )
            .map_err(sql_error)?;
        Ok(changed > 0)
    }

    /// Read one runtime principal by key. Returns None when absent.
    pub fn get(&self, iss: &str, sub: &str) -> Result<Option<RuntimePrincipal>> {
        self.conn
            .query_row(
                "SELECT iss, sub, name, label, scopes, created_us, updated_us
                 FROM runtime_principal WHERE iss = ?1 AND sub = ?2",
                params![iss, sub],
                row_to_principal,
            )
            .optional()
            .map_err(sql_error)
    }

    /// Read every runtime principal, in table order.
    pub fn list(&self) -> Result<Vec<RuntimePrincipal>> {
        let mut stmt = self
            .conn
            .prepare("SELECT iss, sub, name, label, scopes, created_us, updated_us FROM runtime_principal")
            .map_err(sql_error)?;
        let rows = stmt
            .query_map([], row_to_principal)
            .map_err(sql_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_error)
    }

    /// Record a rejected would-be user. A repeat sighting refreshes the
    /// name and `last_seen_us` but not `first_seen_us`, so the TTL is
    /// fixed from the first attempt. Then sweep: expired rows first, then
    /// the cap, LRU by activity.
    pub fn record_waitlist(&self, iss: &str, sub: &str, name: &str, ttl_us: i64) -> Result<()> {
        let now = (self.now_us)();
        self.conn
            .execute(
                "INSERT INTO principal_waitlist(iss, sub, name, first_seen_us, last_seen_us)
                 VALUES(?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(iss, sub) DO UPDATE SET name = excluded.name, last_seen_us = excluded.last_seen_us",
                params![iss, sub, name, now],
            )
            .map_err(sql_error)?;
        self.evict(ttl_us)
    }

    /// Read one waitlist entry by key. Returns None when absent.
    pub fn waitlist_get(&self, iss: &str, sub: &str) -> Result<Option<WaitlistEntry>> {
        self.conn
            .query_row(
                "SELECT iss, sub, name, first_seen_us, last_seen_us
                 FROM principal_waitlist WHERE iss = ?1 AND sub = ?2",
                params![iss, sub],
                |r| {
                    Ok(WaitlistEntry {
                        iss: r.get(0)?,
                        sub: r.get(1)?,
                        name: r.get(2)?,
                        first_seen_us: r.get(3)?,
                        last_seen_us: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(sql_error)
    }

    /// Read every waitlist entry, in table order.
    pub fn waitlist(&self) -> Result<Vec<WaitlistEntry>> {
        let mut stmt = self
            .conn
            .prepare("SELECT iss, sub, name, first_seen_us, last_seen_us FROM principal_waitlist")
            .map_err(sql_error)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(WaitlistEntry {
                    iss: r.get(0)?,
                    sub: r.get(1)?,
                    name: r.get(2)?,
                    first_seen_us: r.get(3)?,
                    last_seen_us: r.get(4)?,
                })
            })
            .map_err(sql_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_error)
    }

    /// Dismiss a waitlist entry without promoting it. Returns false when
    /// no row carries the key.
    pub fn dismiss_waitlist(&self, iss: &str, sub: &str) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "DELETE FROM principal_waitlist WHERE iss = ?1 AND sub = ?2",
                params![iss, sub],
            )
            .map_err(sql_error)?;
        Ok(changed > 0)
    }

    /// Promote a waitlist entry to a runtime principal in one transaction.
    /// Returns None when the entry does not exist. The name comes from the
    /// entry; the admin-supplied scopes must already be canonical and
    /// non-empty.
    pub fn approve(&self, iss: &str, sub: &str, scopes: &[String]) -> Result<Option<RuntimePrincipal>> {
        let now = (self.now_us)();
        let tx = self.conn.unchecked_transaction().map_err(sql_error)?;
        let name: Option<String> = tx
            .query_row(
                "SELECT name FROM principal_waitlist WHERE iss = ?1 AND sub = ?2",
                params![iss, sub],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        let Some(name) = name else {
            return Ok(None);
        };
        tx.execute(
            "INSERT INTO runtime_principal(iss, sub, name, label, scopes, created_us, updated_us)
             VALUES(?1, ?2, ?3, NULL, ?4, ?5, ?5)",
            params![iss, sub, name, serde_json::to_string(scopes).map_err(|e| MCSError::JsonError(e))?, now],
        )
        .map_err(sql_error)?;
        tx.execute(
            "DELETE FROM principal_waitlist WHERE iss = ?1 AND sub = ?2",
            params![iss, sub],
        )
        .map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        Ok(Some(RuntimePrincipal {
            iss: iss.to_owned(),
            sub: sub.to_owned(),
            name,
            label: None,
            scopes: scopes.to_vec(),
            created_us: now,
            updated_us: now,
        }))
    }

    /// Expire rows older than `ttl_us` (fixed from first_seen), then trim
    /// to [`WAITLIST_CAP`], evicting least-recently-seen first.
    fn evict(&self, ttl_us: i64) -> Result<()> {
        let now = (self.now_us)();
        let tx = self.conn.unchecked_transaction().map_err(sql_error)?;
        tx.execute(
            "DELETE FROM principal_waitlist WHERE first_seen_us < ?1",
            params![now.saturating_sub(ttl_us)],
        )
        .map_err(sql_error)?;
        let count: i64 = tx
            .query_row("SELECT count(*) FROM principal_waitlist", [], |r| r.get(0))
            .map_err(sql_error)?;
        if count > WAITLIST_CAP {
            let excess = count - WAITLIST_CAP;
            tx.execute(
                "DELETE FROM principal_waitlist WHERE (iss, sub) IN (
                     SELECT iss, sub FROM principal_waitlist
                     ORDER BY last_seen_us ASC, first_seen_us ASC
                     LIMIT ?1)",
                params![excess],
            )
            .map_err(sql_error)?;
        }
        tx.commit().map_err(sql_error)
    }
}

fn sql_error(e: rusqlite::Error) -> MCSError {
    MCSError::MemoryError(format!("principals store: {e}"))
}

fn row_to_principal(r: &rusqlite::Row<'_>) -> rusqlite::Result<RuntimePrincipal> {
    let scopes_json: String = r.get(4)?;
    let scopes = serde_json::from_str(&scopes_json).unwrap_or_default();
    Ok(RuntimePrincipal {
        iss: r.get(0)?,
        sub: r.get(1)?,
        name: r.get(2)?,
        label: r.get(3)?,
        scopes,
        created_us: r.get(5)?,
        updated_us: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    fn moved_clock() -> (Arc<dyn Fn() -> i64 + Send + Sync>, Arc<Mutex<i64>>) {
        let t = Arc::new(Mutex::new(1_000_000i64));
        let clock: Arc<dyn Fn() -> i64 + Send + Sync> = {
            let t = t.clone();
            Arc::new(move || *t.lock())
        };
        (clock, t)
    }

    fn store_at(now: Arc<dyn Fn() -> i64 + Send + Sync>) -> (PrincipalsStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.mcpmem");
        let conn = rusqlite::Connection::open(&path).unwrap();
        let (_, sql) = mcpmem_core::events::MIGRATIONS
            .iter()
            .find(|(v, _)| *v == 5)
            .expect("migration 5 exists");
        conn.execute_batch(sql).unwrap();
        drop(conn);
        (PrincipalsStore::open_with_clock(path.to_str().unwrap(), 5000, now).unwrap(), dir)
    }

    fn scopes() -> Vec<String> {
        crate::principals::canonical_scopes(&["graph-read".to_owned()]).unwrap()
    }

    #[test]
    fn create_update_get_delete_round_trip() {
        let (clock, t) = moved_clock();
        let (store, _dir) = store_at(clock);
        store.create("iss", "sub", "ada", None, &scopes()).unwrap();
        let got = store.get("iss", "sub").unwrap().unwrap();
        assert_eq!(got.name, "ada");
        assert_eq!(got.created_us, 1_000_000);
        *t.lock() = 2_000_000;
        store.update("iss", "sub", "ada l", Some("label"), &scopes()).unwrap();
        let got = store.get("iss", "sub").unwrap().unwrap();
        assert_eq!(got.label.as_deref(), Some("label"));
        assert_eq!(got.updated_us, 2_000_000);
        assert!(store.delete("iss", "sub").unwrap());
        assert!(!store.delete("iss", "sub").unwrap());
        assert!(store.get("iss", "sub").unwrap().is_none());
    }

    #[test]
    fn waitlist_upsert_refreshes_last_seen_but_not_first_seen() {
        let (clock, t) = moved_clock();
        let (store, _dir) = store_at(clock);
        store.record_waitlist("iss", "sub", "ada", 86_400_000_000).unwrap();
        *t.lock() = 2_000_000;
        store.record_waitlist("iss", "sub", "ada2", 86_400_000_000).unwrap();
        let e = store.waitlist_get("iss", "sub").unwrap().unwrap();
        assert_eq!(e.first_seen_us, 1_000_000);
        assert_eq!(e.last_seen_us, 2_000_000);
        assert_eq!(e.name, "ada2");
    }

    #[test]
    fn expired_entries_evict_from_first_seen() {
        let (clock, t) = moved_clock();
        let (store, _dir) = store_at(clock);
        let ttl_us = 86_400_000_000;
        store.record_waitlist("iss", "a", "a", ttl_us).unwrap();
        *t.lock() += ttl_us + 1;
        // A second record runs the eviction sweep.
        store.record_waitlist("iss", "b", "b", ttl_us).unwrap();
        assert!(store.waitlist_get("iss", "a").unwrap().is_none());
        assert!(store.waitlist_get("iss", "b").unwrap().is_some());
    }

    #[test]
    fn waitlist_is_capped_at_25_evicting_least_recent() {
        let (clock, _t) = moved_clock();
        let (store, _dir) = store_at(clock);
        for i in 0..26 {
            let name = format!("u{i}");
            store.record_waitlist("iss", &name, &name, 86_400_000_000).unwrap();
        }
        let rows = store.waitlist().unwrap();
        assert_eq!(rows.len() as i64, WAITLIST_CAP);
        // u0 was first, so it is the oldest by last_seen and is gone.
        assert!(rows.iter().all(|r| r.sub != "u0"));
        assert!(rows.iter().any(|r| r.sub == "u25"));
    }

    #[test]
    fn approve_creates_a_principal_and_removes_the_entry() {
        let (clock, _t) = moved_clock();
        let (store, _dir) = store_at(clock);
        store.record_waitlist("iss", "sub", "ada", 86_400_000_000).unwrap();
        let scopes = scopes();
        let p = store.approve("iss", "sub", &scopes).unwrap().unwrap();
        assert_eq!(p.name, "ada");
        assert_eq!(p.label, None);
        assert!(store.waitlist_get("iss", "sub").unwrap().is_none());
        assert!(store.get("iss", "sub").unwrap().is_some());
        // Approving a nonexistent entry is a miss, not an error.
        assert!(store.approve("iss", "nope", &scopes).unwrap().is_none());
    }
}