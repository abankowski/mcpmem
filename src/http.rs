//! MCP **Streamable HTTP** transport (the 2025-03-26 transport that
//! superseded the older HTTP+SSE pair).
//!
//! * `POST /mcp` — the client sends one JSON-RPC message (or a batch array).
//!   The reply is delivered as `application/json` by default, or as a one-shot
//!   `text/event-stream` (SSE) event when the client `Accept`s it. A body of
//!   only notifications gets `202 Accepted` with no content.
//! * `GET /mcp` — opens a standalone server→client SSE stream. This server has
//!   no server-initiated messages, so the stream simply stays open with
//!   keep-alives; it exists for spec compliance.
//!
//! `/` is also wired to the same handlers for convenience. The JSON-RPC
//! semantics are identical to the stdio and TCP transports — only framing
//! differs (see [`crate::server::dispatch_http_body`]).
//!
//! The browser viewer has a static `/ui` shell and scoped JSON routes.
//! `GET /ui/workspaces` lists accessible graphs. The `/ui/graph`,
//! `/ui/search`, `/ui/node`, and `/ui/expand` data routes resolve the selected
//! workspace before they read its graph. The data routes use the same
//! `graph-read` permission as `read_graph`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use futures::StreamExt;
use mcpmem_core::attachments::{AttachmentError, AttachmentLimits, AttachmentRepository};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tower_http::compression::CompressionLayer;
use tracing::{error, info};

use crate::authz::Principal;
use crate::errors::{MCSError, Result};
use crate::kg::GraphHandle;
use crate::oauth_routes::OauthState;
use crate::server::{self, HttpOutcome};
use crate::tools::ToolCategory;
use crate::workspace::{WorkspaceAccess, WorkspaceError};

/// The subscription tools and the admin handlers below share the store's
/// validation: both call [`webhooks_actions`]' checks, so the MCP surface
/// and the admin surface accept exactly the same payloads.
#[cfg(feature = "webhooks")]
use crate::actions::webhooks as webhooks_actions;
/// The subscription store and its admin handlers exist only in a build with
/// the `webhooks` feature, so everything they import is gated with them: a
/// default build has no webhook types in `http.rs`'s namespace at all.
#[cfg(feature = "webhooks")]
use mcpmem_core::mutation::ChangeOperation;
#[cfg(feature = "webhooks")]
use mcpmem_core::subscriptions::{SubscriptionRepository, WebhookSubscription};
#[cfg(feature = "webhooks")]
use mcpmem_webhook::WorkerError;
#[cfg(feature = "webhooks")]
use uuid::Uuid;

/// The graph viewer's static assets, embedded at build time (served from `/ui`).
const UI_INDEX_HTML: &str = include_str!("ui/index.html");
const UI_CSS: &str = include_str!("ui/graph.css");
const UI_JS: &str = include_str!("ui/graph.js");

/// The site navigation bar shared by both browser shells. The two pages keep
/// their own palettes, so this one stylesheet carries the bar's own tokens
/// and never borrows either page's.
const UI_NAV_CSS: &str = include_str!("ui/nav.css");

/// The admin SPA's static assets, embedded at build time (served from
/// `/ui/admin`). Like the viewer's, the shell holds no data: the JSON API
/// behind it is the gate.
const ADMIN_INDEX_HTML: &str = include_str!("ui/admin.html");
const ADMIN_JS: &str = include_str!("ui/admin.js");
const ADMIN_CSS: &str = include_str!("ui/admin.css");

/// Upper bound on entities returned to the viewer in one `GET /ui/graph` load,
/// mirroring the `read_graph` search cap. Keeps the payload — and the browser's
/// force layout — bounded for very large graphs.
const MAX_UI_NODES: usize = 1000;

/// Upper bound on hops for a single `GET /ui/expand` traversal (double-click to
/// expand). One hop matches the Neo4j "expand relationships" gesture; the cap
/// bounds a single interaction's payload.
const MAX_UI_EXPAND_DEPTH: u32 = 3;

/// The raw upload and blob download move at most this many bytes per spool or
/// SQLite operation. The MCP JSON-RPC body limit is unchanged.
const ATTACHMENT_STREAM_CHUNK: usize = 64 * 1024;

/// The number of upload bodies that may hold an anonymous spool in this
/// process at once. Each spool can hold up to `max_bytes` of uncommitted
/// body bytes, so this bound caps the aggregate temporary disk a burst of
/// concurrent chunked uploads can consume at
/// `MAX_CONCURRENT_ATTACHMENT_SPOOLS * max_bytes`. A new upload is rejected
/// before it opens a spool file when the bound is full.
pub const MAX_CONCURRENT_ATTACHMENT_SPOOLS: usize = 4;

/// The `Server` response header carried on every HTTP response,
/// `mcpmem <version>`. An operator can identify the running build from any
/// reply — `curl -i http://host:port/` answers before any MCP handshake.
const SERVER_HEADER_VALUE: &str = concat!("mcpmem ", env!("CARGO_PKG_VERSION"));

/// Shared HTTP state. MCP dispatch and viewer requests use the same registry
/// and bounded graph-handle cache; there is no process-global viewer graph.
#[derive(Clone)]
pub struct HttpState {
    registry: Arc<crate::workspace::WorkspaceRegistry>,
    /// The bounded per-workspace handle cache MCP dispatch resolves through.
    handles: Arc<crate::workspace::WorkspaceHandles>,
    auth_token: Option<Arc<str>>,
    bearer_scopes: Arc<[ToolCategory]>,
    /// The tool categories enabled on this server. These are the scopes the
    /// discovery documents and the 401 challenge advertise.
    pub(crate) enabled_categories: Arc<[ToolCategory]>,
    /// `Some` turns OAuth on: the discovery routes answer, and the challenge
    /// names them.
    pub(crate) oauth: Option<Arc<OauthState>>,
    /// The per-file cap, workspace budget, and MIME policy for attachment routes.
    attachment_limits: Arc<AttachmentLimits>,
    /// The process-wide cap on concurrently spooling upload bodies. One permit
    /// is held from just before the spool opens until the storing transaction
    /// finishes, so abandoned or disconnected uploads cannot stack anonymous
    /// spool files past the bound.
    attachment_spools: Arc<tokio::sync::Semaphore>,
}

/// What a test wants its [`HttpState`] to hold. Named fields, because
/// `bearer_scopes` and `enabled_categories` are two lists of one type and a
/// positional call cannot show which is which.
#[doc(hidden)]
pub struct TestSetup {
    pub db_path: std::path::PathBuf,
    /// `Some` turns OAuth on for this state.
    pub oauth: Option<crate::config::OAuthConfig>,
    /// The static bearer token, when the deployment under test configures one
    /// beside OAuth. `--auth-token-file` and `--oidc-issuer` are independent
    /// flags and the runbook sells the pair, so the combination has to be
    /// reachable from a fixture.
    pub auth_token: Option<Arc<str>>,
    /// How the OAuth state reads a client identifier metadata document.
    /// `None` is the shipped fetcher, which speaks `https` to a host on the
    /// operator's allow-list — so a test that drives the authorization
    /// endpoint with a metadata-document identifier names one here.
    pub metadata_fetch: Option<Arc<dyn mcpmem_oauth::registration::Fetch>>,
    /// Scopes held by the configured static bearer token.
    /// An HTTP request without a credential never inherits these scopes.
    pub bearer_scopes: Vec<ToolCategory>,
    /// Categories this server exposes. These are the advertised OAuth scopes,
    /// and they publish the process-wide category flags.
    pub enabled_categories: Vec<ToolCategory>,
    /// The clock the OAuth state reads. `None` uses the wall clock.
    pub now_us: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
}

impl HttpState {
    /// The OAuth state, or `None` when OAuth is off. For a test that inspects
    /// the store or the clock behind a router.
    #[doc(hidden)]
    pub const fn oauth(&self) -> Option<&Arc<OauthState>> {
        self.oauth.as_ref()
    }

    /// Build a state over a fresh database, for integration tests that drive
    /// [`router`] with `tower::ServiceExt::oneshot`.
    ///
    /// The graph is opened through [`crate::server::MCPServer::new_kg`], the
    /// same entry point `src/main.rs` uses. That matters twice: it migrates the
    /// schema the OAuth store needs, and it publishes the process-wide tool
    /// category flags that `dispatch_http_body` consults before any scope
    /// check. Building the graph directly leaves those flags `false`, and every
    /// `tools/call` through the router then answers `MethodNotFound`.
    ///
    /// Those flags are process-wide, so two tests in one binary that need
    /// different category sets must not run at the same time.
    ///
    /// `TestSetup::auth_token` decides whether the state carries a static
    /// bearer token beside the OAuth one. A fixture that names none tests the
    /// OAuth credential alone; one that names it tests the deployment the
    /// runbook sells, where both are configured and `principal_of` falls
    /// through from the first to the second.
    #[doc(hidden)]
    pub fn for_test(setup: TestSetup) -> HttpState {
        let TestSetup {
            db_path,
            oauth,
            auth_token,
            metadata_fetch,
            bearer_scopes,
            enabled_categories,
            now_us,
        } = setup;
        let config = crate::config::Config {
            memory_file_path: db_path.to_string_lossy().into_owned(),
            legacy_owner_id: Some("machine:local".into()),
            oauth: oauth.clone(),
            auth_token: auth_token.clone(),
            enabled_categories: enabled_categories.clone(),
            ..crate::config::Config::default()
        };
        let attachment_limits = attachment_limits(config.attachments.clone());
        let busy_timeout_ms = config.busy_timeout_ms;
        let server = crate::server::MCPServer::new_kg(config).expect("build the test server");
        let registry = server.workspace_registry();
        let handles = server.workspace_handles();
        let oauth = oauth.map(|config| {
            let state = match now_us {
                Some(clock) => {
                    OauthState::open_with_clock(config, &db_path, busy_timeout_ms, clock)
                }
                None => OauthState::open(config, &db_path, busy_timeout_ms),
            };
            let state = state.expect("open the test OAuth store");
            Arc::new(match metadata_fetch {
                Some(fetch) => state.with_metadata_fetch(fetch),
                None => state,
            })
        });
        HttpState {
            registry,
            handles,
            auth_token,
            bearer_scopes: Arc::from(bearer_scopes),
            enabled_categories: Arc::from(enabled_categories),
            oauth,
            attachment_limits,
            attachment_spools: Arc::new(tokio::sync::Semaphore::new(
                MAX_CONCURRENT_ATTACHMENT_SPOOLS,
            )),
        }
    }
}

/// Everything [`run`] needs. One struct rather than positional parameters:
/// `bearer_scopes` and `enabled_categories` have the same type, and no
/// compiler catches a swap between two arguments of one type.
pub struct HttpRunConfig {
    pub addr: String,
    pub registry: Arc<crate::workspace::WorkspaceRegistry>,
    /// The bounded per-workspace handle cache MCP dispatch resolves through.
    pub handles: Arc<crate::workspace::WorkspaceHandles>,
    pub auth_token: Option<Arc<str>>,
    /// Scopes granted to the static bearer token.
    pub bearer_scopes: Arc<[ToolCategory]>,
    /// Tool categories enabled on this server, advertised as OAuth scopes.
    pub enabled_categories: Arc<[ToolCategory]>,
    /// The upload cap, budget, and MIME policy from the server configuration.
    pub attachments: crate::config_file::AttachmentsSection,
    pub oauth: Option<Arc<OauthState>>,
    pub tls_cert: Option<std::path::PathBuf>,
    pub tls_key: Option<std::path::PathBuf>,
}

/// Build the axum router for the HTTP transport. Exposed so tests can drive it
/// with `tower::ServiceExt::oneshot` without binding a socket.
pub fn router(state: HttpState) -> Router {
    let router = Router::new()
        .route("/mcp", post(post_handler).get(get_handler))
        .route("/", post(post_handler).get(get_handler))
        .route("/ui", get(ui_handler))
        .route("/ui/nav.css", get(ui_nav_css_handler))
        .route("/ui/graph.css", get(ui_css_handler))
        .route("/ui/graph.js", get(ui_js_handler))
        .route("/ui/workspaces", get(ui_workspaces_handler))
        .route("/ui/graph", get(ui_graph_handler))
        .route("/ui/search", get(ui_search_handler))
        .route("/ui/node", get(ui_node_handler))
        .route("/ui/expand", get(ui_expand_handler))
        .route(
            "/ui/attachments",
            get(list_attachments_handler)
                .post(post_attachment_handler)
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/ui/attachments/{id}",
            get(get_attachment_handler).delete(delete_attachment_handler),
        )
        .route(
            "/ui/attachments/{id}/pages",
            get(get_attachment_page_handler),
        )
        .route(
            "/ui/attachments/{id}/download",
            get(download_attachment_handler),
        )
        // The admin SPA shell and its assets, static like the viewer's. The
        // JSON API below is the gate: these routes hold no data.
        .route("/ui/admin", get(admin_page_handler))
        .route("/ui/admin/callback", get(admin_page_handler))
        .route("/ui/admin.js", get(admin_js_handler))
        .route("/ui/admin.css", get(admin_css_handler))
        // The admin API. Every handler is gated on the `admin` scope, and the
        // static `/ui/admin` page routes arrive with their assets in the task
        // that ships the admin UI.
        .route(
            "/ui/api/principals",
            get(admin_list_principals).post(admin_create_principal),
        )
        .route(
            "/ui/api/principals/{id}",
            patch(admin_update_principal).delete(admin_delete_principal),
        )
        .route("/ui/api/waitlist", get(admin_list_waitlist))
        .route(
            "/ui/api/waitlist/{id}/approve",
            post(admin_approve_waitlist),
        )
        .route("/ui/api/waitlist/{id}", delete(admin_dismiss_waitlist));
    // The webhook-subscription admin API exists only in a build with the
    // `webhooks` feature. Elsewhere the routes are absent and the SPA
    // answers their 404 by hiding the section.
    #[cfg(feature = "webhooks")]
    let router = attach_webhook_admin_routes(router);
    // The managed-repo admin API exists only in a build with the `code`
    // feature. Elsewhere the routes are absent and the SPA hides the section.
    #[cfg(feature = "code")]
    let router = attach_repo_admin_routes(router);
    // The two `.well-known` documents. With OAuth off both answer 404, so a
    // server without OAuth advertises no authorization server.
    crate::oauth_routes::attach(router)
        // Compress responses when the client advertises support. The graph JSON
        // and the embedded HTML/CSS/JS are highly compressible; the default
        // predicate skips already-compressed types and, importantly, SSE
        // (`text/event-stream`), so the `/mcp` streams are left untouched.
        .layer(CompressionLayer::new())
        .layer(DefaultBodyLimit::max(server::MAX_REQUEST_BYTES))
        // Outermost, after the layers above: the `Server` header must survive
        // on every response, including ones those layers reject.
        .layer(middleware::from_fn(server_header_layer))
        .with_state(state)
}

/// Bind `config.addr` and serve the HTTP transport until the process is killed.
///
/// When `tls_cert` and `tls_key` are both set, the transport is served over TLS
/// (HTTPS); otherwise it stays plaintext. The caller (`config.rs`) guarantees
/// the two are set together.
pub async fn run(config: HttpRunConfig) -> Result<()> {
    let HttpRunConfig {
        addr,
        registry,
        handles,
        auth_token,
        bearer_scopes,
        enabled_categories,
        attachments,
        oauth,
        tls_cert,
        tls_key,
    } = config;
    crate::config::Config::require_http_auth(oauth.is_some(), auth_token.as_deref())?;
    let auth = if oauth.is_some() {
        if auth_token.is_some() {
            "static bearer + oauth"
        } else {
            "oauth"
        }
    } else {
        "static bearer"
    };
    let state = HttpState {
        registry,
        handles,
        auth_token,
        bearer_scopes,
        enabled_categories,
        attachment_limits: attachment_limits(attachments),
        attachment_spools: Arc::new(tokio::sync::Semaphore::new(
            MAX_CONCURRENT_ATTACHMENT_SPOOLS,
        )),
        oauth,
    };

    if let (Some(cert), Some(key)) = (tls_cert, tls_key) {
        let tls = crate::tls::server_config(&cert, &key)
            .await
            .map_err(MCSError::IoError)?;
        let socket_addr = resolve_addr(&addr)?;
        info!(
            "Listening for HTTPS (Streamable) MCP on https://{socket_addr}/mcp (TLS, auth {auth})"
        );
        // `into_make_service_with_connect_info` rather than
        // `into_make_service`: without it no handler can see the peer address,
        // and `oauth_routes::Peer` would count every anonymous caller in one
        // bucket — a rate limit that one client can use to lock out the rest.
        axum_server::bind_rustls(socket_addr, tls)
            .serve(router(state).into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await
            .map_err(MCSError::IoError)?;
    } else {
        let listener = TcpListener::bind(&addr).await.map_err(MCSError::IoError)?;
        info!("Listening for HTTP (Streamable) MCP on http://{addr}/mcp (auth {auth})");
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .map_err(MCSError::IoError)?;
    }
    Ok(())
}

/// Resolve a `host:port` string to a single `SocketAddr` for `axum_server`,
/// which binds an address rather than an already-bound listener.
fn resolve_addr(addr: &str) -> Result<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    addr.to_socket_addrs()
        .map_err(MCSError::IoError)?
        .next()
        .ok_or_else(|| {
            MCSError::IoError(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("could not resolve bind address '{addr}'"),
            ))
        })
}

/// Tag every response with the server version, whatever inner layer produced
/// it — a handler result, a 404 fallback, a 413 from the body limit. Placed
/// outermost in `router`, so no response leaves without it.
async fn server_header_layer(request: Request<axum::body::Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::SERVER,
        HeaderValue::from_static(SERVER_HEADER_VALUE),
    );
    response
}

fn wants_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/event-stream"))
}

/// Resolve the caller, or `None` when the request is unauthorized.
///
/// The order is OAuth, configured static bearer, then a registered machine.
/// Each credential keeps its own scopes and principal ID.
///
/// A presented token must match one of these credentials. A missing or unknown
/// token is unauthorized; open HTTP never gets a machine identity.
fn principal_of(state: &HttpState, headers: &HeaderMap) -> Option<Principal> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().strip_prefix("Bearer ").unwrap_or(v).trim());

    if let (Some(oauth), Some(token)) = (state.oauth.as_ref(), presented)
        && let Some(grant) = oauth.validate(token)
    {
        if !oauth.human_registered(&grant.principal) {
            return None;
        }
        return Some(crate::authz::oauth_principal(
            &grant.principal,
            grant.scopes.into_iter().collect(),
        ));
    }
    if let (Some(expected), Some(token)) = (state.auth_token.as_ref(), presented)
        && server::token_matches(token, expected)
    {
        return Some(crate::authz::bearer_principal(&state.bearer_scopes));
    }
    if let Some(token) = presented {
        match state.registry.authenticate_machine(token) {
            Ok(Some(principal)) => return Some(principal),
            Ok(None) => {}
            Err(error) => {
                error!("machine credential lookup failed: {error}");
                return None;
            }
        }
    }
    None
}

/// The RFC 6750 challenge. With OAuth on it names the resource metadata, so the
/// client can discover the authorization server. With OAuth off it is bare.
///
/// The `scope` parameter is omitted when no category is enabled. RFC 6749
/// section 3.3, which RFC 6750 section 3 defers to, admits no empty scope
/// list, and a parser that rejects the malformed parameter discards the whole
/// header — including the only discovery pointer the client has.
fn unauthorized(state: &HttpState) -> Response {
    let mut value = String::from("Bearer");
    if let Some(oauth) = state.oauth.as_ref() {
        value.push_str(&format!(
            " resource_metadata=\"{}\"",
            oauth.resource_metadata()
        ));
        let scope = state
            .enabled_categories
            .iter()
            .map(|c| c.slug())
            .collect::<Vec<_>>()
            .join(" ");
        if !scope.is_empty() {
            value.push_str(&format!(", scope=\"{scope}\""));
        }
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, value)],
        "Unauthorized",
    )
        .into_response()
}

/// The RFC 6750 section 3.1 `insufficient_scope` challenge, which is the 403
/// a connector parses to learn what to ask the human for next.
///
/// The order of the parameters is the contract, and it is fixed here rather
/// than assembled by the caller: `error`, then `scope`, then — with OAuth on
/// — `resource_metadata`. The last one is what makes the 403 actionable: a
/// client that has been refused for a scope it does not hold needs the
/// document naming the authorization server to go and get one.
fn insufficient_scope(state: &HttpState, scopes: &[&'static str]) -> Response {
    let scope = scopes.join(" ");
    let mut value = format!("Bearer error=\"insufficient_scope\", scope=\"{scope}\"");
    if let Some(oauth) = state.oauth.as_ref() {
        value.push_str(&format!(
            ", resource_metadata=\"{}\"",
            oauth.resource_metadata()
        ));
    }
    (
        StatusCode::FORBIDDEN,
        [(header::WWW_AUTHENTICATE, value)],
        "insufficient scope",
    )
        .into_response()
}

/// `POST /mcp` — the one request every tool call arrives on.
///
/// Resolving the caller and dispatching happen in one blocking task, and the
/// order inside it is the order it was before: no body is dispatched for a
/// caller this server will not honour.
///
/// The reason they share the task is [`OauthState::validate`]. It takes the
/// single store mutex and runs a SQLite query under it, which is what
/// `OauthState::with_store` tells its callers not to do on the reactor. On
/// this path there is already a blocking task to put it in, so it goes there.
/// A WAL point lookup is microseconds and a reader never waits on a writer, so
/// what this removes is a ceiling rather than a stall — but the ceiling is on
/// every authenticated request in the process, and the rule the crate states
/// about its own lock should hold where the transport can make it hold.
async fn post_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let registry = state.registry.clone();
    let handles = state.handles.clone();
    let auth = state.clone();
    // Read before `headers` moves into the task. One header lookup, and the
    // alternative is cloning the whole map per request.
    let sse = wants_sse(&headers);
    let result = tokio::task::spawn_blocking(move || {
        let principal = principal_of(&auth, &headers)?;
        Some(server::dispatch_http_body(
            &body, &principal, &registry, &handles,
        ))
    })
    .await;

    let outcome = match result {
        Ok(Some(outcome)) => outcome,
        Ok(None) => return unauthorized(&state),
        Err(join_err) => {
            error!("dispatch task panicked: {join_err}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };

    match outcome {
        // Body held only notifications → nothing to return.
        Ok(HttpOutcome::Accepted) => StatusCode::ACCEPTED.into_response(),
        Ok(HttpOutcome::Body(value)) => {
            if sse {
                // One JSON-RPC reply delivered as a single SSE event, then close.
                let json = serde_json::to_string(&value).unwrap();
                let stream = futures::stream::once(async move {
                    Ok::<Event, Infallible>(Event::default().data(json))
                });
                Sse::new(stream).into_response()
            } else {
                Json(value).into_response()
            }
        }
        Ok(HttpOutcome::InsufficientScope(scopes)) => insufficient_scope(&state, &scopes),
        Err(e) => {
            // Malformed JSON body → JSON-RPC parse error.
            let resp = json!({
                "jsonrpc": "2.0",
                "error": { "code": -32700, "message": format!("Parse error: {e}") },
                "id": null
            });
            (StatusCode::BAD_REQUEST, Json(resp)).into_response()
        }
    }
}

async fn get_handler(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if principal_of(&state, &headers).is_none() {
        return unauthorized(&state);
    }
    // No server-initiated messages: an open, keep-alive'd stream for compliance.
    let stream = futures::stream::pending::<std::result::Result<Event, Infallible>>();
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Like [`principal_of`], but also accepting the static bearer token from a
/// `token` query parameter.
///
/// The shipped viewer does not use that fallback. `src/ui/graph.js` reads
/// `#token=` from the URL fragment — which a browser never sends to a server —
/// keeps it in `sessionStorage`, and puts it in the `Authorization` header of
/// every data request, so a human at `/ui` is resolved by [`principal_of`] and
/// may present either credential. The query fallback is for a script, and it
/// takes the static token alone: an issued token in a URL is a credential in a
/// history file, a proxy log and a `Referer` header.
fn principal_of_ui(
    state: &HttpState,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Option<Principal> {
    principal_of(state, headers).or_else(|| match (state.auth_token.as_ref(), query_token) {
        (Some(expected), Some(token)) if server::token_matches(token, expected) => {
            Some(crate::authz::bearer_principal(&state.bearer_scopes))
        }
        _ => None,
    })
}

/// The principal behind this request, if it holds the admin scope. The
/// static bearer token can never hold admin, so every admin is a human
/// resolved through an OAuth grant.
fn admin_principal(state: &HttpState, headers: &HeaderMap) -> Option<Principal> {
    let p = principal_of_ui(state, headers, None)?;
    p.scopes
        .contains(crate::principals::ADMIN_SCOPE)
        .then_some(p)
}

/// The gate every `/ui/api/*` handler runs first. On success, return the
/// authorized principal so workspace checks do not repeat token validation.
fn admin_gate(
    state: &HttpState,
    headers: &HeaderMap,
) -> std::result::Result<Principal, Box<Response>> {
    if let Some(principal) = admin_principal(state, headers) {
        return Ok(principal);
    }
    let response = if principal_of_ui(state, headers, None).is_some() {
        insufficient_scope(state, &[crate::principals::ADMIN_SCOPE])
    } else {
        unauthorized(state)
    };
    Err(Box::new(response))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentView {
    attachment_id: i64,
    filename: String,
    mime: String,
    size_bytes: i64,
    status: String,
    revision: i64,
    error_stage: Option<String>,
    last_error: Option<String>,
    page_count: Option<i64>,
}

fn attachment_row(
    conn: &Connection,
    id: i64,
    entity_name: bool,
) -> std::result::Result<(AttachmentView, Option<String>), Box<Response>> {
    let entity_select = if entity_name { "e.name" } else { "NULL" };
    let (view, entity) = conn
        .query_row(
            &format!(
                "SELECT a.id,a.filename,a.mime,a.size_bytes,a.status,a.revision,
                        a.error_stage,a.last_error,
                        (SELECT count(*) FROM attachment_text t WHERE t.attachment_id=a.id),
                        {entity_select}
                 FROM attachment a
                 JOIN entity e ON e.id=a.entity_id
                 JOIN entity_revision r ON r.entity_id=e.id
                 WHERE a.id=?1 AND e.flags=0 AND r.deleted=0"
            ),
            [id],
            |row| {
                Ok((
                    AttachmentView {
                        attachment_id: row.get(0)?,
                        filename: row.get(1)?,
                        mime: row.get(2)?,
                        size_bytes: row.get(3)?,
                        status: row.get(4)?,
                        revision: row.get(5)?,
                        error_stage: row.get(6)?,
                        last_error: row.get(7)?,
                        page_count: if entity_name { Some(row.get(8)?) } else { None },
                    },
                    row.get(9)?,
                ))
            },
        )
        .optional()
        .map_err(|error| attachment_db_error(&error))?
        .ok_or_else(|| Box::new(not_found()))?;
    let entity = if entity_name { entity } else { None };
    Ok((view, entity))
}

async fn list_attachments_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let entity_name = match params.get("entityName").filter(|s| !s.is_empty()) {
        Some(name) => name.clone(),
        None => return bad_request("missing 'entityName' parameter"),
    };
    let path = match attachment_path(
        &state,
        &headers,
        &params,
        "list_attachments",
        WorkspaceAccess::Read,
    )
    .await
    {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let limit = params
        .get("limit")
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(100)
        .clamp(1, 1000);
    match attachment_result(path, move |conn| {
        let mut rows = conn
            .prepare(
                "SELECT a.id,a.filename,a.mime,a.size_bytes,a.status,a.revision,
                        a.error_stage,a.last_error,
                        (SELECT count(*) FROM attachment_text t WHERE t.attachment_id=a.id)
                 FROM attachment a
                 JOIN entity e ON e.id=a.entity_id
                 JOIN entity_revision r ON r.entity_id=e.id
                 WHERE e.name=?1 AND e.flags=0 AND r.deleted=0
                 ORDER BY a.id DESC LIMIT ?2",
            )
            .map_err(|error| attachment_db_error(&error))?;
        let rows = rows
            .query_map(params![entity_name, limit], |row| {
                Ok(AttachmentView {
                    attachment_id: row.get(0)?,
                    filename: row.get(1)?,
                    mime: row.get(2)?,
                    size_bytes: row.get(3)?,
                    status: row.get(4)?,
                    revision: row.get(5)?,
                    error_stage: row.get(6)?,
                    last_error: row.get(7)?,
                    page_count: row.get(8)?,
                })
            })
            .map_err(|error| attachment_db_error(&error))?;
        let attachments = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| attachment_db_error(&error))?;
        Ok(Json(json!({ "attachments": attachments })))
    })
    .await
    {
        Ok(response) => response.into_response(),
        Err(response) => *response,
    }
}

async fn get_attachment_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let path = match attachment_path(
        &state,
        &headers,
        &params,
        "get_attachment",
        WorkspaceAccess::Read,
    )
    .await
    {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let id = match attachment_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    match attachment_result(path, move |conn| {
        let (view, entity_name) = attachment_row(conn, id, true)?;
        let mut payload = serde_json::to_value(view).map_err(|error| {
            error!("attachment metadata serialize error: {error}");
            Box::new(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            ))
        })?;
        payload["entityName"] = serde_json::Value::String(entity_name.unwrap_or_default());
        Ok(Json(payload))
    })
    .await
    {
        Ok(response) => response.into_response(),
        Err(response) => *response,
    }
}

async fn get_attachment_page_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let path = match attachment_path(
        &state,
        &headers,
        &params,
        "get_attachment_page",
        WorkspaceAccess::Read,
    )
    .await
    {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let id = match attachment_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let page = match attachment_query_number(&params, "page", None, 1) {
        Ok(page) => page,
        Err(response) => return *response,
    };
    let offset = match attachment_query_number(&params, "offset", Some(0), 0) {
        Ok(offset) => offset,
        Err(response) => return *response,
    };
    let limit = match params
        .get("maxChars")
        .and_then(|raw| raw.parse::<i64>().ok())
    {
        Some(limit) if (1..=4096).contains(&limit) => limit,
        Some(_) => return bad_request("'maxChars' must be between 1 and 4096"),
        None => 4096,
    };
    attachment_result(path, move |conn| {
        let text: Option<String> = conn
            .query_row(
                "SELECT t.text FROM attachment_text t
                 JOIN attachment a ON a.id=t.attachment_id
                 JOIN entity e ON e.id=a.entity_id
                 JOIN entity_revision r ON r.entity_id=e.id
                 WHERE t.attachment_id=?1 AND t.page=?2
                   AND e.flags=0 AND r.deleted=0",
                [id, page],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| attachment_db_error(&error))?;
        let Some(text) = text else {
            return Ok((
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "no such page" })),
            ));
        };
        let start: usize = text.chars().take(offset as usize).map(char::len_utf8).sum();
        let rest = &text[start..];
        let remaining = rest.chars().count();
        let taken = remaining.min(limit as usize);
        let end = rest
            .char_indices()
            .nth(taken)
            .map_or(text.len(), |(index, _)| start + index);
        let response = json!({
            "page": page,
            "text": &text[start..end],
            "offset": offset,
            "nextOffset": offset + taken as i64,
            "eof": taken == remaining,
        });
        Ok((StatusCode::OK, Json(response)))
    })
    .await
    .map_or_else(
        |response| *response,
        |(status, body)| (status, body).into_response(),
    )
}

/// Read one bounded span of a stored blob through SQLite's incremental blob
/// I/O, so a download never materializes the whole file in Rust memory.
fn read_blob_chunk(
    conn: &Connection,
    row_id: i64,
    offset: i64,
    amount: usize,
) -> std::result::Result<Option<Vec<u8>>, Box<Response>> {
    let blob = conn
        .blob_open("main", "attachment", "content", row_id, true)
        .map_err(|error| attachment_db_error(&error))?;
    if offset < 0 || offset as usize >= blob.len() {
        return Ok(None);
    }
    let at = offset as usize;
    let take = (blob.len() - at).min(amount);
    let mut buffer = vec![0_u8; take];
    blob.read_at(&mut buffer, at)
        .map_err(|error| attachment_db_error(&error))?;
    Ok(Some(buffer))
}

/// A download filename is response-header material: strip the bytes that can
/// split or corrupt the header (line breaks, quotes, backslashes, control
/// characters). A name reduced to nothing falls back to the bare directive.
fn download_disposition(filename: &str) -> HeaderValue {
    let clean: String = filename
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .collect();
    match HeaderValue::from_str(&format!("attachment; filename=\"{clean}\"")) {
        Ok(value) => value,
        Err(_) => HeaderValue::from_static("attachment"),
    }
}

/// `GET /ui/attachments/{id}/download` — the stored bytes, streamed in
/// bounded pieces straight out of the SQLite blob. Same `attachments` scope
/// and workspace read access as the other read routes; the MIME type and the
/// safe filename come from the stored row.
async fn download_attachment_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let path = match attachment_path(
        &state,
        &headers,
        &params,
        "read_attachment_chunk",
        WorkspaceAccess::Read,
    )
    .await
    {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let id = match attachment_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let result = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(path).map_err(|error| attachment_db_error(&error))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| attachment_db_error(&error))?;
        let (mime, filename, size) = conn
            .query_row(
                "SELECT a.mime,a.filename,a.size_bytes
                 FROM attachment a
                 JOIN entity e ON e.id=a.entity_id
                 JOIN entity_revision r ON r.entity_id=e.id
                 WHERE a.id=?1 AND e.flags=0 AND r.deleted=0",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| attachment_db_error(&error))?
            .ok_or_else(|| Box::new(not_found()))?;
        Ok::<_, Box<Response>>((mime, filename, size, conn))
    })
    .await;
    let (mime, filename, size, conn) = match result {
        Ok(Ok(row)) => row,
        Ok(Err(response)) => return *response,
        Err(error) => {
            error!("attachment download task panicked: {error}");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    let content_type = HeaderValue::from_str(&mime)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let stream = futures::stream::unfold(Some((conn, id, 0_i64, size)), |state| async move {
        let (conn, row_id, offset, size) = state?;
        if offset >= size {
            return None;
        }
        let chunk = match read_blob_chunk(&conn, row_id, offset, ATTACHMENT_STREAM_CHUNK) {
            Ok(Some(bytes)) => bytes,
            Ok(None) | Err(_) => {
                return Some((
                    Err::<Bytes, std::io::Error>(std::io::Error::other("attachment read failed")),
                    None,
                ));
            }
        };
        let next_offset = offset + chunk.len() as i64;
        let next_state = (next_offset < size).then_some((conn, row_id, next_offset, size));
        Some((Ok::<Bytes, std::io::Error>(Bytes::from(chunk)), next_state))
    });
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_DISPOSITION, download_disposition(&filename)),
            (
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&size.to_string())
                    .unwrap_or_else(|_| HeaderValue::from_static("0")),
            ),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

/// `DELETE /ui/attachments/{id}` — remove the attachment and every row it
/// published (page text, segments, jobs, vectors) in one graph transaction.
/// Requires the `attachments` scope and workspace write access.
async fn delete_attachment_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let path = match attachment_path(
        &state,
        &headers,
        &params,
        "delete_attachment",
        WorkspaceAccess::Write,
    )
    .await
    {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let id = match attachment_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    match attachment_result(path, move |conn| {
        AttachmentRepository::new(conn)
            .delete_attachment(id)
            .map(|()| StatusCode::NO_CONTENT)
            .map_err(attachment_failure)
    })
    .await
    {
        Ok(status) => status.into_response(),
        Err(response) => *response,
    }
}

/// Stream a raw upload body through one anonymous spool file, counting and
/// hashing every byte. [`tempfile::tempfile`] creates the file already
/// unlinked, so a dropped request, an error, or a panic removes the spool
/// with the handle — no path survives. The per-frame copy keeps a full file
/// out of Rust memory; the byte counter alone bounds the upload. An empty
/// body spools as a valid zero-byte file: the repository stores it exactly
/// like the MCP contract's `expectedBytes == 0` upload.
async fn spool_body(
    body: Body,
    limits: &AttachmentLimits,
) -> std::result::Result<(std::fs::File, i64, [u8; 32]), Box<Response>> {
    let mut spool = match tempfile::tempfile() {
        Ok(spool) => spool,
        Err(error) => {
            error!("attachment spool create error: {error}");
            return Err(Box::new(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            )));
        }
    };
    let mut count = 0_i64;
    let mut hasher = Sha256::new();
    let mut frames = Box::pin(body.into_data_stream());
    while let Some(frame) = frames.next().await {
        let bytes = match frame {
            Ok(bytes) => bytes,
            Err(error) => {
                error!("attachment upload stream error: {error}");
                return Err(Box::new(json_error(
                    StatusCode::BAD_REQUEST,
                    "attachment upload stream failed",
                )));
            }
        };
        count = match count.checked_add(bytes.len() as i64) {
            Some(count) => count,
            None => {
                return Err(Box::new(json_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "attachment exceeds the per-file size limit",
                )));
            }
        };
        if count > limits.max_bytes {
            return Err(Box::new(json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "attachment exceeds the per-file size limit",
            )));
        }
        let mut rest = bytes.as_ref();
        while !rest.is_empty() {
            let amount = ATTACHMENT_STREAM_CHUNK.min(rest.len());
            if let Err(error) = spool.write_all(&rest[..amount]) {
                error!("attachment spool write error: {error}");
                return Err(Box::new(json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error",
                )));
            }
            hasher.update(&rest[..amount]);
            rest = &rest[amount..];
        }
    }
    if let Err(error) = spool.seek(SeekFrom::Start(0)) {
        error!("attachment spool seek error: {error}");
        return Err(Box::new(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error",
        )));
    }
    Ok((spool, count, hasher.finalize().into()))
}

/// Replay a spool one bounded chunk at a time. The file stays unlinked while
/// the repository reads it inside its transaction, so a failed commit leaves
/// no graph row and no spool path behind.
struct SpoolReader {
    file: std::fs::File,
}

impl Read for SpoolReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(out)
    }
}

impl SpoolReader {
    const fn new(spool: std::fs::File) -> Self {
        Self { file: spool }
    }
}

async fn post_attachment_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    body: Body,
) -> Response {
    let filename = match params.get("filename").filter(|s| !s.is_empty()) {
        Some(filename) => filename.clone(),
        None => return bad_request("missing 'filename' parameter"),
    };
    let entity_name = match params.get("entityName").filter(|s| !s.is_empty()) {
        Some(name) => name.clone(),
        None => return bad_request("missing 'entityName' parameter"),
    };
    let mime = match headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        Some(mime) if !mime.is_empty() => mime.to_owned(),
        _ => return bad_request("missing Content-Type header"),
    };
    let path = match attachment_path(
        &state,
        &headers,
        &params,
        "begin_attachment_upload",
        WorkspaceAccess::Write,
    )
    .await
    {
        Ok(path) => path,
        Err(response) => return *response,
    };
    if let Some(length) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        && length > state.attachment_limits.max_bytes
    {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "attachment exceeds the per-file size limit",
        );
    }
    // One permit per in-flight spool, held until the storing transaction
    // finishes. A full bound rejects the upload before it opens a spool file,
    // so many partial chunked uploads cannot stack arbitrary temporary disk.
    let _spool = match state.attachment_spools.try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "attachment spool capacity is full; retry later",
            );
        }
    };
    let (spool, count, digest) = match spool_body(body, &state.attachment_limits).await {
        Ok(spooled) => spooled,
        Err(response) => return *response,
    };
    let limits = Arc::clone(&state.attachment_limits);
    let entity_name_copy = entity_name.clone();
    match attachment_result(path, move |conn| {
        let entity_id = conn
            .query_row(
                "SELECT e.id FROM entity e
                 JOIN entity_revision r ON r.entity_id=e.id
                 WHERE e.name=?1 AND e.flags=0 AND r.deleted=0",
                [entity_name_copy],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| attachment_db_error(&error))?;
        let Some(entity_id) = entity_id else {
            return Err(Box::new(not_found()));
        };
        let mut reader = SpoolReader::new(spool);
        let attachment_id = AttachmentRepository::new(conn)
            .store_reader(
                entity_id,
                &filename,
                &mime,
                &mut reader,
                count,
                &digest,
                &limits,
                mcpmem_core::events::now_us(),
            )
            .map_err(attachment_failure)?;
        Ok(Json(json!({
            "attachmentId": attachment_id,
            "status": "uploaded",
        })))
    })
    .await
    {
        Ok(response) => (StatusCode::CREATED, response).into_response(),
        Err(response) => *response,
    }
}

/// The id path segment → (iss, sub).
fn key_of_id(id: &str) -> Option<(String, String)> {
    mcpmem_oauth::parse_principal_id(id)
}

/// Whether a built-in principal owns this identity. The keys are owned
/// pairs; `contains` cannot borrow a `(&str, &str)` from them, so the
/// comparison is spelled out rather than cloned per row.
fn is_builtin(oauth: &OauthState, iss: &str, sub: &str) -> bool {
    oauth.builtin_keys.iter().any(|(i, s)| i == iss && s == sub)
}

/// One principal as the admin API answers it: built-ins from the principals
/// file, runtime rows from the store, both in the one shape.
#[derive(serde::Serialize)]
struct PrincipalView {
    id: String,
    name: String,
    iss: String,
    sub: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    scopes: Vec<String>,
    builtin: bool,
    /// The SPA reads this key; the underscore spelling is the field's, the
    /// wire spelling is the contract's.
    #[serde(rename = "maskedByBuiltin")]
    masked_by_builtin: bool,
}

#[derive(serde::Deserialize)]
struct PrincipalInput {
    name: String,
    iss: String,
    sub: String,
    #[serde(default)]
    label: Option<String>,
    scopes: Vec<String>,
}

#[derive(serde::Deserialize)]
struct PrincipalPatch {
    #[serde(default)]
    name: Option<String>,
    /// Some("") clears the label.
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
}

#[derive(serde::Deserialize)]
struct ApproveBody {
    #[serde(default)]
    scopes: Option<Vec<String>>,
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

fn bad_request(message: impl Into<String>) -> Response {
    json_error(StatusCode::BAD_REQUEST, message)
}

fn conflict(message: impl Into<String>) -> Response {
    json_error(StatusCode::CONFLICT, message)
}

fn not_found() -> Response {
    json_error(StatusCode::NOT_FOUND, "no such row")
}

/// Hide whether a denied workspace exists. Invalid selectors and absent
/// defaults keep their distinct input errors for the viewer.
fn workspace_failure(error: &WorkspaceError) -> Response {
    match error {
        WorkspaceError::NotFound | WorkspaceError::AccessDenied => not_found(),
        WorkspaceError::SelectionRequired | WorkspaceError::InvalidInput(_) => {
            bad_request(error.to_string())
        }
        WorkspaceError::Storage(_) | WorkspaceError::Io(_) | WorkspaceError::Graph(_) => {
            error!("workspace lookup failed: {error}");
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "workspace registry error",
            )
        }
    }
}

fn store_failure(e: impl std::fmt::Display) -> Response {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("principals store: {e}"),
    )
}

/// A 500 whose message names the OAuth store, not the principals store: the
/// token-revocation path in the delete handler can fail in either, and a
/// mislabeled body sends the operator to the wrong logs.
fn oauth_store_failure(e: impl std::fmt::Display) -> Response {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("oauth store: {e}"),
    )
}

/// `GET /ui/api/principals` — every principal the server knows: the built-ins
/// from the principals file, then the runtime rows, with a row whose identity
/// a built-in owns marked `masked_by_builtin`. The sidebar reads the mask
/// flag and the `defaultNewPrincipalScopes` list to render the create form.
async fn admin_list_principals(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut out: Vec<PrincipalView> = oauth
        .config
        .principals
        .iter()
        .map(|p| PrincipalView {
            id: mcpmem_oauth::principal_id(&p.iss, &p.sub),
            name: p.name.clone(),
            iss: p.iss.clone(),
            sub: p.sub.clone(),
            label: p.label.clone(),
            scopes: p.scopes.clone(),
            builtin: true,
            masked_by_builtin: false,
        })
        .collect();
    let runtime = match oauth.with_principals(|s| s.list()) {
        Ok(rows) => rows,
        Err(e) => return store_failure(e),
    };
    for row in runtime {
        let masked = is_builtin(oauth, &row.iss, &row.sub);
        out.push(PrincipalView {
            id: mcpmem_oauth::principal_id(&row.iss, &row.sub),
            name: row.name,
            iss: row.iss,
            sub: row.sub,
            label: row.label,
            scopes: row.scopes,
            builtin: false,
            masked_by_builtin: masked,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "principals": out,
            "defaultNewPrincipalScopes": oauth.config.default_new_principal_scopes,
        })),
    )
        .into_response()
}

/// `POST /ui/api/principals` — create one runtime principal. A key a
/// built-in owns is refused before the store is touched: the principals file
/// is the operator's source of truth for those identities.
async fn admin_create_principal(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let input: PrincipalInput = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("the body must be a JSON principal"),
    };
    // Trim into the stored values, like the file loader does: an untrimmed
    // `sub` can never equal a provider claim, so a stray space would create
    // a principal that can never authenticate — and whitespace-only
    // variants of a built-in key would slip past the collision check.
    let name = input.name.trim();
    let iss = input.iss.trim();
    let sub = input.sub.trim();
    if name.is_empty() || iss.is_empty() || sub.is_empty() {
        return bad_request("name, iss and sub are required");
    }
    let scopes = match crate::principals::canonical_scopes(&input.scopes) {
        Ok(s) if !s.is_empty() => s,
        _ => return bad_request("at least one known scope is required"),
    };
    if is_builtin(oauth, iss, sub) {
        return conflict("a built-in principal owns this identity; it is immutable");
    }
    // A duplicate runtime key is refused before the store is touched, so the
    // answer is 409 and not the store's UNIQUE-constraint error.
    match oauth.with_principals(|s| s.get(iss, sub)) {
        Ok(Some(_)) => return conflict("a runtime principal already owns this identity"),
        Ok(None) => {}
        Err(e) => return store_failure(e),
    }
    if let Err(e) =
        oauth.with_principals(|s| s.create(iss, sub, name, input.label.as_deref(), &scopes))
    {
        // The pre-check above is not a lock: two concurrent identical POSTs
        // can both pass it, and the second then hits the UNIQUE constraint.
        // That race is classified by the store and answered as a conflict
        // too, so the caller sees the same 409 either way.
        if matches!(e, crate::errors::MCSError::ConstraintViolation(_)) {
            return conflict("a runtime principal already owns this identity");
        }
        return store_failure(e);
    }
    let view = PrincipalView {
        id: mcpmem_oauth::principal_id(iss, sub),
        name: name.to_owned(),
        iss: iss.to_owned(),
        sub: sub.to_owned(),
        label: input.label,
        scopes,
        builtin: false,
        masked_by_builtin: false,
    };
    (StatusCode::CREATED, Json(view)).into_response()
}

/// `PATCH /ui/api/principals/{id}` — change name, label or scopes of one
/// runtime principal. Every field is optional; a named field replaces the
/// stored value.
async fn admin_update_principal(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: String,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((iss, sub)) = key_of_id(&id) else {
        return not_found();
    };
    if is_builtin(oauth, &iss, &sub) {
        return conflict("a built-in principal owns this identity; it is immutable");
    }
    let row = match oauth.with_principals(|s| s.get(&iss, &sub)) {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(e) => return store_failure(e),
    };
    let patch: PrincipalPatch = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("the body must be a JSON patch"),
    };
    let name = patch.name.as_deref().unwrap_or(&row.name).trim().to_owned();
    if name.is_empty() {
        return bad_request("name must not be empty");
    }
    let label = match patch.label {
        Some(raw) if raw.is_empty() => None,
        value => value.or(row.label.clone()),
    };
    let scopes = match patch.scopes {
        Some(raw) => match crate::principals::canonical_scopes(&raw) {
            Ok(s) if !s.is_empty() => s,
            _ => return bad_request("at least one known scope is required"),
        },
        None => row.scopes,
    };
    if let Err(e) =
        oauth.with_principals(|s| s.update(&iss, &sub, &name, label.as_deref(), &scopes))
    {
        return store_failure(e);
    }
    (
        StatusCode::OK,
        Json(PrincipalView {
            id,
            name,
            iss,
            sub,
            label,
            scopes,
            builtin: false,
            masked_by_builtin: false,
        }),
    )
        .into_response()
}

/// `DELETE /ui/api/principals/{id}` — remove a runtime principal, its workspace
/// access, its pending OAuth grants and its live token families.
///
/// The admin path keeps its bare ID segment. Convert it to the stable human
/// ID before the owner check or token revocation.
async fn admin_delete_principal(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((iss, sub)) = key_of_id(&id) else {
        return not_found();
    };
    if is_builtin(oauth, &iss, &sub) {
        return conflict("a built-in principal owns this identity; it is immutable");
    }
    let row = match oauth.with_principals(|s| s.get(&iss, &sub)) {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(e) => return store_failure(e),
    };
    let stable_id = crate::principals::human_id(&iss, &sub);
    // Hold the sidecar write lock across the owner check, OAuth revocation,
    // runtime row deletion, and access cleanup. Acquire OAuth and runtime
    // store locks only after the registry lock; neither store calls back into
    // the registry here. A failed store operation rolls back access cleanup.
    // A registry commit error after the runtime delete cannot restore that
    // row across the two SQLite files. Return the registry error in that case.
    let revoked = match state.registry.with_human_access_cleanup(&stable_id, || {
        let revoked = oauth
            .revoke_principal(&stable_id)
            .map_err(|error| Box::new(oauth_store_failure(error)))?;
        oauth
            .with_principals(|s| s.delete(&iss, &sub))
            .map_err(|error| Box::new(store_failure(error)))?;
        Ok::<_, Box<Response>>(revoked)
    }) {
        Ok(Ok(revoked)) => revoked,
        Ok(Err(response)) => return *response,
        Err(crate::workspace::WorkspaceError::AccessDenied) => {
            return conflict("a workspace owner cannot be deleted");
        }
        Err(error) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("workspace registry: {error}"),
            );
        }
    };
    tracing::info!(
        name = %row.name,
        revoked,
        "deleted principal; removed workspace access, pending grants and token families"
    );
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /ui/api/waitlist` — every entry awaiting approval, in the order the
/// store keeps them.
async fn admin_list_waitlist(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let entries = match oauth.with_principals(|s| s.waitlist()) {
        Ok(rows) => rows,
        Err(e) => return store_failure(e),
    };
    let out: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|e| {
            serde_json::json!({
                "id": mcpmem_oauth::principal_id(&e.iss, &e.sub),
                "name": e.name,
                "iss": e.iss,
                "sub": e.sub,
                "firstSeenUs": e.first_seen_us,
                "lastSeenUs": e.last_seen_us,
            })
        })
        .collect();
    (StatusCode::OK, Json(serde_json::json!({ "entries": out }))).into_response()
}

/// `POST /ui/api/waitlist/{id}/approve` — promote one entry to a runtime
/// principal in one transaction. A body without a `scopes` member promotes
/// with the configured default list.
async fn admin_approve_waitlist(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: String,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((iss, sub)) = key_of_id(&id) else {
        return not_found();
    };
    let scopes = match serde_json::from_str::<ApproveBody>(&body) {
        Ok(body) => body
            .scopes
            .unwrap_or_else(|| oauth.config.default_new_principal_scopes.clone()),
        Err(_) => return bad_request("the body must be JSON with an optional scopes list"),
    };
    let scopes = match crate::principals::canonical_scopes(&scopes) {
        Ok(s) if !s.is_empty() => s,
        _ => return bad_request("at least one known scope is required"),
    };
    match oauth.with_principals(|s| s.approve(&iss, &sub, &scopes)) {
        Ok(Some(row)) => (
            StatusCode::CREATED,
            Json(PrincipalView {
                id: mcpmem_oauth::principal_id(&iss, &sub),
                name: row.name,
                iss,
                sub,
                label: None,
                scopes,
                builtin: false,
                masked_by_builtin: false,
            }),
        )
            .into_response(),
        Ok(None) => not_found(),
        Err(e) => store_failure(e),
    }
}

/// `DELETE /ui/api/waitlist/{id}` — discard one entry without promoting it.
async fn admin_dismiss_waitlist(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((iss, sub)) = key_of_id(&id) else {
        return not_found();
    };
    match oauth.with_principals(|s| s.dismiss_waitlist(&iss, &sub)) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => store_failure(e),
    }
}

// ---------------------------------------------------------------------------
// Webhook subscriptions (`/ui/api/webhooks`), behind the `webhooks` feature
// ---------------------------------------------------------------------------

/// The list response. A wrapper struct rather than a hand-built JSON map,
/// so the rows serialize through the stored type's serde spelling — the
/// same `subscriptionId` / `eventOperations` / `consumerOrigin` / `secretRef`
/// keys the SPA echoes back unchanged.
#[cfg(feature = "webhooks")]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WebhookList {
    subscriptions: Vec<WebhookSubscription>,
    /// The names of the signing keys this process can deliver with, sorted.
    /// An empty list is a store-now deployment: a reference is accepted at
    /// registration time and resolved by the delivery process.
    configured_secrets: Vec<String>,
    /// Whether this process runs the delivery worker. The UI warns when a
    /// subscription is stored but nothing can deliver it.
    delivery_role: bool,
}

/// The create body: every stored field except the server-generated id.
#[cfg(feature = "webhooks")]
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebhookInput {
    endpoint: String,
    consumer_origin: String,
    secret_ref: String,
    #[serde(default)]
    event_operations: Vec<ChangeOperation>,
    #[serde(default)]
    entity_types: Vec<String>,
    #[serde(default)]
    ignored_origins: Vec<String>,
    /// Matches the MCP tool: absent means enabled.
    #[serde(default)]
    enabled: Option<bool>,
}

/// The patch body: every field optional; a named field replaces the stored
/// value. An empty list in `eventOperations` / `entityTypes` /
/// `ignoredOrigins` replaces with "deliver every operation / type / origin".
#[cfg(feature = "webhooks")]
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebhookPatch {
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    consumer_origin: Option<String>,
    #[serde(default)]
    secret_ref: Option<String>,
    #[serde(default)]
    event_operations: Option<Vec<ChangeOperation>>,
    #[serde(default)]
    entity_types: Option<Vec<String>>,
    #[serde(default)]
    ignored_origins: Option<Vec<String>>,
    #[serde(default)]
    enabled: Option<bool>,
}

/// Authorize one admin request before opening its workspace subscription
/// store. A non-owner gets the same response as an unknown workspace.
#[cfg(feature = "webhooks")]
fn webhook_workspace_connection(
    state: &HttpState,
    headers: &HeaderMap,
    params: &HashMap<String, String>,
) -> std::result::Result<rusqlite::Connection, Box<Response>> {
    let principal = admin_gate(state, headers)?;
    let record = state
        .registry
        .resolve(
            &principal.id,
            params.get("workspaceId").map(String::as_str),
            WorkspaceAccess::Owner,
        )
        .map_err(|error| Box::new(workspace_failure(&error)))?;
    webhooks_actions::open_connection_at(&record.graph_path)
        .map_err(|error| Box::new(webhook_store_failure(error)))
}

/// Attach the webhook-subscription admin routes. Every handler is gated on
/// the `admin` scope like the principals routes; the routes themselves exist
/// only when the `webhooks` feature compiled them in.
#[cfg(feature = "webhooks")]
fn attach_webhook_admin_routes(router: Router<HttpState>) -> Router<HttpState> {
    router
        .route(
            "/ui/api/webhooks",
            get(admin_list_webhooks).post(admin_create_webhook),
        )
        .route(
            "/ui/api/webhooks/{id}",
            patch(admin_update_webhook).delete(admin_delete_webhook),
        )
        .route("/ui/api/webhooks/{id}/test", post(admin_test_webhook))
}

/// Attach the managed-repo admin routes. Every handler is gated on the
/// `admin` scope like the principals routes; the routes themselves exist
/// only when the `code` feature compiled them in.
#[cfg(feature = "code")]
fn attach_repo_admin_routes(router: Router<HttpState>) -> Router<HttpState> {
    router
        .route(
            "/ui/api/repos",
            get(admin_list_repos).post(admin_create_repo),
        )
        .route("/ui/api/repos/{key}/reindex", post(admin_reindex_repo))
        .route("/ui/api/repos/{key}", delete(admin_remove_repo))
}

/// `POST /ui/api/webhooks/{id}/test` — deliver one signed test event to the
/// subscription's endpoint and report the HTTP status. The delivery-time
/// policy runs in full: allowlist, DNS and the public-address check, then a
/// signature with the subscription's secret reference. No outbox row is
/// written and no delivery state changes, so a test never disturbs the
/// worker's queue or its dead-letter accounting.
#[cfg(feature = "webhooks")]
async fn admin_test_webhook(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let conn = match webhook_workspace_connection(&state, &headers, &params) {
        Ok(conn) => conn,
        Err(response) => return *response,
    };
    let id = match Uuid::parse_str(id.as_str()) {
        Ok(id) => id,
        // A malformed id names no row, the same answer the principals routes
        // give for an id their key format cannot parse.
        Err(_) => return not_found(),
    };
    let subscription = match SubscriptionRepository::new(&conn).get(id) {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(e) => return webhook_store_failure(e),
    };
    let Some(kit) = webhooks_actions::test_kit() else {
        return bad_request(
            "webhook test delivery is not configured: the server has no [webhooks] section",
        );
    };
    let started = std::time::Instant::now();
    // The deliverable makes a real DNS lookup and a real HTTPS request, so it
    // runs off the async runtime, the same way the worker's own blocking
    // transport does.
    let outcome = match tokio::task::spawn_blocking(move || kit.deliver(&subscription)).await {
        Ok(outcome) => outcome,
        Err(e) => return webhook_store_failure(format!("test delivery task failed: {e}")),
    };
    let latency_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    match outcome {
        Ok(response) => {
            let body = json!({
                "status": response.status,
                "ok": (200..300).contains(&response.status),
                "latencyUs": latency_us,
            });
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(WorkerError::Policy(message)) => {
            bad_request(format!("webhook test refused by policy: {message}"))
        }
        Err(WorkerError::Secret(message)) => {
            bad_request(format!("webhook test refused: {message}"))
        }
        Err(WorkerError::Delivery(message)) => json_error(
            StatusCode::BAD_GATEWAY,
            format!("webhook test delivery failed: {message}"),
        ),
        Err(WorkerError::Database(message)) => webhook_store_failure(message),
        Err(WorkerError::Core(message)) => webhook_store_failure(message),
    }
}

/// A 500 whose message names the webhook store, so an operator does not
/// chase the principals store for a subscription failure.
#[cfg(feature = "webhooks")]
fn webhook_store_failure(e: impl std::fmt::Display) -> Response {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("webhook store: {e}"),
    )
}

/// The caps the MCP add tool applies, so the admin API accepts nothing the
/// tool would refuse. `Some(message)` is the 400 body; `None` proceeds.
#[cfg(feature = "webhooks")]
fn webhook_caps_error(subscription: &WebhookSubscription) -> Option<String> {
    let max_origin = webhooks_actions::MAX_CONSUMER_ORIGIN_BYTES;
    if subscription.consumer_origin.len() > max_origin {
        return Some(format!("consumerOrigin too long (max {max_origin} bytes)"));
    }
    let max_items = webhooks_actions::MAX_LIST_ITEMS;
    for (field, count) in [
        ("eventOperations", subscription.event_operations.len()),
        ("entityTypes", subscription.entity_types.len()),
        ("ignoredOrigins", subscription.ignored_origins.len()),
    ] {
        if count > max_items {
            return Some(format!("Too many entries in '{field}' (max {max_items})"));
        }
    }
    None
}

/// `GET /ui/api/webhooks` — subscriptions in the selected workspace, oldest first.
#[cfg(feature = "webhooks")]
async fn admin_list_webhooks(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let conn = match webhook_workspace_connection(&state, &headers, &params) {
        Ok(conn) => conn,
        Err(response) => return *response,
    };
    let subscriptions = match SubscriptionRepository::new(&conn).list() {
        Ok(rows) => rows,
        Err(e) => return webhook_store_failure(e),
    };
    (
        StatusCode::OK,
        Json(WebhookList {
            subscriptions,
            configured_secrets: webhooks_actions::configured_secret_refs(),
            delivery_role: webhooks_actions::delivery_role(),
        }),
    )
        .into_response()
}

/// `POST /ui/api/webhooks` — create one subscription. The rules are the MCP
/// tool's own, imported rather than copied: the endpoint URL shape, the
/// consumer-origin and list caps, and `WebhookSubscription::validate`
/// through the store's upsert. The id is server-generated.
#[cfg(feature = "webhooks")]
async fn admin_create_webhook(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    body: String,
) -> Response {
    let conn = match webhook_workspace_connection(&state, &headers, &params) {
        Ok(conn) => conn,
        Err(response) => return *response,
    };
    let input: WebhookInput = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("the body must be a JSON subscription"),
    };
    let subscription = WebhookSubscription {
        subscription_id: Uuid::new_v4(),
        endpoint: input.endpoint,
        event_operations: input.event_operations,
        entity_types: input.entity_types,
        ignored_origins: input.ignored_origins,
        consumer_origin: input.consumer_origin,
        secret_ref: input.secret_ref,
        enabled: input.enabled.unwrap_or(true),
    };
    match webhooks_actions::validate_endpoint_shape(&subscription.endpoint) {
        Ok(()) => {}
        Err(e) => {
            return bad_request(format!("invalid webhook endpoint: {e}"));
        }
    }
    // The registration-time secret check, the same one the MCP add tool
    // runs: an unknown `secretRef` is refused before the row is written.
    match webhooks_actions::validate_secret_ref(&subscription.secret_ref) {
        Ok(()) => {}
        Err(e) => {
            return bad_request(format!("invalid webhook subscription: {e}"));
        }
    }
    if let Some(message) = webhook_caps_error(&subscription) {
        return bad_request(message);
    }
    if let Err(e) = SubscriptionRepository::new(&conn).upsert(subscription.clone()) {
        // The checks above are not the only validator: upsert runs
        // `WebhookSubscription::validate` (non-empty fields, length and
        // control-character bounds), and its refusal is a bad request, not
        // a server fault.
        if matches!(e, MCSError::InvalidParams(_)) {
            return bad_request(format!("invalid webhook subscription: {e}"));
        }
        return webhook_store_failure(e);
    }
    (StatusCode::CREATED, Json(subscription)).into_response()
}

/// `PATCH /ui/api/webhooks/{id}` — change any subset of one subscription.
/// A named field replaces the stored value; everything else stays.
#[cfg(feature = "webhooks")]
async fn admin_update_webhook(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    body: String,
) -> Response {
    let conn = match webhook_workspace_connection(&state, &headers, &params) {
        Ok(conn) => conn,
        Err(response) => return *response,
    };
    let id = match Uuid::parse_str(id.as_str()) {
        Ok(id) => id,
        // A malformed id names no row, the same answer the principals routes
        // give for an id their key format cannot parse.
        Err(_) => return not_found(),
    };
    let patch: WebhookPatch = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("the body must be a JSON patch"),
    };
    // The patched value is validated, never the stored one: a patch that
    // names no `secretRef` leaves an existing reference alone, whatever the
    // current kit holds.
    if let Some(new_ref) = patch.secret_ref.as_deref() {
        match webhooks_actions::validate_secret_ref(new_ref) {
            Ok(()) => {}
            Err(e) => {
                return bad_request(format!("invalid webhook subscription: {e}"));
            }
        }
    }
    let repo = SubscriptionRepository::new(&conn);
    let row = match repo.get(id) {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(e) => return webhook_store_failure(e),
    };
    let merged = WebhookSubscription {
        subscription_id: id,
        endpoint: patch
            .endpoint
            .as_deref()
            .unwrap_or(&row.endpoint)
            .to_owned(),
        event_operations: patch.event_operations.unwrap_or(row.event_operations),
        entity_types: patch.entity_types.unwrap_or(row.entity_types),
        ignored_origins: patch.ignored_origins.unwrap_or(row.ignored_origins),
        consumer_origin: patch
            .consumer_origin
            .as_deref()
            .unwrap_or(&row.consumer_origin)
            .to_owned(),
        secret_ref: patch
            .secret_ref
            .as_deref()
            .unwrap_or(&row.secret_ref)
            .to_owned(),
        enabled: patch.enabled.unwrap_or(row.enabled),
    };
    match webhooks_actions::validate_endpoint_shape(&merged.endpoint) {
        Ok(()) => {}
        Err(e) => {
            return bad_request(format!("invalid webhook endpoint: {e}"));
        }
    }
    if let Some(message) = webhook_caps_error(&merged) {
        return bad_request(message);
    }
    if let Err(e) = repo.upsert(merged.clone()) {
        if matches!(e, MCSError::InvalidParams(_)) {
            return bad_request(format!("invalid webhook subscription: {e}"));
        }
        return webhook_store_failure(e);
    }
    (StatusCode::OK, Json(merged)).into_response()
}

/// `DELETE /ui/api/webhooks/{id}` — delete one subscription. Deleting an id
/// that names no row is a 404, the same answer the principals routes give;
/// the MCP tool's lenient delete is a separate contract.
#[cfg(feature = "webhooks")]
async fn admin_delete_webhook(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let conn = match webhook_workspace_connection(&state, &headers, &params) {
        Ok(conn) => conn,
        Err(response) => return *response,
    };
    let id = match Uuid::parse_str(id.as_str()) {
        Ok(id) => id,
        Err(_) => return not_found(),
    };
    match SubscriptionRepository::new(&conn).delete(id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => webhook_store_failure(e),
    }
}

// ---------------------------------------------------------------------------
// Managed repositories (`/ui/api/repos`), behind the `code` feature
// ---------------------------------------------------------------------------

/// `GET /ui/api/repos` — every managed repository with its live state.
/// Mutations answer 202 and run the job on a detached thread; the next poll
/// of the list shows the transition.
#[cfg(feature = "code")]
async fn admin_list_repos(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    match crate::repos::list() {
        Ok(rows) => (StatusCode::OK, Json(serde_json::json!({ "repos": rows }))).into_response(),
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("repos store: {e}"),
        ),
    }
}

/// `POST /ui/api/repos` — register one repository and schedule its clone +
/// index job. The 202 answers before the job finishes; row state moves
/// `pending` → `cloning` → `indexing` → `indexed` (or `error`) and the list
/// shows it.
#[cfg(feature = "code")]
async fn admin_create_repo(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let input: crate::repos::RepoInput = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            return bad_request(
                "the body must be a JSON repo: {key, url, authKind?, authSecret?, snippets?}",
            );
        }
    };
    if let Err(e) = crate::repos::register(&input) {
        return match e {
            MCSError::ConstraintViolation(message) => conflict(message),
            MCSError::InvalidParams(message) => bad_request(message),
            other => json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("repos store: {other}"),
            ),
        };
    }
    let key = input.key;
    if let Err(e) = crate::repos::add_job(&key) {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("repos store: {e}"),
        );
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "accepted", "key": key })),
    )
        .into_response()
}

/// `POST /ui/api/repos/{key}/reindex` — schedule a fetch + reindex job for
/// one repository. An unknown key is a 404; a job already in flight for the
/// key conflicts.
#[cfg(feature = "code")]
async fn admin_reindex_repo(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    match crate::repos::get_row(&key) {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("repos store: {e}"),
            );
        }
    }
    match crate::repos::reindex_job(&key) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "accepted", "key": key })),
        )
            .into_response(),
        Err(MCSError::InvalidParams(message)) => json_error(StatusCode::CONFLICT, message),
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("repos store: {e}"),
        ),
    }
}

/// `DELETE /ui/api/repos/{key}` — schedule the removal of one repository:
/// its index DB, its worktree and its row. An unknown key is a 404.
#[cfg(feature = "code")]
async fn admin_remove_repo(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    match crate::repos::get_row(&key) {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("repos store: {e}"),
            );
        }
    }
    match crate::repos::remove_job(&key) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "accepted", "key": key })),
        )
            .into_response(),
        Err(MCSError::InvalidParams(message)) => json_error(StatusCode::CONFLICT, message),
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("repos store: {e}"),
        ),
    }
}

/// `GET /ui — serve the browser graph viewer's HTML shell. The shell and its
/// `/ui/graph.css` + `/ui/graph.js` assets hold no graph data, so they are served
/// without auth; the data they fetch (`/ui/graph`, `/ui/expand`) is what carries
/// the auth + permission gate.
async fn ui_handler() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        UI_INDEX_HTML,
    )
        .into_response()
}

/// `GET /ui/graph.css` — the viewer stylesheet (static asset, no auth).
async fn ui_css_handler() -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], UI_CSS).into_response()
}

/// `GET /ui/nav.css` — the shared site-navigation stylesheet (static asset,
/// no auth).
async fn ui_nav_css_handler() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        UI_NAV_CSS,
    )
        .into_response()
}

/// `GET /ui/graph.js` — the viewer application script (static asset, no auth).
async fn ui_js_handler() -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        UI_JS,
    )
        .into_response()
}

/// `GET /ui/admin` — serve the admin SPA's HTML shell. The callback URL is
/// the same shell: after the provider redirects back with `?code=`, the
/// script in the page completes the PKCE exchange. The shell and its assets
/// hold no data, so they are served without auth; the JSON API behind them is
/// the gate.
async fn admin_page_handler() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        ADMIN_INDEX_HTML,
    )
        .into_response()
}

/// `GET /ui/admin.css` — the admin stylesheet (static asset, no auth).
async fn admin_css_handler() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        ADMIN_CSS,
    )
        .into_response()
}

/// `GET /ui/admin.js` — the admin application script (static asset, no auth).
async fn admin_js_handler() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        ADMIN_JS,
    )
        .into_response()
}

/// Shared auth + scope gate for the viewer's workspace list and data routes.
/// Return the authenticated principal only after the category and the
/// `read_graph` scope checks pass.
///
/// The scope decision is [`crate::authz::allows_tool`]'s and is asked about
/// `read_graph` by name, never spelled here. `src/authz.rs` is the one place
/// that decision is made, and asking it about the tool the viewer stands for
/// is what keeps the two provably in step: a tool moved to another scope
/// moves the viewer with it.
///
/// The 401 is [`unauthorized`] and the scope refusal is [`insufficient_scope`],
/// the same two challenges `/mcp` sends: one server answers with one shape, so
/// a scripted viewer client can discover the authorization server from the 401
/// and learn the scope to ask for from the 403.
fn ui_data_gate(
    state: &HttpState,
    headers: &HeaderMap,
    params: &HashMap<String, String>,
) -> std::result::Result<Principal, Box<Response>> {
    let Some(principal) = principal_of_ui(state, headers, params.get("token").map(String::as_str))
    else {
        return Err(Box::new(unauthorized(state)));
    };
    if !server::graph_read_enabled() {
        return Err(Box::new(
            (
                StatusCode::FORBIDDEN,
                "graph-read tools are disabled; start the server with --enable-graph-read (or --enable-all) to view the graph",
            )
                .into_response(),
        ));
    }
    if !crate::authz::allows_tool(&principal, "read_graph") {
        // The scope the challenge names is the one `authz` reports missing, so
        // the header cannot drift from the decision that produced it.
        let missing = crate::authz::missing_scope(&principal, "read_graph")
            .unwrap_or(ToolCategory::GraphRead.slug());
        return Err(Box::new(insufficient_scope(state, &[missing])));
    }
    Ok(principal)
}

/// `GET /ui/workspaces` — the MCP workspace-list page for this caller.
async fn ui_workspaces_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let principal = match ui_data_gate(&state, &headers, &params) {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let limit = match params.get("limit") {
        None => 100,
        Some(raw) => match raw.parse::<usize>() {
            Ok(limit) if limit > 0 => limit,
            _ => return bad_request("'limit' must be a positive integer"),
        },
    };
    let cursor = params.get("cursor").cloned();
    let registry = state.registry;
    match tokio::task::spawn_blocking(move || {
        registry.list(&principal.id, cursor.as_deref(), limit)
    })
    .await
    {
        Ok(Ok(page)) => Json(page).into_response(),
        Ok(Err(error)) => workspace_failure(&error),
        Err(error) => {
            error!("/ui/workspaces task panicked: {error}");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// A viewer data request resolves the same registry record and handle as MCP.
/// Resolve inside the payload task so registry I/O never blocks the reactor.
struct UiSelection {
    registry: Arc<crate::workspace::WorkspaceRegistry>,
    handles: Arc<crate::workspace::WorkspaceHandles>,
    principal_id: String,
    workspace_id: Option<String>,
}

impl UiSelection {
    fn new(state: HttpState, principal: Principal, params: &HashMap<String, String>) -> Self {
        Self {
            registry: state.registry,
            handles: state.handles,
            principal_id: principal.id,
            workspace_id: params.get("workspaceId").cloned(),
        }
    }

    fn graph(self) -> std::result::Result<Arc<GraphHandle>, WorkspaceError> {
        let record = self.registry.resolve(
            &self.principal_id,
            self.workspace_id.as_deref(),
            WorkspaceAccess::Read,
        )?;
        Ok(self.handles.get(&record)?.kg)
    }
}

fn parse_usize(params: &HashMap<String, String>, key: &str, default: usize) -> usize {
    params
        .get(key)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Run workspace selection and the graph payload in one blocking task.
async fn ui_json<F>(selection: UiSelection, what: &'static str, build: F) -> Response
where
    F: FnOnce(&GraphHandle) -> Result<String> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || {
        let kg = selection.graph()?;
        Ok::<_, WorkspaceError>(build(&kg))
    })
    .await
    {
        Ok(Ok(Ok(json))) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Ok(Err(error)) => workspace_failure(&error),
        Ok(Ok(Err(error))) => {
            error!("{what} error: {error}");
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
        Err(join_err) => {
            error!("{what} task panicked: {join_err}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// `GET /ui/graph` — a page of the whole graph for the viewer: entities, the
/// relations among them, the entity-type legend, overall stats, and a pagination
/// cursor. Requires the `graph-read` category and a readable workspace.
/// Query params: `entityType` (filter), `offset`, `limit` (capped at
/// [`MAX_UI_NODES`]), `workspaceId` and `token` (auth fallback).
async fn ui_graph_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let principal = match ui_data_gate(&state, &headers, &params) {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let entity_type = params.get("entityType").filter(|s| !s.is_empty()).cloned();
    let offset = parse_usize(&params, "offset", 0);
    let limit = parse_usize(&params, "limit", 300).clamp(1, MAX_UI_NODES);
    let selection = UiSelection::new(state, principal, &params);
    ui_json(selection, "/ui/graph", move |kg| {
        build_graph_payload(kg, entity_type.as_deref(), offset, limit)
    })
    .await
}

/// `GET /ui/search` — a page of FTS5 matches for the viewer's search box, in the
/// same `{entities, relations, entityTypes, stats, page}` shape as `/ui/graph`
/// (the matched nodes; the user double-clicks to expand their relationships).
/// Same auth + `graph-read` gate. Query params: `q` (the query; prefix-matched),
/// `entityType` (filter), `offset`, `limit` (capped at [`MAX_UI_NODES`]),
/// `workspaceId` and `token`.
async fn ui_search_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let principal = match ui_data_gate(&state, &headers, &params) {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let query = params
        .get("q")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let entity_type = params.get("entityType").filter(|s| !s.is_empty()).cloned();
    let offset = parse_usize(&params, "offset", 0);
    let limit = parse_usize(&params, "limit", 100).clamp(1, MAX_UI_NODES);
    let selection = UiSelection::new(state, principal, &params);
    ui_json(selection, "/ui/search", move |kg| {
        build_search_payload(kg, &query, entity_type.as_deref(), offset, limit)
    })
    .await
}

/// `GET /ui/node` — one entity with its observation bodies, for the inspector to
/// lazy-load when a node is selected. The list endpoints (`/ui/graph`,
/// `/ui/search`) deliberately omit observation bodies (they only carry
/// `obsCount`) to keep those payloads small; this fetches the bodies for the
/// single node the user is looking at. Same auth + `graph-read` gate. Query
/// params: `name` (required), `workspaceId` and `token` (auth fallback).
async fn ui_node_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let principal = match ui_data_gate(&state, &headers, &params) {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let Some(name) = params.get("name").filter(|s| !s.is_empty()).cloned() else {
        return (StatusCode::BAD_REQUEST, "missing 'name' parameter").into_response();
    };
    let selection = UiSelection::new(state, principal, &params);
    match tokio::task::spawn_blocking(move || {
        let kg = selection.graph()?;
        Ok::<_, WorkspaceError>(kg.get_entity(&name))
    })
    .await
    {
        Ok(Err(error)) => workspace_failure(&error),
        Ok(Ok(Ok(Some(entity)))) => match serde_json::to_string(&entity) {
            Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
            Err(e) => {
                error!("/ui/node serialize error: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        },
        Ok(Ok(Ok(None))) => (StatusCode::NOT_FOUND, "entity not found").into_response(),
        Ok(Ok(Err(e))) => {
            error!("/ui/node error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(join_err) => {
            error!("/ui/node task panicked: {join_err}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// The viewer's shared page metadata, spliced onto every list payload: the
/// entity-type legend, the graph-wide totals, and the pagination cursor that
/// drives Prev/Next without a second round-trip.
struct PageMeta<'a> {
    type_counts: &'a [(String, usize)],
    entities_total: usize,
    relations_total: usize,
    offset: usize,
    limit: usize,
    returned: usize,
    has_more: bool,
}

/// Splice [`PageMeta`] onto a `{entities,…,relations,…}` JSON object *in place,
/// without reparsing it*. `graph` is a complete object built by us, so it ends in
/// `}`; we pop that brace, append the extra keys, and re-close. This replaces a
/// full `serde_json::from_str` → mutate → `to_string` round-trip over what is
/// often the largest payload the server emits.
fn splice_meta(graph: &mut String, m: &PageMeta) {
    use std::fmt::Write as _;
    debug_assert!(graph.ends_with('}'));
    graph.pop();
    graph.push_str(",\"entityTypes\":[");
    for (i, (t, c)) in m.type_counts.iter().enumerate() {
        if i > 0 {
            graph.push(',');
        }
        graph.push_str("{\"type\":");
        crate::kg::push_json_str(graph, t);
        let _ = write!(graph, ",\"count\":{c}}}");
    }
    let _ = write!(
        graph,
        "],\"stats\":{{\"entities\":{e},\"relations\":{r}}},\
         \"page\":{{\"offset\":{o},\"limit\":{l},\"returned\":{ret},\"hasMore\":{hm}}}}}",
        e = m.entities_total,
        r = m.relations_total,
        o = m.offset,
        l = m.limit,
        ret = m.returned,
        hm = m.has_more,
    );
}

/// Assemble the `/ui/graph` JSON: an observation-free page of the graph from
/// [`GraphHandle::read_graph_filtered_lite`] plus the shared viewer metadata
/// (gathered in one reader acquisition via [`GraphHandle::ui_meta`]). `hasMore`
/// compares this page against the scope total (the filtered type's count, or the
/// whole-graph entity count) so the viewer can enable Next.
fn build_graph_payload(
    kg: &GraphHandle,
    entity_type: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<String> {
    let (mut graph, returned) = kg.read_graph_filtered_lite(entity_type, offset, limit)?;
    let (type_counts, entities_total, relations_total) = kg.ui_meta();
    let scope_total = match entity_type {
        Some(t) if !t.is_empty() => type_counts
            .iter()
            .find(|(n, _)| n == t)
            .map_or(0, |(_, c)| *c),
        _ => entities_total,
    };
    let has_more = offset.saturating_add(returned) < scope_total;

    splice_meta(
        &mut graph,
        &PageMeta {
            type_counts: &type_counts,
            entities_total,
            relations_total,
            offset,
            limit,
            returned,
            has_more,
        },
    );
    Ok(graph)
}

/// Turn a free-text search box query into a safe FTS5 MATCH expression: keep
/// alphanumeric/underscore tokens (dropping punctuation that would otherwise be
/// FTS operators and silently fail the query), AND them together, and make the
/// final token a prefix (`term*`) for a natural search-as-you-type feel.
fn fts_query(raw: &str) -> String {
    let tokens: Vec<String> = raw
        .split_whitespace()
        .map(|t| {
            t.chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
        })
        .filter(|t| !t.is_empty())
        .collect();
    let n = tokens.len();
    tokens
        .into_iter()
        .enumerate()
        .map(|(i, t)| if i + 1 == n { format!("{t}*") } else { t })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Assemble the `/ui/search` JSON: an observation-free page of FTS5 matches from
/// [`GraphHandle::search_nodes_lite_json`] as `{entities, relations: []}` (the
/// matched nodes; the user double-clicks to expand their relationships) plus the
/// shared viewer metadata. `hasMore` is detected server-side by fetching one
/// extra match past the page.
fn build_search_payload(
    kg: &GraphHandle,
    query: &str,
    entity_type: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<String> {
    let fts = fts_query(query);
    let (entities_json, returned, has_more) =
        kg.search_nodes_lite_json(&fts, entity_type, offset, limit);

    let mut graph = String::with_capacity(entities_json.len() + 48);
    graph.push_str("{\"entities\":");
    graph.push_str(&entities_json);
    graph.push_str(",\"relations\":[]}");

    let (type_counts, entities_total, relations_total) = kg.ui_meta();
    splice_meta(
        &mut graph,
        &PageMeta {
            type_counts: &type_counts,
            entities_total,
            relations_total,
            offset,
            limit,
            returned,
            has_more,
        },
    );
    Ok(graph)
}

/// `GET /ui/expand` — the neighbourhood of one entity, for the viewer's
/// double-click-to-expand traversal. Returns `{entities, relations}` (the same
/// shape as `/ui/graph`) from [`GraphHandle::neighbors`], which the viewer merges
/// into the current graph. Same auth + `graph-read` gate as `/ui/graph`.
///
/// Query params: `name` (required, the entity to expand), `depth` (1..=
/// [`MAX_UI_EXPAND_DEPTH`], default 1), `direction` (`outgoing` / `incoming` /
/// `both`, default both), `workspaceId` and `token` (auth fallback).
async fn ui_expand_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let principal = match ui_data_gate(&state, &headers, &params) {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let Some(name) = params.get("name").filter(|s| !s.is_empty()).cloned() else {
        return (StatusCode::BAD_REQUEST, "missing 'name' parameter").into_response();
    };
    let depth = params
        .get("depth")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1)
        .clamp(1, MAX_UI_EXPAND_DEPTH);
    // `Direction::parse` expects the uppercase MCP spelling; default is `Both`.
    let direction =
        crate::kg::Direction::parse(params.get("direction").map(|s| s.to_uppercase()).as_deref());

    let selection = UiSelection::new(state, principal, &params);
    let result = tokio::task::spawn_blocking(move || {
        let kg = selection.graph()?;
        Ok::<_, WorkspaceError>(kg.neighbors(&name, direction, None, depth))
    })
    .await;

    match result {
        Ok(Ok(Ok(json))) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Ok(Err(error)) => workspace_failure(&error),
        // An unknown entity is a client error (bad `name`), not a server fault.
        Ok(Ok(Err(MCSError::InvalidParams(msg)))) => (StatusCode::NOT_FOUND, msg).into_response(),
        Ok(Ok(Err(e))) => {
            error!("/ui/expand error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(join_err) => {
            error!("/ui/expand task panicked: {join_err}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

fn attachment_limits(settings: crate::config_file::AttachmentsSection) -> Arc<AttachmentLimits> {
    Arc::new(AttachmentLimits {
        max_bytes: settings.max_bytes,
        workspace_byte_budget: settings.workspace_byte_budget,
        allow_mime: settings.allow_mime,
    })
}

/// What the blocking gate finished with, spelled out so the match in
/// [`attachment_path`] never has to guess at nested result types.
enum AttachmentPath {
    Path(PathBuf),
    Unauthorized,
    MissingScope(&'static str),
    Workspace(WorkspaceError),
}

/// The category, credential, scope, and workspace grant are independent gates.
/// Finish all four before opening a temporary file or reading an upload body.
async fn attachment_path(
    state: &HttpState,
    headers: &HeaderMap,
    params: &HashMap<String, String>,
    tool: &'static str,
    access: WorkspaceAccess,
) -> std::result::Result<PathBuf, Box<Response>> {
    if !server::attachments_enabled() {
        return Err(Box::new(json_error(
            StatusCode::FORBIDDEN,
            "attachments tools are disabled",
        )));
    }
    let auth = state.clone();
    let headers = headers.clone();
    let registry = Arc::clone(&state.registry);
    let requested = params.get("workspaceId").cloned();
    let outcome = tokio::task::spawn_blocking(move || {
        let Some(principal) = principal_of(&auth, &headers) else {
            return AttachmentPath::Unauthorized;
        };
        if let Some(missing) = crate::authz::missing_scope(&principal, tool) {
            return AttachmentPath::MissingScope(missing);
        }
        match registry.resolve(&principal.id, requested.as_deref(), access) {
            Ok(record) => AttachmentPath::Path(record.graph_path),
            Err(error) => AttachmentPath::Workspace(error),
        }
    })
    .await;
    match outcome {
        Ok(AttachmentPath::Path(path)) => Ok(path),
        Ok(AttachmentPath::Unauthorized) => Err(Box::new(unauthorized(state))),
        Ok(AttachmentPath::MissingScope(scope)) => {
            Err(Box::new(insufficient_scope(state, &[scope])))
        }
        Ok(AttachmentPath::Workspace(error)) => Err(Box::new(workspace_failure(&error))),
        Err(error) => {
            error!("attachment access task panicked: {error}");
            Err(Box::new(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            )))
        }
    }
}

fn attachment_db_error(error: &rusqlite::Error) -> Box<Response> {
    error!("attachment graph error: {error}");
    Box::new(json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "attachment storage error",
    ))
}

fn attachment_bootstrap_error(error: &mcpmem_core::errors::MCSError) -> Box<Response> {
    error!("attachment graph bootstrap error: {error}");
    Box::new(json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "attachment storage error",
    ))
}

fn attachment_failure(error: AttachmentError) -> Box<Response> {
    let status = match error {
        AttachmentError::DuplicateFilename => StatusCode::CONFLICT,
        AttachmentError::Mime => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        AttachmentError::Size | AttachmentError::WorkspaceBudget => StatusCode::PAYLOAD_TOO_LARGE,
        AttachmentError::NotFound | AttachmentError::WrongPrincipal => StatusCode::NOT_FOUND,
        AttachmentError::Storage(source) => {
            error!("attachment storage error: {source}");
            return Box::new(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "attachment storage error",
            ));
        }
        _ => StatusCode::BAD_REQUEST,
    };
    Box::new(json_error(status, error.to_string()))
}

fn attachment_id(raw: &str) -> std::result::Result<i64, Box<Response>> {
    raw.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| Box::new(bad_request("attachment id must be a positive integer")))
}

fn attachment_query_number(
    params: &HashMap<String, String>,
    name: &str,
    default: Option<i64>,
    min: i64,
) -> std::result::Result<i64, Box<Response>> {
    let number = match params.get(name) {
        Some(raw) => raw.parse::<i64>().ok().filter(|value| *value >= min),
        None => default.filter(|value| *value >= min),
    };
    number.ok_or_else(|| Box::new(bad_request(format!("'{name}' must be an integer >= {min}"))))
}

async fn attachment_result<T, F>(
    path: PathBuf,
    operation: F,
) -> std::result::Result<T, Box<Response>>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> std::result::Result<T, Box<Response>> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || {
        let conn = Connection::open(path).map_err(|error| attachment_db_error(&error))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| attachment_db_error(&error))?;
        // The registry migrates only the legacy graph at startup; every other
        // graph initializes lazily through the WorkspaceHandles get path,
        // which these routes bypass by opening the file directly. Bootstrap
        // the resolved graph here so a first request against a pre-upgrade
        // workspace does not fail with `no such table: attachment`. The
        // existence check keeps the common already-migrated path to one read
        // with no write transaction.
        let migrated: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='attachment')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| attachment_db_error(&error))?;
        if !migrated {
            mcpmem_core::schema::initialize_database(&conn)
                .map_err(|error| attachment_bootstrap_error(&error))?;
        }
        operation(&conn)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => {
            error!("attachment graph task panicked: {error}");
            Err(Box::new(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::server::MCPServer;
    use crate::tools::ToolCategory;

    const UI_TEST_BEARER: &str = "unit-ui-token";

    fn ui_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {UI_TEST_BEARER}").parse().unwrap(),
        );
        headers
    }

    /// A viewer state with a static bearer and the given credential scopes.
    /// Grant its legacy workspace to that bearer and set a saved default.
    /// The scope gate remains independent of graph access.
    fn ui_state(dir: &tempfile::TempDir, scopes: &[ToolCategory]) -> HttpState {
        let config = Config {
            memory_file_path: dir.path().join("memory.db").to_string_lossy().into_owned(),
            legacy_owner_id: Some("machine:local".into()),
            auth_token: Some(Arc::from(UI_TEST_BEARER)),
            enabled_categories: vec![ToolCategory::GraphRead, ToolCategory::GraphWrite],
            ..Config::default()
        };
        let server = MCPServer::new_kg(config).expect("test server builds");
        let registry = server.workspace_registry();
        let handles = server.workspace_handles();
        let legacy_id = registry
            .all_paths()
            .expect("the registry lists its legacy workspace")
            .into_iter()
            .next()
            .expect("the legacy workspace exists")
            .0;
        registry
            .grant("machine:local", &legacy_id, "machine:static", "reader")
            .expect("the bearer can read its test workspace");
        registry
            .set_default("machine:static", &legacy_id)
            .expect("the bearer has a saved default");
        HttpState {
            registry,
            handles,
            auth_token: Some(Arc::from(UI_TEST_BEARER)),
            bearer_scopes: Arc::from(scopes),
            enabled_categories: Arc::from(&[ToolCategory::GraphRead, ToolCategory::GraphWrite][..]),
            oauth: None,
            attachment_limits: attachment_limits(Config::default().attachments),
            attachment_spools: Arc::new(tokio::sync::Semaphore::new(
                MAX_CONCURRENT_ATTACHMENT_SPOOLS,
            )),
        }
    }

    /// The viewer reads the whole graph, so it needs the same `graph-read`
    /// scope as `read_graph`. Without it the endpoint must refuse, even though
    /// the category is enabled and the token is accepted.
    #[tokio::test]
    async fn ui_graph_refuses_a_credential_without_the_graph_read_scope() {
        let dir = tempfile::tempdir().unwrap();
        let state = ui_state(&dir, &[ToolCategory::Vectors]);
        let resp = ui_graph_handler(State(state), ui_headers(), Query(HashMap::new())).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// Control: the same request with the scope is served.
    #[tokio::test]
    async fn ui_graph_serves_a_credential_holding_the_graph_read_scope() {
        let dir = tempfile::tempdir().unwrap();
        let state = ui_state(&dir, &[ToolCategory::GraphRead]);
        let resp = ui_graph_handler(State(state), ui_headers(), Query(HashMap::new())).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Every response leaves the router with `Server: mcpmem <version>`, so
    /// an operator can identify a running build without opening a session.
    #[tokio::test]
    async fn every_response_carries_the_server_version_header() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        let state = ui_state(&dir, &[ToolCategory::GraphRead]);
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/ui/graph")
                    .header(header::AUTHORIZATION, "Bearer unit-ui-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let server = response
            .headers()
            .get(header::SERVER)
            .expect("every response carries a Server header")
            .to_str()
            .unwrap();
        assert_eq!(
            server,
            concat!("mcpmem ", env!("CARGO_PKG_VERSION")),
            "the Server header must name the binary and the version"
        );
        // A 404 fallback is produced inside the router, below the header
        // layer — it must carry the header too.
        let not_found = router(ui_state(&dir, &[ToolCategory::GraphRead]))
            .oneshot(
                Request::builder()
                    .uri("/no-such-route")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
        assert!(not_found.headers().contains_key(header::SERVER));
        // Drain both bodies so the responses are fully read.
        let _ = response.into_body().collect().await.unwrap();
        let _ = not_found.into_body().collect().await.unwrap();
    }

    /// Admin-route tests for the webhook subscription API, behind the same
    /// feature gate as the routes themselves.
    #[cfg(feature = "webhooks")]
    mod webhook_admin_tests {
        use super::*;
        use crate::principals::ADMIN_SCOPE;
        use http_body_util::BodyExt;
        use mcpmem_oauth::store::{Grant, TokenKind};
        use tower::ServiceExt;

        const NOW_US: i64 = 1_700_000_000_000_000;

        fn oauth_config() -> crate::config::OAuthConfig {
            crate::config::OAuthConfig {
                public_url: "https://mem.example.com".into(),
                oidc_issuer: "https://idp.invalid".into(),
                oidc_client_id: "mcpmem-test".into(),
                oidc_client_secret: None,
                principals: vec![crate::principals::PrincipalEntry {
                    name: "admin".into(),
                    iss: "https://idp.invalid".into(),
                    sub: "admin@example.test".into(),
                    label: None,
                    scopes: vec![ADMIN_SCOPE.to_owned()],
                }],
                cimd_allowed_domains: Vec::new(),
                trust_forwarded_proto: false,
                approval_waitlist: false,
                approval_waitlist_ttl_seconds: 24 * 60 * 60,
                default_new_principal_scopes: vec!["graph-read".to_owned()],
            }
        }

        /// A kit holding exactly the named signing keys. The names double as
        /// the key bytes: a test key's content never matters, only its
        /// presence in the map does. The resolver and connector are never
        /// reached — these tests judge registration-time checks, not
        /// delivery.
        fn kit_with(secret_names: &[&str]) -> Arc<webhooks_actions::WebhookTestKit> {
            let secrets = secret_names
                .iter()
                .map(|name| {
                    (
                        (*name).to_owned(),
                        mcpmem_webhook::SigningKey::new(name.as_bytes().to_vec())
                            .expect("a non-empty test key"),
                    )
                })
                .collect();
            Arc::new(webhooks_actions::WebhookTestKit::for_test(
                std::collections::BTreeSet::new(),
                secrets,
                Arc::new(mcpmem_webhook::SystemResolver),
                Arc::new(mcpmem_webhook::HttpsConnector::production()),
                false,
                mcpmem_webhook::DEFAULT_MAX_BODY,
            ))
        }

        /// A state with OAuth on and an admin token. The admin owns a graph
        /// and has a saved default. Keep its database alive for each request.
        fn admin_state() -> (HttpState, String, tempfile::TempDir) {
            let dir = tempfile::tempdir().unwrap();
            let state = HttpState::for_test(TestSetup {
                db_path: dir.path().join("memory.db"),
                oauth: Some(oauth_config()),
                auth_token: None,
                metadata_fetch: None,
                bearer_scopes: Vec::new(),
                enabled_categories: ToolCategory::ALL.to_vec(),
                now_us: Some(Arc::new(|| NOW_US)),
            });
            let principal_id =
                crate::principals::human_id("https://idp.invalid", "admin@example.test");
            state
                .registry
                .create(
                    &principal_id,
                    "admin-fixture",
                    crate::workspace::Visibility::Private,
                    |path| state.handles.initialize_graph(path),
                )
                .expect("the admin owns a workspace with a saved default");
            let oauth = state.oauth().expect("oauth is on");
            let token = "admin-test-token";
            oauth.with_store(|store| {
                store
                    .put_token(
                        token,
                        TokenKind::Access,
                        &Grant {
                            client_id: "mcpmem-test".into(),
                            principal: crate::principals::human_id(
                                "https://idp.invalid",
                                "admin@example.test",
                            ),
                            scopes: vec![ADMIN_SCOPE.to_owned()],
                            resource: oauth.resource(),
                            family: "admin-test-family".into(),
                        },
                        NOW_US,
                        NOW_US + 3_600_000_000,
                    )
                    .expect("the admin token stores")
            });
            (state, token.to_owned(), dir)
        }

        /// Drive one admin-api request through the router and return the
        /// status plus the parsed JSON body.
        async fn api_request(
            state: &HttpState,
            method: &str,
            uri: &str,
            body: &str,
            token: &str,
        ) -> (StatusCode, serde_json::Value) {
            let request = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(axum::body::Body::from(body.to_owned()))
                .unwrap();
            let response = router(state.clone()).oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            (status, value)
        }

        #[tokio::test]
        async fn the_list_names_the_configured_secrets_sorted() {
            let (state, token, _dir) = admin_state();
            let response =
                webhooks_actions::with_test_kit_async(Some(kit_with(&["stripe", "n8n"])), async {
                    api_request(&state, "GET", "/ui/api/webhooks", "", &token).await
                })
                .await;
            assert_eq!(response.0, StatusCode::OK);
            assert_eq!(
                response.1.get("configuredSecrets"),
                Some(&serde_json::json!(["n8n", "stripe"])),
                "the list must name the configured signing keys, sorted"
            );
        }

        /// A server with no kit answers an empty list: the store-now shape
        /// the SPA uses to offer a free-text reference.
        #[tokio::test]
        async fn the_list_answers_an_empty_secret_list_without_a_kit() {
            let (state, token, _dir) = admin_state();
            let response = webhooks_actions::with_test_kit_async(None, async {
                api_request(&state, "GET", "/ui/api/webhooks", "", &token).await
            })
            .await;
            assert_eq!(response.0, StatusCode::OK);
            assert_eq!(
                response.1.get("configuredSecrets"),
                Some(&serde_json::json!([]))
            );
        }

        #[tokio::test]
        async fn create_refuses_an_unknown_secret_name() {
            let (state, token, _dir) = admin_state();
            let response = webhooks_actions::with_test_kit_async(
                Some(kit_with(&["n8n"])),
                async {
                    api_request(
                        &state,
                        "POST",
                        "/ui/api/webhooks",
                        r#"{"endpoint":"https://hooks.example.test/receive","consumerOrigin":"https://example.test","secretRef":"stripe"}"#,
                        &token,
                    )
                    .await
                },
            )
            .await;
            assert_eq!(response.0, StatusCode::BAD_REQUEST);
            let message = response.1["error"]
                .as_str()
                .expect("a refusal carries a JSON error body");
            assert!(
                message.contains("secretRef 'stripe' is not configured"),
                "{message}"
            );
            assert!(message.contains("n8n"), "{message}");
        }

        #[tokio::test]
        async fn create_accepts_a_configured_secret_name() {
            let (state, token, _dir) = admin_state();
            let response = webhooks_actions::with_test_kit_async(
                Some(kit_with(&["n8n"])),
                async {
                    api_request(
                        &state,
                        "POST",
                        "/ui/api/webhooks",
                        r#"{"endpoint":"https://hooks.example.test/receive","consumerOrigin":"https://example.test","secretRef":"n8n"}"#,
                        &token,
                    )
                    .await
                },
            )
            .await;
            assert_eq!(response.0, StatusCode::CREATED);
            assert_eq!(response.1["secretRef"], "n8n");
        }

        /// The patch validator reads the patched value, never the stored one:
        /// a new unknown name is refused, and a patch that names no secret is
        /// accepted whatever the current kit holds.
        #[tokio::test]
        async fn update_validates_only_a_new_secret_name() {
            let (state, token, _dir) = admin_state();
            let created = webhooks_actions::with_test_kit_async(
                Some(kit_with(&["n8n"])),
                async {
                    api_request(
                        &state,
                        "POST",
                        "/ui/api/webhooks",
                        r#"{"endpoint":"https://hooks.example.test/receive","consumerOrigin":"https://example.test","secretRef":"n8n"}"#,
                        &token,
                    )
                    .await
                },
            )
            .await;
            assert_eq!(created.0, StatusCode::CREATED);
            let id = created.1["subscriptionId"]
                .as_str()
                .expect("the create echoes the id")
                .to_owned();

            let refused = webhooks_actions::with_test_kit_async(Some(kit_with(&["n8n"])), async {
                api_request(
                    &state,
                    "PATCH",
                    &format!("/ui/api/webhooks/{id}"),
                    r#"{"secretRef":"stripe"}"#,
                    &token,
                )
                .await
            })
            .await;
            assert_eq!(
                refused.0,
                StatusCode::BAD_REQUEST,
                "a patch naming an unknown secret must be refused"
            );

            // The new kit does not hold the stored name: a patch that does
            // not name a secret must pass anyway, proving the stored value
            // is not re-checked.
            let kept = webhooks_actions::with_test_kit_async(Some(kit_with(&["github"])), async {
                api_request(
                    &state,
                    "PATCH",
                    &format!("/ui/api/webhooks/{id}"),
                    r#"{"consumerOrigin":"https://other.example.test"}"#,
                    &token,
                )
                .await
            })
            .await;
            assert_eq!(kept.0, StatusCode::OK);
            assert_eq!(kept.1["secretRef"], "n8n");
        }
    }
}
