//! MCP handlers for managed repositories, driven directly.

use std::path::Path;
use std::process::Command;

use mcpmem::actions::code::{
    handle_code_repo_add, handle_code_repo_list, handle_code_repo_reindex, handle_code_repo_remove,
};
use mcpmem::code_registry;
use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::kg::GraphHandle;
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
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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

// The repos and code_registry stores are first-wins process singletons, so
// every test in this binary must share one backing directory. The code base
// is `{db}.code` — the same derivation repos.rs uses — so wipe and resolve
// paths stay aligned.
static FIXTURE: std::sync::LazyLock<(tempfile::TempDir, std::path::PathBuf)> =
    std::sync::LazyLock::new(|| {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.mcpmem");
        // A graph open migrates the shared memory database; migration 0006
        // creates the `code_repo` table the managed-repo store reads.
        // (Same pattern as tests/event_outbox.rs and repo_service.rs.)
        let _kg = GraphHandle::new(
            &db,
            Durability::Async,
            SqliteTuning::default(),
            std::num::NonZeroUsize::new(8).unwrap(),
            2,
        )
        .unwrap();
        code_registry::init(
            std::path::PathBuf::from(format!("{}.code", db.display())),
            Durability::Async,
            SqliteTuning::default(),
            std::num::NonZeroUsize::new(8).unwrap(),
            2,
        );
        repos::init(db.clone(), 5000);
        (dir, db)
    });

fn setup() -> &'static (tempfile::TempDir, std::path::PathBuf) {
    &FIXTURE
}

/// The text field of an MCP text-content result.
fn text_of(wrapper: &serde_json::Value) -> &str {
    wrapper["content"][0]["text"].as_str().unwrap_or("")
}

#[test]
fn mcp_add_list_reindex_remove_round_trip() {
    let (dir, _db) = setup();
    let repo = fixture(dir.path(), "up");

    let added = handle_code_repo_add(Some(&serde_json::json!({
        "key": "cli-repo",
        "url": repo.to_string_lossy(),
        "authKind": "none",
    })))
    .expect("add succeeds");
    assert!(text_of(&added).contains("indexed"), "{added}");

    let listed = handle_code_repo_list(None).expect("list succeeds");
    let text = text_of(&listed);
    assert!(text.contains("cli-repo"), "{listed}");
    assert!(
        !text.contains("authSecret"),
        "secrets never listed: {listed}"
    );

    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn alpha() -> u32 { 1 }\npub fn beta() -> u32 { 2 }\n",
    )
    .unwrap();
    git(&["add", "."], Some(&repo));
    git(&["commit", "-q", "-m", "second"], Some(&repo));
    let reindexed = handle_code_repo_reindex(Some(&serde_json::json!({ "key": "cli-repo" })))
        .expect("reindex succeeds");
    assert!(text_of(&reindexed).contains("\"symbols\":2"), "{reindexed}");

    let removed = handle_code_repo_remove(Some(&serde_json::json!({ "key": "cli-repo" })))
        .expect("remove succeeds");
    assert!(text_of(&removed).contains("removed"), "{removed}");
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

#[test]
fn mcp_add_without_auth_kind_defaults_to_none() {
    let (dir, _db) = setup();
    let repo = fixture(dir.path(), "up2");

    let added = handle_code_repo_add(Some(&serde_json::json!({
        "key": "no-auth-key",
        "url": repo.to_string_lossy(),
    })))
    .expect("add succeeds");
    assert!(text_of(&added).contains("indexed"), "{added}");

    let row = repos::get_row("no-auth-key").unwrap().expect("row exists");
    assert_eq!(row.auth_kind, "none");
}
