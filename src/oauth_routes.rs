//! HTTP adapters for the OAuth authorization server. Every handler here is thin:
//! it parses the request, calls `mcpmem_oauth`, and shapes the response.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
#[cfg(feature = "oauth")]
use axum::extract::Query;
use axum::extract::{Path as UrlPath, State};
#[cfg(feature = "oauth")]
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, http::StatusCode};
use mcpmem_oauth::registration::RegistrationError;
#[cfg(feature = "oauth")]
use mcpmem_oauth::store::LoginRecord;
#[cfg(feature = "oauth")]
use mcpmem_oauth::token::TokenError;
#[cfg(feature = "oauth")]
use mcpmem_oauth::upstream::{IdentityClaims, Provider, UpstreamError};
use parking_lot::Mutex;
#[cfg(feature = "oauth")]
use serde::Deserialize;
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
    /// The upstream provider, discovered on the first authorization request
    /// and kept for the lifetime of the process.
    ///
    /// Discovery is one HTTP round trip to a host this server does not
    /// control, so it does not belong on the startup path — a provider that is
    /// slow or briefly down would stop the server from serving the graph. It
    /// also does not belong on every request. A `OnceCell` is both: the first
    /// login pays for it, a concurrent second login waits for the same
    /// initialization rather than starting another, and a failed discovery
    /// leaves the cell empty so the next login retries.
    #[cfg(feature = "oauth")]
    provider: tokio::sync::OnceCell<Provider>,
    /// How a client identifier metadata document is read.
    ///
    /// A trait object rather than a concrete fetcher, for the reason the trait
    /// itself states: the transport is chosen once, by the process that owns
    /// it. A build with the `oauth` feature holds
    /// [`mcpmem_oauth::upstream::MetadataFetch`]; a graph-only build holds a
    /// fetcher that refuses, and never reaches it, because that build serves
    /// no authorization endpoint at all. A test substitutes a document
    /// without a network.
    metadata_fetch: Arc<dyn mcpmem_oauth::registration::Fetch>,
    /// The per-peer request limits on the endpoints that answer an anonymous
    /// caller. In memory and per process: see `mcpmem_oauth::limits`.
    pub limits: mcpmem_oauth::limits::Limits,
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

    /// The URL of the protected-resource metadata document, which is what
    /// both `WWW-Authenticate` challenges point a client at.
    ///
    /// One spelling, for the reason [`OauthState::resource`] gives. A
    /// connector parses this value out of the 401 and out of the 403, and a
    /// change to the well-known path must not have to find two hand-built
    /// copies of it in the transport.
    pub fn resource_metadata(&self) -> String {
        format!(
            "{}/.well-known/oauth-protected-resource",
            self.config.public_url
        )
    }

    /// The grant behind a bearer token presented to this server, or `None`
    /// when the token is not one this server will honour.
    ///
    /// The audience it compares against is [`OauthState::resource`] and
    /// nothing else. A caller cannot pass one, so no transport path can come
    /// to compare against a value that has drifted from the one the discovery
    /// document publishes — which is the whole point of there being one
    /// spelling.
    pub fn validate(&self, token: &str) -> Option<mcpmem_oauth::store::Grant> {
        let now_us = (self.now_us)();
        let resource = self.resource();
        self.with_store(|store| mcpmem_oauth::token::validate(store, token, &resource, now_us))
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
    ///
    /// # The one thing under this lock that does wait
    ///
    /// Every `f` here runs SQLite statements, and a statement is not
    /// instantaneous. A WAL point lookup is microseconds and a reader never
    /// waits on a writer, so the cost is a throughput ceiling on this one
    /// mutex rather than a stalled reactor — but it is a wait, and the rule
    /// above would read as a denial of it.
    ///
    /// So it is named instead. `POST /mcp` — every tool call this server
    /// answers — resolves its caller inside the blocking task it already has
    /// (`crate::http::post_handler`), so the hottest path takes this lock off
    /// the reactor. The SSE `GET /mcp`, which is one request per session, and
    /// the four `/ui` data endpoints, whose gate runs before the payload
    /// builder they spawn, still validate inline. Moving those would put the
    /// authorization decision of four handlers into blocking tasks for a
    /// microsecond lookup, which buys less than the gate ordering it would
    /// disturb.
    pub fn with_store<T>(&self, f: impl FnOnce(&mcpmem_oauth::store::Store) -> T) -> T {
        f(&self.store.lock())
    }

    /// Revoke every live token family that names `principal`, and return how
    /// many families were revoked.
    pub fn revoke_principal(&self, principal: &str) -> std::result::Result<usize, String> {
        self.with_store(|store| store.revoke_principal(principal).map_err(|e| e.to_string()))
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
        let now = now_us();
        let store = mcpmem_oauth::store::Store::new(conn);
        // The admin UI is a public PKCE client of this server's own AS. Seed
        // it once; put_client upserts, so a repeat start only refreshes the
        // row, and the same clock reading stamps `created_us` and `last_used_us`.
        store
            .put_client(&mcpmem_oauth::store::ClientRecord {
                client_id: mcpmem_oauth::ADMIN_CLIENT_ID.to_owned(),
                client_name: "mcpmem admin UI".to_owned(),
                redirect_uris: vec![format!("{}/ui/admin/callback", config.public_url)],
                source: mcpmem_oauth::store::ClientRecord::RESERVED.to_owned(),
                created_us: now,
                last_used_us: now,
            })
            .map_err(|e| {
                crate::errors::MCSError::MemoryError(format!(
                    "failed to seed the admin UI client: {e}"
                ))
            })?;
        Ok(OauthState {
            config,
            store: Mutex::new(store),
            now_us,
            limits: mcpmem_oauth::limits::Limits::new(),
            #[cfg(feature = "oauth")]
            provider: tokio::sync::OnceCell::new(),
            metadata_fetch: default_metadata_fetch(),
        })
    }

    /// Read client identifier metadata documents with `fetch` instead of the
    /// shipped one.
    ///
    /// For a test. The real fetcher speaks `https` to a host on the operator's
    /// allow-list, which no loopback fixture can be, so a test that drives the
    /// authorization endpoint with a metadata-document identifier supplies the
    /// document here.
    #[doc(hidden)]
    #[must_use]
    pub fn with_metadata_fetch(
        mut self,
        fetch: Arc<dyn mcpmem_oauth::registration::Fetch>,
    ) -> OauthState {
        self.metadata_fetch = fetch;
        self
    }

    /// One pass of periodic maintenance over the OAuth tables, at this
    /// server's own clock reading.
    ///
    /// This is the whole of what the five-minute tick in
    /// [`crate::server`] runs, and it lives here rather than there so that it
    /// is reachable without a bound socket: `tests/oauth_flow.rs` drives it
    /// against a moved clock, which a test of the tick itself could only do
    /// by waiting five minutes.
    ///
    /// Eviction runs **before** the sweep, and the order is the contract
    /// [`mcpmem_oauth::store::Store::evict_clients`] states: a client is
    /// judged against the tokens it held when the pass started, not against
    /// the ones this pass has just deleted.
    ///
    /// One lock acquisition for both, so no request can interleave between
    /// them and see a client evicted while its expired tokens are still
    /// there.
    pub fn maintain(&self) -> Maintenance {
        let now_us = (self.now_us)();
        self.with_store(|store| {
            let evicted = match store.evict_clients(now_us, CLIENT_MAX_IDLE_US) {
                Ok(evicted) => evicted,
                Err(e) => {
                    tracing::warn!("the OAuth client eviction failed: {e}");
                    0
                }
            };
            let swept = match store.sweep(now_us) {
                Ok(swept) => swept,
                Err(e) => {
                    tracing::warn!("the OAuth sweep failed: {e}");
                    0
                }
            };
            Maintenance { swept, evicted }
        })
    }

    /// The upstream provider, discovering it if this is the first caller.
    #[cfg(feature = "oauth")]
    async fn provider(&self) -> std::result::Result<&Provider, UpstreamError> {
        self.provider
            .get_or_try_init(|| Provider::discover(&self.config.oidc_issuer))
            .await
    }
}

fn open_failed(e: &rusqlite::Error) -> MCSError {
    MCSError::MemoryError(format!("failed to open the OAuth store: {e}"))
}

/// The fetcher a server reads client identifier metadata documents with.
///
/// With the `oauth` feature this is the shipped one. Without it there is no
/// authorization endpoint to resolve a document for — `attach` compiles the
/// route out — so the only honest fetcher is one that refuses, and it is never
/// reached.
#[cfg(feature = "oauth")]
fn default_metadata_fetch() -> Arc<dyn mcpmem_oauth::registration::Fetch> {
    Arc::new(mcpmem_oauth::upstream::MetadataFetch::new())
}

#[cfg(not(feature = "oauth"))]
fn default_metadata_fetch() -> Arc<dyn mcpmem_oauth::registration::Fetch> {
    /// This build carries no HTTP client, by construction: see
    /// `.github/workflows/ci.yml`.
    struct NoFetch;
    impl mcpmem_oauth::registration::Fetch for NoFetch {
        fn get(&self, _url: &str) -> std::result::Result<String, String> {
            Err("this build carries no HTTP client".to_owned())
        }
    }
    Arc::new(NoFetch)
}

/// How long a client registration survives without being used, in
/// microseconds. Thirty days, which is the life of a refresh token: a client
/// that has not presented itself for longer than the longest credential it
/// could be holding is holding nothing.
const CLIENT_MAX_IDLE_US: i64 = 30 * 24 * 60 * 60 * 1_000_000;

/// What one [`OauthState::maintain`] pass removed. Two named counts, because
/// both are `u64` and a tuple would let a log line report one as the other.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Maintenance {
    /// Expired logins, authorization codes and tokens.
    pub swept: u64,
    /// Client registrations idle past [`CLIENT_MAX_IDLE_US`] holding no token.
    pub evicted: u64,
}

impl Maintenance {
    /// Whether this pass removed anything, which is what decides if it is
    /// worth a log line.
    pub const fn is_empty(&self) -> bool {
        self.swept == 0 && self.evicted == 0
    }
}

/// The address a request is counted against by `mcpmem_oauth::limits`.
///
/// It is an extractor rather than a helper each handler calls, because the
/// rule has two sources and picking the wrong one is a security bug in both
/// directions. Reading `X-Forwarded-For` when nothing sets it lets any caller
/// choose its own bucket, and so bypass every limit with one header. Reading
/// the connection when a proxy terminated it counts every caller as the proxy,
/// and so locks the whole internet out of the endpoint the moment one client
/// misbehaves.
///
/// So the source is the deployment's, not the handler's:
/// `--oauth-trust-forwarded-proto` says a trusted proxy sits in front. With it
/// set, the leftmost `X-Forwarded-For` entry is the client the proxy saw.
/// Without it, the peer of the connection.
///
/// The flag is named for a header this server never reads. `X-Forwarded-Proto`
/// appears nowhere in this workspace, and nothing needs it: the canonical URL
/// is `--public-url` and is never derived from the request, so there is no
/// question about the scheme for a header to answer. What the flag declares is
/// the deployment — a proxy terminates TLS and forwards — and this extractor
/// is the one place that declaration changes what a request means.
///
/// **The key is always a parsed address, never the text that named it.** Both
/// halves of that matter:
///
/// - The port is dropped. A peer is an address, and counting `ip:port` would
///   give every request its own bucket and bound nothing at all.
/// - A forwarded value that does not parse as an IP address is **not** used.
///   It is caller-chosen text of caller-chosen length, so using it verbatim
///   would let one caller bypass the limit with a fresh random string per
///   request *and* grow the limiter's map by the size of its own header —
///   which is exactly the bound `mcpmem_oauth::limits::MAX_KEYS` claims to
///   state. Parsing also collapses the spellings of one address: RFC 5952
///   lets `2001:db8::1` be written a dozen ways, and each way would otherwise
///   be a fresh allowance.
///
/// The fallback is a chain, not a single value. A forwarded header that is
/// absent or unusable leaves the connection address, which is the proxy's and
/// therefore one shared bucket for every caller behind it; only a request with
/// no connection address either reaches [`UNKNOWN_PEER`]. Both ends of that
/// chain are one bucket, which is the point — a caller can influence neither,
/// and a trusted proxy that stops setting the header degrades to counting
/// everybody together rather than to counting nobody.
///
/// `crate::http::run` supplies the connection address on both the TLS and the
/// plaintext path, so [`UNKNOWN_PEER`] is reached in practice only by a test
/// driving the router directly.
struct Peer(String);

/// The bucket a request whose address cannot be established is counted
/// against. Not an address, so no real peer shares it.
const UNKNOWN_PEER: &str = "unknown";

impl axum::extract::FromRequestParts<HttpState> for Peer {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &HttpState,
    ) -> std::result::Result<Peer, Self::Rejection> {
        let forwarded = state
            .oauth
            .as_ref()
            .filter(|oauth| oauth.config.trust_forwarded_proto)
            .and_then(|_| forwarded_for(&parts.headers));
        if let Some(client) = forwarded {
            return Ok(Peer(client));
        }
        // The connection address is canonicalised for the same reason the
        // forwarded one is: a dual-stack listener on `[::]` reports an IPv4
        // client as `::ffff:a.b.c.d`, and that is the same host as
        // `a.b.c.d` arriving on an IPv4 listener. One host, one key.
        Ok(Peer(
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map_or_else(
                    || UNKNOWN_PEER.to_owned(),
                    |info| info.0.ip().to_canonical().to_string(),
                ),
        ))
    }
}

/// The leftmost `X-Forwarded-For` entry as a canonical address, which is the
/// client the outermost trusted proxy saw. Later entries are that proxy's own
/// upstreams and the header may repeat, so the first value of the first header
/// wins.
///
/// `None` when the header is absent, when its first entry is empty, or when
/// that entry is not an IP address. The value is caller-chosen and a trusted
/// proxy that does not overwrite it passes the caller's own text through, so
/// nothing here may be used as a key on the strength of being present.
///
/// **One host must yield exactly one key**, and three spellings of one host
/// reach this function from real proxies:
///
/// - A **bracketed** IPv6 entry, `[2001:db8::1]`, which is how a proxy writes
///   a 16-byte address in a header whose separator is a comma. The brackets
///   are stripped before the parse. Leaving them in is the worst of the three:
///   the parse fails, so the request falls back to the connection address —
///   the proxy's — and every client behind that proxy shares one bucket.
/// - The **v4-mapped** form, `::ffff:203.0.113.7`, which names the same host
///   as `203.0.113.7`. [`std::net::IpAddr::to_canonical`] folds it; `to_string`
///   does not.
/// - Any of the ways RFC 5952 lets a zero run be written. Parsing and
///   re-rendering folds those by itself.
///
/// A port is **not** stripped, and that is deliberate: `X-Forwarded-For`
/// carries addresses, not endpoints (`Forwarded` is the header that carries
/// ports, RFC 7239), so `203.0.113.7:9000` here is a misconfigured proxy
/// rather than a peer. It fails the parse and falls back, which is the safe
/// answer — accepting it would key on a port and bound nothing.
fn forwarded_for(headers: &axum::http::HeaderMap) -> Option<String> {
    let value = headers.get("x-forwarded-for")?.to_str().ok()?;
    let client = value.split(',').next()?.trim();
    let client = client
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(client);
    Some(
        client
            .parse::<std::net::IpAddr>()
            .ok()?
            .to_canonical()
            .to_string(),
    )
}

/// The RFC 6585 section 4 answer to a caller over its limit.
///
/// `Retry-After` is the whole window rather than the time left in it: see
/// `mcpmem_oauth::limits::RateLimiter::retry_after_seconds`. The body is an
/// OAuth error object, because every endpoint this guards answers JSON and a
/// connector that parses the refusals must not meet a different shape here.
fn too_many_requests(limiter: &mcpmem_oauth::limits::RateLimiter) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(
            axum::http::header::RETRY_AFTER,
            limiter.retry_after_seconds().to_string(),
        )],
        Json(json!({
            "error": "temporarily_unavailable",
            "error_description": "too many requests from this address",
        })),
    )
        .into_response()
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
///
/// The three login routes, the token endpoint and the revocation endpoint are
/// absent from a build without the `oauth` feature, and that is their intended
/// meaning: such a build carries no HTTP client, so it can reach no OpenID
/// Connect provider, can serve no login, has nothing to ask a human to consent
/// to, and so can never hold an authorization code to exchange.
pub fn attach(router: Router<HttpState>) -> Router<HttpState> {
    let router = router
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
        .route("/oauth/register", post(register));
    #[cfg(feature = "oauth")]
    let router = router
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/callback", get(callback))
        .route("/oauth/consent", post(consent))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke_token));
    router
}

/// `POST /oauth/register` — RFC 7591 dynamic client registration.
///
/// The endpoint is open, as RFC 7591 section 1.2 allows and as every MCP
/// client assumes: a client that has just discovered this server holds no
/// credential to authenticate a registration with. Nothing in the body is
/// trusted — see `mcpmem_oauth::registration::RegistrationRequest` — and the
/// cost of one request is one bounded row: `register` caps the name, the
/// number of redirect URIs and the length of each. `mcpmem_oauth::limits`
/// bounds how many requests arrive; a rate limit cannot bound the size of a
/// row, which is why the caps live in `register` and not here.
///
/// The body is read as bytes rather than through the `Json` extractor. A
/// malformed body must answer with the RFC 7591 section 3.2.2 error object,
/// and the extractor's own rejection is a different shape a client cannot
/// parse.
async fn register(
    State(state): State<HttpState>,
    Peer(peer): Peer,
    body: axum::body::Bytes,
) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let now_us = (oauth.now_us)();
    // Before the body is parsed: parsing is the work a limited caller must not
    // be able to make this server do.
    if !oauth.limits.register.check(&peer, now_us) {
        tracing::warn!(%peer, "registration rate limit reached");
        return too_many_requests(&oauth.limits.register);
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return registration_refused(&RegistrationError::InvalidClientMetadata);
    };
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

/// The scopes this server advertises: one slug per enabled tool category,
/// plus the admin scope whenever an OAuth state exists.
fn scopes(state: &HttpState) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = state
        .enabled_categories
        .iter()
        .map(|c| c.slug())
        .collect();
    if state.oauth.is_some() {
        out.push(crate::principals::ADMIN_SCOPE);
    }
    out
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

// ── The upstream OpenID Connect leg ─────────────────────────────────────────

/// How long a login may stay in flight, in microseconds. It bounds the time a
/// human has between arriving at the provider and coming back, and it is the
/// window in which a stolen `state` is worth anything.
#[cfg(feature = "oauth")]
const LOGIN_TTL_US: i64 = 10 * 60 * 1_000_000;

/// The most scopes one authorization request may name.
///
/// This server advertises one slug per tool category, and a request asking for
/// every one of them is far below this. The cap is not about what is
/// meaningful — Task 7 intersects the request with the principal's grant — but
/// about the size of the row an unauthenticated request writes, which is the
/// same rule `register` states for its own caps.
#[cfg(feature = "oauth")]
const MAX_REQUESTED_SCOPES: usize = 16;

/// The scopes an authorization request names, split on whitespace as RFC 6749
/// section 3.3 defines. The caller splits once and keeps the result: the list
/// counted against [`MAX_REQUESTED_SCOPES`] is the list stored on the login
/// row, so no second split can disagree with the first.
#[cfg(feature = "oauth")]
fn requested_scopes(scope: &Option<String>) -> Vec<String> {
    scope
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// What a client may send to `GET /oauth/authorize`.
///
/// Every member is optional here and checked below, so that a missing one is
/// this server's plain refusal rather than the extractor's rejection, which a
/// human would read as a crash.
#[cfg(feature = "oauth")]
#[derive(Deserialize)]
struct AuthorizeParams {
    /// RFC 6749 section 3.1.1 makes it required, and this server supports one
    /// value. `metadata::authorization_server` advertises the same one.
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    /// The client's own state, opaque here and returned unchanged. RFC 6749
    /// section 4.1.1 makes it optional.
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    scope: Option<String>,
    /// RFC 8707. Optional, and when present it must name this server's one
    /// resource.
    resource: Option<String>,
}

#[cfg(feature = "oauth")]
#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
}

/// `GET /oauth/authorize` — start a login at the upstream provider.
///
/// **Every refusal here is a plain 400, never a redirect.** That is the whole
/// reason the checks are in this order. A redirect built from an unvalidated
/// `redirect_uri` is an open redirect, and an open redirect on the
/// authorization endpoint is how an authorization code is delivered to
/// somebody else. RFC 6749 section 4.1.2.1 says the same thing: when the
/// redirect URI is missing, unknown or mismatched, the error is shown to the
/// human and not sent anywhere.
///
/// The comparison against the registered list is byte-for-byte. No prefix and
/// no wildcard: `https://client.example/cb` does not admit
/// `https://client.example/cb/anything`, and a registered URI carries no
/// fragment (`mcpmem_oauth::registration`), so nothing here can be smuggled
/// past the equality.
#[cfg(feature = "oauth")]
async fn authorize(
    State(state): State<HttpState>,
    Peer(peer): Peer,
    Query(q): Query<AuthorizeParams>,
) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !oauth.limits.authorize.check(&peer, (oauth.now_us)()) {
        tracing::warn!(%peer, "authorization rate limit reached");
        return too_many_requests(&oauth.limits.authorize);
    }
    start_login(oauth, q).await
}

/// Validate the authorization request, record the login, and answer.
///
/// The answer is built here rather than handed back as a `Result` for the
/// caller to shape: a refusal and a redirect are both one `Response`, and a
/// `Result<String, Response>` would put a 128-byte error on every return of a
/// function whose happy path is the rare one.
#[cfg(feature = "oauth")]
async fn start_login(oauth: &Arc<OauthState>, q: AuthorizeParams) -> Response {
    let (Some(client_id), Some(redirect_uri), Some(code_challenge)) =
        (q.client_id, q.redirect_uri, q.code_challenge)
    else {
        return refused("client_id, redirect_uri and code_challenge are all required");
    };
    if q.response_type.as_deref() != Some("code") {
        return refused("response_type must be code");
    }
    if q.code_challenge_method.as_deref() != Some("S256") {
        return refused("code_challenge_method must be S256");
    }
    if code_challenge.is_empty() {
        return refused("code_challenge must not be empty");
    }
    // The row this request writes is bounded here, for the reason the
    // registration endpoint states two screens up: a rate limit bounds how
    // many rows arrive, never how large one is, and this endpoint is reachable
    // by anyone who has registered a client once. Which scopes a token ends up
    // carrying is Task 7's intersection with the principal's grant; how many a
    // stranger may store is this endpoint's problem.
    //
    // Split once and kept: the list that is counted is the list that is
    // stored, and no second split can disagree with the first.
    let requested = requested_scopes(&q.scope);
    // A request naming no scope is refused here, not after the human has
    // signed in. Consent offers the intersection of this list with what the
    // human holds, so an empty list can never become a grant: the callback
    // would spend a real sign-in and then answer with the one refusal page it
    // gives every failed login, while the client waited for a redirect that
    // never comes. RFC 6749 section 3.3 makes `scope` optional in the
    // protocol; this server protects one resource whose every operation needs
    // a scope, so a request for none of them is a defect in the request. It
    // discloses nothing about any human, which is why it can be a plain 400
    // here while the empty *intersection* at the callback cannot.
    if requested.is_empty() {
        return refused("scope must name at least one scope");
    }
    if requested.len() > MAX_REQUESTED_SCOPES {
        return refused("too many scopes");
    }
    let resource = oauth.resource();
    if q.resource.is_some_and(|asked| asked != resource) {
        return refused("resource does not name this server");
    }

    let client = match client_of(oauth, &client_id, (oauth.now_us)()).await {
        Ok(client) => client,
        Err(refusal) => return refusal.response(),
    };
    // Byte for byte, including the port of a loopback URI. That declines half
    // of RFC 8252 section 7.3, deliberately;
    // `mcpmem_oauth::registration::is_acceptable_redirect_uri` records why and
    // what it costs.
    if !client.redirect_uris.contains(&redirect_uri) {
        return refused("redirect_uri is not registered for this client");
    }

    // Discovery happens before the row is written, so a provider that cannot
    // be reached leaves nothing behind to sweep up.
    let provider = match oauth.provider().await {
        Ok(provider) => provider,
        Err(e) => {
            tracing::error!(error = %e, issuer = %oauth.config.oidc_issuer,
                            "the upstream provider could not be discovered");
            return (
                StatusCode::BAD_GATEWAY,
                "the upstream provider could not be reached",
            )
                .into_response();
        }
    };

    let now_us = (oauth.now_us)();
    let login = LoginRecord {
        state: mcpmem_oauth::new_token(),
        client_id,
        redirect_uri,
        client_state: q.state,
        code_challenge,
        resource,
        scopes: requested,
        upstream_verifier: mcpmem_oauth::new_token(),
        nonce: mcpmem_oauth::new_token(),
        csrf: mcpmem_oauth::new_token(),
        principal: None,
        created_us: now_us,
        expires_us: now_us + LOGIN_TTL_US,
    };
    let url = provider.authorize_url(
        &oauth.config.oidc_client_id,
        &callback_uri(oauth),
        &login.state,
        &login.nonce,
        &mcpmem_oauth::s256_challenge(&login.upstream_verifier),
    );
    // The login row and the client's last use, under one lock: the client just
    // presented itself and was accepted, which is what `last_used_us` records
    // and what `Store::evict_clients` measures against. Without this write the
    // column never moves past registration, and a connector in daily service
    // is evicted thirty days after it registered — it re-registers
    // automatically, but the human consents again for no reason.
    //
    // A failed touch is not a failed login. The row it updates is a
    // housekeeping timestamp; refusing a human who has done nothing wrong
    // because a `UPDATE` failed would trade a real outcome for a bookkeeping
    // one.
    if let Err(e) = oauth.with_store(|store| {
        if let Err(e) = store.touch_client(&login.client_id, now_us) {
            tracing::warn!(error = %e, "the OAuth store refused to record a client use");
        }
        store.put_login(&login)
    }) {
        tracing::error!(error = %e, "the OAuth store refused to record a login");
        return server_error();
    }
    (StatusCode::FOUND, [(header::LOCATION, url)]).into_response()
}

/// The client behind `client_id`, resolving a client identifier metadata
/// document when the store holds no row for it.
///
/// Two ways a client becomes known, and the store is asked first. A client
/// that registered under RFC 7591 has a row; a client that publishes its
/// metadata at an https URL and presents that URL as its identifier has none
/// on its first authorization request, and one on every later one — the
/// resolved record is stored, so a document is read once per client and not
/// once per login.
///
/// An identifier that is not an https URL cannot be a document, so it is
/// refused here without a request. Everything else about the document —
/// the host against `--cimd-allowed-domain`, the length of the URL, the
/// members, the redirect URIs — is
/// [`mcpmem_oauth::registration::resolve_metadata_document`]'s, and it checks
/// the host before anything leaves this process.
///
/// The resolution runs on a blocking thread. `Fetch::get` is synchronous and
/// reaches a host this server does not control, and the reactor may not wait
/// on one. The wait is bounded by the fetcher's own 5 second timeout, and how
/// many of them one peer may start is bounded by the authorization endpoint's
/// rate limit.
///
/// A refused document is one plain 400. It names the document rather than the
/// identifier, because the caller chose the URL and learns nothing from being
/// told what it already sent; the reason goes to the log.
#[cfg(feature = "oauth")]
async fn client_of(
    oauth: &Arc<OauthState>,
    client_id: &str,
    now_us: i64,
) -> std::result::Result<mcpmem_oauth::store::ClientRecord, ClientRefusal> {
    match oauth.with_store(|store| store.get_client(client_id)) {
        Ok(Some(client)) => return Ok(client),
        Ok(None) => {}
        Err(e) => {
            tracing::error!(error = %e, "the OAuth store refused to read a client");
            return Err(ClientRefusal::Store);
        }
    }
    if !client_id.starts_with("https://") {
        return Err(ClientRefusal::Request("unknown client_id"));
    }

    let fetch = Arc::clone(&oauth.metadata_fetch);
    let allowed = oauth.config.cimd_allowed_domains.clone();
    let url = client_id.to_owned();
    let resolved = tokio::task::spawn_blocking(move || {
        mcpmem_oauth::registration::resolve_metadata_document(
            &url,
            &allowed,
            fetch.as_ref(),
            now_us,
        )
    })
    .await;
    let record = match resolved {
        Ok(Ok(record)) => record,
        Ok(Err(e)) => {
            tracing::warn!(client_id = %client_id, error = %e,
                           "a client identifier metadata document was refused");
            return Err(ClientRefusal::Request(
                "this client identifier metadata document could not be used",
            ));
        }
        Err(e) => {
            tracing::error!(error = %e, "the metadata document resolution panicked");
            return Err(ClientRefusal::Store);
        }
    };
    if let Err(e) = oauth.with_store(|store| store.put_client(&record)) {
        tracing::error!(error = %e, "the OAuth store refused to record a client");
        return Err(ClientRefusal::Store);
    }
    Ok(record)
}

/// Why [`client_of`] could not name the client of an authorization request.
///
/// The reason rather than the built response, because an `Err` carrying a whole
/// `Response` is 128 bytes on every return of a function whose happy path is
/// the common one — the same argument [`start_login`] makes for shaping its own
/// answer.
#[cfg(feature = "oauth")]
enum ClientRefusal {
    /// A plain 400 naming this reason. The caller chose the identifier, so
    /// nothing here discloses anything it did not send.
    Request(&'static str),
    /// The store failed. One 500, and the detail is in the log.
    Store,
}

#[cfg(feature = "oauth")]
impl ClientRefusal {
    fn response(self) -> Response {
        match self {
            ClientRefusal::Request(reason) => refused(reason),
            ClientRefusal::Store => server_error(),
        }
    }
}

/// `GET /oauth/callback` — the upstream provider sends the human back here.
///
/// **Every outcome that is not a successful login of an allowed human is the
/// same 403 page.** The caller is anonymous: whoever holds the URL can send
/// this request, and a status or a body that differs by reason answers the
/// question *is this person allowed here* for anybody who asks. So an unknown
/// state, a refused exchange, a forged token and a human who is simply not on
/// the list are indistinguishable from the outside, and the reason goes to the
/// log instead.
///
/// It never redirects. The client's `redirect_uri` is reached only after
/// consent, in Task 7, and a redirect from here would carry the outcome of an
/// identity check to a party that has not been granted anything yet.
///
/// It is bounded per peer like the other five anonymous endpoints, and it is
/// the one that most needs it: every request reads the store, and a request
/// naming a live login makes **this** server call the upstream provider — so
/// an unbounded loop here is a loop against somebody else's infrastructure
/// with this server's name on it. The check comes before the parameters are
/// judged, because a refusal the caller can trigger for free is exactly what
/// a loop sends.
#[cfg(feature = "oauth")]
async fn callback(
    State(state): State<HttpState>,
    Peer(peer): Peer,
    Query(q): Query<CallbackParams>,
) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !oauth.limits.callback.check(&peer, (oauth.now_us)()) {
        tracing::warn!(%peer, "callback rate limit reached");
        return too_many_requests(&oauth.limits.callback);
    }
    let (Some(code), Some(login_state)) = (q.code, q.state) else {
        // Not an identity outcome: no login was even named, so this discloses
        // nothing that the request itself did not already contain.
        return refused("the callback needs a code and a state");
    };
    match finish_login(oauth, &code, &login_state).await {
        Ok(page) => page,
        Err(reason) => {
            tracing::warn!(reason = %reason, "an upstream login was refused");
            login_refused()
        }
    }
}

/// Take the login, exchange the code, and name the human. The error is the
/// reason, for the log alone: the page a refused caller sees is the same
/// whatever it says.
#[cfg(feature = "oauth")]
async fn finish_login(
    oauth: &Arc<OauthState>,
    code: &str,
    login_state: &str,
) -> std::result::Result<Response, String> {
    let now_us = (oauth.now_us)();
    // Taken, not read: one login row can drive at most one exchange, so no
    // concurrent second callback can reach the provider with the same state.
    //
    // It is put back on the two paths that are not an outcome about the human.
    // A login that already names its human is waiting for consent, and a
    // login whose exchange never succeeded is still in flight — the `state`
    // travels through the provider, so whoever sees it there could otherwise
    // end a human's login with one bogus code. Every later failure keeps the
    // row consumed: by then the upstream code is spent, and what is left is an
    // answer about the human that a second callback must not ask again.
    let login = oauth
        .with_store(|store| store.take_login(login_state, now_us))
        .map_err(|e| format!("the store refused to read the login: {e}"))?
        .ok_or_else(|| "no login in flight carries this state".to_owned())?;
    if login.principal.is_some() {
        // The row written back below is live again under the same state, so
        // the take above is not by itself the replay guard the shape needs. A
        // login that already names its human is waiting for consent, not for a
        // second callback: put it back, so that anyone holding the callback
        // URL cannot destroy a completed login or spend its code twice.
        return Err(restored(
            oauth,
            &login,
            "this login was already completed".to_owned(),
        ));
    }

    let provider = match oauth.provider().await {
        Ok(provider) => provider,
        Err(e) => return Err(restored(oauth, &login, e.to_string())),
    };
    let claims = match provider
        .exchange(
            code,
            &login.upstream_verifier,
            &oauth.config.oidc_client_id,
            oauth.config.oidc_client_secret.as_deref(),
            &callback_uri(oauth),
            &login.nonce,
        )
        .await
    {
        Ok(claims) => claims,
        Err(e) => return Err(restored(oauth, &login, e.to_string())),
    };

    let principal = principal_of(oauth, &claims)?;
    // The offered set is decided here, once, because this is where the human
    // becomes known. It is stored on the login row, so the set the page shows
    // and the set `mcpmem_oauth::consent::approve` validates against are one
    // value that cannot drift apart.
    let offered = mcpmem_oauth::consent::offered(&login.scopes, &principal.scope_set());
    if offered.is_empty() {
        // Nothing to consent to. This is a refusal of the login, and it gets
        // the one page every refusal here gets: whether a named human holds a
        // given scope is not a question an anonymous caller may ask.
        return Err(format!(
            "{} holds none of the requested scopes",
            principal.name
        ));
    }
    let client = oauth
        .with_store(|store| store.get_client(&login.client_id))
        .map_err(|e| format!("the store refused to read the client: {e}"))?
        .ok_or_else(|| "the client of this login is no longer registered".to_owned())?;

    let page = consent_page(&login, &client, principal, &offered);
    oauth
        .with_store(|store| {
            store.put_login(&LoginRecord {
                principal: Some(principal.name.clone()),
                scopes: offered,
                ..login
            })
        })
        .map_err(|e| format!("the store refused to record the human: {e}"))?;
    Ok(page)
}

/// Put `login` back and return `reason`, for a callback that failed before it
/// learned anything about the human.
///
/// The reason is the caller's; a store that refuses the restore appends its
/// own, because the two are different problems and the operator reading the
/// log needs both. Neither reaches the caller: [`callback`] answers one page
/// whatever this says.
#[cfg(feature = "oauth")]
fn restored(oauth: &OauthState, login: &LoginRecord, reason: String) -> String {
    match oauth.with_store(|store| store.put_login(login)) {
        Ok(()) => reason,
        Err(e) => format!("{reason}; and the store refused to restore the login: {e}"),
    }
}

/// The allowed human this identity belongs to.
///
/// The match is on `iss` and `sub` together. `sub` alone would let a second
/// provider — one an operator added later, or one an attacker stood up —
/// mint a subject that names somebody here.
///
/// The whole entry comes back, not the name alone: the consent page needs the
/// label and the offered set needs the scopes, and a second lookup for each
/// could find a different entry from the one that admitted the login.
#[cfg(feature = "oauth")]
fn principal_of<'a>(
    oauth: &'a OauthState,
    claims: &IdentityClaims,
) -> std::result::Result<&'a crate::principals::PrincipalEntry, String> {
    oauth
        .config
        .principals
        .iter()
        .find(|p| p.key() == (claims.iss.as_str(), claims.sub.as_str()))
        .ok_or_else(|| {
            format!(
                "no principal is registered for {} {}",
                claims.iss, claims.sub
            )
        })
}

/// The redirect URI this server registers at the upstream provider. One
/// spelling, because the value is sent twice — once with the authorization
/// request and once with the token exchange — and a provider refuses the
/// exchange when the two differ.
#[cfg(feature = "oauth")]
fn callback_uri(oauth: &OauthState) -> String {
    format!("{}/oauth/callback", oauth.config.public_url)
}

/// A refused authorization request: plain, and with no `Location`.
#[cfg(feature = "oauth")]
fn refused(reason: &'static str) -> Response {
    (StatusCode::BAD_REQUEST, reason).into_response()
}

#[cfg(feature = "oauth")]
fn server_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "this server failed to record the request",
    )
        .into_response()
}

/// The one page a refused login gets. It names no reason, and there is only
/// one of it: see [`callback`].
#[cfg(feature = "oauth")]
fn login_refused() -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>Sign-in failed</title></head><body>\
         <h1>Sign-in failed</h1>\
         <p>This sign-in could not be completed. Close this window and start again.</p>\
         </body></html>",
    )
        .into_response()
}

/// What the human sees once the provider has vouched for them: the consent
/// form, and the values `POST /oauth/consent` needs back.
///
/// The rendering, and every escape in it, lives in
/// [`mcpmem_oauth::consent::page`]. Nothing here formats markup: the client
/// name comes from an unauthenticated registration request, and one escaping
/// rule in one place is the only way that stays true.
///
/// `csrf` goes to the human and comes back on the form. It is bound to this
/// login and single-use, because approval consumes the login row.
#[cfg(feature = "oauth")]
fn consent_page(
    login: &LoginRecord,
    client: &mcpmem_oauth::store::ClientRecord,
    principal: &crate::principals::PrincipalEntry,
    offered: &[String],
) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // A consent form is a decision about one login. A cached copy
            // would carry a token that the first approval has already spent.
            (header::CACHE_CONTROL, "no-store"),
        ],
        mcpmem_oauth::consent::page(
            // Whether a name names anything is the page's question, and
            // `mcpmem_oauth::consent::client_label` states the property once.
            // This check has been wrong three times against ever narrower
            // input; it is not a condition to reproduce here.
            mcpmem_oauth::consent::client_label(&client.client_name, &client.client_id),
            principal.label.as_deref().unwrap_or(&principal.name),
            // The destination of the grant, which the page names beside the
            // client's self-chosen name. It is the login row's own value,
            // matched byte for byte against the registered set at
            // `start_login`, and never the one a consent form sent.
            &login.redirect_uri,
            offered,
            &login.csrf,
            &login.state,
        ),
    )
        .into_response()
}

/// What a consent form may send.
///
/// The body is parsed as pairs rather than deserialized into a shape: `scope`
/// repeats once per ticked box, and no `Deserialize` implementation here
/// collects a repeated key into a list. The pairs are read once, into this.
#[cfg(feature = "oauth")]
struct ConsentForm {
    csrf: Option<String>,
    state: Option<String>,
    scopes: Vec<String>,
    deny: bool,
    /// The body carried more scopes than [`MAX_APPROVED_SCOPES`]. Recorded
    /// rather than acted on here, so the parser has one job and the refusal
    /// stays with every other refusal.
    too_many_scopes: bool,
}

/// The most scopes one consent form may carry.
///
/// The offered set is at most one slug per tool category, so a form ticking
/// every box is far below this. The cap is on the size of the list a crafted
/// body can make this server allocate and then compare, for the same reason
/// [`MAX_REQUESTED_SCOPES`] exists: a rate limit bounds how many requests
/// arrive, never how large one is.
///
/// Reaching it is a refusal, not a truncation. Truncating would answer a
/// crafted body with a grant the human never saw a form for, and silence is
/// the wrong answer to a request this server did not honour in full.
#[cfg(feature = "oauth")]
const MAX_APPROVED_SCOPES: usize = 16;

#[cfg(feature = "oauth")]
fn parse_consent_form(body: &[u8]) -> ConsentForm {
    let mut form = ConsentForm {
        csrf: None,
        state: None,
        scopes: Vec::new(),
        deny: false,
        too_many_scopes: false,
    };
    for (key, value) in url::form_urlencoded::parse(body) {
        match key.as_ref() {
            "csrf" => form.csrf = Some(value.into_owned()),
            "state" => form.state = Some(value.into_owned()),
            "scope" => {
                if form.scopes.len() < MAX_APPROVED_SCOPES {
                    form.scopes.push(value.into_owned());
                } else {
                    form.too_many_scopes = true;
                }
            }
            // The Deny button carries a value; a form with no button pressed
            // carries neither, and is an approval of whatever was ticked.
            "deny" => form.deny = true,
            _ => {}
        }
    }
    form
}

/// `POST /oauth/consent` — the human's decision.
///
/// This is the only place an authorization code is minted, and the only place
/// this server redirects to a client. That redirect is safe because the
/// `redirect_uri` is not in this request: it comes from the login row, where
/// `GET /oauth/authorize` put it after matching it against the client's
/// registered list.
///
/// **A refusal is never a redirect.** A caller who fails the token check has
/// not been shown to hold this login, and sending them anywhere — even to a
/// registered URI — would answer a question about somebody else's session.
#[cfg(feature = "oauth")]
async fn consent(
    State(state): State<HttpState>,
    Peer(peer): Peer,
    body: axum::body::Bytes,
) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Bounded because a form carrying the wrong `csrf` is refused and leaves
    // the login row in place — by design, so a human whose browser lost the
    // page can come back — which also means a guess costs the guesser nothing
    // and may be repeated. The window is ten minutes wide; this bounds how
    // many guesses fit in it.
    if !oauth.limits.consent.check(&peer, (oauth.now_us)()) {
        tracing::warn!(%peer, "consent rate limit reached");
        return too_many_requests(&oauth.limits.consent);
    }
    let form = parse_consent_form(&body);
    if form.too_many_scopes {
        return refused("too many scopes");
    }
    let (Some(csrf), Some(login_state)) = (form.csrf, form.state) else {
        return refused("the consent form needs a state and a csrf token");
    };

    let now_us = (oauth.now_us)();
    let iss = oauth.config.public_url.clone();
    if form.deny {
        return match oauth
            .with_store(|store| mcpmem_oauth::consent::deny(store, &login_state, &csrf, now_us))
        {
            Ok(d) => client_redirect(
                &d.redirect_uri,
                &[
                    ("error", "access_denied"),
                    ("error_description", "the human refused this request"),
                ],
                d.client_state.as_deref(),
                &iss,
            ),
            Err(e) => consent_refused(&e),
        };
    }
    match oauth.with_store(|store| {
        mcpmem_oauth::consent::approve(store, &login_state, &csrf, &form.scopes, now_us)
    }) {
        Ok(a) => client_redirect(
            &a.redirect_uri,
            &[("code", a.code.as_str())],
            a.client_state.as_deref(),
            &iss,
        ),
        Err(e) => consent_refused(&e),
    }
}

/// The answer to a decision this server did not accept.
///
/// The match is exhaustive on purpose: a new refusal must name its own status,
/// and 400 is not a safe default for one that means *you are not the party
/// holding this login*.
#[cfg(feature = "oauth")]
fn consent_refused(e: &mcpmem_oauth::consent::ConsentError) -> Response {
    use mcpmem_oauth::consent::ConsentError;
    match e {
        // 403, not 400: the request was well formed and this caller is not
        // the one the form was handed to.
        ConsentError::BadCsrf => (
            StatusCode::FORBIDDEN,
            "this consent form does not belong to a login in flight",
        )
            .into_response(),
        ConsentError::UnknownLogin
        | ConsentError::NotAuthenticated
        | ConsentError::NotOffered
        | ConsentError::NothingApproved => (
            StatusCode::BAD_REQUEST,
            "this consent could not be recorded",
        )
            .into_response(),
        ConsentError::Store(inner) => {
            tracing::error!(error = %inner, "the OAuth store refused to record a consent");
            server_error()
        }
    }
}

/// Redirect to the client, carrying `params`, the client's own state when it
/// sent one, and this server's issuer identifier.
///
/// `iss` is RFC 9207: a client with more than one authorization server
/// configured must be able to tell which one answered, or a code from a
/// server the attacker controls can be delivered as though it came from this
/// one. It is on every redirect, including a refusal.
///
/// `base` is a registered redirect URI, so it carries no fragment
/// (`mcpmem_oauth::registration`) and appending a query is safe. It may carry
/// a query of its own, which is legal and must be kept.
#[cfg(feature = "oauth")]
fn client_redirect(
    base: &str,
    params: &[(&str, &str)],
    client_state: Option<&str>,
    iss: &str,
) -> Response {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in params {
        query.append_pair(key, value);
    }
    if let Some(client_state) = client_state {
        query.append_pair("state", client_state);
    }
    query.append_pair("iss", iss);
    let separator = if base.contains('?') { '&' } else { '?' };
    let location = format!("{base}{separator}{}", query.finish());
    (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
}

// ── Tokens ──────────────────────────────────────────────────────────────────

/// What a token or revocation request may carry.
///
/// Parsed as pairs rather than deserialized into a shape, for a reason no
/// `Deserialize` implementation gives: RFC 6749 section 3.1 forbids a repeated
/// parameter, and every form deserializer silently keeps one of the two. Which
/// one it keeps decides whether a smuggled second `code_verifier` is the one
/// checked, so the repeat is recorded here and refused rather than resolved.
#[cfg(feature = "oauth")]
#[derive(Default)]
struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    code_verifier: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    refresh_token: Option<String>,
    /// RFC 7009's one required parameter, on the revocation endpoint.
    token: Option<String>,
    /// A parameter arrived more than once. Recorded rather than acted on
    /// here, so the parser has one job and the refusal stays with the others.
    repeated: bool,
}

#[cfg(feature = "oauth")]
fn parse_token_form(body: &[u8]) -> TokenForm {
    let mut form = TokenForm::default();
    for (key, value) in url::form_urlencoded::parse(body) {
        let slot = match key.as_ref() {
            "grant_type" => &mut form.grant_type,
            "code" => &mut form.code,
            "code_verifier" => &mut form.code_verifier,
            "client_id" => &mut form.client_id,
            "redirect_uri" => &mut form.redirect_uri,
            "refresh_token" => &mut form.refresh_token,
            "token" => &mut form.token,
            // `token_type_hint` (RFC 7009 section 2.1) is accepted and
            // ignored: it is an optimization for a server that keeps its two
            // kinds in different tables, and this one finds the row either
            // way. Everything else a client sends is ignored as well, which
            // is what RFC 6749 section 3.1 asks for.
            _ => continue,
        };
        if slot.is_some() {
            form.repeated = true;
        }
        *slot = Some(value.into_owned());
    }
    form
}

/// The value of a required parameter. An empty value is a missing one: a
/// client whose variable was never filled in sends `client_id=`, and reading
/// that as a present value turns a malformed request into a refused grant.
#[cfg(feature = "oauth")]
fn required<'a>(
    value: Option<&'a String>,
    missing: &'static str,
) -> std::result::Result<&'a str, TokenError> {
    value
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .ok_or(TokenError::InvalidRequest(missing))
}

/// `POST /oauth/token` — RFC 6749 section 3.2.
///
/// The endpoint is unauthenticated, because every client this server
/// registers is public: it runs on somebody's laptop and can keep no secret,
/// which is why the discovery document advertises
/// `token_endpoint_auth_methods_supported: ["none"]`. PKCE stands in for
/// client authentication — the code is redeemable only by whoever holds the
/// verifier behind the challenge the authorization request carried.
#[cfg(feature = "oauth")]
async fn token(
    State(state): State<HttpState>,
    Peer(peer): Peer,
    body: axum::body::Bytes,
) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let now_us = (oauth.now_us)();
    // Unauthenticated, so a caller may present a guess. One authorization code
    // has a sixty-second life and one refresh token is a 32-byte random value,
    // so guessing was never the threat this bounds; the cost of the attempt is.
    if !oauth.limits.credential.check(&peer, now_us) {
        tracing::warn!(%peer, "token rate limit reached");
        return too_many_requests(&oauth.limits.credential);
    }
    let form = parse_token_form(&body);
    match oauth.with_store(|store| granted(store, &form, now_us)) {
        Ok(response) => (
            StatusCode::OK,
            // RFC 6749 section 5.1. The body carries two live credentials, so
            // nothing between here and the client may keep a copy.
            [(header::CACHE_CONTROL, "no-store")],
            Json(response),
        )
            .into_response(),
        Err(e) => token_refused(&e),
    }
}

/// Dispatch one token request to the grant it names, and record the client's
/// use when one is granted.
///
/// The touch is on the **granted** path alone. A refused request names a
/// `client_id` too, and honouring that would let anyone keep any registration
/// alive for ever with one well-formed refusal a month — the opposite of what
/// the eviction is for. A grant, by contrast, has already been shown to belong
/// to that client.
///
/// It runs under the store lock, and everything it calls is one statement
/// group against that store — no `await`, no network, nothing that blocks.
#[cfg(feature = "oauth")]
fn granted(
    store: &mcpmem_oauth::store::Store,
    form: &TokenForm,
    now_us: i64,
) -> std::result::Result<mcpmem_oauth::token::TokenResponse, TokenError> {
    if form.repeated {
        return Err(TokenError::InvalidRequest(
            "a parameter arrived more than once",
        ));
    }
    let granted = match form.grant_type.as_deref() {
        Some("authorization_code") => mcpmem_oauth::token::grant_authorization_code(
            store,
            &mcpmem_oauth::token::CodeExchange {
                code: required(form.code.as_ref(), "code is required")?,
                code_verifier: required(form.code_verifier.as_ref(), "code_verifier is required")?,
                client_id: required(form.client_id.as_ref(), "client_id is required")?,
                redirect_uri: required(form.redirect_uri.as_ref(), "redirect_uri is required")?,
            },
            now_us,
        ),
        Some("refresh_token") => mcpmem_oauth::token::grant_refresh(
            store,
            &mcpmem_oauth::token::RefreshExchange {
                refresh_token: required(form.refresh_token.as_ref(), "refresh_token is required")?,
                client_id: required(form.client_id.as_ref(), "client_id is required")?,
            },
            now_us,
        ),
        _ => Err(TokenError::UnsupportedGrantType),
    }?;
    if let Some(client_id) = form.client_id.as_deref()
        && let Err(e) = store.touch_client(client_id, now_us)
    {
        tracing::warn!(error = %e, "the OAuth store refused to record a client use");
    }
    Ok(granted)
}

/// `POST /oauth/revoke` — RFC 7009.
///
/// Unauthenticated for the same reason the token endpoint is, and it gives
/// nobody a power they did not have: the only value that revokes a family is a
/// live token from it, and whoever holds one of those can already spend it.
#[cfg(feature = "oauth")]
async fn revoke_token(
    State(state): State<HttpState>,
    Peer(peer): Peer,
    body: axum::body::Bytes,
) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let now_us = (oauth.now_us)();
    // The same limiter as the token endpoint: both are one caller presenting
    // one credential, and neither is reached from a browser, so one budget for
    // "present a credential" is what a legitimate client needs.
    if !oauth.limits.credential.check(&peer, now_us) {
        tracing::warn!(%peer, "revocation rate limit reached");
        return too_many_requests(&oauth.limits.credential);
    }
    let form = parse_token_form(&body);
    if form.repeated {
        return token_refused(&TokenError::InvalidRequest(
            "a parameter arrived more than once",
        ));
    }
    let token = match required(form.token.as_ref(), "token is required") {
        Ok(token) => token,
        Err(e) => return token_refused(&e),
    };
    match oauth.with_store(|store| mcpmem_oauth::token::revoke(store, token, now_us)) {
        // RFC 7009 section 2.2: an empty 200, including for a token this
        // server has never heard of.
        Ok(()) => (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")]).into_response(),
        Err(e) => token_refused(&e),
    }
}

/// The RFC 6749 section 5.2 error object for a refused token request.
///
/// A description accompanies the two codes that describe the *request*: both
/// are a developer's own mistake and neither says anything about anybody's
/// credential. `invalid_grant` carries none. Unknown, expired, already spent,
/// revoked, issued to another client, issued for another redirect URI, or
/// presented with the wrong verifier are one answer to an unauthenticated
/// caller, and which of the seven it was is the one thing a guesser wants.
/// The reason goes to the log instead.
#[cfg(feature = "oauth")]
fn token_refused(e: &TokenError) -> Response {
    let description = match e {
        TokenError::InvalidRequest(reason) => Some(*reason),
        TokenError::UnsupportedGrantType => {
            Some("this server issues the authorization_code and refresh_token grants")
        }
        TokenError::InvalidGrant(_) => None,
        TokenError::Store(inner) => {
            tracing::error!(error = %inner, "the OAuth store refused a token request");
            return server_error();
        }
    };
    tracing::debug!(error = %e, "a token request was refused");
    let mut body = json!({ "error": e.code() });
    if let Some(description) = description {
        body["error_description"] = json!(description);
    }
    (
        StatusCode::BAD_REQUEST,
        [(header::CACHE_CONTROL, "no-store")],
        Json(body),
    )
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
            approval_waitlist: false,
            approval_waitlist_ttl_seconds: 24 * 60 * 60,
            default_new_principal_scopes: vec!["graph-read".to_owned()],
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
            auth_token: None,
            metadata_fetch: None,
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
            auth_token: None,
            metadata_fetch: None,
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
