//! HTTP adapters for the OAuth authorization server. Every handler here is thin:
//! it parses the request, calls `mcpmem_oauth`, and shapes the response.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path as UrlPath, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, http::StatusCode};
use parking_lot::Mutex;

use crate::errors::{MCSError, Result};
use crate::http::HttpState;

/// Everything the OAuth routes share. The clock is a field, not a call to the
/// wall clock: expiry tests cannot sleep for an hour.
///
/// The store is behind a mutex because it owns a `rusqlite::Connection`, which
/// is `Send` but not `Sync`. `HttpState` is cloned per request and must be
/// `Send + Sync`, so the connection needs an owner that makes it `Sync`. One
/// serialized connection is also what SQLite wants for writes, and the OAuth
/// tables see one statement per request.
///
/// Never hold the guard across an `await`. The lock is not async-aware, so a
/// task that sleeps while holding it — an upstream token exchange, say —
/// blocks every other OAuth request on that worker. Take the lock, finish the
/// statement, drop the guard.
pub struct OauthState {
    pub config: crate::config::OAuthConfig,
    pub store: Mutex<mcpmem_oauth::store::Store>,
    pub now_us: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl OauthState {
    /// Open the OAuth store on `db_path` and build the shared state.
    ///
    /// # Precondition
    ///
    /// The schema must already be migrated: build the [`crate::kg::GraphHandle`]
    /// first, which runs `initialize_database` and so creates the four
    /// `oauth_*` tables. `busy_timeout_ms` satisfies
    /// [`mcpmem_oauth::store::Store::new`]'s own precondition — without it a
    /// concurrent refresh-token replay fails with `SQLITE_BUSY` instead of
    /// revoking the family.
    pub fn open(
        config: crate::config::OAuthConfig,
        db_path: &Path,
        busy_timeout_ms: u64,
    ) -> Result<OauthState> {
        Self::open_with_clock(
            config,
            db_path,
            busy_timeout_ms,
            Arc::new(mcpmem_core::events::now_us),
        )
    }

    /// Like [`OauthState::open`], but reading `now_us` instead of the wall
    /// clock. A test that has to observe an expiry injects a clock it can move;
    /// it cannot sleep for the lifetime of a token.
    pub fn open_with_clock(
        config: crate::config::OAuthConfig,
        db_path: &Path,
        busy_timeout_ms: u64,
        now_us: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Result<OauthState> {
        let conn = rusqlite::Connection::open(db_path).map_err(|e| open_failed(&e))?;
        conn.busy_timeout(Duration::from_millis(busy_timeout_ms))
            .map_err(|e| open_failed(&e))?;
        Ok(OauthState {
            config,
            store: Mutex::new(mcpmem_oauth::store::Store::new(conn)),
            now_us,
        })
    }
}

fn open_failed(e: &rusqlite::Error) -> MCSError {
    MCSError::MemoryError(format!("failed to open the OAuth store: {e}"))
}

/// Add the discovery documents.
///
/// The protected-resource document is published twice. A client that knows
/// only the origin fetches the bare path; a client that follows RFC 9728
/// section 3.1 inserts the well-known suffix between the host and the path of
/// the resource identifier, and so fetches the suffixed path. Every route
/// answers 404 when OAuth is off, so a server without OAuth advertises no
/// authorization server at all.
pub fn attach(router: Router<HttpState>) -> Router<HttpState> {
    router
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/{*resource_path}",
            get(protected_resource_at),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
}

/// The scopes this server advertises: one slug per enabled tool category.
fn scopes(state: &HttpState) -> Vec<&'static str> {
    state.enabled_categories.iter().map(|c| c.slug()).collect()
}

/// The scheme and host of `public_url`, without any path prefix. RFC 9728
/// section 3.1 inserts the well-known suffix between the host and the path, so
/// the path a client sends back is relative to the origin, not to
/// `public_url`. `--public-url` is validated as an `https` URL, so the fallback
/// is unreachable in a running server.
fn origin(public_url: &str) -> &str {
    const SCHEME: &str = "https://";
    let Some(rest) = public_url.strip_prefix(SCHEME) else {
        return public_url;
    };
    match rest.find('/') {
        Some(i) => &public_url[..SCHEME.len() + i],
        None => public_url,
    }
}

/// `GET /.well-known/oauth-protected-resource` — the document for a client that
/// starts from the origin. This server protects one resource, its `/mcp`
/// endpoint, so that is what the document names.
async fn protected_resource(State(state): State<HttpState>) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let resource = format!("{}/mcp", oauth.config.public_url);
    document(&state, oauth, &resource)
}

/// `GET /.well-known/oauth-protected-resource/{*resource_path}` — the RFC 9728
/// section 3.1 form. The resource identifier is the origin plus the captured
/// path, and the document returns exactly that, as section 3.3 requires. A
/// path that names no resource of ours is 404: this server has one, and echoing
/// any other would advertise a resource it does not protect.
async fn protected_resource_at(
    State(state): State<HttpState>,
    UrlPath(resource_path): UrlPath<String>,
) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let requested = format!(
        "{}/{}",
        origin(&oauth.config.public_url),
        resource_path.trim_end_matches('/')
    );
    if requested != format!("{}/mcp", oauth.config.public_url) {
        return StatusCode::NOT_FOUND.into_response();
    }
    document(&state, oauth, &requested)
}

/// The protected-resource document for one resource identifier. The caller
/// establishes that `resource` is one this server protects.
fn document(state: &HttpState, oauth: &OauthState, resource: &str) -> Response {
    Json(mcpmem_oauth::metadata::protected_resource(
        resource,
        &oauth.config.public_url,
        &scopes(state),
    ))
    .into_response()
}

async fn authorization_server(State(state): State<HttpState>) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Json(mcpmem_oauth::metadata::authorization_server(
        &oauth.config.public_url,
        &scopes(&state),
    ))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCategory;

    fn config() -> crate::config::OAuthConfig {
        crate::config::OAuthConfig {
            public_url: "https://mem.example.com".into(),
            oidc_issuer: "https://idp.invalid".into(),
            oidc_client_id: "mcpmem-test".into(),
            oidc_client_secret: None,
            principals: Vec::new(),
            cimd_allowed_domains: Vec::new(),
            trust_forwarded_proto: false,
        }
    }

    /// The state must read the clock it was given. A handler that stamps an
    /// expiry from the wall clock instead cannot be tested: no test can wait
    /// for a token to age out. The store round-trip itself is covered from the
    /// outside, in `tests/oauth_discovery.rs`.
    #[test]
    fn the_injected_clock_replaces_the_wall_clock() {
        const FIXED: i64 = 1_700_000_000_000_000;
        let dir = tempfile::tempdir().unwrap();
        let state = HttpState::for_test(crate::http::TestSetup {
            db_path: dir.path().join("t.mcpmem"),
            oauth: Some(config()),
            bearer_scopes: vec![ToolCategory::GraphRead],
            enabled_categories: ToolCategory::ALL.to_vec(),
            now_us: Some(Arc::new(|| FIXED)),
        });
        let oauth = state.oauth().expect("oauth is on");
        assert_eq!((oauth.now_us)(), FIXED);
    }

    /// Without an injected clock the state reads the wall clock, which is what
    /// a running server needs.
    #[test]
    fn the_default_clock_is_the_wall_clock() {
        let dir = tempfile::tempdir().unwrap();
        let state = HttpState::for_test(crate::http::TestSetup {
            db_path: dir.path().join("t.mcpmem"),
            oauth: Some(config()),
            bearer_scopes: ToolCategory::ALL.to_vec(),
            enabled_categories: ToolCategory::ALL.to_vec(),
            now_us: None,
        });
        let oauth = state.oauth().expect("oauth is on");
        let observed = (oauth.now_us)();
        let wall = mcpmem_core::events::now_us();
        assert!(
            (wall - observed).abs() < 10_000_000,
            "observed {observed}, wall clock {wall}"
        );
    }

    /// RFC 9728 section 3.1 puts the well-known suffix between the host and the
    /// path, so a `--public-url` carrying a path prefix must contribute only its
    /// origin when a requested path is resolved.
    #[test]
    fn the_origin_drops_a_path_prefix() {
        assert_eq!(origin("https://mem.example.com"), "https://mem.example.com");
        assert_eq!(
            origin("https://mem.example.com/server"),
            "https://mem.example.com"
        );
        assert_eq!(
            origin("https://mem.example.com/a/b"),
            "https://mem.example.com"
        );
    }
}
