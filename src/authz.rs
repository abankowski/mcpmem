//! Who is calling, and what they may call.
//!
//! Scopes are the tool-category slugs (`graph-read`, `graph-write`, `vectors`,
//! `code`). The effective capability of a request is the intersection of the
//! process-wide `--enable-*` set and the scopes on the presented credential.

use std::collections::BTreeSet;
use std::sync::LazyLock;

pub use mcpmem_core::auth::{Principal, PrincipalKind};

use crate::tools::{self, ToolCategory};

/// The shared, immutable stdio principal. The line transports dispatch one of
/// these per message, so the scope set is built once instead of per request.
pub(crate) static LOCAL_PRINCIPAL: LazyLock<Principal> =
    LazyLock::new(|| principal_with("local", ToolCategory::ALL));

/// The stdio caller. stdio is local and unauthenticated, so it holds every
/// scope; the process-wide category flags still apply.
pub fn local_principal() -> Principal {
    LOCAL_PRINCIPAL.clone()
}

/// The principal behind the static bearer token, with operator-chosen scopes.
pub fn bearer_principal(scopes: &[ToolCategory]) -> Principal {
    principal_with("static", scopes)
}

/// A principal named by the OAuth layer, with the scopes the human approved.
pub fn oauth_principal(id: &str, scopes: BTreeSet<String>) -> Principal {
    Principal {
        id: id.to_owned(),
        kind: PrincipalKind::Human,
        scopes,
        allowed_origins: BTreeSet::new(),
    }
}

fn principal_with(id: &str, scopes: &[ToolCategory]) -> Principal {
    Principal {
        id: id.to_owned(),
        kind: PrincipalKind::Machine,
        scopes: scopes.iter().map(|c| c.slug().to_owned()).collect(),
        allowed_origins: BTreeSet::new(),
    }
}

/// `true` when the principal holds the scope this tool needs. An unknown tool
/// name is never allowed.
#[inline]
pub fn allows_tool(principal: &Principal, tool: &str) -> bool {
    tools::scope_of(tool).is_some_and(|s| principal.scopes.contains(s))
}
