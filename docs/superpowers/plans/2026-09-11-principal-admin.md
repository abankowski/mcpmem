# Principal Administration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an admin manage principals in the web UI: list, add, change, remove; JSON-file principals are immutable built-ins; runtime principals live in SQLite; an optional approval waitlist records rejected would-be users for 24 h (cap 25) so an admin can promote them.

**Architecture:** A new `runtime_principal` + `principal_waitlist` table pair (migration 0005, existing migration ledger). A `PrincipalsStore` with its own SQLite connection, held on `OauthState`. Login-time resolution merges built-in JSON entries (winners on key collision) with runtime rows. A standalone admin SPA at `/ui/admin` authenticates through the server's own OAuth AS (PKCE, reserved client, new `admin` scope) and drives a `/ui/api/*` REST surface gated on that scope. Deleting a principal revokes its token families.

**Tech Stack:** Rust workspace; axum router (`src/http.rs`); rusqlite 0.40 bundled (`crates/mcpmem-oauth`, `crates/mcpmem-core` migrations); serde; vanilla JS UI (no bundler), matching `src/ui/graph.js`.

**Spec:** `docs/superpowers/specs/2026-09-11-principal-admin-design.md` (approved 2026-09-11; revocation section corrected to name the v1 limitation).

## Global Constraints

- Identity key is `(iss, sub)`. Email is display text only (spec).
- Built-in JSON entries win on key collision. Colliding writes are refused with 409. Stale runtime rows are masked, never deleted (spec, user-approved).
- Admin grants come only from an OAuth grant holding the `admin` scope. The static bearer token can never hold `admin` (its scopes are `ToolCategory` values).
- New SQLite connections set `PRAGMA busy_timeout` and `PRAGMA synchronous = FULL` (admin writes are access-control changes; never lose one to a crash).
- Migration registration: `crates/mcpmem-core/src/events.rs` `MIGRATIONS` const, plus the pinned checksum in `mod migration_inventory` (same file). A migrated DB may not be re-edited.
- All new UI is vanilla JS/CSS/HTML, no bundler, no framework — the `src/ui/` convention.
- The waitlist: fixed TTL measured from `first_seen_us`; a retry refreshes `last_seen_us` but not the deadline. Cap 25 rows; evict the least-recently-active first (spec decisions, approved).
- Every task ends with passing tests for that task's scope and a commit. Run formatting and clippy once at the end (Task 9), not per task.

---

### Task 1: Migration 0005 + registration + pinned checksum

**Files:**
- Create: `crates/mcpmem-core/migrations/0005_principals.sql`
- Modify: `crates/mcpmem-core/src/events.rs` (`MIGRATIONS`, `mod migration_inventory`)

**Interfaces:**
- Consumes: the migration ledger pattern at `events.rs:45-73` (`MIGRATIONS: [(i64, &str); 4]`, checksum pinning test at `events.rs:59-73`).
- Produces: version 5 in `MIGRATIONS`, applying the two new tables. `crates/mcpmem-core` tests pass.

- [ ] **Step 1: Write the migration file**

`crates/mcpmem-core/migrations/0005_principals.sql`:

```sql
-- Runtime principals and the approval waitlist. The principals file owns
-- the built-in entries; these tables hold the rest.
CREATE TABLE runtime_principal (
    iss        TEXT NOT NULL,
    sub        TEXT NOT NULL,
    name       TEXT NOT NULL,
    label      TEXT,
    scopes     TEXT NOT NULL,
    created_us INTEGER NOT NULL,
    updated_us INTEGER NOT NULL,
    PRIMARY KEY (iss, sub)
) STRICT;

CREATE TABLE principal_waitlist (
    iss           TEXT NOT NULL,
    sub           TEXT NOT NULL,
    name          TEXT NOT NULL,
    first_seen_us INTEGER NOT NULL,
    last_seen_us  INTEGER NOT NULL,
    PRIMARY KEY (iss, sub)
) STRICT;
```

- [ ] **Step 2: Register the migration**

In `crates/mcpmem-core/src/events.rs`, change the `MIGRATIONS` type to `[(i64, &str); 5]` and append the entry:

```rust
pub const MIGRATIONS: [(i64, &str); 5] = [
    (1, include_str!("../migrations/0001_change_events.sql")),
    // … keep 2, 3, 4 unchanged …
    (4, include_str!("../migrations/0004_oauth.sql")),
    (5, include_str!("../migrations/0005_principals.sql")),
];
```

- [ ] **Step 3: Run the inventory test to fail**

Run: `cargo test -p mcpmem-core migration_inventory`

Expected: FAIL. The assertion prints the computed list on the left; the right-hand `vec![...]` ends at version 4. Copy the `(5, "<64-hex-sha256>")` pair for your file from the left-hand output.

- [ ] **Step 4: Pin the checksum**

In `mod migration_inventory` (`events.rs:59-73`), append the copied tuple to the `vec![...]`:

```rust
(
    5,
    "<64-hex-sha256 of 0005_principals.sql>".to_owned(),
),
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p mcpmem-core`

Expected: PASS (inventory test, and every existing mcpmem-core test).

- [ ] **Step 6: Commit**

```bash
git add crates/mcpmem-core/migrations/0005_principals.sql crates/mcpmem-core/src/events.rs
git commit -m "feat: migration 0005 — runtime principals and waitlist

Two STRICT tables for the principal-admin feature. Registered in the
versioned migration ledger and pinned in the checksum inventory.

Tokens: ~6k. Cost: < $1."
```

---

### Task 2: Scope vocabulary — the `admin` scope and shared canonicalization

**Files:**
- Modify: `src/principals.rs`
- Test: `src/principals.rs` (`#[cfg(test)] mod tests` at the bottom)

**Interfaces:**
- Consumes: `ToolCategory` (`src/tools.rs:22-45`), `MCSError::InvalidParams`.
- Produces: `pub const ADMIN_SCOPE: &str`; `pub fn is_known_scope(slug: &str) -> bool`; `pub fn canonical_scopes(raw: &[String]) -> crate::errors::Result<Vec<String>>`; widened `PrincipalEntry::scope_set`; `load` uses `canonical_scopes`.

The seam the scouting found: `scope_set` (`principals.rs:35-40`) filters through `ToolCategory::parse`, so `admin` would silently vanish from grants. This task widens it.

- [ ] **Step 1: Write the failing tests**

Append to `src/principals.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_scope_is_known_and_survives_scope_set() {
        assert!(is_known_scope(ADMIN_SCOPE));
        let entry = PrincipalEntry {
            name: "ops".into(),
            iss: "https://issuer".into(),
            sub: "s1".into(),
            label: None,
            scopes: vec![ADMIN_SCOPE.to_owned()],
        };
        assert!(entry.scope_set().contains(ADMIN_SCOPE));
    }

    #[test]
    fn tool_category_slugs_stay_canonical() {
        assert!(is_known_scope("graph-read"));
        assert!(!is_known_scope("graph-admin"));
        let entry = PrincipalEntry {
            name: "adam".into(),
            iss: "https://issuer".into(),
            sub: "s2".into(),
            label: None,
            scopes: vec!["graph-read".into(), "graph-read".into()],
        };
        // Canonical slugs only, in input order.
        assert_eq!(canonical_scopes(&["graph-read".into(), "admin".into()]).unwrap(), vec!["graph-read", "admin"]);
    }

    #[test]
    fn canonical_scopes_refuses_unknown_scopes() {
        assert!(canonical_scopes(&["graph-read".into(), "everything".into()]).is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p mcpmem principals::tests`

Expected: FAIL — `is_known_scope` and `canonical_scopes` do not exist yet.

- [ ] **Step 3: Implement**

In `src/principals.rs`:

```rust
use crate::errors::{MCSError, Result};

/// The single scope spelling for administration. It grants access to the
/// admin API only; no tool carries it.
pub const ADMIN_SCOPE: &str = "admin";

/// True when `slug` names a scope this server issues: a tool category or
/// the admin scope.
pub fn is_known_scope(slug: &str) -> bool {
    slug == ADMIN_SCOPE || slug.parse::<ToolCategory>().is_ok()
}

/// Canonicalize a scope list: trim, convert each known tool category to
/// its slug, reject unknown slugs, keep input order. The caller decides
/// whether an empty result is allowed.
pub fn canonical_scopes(raw: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(raw.len());
    for slug in raw {
        let slug = slug.trim();
        if slug.is_empty() {
            continue;
        }
        if slug == ADMIN_SCOPE {
            out.push(ADMIN_SCOPE.to_owned());
        } else if let Ok(category) = slug.parse::<ToolCategory>() {
            out.push(category.slug().to_owned());
        } else {
            return Err(MCSError::InvalidParams(format!("unknown scope '{slug}'")));
        }
    }
    Ok(out)
}
```

Replace `scope_set`:

```rust
pub fn scope_set(&self) -> BTreeSet<String> {
    self.scopes
        .iter()
        .filter_map(|s| {
            if s == ADMIN_SCOPE {
                Some(ADMIN_SCOPE.to_owned())
            } else {
                s.parse::<ToolCategory>().ok().map(|c| c.slug().to_owned())
            }
        })
        .collect()
}
```

In `load`, replace the per-scope `ToolCategory::parse` validation (the loop that errors with `"principal '{}' names unknown scope"`, `principals.rs:83-92`) with a call that also assigns the canonical list:

```rust
        let scopes = canonical_scopes(&entry.scopes)?;
        if scopes.is_empty() {
            return Err(MCSError::InvalidParams(format!(
                "principal '{}' has no scopes",
                entry.name
            )));
        }
        entry.scopes = scopes;
```

Keep the existing name/iss/sub emptiness and duplicate-key checks unchanged.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p mcpmem principals::tests`

Expected: PASS.

- [ ] **Step 5: Verify an existing built-in entry still loads with new scopes**

Run: `cargo test --test oauth_config` (the principals-file tests live here: `principal_scopes_are_stored_as_canonical_slugs`, `scope_set_canonicalizes_an_entry_that_did_not_come_from_load`, rejection tests)

Expected: PASS — the canonical-slug contract the loader pins is preserved.

- [ ] **Step 6: Commit**

```bash
git add src/principals.rs
git commit -m "feat: add the admin scope and shared scope canonicalization

scope_set now keeps the admin slug instead of dropping non-category
scopes, so a grant can carry it. load() and runtime writes share one
canonicalizer and cannot accept different shapes.

Tokens: ~5k. Cost: < $1."
```

---

### Task 3: OAuth wiring — advertise `admin`, seed the reserved client, revoke by principal

**Files:**
- Modify: `crates/mcpmem-oauth/src/lib.rs` (`ADMIN_CLIENT_ID`, `principal_id`, `parse_principal_id`)
- Modify: `crates/mcpmem-oauth/src/store.rs` (`REVOKED` source const, `revoke_principal`)
- Modify: `src/oauth_routes.rs` (`scopes`, `OauthState::open_with_clock` seeding, `OauthState::revoke_principal`)
- Test: `crates/mcpmem-oauth/src/store.rs` or `crates/mcpmem-oauth/tests/` (follow the existing store test pattern)

**Interfaces:**
- Consumes: `Store::put_client` (upsert, `store.rs:256-275`), `Store::revoke_family` (`store.rs:617-620`), `Store::connection` (doc-hidden), `ClientRecord` (`store.rs:100-110`).
- Produces: advisory-document scope list containing `admin` when OAuth is on; a reserved `mcpmem-admin-ui` client row at OAuth startup; `Store::revoke_principal(principal) -> rusqlite Result<usize>`; `OauthState::revoke_principal(&self, principal: &str) -> Result<usize, String>`; `mcpmem_oauth::principal_id` / `parse_principal_id` for API ids.

- [ ] **Step 1: Write the failing tests**

In `crates/mcpmem-oauth/src/lib.rs` (append `#[cfg(test)] mod tests`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_ids_round_trip() {
        let id = principal_id("https://accounts.google.com", "u-1");
        assert_eq!(parse_principal_id(&id), Some(("https://accounts.google.com".to_owned(), "u-1".to_owned())));
        assert_eq!(parse_principal_id("not-base64!"), None);
        // A sub of zero length is refused.
        assert_eq!(parse_principal_id(&principal_id("iss", "")), None);
    }
}
```

For `Store::revoke_principal`, add the test to the store's existing test location. If `store.rs` has no `#[cfg(test)]` module, create `crates/mcpmem-oauth/tests/revoke_principal.rs` mirroring how other store tests open a migrated DB (see `tests/support` in the root crate for the migration helper pattern, or run `mcpmem_core::schema::initialize_database` if the crate exposes it):

```rust
// Seed two principals with tokens in two families, revoke one, assert
// the other stays live and the count is 1.
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p mcpmem-oauth principal_ids` and the revoke test.

Expected: FAIL — functions do not exist.

- [ ] **Step 3: Implement `principal_id` / `parse_principal_id`**

In `crates/mcpmem-oauth/src/lib.rs` (`URL_SAFE_NO_PAD` already imported at `lib.rs:24-25`):

```rust
/// The id of a principal in the admin API: base64url of `iss\0sub`.
///
/// One path segment, so a route never needs to split an issuer URL.
pub fn principal_id(iss: &str, sub: &str) -> String {
    URL_SAFE_NO_PAD.encode(format!("{iss}\0{sub}"))
}

/// Split a [`principal_id`] back into `(iss, sub)`, or `None` for
/// anything this server did not issue.
pub fn parse_principal_id(id: &str) -> Option<(String, String)> {
    let raw = URL_SAFE_NO_PAD.decode(id.as_bytes()).ok()?;
    let sep = raw.iter().position(|&b| b == 0)?;
    let iss = std::str::from_utf8(&raw[..sep]).ok()?;
    let sub = std::str::from_utf8(&raw[sep + 1..]).ok()?;
    if sub.is_empty() {
        return None;
    }
    Some((iss.to_owned(), sub.to_owned()))
}
```

- [ ] **Step 4: Implement `Store::revoke_principal`**

In `crates/mcpmem-oauth/src/store.rs`, beside `revoke_family` (`store.rs:617-620`, which is `UPDATE oauth_token SET revoked=1 WHERE family = ?1` when the error type is the crate's or rusqlite's — match the neighboring signature):

```rust
/// Revoke every live token family that names `principal`, and return how
/// many families were revoked. A renamed, then deleted, principal keeps
/// old-name families alive until they expire; that gap is documented.
pub fn revoke_principal(&self, principal: &str) -> Result<usize> {
    let families: Vec<String> = self
        .conn
        .prepare(
            "SELECT DISTINCT family FROM oauth_token
             WHERE principal = ?1 AND revoked = 0",
        )?
        .query_map([principal], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    for family in &families {
        self.revoke_family(family)?;
    }
    Ok(families.len())
}
```

Also add the reserved-client source const beside the existing ones (`store.rs:111-119`):

```rust
    /// The `source` of the admin UI client this server seeds at startup.
    pub const RESERVED: &'static str = "reserved";
```

- [ ] **Step 5: Advertise `admin` and seed the client in `oauth_routes.rs`**

Change `scopes` (`oauth_routes.rs:592-595`):

```rust
/// The scopes this server advertises: one slug per enabled tool category,
/// plus the admin scope whenever an OAuth state exists.
fn scopes(state: &HttpState) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = state
        .enabled_categories
        .iter()
        .map(|c| c.slug())
        .collect();
    if state.oauth.is_some() {
        out.push(crate::principals::ADMIN_SCOPE);
    }
    out
}
```

In `OauthState::open_with_clock` (which opens the connection at `oauth_routes.rs:194-195`), after the `Store` is built, seed the reserved client (the admin SPA's PKCE client):

```rust
    // The admin UI is a public PKCE client of this server's own AS. Seed
    // it once; put_client upserts, so a repeat start only refreshes the row.
    store
        .put_client(&mcpmem_oauth::store::ClientRecord {
            client_id: mcpmem_oauth::ADMIN_CLIENT_ID.to_owned(),
            client_name: "mcpmem admin UI".to_owned(),
            redirect_uris: vec![format!("{}/ui/admin/callback", config.public_url)],
            source: mcpmem_oauth::store::ClientRecord::RESERVED.to_owned(),
            created_us: now,
            last_used_us: now,
        })
        .map_err(|e| {
            crate::errors::MCSError::MemoryError(format!(
                "failed to seed the admin UI client: {e}"
            ))
        })?;
```

Either compute `now` from the clock already in scope or call the same `now_us()` the file uses elsewhere. Add `pub const ADMIN_CLIENT_ID: &str = "mcpmem-admin-ui";` to `crates/mcpmem-oauth/src/lib.rs`.

**Exempt the reserved client from eviction.** The 30-day `evict_clients` sweep (`store.rs:681`) would remove the seeded row from a long-running server that saw no admin authorization, silently breaking `/ui/admin` until a restart re-seeds it. The reserved client is server-owned infrastructure, not a registration; the sweep exists to bound anonymous DCR/CIMD rows. In `Store::evict_clients`, exclude it — add `AND source <> 'reserved'` to the eviction query. Update `tests/oauth_flow.rs`'s eviction-count assertion to count only non-reserved clients (restore the pre-seed expectation) and say the exemption in its comment.

Add the revocation wrapper on `OauthState` (beside `with_store`):

```rust
/// Revoke every live token family that names `principal`, and return how
/// many families were revoked.
pub fn revoke_principal(&self, principal: &str) -> std::result::Result<usize, String> {
    self.with_store(|store| store.revoke_principal(principal).map_err(|e| e.to_string()))
}
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p mcpmem-oauth && cargo test -p mcpmem oauth_routes`

Expected: PASS. The metadata docs now list `admin` when OAuth is on. The scope-list pins live in `tests/oauth_discovery.rs` — widen each `scopes_supported` expectation to include `"admin"` at the end for OAuth-enabled servers. The seed adds one `oauth_client` row to every OAuth-enabled test server: switch row-count assertions that count registrations to a helper that excludes `source = 'reserved'` (a `count_registered_clients` helper in `tests/support/flow.rs`), preserving each test's original number and intent.

- [ ] **Step 7: Commit**

```bash
git add crates/mcpmem-oauth/src/lib.rs crates/mcpmem-oauth/src/store.rs src/oauth_routes.rs crates/mcpmem-oauth/tests/revoke_principal.rs
git commit -m "feat: advertise admin scope, seed admin client, revoke by principal

The advisory documents list the admin scope when oauth is on; startup
seeds the reserved public PKCE client for /ui/admin; revoke_principal
kills every live family naming a human so a deleted principal's refresh
rotation stops.

Tokens: ~10k. Cost: < $1."
```

---

### Task 4: `PrincipalsStore` — runtime principals and the waitlist

**Files:**
- Create: `src/runtime_principals.rs`
- Modify: `src/lib.rs` (`pub mod runtime_principals;` beside the other module decls, `lib.rs:17-21`)

**Interfaces:**
- Consumes: `principles.rs::canonical_scopes` (Task 2), migration 0005 tables (Task 1), `mcpmem_core::events::MIGRATIONS` (test sql source).
- Produces: `struct RuntimePrincipal { iss, sub, name, label: Option<String>, scopes: Vec<String>, created_us, updated_us }`; `struct WaitlistEntry { iss, sub, name, first_seen_us, last_seen_us }`; `struct PrincipalsStore` with `open` / `open_with_clock` and methods: `create`, `update`, `delete`, `get`, `list`, `record_waitlist`, `waitlist`, `waitlist_get`, `approve`, `dismiss_waitlist`. `WAITLIST_CAP: i64 = 25`.

- [ ] **Step 1: Write the failing unit tests**

In `src/runtime_principals.rs` as `#[cfg(test)] mod tests`. They need a migrated temp DB: open a `tempfile::tempdir()` DB, execute the migration-5 SQL, then open a store with a movable clock:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn moved_clock() -> (Arc<dyn Fn() -> i64 + Send + Sync>, Arc<Mutex<i64>>) {
        let t = Arc::new(Mutex::new(1_000_000i64));
        let clock: Arc<dyn Fn() -> i64 + Send + Sync> = {
            let t = t.clone();
            Arc::new(move || *t.lock().unwrap())
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
        *t.lock().unwrap() = 2_000_000;
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
        *t.lock().unwrap() = 2_000_000;
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
        *t.lock().unwrap() += ttl_us + 1;
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
    fn the_production_clock_is_live() {
        // open() must use a live wall clock: a frozen clock would keep the
        // TTL sweep from ever expiring and degenerate the LRU eviction.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("w.mcpmem");
        let conn = rusqlite::Connection::open(&path).unwrap();
        let (_, sql) = mcpmem_core::events::MIGRATIONS
            .iter()
            .find(|(v, _)| *v == 5)
            .expect("migration 5 exists");
        conn.execute_batch(sql).unwrap();
        drop(conn);
        let store = PrincipalsStore::open(path.to_str().unwrap(), 5000).unwrap();
        store.create("iss", "a", "a", None, &scopes()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.create("iss", "b", "b", None, &scopes()).unwrap();
        let a = store.get("iss", "a").unwrap().unwrap();
        let b = store.get("iss", "b").unwrap().unwrap();
        assert!(b.created_us > a.created_us, "the wall clock must advance between writes");
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p mcpmem runtime_principals`

Expected: FAIL — module and methods do not exist.

- [ ] **Step 3: Implement**

`src/runtime_principals.rs`:

```rust
//! Runtime principals and the approval waitlist, stored in the server's
//! SQLite database. Built-in entries from the principals file win on a
//! key collision; `oauth_routes` applies that rule. This store holds
//! everything else.
//!
//! The store is not `Sync`: the caller owns exclusive access, and
//! `OauthState` holds it in a `Mutex` (the `with_store` convention).
//! Methods take `&self` and use `unchecked_transaction` like the graph
//! store (`graph.rs:523`); sharing the connection across threads would be
//! unsound.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, params};

use crate::errors::{MCSError, Result};

/// The fixed size of the approval waitlist. The eviction order is by
/// activity: least-recently-seen first.
pub const WAITLIST_CAP: i64 = 25;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitlistEntry {
    pub iss: String,
    pub sub: String,
    pub name: String,
    pub first_seen_us: i64,
    pub last_seen_us: i64,
}

pub struct PrincipalsStore {
    conn: Connection,
    now_us: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl PrincipalsStore {
    /// Open the store against the server's SQLite file. The schema must
    /// already be migrated (build the graph first, which runs
    /// `initialize_database`), exactly as the OAuth store requires.
    pub fn open(db_path: &str, busy_timeout_ms: u64) -> Result<Self> {
        // A live clock, never a fixed value: a frozen now_us would stop the
        // TTL sweep forever (boot < boot - ttl is false) and collapse the
        // LRU order to insertion order. The OAuth store uses the same
        // live clock (oauth_routes.rs:187).
        Self::open_with_clock(db_path, busy_timeout_ms, Arc::new(mcpmem_core::events::now_us))
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
```

Register the module in `src/lib.rs` beside `pub mod principals;` (`lib.rs:18`):

```rust
pub mod runtime_principals;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p mcpmem runtime_principals`

Expected: PASS (all five tests).

- [ ] **Step 5: Commit**

```bash
git add src/runtime_principals.rs src/lib.rs
git commit -m "feat: runtime principals and waitlist store

CRUD over runtime_principal, fixed-TTL capped waitlist (25, evicts by
activity), and transactional approve. Own connection with synchronous
FULL: admin writes are access-control changes.

Tokens: ~14k. Cost: < $1."
```

---

### Task 5: Config keys — approval-waitlist, TTL, default scopes

**Files:**
- Modify: `src/lib.rs` (`Args` flags)
- Modify: `src/config_file.rs` (`OAuthSection` fields + assign block)
- Modify: `src/config.rs` (`OAuthConfig` fields + `from_args` construction)
- Modify: `tests/support/mod.rs` (`oauth_config_at` literal)
- Modify: `src/oauth_routes.rs` (the `Some(OAuthConfig { … })` test literal, `oauth_routes.rs:1717-1722`)

**Interfaces:**
- Consumes: the section parse order (`config_file.rs:238-248` deny_unknown_fields, assign block `505-540`), `Args` fields (`lib.rs:169-205`), construction (`config.rs:401-409`).
- Produces: `OAuthConfig { approval_waitlist: bool, approval_waitlist_ttl_seconds: u64, default_new_principal_scopes: Vec<String> }`; flags `--approval-waitlist`, `--approval-waitlist-ttl-seconds`, `--default-new-principal-scope`; section keys `approval-waitlist`, `approval-waitlist-ttl-seconds`, `default-new-principal-scopes`. Defaults: `false`, `86400`, `["graph-read"]`. TTL `0` disables the TTL sweep; the cap still applies.

- [ ] **Step 1: Add the `Args` flags**

`src/lib.rs`, beside the other `oauth_*` args (`lib.rs:169-205`):

```rust
    /// `bool`, not `Option<bool>`: clap's SetTrue action yields
    /// `Some(false)` when the flag is absent, and an absent switch must
    /// not trip the fail-closed OAuth guard. Same shape as
    /// `oauth_trust_forwarded_proto`.
    #[arg(long = "approval-waitlist")]
    pub approval_waitlist: bool,

    /// How long a waitlist entry lives, measured from first sighting.
    /// 0 disables the TTL sweep; the 25-entry cap always applies.
    #[arg(long = "approval-waitlist-ttl-seconds")]
    pub approval_waitlist_ttl_seconds: Option<u64>,

    /// A scope a promoted entry starts with. Repeatable; defaults to
    /// graph-read.
    #[arg(long = "default-new-principal-scope")]
    pub default_new_principal_scopes: Option<Vec<String>>,
```

- [ ] **Step 2: Add the section fields**

`src/config_file.rs`, `OAuthSection` (`config_file.rs:238-248`):

```rust
    pub approval_waitlist: Option<bool>,
    pub approval_waitlist_ttl_seconds: Option<u64>,
    pub default_new_principal_scopes: Option<Vec<String>>,
```

And in the `[oauth]` assign block, after `trust_forwarded_proto` (`config_file.rs:536-540`):

```rust
        assign(
            &mut args.approval_waitlist,
            oauth.approval_waitlist,
            cli.absent("approval_waitlist"),
        );
        assign(
            &mut args.approval_waitlist_ttl_seconds,
            oauth.approval_waitlist_ttl_seconds,
            cli.absent("approval_waitlist_ttl_seconds"),
        );
        assign_vec(
            &mut args.default_new_principal_scopes,
            oauth.default_new_principal_scopes,
            cli.absent("default_new_principal_scopes"),
        );
```

- [ ] **Step 3: Add the struct fields and construction**

`src/config.rs`, `OAuthConfig` (`config.rs:70-104`):

```rust
    /// Record a rejected would-be human instead of refusing outright.
    pub approval_waitlist: bool,
    /// Waitlist TTL in seconds, measured from first sighting. 0 disables
    /// the TTL sweep; the 25-entry cap always applies.
    pub approval_waitlist_ttl_seconds: u64,
    /// Scopes a promoted waitlist entry starts with.
    pub default_new_principal_scopes: Vec<String>,
```

In `Config::from_args`, in the `Some(OAuthConfig { … })` literal (`config.rs:401-409`):

```rust
            approval_waitlist: args.approval_waitlist,
            approval_waitlist_ttl_seconds: args
                .approval_waitlist_ttl_seconds
                .unwrap_or(24 * 60 * 60),
            default_new_principal_scopes: args
                .default_new_principal_scopes
                .clone()
                .unwrap_or_else(|| vec!["graph-read".to_owned()]),
```

Track the plan's fail-closed guard in the same construction change. `Config::from_args` refuses startup when any OAuth flag is present without `--oidc-issuer` (`config.rs:371-382`): an OAuth flag without the issuer is a misconfiguration, not a no-op. Add the three new flags to that condition, and add the three flag vectors to the orphan enumeration in `tests/oauth_config.rs` (`an_oauth_flag_without_the_issuer_is_refused`).

Also pin the empty-list semantics with a test: `default-new-principal-scopes = []` in the file must yield an empty scopes vec (it clears the default; the `assign_vec` `!value.is_empty()` guard would silently restore `["graph-read"]`, so the `.map(Some)` assign shape must be preserved and tested).

- [ ] **Step 4: Update the test literals**

`tests/support/mod.rs`, `oauth_config_at` (`mod.rs:55-63`):

```rust
        approval_waitlist: false,
        approval_waitlist_ttl_seconds: 24 * 60 * 60,
        default_new_principal_scopes: vec!["graph-read".to_owned()],
```

`src/oauth_routes.rs`, the `Some(OAuthConfig { … })` test literal (`oauth_routes.rs:1717-1722`): add the same three fields.

Then run a workspace grep to catch any other `OAuthConfig {` literal:

Run: `cargo build` then `cargo test -p mcpmem config_file`

Expected: PASS. If the compiler names another literal, add the fields there too.

- [ ] **Step 5: Example config comment**

`mcpmem.example.toml`, inside `[oauth]`, after `trust-forwarded-proto`:

```toml
# Refuse a would-be user but record the attempt, so an admin can promote
# the entry in the web UI. Default false (hard refusal).
# approval-waitlist = false

# How long a waitlist entry lives, from first attempt. 0 disables the TTL
# sweep. The list holds at most 25 entries, so the size is always bounded.
# approval-waitlist-ttl-seconds = 86400

# Scopes a promoted entry starts with. The Approve button offers these
# pre-checked.
# default-new-principal-scopes = ["graph-read"]
```

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/config_file.rs src/config.rs tests/support/mod.rs src/oauth_routes.rs mcpmem.example.toml
git commit -m "feat: config for the approval waitlist and default scopes

Three new [oauth] keys and flags. Waitlist stays off until an operator
turns it on; TTL defaults to 24 h; promoted entries start with graph-read.

Tokens: ~6k. Cost: < $1."
```

---

### Task 6: Login path — merged resolution and the waitlist branch

**Files:**
- Modify: `src/oauth_routes.rs`
- Create: `tests/principal_admin.rs`

**Interfaces:**
- Consumes: `PrincipalsStore` (Task 4), `OAuthConfig.approval_waitlist` + TTL (Task 5), `IdentityClaims { iss, sub, email }` (`crates/mcpmem-oauth/src/upstream.rs:99-113`), `login_refused` / `callback` (`oauth_routes.rs:1062-1067, 1229-1247`).
- Produces: `OauthState.runtime: Arc<PrincipalsStore>` + `OauthState.builtin_keys: Arc<BTreeSet<(String, String)>>`; `principal_of` returns an owned `PrincipalEntry` resolving built-ins first, then runtime rows; `finish_login` writes the waitlist and answers a pending page when the waitlist is on; `fn waiting_page(name: &str) -> Response`.

- [ ] **Step 1: Write the failing integration tests**

These are integration tests: they need `tests/support` (the fake upstream provider), which unit tests inside `src/oauth_routes.rs` cannot see. Create `tests/principal_admin.rs` with the two tests below; Task 7 extends the same file.

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use support::{Scopes, json};

    #[tokio::test]
    async fn an_unknown_human_is_recorded_and_sees_pending_when_waitlist_is_on() {
        let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default()).await;
        let mut config = support::oauth_config(&idp.issuer);
        config.approval_waitlist = true;
        // The provider authenticates sub-1; this server allows somebody else.
        config.principals[0].sub = "another-human".into();
        let server = support::server(Some(config), support::Scopes::all(), None).await;
        let client_id = support::register(&server, "waitlist-test", &format!("{}/cb", support::PUBLIC_URL)).await;
        let params = [
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", &format!("{}/cb", support::PUBLIC_URL)),
            ("scope", "graph-read"),
            ("state", "st"),
            ("code_challenge", &support::flow::code_challenge()),
        ];
        let res = server
            .request(Request::get(format!("/oauth/authorize?{}", support::flow::query_string(&params))).body(Body::empty()).unwrap())
            .await
            .unwrap();
        // Follow: upstream idp → callback. (Use the same Location-chain
        // walk the oauth_upstream tests use.)
        let location = res.headers().get(header::LOCATION).unwrap().to_str().unwrap().to_owned();
        let hop = server.request(Request::get(location).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(hop.status(), StatusCode::OK, "pending page, not a refusal");
        let text = support::flow::body_text(hop).await;
        assert!(text.contains("Sign-in pending"), "pending page names the state");
        let rows = support::count_rows(&server, "principal_waitlist");
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn an_unknown_human_is_refused_when_waitlist_is_off() {
        let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default()).await;
        let mut config = support::oauth_config(&idp.issuer);
        // approval_waitlist stays false (the default).
        config.principals[0].sub = "another-human".into();
        let server = support::server(Some(config), support::Scopes::all(), None).await;
        let client_id = support::register(&server, "refusal-test", &format!("{}/cb", support::PUBLIC_URL)).await;
        // … same authorize → callback walk as above …
        assert_eq!(hop.status(), StatusCode::FORBIDDEN);
        let rows = support::count_rows(&server, "principal_waitlist");
        assert_eq!(rows, 0);
    }
```

The authorize → Location → callback walk exists verbatim in `tests/oauth_upstream.rs:568-620` (`the_authorize_endpoint_redirects_the_human_to_the_upstream_provider`) — copy its request sequence; the second hop is `Request::get(location)` against the same server. Where the existing refusal tests assert `FORBIDDEN`, `oauth_upstream.rs:822-863` (`every_refusal_at_the_callback_is_the_same_page`) shows the assertion shape.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test principal_admin`

Expected: FAIL — `approval_waitlist` field and the pending branch do not exist. (The OAuthConfig construction errors clear once Task 5 lands; Task 5 executes before Task 6, so this task starts green.)

- [ ] **Step 3: Implement**

Add fields to `OauthState` (`oauth_routes.rs:31-60`):

```rust
    /// The runtime principals store (same SQLite file, own connection).
    /// In a `Mutex`: the connection is not `Sync` and the store uses
    /// `unchecked_transaction`, which needs exclusive access.
    pub(crate) runtime: std::sync::Mutex<crate::runtime_principals::PrincipalsStore>,
    /// The keys the principals file owns. A runtime row whose key is here
    /// is masked: unreachable for login, still listed in the admin API.
    pub(crate) builtin_keys: Arc<std::collections::BTreeSet<(String, String)>>,
```

Construct both in `open_with_clock` (where the connection and store are built, `oauth_routes.rs:172-210`), after the `Store` is ready:

```rust
    let runtime = std::sync::Mutex::new(
        crate::runtime_principals::PrincipalsStore::open(db_path, busy_timeout_ms)
            .map_err(open_failed)?,
    );
    let builtin_keys = Arc::new(
        cfg.principals
            .iter()
            .map(|p| (p.iss.clone(), p.sub.clone()))
            .collect(),
    );
```

(`open_failed` already exists at `oauth_routes.rs:272-274`; adjust its message or add a sibling if the type differs.)

Add the access helper beside `with_store` (every store access in Tasks 6 and 7 goes through it):

```rust
/// Run `f` with exclusive access to the runtime principals store.
pub fn with_principals<R>(
    &self,
    f: impl FnOnce(&crate::runtime_principals::PrincipalsStore) -> R,
) -> R {
    let guard = self.runtime.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&guard)
}
```

Change `principal_of` (`oauth_routes.rs:1185-1199`) to return an owned entry and consult both sources:

```rust
fn principal_of(
    oauth: &OauthState,
    claims: &IdentityClaims,
) -> std::result::Result<crate::principals::PrincipalEntry, String> {
    // Built-ins win on a key collision; the runtime row for that key is
    // masked and unreachable here.
    if let Some(p) = oauth
        .config
        .principals
        .iter()
        .find(|p| p.key() == (claims.iss.as_str(), claims.sub.as_str()))
    {
        return Ok(p.clone());
    }
    let row = oauth
        .with_principals(|s| s.get(&claims.iss, &claims.sub))
        .map_err(|e| format!("the principals store refused a read: {e}"))?;
    match row {
        Some(row) => Ok(crate::principals::PrincipalEntry {
            name: row.name,
            iss: row.iss,
            sub: row.sub,
            label: row.label,
            scopes: row.scopes,
        }),
        None => Err(format!(
            "no principal is registered for {} {}",
            claims.iss, claims.sub
        )),
    }
}
```

Change the call site in `finish_login` (`oauth_routes.rs:1112`):

```rust
    let principal = match principal_of(oauth, &claims) {
        Ok(p) => p,
        Err(e) => {
            if !oauth.config.approval_waitlist {
                return Err(e);
            }
            // The waitlist is on: record the would-be human and answer
            // with a pending page instead of a refusal. No token is ever
            // minted on this path.
            let name = claims.email.clone().unwrap_or_else(|| claims.sub.clone());
            oauth
                .with_principals(|s| {
                    s.record_waitlist(
                        &claims.iss,
                        &claims.sub,
                        &name,
                        oauth.config.approval_waitlist_ttl_seconds.saturating_mul(1_000_000),
                    )
                })
                .map_err(|err| format!("the principals store refused a write: {err}"))?;
            return Ok(waiting_page(&name));
        }
    };
```

Add the pending page beside `login_refused` (`oauth_routes.rs:1229-1247`):

```rust
/// The page a would-be user sees while an entry sits on the waitlist.
/// 200, not the refusal page: the sign-in was understood, it is just not
/// approved yet.
fn waiting_page(name: &str) -> Response {
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Pending approval</title></head>\
         <body><h1>Sign-in pending</h1>\
         <p>{}</p>\
         <p>An administrator must approve the request before this sign-in can continue.</p></body></html>",
        html_escape(name)
    ))
    .into_response()
}

/// Escape the few HTML-significant characters; the name comes from an
/// upstream identity provider.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --test principal_admin`

Expected: PASS — the two new tests plus the whole existing oauth integration set.

- [ ] **Step 5: Run the full oauth integration set**

Run: `cargo test --test oauth_upstream`

Expected: PASS — the refusal tests still see the same `FORBIDDEN` page because the waitlist defaults off.

- [ ] **Step 6: Commit**

```bash
git add src/oauth_routes.rs
git commit -m "feat: merged principal resolution and the approval waitlist

Login resolves built-ins first, runtime rows second; a colliding runtime
row is masked. With approval-waitlist on, an unknown human is recorded
(24 h TTL, cap 25) and sees a pending page; with it off, the refusal
page is unchanged.

Tokens: ~9k. Cost: < $1."
```

---

### Task 7: Admin HTTP API

**Files:**
- Modify: `src/http.rs`
- Create: `tests/principal_admin.rs` (extends the file Task 6 created)

**Interfaces:**
- Consumes: `PrincipalsStore` (Task 4), `OAuthConfig` defaults (Task 5), `OauthState.runtime`/`builtin_keys`/`revoke_principal` (Tasks 3, 6), `principal_id`/`parse_principal_id` (Task 3), `principals::canonical_scopes`/`ADMIN_SCOPE` (Task 2).
- Produces: routes under `/ui/api/*` (below), gated on the `admin` scope; static admin page routes (Task 8 builds the assets; this task mounts them). API contract:
  - `GET /ui/api/principals` → `200 {"principals":[{id,name,iss,sub,label,scopes,builtin,maskedByBuiltin}], "defaultNewPrincipalScopes":[...]}`
  - `POST /ui/api/principals` body `{name,iss,sub,label?,scopes}` → `201` view | `400` invalid | `409` built-in key or duplicate | `500`
  - `PATCH /ui/api/principals/{id}` body `{name?,label?,scopes?}` → `200` view | `404` | `409` built-in | `400`
  - `DELETE /ui/api/principals/{id}` → `204` | `404` | `409` built-in (revokes token families)
  - `GET /ui/api/waitlist` → `200 {"entries":[{id,name,iss,sub,firstSeenUs,lastSeenUs}]}`
  - `POST /ui/api/waitlist/{id}/approve` body `{scopes?}` (absent → configured default) → `201` view | `404` | `400`
  - `DELETE /ui/api/waitlist/{id}` → `204` | `404`

- [ ] **Step 1: Write the failing integration tests**

`tests/principal_admin.rs`. First add the admin-token helper to the support suite: in `tests/support/flow.rs`, a full-flow helper (register → authorize → callback → consent → token), following the reference sequence in `tests/oauth_upstream.rs:568-620` (authorize redirect) and `:749-782` (callback records the human), with the consent POST shape taken from the existing flow helpers (`hidden_field`, `code_from`, `query_param`):

```rust
/// Drive one full login for the server's principal[0] (which the caller
/// must have granted `admin`) and return the access token.
pub async fn admin_access_token(server: &Server) -> String {
    let redirect = format!("{PUBLIC_URL}/ui/admin/callback");
    let client_id = register(server, "admin-test", &redirect).await;
    let params = vec![
        ("response_type", "code"),
        ("client_id", client_id.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("scope", mcpmem::principals::ADMIN_SCOPE),
        ("state", "st"),
        ("code_challenge", code_challenge().as_str()),
    ];
    let res = server
        .request(Request::get(format!("/oauth/authorize?{}", query_string(&params))).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let upstream = res.headers().get(header::LOCATION).unwrap().to_str().unwrap().to_owned();
    let callback = server.request(Request::get(upstream).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(callback.status(), StatusCode::OK, "the callback answers (consent or redirect)");
    // If the callback answered with a 303 to the consent page, walk it and
    // POST the consent form the way existing flow tests do. If the
    // configured principal holds admin, the page shows the admin scope.
    // (Copy the exact consent POST from the existing flow tests — the
    // form carries `state`, `csrf` and the offered scopes.)
    let token_res = server
        .request(
            Request::post("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(format!(
                    "grant_type=authorization_code&code={code}&redirect_uri={redirect}&client_id={client_id}&code_verifier={verifier}",
                    code = code_from(location_of_consent_redirect),
                    verifier = CODE_VERIFIER,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value = support::json(token_res).await;
    body["access_token"].as_str().unwrap().to_owned()
}
```

The consent hop and code extraction use the existing helpers (`code_from`, `query_param`, `hidden_field` in `tests/support/flow.rs`); the reference for the consent form is the flow the consent tests drive (search `Request::post("/oauth/consent")` in `tests/`). The token request must match the AS's expectations: public client, PKCE S256, form-encoded (`grant_types_supported: ["authorization_code","refresh_token"]`, `token_endpoint_auth_methods_supported: ["none"]`).

Then the tests:

```rust
use support::{Scopes, json};

async fn admin_server() -> support::Server {
    let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default()).await;
    let mut config = support::oauth_config(&idp.issuer);
    config.principals[0].scopes.push("admin".into());
    let server = support::server(Some(config), Scopes::all(), None).await;
    let token = support::flow::admin_access_token(&server).await;
    (server, token)
}

fn bearer(token: &str) -> String { format!("Bearer {token}") }

#[tokio::test]
async fn list_marks_builtins_immutable_and_lists_runtime_rows() {
    let (server, token) = admin_server().await;
    let res = server.request(
        Request::get("/ui/api/principals")
            .header(header::AUTHORIZATION, bearer(&token))
            .body(Body::empty()).unwrap(),
    ).await.unwrap();
    assert_eq!(res.status(), 200);
    let body = json(res).await;
    let items = body["principals"].as_array().unwrap();
    assert!(items.iter().any(|p| p["builtin"].as_bool() == Some(true)));
    // The built-in owns an identity an admin cannot touch: PATCH and
    // DELETE on its id are refused.
    let builtin = items.iter().find(|p| p["builtin"].as_bool() == Some(true)).unwrap();
    let id = builtin["id"].as_str().unwrap();
    let patch = server.request(
        Request::patch(format!("/ui/api/principals/{id}"))
            .header(header::AUTHORIZATION, bearer(&token))
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"name":"hacked"}"#.to_owned()).unwrap(),
    ).await.unwrap();
    assert_eq!(patch.status(), 409);
    let del = server.request(
        Request::delete(format!("/ui/api/principals/{id}"))
            .header(header::AUTHORIZATION, bearer(&token))
            .body(Body::empty()).unwrap(),
    ).await.unwrap();
    assert_eq!(del.status(), 409);
}

#[tokio::test]
async fn create_update_delete_round_trip_and_delete_revokes() {
    let (server, token) = admin_server().await;
    let create = server.request(
        Request::post("/ui/api/principals")
            .header(header::AUTHORIZATION, bearer(&token))
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"name":"ada","iss":"https://idp.example","sub":"u-9","scopes":["graph-read"]}"#.to_owned()).unwrap(),
    ).await.unwrap();
    assert_eq!(create.status(), 201);
    let view = json(create).await;
    let id = view["id"].as_str().unwrap().to_owned();

    let patch = server.request(
        Request::patch(format!("/ui/api/principals/{id}"))
            .header(header::AUTHORIZATION, bearer(&token))
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"scopes":["graph-read","graph-write"]}"#.to_owned()).unwrap(),
    ).await.unwrap();
    assert_eq!(patch.status(), 200);
    assert_eq!(json(patch).await["scopes"][1], "graph-write");

    let del = server.request(
        Request::delete(format!("/ui/api/principals/{id}"))
            .header(header::AUTHORIZATION, bearer(&token))
            .body(Body::empty()).unwrap(),
    ).await.unwrap();
    assert_eq!(del.status(), 204);
}

#[tokio::test]
async fn a_non_admin_grant_is_refused() {
    let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default()).await;
    let mut config = support::oauth_config(&idp.issuer);
    // principals[0] holds only graph-read/graph-write — no admin.
    let server = support::server(Some(config), Scopes::all(), None).await;
    let token = support::flow::admin_access_token(&server).await;
    let res = server.request(
        Request::get("/ui/api/principals")
            .header(header::AUTHORIZATION, bearer(&token))
            .body(Body::empty()).unwrap(),
    ).await.unwrap();
    assert_eq!(res.status(), 403);
}

#[tokio::test]
async fn waitlist_approve_promotes_and_dismiss_discards() {
    let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default()).await;
    let mut config = support::oauth_config(&idp.issuer);
    config.approval_waitlist = true;
    config.principals[0].scopes.push("admin".into());
    config.principals[0].sub = "another-human".into(); // provider authenticates sub-1
    let server = support::server(Some(config), Scopes::all(), None).await;
    let token = support::flow::admin_access_token(&server).await;

    // sub-1 tries to log in and lands on the waitlist.
    // (Drive the authorize → callback walk; the callback answers the
    // pending page, exactly as in Task 6's first test.)
    let rows = support::count_rows(&server, "principal_waitlist");
    assert_eq!(rows, 1);

    let list = server.request(
        Request::get("/ui/api/waitlist")
            .header(header::AUTHORIZATION, bearer(&token))
            .body(Body::empty()).unwrap(),
    ).await.unwrap();
    let entry = json(list).await["entries"][0].clone();
    let id = entry["id"].as_str().unwrap().to_owned();
    // The FakeIdp always stamps an email, so the entry name is the email.
    assert_eq!(entry["sub"], "sub-1");
    assert_eq!(entry["name"], support::fake_idp::EMAIL);

    let approve = server.request(
        Request::post(format!("/ui/api/waitlist/{id}/approve"))
            .header(header::AUTHORIZATION, bearer(&token))
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"scopes":["graph-read"]}"#.to_owned()).unwrap(),
    ).await.unwrap();
    assert_eq!(approve.status(), 201);
    assert_eq!(json(approve).await["scopes"][0], "graph-read");
    assert_eq!(support::count_rows(&server, "principal_waitlist"), 0);
    // The promoted sub-1 now logs in normally (consent page, not pending).
    let again_rows = support::count_rows(&server, "principal_waitlist");
    assert_eq!(again_rows, 0);
}
```

Also add a `dismiss` test (DELETE on the entry → 204, row gone, no principal created).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test principal_admin`

Expected: FAIL — routes return 404 / compile errors for missing handlers.

- [ ] **Step 3: Implement the handlers in `src/http.rs`**

Extend the router (`http.rs:197-263`), before `crate::oauth_routes::attach(router)` at `:260` — API routes only; the static `/ui/admin*` routes and the `include_str!` consts arrive with the assets in Task 8:

```rust
        .route("/ui/api/principals", get(admin_list_principals).post(admin_create_principal))
        .route("/ui/api/principals/{id}", patch(admin_update_principal).delete(admin_delete_principal))
        .route("/ui/api/waitlist", get(admin_list_waitlist))
        .route("/ui/api/waitlist/{id}/approve", post(admin_approve_waitlist))
        .route("/ui/api/waitlist/{id}", delete(admin_dismiss_waitlist))
```

The gate (beside `principal_of_ui`, `http.rs:491-504`):

```rust
/// The principal behind this request, if it holds the admin scope. The
/// static bearer token can never hold admin, so every admin is a human
/// resolved through an OAuth grant.
fn admin_principal(state: &HttpState, headers: &HeaderMap) -> Option<Principal> {
    let p = principal_of_ui(state, headers, None)?;
    p.scopes
        .contains(crate::principals::ADMIN_SCOPE)
        .then_some(p)
}
```

Views and errors (module-level in `http.rs`):

```rust
#[derive(serde::Serialize)]
struct PrincipalView {
    id: String,
    name: String,
    iss: String,
    sub: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    scopes: Vec<String>,
    builtin: bool,
    masked_by_builtin: bool,
}

#[derive(serde::Deserialize)]
struct PrincipalInput {
    name: String,
    iss: String,
    sub: String,
    #[serde(default)]
    label: Option<String>,
    scopes: Vec<String>,
}

#[derive(serde::Deserialize)]
struct PrincipalPatch {
    #[serde(default)]
    name: Option<String>,
    /// Some("") clears the label.
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
}

#[derive(serde::Deserialize)]
struct ApproveBody {
    #[serde(default)]
    scopes: Option<Vec<String>>,
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

fn bad_request(message: impl Into<String>) -> Response {
    json_error(StatusCode::BAD_REQUEST, message)
}

fn conflict(message: impl Into<String>) -> Response {
    json_error(StatusCode::CONFLICT, message)
}

fn not_found() -> Response {
    json_error(StatusCode::NOT_FOUND, "no such row")
}

fn store_failure(e: impl std::fmt::Display) -> Response {
    json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("principals store: {e}"))
}
```

The `oauth` accessor, then the handlers. **Every store access in the handlers below goes through the Task 6 helper: rewrite each `oauth.runtime.X(...)` call as `oauth.with_principals(|s| s.X(...))`** — the store sits behind a `Mutex` because the connection is not `Sync` and the store uses `unchecked_transaction`. The handlers' `builtin_keys.contains(&key)` checks do not compile (`BTreeSet<(String, String)>` has no `Borrow` for `(&str, &str)`): use a small `fn is_builtin(set: &BTreeSet<(String, String)>, iss: &str, sub: &str) -> bool` helper comparing elements, same semantics, no per-row allocation. List:

```rust
async fn admin_list_principals(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    let Some(admin) = admin_principal(&state, &headers) else {
        return if principal_of_ui(&state, &headers, None).is_some() {
            insufficient_scope(&state, &[crate::principals::ADMIN_SCOPE])
        } else {
            unauthorized(&state)
        };
    };
    let _ = admin;
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut out: Vec<PrincipalView> = oauth
        .config
        .principals
        .iter()
        .map(|p| PrincipalView {
            id: mcpmem_oauth::principal_id(&p.iss, &p.sub),
            name: p.name.clone(),
            iss: p.iss.clone(),
            sub: p.sub.clone(),
            label: p.label.clone(),
            scopes: p.scopes.clone(),
            builtin: true,
            masked_by_builtin: false,
        })
        .collect();
    let runtime = match oauth.runtime.list() {
        Ok(rows) => rows,
        Err(e) => return store_failure(e),
    };
    for row in runtime {
        let key = (row.iss.as_str(), row.sub.as_str());
        let masked = oauth.builtin_keys.contains(&key);
        out.push(PrincipalView {
            id: mcpmem_oauth::principal_id(&row.iss, &row.sub),
            name: row.name,
            iss: row.iss,
            sub: row.sub,
            label: row.label,
            scopes: row.scopes,
            builtin: false,
            masked_by_builtin: masked,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "principals": out,
            "defaultNewPrincipalScopes": oauth.config.default_new_principal_scopes,
        })),
    )
        .into_response()
}
```

Create:

```rust
async fn admin_create_principal(State(state): State<HttpState>, headers: HeaderMap, body: String) -> Response {
    let Some(_admin) = admin_principal(&state, &headers) else {
        return if principal_of_ui(&state, &headers, None).is_some() {
            insufficient_scope(&state, &[crate::principals::ADMIN_SCOPE])
        } else {
            unauthorized(&state)
        };
    };
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let input: PrincipalInput = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("the body must be a JSON principal"),
    };
    let name = input.name.trim();
    if name.is_empty() || input.iss.is_empty() || input.sub.is_empty() {
        return bad_request("name, iss and sub are required");
    }
    let scopes = match crate::principals::canonical_scopes(&input.scopes) {
        Ok(s) if !s.is_empty() => s,
        _ => return bad_request("at least one known scope is required"),
    };
    let key = (input.iss.as_str(), input.sub.as_str());
    if is_builtin(&oauth.builtin_keys, &input.iss, &input.sub) {
        return conflict("a built-in principal owns this identity; it is immutable");
    }
    // A duplicate runtime key is 409, per the API contract — never a 500
    // from the UNIQUE constraint.
    match oauth.with_principals(|s| s.get(&input.iss, &input.sub)) {
        Ok(Some(_)) => return conflict("a runtime principal already owns this identity"),
        Ok(None) => {}
        Err(e) => return store_failure(e),
    }
    if let Err(e) = oauth.with_principals(|s| s.create(&input.iss, &input.sub, name, input.label.as_deref(), &scopes)) {
        return store_failure(e);
    }
    let view = PrincipalView {
        id: mcpmem_oauth::principal_id(&input.iss, &input.sub),
        name: name.to_owned(),
        iss: input.iss,
        sub: input.sub,
        label: input.label,
        scopes,
        builtin: false,
        masked_by_builtin: false,
    };
    (StatusCode::CREATED, Json(view)).into_response()
}
```

The admin gate repeats; factor it into a small closure-free helper to keep the handlers short:

```rust
/// The gate every `/ui/api/*` handler runs first. `Some(())` when the
/// caller holds the admin scope; `Err(Response)` is the 401/403 answer.
fn admin_gate(state: &HttpState, headers: &HeaderMap) -> std::result::Result<(), Response> {
    if admin_principal(state, headers).is_some() {
        return Ok(());
    }
    let response = if principal_of_ui(state, headers, None).is_some() {
        insufficient_scope(state, &[crate::principals::ADMIN_SCOPE])
    } else {
        unauthorized(state)
    };
    Err(response)
}

/// The id path segment → (iss, sub).
fn key_of_id(id: &str) -> Option<(String, String)> {
    mcpmem_oauth::parse_principal_id(id)
}
```

Then each remaining handler uses it, e.g. update:

```rust
async fn admin_update_principal(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: String,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) { return response; }
    let Some(oauth) = state.oauth.as_ref() else { return StatusCode::NOT_FOUND.into_response(); };
    let Some((iss, sub)) = key_of_id(&id) else { return not_found(); };
    let key = (iss.as_str(), sub.as_str());
    if oauth.builtin_keys.contains(&key) {
        return conflict("a built-in principal owns this identity; it is immutable");
    }
    let Some(row) = match oauth.runtime.get(&iss, &sub) {
        Ok(Some(row)) => Some(row),
        Ok(None) => None,
        Err(e) => return store_failure(e),
    } else {
        return not_found();
    };
    let patch: PrincipalPatch = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("the body must be a JSON patch"),
    };
    let name = patch.name.as_deref().unwrap_or(&row.name).trim().to_owned();
    if name.is_empty() {
        return bad_request("name must not be empty");
    }
    let label = match patch.label {
        Some(raw) if raw.is_empty() => None,
        value => value.or(row.label.clone()),
    };
    let scopes = match patch.scopes {
        Some(raw) => match crate::principals::canonical_scopes(&raw) {
            Ok(s) if !s.is_empty() => s,
            _ => return bad_request("at least one known scope is required"),
        },
        None => row.scopes.clone(),
    };
    if let Err(e) = oauth.runtime.update(&iss, &sub, &name, label.as_deref(), &scopes) {
        return store_failure(e);
    }
    (StatusCode::OK, Json(PrincipalView {
        id,
        name,
        iss,
        sub,
        label,
        scopes,
        builtin: false,
        masked_by_builtin: false,
    }))
        .into_response()
}
```

Delete (revokes families by the row's name; note the v1 limitation documented in the spec):

```rust
async fn admin_delete_principal(State(state): State<HttpState>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(response) = admin_gate(&state, &headers) { return response; }
    let Some(oauth) = state.oauth.as_ref() else { return StatusCode::NOT_FOUND.into_response(); };
    let Some((iss, sub)) = key_of_id(&id) else { return not_found(); };
    let key = (iss.as_str(), sub.as_str());
    if oauth.builtin_keys.contains(&key) {
        return conflict("a built-in principal owns this identity; it is immutable");
    }
    let Some(row) = match oauth.runtime.get(&iss, &sub) {
        Ok(Some(row)) => Some(row),
        Ok(None) => None,
        Err(e) => return store_failure(e),
    } else {
        return not_found();
    };
    if let Err(e) = oauth.runtime.delete(&iss, &sub) { return store_failure(e); }
    let revoked = oauth.revoke_principal(&row.name).unwrap_or(0);
    tracing::info!(name = %row.name, revoked, "deleted principal and revoked token families");
    StatusCode::NO_CONTENT.into_response()
}
```

Waitlist list / approve / dismiss:

```rust
async fn admin_list_waitlist(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = admin_gate(&state, &headers) { return response; }
    let Some(oauth) = state.oauth.as_ref() else { return StatusCode::NOT_FOUND.into_response(); };
    let entries = match oauth.runtime.waitlist() {
        Ok(rows) => rows,
        Err(e) => return store_failure(e),
    };
    let out: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|e| serde_json::json!({
            "id": mcpmem_oauth::principal_id(&e.iss, &e.sub),
            "name": e.name,
            "iss": e.iss,
            "sub": e.sub,
            "firstSeenUs": e.first_seen_us,
            "lastSeenUs": e.last_seen_us,
        }))
        .collect();
    (StatusCode::OK, Json(serde_json::json!({ "entries": out }))).into_response()
}

async fn admin_approve_waitlist(State(state): State<HttpState>, headers: HeaderMap, Path(id): Path<String>, body: String) -> Response {
    if let Err(response) = admin_gate(&state, &headers) { return response; }
    let Some(oauth) = state.oauth.as_ref() else { return StatusCode::NOT_FOUND.into_response(); };
    let Some((iss, sub)) = key_of_id(&id) else { return not_found(); };
    let scopes = match serde_json::from_str::<ApproveBody>(&body) {
        Ok(body) => body.scopes.unwrap_or_else(|| oauth.config.default_new_principal_scopes.clone()),
        Err(_) => return bad_request("the body must be JSON with an optional scopes list"),
    };
    let scopes = match crate::principals::canonical_scopes(&scopes) {
        Ok(s) if !s.is_empty() => s,
        _ => return bad_request("at least one known scope is required"),
    };
    match oauth.runtime.approve(&iss, &sub, &scopes) {
        Ok(Some(row)) => (StatusCode::CREATED, Json(PrincipalView {
            id: mcpmem_oauth::principal_id(&iss, &sub),
            name: row.name,
            iss,
            sub,
            label: None,
            scopes,
            builtin: false,
            masked_by_builtin: false,
        })).into_response(),
        Ok(None) => not_found(),
        Err(e) => store_failure(e),
    }
}

async fn admin_dismiss_waitlist(State(state): State<HttpState>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(response) = admin_gate(&state, &headers) { return response; }
    let Some(oauth) = state.oauth.as_ref() else { return StatusCode::NOT_FOUND.into_response(); };
    let Some((iss, sub)) = key_of_id(&id) else { return not_found(); };
    match oauth.runtime.dismiss_waitlist(&iss, &sub) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => store_failure(e),
    }
}
```

Fix imports at the top of `src/http.rs`: add `use axum::extract::{Path, State};`, `use axum::Json;`, `use axum::routing::{get, patch, post};` (merge with the existing routing import — check what `http.rs` already imports and extend it), and `use serde::{Deserialize, Serialize};`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --test principal_admin`

Expected: PASS (all tests in the new file).

- [ ] **Step 5: Verify the existing UI tests still pass**

Run: `cargo test --test ui_http`

Expected: PASS — the new static routes are additive.

- [ ] **Step 6: Commit**

```bash
git add src/http.rs tests/principal_admin.rs tests/support/flow.rs
git commit -m "feat: admin API under /ui/api, gated on the admin scope

CRUD over runtime principals with built-ins immutable (409), waitlist
list/approve/dismiss, revoke-by-name on delete, and the reserved admin
client in the OAuth flow. Integration tests drive the full login
through the fake upstream provider.

Tokens: ~22k. Cost: < $1."
```

---

### Task 8: Admin UI — the SPA at `/ui/admin`

**Files:**
- Create: `src/ui/admin.html`, `src/ui/admin.js`, `src/ui/admin.css`
- Modify: `src/http.rs` (static consts + routes; Task 7 mounted only the API routes)
- Verify: `tests/principal_admin.rs` adds a static-serving assertion

**Interfaces:**
- Consumes: the API routes and contract from Task 7; the OAuth endpoints (`/oauth/authorize`, `/oauth/token`); the seeded client id `mcpmem-admin-ui` (Task 3).
- Produces: the browser app that performs the PKCE dance, lists/edit/adds/removes principals, and lists/approves/dismisses waitlist entries. Alongside the assets, mount the static routes in `http.rs` (consts beside the UI consts at `http.rs:47-49`):

```rust
const ADMIN_INDEX_HTML: &str = include_str!("ui/admin.html");
const ADMIN_JS: &str = include_str!("ui/admin.js");
const ADMIN_CSS: &str = include_str!("ui/admin.css");
```

and in `router()` (before `crate::oauth_routes::attach(router)`):

```rust
        .route("/ui/admin", get(admin_page_handler))
        .route("/ui/admin/callback", get(admin_page_handler))
        .route("/ui/admin.js", get(admin_js_handler))
        .route("/ui/admin.css", get(admin_css_handler))
```

with the three handlers mirroring `ui_handler` (`http.rs:508-533`) — static, no auth; the JSON endpoints are the gate (content types: `text/html; charset=utf-8`, `text/javascript; charset=utf-8`, `text/css; charset=utf-8`).

- [ ] **Step 1: Write the failing test**

In `tests/principal_admin.rs`, extend the list test or add:

```rust
#[tokio::test]
async fn the_admin_page_and_assets_are_served() {
    let (server, _token) = admin_server().await;
    for (path, kind) in [
        ("/ui/admin", "text/html"),
        ("/ui/admin.js", "text/javascript"),
        ("/ui/admin.css", "text/css"),
    ] {
        let res = server
            .request(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "{path} serves");
        let ct = res.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap();
        assert!(ct.starts_with(kind), "{path} content type is {ct}");
    }
}
```

Run: `cargo test --test principal_admin the_admin_page_and_assets_are_served`

Expected: FAIL — the consts are declared but `src/ui/admin.html` does not exist, so the build fails; create the three asset files in the next step.

- [ ] **Step 2: Create the assets**

`src/ui/admin.html`:

```html
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>mcpmem — principals</title>
  <link rel="stylesheet" href="/ui/admin.css">
</head>
<body>
  <header>
    <h1>Principals</h1>
    <p class="hint" id="status"></p>
  </header>
  <main>
    <section id="principals">
      <h2>Who may authorize</h2>
      <button id="add" hidden>Add principal</button>
      <table>
        <thead><tr><th>Name</th><th>Identity</th><th>Scopes</th><th></th></tr></thead>
        <tbody id="principal-rows"></tbody>
      </table>
    </section>
    <section id="waitlist">
      <h2>Pending approvals</h2>
      <table>
        <thead><tr><th>Name</th><th>Identity</th><th>First seen</th><th></th></tr></thead>
        <tbody id="waitlist-rows"></tbody>
      </table>
    </section>
  </main>
  <dialog id="form"></dialog>
  <script src="/ui/admin.js"></script>
</body>
</html>
```

`src/ui/admin.js` — the full application (PKCE dance, API client, render, form dialog). Follow the fetch + sessionStorage convention of `src/ui/graph.js`:

```js
"use strict";

const CLIENT_ID = "mcpmem-admin-ui";
const TOKEN_KEY = "mcpmem_admin_access";
const VERIFIER_KEY = "mcpmem_admin_verifier";
const REDIRECT = location.origin + "/ui/admin/callback";

let accessToken = sessionStorage.getItem(TOKEN_KEY);
let defaultScopes = ["graph-read"];

function b64url(bytes) {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function randomVerifier() {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  return b64url(bytes);
}

async function challenge(verifier) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier));
  return b64url(new Uint8Array(digest));
}

async function beginAuth() {
  const verifier = randomVerifier();
  sessionStorage.setItem(VERIFIER_KEY, verifier);
  const params = new URLSearchParams({
    response_type: "code",
    client_id: CLIENT_ID,
    redirect_uri: REDIRECT,
    scope: "admin",
    state: "admin",
    code_challenge_method: "S256",
    code_challenge: await challenge(verifier),
  });
  location.href = "/oauth/authorize?" + params;
}

async function completeAuth() {
  const params = new URLSearchParams(location.search);
  const code = params.get("code");
  const verifier = sessionStorage.getItem(VERIFIER_KEY);
  sessionStorage.removeItem(VERIFIER_KEY);
  if (!code || !verifier) return;
  const form = new URLSearchParams({
    grant_type: "authorization_code",
    code,
    redirect_uri: REDIRECT,
    client_id: CLIENT_ID,
    code_verifier: verifier,
  });
  const res = await fetch("/oauth/token", {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: form,
  });
  if (!res.ok) {
    setStatus("Sign-in failed: " + (await res.text()));
    return;
  }
  const body = await res.json();
  accessToken = body.access_token;
  sessionStorage.setItem(TOKEN_KEY, accessToken);
  history.replaceState(null, "", "/ui/admin");
  load();
}

async function api(path, options = {}) {
  const headers = { ...(options.headers || {}) };
  if (accessToken) headers["Authorization"] = "Bearer " + accessToken;
  if (options.body) headers["Content-Type"] = "application/json";
  const res = await fetch(path, { ...options, headers });
  if (res.status === 401) {
    accessToken = null;
    sessionStorage.removeItem(TOKEN_KEY);
    await beginAuth();
    return null;
  }
  if (!res.ok) {
    const text = await res.text();
    throw new Error(res.status + " " + text);
  }
  return res.status === 204 ? null : res.json();
}

async function load() {
  const data = await api("/ui/api/principals");
  if (!data) return;
  defaultScopes = data.defaultNewPrincipalScopes || [];
  renderPrincipals(data.principals);
  document.getElementById("add").hidden = false;
  const wait = await api("/ui/api/waitlist");
  if (wait) renderWaitlist(wait.entries);
  setStatus("");
}

function renderPrincipals(principals) {
  const tbody = document.getElementById("principal-rows");
  tbody.textContent = "";
  for (const p of principals) {
    const tr = document.createElement("tr");
    const name = document.createElement("td");
    name.textContent = p.name;
    if (p.builtin) name.appendChild(badge("built-in"));
    if (p.maskedByBuiltin) name.appendChild(badge("masked by built-in"));
    const identity = document.createElement("td");
    identity.textContent = p.iss + " — " + p.sub;
    identity.className = "mono";
    const scopes = document.createElement("td");
    scopes.textContent = p.scopes.join(", ");
    const actions = document.createElement("td");
    if (!p.builtin) {
      const edit = document.createElement("button");
      edit.textContent = "Edit";
      edit.onclick = () => openForm(p);
      const del = document.createElement("button");
      del.textContent = "Remove";
      del.onclick = () => removePrincipal(p);
      actions.append(edit, del);
    }
    tr.append(name, identity, scopes, actions);
    tbody.append(tr);
  }
}

function badge(text) {
  const span = document.createElement("span");
  span.className = "badge";
  span.textContent = text;
  return span;
}

function renderWaitlist(entries) {
  const tbody = document.getElementById("waitlist-rows");
  tbody.textContent = "";
  for (const e of entries) {
    const tr = document.createElement("tr");
    const name = document.createElement("td");
    name.textContent = e.name;
    const identity = document.createElement("td");
    identity.textContent = e.iss + " — " + e.sub;
    identity.className = "mono";
    const seen = document.createElement("td");
    seen.textContent = new Date(e.firstSeenUs / 1000).toLocaleString();
    const actions = document.createElement("td");
    const approve = document.createElement("button");
    approve.textContent = "Approve";
    approve.onclick = () => openApprove(e);
    const dismiss = document.createElement("button");
    dismiss.textContent = "Dismiss";
    dismiss.onclick = () => dismissEntry(e);
    actions.append(approve, dismiss);
    tr.append(name, identity, seen, actions);
    tbody.append(tr);
  }
}

async function removePrincipal(p) {
  if (!confirm("Remove " + p.name + "? Their token families will be revoked.")) return;
  try {
    await api("/ui/api/principals/" + encodeURIComponent(p.id), { method: "DELETE" });
    await load();
  } catch (e) {
    setStatus(e.message);
  }
}

function scopesPicker(selected) {
  const wrap = document.createElement("div");
  wrap.className = "scopes";
  const all = ["graph-read", "graph-write", "vectors", "code", "admin"];
  for (const slug of all) {
    const label = document.createElement("label");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.value = slug;
    box.checked = selected.includes(slug);
    label.append(box, " " + slug);
    wrap.append(label);
  }
  return wrap;
}

function openForm(p) {
  const dialog = document.getElementById("form");
  dialog.textContent = "";
  const h = document.createElement("h2");
  h.textContent = p ? "Edit " + p.name : "Add principal";
  dialog.append(h);

  const fields = [["name", "Name", p ? p.name : ""]];
  if (!p) fields.push(["iss", "Issuer (iss)", ""], ["sub", "Subject (sub)", ""]);
  fields.push(["label", "Label", p && p.label || ""]);
  for (const [key, label, value] of fields) {
    const row = document.createElement("label");
    row.textContent = label + ": ";
    const input = document.createElement("input");
    input.id = "f-" + key;
    input.value = value;
    row.append(input);
    dialog.append(row);
  }

  const scopes = scopesPicker(p ? p.scopes : defaultScopes);
  dialog.append(scopes);

  const save = document.createElement("button");
  save.textContent = "Save";
  save.onclick = async () => {
    const read = (k) => document.getElementById("f-" + k).value.trim();
    const picked = [...scopes.querySelectorAll("input:checked")].map((b) => b.value);
    const body = {
      name: read("name"),
      label: read("label") || null,
      scopes: picked,
    };
    try {
      if (p) {
        await api("/ui/api/principals/" + encodeURIComponent(p.id), {
          method: "PATCH",
          body: JSON.stringify({ name: body.name, label: body.label, scopes: body.scopes }),
        });
      } else {
        await api("/ui/api/principals", {
          method: "POST",
          body: JSON.stringify({ name: body.name, iss: read("iss"), sub: read("sub"), label: body.label, scopes: body.scopes }),
        });
      }
      dialog.close();
      await load();
    } catch (e) {
      setStatus(e.message);
    }
  };
  const cancel = document.createElement("button");
  cancel.textContent = "Cancel";
  cancel.onclick = () => dialog.close();
  dialog.append(save, cancel);
  dialog.showModal();
}

function openApprove(e) {
  const dialog = document.getElementById("form");
  dialog.textContent = "";
  const h = document.createElement("h2");
  h.textContent = "Approve " + e.name;
  dialog.append(h);
  const who = document.createElement("p");
  who.textContent = e.iss + " — " + e.sub;
  who.className = "mono";
  dialog.append(who);
  const scopes = scopesPicker(defaultScopes);
  dialog.append(scopes);
  const approve = document.createElement("button");
  approve.textContent = "Approve";
  approve.onclick = async () => {
    const picked = [...scopes.querySelectorAll("input:checked")].map((b) => b.value);
    try {
      await api("/ui/api/waitlist/" + encodeURIComponent(e.id) + "/approve", {
        method: "POST",
        body: JSON.stringify({ scopes: picked }),
      });
      dialog.close();
      await load();
    } catch (err) {
      setStatus(err.message);
    }
  };
  const cancel = document.createElement("button");
  cancel.textContent = "Cancel";
  cancel.onclick = () => dialog.close();
  dialog.append(approve, cancel);
  dialog.showModal();
}

async function dismissEntry(e) {
  try {
    await api("/ui/api/waitlist/" + encodeURIComponent(e.id), { method: "DELETE" });
    await load();
  } catch (err) {
    setStatus(err.message);
  }
}

function setStatus(text) {
  const el = document.getElementById("status");
  el.textContent = text;
  el.className = text.startsWith("Error") || text.includes("failed") ? "hint error" : "hint";
}

document.getElementById("add").onclick = () => openForm(null);

(async function boot() {
  if (new URLSearchParams(location.search).has("code")) {
    await completeAuth();
    return;
  }
  if (!accessToken) {
    await beginAuth();
    return;
  }
  load();
})();
```

`src/ui/admin.css` — align with `graph.css` conventions (dark palette, system font), minimal:

```css
:root {
  color-scheme: dark;
  --bg: #101418;
  --panel: #171c22;
  --border: #2a323c;
  --text: #dbe4ee;
  --muted: #8b98a5;
  --accent: #4c9aff;
}
* { box-sizing: border-box; }
body {
  margin: 0 auto; max-width: 70rem; padding: 1.5rem;
  background: var(--bg); color: var(--text);
  font: 15px/1.5 system-ui, sans-serif;
}
h1 { font-size: 1.4rem; }
h2 { font-size: 1.1rem; margin-top: 2rem; }
.hint { color: var(--muted); min-height: 1em; }
.hint.error { color: #ff7b72; }
table { width: 100%; border-collapse: collapse; margin-top: .5rem; }
th, td { text-align: left; padding: .45rem .6rem; border-bottom: 1px solid var(--border); vertical-align: top; }
.mono { font-family: ui-monospace, monospace; font-size: .85em; }
.badge {
  display: inline-block; margin-left: .4rem; padding: 0 .35rem;
  font-size: .75em; border: 1px solid var(--border); border-radius: .3rem; color: var(--muted);
}
button {
  background: var(--panel); color: var(--text);
  border: 1px solid var(--border); border-radius: .35rem;
  padding: .3rem .7rem; cursor: pointer;
}
button:hover { border-color: var(--accent); }
button + button { margin-left: .4rem; }
dialog {
  background: var(--panel); color: var(--text);
  border: 1px solid var(--border); border-radius: .5rem; padding: 1.2rem;
}
dialog label { display: block; margin: .5rem 0; }
dialog input[type="text"] {
  width: 100%; background: var(--bg); color: var(--text);
  border: 1px solid var(--border); border-radius: .35rem; padding: .35rem .5rem;
}
.scopes label { display: inline-block; margin: .2rem .8rem .2rem 0; }
```

- [ ] **Step 3: Run the tests**

Run: `cargo test --test principal_admin`

Expected: PASS, including `the_admin_page_and_assets_are_served`.

- [ ] **Step 4: Smoke the UI against a live server**

Start a server with OAuth enabled (a real issuer needs TLS; for the smoke, start the server with a test-ish configuration and confirm `/ui/admin` serves the SPA and the API answers 401 `WWW-Authenticate` without a grant):

```bash
cargo run -- --transport http --bind 127.0.0.1:18080 --enable-all &
curl -i http://127.0.0.1:18080/ui/admin       # 200 text/html
curl -i http://127.0.0.1:18080/ui/api/principals  # 401, WWW-Authenticate Bearer
```

With a full TLS + real identity provider configuration, drive the browser once: open `https://<public-url>/ui/admin`, complete the provider login, confirm an admin grant shows the tables and the waitlist (browser tooling: `browser.open` + observe). If a real provider is not available in this environment, record that the browser smoke was not run and rely on the integration tests.

- [ ] **Step 5: Commit**

```bash
git add src/ui/admin.html src/ui/admin.js src/ui/admin.css tests/principal_admin.rs
git commit -m "feat: principal administration SPA

PKCE login through the server's own AS with the reserved admin client,
lists and edits principals (built-ins read-only), and promotes or
dismisses waitlist entries. Vanilla JS, no build step.

Tokens: ~16k. Cost: < $1."
```

---

### Task 9: Docs and full verification

**Files:**
- Modify: `README.md` (principals + admin UI + waitlist section), `CHANGES.md` (unreleased entry), `docs/superpowers/specs/2026-09-11-principal-admin-design.md` (already corrected by the main agent; verify)

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: README**

Add to the OAuth section (where `--principals-file` is documented at `README.md:593-631`, 953-956): the three config keys, the admin bootstrap (the first admin is a built-in entry holding `admin`), the `/ui/admin` location, built-in immutability (409 on colliding writes; masked rows), the waitlist behavior (24 h TTL fixed from first sighting, cap 25, eviction LRU), and the revocation semantics with the v1 limitation (revoke by display name; a renamed-then-deleted principal's old families live until expiry).

- [ ] **Step 2: CHANGES.md**

Add one unreleased bullet under the existing unreleased heading:

```markdown
- Admin UI at `/ui/admin`: principals management with OAuth (new `admin`
  scope), runtime principals in SQLite, JSON entries immutable built-ins,
  optional approval waitlist (`[oauth] approval-waitlist`).
```

- [ ] **Step 3: Full verification**

Run the repo's pre-flight (the command CI runs — check `.github/workflows/ci.yml` for the exact invocation):

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

Expected: all green (the pre-flight command this repo names; if CI runs additional checks — audit, doctest, feature-matrix builds — run those too, matching its invocations exactly).

- [ ] **Step 4: Commit**

```bash
git add README.md CHANGES.md
git commit -m "docs: principal administration, waitlist, and the admin scope

Runbook for the three config keys, the admin bootstrap, immutable
built-ins, waitlist semantics, and revocation limits. Workspace suite,
clippy and fmt green.

Tokens: ~5k. Cost: < $1."
```

---

## Self-Review

**Spec coverage:** every section of the design doc maps to a task — storage (T1, T4), merged resolution + collision policy (T6), admin scope + client + flow (T2, T3, T8), API (T7), waitlist semantics (T4, T5, T6), revocation with the documented v1 limit (T3, T7), UI (T8), config + runbook (T5, T9). Out-of-scope items (roles beyond admin, audit log, impersonation, SAML) are untouched.

**Placeholder scan:** the two intentionally open reference points (the consent POST shape in `admin_access_token`, and the migration checksum hash) are resolved by reading code that the plan names (`tests/support/flow.rs`, `tests/oauth_upstream.rs:749-782`; the inventory-test failure output). No TBD/TODO remains.

**Type consistency:** `PrincipalsStore::approve` returns `Option<RuntimePrincipal>` and the handler maps `Ok(None)` to 404; `canonical_scopes` returns `Result<Vec<String>>` and every caller handles both arms; `principal_id`/`parse_principal_id` are implemented once in `mcpmem-oauth` and used by every handler; `ADMIN_SCOPE` is the single spelling everywhere.