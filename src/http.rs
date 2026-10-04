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
//! The optional browser UI lives in [`crate::ui`], a feature-gated module.
//! [`router`] attaches it only when the `ui` feature is compiled: the module
//! then reads the resolved runtime switch itself and registers the pages,
//! the embedded assets, and the `/ui/api/*` JSON adapters, or leaves every
//! `/ui/*` path to the router's ordinary 404.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use mcpmem_core::attachments::AttachmentLimits;
use serde_json::json;
use tokio::net::TcpListener;
use tower_http::compression::CompressionLayer;
use tracing::{error, info};

use crate::authz::Principal;
use crate::errors::{MCSError, Result};
use crate::oauth_routes::OauthState;
use crate::server::{self, HttpOutcome};
use crate::tools::ToolCategory;
use crate::workspace::WorkspaceError;

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
    pub(crate) registry: Arc<crate::workspace::WorkspaceRegistry>,
    /// The bounded per-workspace handle cache MCP dispatch resolves through.
    pub(crate) handles: Arc<crate::workspace::WorkspaceHandles>,
    auth_token: Option<Arc<str>>,
    bearer_scopes: Arc<[ToolCategory]>,
    /// The tool categories enabled on this server. These are the scopes the
    /// discovery documents and the 401 challenge advertise.
    pub(crate) enabled_categories: Arc<[ToolCategory]>,
    /// `Some` turns OAuth on: the discovery routes answer, and the challenge
    /// names them.
    pub(crate) oauth: Option<Arc<OauthState>>,
    /// The per-file cap, workspace budget, and MIME policy for attachment routes.
    pub(crate) attachment_limits: Arc<AttachmentLimits>,
    /// The process-wide cap on concurrently spooling upload bodies. One permit
    /// is held from just before the spool opens until the storing transaction
    /// finishes, so abandoned or disconnected uploads cannot stack anonymous
    /// spool files past the bound.
    pub(crate) attachment_spools: Arc<tokio::sync::Semaphore>,
    /// The resolved runtime switch for the optional browser UI. The `ui`
    /// module registers its routes only when this is true; otherwise every
    /// `/ui/*` path is an ordinary 404.
    pub(crate) ui_enabled: bool,
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
    /// The resolved runtime switch for the optional browser UI. On, the
    /// router carries the UI routes and the OAuth store seeds its reserved
    /// browser clients; off, both stay absent.
    pub ui_enabled: bool,
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
            ui_enabled,
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
        let state = HttpState {
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
            ui_enabled,
        };
        // The seed is the server.rs startup step the test fixture stands in
        // for: a test state opens the store, and the reserved browser clients
        // exist exactly when the resolved switch says so.
        if let Some(oauth_state) = state.oauth.as_ref() {
            oauth_state
                .seed_browser_clients(ui_enabled)
                .expect("seed the reserved browser clients");
        }
        state
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
    /// The resolved runtime switch for the optional browser UI.
    pub ui_enabled: bool,
    pub tls_cert: Option<std::path::PathBuf>,
    pub tls_key: Option<std::path::PathBuf>,
}

/// Build the axum router for the HTTP transport. Exposed so tests can drive it
/// with `tower::ServiceExt::oneshot` without binding a socket.
pub fn router(state: HttpState) -> Router {
    let router = Router::new()
        .route("/mcp", post(post_handler).get(get_handler))
        .route("/", post(post_handler).get(get_handler));
    // The optional browser UI: pages, embedded assets, and the `/ui/api/*`
    // adapters. The module reads the resolved runtime switch itself and
    // registers nothing when the UI is off, so every `/ui/*` path is then an
    // ordinary 404. Without the feature there is no module and no route.
    #[cfg(feature = "ui")]
    let router = crate::ui::attach(router, &state);
    // The two `.well-known` documents and the OAuth routes. With OAuth off
    // the documents answer 404, so a server without OAuth advertises no
    // authorization server.
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
        ui_enabled,
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
        ui_enabled,
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
pub(crate) fn principal_of(state: &HttpState, headers: &HeaderMap) -> Option<Principal> {
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
pub(crate) fn unauthorized(state: &HttpState) -> Response {
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
pub(crate) fn insufficient_scope(state: &HttpState, scopes: &[&'static str]) -> Response {
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
/// The shipped viewer does not use that fallback. The viewer app reads
/// `#token=` from the URL fragment — which a browser never sends to a server —
/// keeps it in `sessionStorage`, and puts it in the `Authorization` header of
/// every data request, so a human at `/ui` is resolved by [`principal_of`] and
/// may present either credential. The query fallback is for a script, and it
/// takes the static token alone: an issued token in a URL is a credential in a
/// history file, a proxy log and a `Referer` header.
pub(crate) fn principal_of_ui(
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
pub(crate) fn admin_principal(state: &HttpState, headers: &HeaderMap) -> Option<Principal> {
    let p = principal_of_ui(state, headers, None)?;
    p.scopes
        .contains(crate::principals::ADMIN_SCOPE)
        .then_some(p)
}

/// The gate every `/ui/api/*` handler runs first. On success, return the
/// authorized principal so workspace checks do not repeat token validation.
pub(crate) fn admin_gate(
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

pub(crate) fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

pub(crate) fn bad_request(message: impl Into<String>) -> Response {
    json_error(StatusCode::BAD_REQUEST, message)
}

pub(crate) fn conflict(message: impl Into<String>) -> Response {
    json_error(StatusCode::CONFLICT, message)
}

pub(crate) fn not_found() -> Response {
    json_error(StatusCode::NOT_FOUND, "no such row")
}

/// Hide whether a denied workspace exists. Invalid selectors and absent
/// defaults keep their distinct input errors for the viewer.
pub(crate) fn workspace_failure(error: &WorkspaceError) -> Response {
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

pub(crate) fn store_failure(e: impl std::fmt::Display) -> Response {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("principals store: {e}"),
    )
}

/// A 500 whose message names the OAuth store, not the principals store: the
/// token-revocation path in the delete handler can fail in either, and a
/// mislabeled body sends the operator to the wrong logs.
pub(crate) fn oauth_store_failure(e: impl std::fmt::Display) -> Response {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("oauth store: {e}"),
    )
}

pub(crate) fn attachment_limits(
    settings: crate::config_file::AttachmentsSection,
) -> Arc<AttachmentLimits> {
    Arc::new(AttachmentLimits {
        max_bytes: settings.max_bytes,
        workspace_byte_budget: settings.workspace_byte_budget,
        allow_mime: settings.allow_mime,
    })
}

#[cfg(all(test, feature = "ui"))]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::server::MCPServer;
    use crate::tools::ToolCategory;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const UI_TEST_BEARER: &str = "unit-ui-token";

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
            ui_enabled: true,
        }
    }

    /// Drive one viewer request through the router with the static bearer.
    async fn ui_request(state: &HttpState, uri: &str) -> Response<axum::body::Body> {
        router(state.clone())
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::AUTHORIZATION, format!("Bearer {UI_TEST_BEARER}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// The viewer reads the whole graph, so it needs the same `graph-read`
    /// scope as `read_graph`. Without it the endpoint must refuse, even though
    /// the category is enabled and the token is accepted.
    #[tokio::test]
    async fn ui_graph_refuses_a_credential_without_the_graph_read_scope() {
        let dir = tempfile::tempdir().unwrap();
        let state = ui_state(&dir, &[ToolCategory::Vectors]);
        let resp = ui_request(&state, "/ui/api/graph").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let _ = resp.into_body().collect().await.unwrap();
    }

    /// Control: the same request with the scope is served.
    #[tokio::test]
    async fn ui_graph_serves_a_credential_holding_the_graph_read_scope() {
        let dir = tempfile::tempdir().unwrap();
        let state = ui_state(&dir, &[ToolCategory::GraphRead]);
        let resp = ui_request(&state, "/ui/api/graph").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = resp.into_body().collect().await.unwrap();
    }

    /// Every response leaves the router with `Server: mcpmem <version>`, so
    /// an operator can identify a running build without opening a session.
    #[tokio::test]
    async fn every_response_carries_the_server_version_header() {
        let dir = tempfile::tempdir().unwrap();
        let state = ui_state(&dir, &[ToolCategory::GraphRead]);
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/ui/api/graph")
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
}
