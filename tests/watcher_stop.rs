//! The watcher registry: spawn, stop, and the second-stop answer.

use std::sync::Arc;

use mcpmem::code_registry;
use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::kg::GraphHandle;

fn warm_project(dir: &tempfile::TempDir) -> Arc<GraphHandle> {
    let base = dir.path().join("code");
    code_registry::init(
        base,
        Durability::Async,
        SqliteTuning::default(),
        std::num::NonZeroUsize::new(8).unwrap(),
        2,
    );
    code_registry::resolve("watchme").expect("resolve opens a project")
}

#[test]
fn stop_watcher_joins_and_answers_once() {
    let dir = tempfile::tempdir().unwrap();
    let watched = dir.path().join("tree");
    std::fs::create_dir_all(&watched).unwrap();
    let kg = warm_project(&dir);

    mcpmem::watcher::spawn_watcher(kg.clone(), watched.to_string_lossy().into_owned(), "watchme", false);
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
    // The thread is joined, so it no longer pins the project handle; the
    // registry's warm slot and the local binding are the only strong
    // references left.
    assert_eq!(Arc::strong_count(&kg), 2);
}

#[test]
fn stop_watcher_on_unknown_project_is_false() {
    assert!(!mcpmem::watcher::stop_watcher("never-started"));
}