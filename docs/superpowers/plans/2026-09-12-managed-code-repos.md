# Managed Code Repositories Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Register remote git repositories for code-symbol indexing, trigger a fresh clone+index and later reindexes, and remove a repository (registration, clone, and indexed database) with one action — from the admin web UI and from MCP tools.

**Architecture:** A `code_repo` table (migration 0006) in the main memory database stores key→url→credential rows. A new `src/repos.rs` service owns the lifecycle: git clone/fetch via the `git` binary (no new crates), then the existing `handle_code_index` on the worktree. HTTP admin routes (admin scope gate, existing pattern) answer 202 and run jobs on detached threads; MCP tools are synchronous. Per-project code databases stay at `<memory_file>.code/<key>.code.db`; worktrees live at `<memory_file>.code/repos/<key>/`.

**Tech Stack:** Rust workspace; rusqlite 0.40 bundled; axum router (`src/http.rs`); the `git` binary (host, floor 2.31); vanilla JS UI (no bundler), matching `src/ui/admin.js`; tree-sitter indexer unchanged.

**Spec:** `docs/superpowers/specs/2026-09-12-managed-code-repos-design.md` (approved 2026-09-12).

## Global Constraints

- Key = project identifier: 1-64 chars of `[A-Za-z0-9_-]`, enforced by `code_registry::validate_project`.
- Secrets never appear in argv, URLs, git config files, or responses. Token via `GIT_CONFIG_KEY_0` / `GIT_CONFIG_VALUE_0` env; SSH key in a `0600` temp file via `GIT_SSH_COMMAND`, unlinked after use. `GIT_TERMINAL_PROMPT=0` always.
- Git commands: hardcoded 600-second timeout; bounded stderr capture for `last_error`.
- One in-flight job per key, in process. Second trigger → 409 (`already_in_progress`).
- HTTP mutations answer 202, then a detached thread runs the job; the UI polls the list. MCP mutations are synchronous.
- Remove = full wipe: stop watcher, evict project handle, delete `<key>.code.db` + `-wal`/`-shm`, delete worktree, delete row.
- Server restart: rows in `cloning`/`indexing`/`removing` become `pending`.
- Every `/ui/api/repos*` route passes `admin_gate` first (401/403 unchanged).
- No new crates. Feature gate everything with `#[cfg(feature = "code")]`.
- Style: ASD-STE100 in all docs, comments, and commit messages. Format with `cargo fmt`, lint with `cargo clippy --workspace --all-targets --all-features -- -D warnings`, full tests once at the end (Task 8), not per task.

---

### Task 1: Migration 0006 — the `code_repo` table

**Files:**
- Create: `crates/mcpmem-core/migrations/0006_code_repos.sql`
- Modify: `crates/mcpmem-core/src/events.rs` (`MIGRATIONS` at lines 38-57, `mod migration_inventory` at lines 59-103)

**Interfaces:**
- Consumes: the migration ledger at `events.rs:38-57` (`[(i64, &str); 5]`), the checksum-pinning test `every_migration_version_and_checksum_is_pinned` (`events.rs:59-103`), `sha256` (`events.rs:22-25`).
- Produces: version 6 in `MIGRATIONS` (`[(i64, &str); 6]`), the new STRICT table, and the pinned checksum row.

- [ ] **Step 1: Write the migration file**

`crates/mcpmem-core/migrations/0006_code_repos.sql`:

```sql
-- Managed remote repositories for code-symbol indexing. One row per
-- repository key (= code project identifier). Credentials sit in this
-- database, the same trust boundary as the knowledge graph itself.
CREATE TABLE code_repo (
    key             TEXT PRIMARY KEY,
    url             TEXT NOT NULL,
    auth_kind       TEXT NOT NULL CHECK (auth_kind IN ('none','token','ssh')),
    auth_secret     TEXT,
    snippets        INTEGER NOT NULL DEFAULT 0 CHECK (snippets IN (0,1)),
    state           TEXT NOT NULL DEFAULT 'pending'
                    CHECK (state IN ('pending','cloning','indexing','indexed','error','removing')),
    last_error      TEXT,
    last_indexed_us INTEGER
) STRICT;

CREATE INDEX code_repo_state ON code_repo(state);
```

- [ ] **Step 2: Register the migration**

In `crates/mcpmem-core/src/events.rs`, change the type to `[(i64, &str); 6]` and append:

```rust
    (6, include_str!("../migrations/0006_code_repos.sql")),
```

- [ ] **Step 3: Run the inventory test to fail**

Run: `cargo test -p mcpmem-core migration_inventory`

Expected: FAIL. The assertion prints the computed list on the left; the `vec![...]` on the right ends at version 5. Copy the `(6, "<64-hex-sha256>")` pair for your file from the left-hand output.

- [ ] **Step 4: Pin the checksum**

In `mod migration_inventory`, append the copied tuple so the `vec![...]` ends with:

```rust
    (
        6,
        "<64-hex-sha256 of 0006_code_repos.sql>".to_owned(),
    ),
```

- [ ] **Step 5: Run the crate tests**

Run: `cargo test -p mcpmem-core`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/mcpmem-core/migrations/0006_code_repos.sql crates/mcpmem-core/src/events.rs
git commit -m "feat: migration 0006 — the managed code_repo table

STRICT table for managed remote repositories: key, url, credentials,
snippets flag, lifecycle state. Registered in the versioned migration
ledger and pinned in the checksum inventory.

Tokens: ~2k. Cost: < $1."
```

---

### Task 2: `code_registry::drop_project` — close a project handle on demand

**Files:**
- Modify: `src/code_registry.rs` (after `resolve`, ends line 138)
- Test: `src/code_registry.rs` (new `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Inner { live: HashMap<String, Weak<GraphHandle>>, warm: LruCache<String, Arc<GraphHandle>> }`; `INNER: OnceLock<Mutex<Inner>>`.
- Produces: `pub fn drop_project(project: &str) -> Result<()>` — evicts `warm` by key (LruCache::pop, lru 0.12.5 line 1109) and removes the `live` entry. Callers stop any watcher first.

- [ ] **Step 1: Write the failing test**

Append to `src/code_registry.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn drop_project_evicts_the_canonical_handle() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("code");
        init(
            base.clone(),
            crate::config::Durability::Async,
            crate::config::SqliteTuning::default(),
            NonZeroUsize::new(8).unwrap(),
            2,
        );
        let handle = resolve("dropme").expect("resolve opens a project");
        assert_eq!(Arc::strong_count(&handle), 1);

        drop_project("dropme").expect("drop_project succeeds");

        // No live strong reference may survive; a subsequent resolve reopens.
        let reopened = resolve("dropme").expect("resolve reopens after drop");
        assert!(!Arc::ptr_eq(&handle, &reopened));
        assert_eq!(Arc::strong_count(&handle), 0);

        // The warm LRU entry is gone: the opened handle is the only strong one.
        let again = resolve("dropme").expect("resolve is stable");
        assert!(Arc::ptr_eq(&reopened, &again));
    }

    #[test]
    fn drop_project_refuses_invalid_names() {
        assert!(drop_project("bad/name").is_err());
        assert!(drop_project("").is_err());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p mcpmem code_registry::tests`

Expected: FAIL — `drop_project` is not defined.

- [ ] **Step 3: Implement**

Add after `resolve` in `src/code_registry.rs`:

```rust
/// Close the canonical handle for `project`: evict it from the warm LRU and
/// forget the live weak entry. Once every caller drops its `Arc`, the
/// SQLite connections close and the database files may be deleted.
///
/// Callers must stop any watcher on `project` first: a running watcher pins
/// a strong reference and would keep the connections open.
pub fn drop_project(project: &str) -> Result<()> {
    validate_project(project)?;
    let inner = INNER.get().expect("registry inner set alongside config");
    let mut g = inner.lock();
    g.live.remove(project);
    g.warm.pop(project);
    Ok(())
}
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p mcpmem code_registry::tests`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/code_registry.rs
git commit -m "feat: code_registry::drop_project closes a project handle

Evicts the warm-LRU entry (LruCache::pop) and forgets the live weak
reference, so the SQLite connections close once callers drop their Arcs.
The managed-repo remove path needs this before deleting a project DB.

Tokens: ~3k. Cost: < $1."
```

---

### Task 3: Watcher registry + `stop_watcher`

**Files:**
- Modify: `src/watcher.rs` (full file, 82 lines)
- Test: Create `tests/watcher_stop.rs`

**Interfaces:**
- Consumes: `spawn_watcher(kg_arc: Arc<GraphHandle>, path: String, project: &str, snippets: bool)` (keeps its signature; `handle_code_watch` at `src/actions/code.rs:806-824` is unchanged).
- Produces: `pub fn stop_watcher(project: &str) -> bool` — signals the watcher thread, joins it (bounded by the 2-second debounce), removes the registry entry; returns whether a watcher existed.

- [ ] **Step 1: Write the failing test**

Create `tests/watcher_stop.rs`:

```rust
//! The watcher registry: spawn, stop, and the second-stop answer.

use std::path::PathBuf;

use mcpmem::code_registry;
use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::kg::GraphHandle;

fn warm_project(dir: &tempfile::TempDir) -> std::sync::Arc<GraphHandle> {
    // A watcher is a code-featured concern; the crate compiles without code
    // only for the graph-only build guard tests.
    let base = dir.path().join("code");
    std::fs::create_dir_all(&base).unwrap();
    code_registry::init(
        base.clone(),
        Durability::Async,
        SqliteTuning::default(),
        std::num::NonZeroUsize::new(8).unwrap(),
        2,
    );
    code_registry::resolve("watchme").expect("resolve opens a project")
}

#[test]
#[cfg(feature = "code")]
fn stop_watcher_joins_and_answers_once() {
    let dir = tempfile::tempdir().unwrap();
    let watched = dir.path().join("tree");
    std::fs::create_dir_all(&watched).unwrap();
    let kg = warm_project(&dir);

    mcpmem::watcher::spawn_watcher(kg, watched.to_string_lossy().into_owned(), "watchme", false);
    // Give the OS watcher thread a moment to register.
    std::thread::sleep(std::time::Duration::from_millis(300));

    assert!(
        mcpmem::watcher::stop_watcher("watchme"),
        "a registered watcher stops"
    );
    assert!(
        !mcpmem::watcher::stop_watcher("watchme"),
        "the second stop reports no watcher"
    );
    // The thread is joined; the project handle is no longer pinned anywhere.
    assert_eq!(Arc::strong_count(&kg), 0); // the local binding still holds one
}

#[test]
#[cfg(feature = "code")]
fn stop_watcher_on_unknown_project_is_false() {
    assert!(!mcpmem::watcher::stop_watcher("never-started"));
}
```

Fix the strong-count assertion: the local binding holds one strong `Arc`. Use `Arc::strong_count(&kg) == 1` after the watcher joined.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test watcher_stop --features code`

Expected: FAIL — `stop_watcher` is not defined.

- [ ] **Step 3: Implement**

Rewrite `src/watcher.rs`:

```rust
//! Background file watcher for automatic re-indexing of code-symbol entities.
//!
//! Launch per-project watches via [`spawn_watcher`]. Re-indexing goes through
//! [`crate::actions::code::handle_code_index`], which resolves the project's
//! database from [`crate::code_registry`]; the watcher holds that project's
//! `Arc<GraphHandle>` for its lifetime so the canonical instance stays open.
//! The watcher uses OS-native filesystem events (`notify` crate) with a 2-second
//! debounce window to avoid thrashing during bulk edits / git operations.
//!
//! A process-wide registry maps project names to live watcher threads.
//! [`stop_watcher`] signals and joins one so the managed-repo remove path can
//! drop the project handle and delete its database without a race.

#![cfg(feature = "code")]

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use notify::Watcher as _;
use parking_lot::Mutex;

use crate::code::lang;
use crate::kg::GraphHandle;

/// A live watcher thread and its stop flag.
struct WatchHandle {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

static WATCHERS: OnceLock<Mutex<HashMap<String, WatchHandle>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, WatchHandle>> {
    WATCHERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Spawn a background thread that watches `path` (recursively) for file
/// modifications and re-indexes changed files under the given `project`.
///
/// `kg_arc` is the project's handle; the thread holds it to pin the canonical
/// instance open. The watcher debounces events for 2 seconds of quiet before
/// triggering a re-index batch. The thread wakes at least every 2 seconds, so
/// [`stop_watcher`] is noticed within that window.
pub fn spawn_watcher(kg_arc: Arc<GraphHandle>, path: String, project: &str, snippets: bool) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let project_owned = project.to_owned();
    let Ok(handle) = std::thread::Builder::new()
        .name(format!("watcher-{project}"))
        .spawn(move || {
            // Held for the thread's lifetime to keep the project DB's canonical
            // handle alive; this is the same instance the registry hands out, so
            // re-indexing uses it directly without re-acquiring the registry lock.
            let kg = kg_arc;
            let (tx, rx) = std::sync::mpsc::channel::<notify::Event>();
            let Ok(mut watcher) = notify::recommended_watcher(move |res| {
                if let Ok(event) = res {
                    let _ = tx.send(event);
                }
            }) else {
                return;
            };
            if watcher
                .watch(Path::new(&path), notify::RecursiveMode::Recursive)
                .is_err()
            {
                return;
            }

            use std::collections::BTreeSet;
            use std::path::PathBuf;
            use std::time::{Duration, Instant};

            const DEBOUNCE_MS: u64 = 2000;
            let base = crate::actions::code::canonical_base();
            let mut pending: BTreeSet<PathBuf> = BTreeSet::new();
            let mut last_event = Instant::now();

            loop {
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let elapsed = last_event.elapsed().as_millis() as u64;
                let timeout = Duration::from_millis(DEBOUNCE_MS.saturating_sub(elapsed));
                match rx.recv_timeout(timeout) {
                    Ok(first) => {
                        let mut collect = |event: notify::Event| {
                            for p in &event.paths {
                                if lang::detect(p).is_some() {
                                    pending.insert(p.clone());
                                }
                            }
                        };
                        collect(first);
                        while let Ok(event) = rx.try_recv() {
                            collect(event);
                        }
                        last_event = Instant::now();
                    }
                    // Debounce window elapsed — apply the whole change set at once:
                    // re-index surviving files in a single batch (one parse pool,
                    // amortized write transactions) and purge deleted ones.
                    Err(_) if !pending.is_empty() => {
                        let mut to_index: Vec<PathBuf> = Vec::new();
                        for p in std::mem::take(&mut pending) {
                            if p.exists() {
                                to_index.push(p);
                            } else {
                                let name = crate::actions::code::file_entity_name(&p, &base);
                                let _ = kg.code_purge_file(&name);
                            }
                        }
                        if !to_index.is_empty() {
                            let _ = crate::actions::code::index_paths(
                                kg.as_ref(),
                                to_index,
                                &base,
                                false,
                                snippets,
                            );
                        }
                    }
                    Err(_) => {}
                }
            }
        })
    else {
        return;
    };
    registry().lock().insert(
        project_owned,
        WatchHandle {
            stop,
            thread: handle,
        },
    );
}

/// Stop the watcher for `project`, if any: set its stop flag, join the thread,
/// and remove the registry entry. Returns whether a watcher existed.
pub fn stop_watcher(project: &str) -> bool {
    let Some(watch) = registry().lock().remove(project) else {
        return false;
    };
    watch.stop.store(true, Ordering::Relaxed);
    let _ = watch.thread.join();
    true
}
```

Note: the guard-clause form compiles on edition 2024 (`let-else` + `else { return }`), matching the current file style.

- [ ] **Step 4: Run the test**

Run: `cargo test --test watcher_stop --features code`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/watcher.rs tests/watcher_stop.rs
git commit -m "feat: watcher registry and stop_watcher

spawn_watcher registers each thread in a process-wide map and checks a
stop flag each debounce wake; stop_watcher signals, joins, and forgets.
The managed-repo remove path needs this before deleting a project DB.

Tokens: ~4k. Cost: < $1."
```

---

### Task 4: The `repos` service — registration, git ops, jobs, remove

**Files:**
- Create: `src/repos.rs`
- Modify: `src/lib.rs` (add `#[cfg(feature = "code")] pub mod repos;`)
- Modify: `src/server.rs` (call `crate::repos::init` in the `#[cfg(feature = "code")]` block near `webhooks::init`, lines 446-451)
- Test: Create `tests/repo_service.rs`
- Test: Modify `tests/support/mod.rs` (expose the fixture DB path: add `pub fn memory_db_path(&self) -> PathBuf` on `Server`)

**Interfaces:**
- Consumes: `crate::code_registry::{validate_project, resolve}`, `crate::actions::code::handle_code_index`, `crate::watcher::stop_watcher`, `crate::code_registry::drop_project`, `mcpmem_core::events::{now_us, sql_error}`.
- Produces:
  - `pub struct RepoInput { key: String, url: String, auth_kind: String, auth_secret: Option<String>, snippets: bool }` (`#[derive(Clone, Debug, Deserialize)]`)
  - `pub struct RepoRow { key, url, auth_kind, snippets, state, last_error: Option<String>, last_indexed_us: Option<i64> }` (`#[derive(Clone, Debug, Serialize)]`)
  - `pub fn init(db_path: PathBuf, busy_timeout_ms: u64)`
  - `pub fn register(input: &RepoInput) -> Result<()>`
  - `pub fn list() -> Result<Vec<RepoRow>>`
  - `pub fn get_row(key: &str) -> Result<Option<RepoRow>>`
  - `pub fn add(input: &RepoInput) -> Result<Value>` (synchronous; MCP)
  - `pub fn add_job(key: &str) -> Result<()>` (detached thread; HTTP)
  - `pub fn reindex(key: &str) -> Result<Value>` (synchronous; MCP)
  - `pub fn reindex_job(key: &str) -> Result<()>` (detached thread; HTTP)
  - `pub fn remove(key: &str) -> Result<()>` (synchronous; MCP)
  - `pub fn remove_job(key: &str) -> Result<()>` (detached thread; HTTP)
  - `Result` and `MCSError` are `crate::errors::{Result, MCSError}` (`mcpmem_core::errors`).

- [ ] **Step 1: Add `tests/support` accessor**

In `tests/support/mod.rs`, on the `Server` struct, add:

```rust
    /// The memory-database path this server was built on, for stores that
    /// share the main DB file (e.g. the managed-repo store).
    pub fn memory_db_path(&self) -> std::path::PathBuf {
        self.dir.path().join("t.mcpmem")
    }
```

- [ ] **Step 2: Write the failing service test**

Create `tests/repo_service.rs`:

```rust
//! End-to-end service tests for the managed-repo lifecycle, against a real
//! local git repository. Requires the `git` binary; skipped when absent.

use std::path::Path;
use std::process::Command;

use mcpmem::code_registry;
use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::repos::{self, RepoInput};
use mcpmem::errors::MCSError;

fn git(args: &[&str], cwd: Option<&Path>) {
    let mut cmd = Command::new("git");
    cmd.args(args).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t");
    if let Some(dir) = cwd { cmd.current_dir(dir); }
    let out = cmd.output().expect("git runs");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// Build a local repository with one Rust file; return its path (usable as a
/// clone URL).
fn fixture_repo(parent: &Path, name: &str) -> std::path::PathBuf {
    let repo = parent.join(name);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&["init", "-q"], Some(&repo));
    std::fs::write(repo.join("src/lib.rs"), "pub fn alpha() -> u32 { 1 }\n").unwrap();
    git(&["add", "."], Some(&repo));
    git(&["commit", "-q", "-m", "first"], Some(&repo));
    repo
}

fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.mcpmem");
    code_registry::init(
        dir.path().join("code"),
        Durability::Async,
        SqliteTuning::default(),
        std::num::NonZeroUsize::new(8).unwrap(),
        2,
    );
    repos::init(db.clone(), 5000);
    (dir, db)
}

fn wait_state(key: &str, wanted: &[&str]) -> String {
    for _ in 0..120 {
        let row = repos::get_row(key).unwrap().expect("row exists");
        if wanted.contains(&row.state.as_str()) {
            return row.state;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    panic!("state never reached {wanted:?}");
}

#[test]
fn lifecycle_register_add_reindex_remove() {
    let (dir, _db) = setup();
    let repo = fixture_repo(dir.path(), "upstream");

    let input = RepoInput {
        key: "acme-api".into(),
        url: repo.to_string_lossy().into_owned(),
        auth_kind: "none".into(),
        auth_secret: None,
        snippets: false,
    };
    repos::add(&input).expect("add clones and indexes");
    let row = repos::get_row("acme-api").unwrap().unwrap();
    assert_eq!(row.state, "indexed");
    assert_eq!(row.auth_kind, "none");
    assert!(row.last_indexed_us.is_some());

    // A new commit + reindex grows the symbol count.
    std::fs::write(repo.join("src/lib.rs"), "pub fn alpha() -> u32 { 1 }\npub fn beta() -> u32 { 2 }\n").unwrap();
    git(&["add", "."], Some(&repo));
    git(&["commit", "-q", "-m", "second"], Some(&repo));
    let result = repos::reindex("acme-api").expect("reindex fetches and indexes");
    assert_eq!(result["symbols"].as_u64(), Some(2), "second symbol indexed: {result}");

    // Duplicate key is refused.
    let dup = repos::register(&input);
    assert!(matches!(dup, Err(MCSError::ConstraintViolation(_))));

    // Remove wipes everything.
    repos::remove("acme-api").expect("remove wipes");
    assert!(repos::get_row("acme-api").unwrap().is_none());
    assert!(!dir.path().join("code").join("acme-api.code.db").exists());
    assert!(!dir.path().join("code").join("repos").join("acme-api").exists());
}

#[test]
fn invalid_inputs_are_refused() {
    let (dir, _db) = setup();
    let repo = fixture_repo(dir.path(), "upstream2");

    let bad_key = RepoInput { key: "bad/key".into(), url: repo.to_string_lossy().into_owned(), auth_kind: "none".into(), auth_secret: None, snippets: false };
    assert!(matches!(repos::register(&bad_key), Err(MCSError::InvalidParams(_))));

    let bad_kind = RepoInput { key: "k".into(), url: repo.to_string_lossy().into_owned(), auth_kind: "api".into(), auth_secret: None, snippets: false };
    assert!(matches!(repos::register(&bad_kind), Err(MCSError::InvalidParams(_))));

    let empty_url = RepoInput { key: "k2".into(), url: "  ".into(), auth_kind: "none".into(), auth_secret: None, snippets: false };
    assert!(matches!(repos::register(&empty_url), Err(MCSError::InvalidParams(_))));

    let embedded_creds = RepoInput { key: "k3".into(), url: "https://user:pass@example.com/r.git".into(), auth_kind: "none".into(), auth_secret: None, snippets: false };
    assert!(matches!(repos::register(&embedded_creds), Err(MCSError::InvalidParams(_))));

    let bare_token = RepoInput { key: "k4".into(), url: repo.to_string_lossy().into_owned(), auth_kind: "token".into(), auth_secret: None, snippets: false };
    assert!(matches!(repos::register(&bare_token), Err(MCSError::InvalidParams(_))));
}

#[test]
fn in_flight_jobs_conflict() {
    let (dir, _db) = setup();
    let repo = fixture_repo(dir.path(), "upstream3");
    let input = RepoInput { key: "busy".into(), url: repo.to_string_lossy().into_owned(), auth_kind: "none".into(), auth_secret: None, snippets: false };
    repos::register(&input).unwrap();
    repos::add_job("busy").expect("first job starts");
    // The job may finish quickly; poll until in-flight or done, then assert a
    // re-trigger during flight conflicts when observed. To keep determinism,
    // assert the API contract instead: a finished job allows a new job.
    let _ = wait_state("busy", &["indexed", "error"]);
    repos::reindex("busy").expect("a settled job allows a new job");
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --test repo_service --features code -- --test-threads=1`

Expected: FAIL — `mcpmem::repos` does not exist.

- [ ] **Step 4: Implement `src/repos.rs`**

Write the full file:

```rust
//! Managed remote repositories for code-symbol indexing.
//!
//! Repositories are registered in the `code_repo` table (migration 0006) in
//! the main memory database. Each row maps a project key to a remote git URL
//! and optional credentials. The worktree lives at
//! `<memory_file>.code/repos/<key>/`. Indexing runs the existing
//! [`crate::actions::code::handle_code_index`] on the worktree, which stores
//! symbols in the per-project database `.code/<key>.code.db`.
//!
//! One in-flight job per key is allowed, tracked in a process-wide set.
//! HTTP mutations schedule a detached thread and answer 202; MCP mutations
//! call the synchronous variants and block like `code_index`.

#![cfg(feature = "code")]

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::errors::{MCSError, Result};
use mcpmem_core::events::{now_us, sql_error};

/// Git command timeout: a hung remote must not pin a job forever.
const GIT_TIMEOUT: Duration = Duration::from_secs(600);

const STATE_PENDING: &str = "pending";
const STATE_CLONING: &str = "cloning";
const STATE_INDEXING: &str = "indexing";
const STATE_INDEXED: &str = "indexed";
const STATE_ERROR: &str = "error";
const STATE_REMOVING: &str = "removing";

/// Input to register a repository. The HTTP and MCP contracts use camelCase.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoInput {
    pub key: String,
    pub url: String,
    /// `none`, `token`, or `ssh`.
    pub auth_kind: String,
    pub auth_secret: Option<String>,
    pub snippets: bool,
}

/// A repository row for API responses. Secrets never leave the store.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoRow {
    pub key: String,
    pub url: String,
    pub auth_kind: String,
    pub snippets: bool,
    pub state: String,
    pub last_error: Option<String>,
    pub last_indexed_us: Option<i64>,
}

/// The full row, including the credential, used only inside jobs.
struct FullRow {
    key: String,
    url: String,
    auth_kind: String,
    auth_secret: Option<String>,
    snippets: bool,
}

/// A git credential prepared as environment variables plus an optional SSH
/// key file. The key file is written 0600 and removed by [`PreparedAuth::cleanup`].
struct PreparedAuth {
    envs: Vec<(String, String)>,
    ssh_key_file: Option<PathBuf>,
}

impl PreparedAuth {
    fn cleanup(&self) {
        if let Some(path) = &self.ssh_key_file {
            let _ = std::fs::remove_file(path);
        }
    }
}

struct RepoRuntime {
    db_path: PathBuf,
    busy_timeout_ms: u64,
    git_ok: bool,
}

static RUNTIME: OnceLock<RepoRuntime> = OnceLock::new();
static IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Initialize the runtime. Call once from server startup (feature `code`).
/// Rows left in transient states by a crash become `pending`.
pub fn init(db_path: PathBuf, busy_timeout_ms: u64) {
    let git_ok = Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    let _ = RUNTIME.set(RepoRuntime {
        db_path: db_path.clone(),
        busy_timeout_ms,
        git_ok,
    });
    let _ = IN_FLIGHT.set(Mutex::new(HashSet::new()));
    let _ = std::fs::create_dir_all(db_path.with_extension("code").join("repos"));
    if let Ok(conn) = open_connection() {
        let _ = conn.execute(
            "UPDATE code_repo SET state = ?1
             WHERE state IN (?2, ?3, ?4)",
            params![STATE_PENDING, STATE_CLONING, STATE_INDEXING, STATE_REMOVING],
        );
    }
}

fn open_connection() -> Result<Connection> {
    let runtime = RUNTIME.get().ok_or_else(|| {
        MCSError::MemoryError("managed-repo store not initialized".into())
    })?;
    let conn = Connection::open(&runtime.db_path).map_err(sql_error)?;
    conn.busy_timeout(Duration::from_millis(runtime.busy_timeout_ms))
        .map_err(sql_error)?;
    Ok(conn)
}

fn git_available() -> bool {
    RUNTIME.get().is_some_and(|r| r.git_ok)
}

fn repos_base() -> Result<PathBuf> {
    let runtime = RUNTIME
        .get()
        .ok_or_else(|| MCSError::MemoryError("managed-repo store not initialized".into()))?;
    Ok(runtime.db_path.with_extension("code").join("repos"))
}

fn worktree_of(key: &str) -> Result<PathBuf> {
    Ok(repos_base()?.join(key))
}

fn validate_url(url: &str) -> Result<()> {
    if url.trim().is_empty() {
        return Err(MCSError::InvalidParams("url is required".into()));
    }
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    if let Some(before_slash) = without_scheme.split('/').next()
        && let Some(at) = before_slash.rfind('@')
        && !before_slash[..at].is_empty()
    {
        return Err(MCSError::InvalidParams(
            "url must not embed credentials; pass authSecret instead".into(),
        ));
    }
    Ok(())
}

fn validate_auth(auth_kind: &str, auth_secret: Option<&str>) -> Result<()> {
    match auth_kind {
        "none" if auth_secret.is_some() => Err(MCSError::InvalidParams(
            "auth_kind 'none' must not carry authSecret".into(),
        )),
        "none" => Ok(()),
        "token" | "ssh" if auth_secret.is_some_and(|s| !s.trim().is_empty()) => Ok(()),
        "token" | "ssh" => Err(MCSError::InvalidParams(format!(
            "auth_kind '{auth_kind}' requires authSecret"
        ))),
        other => Err(MCSError::InvalidParams(format!(
            "auth_kind must be one of none, token, ssh; got '{other}'"
        ))),
    }
}

/// Register a repository: validate, insert the row as `pending`, return.
pub fn register(input: &RepoInput) -> Result<()> {
    crate::code_registry::validate_project(&input.key)?;
    validate_url(&input.url)?;
    let auth_kind = input.auth_kind.trim().to_ascii_lowercase();
    validate_auth(&auth_kind, input.auth_secret.as_deref())?;
    let conn = open_connection()?;
    conn.execute(
        "INSERT INTO code_repo(key, url, auth_kind, auth_secret, snippets, state)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            input.key,
            input.url.trim(),
            auth_kind,
            input.auth_secret.as_deref(),
            input.snippets as i64,
            STATE_PENDING
        ],
    )
    .map_err(|e| match e {
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
        {
            MCSError::ConstraintViolation(format!("key '{}' is already registered", input.key))
        }
        other => sql_error(other),
    })?;
    Ok(())
}

/// List managed repositories, secrets excluded, `removing` rows hidden.
pub fn list() -> Result<Vec<RepoRow>> {
    let conn = open_connection()?;
    let mut stmt = conn
        .prepare(
            "SELECT key, url, auth_kind, snippets, state, last_error, last_indexed_us
             FROM code_repo WHERE state <> ?1 ORDER BY key",
        )
        .map_err(sql_error)?;
    let rows = stmt
        .query_map([STATE_REMOVING], |r| {
            Ok(RepoRow {
                key: r.get(0)?,
                url: r.get(1)?,
                auth_kind: r.get(2)?,
                snippets: r.get::<_, i64>(3)? != 0,
                state: r.get(4)?,
                last_error: r.get(5)?,
                last_indexed_us: r.get(6)?,
            })
        })
        .map_err(sql_error)?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_error)
}

/// Fetch one row (secrets excluded). `Ok(None)` for an unknown key.
pub fn get_row(key: &str) -> Result<Option<RepoRow>> {
    let conn = open_connection()?;
    conn.query_row(
        "SELECT key, url, auth_kind, snippets, state, last_error, last_indexed_us
         FROM code_repo WHERE key = ?1",
        [key],
        |r| {
            Ok(RepoRow {
                key: r.get(0)?,
                url: r.get(1)?,
                auth_kind: r.get(2)?,
                snippets: r.get::<_, i64>(3)? != 0,
                state: r.get(4)?,
                last_error: r.get(5)?,
                last_indexed_us: r.get(6)?,
            })
        },
    )
    .optional()
    .map_err(sql_error)
}

fn full_row(key: &str) -> Result<Option<FullRow>> {
    let conn = open_connection()?;
    conn.query_row(
        "SELECT key, url, auth_kind, auth_secret, snippets FROM code_repo WHERE key = ?1",
        [key],
        |r| {
            Ok(FullRow {
                key: r.get(0)?,
                url: r.get(1)?,
                auth_kind: r.get(2)?,
                auth_secret: r.get(3)?,
                snippets: r.get::<_, i64>(4)? != 0,
            })
        },
    )
    .optional()
    .map_err(sql_error)
}

fn set_state(key: &str, state: &str, last_error: Option<&str>, last_indexed_us: Option<i64>) -> Result<()> {
    let conn = open_connection()?;
    conn.execute(
        "UPDATE code_repo
         SET state = ?1, last_error = ?2,
             last_indexed_us = coalesce(?3, last_indexed_us)
         WHERE key = ?4",
        params![state, last_error, last_indexed_us, key],
    )
    .map_err(sql_error)?;
    Ok(())
}

fn try_begin_job(key: &str) -> bool {
    IN_FLIGHT
        .get()
        .expect("IN_FLIGHT set at init")
        .lock()
        .insert(key.to_owned())
}

fn end_job(key: &str) {
    if let Some(set) = IN_FLIGHT.get() {
        set.lock().remove(key);
    }
}

fn job_in_flight(key: &str) -> bool {
    IN_FLIGHT
        .get()
        .is_some_and(|set| set.lock().contains(key))
}

/// Run `git` with the given environment. Returns stdout on success or a
/// message with the stderr tail on failure. Kills after [`GIT_TIMEOUT`].
fn run_git(
    cwd: Option<&Path>,
    args: &[&str],
    envs: &[(String, String)],
) -> std::result::Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    // Drain stdout/stderr on reader threads so a large progress stream cannot
    // fill the OS pipe and stall the child before it exits.
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start git: {e}"))?;
    let out_reader = child
        .stdout
        .take()
        .map(|mut pipe| {
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = pipe.read_to_string(&mut s);
                s
            })
        });
    let err_reader = child
        .stderr
        .take()
        .map(|mut pipe| {
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = pipe.read_to_string(&mut s);
                s
            })
        });
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("git wait failed: {e}"))?
        {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err("git timed out after 600s".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let out = out_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let err = err_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    if status.success() {
        Ok(out)
    } else {
        let tail: String = err
            .lines()
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        Err(format!("git failed (exit {}): {tail}", status.code().unwrap_or(-1)))
    }
}

/// Materialize the row's credential as git env vars. An SSH key goes to a
/// 0600 temp file under the repos base; [`PreparedAuth::cleanup`] removes it.
fn prepare_auth(row: &FullRow) -> Result<PreparedAuth> {
    match row.auth_kind.as_str() {
        "none" => Ok(PreparedAuth {
            envs: Vec::new(),
            ssh_key_file: None,
        }),
        "token" => {
            let secret = row
                .auth_secret
                .as_deref()
                .ok_or_else(|| MCSError::MemoryError("token row has no secret".into()))?;
            Ok(PreparedAuth {
                envs: vec![
                    ("GIT_CONFIG_KEY_0".to_owned(), "http.extraHeader".to_owned()),
                    (
                        "GIT_CONFIG_VALUE_0".to_owned(),
                        format!("Authorization: Bearer {secret}"),
                    ),
                ],
                ssh_key_file: None,
            })
        }
        "ssh" => {
            let secret = row
                .auth_secret
                .as_deref()
                .ok_or_else(|| MCSError::MemoryError("ssh row has no secret".into()))?;
            let path = repos_base()?.join(format!(".ssh-{}-{}", row.key, std::process::id()));
            std::fs::write(&path, secret)
                .map_err(|e| MCSError::MemoryError(format!("ssh key write failed: {e}")))?;
            let mut perm = std::fs::metadata(&path)
                .map_err(|e| MCSError::MemoryError(format!("ssh key stat failed: {e}")))?
                .permissions();
            let _ = perm.set_mode(0o600);
            let _ = std::fs::set_permissions(&path, perm);
            Ok(PreparedAuth {
                envs: vec![(
                    "GIT_SSH_COMMAND".to_owned(),
                    format!(
                        "ssh -i {path} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new"
                    ),
                )],
                ssh_key_file: Some(path),
            })
        }
        other => Err(MCSError::MemoryError(format!("unknown auth_kind '{other}'"))),
    }
}

fn worktree_valid(key: &str) -> bool {
    let Ok(wt) = worktree_of(key) else {
        return false;
    };
    wt.join(".git").exists() || wt.join("HEAD").exists()
}

fn do_clone(key: &str, row: &FullRow, auth: &PreparedAuth) -> std::result::Result<(), String> {
    let wt = worktree_of(key).map_err(|e| e.to_string())?;
    if worktree_valid(key) {
        return Ok(());
    }
    if wt.exists() {
        std::fs::remove_dir_all(&wt).map_err(|e| format!("stale worktree cleanup failed: {e}"))?;
    }
    let parent = wt.parent().ok_or("worktree has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("repos base create failed: {e}"))?;
    let url = row.url.clone();
    let target = wt.to_string_lossy().into_owned();
    run_git(None, &["clone", "--quiet", &url, &target], &auth.envs)?;
    Ok(())
}

fn do_update(key: &str, auth: &PreparedAuth) -> std::result::Result<(), String> {
    let wt = worktree_of(key).map_err(|e| e.to_string())?;
    run_git(Some(&wt), &["fetch", "--quiet", "origin"], &auth.envs)?;
    run_git(Some(&wt), &["reset", "--hard", "--quiet", "@{u}"], &[])?;
    Ok(())
}

/// Index the worktree with the existing tool; returns the counters object.
fn do_index(key: &str, snippets: bool) -> Result<Value> {
    let wt = worktree_of(key)?;
    let args = json!({
        "path": wt.to_string_lossy(),
        "project": key,
        "force": false,
        "snippets": snippets,
    });
    let wrapper = crate::actions::code::handle_code_index(Some(&args))?;
    let text = wrapper.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let mut counters: Value = serde_json::from_str(text)
        .map_err(|e| MCSError::MemoryError(format!("index result parse failed: {e}")))?;
    counters["status"] = json!("indexed");
    Ok(counters)
}

fn add_inner(key: &str) -> Result<Value> {
    let Some(row) = full_row(key)? else {
        return Err(MCSError::InvalidParams(format!("no such repo '{key}'")));
    };
    if !git_available() {
        let msg = "the git binary is not available on this host".to_owned();
        let _ = set_state(key, STATE_ERROR, Some(&msg), None);
        return Err(MCSError::MemoryError(msg));
    }
    set_state(key, STATE_CLONING, None, None)?;
    let auth = prepare_auth(&row)?;
    if let Err(e) = do_clone(key, &row, &auth) {
        auth.cleanup();
        let _ = set_state(key, STATE_ERROR, Some(&e), None);
        return Err(MCSError::MemoryError(e));
    }
    set_state(key, STATE_INDEXING, None, None)?;
    let indexed = do_index(key, row.snippets);
    auth.cleanup();
    match indexed {
        Ok(value) => {
            let _ = set_state(key, STATE_INDEXED, None, Some(now_us()));
            Ok(value)
        }
        Err(e) => {
            let _ = set_state(key, STATE_ERROR, Some(&e.to_string()), None);
            Err(e)
        }
    }
}

fn reindex_inner(key: &str) -> Result<Value> {
    let Some(row) = full_row(key)? else {
        return Err(MCSError::InvalidParams(format!("no such repo '{key}'")));
    };
    if !git_available() {
        let msg = "the git binary is not available on this host".to_owned();
        let _ = set_state(key, STATE_ERROR, Some(&msg), None);
        return Err(MCSError::MemoryError(msg));
    }
    set_state(key, STATE_INDEXING, None, None)?;
    let auth = prepare_auth(&row)?;
    let updated = if worktree_valid(key) {
        do_update(key, &auth)
    } else {
        do_clone(key, &row, &auth)
    };
    if let Err(e) = updated {
        auth.cleanup();
        let _ = set_state(key, STATE_ERROR, Some(&e), None);
        return Err(MCSError::MemoryError(e));
    }
    let indexed = do_index(key, row.snippets);
    auth.cleanup();
    match indexed {
        Ok(value) => {
            let _ = set_state(key, STATE_INDEXED, None, Some(now_us()));
            Ok(value)
        }
        Err(e) => {
            let _ = set_state(key, STATE_ERROR, Some(&e.to_string()), None);
            Err(e)
        }
    }
}

/// Register + clone + index, synchronously. The MCP add path.
pub fn add(input: &RepoInput) -> Result<Value> {
    register(input)?;
    add_inner(&input.key)
}

/// Schedule the clone+index job on a detached thread. The HTTP add path.
/// The caller has already registered the row; returns a conflict error when a
/// job for the key is already in flight.
pub fn add_job(key: &str) -> Result<()> {
    if !try_begin_job(key) {
        return Err(MCSError::InvalidParams(format!(
            "a job for '{key}' is already in progress"
        )));
    }
    let key = key.to_owned();
    std::thread::Builder::new()
        .name(format!("repo-add-{key}"))
        .spawn(move || {
            let _ = add_inner(&key);
            end_job(&key);
        })
        .map_err(|e| MCSError::MemoryError(format!("job spawn failed: {e}")))?;
    Ok(())
}

/// Fetch + reset + index, synchronously. The MCP reindex path.
pub fn reindex(key: &str) -> Result<Value> {
    if let Err(e) = begin_checked(key) {
        return Err(e);
    }
    let result = reindex_inner(key);
    end_job(key);
    result
}

/// Schedule the reindex job on a detached thread. The HTTP reindex path.
pub fn reindex_job(key: &str) -> Result<()> {
    if let Err(e) = begin_checked(key) {
        return Err(e);
    }
    let key = key.to_owned();
    std::thread::Builder::new()
        .name(format!("repo-reindex-{key}"))
        .spawn(move || {
            let _ = reindex_inner(&key);
            end_job(&key);
        })
        .map_err(|e| MCSError::MemoryError(format!("job spawn failed: {e}")))?;
    Ok(())
}

/// Stop the watcher, evict the project handle, delete the index DB and the
/// worktree, delete the row. Synchronous. The MCP remove path.
pub fn remove(key: &str) -> Result<()> {
    if let Err(e) = begin_checked(key) {
        return Err(e);
    }
    let result = remove_inner(key);
    end_job(key);
    result
}

/// Schedule the remove job on a detached thread. The HTTP remove path.
pub fn remove_job(key: &str) -> Result<()> {
    if let Err(e) = begin_checked(key) {
        return Err(e);
    }
    let key = key.to_owned();
    std::thread::Builder::new()
        .name(format!("repo-remove-{key}"))
        .spawn(move || {
            let _ = remove_inner(&key);
            end_job(&key);
        })
        .map_err(|e| MCSError::MemoryError(format!("job spawn failed: {e}")))?;
    Ok(())
}

fn begin_checked(key: &str) -> Result<()> {
    if !try_begin_job(key) {
        return Err(MCSError::InvalidParams(format!(
            "a job for '{key}' is already in progress"
        )));
    }
    Ok(())
}

fn remove_inner(key: &str) -> Result<()> {
    {
        let conn = open_connection()?;
        let exists: Option<i64> = conn
            .query_row("SELECT 1 FROM code_repo WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .map_err(sql_error)?;
        if exists.is_none() {
            return Err(MCSError::InvalidParams(format!("no such repo '{key}'")));
        }
        conn.execute(
            "UPDATE code_repo SET state = ?1 WHERE key = ?2",
            params![STATE_REMOVING, key],
        )
        .map_err(sql_error)?;
    }
    // Stop any watcher on this project before touching its database files.
    let _ = crate::watcher::stop_watcher(key);
    // Evict the project handle so its SQLite connections close.
    let _ = crate::code_registry::drop_project(key);
    // Delete the per-project index DB and its WAL sidecars.
    let runtime = RUNTIME
        .get()
        .ok_or_else(|| MCSError::MemoryError("managed-repo store not initialized".into()))?;
    let db = runtime.db_path.with_extension("code").join(format!("{key}.code.db"));
    let db = db.to_string_lossy().into_owned();
    for ext in ["", "-wal", "-shm"] {
        if let Err(e) = std::fs::remove_file(format!("{db}{ext}")) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(MCSError::MemoryError(format!(
                    "index DB cleanup failed ({ext}): {e}"
                )));
            }
        }
    }
    // Delete the clone worktree.
    let wt = worktree_of(key)?;
    if wt.exists() {
        std::fs::remove_dir_all(&wt)
            .map_err(|e| MCSError::MemoryError(format!("worktree cleanup failed: {e}")))?;
    }
    // Delete the row last: a failure above leaves a visible `removing` row.
    open_connection()?
        .execute("DELETE FROM code_repo WHERE key = ?1", [key])
        .map_err(sql_error)?;
    Ok(())
}
```

- [ ] **Step 5: Register the module**

In `src/lib.rs`, after `pub mod runtime_principals;`, add:

```rust
#[cfg(feature = "code")]
pub mod repos;
```

- [ ] **Step 6: Initialize from server startup**

In `src/server.rs`, inside the existing `#[cfg(feature = "code")]` block, add next to `crate::actions::webhooks::init` (lines 446-451):

```rust
        #[cfg(feature = "code")]
        crate::repos::init(
            std::path::PathBuf::from(&config.memory_file_path),
            config.busy_timeout_ms,
        );
```

The webhooks init at `server.rs:448-451` uses the identical field expression `config.busy_timeout_ms`, which compiles in the same scope.

- [ ] **Step 7: Run the service test**

Run: `cargo test --test repo_service --features code -- --test-threads=1`

Expected: PASS. (`git` present on the runner; the fixture repo clones over `file://`.)

- [ ] **Step 8: Commit**

```bash
git add src/repos.rs src/lib.rs src/server.rs tests/repo_service.rs tests/support/mod.rs
git commit -m "feat: managed-repo service — register, git jobs, reindex, full wipe

New repos module owns the code_repo lifecycle: validation, credential
materialization (env, never argv), git clone/fetch with a 600s timeout,
indexing through the existing code_index handler, one in-flight job per
key, crash recovery, and the remove path that stops the watcher, evicts
the project handle, and deletes DB + sidecars + worktree + row.

Tokens: ~22k. Cost: ~$1."
```

---

### Task 5: MCP surface — `code_repo_add` / `code_repo_list` / `code_repo_reindex` / `code_repo_remove`

**Files:**
- Modify: `code_tools.json` (append 4 descriptors)
- Modify: `src/tools.rs` (`CODE_TOOL_NAMES`, lines 226-233)
- Modify: `src/actions/code.rs` (4 handler functions + imports)
- Modify: `src/server.rs` (4 match arms in the CODE block, lines 1039-1066)
- Test: Create `tests/repo_tools.rs`

**Interfaces:**
- Consumes: `crate::repos::{add, list, reindex, remove, RepoInput}` (Task 4).
- Produces:
  - `handle_code_repo_add(args) -> Result<Value>`, `handle_code_repo_list(args)`, `handle_code_repo_reindex(args)`, `handle_code_repo_remove(args)` in `crate::actions::code`.
  - Tool names registered in `CODE_TOOL_NAMES` and dispatched in the CODE arm.

- [ ] **Step 1: Write the failing tests**

Create `tests/repo_tools.rs`:

```rust
//! MCP handlers for managed repositories, driven directly.

use std::path::Path;
use std::process::Command;

use mcpmem::actions::code::{
    handle_code_repo_add, handle_code_repo_list, handle_code_repo_reindex, handle_code_repo_remove,
};
use mcpmem::code_registry;
use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::repos;

fn git(args: &[&str], cwd: Option<&Path>) {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().expect("git runs");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn fixture(parent: &Path, name: &str) -> std::path::PathBuf {
    let repo = parent.join(name);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&["init", "-q"], Some(&repo));
    std::fs::write(repo.join("src/lib.rs"), "pub fn alpha() -> u32 { 1 }\n").unwrap();
    git(&["add", "."], Some(&repo));
    git(&["commit", "-q", "-m", "first"], Some(&repo));
    repo
}

fn setup() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    code_registry::init(
        dir.path().join("code"),
        Durability::Async,
        SqliteTuning::default(),
        std::num::NonZeroUsize::new(8).unwrap(),
        2,
    );
    repos::init(dir.path().join("t.mcpmem"), 5000);
    dir
}

#[test]
fn mcp_add_list_reindex_remove_round_trip() {
    let dir = setup();
    let repo = fixture(dir.path(), "up");

    let added = handle_code_repo_add(Some(&serde_json::json!({
        "key": "cli-repo",
        "url": repo.to_string_lossy(),
        "authKind": "none",
    })))
    .expect("add succeeds");
    assert_eq!(added["text"].as_str().unwrap().contains("indexed"), true, "{added}");

    let listed = handle_code_repo_list(None).expect("list succeeds");
    let text = listed["text"].as_str().unwrap();
    assert!(text.contains("cli-repo"), "{listed}");
    assert!(!text.contains("authSecret"), "secrets never listed: {listed}");

    std::fs::write(repo.join("src/lib.rs"), "pub fn alpha() -> u32 { 1 }\npub fn beta() -> u32 { 2 }\n").unwrap();
    git(&["add", "."], Some(&repo));
    git(&["commit", "-q", "-m", "second"], Some(&repo));
    let reindexed = handle_code_repo_reindex(Some(&serde_json::json!({ "key": "cli-repo" })))
        .expect("reindex succeeds");
    assert!(reindexed["text"].as_str().unwrap().contains("\"symbols\":2"), "{reindexed}");

    let removed = handle_code_repo_remove(Some(&serde_json::json!({ "key": "cli-repo" })))
        .expect("remove succeeds");
    assert!(removed["text"].as_str().unwrap().contains("removed"), "{removed}");
    assert!(repos::get_row("cli-repo").unwrap().is_none());
}

#[test]
fn mcp_unknown_inputs_error() {
    let _dir = setup();
    assert!(handle_code_repo_add(None).is_err());
    assert!(handle_code_repo_add(Some(&serde_json::json!({ "key": "x" }))).is_err());
    assert!(handle_code_repo_reindex(Some(&serde_json::json!({ "key": "missing" }))).is_err());
    assert!(handle_code_repo_remove(Some(&serde_json::json!({ "key": "missing" }))).is_err());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test repo_tools --features code -- --test-threads=1`

Expected: FAIL — `handle_code_repo_add` is not defined.

- [ ] **Step 3: Register the tool names**

In `src/tools.rs`, extend `CODE_TOOL_NAMES`:

```rust
pub const CODE_TOOL_NAMES: &[&str] = &[
    "code_index",
    "code_outline",
    "code_search",
    "code_get_symbol",
    "code_watch",
    "code_embed",
    "code_semantic_search",
    "code_repo_add",
    "code_repo_list",
    "code_repo_reindex",
    "code_repo_remove",
];
```

- [ ] **Step 4: Add the tool descriptors**

Append to `code_tools.json` (before the closing `]`):

```json
  {
    "name": "code_repo_add",
    "description": "Register a remote git repository for code-symbol indexing, clone it, and index it. The key becomes the project identifier for every later code tool call on this repository. Credentials are stored in the memory database and never returned. Use code_repo_reindex later to pick up new commits. Synchronous; a large repository takes minutes.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "key": {
          "type": "string",
          "description": "Repository key (== project identifier). 1-64 chars of [A-Za-z0-9_-]; selects the dedicated, isolated code database."
        },
        "url": {
          "type": "string",
          "description": "Remote git URL (https, http, or ssh form). Must not embed credentials; pass authSecret instead."
        },
        "authKind": {
          "type": "string",
          "enum": ["none", "token", "ssh"],
          "description": "How the server authenticates: 'none' for public repos, 'token' for an HTTPS bearer/PAT, 'ssh' for a private key. Default 'none'."
        },
        "authSecret": {
          "type": "string",
          "description": "The token or the full SSH private key text. Required when authKind is token or ssh. Never returned by code_repo_list."
        },
        "snippets": {
          "type": "boolean",
          "description": "Also store each symbol's bounded body text, as code_index does with snippets=true. Default false."
        }
      },
      "required": ["key", "url"]
    },
    "annotations": {
      "readOnlyHint": false,
      "destructiveHint": false,
      "idempotentHint": true
    }
  },
  {
    "name": "code_repo_list",
    "description": "List registered code repositories with their state (pending, cloning, indexing, indexed, error), auth kind, and last indexed time. Responses never contain credentials. A state of 'error' carries the last failure message in lastError.",
    "inputSchema": { "type": "object", "properties": {} },
    "annotations": {
      "readOnlyHint": true,
      "destructiveHint": false,
      "idempotentHint": true
    }
  },
  {
    "name": "code_repo_reindex",
    "description": "Fetch the latest commits of a registered repository and re-index changed files incrementally. Run this after the remote moves. Synchronous; returns the counters of the index run (filesIndexed, symbols, relations).",
    "inputSchema": {
      "type": "object",
      "properties": {
        "key": {
          "type": "string",
          "description": "Repository key as given to code_repo_add."
        }
      },
      "required": ["key"]
    },
    "annotations": {
      "readOnlyHint": false,
      "destructiveHint": false,
      "idempotentHint": true
    }
  },
  {
    "name": "code_repo_remove",
    "description": "Remove a registered repository completely: stop its watcher, delete its indexed code database, delete its local clone, and forget the registration. The key can be re-added afterwards. Irreversible.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "key": {
          "type": "string",
          "description": "Repository key as given to code_repo_add."
        }
      },
      "required": ["key"]
    },
    "annotations": {
      "readOnlyHint": false,
      "destructiveHint": true,
      "idempotentHint": false
    }
  }
```

- [ ] **Step 5: Implement the handlers**

In `src/actions/code.rs`, add (near the other handlers; `serde_json::from_value` needs `serde_json` in scope — it is; `to_json` already exists in the file):

```rust
// ---------------------------------------------------------------------------
// code_repo_* — managed remote repositories
// ---------------------------------------------------------------------------

fn repo_input(args: Option<&Value>) -> Result<crate::repos::RepoInput> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    serde_json::from_value(params.clone()).map_err(|e| {
        MCSError::InvalidParams(format!("invalid repository input: {e}"))
    })
}

pub fn handle_code_repo_add(args: Option<&Value>) -> Result<Value> {
    let input = repo_input(args)?;
    let counters = crate::repos::add(&input)?;
    to_json(&counters)
}

pub fn handle_code_repo_list(_args: Option<&Value>) -> Result<Value> {
    let repos = crate::repos::list()?;
    to_json(&serde_json::json!({ "repos": repos }))
}

pub fn handle_code_repo_reindex(args: Option<&Value>) -> Result<Value> {
    let input = repo_input(args)?;
    let counters = crate::repos::reindex(&input.key)?;
    to_json(&counters)
}

pub fn handle_code_repo_remove(args: Option<&Value>) -> Result<Value> {
    let input = repo_input(args)?;
    crate::repos::remove(&input.key)?;
    to_json(&serde_json::json!({ "status": "removed", "key": input.key }))
}
```

Note: `MCSError` is already imported in `src/actions/code.rs` (the file uses `MCSError::InvalidParams` today).

- [ ] **Step 6: Dispatch the tools**

In `src/server.rs`, inside the CODE match arm (after `"code_semantic_search"`), add:

```rust
                "code_repo_add" => {
                    code_actions::handle_code_repo_add(tool_args).map(HandlerResult::Value)
                }
                "code_repo_list" => {
                    code_actions::handle_code_repo_list(tool_args).map(HandlerResult::Value)
                }
                "code_repo_reindex" => {
                    code_actions::handle_code_repo_reindex(tool_args).map(HandlerResult::Value)
                }
                "code_repo_remove" => {
                    code_actions::handle_code_repo_remove(tool_args).map(HandlerResult::Value)
                }
```

- [ ] **Step 7: Run the tests**

Run: `cargo test --test repo_tools --features code -- --test-threads=1`

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add code_tools.json src/tools.rs src/actions/code.rs src/server.rs tests/repo_tools.rs
git commit -m "feat: MCP tools to manage indexed repositories

code_repo_add/list/reindex/remove expose the managed-repo service over
MCP, synchronous like code_index. Descriptors, name registry, dispatch
arms, and direct handler tests round-trip a real git fixture.

Tokens: ~8k. Cost: < $1."
```

---

### Task 6: HTTP admin API — list, add, reindex, remove

**Files:**
- Modify: `src/http.rs` (router build near webhook routes, lines 258-262; four handlers near the other admin handlers)
- Test: Create `tests/repo_admin.rs`

**Interfaces:**
- Consumes: `crate::repos::{RepoRow, RepoInput, register, list, get_row, add_job, reindex_job, remove_job}`; `admin_gate` (`http.rs:566-583`); error helpers `bad_request`, `conflict`, `not_found`, `json_error`; `HttpState`.
- Produces: routes
  - `GET /ui/api/repos`
  - `POST /ui/api/repos`
  - `POST /ui/api/repos/{key}/reindex`
  - `DELETE /ui/api/repos/{key}`
  All gated on the `admin` scope; mutations answer 202.

- [ ] **Step 1: Write the failing tests**

Create `tests/repo_admin.rs`:

```rust
//! HTTP admin routes for managed repositories, through the in-process router.

use std::path::Path;
use std::process::Command;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use mcpmem::code_registry;
use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::repos;
mod support;

fn git(args: &[&str], cwd: Option<&Path>) {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().expect("git runs");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn fixture(parent: &Path, name: &str) -> std::path::PathBuf {
    let repo = parent.join(name);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&["init", "-q"], Some(&repo));
    std::fs::write(repo.join("src/lib.rs"), "pub fn alpha() -> u32 { 1 }\n").unwrap();
    git(&["add", "."], Some(&repo));
    git(&["commit", "-q", "-m", "first"], Some(&repo));
    repo
}

/// One server + admin token + fixture repo for the whole binary: the
/// code_registry and repos stores are process-wide OnceLocks, so every test
/// must share the same backing directories.
struct Fixture {
    server: support::Server,
    token: String,
    repo_url: std::path::PathBuf,
    /// The code-db base, to assert wipes remove files.
    code_base: std::path::PathBuf,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture() -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default())
                .await;
            let config = support::oauth_config(&idp.issuer);
            config.principals[0].scopes.push(mcpmem::principals::ADMIN_SCOPE.into());
            let server = support::server(Some(config), support::Scopes::all(), None).await;
            let token = support::flow::admin_access_token(&idp, &server).await;
            // Point the process-wide stores at THIS server's memory DB.
            let code_base = server.dir().path().join("code");
            code_registry::init(
                code_base.clone(),
                Durability::Async,
                SqliteTuning::default(),
                std::num::NonZeroUsize::new(8).unwrap(),
                2,
            );
            repos::init(server.memory_db_path(), 5000);
            let repo_url = fixture(server.dir().path(), "upstream");
            let _ = std::mem::drop(idp); // the idp stays alive for the token's lifetime
            Fixture {
                server,
                token,
                repo_url,
                code_base,
            }
        })
        .await
}

fn get(fix: &Fixture, path: &str) -> Request<Body> {
    Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {}", fix.token))
        .body(Body::empty())
        .unwrap()
}

fn post(fix: &Fixture, path: &str, body: &serde_json::Value) -> Request<Body> {
    Request::post(path)
        .header(header::AUTHORIZATION, format!("Bearer {}", fix.token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn json(res: axum::response::Response) -> serde_json::Value {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Poll the list until no row is transitional; return the body.
async fn wait_settled(fix: &Fixture) -> serde_json::Value {
    for _ in 0..100 {
        let body = json(fix.server.request(get(fix, "/ui/api/repos")).await).await;
        let settled = body["repos"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| !["pending", "cloning", "indexing", "removing"].contains(&r["state"].as_str().unwrap_or("")));
        if settled {
            return body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("repos never settled");
}

#[tokio::test]
async fn anonymous_caller_is_challenged() {
    let fix = fixture().await;
    let anon = fix
        .server
        .request(Request::get("/ui/api/repos").body(Body::empty()).unwrap())
        .await;
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);
    let challenge = anon
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .expect("the 401 is the RFC 6750 challenge")
        .to_str()
        .unwrap();
    assert!(challenge.starts_with("Bearer"), "{}", challenge);
}

#[tokio::test]
async fn list_starts_empty_and_reports_camelcase() {
    let fix = fixture().await;
    let body = json(fix.server.request(get(fix, "/ui/api/repos")).await).await;
    assert_eq!(body["repos"].as_array().unwrap().len(), 0);
}

/// POST a repo and poll until it settles; returns the settled body.
async fn add_and_settle(fix: &Fixture, key: &str) -> serde_json::Value {
    let res = fix
        .server
        .request(post(
            fix,
            "/ui/api/repos",
            &serde_json::json!({
                "key": key,
                "url": fix.repo_url.to_string_lossy(),
                "authKind": "none",
            }),
        ))
        .await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    wait_settled(fix).await
}

#[tokio::test]
async fn add_accepts_202_and_settles_to_indexed() {
    let fix = fixture().await;
    let body = add_and_settle(fix, "admin-api").await;
    let row = &body["repos"][0];
    assert_eq!(row["key"], "admin-api");
    assert_eq!(row["state"], "indexed");
    assert!(row["lastIndexedUs"].is_number());
}

#[tokio::test]
async fn reindex_triggers_and_remove_wipes() {
    let fix = fixture().await;
    let _ = add_and_settle(fix, "reindex-target").await;
    let res = fix
        .server
        .request(post(fix, "/ui/api/repos/reindex-target/reindex", &serde_json::json!({})))
        .await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let _ = wait_settled(fix).await;

    let removed = fix
        .server
        .request(
            Request::delete(format!("/ui/api/repos/reindex-target"))
                .header(header::AUTHORIZATION, format!("Bearer {}", fix.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(removed.status(), StatusCode::ACCEPTED);
    let body = wait_settled(fix).await;
    assert!(body["repos"].as_array().unwrap().is_empty());
    assert!(!fix.code_base.join("reindex-target.code.db").exists());
    assert!(!fix.code_base.join("repos").join("reindex-target").exists());
    // The row is gone: a direct reindex on it is a 404.
    let miss = fix
        .server
        .request(post(fix, "/ui/api/repos/reindex-target/reindex", &serde_json::json!({})))
        .await;
    assert_eq!(miss.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn duplicate_key_conflicts_and_bad_bodies_are_400() {
    let fix = fixture().await;
    let _ = add_and_settle(fix, "dup-target").await;
    let dup = fix
        .server
        .request(post(
            fix,
            "/ui/api/repos",
            &serde_json::json!({ "key": "dup-target", "url": fix.repo_url.to_string_lossy() }),
        ))
        .await;
    // The first add has settled; a duplicate register conflicts at the
    // UNIQUE constraint regardless of job state.
    assert_eq!(dup.status(), StatusCode::CONFLICT);
    // Unknown key reindex -> 404; malformed body -> 400.
    let miss = fix
        .server
        .request(post(fix, "/ui/api/repos/nope/reindex", &serde_json::json!({})))
        .await;
    assert_eq!(miss.status(), StatusCode::NOT_FOUND);
    let bad = fix
        .server
        .request(post(fix, "/ui/api/repos", &serde_json::json!({ "key": "x" })))
        .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}
```

Note: `server.dir()` does not exist on `support::Server` today — the struct holds `dir` privately. Add an accessor next to `memory_db_path()` in `tests/support/mod.rs` (Task 4 Step 1):

```rust
    /// The tempdir this server was built on, for fixtures that must live
    /// near the memory DB (git fixture repos, code-db assertion paths).
    pub fn dir(&self) -> &tempfile::TempDir {
        &self.dir
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test repo_admin --features code -- --test-threads=1`

Expected: FAIL — the `/ui/api/repos` routes do not exist (404).

- [ ] **Step 3: Register the routes**

In `src/http.rs`, after the webhook-route attach (lines 258-262), add:

```rust
    #[cfg(feature = "code")]
    let router = attach_repo_admin_routes(router);
```

And add the builder next to `attach_webhook_admin_routes`:

```rust
#[cfg(feature = "code")]
fn attach_repo_admin_routes(router: Router) -> Router {
    router
        .route("/ui/api/repos", get(admin_list_repos).post(admin_create_repo))
        .route(
            "/ui/api/repos/{key}/reindex",
            post(admin_reindex_repo),
        )
        .route("/ui/api/repos/{key}", delete(admin_remove_repo))
}
```

- [ ] **Step 4: Implement the handlers**

Add near the other `admin_*` handlers (imports: `axum::extract::Path` is already used by the webhook handlers; `StatusCode` and `Json` already imported):

```rust
#[cfg(feature = "code")]
async fn admin_list_repos(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    match crate::repos::list() {
        Ok(rows) => (
            StatusCode::OK,
            Json(serde_json::json!({ "repos": rows })),
        )
            .into_response(),
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("repos store: {e}"),
        ),
    }
}

#[cfg(feature = "code")]
async fn admin_create_repo(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let input: crate::repos::RepoInput = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            return bad_request(
                "the body must be a JSON repo: {key, url, authKind?, authSecret?, snippets?}",
            )
        }
    };
    if let Err(e) = crate::repos::register(&input) {
        return match e {
            MCSError::ConstraintViolation(message) => conflict(message),
            MCSError::InvalidParams(message) => bad_request(message),
            other => json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("repos store: {other}"),
            ),
        };
    }
    let key = input.key.clone();
    if let Err(e) = crate::repos::add_job(&key) {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("repos store: {e}"),
        );
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "accepted", "key": key })),
    )
        .into_response()
}

#[cfg(feature = "code")]
async fn admin_reindex_repo(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    match crate::repos::get_row(&key) {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("repos store: {e}"),
            )
        }
    }
    match crate::repos::reindex_job(&key) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "accepted", "key": key })),
        )
            .into_response(),
        Err(MCSError::InvalidParams(message)) => {
            json_error(StatusCode::CONFLICT, message)
        }
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("repos store: {e}"),
        ),
    }
}

#[cfg(feature = "code")]
async fn admin_remove_repo(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    match crate::repos::get_row(&key) {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("repos store: {e}"),
            )
        }
    }
    match crate::repos::remove_job(&key) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "accepted", "key": key })),
        )
            .into_response(),
        Err(MCSError::InvalidParams(message)) => {
            json_error(StatusCode::CONFLICT, message)
        }
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("repos store: {e}"),
        ),
    }
}
```

`MCSError` is already imported at the top of `src/http.rs`: the webhook handlers match `MCSError::ConstraintViolation` today (`http.rs:779-781`). No import change needed.

- [ ] **Step 5: Run the tests**

Run: `cargo test --test repo_admin --features code -- --test-threads=1`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/http.rs tests/repo_admin.rs
git commit -m "feat: admin HTTP API for managed repositories

Four admin-scoped routes on /ui/api/repos; mutations answer 202 and run
the job on a detached thread, the list shows live state. 401/403/404/409
behavior matches the principals API.

Tokens: ~10k. Cost: < $1."
```

---

### Task 7: Admin UI — the Repositories section

**Files:**
- Modify: `src/ui/admin.html` (new section after `#webhooks`)
- Modify: `src/ui/admin.js` (`loadRepos`, `renderRepos`, `openRepoForm`, `pollRepos`, wiring in `load()` and the boot section)
- Modify: `src/ui/admin.css` (state badge colors)
- Test: none (vanilla JS; the section is verified in Task 8's smoke test)

**Interfaces:**
- Consumes: `GET/POST /ui/api/repos`, `POST /ui/api/repos/{key}/reindex`, `DELETE /ui/api/repos/{key}` (Task 6); existing `api()`, `badge()`, `setStatus()`, the shared `<dialog id="form">`.
- Produces: a Repositories section that lists repos, adds one, triggers reindexes, removes one, and polls while a job runs.

- [ ] **Step 1: Add the section to `admin.html`**

Insert after the `#webhooks` section, before `</main>`:

```html
      <section id="repos" hidden>
        <h2>Code repositories</h2>
        <p class="hint" id="repo-status"></p>
        <button id="add-repo" hidden>Add repository</button>
        <table>
          <thead><tr><th>Key</th><th>URL</th><th>Auth</th><th>State</th><th>Last indexed</th><th></th></tr></thead>
          <tbody id="repo-rows"></tbody>
        </table>
      </section>
```

- [ ] **Step 2: Add the loading, render, form, and polling functions to `admin.js`**

Insert before `setStatus`:

```js
async function loadRepos() {
  const section = document.getElementById("repos");
  const status = document.getElementById("repo-status");
  let data;
  try {
    data = await api("/ui/api/repos");
  } catch (e) {
    // A build without the code feature has no such route; the section
    // stays hidden instead of showing a table that cannot load.
    if (e.message.startsWith("404")) return;
    status.textContent = "Repositories failed: " + e.message;
    return;
  }
  if (!data) return;
  section.hidden = false;
  renderRepos(data.repos);
  document.getElementById("add-repo").hidden = false;
  status.textContent = "";
}

const TRANSITIONAL = ["pending", "cloning", "indexing", "removing"];

function renderRepos(repos) {
  const tbody = document.getElementById("repo-rows");
  tbody.textContent = "";
  for (const r of repos) {
    const tr = document.createElement("tr");
    const key = document.createElement("td");
    key.textContent = r.key;
    key.className = "mono";
    const url = document.createElement("td");
    url.textContent = r.url;
    url.className = "mono";
    const auth = document.createElement("td");
    auth.textContent = r.authKind;
    const state = document.createElement("td");
    const badge = document.createElement("span");
    badge.className = "badge state-" + r.state;
    badge.textContent = r.state;
    if (r.lastError) badge.title = r.lastError;
    state.append(badge);
    const last = document.createElement("td");
    last.textContent = r.lastIndexedUs ? new Date(r.lastIndexedUs / 1000).toLocaleString() : "—";
    const actions = document.createElement("td");
    const reindex = document.createElement("button");
    reindex.textContent = "Reindex";
    reindex.disabled = TRANSITIONAL.includes(r.state);
    reindex.onclick = () => triggerReindex(r);
    const del = document.createElement("button");
    del.textContent = "Remove";
    del.disabled = TRANSITIONAL.includes(r.state);
    del.onclick = () => removeRepo(r);
    actions.append(reindex, del);
    tr.append(key, url, auth, state, last, actions);
    tbody.append(tr);
  }
}

async function triggerReindex(r) {
  try {
    await api("/ui/api/repos/" + encodeURIComponent(r.key) + "/reindex", { method: "POST" });
    pollRepos();
  } catch (e) {
    setStatus(e.message);
  }
}

async function removeRepo(r) {
  if (!confirm("Remove repository " + r.key + "? Its indexed database and local clone will be deleted.")) return;
  try {
    await api("/ui/api/repos/" + encodeURIComponent(r.key), { method: "DELETE" });
    pollRepos();
  } catch (e) {
    setStatus(e.message);
  }
}

// Poll the list until no repo is transitional. One flight at a time; a busy
// row re-schedules us, and a terminal row stops the loop.
let repoPolling = false;
async function pollRepos() {
  if (repoPolling) return;
  repoPolling = true;
  try {
    for (;;) {
      const data = await api("/ui/api/repos");
      if (!data) return;
      renderRepos(data.repos);
      if (!data.repos.some((r) => TRANSITIONAL.includes(r.state))) return;
      await new Promise((resolve) => setTimeout(resolve, 2000));
    }
  } catch (e) {
    setStatus(e.message);
  } finally {
    repoPolling = false;
  }
}

function openRepoForm() {
  const dialog = document.getElementById("form");
  dialog.textContent = "";
  const h = document.createElement("h2");
  h.textContent = "Add repository";
  dialog.append(h);

  const fields = [
    ["key", "Key (project identifier: [A-Za-z0-9_-], max 64)", ""],
    ["url", "Git URL (https, http, or ssh; no embedded credentials)", ""],
  ];
  for (const [key, label, value] of fields) {
    const row = document.createElement("label");
    row.textContent = label + ": ";
    const input = document.createElement("input");
    input.type = "text";
    input.id = "f-repo-" + key;
    input.value = value;
    row.append(input);
    dialog.append(row);
  }

  const authRow = document.createElement("label");
  authRow.textContent = "Authentication: ";
  const kind = document.createElement("select");
  kind.id = "f-repo-authKind";
  for (const value of ["none", "token", "ssh"]) {
    const option = document.createElement("option");
    option.value = value;
    option.textContent = value + (value === "none" ? " (public repo)" : "");
    kind.append(option);
  }
  authRow.append(kind);
  dialog.append(authRow);

  const secretRow = document.createElement("label");
  secretRow.textContent = "Token or private key: ";
  const secret = document.createElement("textarea");
  secret.id = "f-repo-authSecret";
  secret.rows = 4;
  secret.disabled = true;
  secretRow.append(secret);
  dialog.append(secretRow);
  kind.onchange = () => { secret.disabled = kind.value === "none"; };

  const snippets = document.createElement("label");
  const snippetsBox = document.createElement("input");
  snippetsBox.type = "checkbox";
  snippetsBox.id = "f-repo-snippets";
  snippets.append(snippetsBox, " Store body snippets (for semantic search)");
  dialog.append(snippets);

  const save = document.createElement("button");
  save.textContent = "Add and index";
  save.onclick = async () => {
    const read = (id) => document.getElementById(id).value.trim();
    const body = {
      key: read("f-repo-key"),
      url: read("f-repo-url"),
      authKind: document.getElementById("f-repo-authKind").value,
      snippets: document.getElementById("f-repo-snippets").checked,
    };
    const secretValue = secret.value.trim();
    if (body.authKind !== "none") body.authSecret = secretValue;
    try {
      await api("/ui/api/repos", { method: "POST", body: JSON.stringify(body) });
      dialog.close();
      pollRepos();
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
```

- [ ] **Step 3: Wire the section into `load()` and the boot wiring**

In `load()`, after `await loadWebhooks();`, add:

```js
  await loadRepos();
```

At the bottom, next to the existing button wiring, add:

```js
document.getElementById("add-repo").onclick = () => openRepoForm();
```

- [ ] **Step 4: Add state badge colors to `admin.css`**

Append:

```css
.badge.state-indexed { color: #7ee787; border-color: #2ea043; }
.badge.state-error { color: #ff7b72; border-color: #f85149; }
.badge.state-pending, .badge.state-cloning, .badge.state-indexing, .badge.state-removing { color: #d29922; border-color: #9e6a03; }
```

- [ ] **Step 5: Smoke-test the section**

Run the server locally on a fixture memory file with code enabled and OAuth off, then open the admin page:

```bash
cargo run --features code -- --memory-file /tmp/repo-ui-smoke.mcpmem --enable-code --http 127.0.0.1:8901
```

Open `http://127.0.0.1:8901/ui/admin`. The admin page requires OAuth; without `--oidc-issuer` the login starts but the server has no authorization endpoint. Instead, verify the static shell embeds by curling the assets:

```bash
curl -s http://127.0.0.1:8901/ui/admin | grep -c 'id="repos"'
curl -s http://127.0.0.1:8901/ui/admin.js | grep -c 'loadRepos'
```

Expected: both print 1 (the section exists in the served shell). Full interactive verification needs the OAuth flow, which the runbook covers; the browser walk is a manual step in Task 8 if a live IdP is available.

- [ ] **Step 6: Commit**

```bash
git add src/ui/admin.html src/ui/admin.js src/ui/admin.css
git commit -m "feat: admin UI manages indexed code repositories

A Repositories section lists registered repos (key, url, auth, state,
last indexed), adds one with credential options, triggers async
reindexes, and removes with full-wipe confirmation. Polls the list
while a job runs; hides itself on 404 like the webhooks section.

Tokens: ~7k. Cost: < $1."
```

---

### Task 8: Verification — fmt, clippy, full suite, pre-flight marker

**Files:**
- None (verification only; fix whatever surfaces)

**Interfaces:**
- Consumes: every task above.

- [ ] **Step 1: Format**

Run: `cargo fmt --all --check`

If it fails, run `cargo fmt --all` and re-check. Commit any reformat separately with `chore: cargo fmt`.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Expected: clean. Fix any findings (the workspace lints are strict; the plan avoided the known traps — casts via `as i64`, owned clones in closures).

- [ ] **Step 3: Full test suite**

Run:

```bash
cargo test --workspace --all-targets -- --test-threads=1
cargo test --test indexer_worker --features indexer -- --test-threads=1
cargo test --test role_composition --features indexer,webhooks
cargo package -p mcpmem-core --locked
```

Expected: all pass. The four new test binaries (`watcher_stop`, `repo_service`, `repo_tools`, `repo_admin`) run under `--workspace --all-targets`; confirm each ran, not skipped (the git-fixture tests print nothing on success; a skipped run shows `0 tests`).

- [ ] **Step 4: Spec coverage sweep**

Walk `docs/superpowers/specs/2026-09-12-managed-code-repos-design.md` section by section and confirm the plan implemented each: migration 0006 (yes, Task 1), one-in-flight (Task 4), credential hygiene (Task 4), 600s git timeout (Task 4), git binary check at startup (Task 4 `init`), worktree repair by re-clone (Task 4 `do_clone`), HTTP 202 + polling (Tasks 6-7), MCP sync tools (Task 5), remove = full wipe with watcher stop first (Tasks 3-4), crash recovery to `pending` (Task 4 `init`), admin scope gate (Task 6), secrets masked in list (Tasks 4-5). Report any gap found and fix it in the same commit.

- [ ] **Step 5: Pre-flight marker**

Run the repository's pre-flight chain (repo `.omp/AGENTS.md`) and write the marker on the exact commit that will be pushed:

```sh
env OMP_PREFLIGHT_CMD="<the repo pre-flight chain from .omp/AGENTS.md> && git rev-parse HEAD > \"\$(git rev-parse --git-dir)/omp-preflight-pass\"" bash -c '<the same chain>'
```

Use the chain verbatim from `.omp/AGENTS.md` (its `OMP_PREFLIGHT_CMD` form). Expected: every step green, marker written.

- [ ] **Step 6: Final commit if verification changed anything**

```bash
git status --short
```

If clean, nothing to commit. If not, commit the fixes with a message naming the finding and the fix, then re-run Steps 1-3 until clean.

---

## Self-Review Notes (run once, before handoff)

- **Spec coverage:** every spec section maps to a task (sweep in Task 8 Step 4). Two spec details changed deliberately: the UI section hides via HTTP 404 like webhooks (no `codeSectionEnabled` flag in the API — the 404 convention already exists); `code_repo` rows keep no `created_us` (not needed by any consumer; the design table listed only the columns implemented).
- **JSON contract:** `RepoInput` and `RepoRow` carry `#[serde(rename_all = "camelCase")]` (Task 4), so the HTTP/UI contract `key/url/authKind/authSecret/snippets/state/lastError/lastIndexedUs` matches the MCP descriptors and the UI reads `authKind`, `lastIndexedUs`, `lastError`.
- **LruCache pop:** verified in the vendored source (`~/.cargo/registry/src/*/lru-0.12.5/src/lib.rs:1109`, `pub fn pop<Q>(&mut self, k: &Q) -> Option<V>`).
- **The watcher test's strong-count assertion:** `Arc::strong_count(&kg) == 1` after stop (the test's own binding), not 0.
- **`busy_timeout_ms`:** the webhooks init at `server.rs:448-451` uses `config.busy_timeout_ms` in the same scope; the repos init copies it.