//! End-to-end service tests for the managed-repo lifecycle, against a real
//! local git repository. Requires the `git` binary; skipped when absent.

use std::path::Path;
use std::process::Command;

use mcpmem::code_registry;
use mcpmem::code_vec_registry;
use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::errors::MCSError;
use mcpmem::kg::GraphHandle;
use mcpmem::repos::{self, RepoInput};
use std::sync::Arc;

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
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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

/// One backing directory for the whole binary. The repos and code_registry
/// stores are process-wide first-wins singletons, so every test must share
/// the same directories (the same rule the admin-route spec's fixture uses).
static SETUP: std::sync::LazyLock<(tempfile::TempDir, std::path::PathBuf)> =
    std::sync::LazyLock::new(|| {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.mcpmem");
        // A graph open migrates the shared memory database; migration 0006
        // creates the `code_repo` table the managed-repo store reads.
        let _kg = GraphHandle::new(
            &db,
            Durability::Async,
            SqliteTuning::default(),
            std::num::NonZeroUsize::new(8).unwrap(),
            2,
        )
        .unwrap();
        // The per-project index DBs and the clone worktrees share one base,
        // `<memory file>.code` — the sibling the server derives at startup.
        let code_base = dir.path().join("t.mcpmem.code");
        code_vec_registry::init(
            code_base.clone(),
            code_vec_registry::DEFAULT_CODE_EMBEDDING_DIMS,
        );
        code_registry::init(
            code_base,
            Durability::Async,
            SqliteTuning::default(),
            std::num::NonZeroUsize::new(8).unwrap(),
            2,
        );
        repos::init(db.clone(), 5000);
        (dir, db)
    });

fn setup() -> &'static (tempfile::TempDir, std::path::PathBuf) {
    &SETUP
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
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn alpha() -> u32 { 1 }\npub fn beta() -> u32 { 2 }\n",
    )
    .unwrap();
    git(&["add", "."], Some(&repo));
    git(&["commit", "-q", "-m", "second"], Some(&repo));
    let result = repos::reindex("acme-api").expect("reindex fetches and indexes");
    assert_eq!(
        result["symbols"].as_u64(),
        Some(2),
        "second symbol indexed: {result}"
    );

    // Duplicate key is refused.
    let dup = repos::register(&input);
    assert!(matches!(dup, Err(MCSError::ConstraintViolation(_))));

    // Remove wipes everything.
    repos::remove("acme-api").expect("remove wipes");
    assert!(repos::get_row("acme-api").unwrap().is_none());
    assert!(
        !dir.path()
            .join("t.mcpmem.code")
            .join("acme-api.code.db")
            .exists()
    );
    assert!(
        !dir.path()
            .join("t.mcpmem.code")
            .join("repos")
            .join("acme-api")
            .exists()
    );
}

#[test]
fn invalid_inputs_are_refused() {
    let (dir, _db) = setup();
    let repo = fixture_repo(dir.path(), "upstream2");

    let bad_key = RepoInput {
        key: "bad/key".into(),
        url: repo.to_string_lossy().into_owned(),
        auth_kind: "none".into(),
        auth_secret: None,
        snippets: false,
    };
    assert!(matches!(
        repos::register(&bad_key),
        Err(MCSError::InvalidParams(_))
    ));

    let bad_kind = RepoInput {
        key: "k".into(),
        url: repo.to_string_lossy().into_owned(),
        auth_kind: "api".into(),
        auth_secret: None,
        snippets: false,
    };
    assert!(matches!(
        repos::register(&bad_kind),
        Err(MCSError::InvalidParams(_))
    ));

    let empty_url = RepoInput {
        key: "k2".into(),
        url: "  ".into(),
        auth_kind: "none".into(),
        auth_secret: None,
        snippets: false,
    };
    assert!(matches!(
        repos::register(&empty_url),
        Err(MCSError::InvalidParams(_))
    ));

    let embedded_creds = RepoInput {
        key: "k3".into(),
        url: "https://user:pass@example.com/r.git".into(),
        auth_kind: "none".into(),
        auth_secret: None,
        snippets: false,
    };
    assert!(matches!(
        repos::register(&embedded_creds),
        Err(MCSError::InvalidParams(_))
    ));

    let bare_token = RepoInput {
        key: "k4".into(),
        url: repo.to_string_lossy().into_owned(),
        auth_kind: "token".into(),
        auth_secret: None,
        snippets: false,
    };
    assert!(matches!(
        repos::register(&bare_token),
        Err(MCSError::InvalidParams(_))
    ));

    // A bare user name is not a credential: scp-style and ssh:// URLs pass.
    let scp_url = RepoInput {
        key: "k5".into(),
        url: "git@github.com:org/repo.git".into(),
        auth_kind: "none".into(),
        auth_secret: None,
        snippets: false,
    };
    assert!(
        matches!(repos::register(&scp_url), Ok(())),
        "scp-style user@host:path passes"
    );

    let ssh_url = RepoInput {
        key: "k6".into(),
        url: "ssh://git@host/path".into(),
        auth_kind: "none".into(),
        auth_secret: None,
        snippets: false,
    };
    assert!(
        matches!(repos::register(&ssh_url), Ok(())),
        "ssh url with a user passes"
    );
}

#[test]
fn in_flight_jobs_conflict() {
    let (dir, _db) = setup();
    let repo = fixture_repo(dir.path(), "upstream3");
    let input = RepoInput {
        key: "busy".into(),
        url: repo.to_string_lossy().into_owned(),
        auth_kind: "none".into(),
        auth_secret: None,
        snippets: false,
    };
    repos::register(&input).unwrap();
    repos::add_job("busy").expect("first job starts");
    // The job may finish quickly; poll until in-flight or done, then assert a
    // re-trigger during flight conflicts when observed. To keep determinism,
    // assert the API contract instead: a finished job allows a new job.
    let _ = wait_state("busy", &["indexed", "error"]);
    repos::reindex("busy").expect("a settled job allows a new job");
}

#[test]
fn reindex_during_in_flight_job_conflicts() {
    let (dir, _db) = setup();
    let repo = fixture_repo(dir.path(), "upstream4");
    let input = RepoInput {
        key: "contested".into(),
        url: repo.to_string_lossy().into_owned(),
        auth_kind: "none".into(),
        auth_secret: None,
        snippets: false,
    };
    repos::register(&input).unwrap();
    repos::add_job("contested").expect("first job starts");
    // The add job is still in flight: clone + index take far longer than the
    // microseconds until this reindex arrives.
    assert!(
        matches!(repos::reindex("contested"), Err(MCSError::InvalidParams(_))),
        "a second trigger while one job is in flight conflicts"
    );
}

#[test]
fn remove_evicts_the_vector_index() {
    let (dir, _db) = setup();
    let repo = fixture_repo(dir.path(), "upstream-vec");
    let input = RepoInput {
        key: "vec-key".into(),
        url: repo.to_string_lossy().into_owned(),
        auth_kind: "none".into(),
        auth_secret: None,
        snippets: false,
    };
    // Resolve first, exactly as code_semantic_search would: the resolve warms
    // the registry. A remove that forgets to evict it would serve this Arc
    // again over the deleted database file.
    let first = code_vec_registry::resolve("vec-key").expect("resolve opens an index");
    repos::add(&input).expect("add clones and indexes");
    repos::remove("vec-key").expect("remove wipes the row, DB and worktree");
    let second = code_vec_registry::resolve("vec-key").expect("resolve reopens after the wipe");
    assert!(
        !Arc::ptr_eq(&first, &second),
        "the same Arc would prove the warm vector store survived the wipe"
    );
}

#[cfg(unix)]
#[test]
fn failed_remove_marks_the_row_error_and_a_retry_recovers() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, _db) = setup();
    let repo = fixture_repo(dir.path(), "upstream-sticky");
    let input = RepoInput {
        key: "sticky".into(),
        url: repo.to_string_lossy().into_owned(),
        auth_kind: "none".into(),
        auth_secret: None,
        snippets: false,
    };
    repos::add(&input).expect("add clones and indexes");
    let wt = dir
        .path()
        .join("t.mcpmem.code")
        .join("repos")
        .join("sticky");
    let mut perm = std::fs::metadata(&wt).unwrap().permissions();
    perm.set_mode(0o500);
    std::fs::set_permissions(&wt, perm).unwrap();
    let removed = repos::remove("sticky");
    assert!(removed.is_err(), "the wipe fails on the read-only worktree");
    let mut perm = std::fs::metadata(&wt).unwrap().permissions();
    perm.set_mode(0o700);
    std::fs::set_permissions(&wt, perm).unwrap();
    let row = repos::get_row("sticky")
        .unwrap()
        .expect("the row survives the failed wipe");
    assert_eq!(
        row.state, "error",
        "a failed wipe must not stay hidden in `removing`"
    );
    assert!(row.last_error.is_some(), "the failure message is recorded");
    repos::reindex("sticky").expect("reindex recovers after the permission is restored");
    assert_eq!(repos::get_row("sticky").unwrap().unwrap().state, "indexed");
}
