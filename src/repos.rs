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
use std::os::unix::fs::PermissionsExt;
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
    /// `none`, `token`, or `ssh`. Defaults to "none" when omitted; the MCP
    /// descriptor documents "Default 'none'".
    #[serde(default = "none_kind")]
    pub auth_kind: String,
    pub auth_secret: Option<String>,
    /// Defaults to false; the MCP descriptor documents "Default false".
    #[serde(default)]
    pub snippets: bool,
}

/// Serde default for a missing `authKind` field; "none" matches the MCP
/// descriptor's documented default and the store's validation.
fn none_kind() -> String {
    "none".into()
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
    let repos_dir =
        std::path::PathBuf::from(format!("{}.code", db_path.display())).join("repos");
    let _ = RUNTIME.set(RepoRuntime {
        db_path,
        busy_timeout_ms,
        git_ok,
    });
    let _ = IN_FLIGHT.set(Mutex::new(HashSet::new()));
    let _ = std::fs::create_dir_all(repos_dir);
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
    Ok(std::path::PathBuf::from(format!("{}.code", runtime.db_path.display())).join("repos"))
}

fn worktree_of(key: &str) -> Result<PathBuf> {
    Ok(repos_base()?.join(key))
}

fn validate_url(url: &str) -> Result<()> {
    if url.trim().is_empty() {
        return Err(MCSError::InvalidParams("url is required".into()));
    }
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    // A bare user name (git@github.com, ssh://git@host) is not a credential;
    // only userinfo carrying a password (user:pass@) is rejected.
    if let Some(before_slash) = without_scheme.split('/').next()
        && let Some(at) = before_slash.rfind('@')
        && before_slash[..at].contains(':')
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
            i64::from(input.snippets),
            STATE_PENDING
        ],
    )
    .map_err(|e| match e {
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                || failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY =>
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
        .is_some_and(|set| set.lock().insert(key.to_owned()))
}

fn end_job(key: &str) {
    if let Some(set) = IN_FLIGHT.get() {
        set.lock().remove(key);
    }
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
            perm.set_mode(0o600);
            std::fs::set_permissions(&path, perm)
                .map_err(|e| MCSError::MemoryError(format!("ssh key chmod failed: {e}")))?;
            Ok(PreparedAuth {
                envs: vec![(
                    "GIT_SSH_COMMAND".to_owned(),
                    format!(
                        "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new",
                        path.display()
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
    if !(wt.join(".git").exists() || wt.join("HEAD").exists()) {
        return false;
    }
    // A corrupt worktree (truncated .git, stopped mid-fetch) must fail this
    // probe so do_clone removes and re-clones it.
    run_git(Some(&wt), &["status", "--porcelain"], &[]).is_ok()
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
    // The code handlers answer in the MCP content envelope; the counters JSON
    // sits in `content[0].text`.
    let text = wrapper["content"][0]["text"].as_str().unwrap_or("");
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
    begin_checked(key)?;
    let result = reindex_inner(key);
    end_job(key);
    result
}

/// Schedule the reindex job on a detached thread. The HTTP reindex path.
pub fn reindex_job(key: &str) -> Result<()> {
    begin_checked(key)?;
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
    begin_checked(key)?;
    let result = remove_inner(key);
    end_job(key);
    result
}

/// Schedule the remove job on a detached thread. The HTTP remove path.
pub fn remove_job(key: &str) -> Result<()> {
    begin_checked(key)?;
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
    let db = std::path::PathBuf::from(format!("{}.code", runtime.db_path.display()))
        .join(format!("{key}.code.db"));
    let db = db.to_string_lossy().into_owned();
    for ext in ["", "-wal", "-shm"] {
        if let Err(e) = std::fs::remove_file(format!("{db}{ext}"))
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(MCSError::MemoryError(format!(
                "index DB cleanup failed ({ext}): {e}"
            )));
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
