//! HTTP adapters for the OAuth authorization server. Every handler here is thin:
//! it parses the request, calls `mcpmem_oauth`, and shapes the response.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path as UrlPath, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, http::StatusCode};
use mcpmem_oauth::registration::RegistrationError;
use parking_lot::Mutex;
use serde_json::json;

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
/// The guard must never be held across an `await`. The lock is not
/// async-aware, so a task that sleeps while holding it — an upstream token
/// exchange, say — blocks every other OAuth request on that worker. That is
/// why the field is private and every reader goes through
/// [`OauthState::with_store`], which makes the mistake unwritable rather than
/// merely discouraged.
pub struct OauthState {
    pub config: crate::config::OAuthConfig,
    store: Mutex<mcpmem_oauth::store::Store>,
    pub now_us: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl OauthState {
    /// The one resource identifier this server protects: its MCP endpoint under
    /// the canonical public URL.
    ///
    /// Every place that has to name the resource reads it here — the discovery
    /// document, the suffix check, and from Task 6 on the `resource` a token is
    /// bound to. A second spelling of the same string is how a later transport
    /// path silently stops matching.
    pub fn resource(&self) -> String {
        format!("{}/mcp", self.config.public_url)
    }

    /// Run `f` with the store locked, and drop the guard before returning.
    ///
    /// The guard cannot escape and cannot span an `await`. `f` returns a `T`
    /// that is fixed by the caller, so it cannot name the guard's lifetime:
    /// neither the guard itself nor a future borrowing the `&Store` type-checks
    /// as a return value. An `await` inside `f` is impossible for the same
    /// reason — `f` is a plain closure, and an `async` block it built would
    /// borrow the store it cannot outlive.
    ///
    /// Every statement group that must be atomic therefore belongs in one
    /// call: two calls are two lock acquisitions with a window between them.
    ///
    /// Two rules the type cannot state, and both matter more as `f` grows:
    ///
    /// - `f` must not call `with_store` again, directly or through a helper
    ///   that holds an `&OauthState`. The mutex is not reentrant, so a second
    ///   acquisition on this thread self-deadlocks the worker — no timeout, no
    ///   panic, no poisoning, and nothing in a log.
    /// - `f` must not block: no `block_on`, no synchronous network call, no
    ///   sleep. Blocking under the lock is exactly what holding the guard
    ///   across an `await` would have done, and the type only rules out the
    ///   spelling with `await` in it.
    pub fn with_store<T>(&self, f: impl FnOnce(&mcpmem_oauth::store::Store) -> T) -> T {
        f(&self.store.lock())
    }

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

/// Add the discovery documents and the registration endpoint.
///
/// Each document is published twice. A client that knows only the origin
/// fetches the bare path. A client that follows RFC 9728 section 3.1 (for the
/// resource) or RFC 8414 section 3.1 (for the issuer) inserts the well-known
/// segment between the host and the path of the identifier it holds, and so
/// fetches the suffixed path. Both matter, because `--public-url` may carry a
/// path prefix: then the bare path is what a stripping proxy delivers, and the
/// suffixed path is what a client derives.
///
/// A suffix must name this server identically, or the route answers 404. Every
/// route answers 404 when OAuth is off, so a server without OAuth advertises no
/// authorization server at all.
pub fn attach(router: Router<HttpState>) -> Router<HttpState> {
    router
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/{*suffix}",
            get(protected_resource_at),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .route(
            "/.well-known/oauth-authorization-server/{*suffix}",
            get(authorization_server_at),
        )
        .route("/oauth/register", post(register))
}

/// `POST /oauth/register` — RFC 7591 dynamic client registration.
///
/// The endpoint is open, as RFC 7591 section 1.2 allows and as every MCP
/// client assumes: a client that has just discovered this server holds no
/// credential to authenticate a registration with. Nothing in the body is
/// trusted — see `mcpmem_oauth::registration::RegistrationRequest` — and the
/// cost of one request is one bounded row: `register` caps the name, the
/// number of redirect URIs and the length of each. Task 9 bounds how many
/// requests arrive; a rate limit cannot bound the size of a row, which is why
/// the caps live in `register` and not here.
///
/// The body is read as bytes rather than through the `Json` extractor. A
/// malformed body must answer with the RFC 7591 section 3.2.2 error object,
/// and the extractor's own rejection is a different shape a client cannot
/// parse.
async fn register(State(state): State<HttpState>, body: axum::body::Bytes) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return registration_refused(&RegistrationError::InvalidClientMetadata);
    };
    let now_us = (oauth.now_us)();
    match oauth.with_store(|store| mcpmem_oauth::registration::register(store, &value, now_us)) {
        Ok(document) => (StatusCode::CREATED, Json(document)).into_response(),
        Err(e) => registration_refused(&e),
    }
}

/// The RFC 7591 section 3.2.2 error object for a refused registration.
///
/// The match is exhaustive on purpose: a new reason for refusing a client must
/// name the code it reports, and `invalid_client_metadata` is not a safe
/// default for every one of them.
fn registration_refused(e: &RegistrationError) -> Response {
    let code = match e {
        RegistrationError::InvalidRedirectUri => "invalid_redirect_uri",
        RegistrationError::InvalidClientMetadata
        | RegistrationError::DomainNotAllowed
        | RegistrationError::MalformedDocument
        | RegistrationError::MetadataMismatch
        | RegistrationError::Fetch(_) => "invalid_client_metadata",
        // The request was well formed and this server failed. Its own detail
        // names the database, so it goes to the log and not to the client.
        RegistrationError::Store(inner) => {
            tracing::error!(error = %inner, "the OAuth store refused a client registration");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "server_error" })),
            )
                .into_response();
        }
    };
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": code, "error_description": e.to_string() })),
    )
        .into_response()
}

/// The scopes this server advertises: one slug per enabled tool category.
fn scopes(state: &HttpState) -> Vec<&'static str> {
    state.enabled_categories.iter().map(|c| c.slug()).collect()
}

/// The scheme and host of `public_url`, without any path prefix. Both
/// specifications insert the well-known segment between the host and the path,
/// so the path a client sends back is relative to the origin, not to
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

/// The identifier a client holding `suffix` built its request URL from, or
/// `None` when that is not `expected`.
///
/// One rule, and only one: identical, or nothing. No trailing slash is
/// trimmed, because `…/mcp/` is a different identifier from `…/mcp`, and RFC
/// 9728 section 3.3 requires the document to name the identifier the client
/// started from. Answering a document about a neighbouring identifier makes a
/// validating client abort on a 200 instead of on a 404.
fn identifier<'a>(oauth: &OauthState, suffix: &str, expected: &'a str) -> Option<&'a str> {
    let requested = format!("{}/{suffix}", origin(&oauth.config.public_url));
    (requested == expected).then_some(expected)
}

/// `GET /.well-known/oauth-protected-resource` — the document for a client that
/// starts from the origin. This server protects one resource, its `/mcp`
/// endpoint, so that is what the document names.
async fn protected_resource(State(state): State<HttpState>) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    resource_document(&state, oauth, &oauth.resource())
}

/// `GET /.well-known/oauth-protected-resource/{*suffix}` — the RFC 9728 section
/// 3.1 form. The document names the identifier the client built the URL from,
/// and a suffix naming anything but this server's one resource is 404: echoing
/// another would advertise a resource this server does not protect.
async fn protected_resource_at(
    State(state): State<HttpState>,
    UrlPath(suffix): UrlPath<String>,
) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let resource = oauth.resource();
    match identifier(oauth, &suffix, &resource) {
        Some(resource) => resource_document(&state, oauth, resource),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The protected-resource document for one resource identifier. The caller
/// establishes that `resource` is the one this server protects.
fn resource_document(state: &HttpState, oauth: &OauthState, resource: &str) -> Response {
    Json(mcpmem_oauth::metadata::protected_resource(
        resource,
        &oauth.config.public_url,
        &scopes(state),
    ))
    .into_response()
}

/// `GET /.well-known/oauth-authorization-server` — the document for a client
/// that starts from the origin.
async fn authorization_server(State(state): State<HttpState>) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    authorization_server_document(&state, oauth)
}

/// `GET /.well-known/oauth-authorization-server/{*suffix}` — the RFC 8414
/// section 3.1 form. The issuer identifier is this server's public URL, so the
/// suffix must reproduce its path exactly. Without this route the flow
/// dead-ends one hop after discovery: the protected-resource document names an
/// authorization server carrying a path, and the client derives this URL from
/// it.
async fn authorization_server_at(
    State(state): State<HttpState>,
    UrlPath(suffix): UrlPath<String>,
) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match identifier(oauth, &suffix, &oauth.config.public_url) {
        Some(_) => authorization_server_document(&state, oauth),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn authorization_server_document(state: &HttpState, oauth: &OauthState) -> Response {
    Json(mcpmem_oauth::metadata::authorization_server(
        &oauth.config.public_url,
        &scopes(state),
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
