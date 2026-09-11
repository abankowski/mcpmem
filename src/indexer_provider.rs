//! The one live embedding provider, reachable from tool dispatch.
//!
//! The worker owns a [`ProviderRegistry`] because it embeds on write. A query
//! tool needs the same registry, and threading it through `MCPServer`,
//! `handle_tools_call` and both transports would touch every dispatch
//! signature for one optional value. This module holds it in a process-wide
//! cell instead, the pattern `code_vec_registry` and `CODE_ENABLED` already
//! use in this crate.
//!
//! `OnceLock` and not `LazyLock`: the registry is built from settings resolved
//! at startup — the configuration file layered under the environment — so the
//! initializer is not known at declaration time. The cell is set once, during
//! startup, before any request is served.

use std::sync::{Arc, OnceLock};

use mcpmem_indexer::ProviderRegistry;

static PROVIDER: OnceLock<Arc<ProviderRegistry>> = OnceLock::new();

/// Publishes the registry for the lifetime of the process. A second call is
/// ignored: startup sets it once, and a test that sets it cannot then race a
/// different value into place.
pub fn init(provider: Arc<ProviderRegistry>) {
    let _ = PROVIDER.set(provider);
}

/// The registry, or `None` when this process configured no provider. A tool
/// that needs one reports a configuration error rather than failing silently.
pub fn get() -> Option<Arc<ProviderRegistry>> {
    PROVIDER.get().cloned()
}

/// Whether a provider is available. Used to decide whether a tool appears in
/// `tools/list`, so a client never sees a tool that cannot work.
pub fn is_configured() -> bool {
    PROVIDER.get().is_some()
}
