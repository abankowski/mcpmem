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

/// The scope `tool` needs but `principal` lacks, or `None` when the principal
/// may call it. This is the one place the scope decision is made. An unknown
/// tool name has no scope, so it is not a scope failure: the dispatcher still
/// answers `Method not found`.
#[inline]
pub fn missing_scope(principal: &Principal, tool: &str) -> Option<&'static str> {
    tools::scope_of(tool).filter(|s| !principal.scopes.contains(*s))
}

/// `true` when the principal holds the scope this tool needs. Derived from
/// [`missing_scope`], so the two can never disagree. The `tools/list` filters
/// use this form, where an unknown name must not be advertised either.
#[inline]
pub fn allows_tool(principal: &Principal, tool: &str) -> bool {
    tools::scope_of(tool).is_some() && missing_scope(principal, tool).is_none()
}
