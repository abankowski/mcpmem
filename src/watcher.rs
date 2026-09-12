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