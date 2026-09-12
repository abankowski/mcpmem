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
//! Two extra routes serve a **browser knowledge-graph viewer** (HTTP transport
//! only):
//! * `GET /ui` — a self-contained, dependency-free HTML/canvas graph explorer.
//! * `GET /ui/graph` — the JSON the viewer renders (entities, relations, type
//!   legend, stats). Gated behind the same `graph-read` permission as
//!   `read_graph`; auth via the `Authorization` header or a `?token=` fallback.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde_json::json;
use tokio::net::TcpListener;
use tower_http::compression::CompressionLayer;
use tracing::{error, info};

use crate::authz::Principal;
use crate::errors::{MCSError, Result};
use crate::kg::GraphHandle;
use crate::oauth_routes::OauthState;
use crate::server::{self, HttpOutcome};
use crate::tools::ToolCategory;
use crate::vector_store::VectorStore;

/// The graph viewer's static assets, embedded at build time (served from `/ui`).
const UI_INDEX_HTML: &str = include_str!("ui/index.html");
const UI_CSS: &str = include_str!("ui/graph.css");
const UI_JS: &str = include_str!("ui/graph.js");

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

/// Shared state for the HTTP handlers: the graph, the optional vector store,
/// an optional bearer token required on every request when present, the scopes
/// that token grants, the categories this server advertises, and the OAuth
/// authorization server when it is on.
#[derive(Clone)]
pub struct HttpState {
    kg: Arc<GraphHandle>,
    vs: Option<Arc<VectorStore>>,
    auth_token: Option<Arc<str>>,
    bearer_scopes: Arc<[ToolCategory]>,
    /// The tool categories enabled on this server. These are the scopes the
    /// discovery documents and the 401 challenge advertise.
    pub(crate) enabled_categories: Arc<[ToolCategory]>,
    /// `Some` turns OAuth on: the discovery routes answer, and the challenge
    /// names them.
    pub(crate) oauth: Option<Arc<OauthState>>,
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
    /// Scopes the presented credential holds. With no static token configured
    /// these are the scopes of the anonymous principal an open server
    /// dispatches with, and the `/ui` gate reads them.
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
            enabled_categories: enabled_categories.clone(),
            ..crate::config::Config::default()
        };
        let busy_timeout_ms = config.busy_timeout_ms;
        let kg = crate::server::MCPServer::new_kg(config)
            .expect("build the test server")
            .graph();
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
            kg,
            vs: None,
            auth_token,
            bearer_scopes: Arc::from(bearer_scopes),
            enabled_categories: Arc::from(enabled_categories),
            oauth,
        }
    }
}

/// Everything [`run`] needs. One struct rather than positional parameters:
/// `bearer_scopes` and `enabled_categories` have the same type, and no
/// compiler catches a swap between two arguments of one type.
pub struct HttpRunConfig {
    pub addr: String,
    pub kg: Arc<GraphHandle>,
    pub vs: Option<Arc<VectorStore>>,
    pub auth_token: Option<Arc<str>>,
    /// Scopes granted to the static bearer token.
    pub bearer_scopes: Arc<[ToolCategory]>,
    /// Tool categories enabled on this server, advertised as OAuth scopes.
    pub enabled_categories: Arc<[ToolCategory]>,
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
        .route("/ui/graph.css", get(ui_css_handler))
        .route("/ui/graph.js", get(ui_js_handler))
        .route("/ui/graph", get(ui_graph_handler))
        .route("/ui/search", get(ui_search_handler))
        .route("/ui/node", get(ui_node_handler))
        .route("/ui/expand", get(ui_expand_handler))
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
    // The two `.well-known` documents. With OAuth off both answer 404, so a
    // server without OAuth advertises no authorization server.
    crate::oauth_routes::attach(router)
        // Compress responses when the client advertises support. The graph JSON
        // and the embedded HTML/CSS/JS are highly compressible; the default
        // predicate skips already-compressed types and, importantly, SSE
        // (`text/event-stream`), so the `/mcp` streams are left untouched.
        .layer(CompressionLayer::new())
        .layer(DefaultBodyLimit::max(server::MAX_REQUEST_BYTES))
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
        kg,
        vs,
        auth_token,
        bearer_scopes,
        enabled_categories,
        oauth,
        tls_cert,
        tls_key,
    } = config;
    let auth = match (auth_token.is_some(), oauth.is_some()) {
        (true, true) => "static bearer + oauth",
        (true, false) => "static bearer",
        (false, true) => "oauth",
        (false, false) => "off",
    };
    let state = HttpState {
        kg,
        vs,
        auth_token,
        bearer_scopes,
        enabled_categories,
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

fn wants_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/event-stream"))
}

/// Resolve the caller, or `None` when the request is unauthorized.
///
/// The order is OAuth token, then the static bearer, then the fully open
/// case, and it is the order of specificity: a value that validates as an
/// issued token is one, and nothing else can be tried for it.
///
/// With OAuth on and no static token, a request without a credential is not
/// allowed: an OAuth server refuses anonymous access even before it can
/// verify an issued token. With neither configured the server stays open,
/// which is the behaviour every existing deployment has — and the open
/// caller holds `bearer_scopes`, so `--static-bearer-scopes` narrows `/mcp`
/// and `/ui` alike rather than one of the two.
fn principal_of(state: &HttpState, headers: &HeaderMap) -> Option<Principal> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().strip_prefix("Bearer ").unwrap_or(v).trim());

    if let (Some(oauth), Some(token)) = (state.oauth.as_ref(), presented)
        && let Some(grant) = oauth.validate(token)
    {
        return Some(crate::authz::oauth_principal(
            &grant.principal,
            grant.scopes.into_iter().collect(),
        ));
    }
    match (state.auth_token.as_ref(), presented) {
        (Some(expected), Some(token)) if server::token_matches(token, expected) => {
            Some(crate::authz::bearer_principal(&state.bearer_scopes))
        }
        (None, _) if state.oauth.is_none() => {
            Some(crate::authz::bearer_principal(&state.bearer_scopes))
        }
        _ => None,
    }
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
    let kg = state.kg.clone();
    let vs = state.vs.clone();
    let auth = state.clone();
    // Read before `headers` moves into the task. One header lookup, and the
    // alternative is cloning the whole map per request.
    let sse = wants_sse(&headers);
    let result = tokio::task::spawn_blocking(move || {
        let principal = principal_of(&auth, &headers)?;
        Some(server::dispatch_http_body(
            &body,
            &kg,
            vs.as_deref(),
            &principal,
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

/// The gate every `/ui/api/*` handler runs first. `Ok(())` when the caller
/// holds the admin scope; `Err(Response)` is the 401/403 answer.
fn admin_gate(state: &HttpState, headers: &HeaderMap) -> std::result::Result<(), Response> {
    if admin_principal(state, headers).is_some() {
        return Ok(());
    }
    let response = if principal_of_ui(state, headers, None).is_some() {
        insufficient_scope(state, &[crate::principals::ADMIN_SCOPE])
    } else {
        unauthorized(state)
    };
    Err(response)
}

/// The id path segment → (iss, sub).
fn key_of_id(id: &str) -> Option<(String, String)> {
    mcpmem_oauth::parse_principal_id(id)
}

/// Whether a built-in principal owns this identity. The keys are owned
/// pairs; `contains` cannot borrow a `(&str, &str)` from them, so the
/// comparison is spelled out rather than cloned per row.
fn is_builtin(oauth: &OauthState, iss: &str, sub: &str) -> bool {
    oauth
        .builtin_keys
        .iter()
        .any(|(i, s)| i == iss && s == sub)
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

fn store_failure(e: impl std::fmt::Display) -> Response {
    json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("principals store: {e}"))
}

/// `GET /ui/api/principals` — every principal the server knows: the built-ins
/// from the principals file, then the runtime rows, with a row whose identity
/// a built-in owns marked `masked_by_builtin`. The sidebar reads the mask
/// flag and the `defaultNewPrincipalScopes` list to render the create form.
async fn admin_list_principals(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return response;
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
        return response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let input: PrincipalInput = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("the body must be a JSON principal"),
    };
    let name = input.name.trim();
    if name.is_empty() || input.iss.is_empty() || input.sub.is_empty() {
        return bad_request("name, iss and sub are required");
    }
    let scopes = match crate::principals::canonical_scopes(&input.scopes) {
        Ok(s) if !s.is_empty() => s,
        _ => return bad_request("at least one known scope is required"),
    };
    if is_builtin(oauth, &input.iss, &input.sub) {
        return conflict("a built-in principal owns this identity; it is immutable");
    }
    // A duplicate runtime key is refused before the store is touched, so the
    // answer is 409 and not the store's UNIQUE-constraint error.
    match oauth.with_principals(|s| s.get(&input.iss, &input.sub)) {
        Ok(Some(_)) => return conflict("a runtime principal already owns this identity"),
        Ok(None) => {}
        Err(e) => return store_failure(e),
    }
    if let Err(e) = oauth
        .with_principals(|s| s.create(&input.iss, &input.sub, name, input.label.as_deref(), &scopes))
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
        id: mcpmem_oauth::principal_id(&input.iss, &input.sub),
        name: name.to_owned(),
        iss: input.iss,
        sub: input.sub,
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
        return response;
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
        None => row.scopes.clone(),
    };
    if let Err(e) = oauth
        .with_principals(|s| s.update(&iss, &sub, &name, label.as_deref(), &scopes))
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

/// `DELETE /ui/api/principals/{id}` — delete one runtime principal and
/// revoke every live token family that names it.
///
/// The revocation runs by the row's *current* name. A token minted under the
/// row's previous name survives a rename-then-delete; the v1 spec documents
/// that gap and this task does not close it.
async fn admin_delete_principal(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return response;
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
    // Revoke before the row goes: a swallowed revoke error would leave a
    // deleted principal's tokens live, and token validation never
    // cross-checks the principals store. On a revoke failure the row stays
    // intact, so the delete is retryable.
    let revoked = match oauth.revoke_principal(&row.name) {
        Ok(n) => n,
        Err(e) => return store_failure(e),
    };
    if let Err(e) = oauth.with_principals(|s| s.delete(&iss, &sub)) {
        return store_failure(e);
    }
    tracing::info!(name = %row.name, revoked, "deleted principal and revoked token families");
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /ui/api/waitlist` — every entry awaiting approval, in the order the
/// store keeps them.
async fn admin_list_waitlist(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return response;
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
        return response;
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
        return response;
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

/// `GET /ui` — serve the browser graph viewer's HTML shell. The shell and its
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
        [(
            header::CONTENT_TYPE,
            "text/javascript; charset=utf-8",
        )],
        ADMIN_JS,
    )
        .into_response()
}

/// Shared auth + scope gate for the viewer's data endpoints (`/ui/graph`,
/// `/ui/search`, `/ui/node`, `/ui/expand`). The viewer reads the whole graph,
/// so it needs both the process-wide category and the scope `read_graph`
/// needs. Returns the error `Response` to send back, or `None` when the
/// request may proceed.
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
) -> Option<Response> {
    let Some(principal) = principal_of_ui(state, headers, params.get("token").map(String::as_str))
    else {
        return Some(unauthorized(state));
    };
    if !server::graph_read_enabled() {
        return Some(
            (
                StatusCode::FORBIDDEN,
                "graph-read tools are disabled; start the server with --enable-graph-read (or --enable-all) to view the graph",
            )
                .into_response(),
        );
    }
    if !crate::authz::allows_tool(&principal, "read_graph") {
        // The scope the challenge names is the one `authz` reports missing, so
        // the header cannot drift from the decision that produced it. The
        // fallback is unreachable while `read_graph` is a tool this server
        // knows, and names the category the viewer stands for.
        let missing = crate::authz::missing_scope(&principal, "read_graph")
            .unwrap_or(ToolCategory::GraphRead.slug());
        return Some(insufficient_scope(state, &[missing]));
    }
    None
}

fn parse_usize(params: &HashMap<String, String>, key: &str, default: usize) -> usize {
    params
        .get(key)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Run a blocking JSON-payload builder off the async reactor (the graph lock may
/// block) and map its `Result<String>` to an HTTP response. Shared by the
/// viewer's `/ui/graph` and `/ui/search` data endpoints.
async fn ui_json<F>(kg: Arc<GraphHandle>, what: &'static str, build: F) -> Response
where
    F: FnOnce(&GraphHandle) -> Result<String> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || build(&kg)).await {
        Ok(Ok(json)) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Ok(Err(e)) => {
            error!("{what} error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(join_err) => {
            error!("{what} task panicked: {join_err}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// `GET /ui/graph` — a page of the whole graph for the viewer: entities, the
/// relations among them, the entity-type legend, overall stats, and a pagination
/// cursor. Requires the `graph-read` category (like `read_graph`) and the same
/// bearer-token gate as the MCP endpoints. Query params: `entityType` (filter),
/// `offset`, `limit` (capped at [`MAX_UI_NODES`]), and `token` (auth fallback).
async fn ui_graph_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(resp) = ui_data_gate(&state, &headers, &params) {
        return resp;
    }
    let entity_type = params.get("entityType").filter(|s| !s.is_empty()).cloned();
    let offset = parse_usize(&params, "offset", 0);
    let limit = parse_usize(&params, "limit", 300).clamp(1, MAX_UI_NODES);
    ui_json(state.kg, "/ui/graph", move |kg| {
        build_graph_payload(kg, entity_type.as_deref(), offset, limit)
    })
    .await
}

/// `GET /ui/search` — a page of FTS5 matches for the viewer's search box, in the
/// same `{entities, relations, entityTypes, stats, page}` shape as `/ui/graph`
/// (the matched nodes; the user double-clicks to expand their relationships).
/// Same auth + `graph-read` gate. Query params: `q` (the query; prefix-matched),
/// `entityType` (filter), `offset`, `limit` (capped at [`MAX_UI_NODES`]),
/// and `token`.
async fn ui_search_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(resp) = ui_data_gate(&state, &headers, &params) {
        return resp;
    }
    let query = params
        .get("q")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let entity_type = params.get("entityType").filter(|s| !s.is_empty()).cloned();
    let offset = parse_usize(&params, "offset", 0);
    let limit = parse_usize(&params, "limit", 100).clamp(1, MAX_UI_NODES);
    ui_json(state.kg, "/ui/search", move |kg| {
        build_search_payload(kg, &query, entity_type.as_deref(), offset, limit)
    })
    .await
}

/// `GET /ui/node` — one entity with its observation bodies, for the inspector to
/// lazy-load when a node is selected. The list endpoints (`/ui/graph`,
/// `/ui/search`) deliberately omit observation bodies (they only carry
/// `obsCount`) to keep those payloads small; this fetches the bodies for the
/// single node the user is looking at. Same auth + `graph-read` gate. Query
/// params: `name` (required) and `token` (auth fallback).
async fn ui_node_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(resp) = ui_data_gate(&state, &headers, &params) {
        return resp;
    }
    let Some(name) = params.get("name").filter(|s| !s.is_empty()).cloned() else {
        return (StatusCode::BAD_REQUEST, "missing 'name' parameter").into_response();
    };
    let kg = state.kg;
    match tokio::task::spawn_blocking(move || kg.get_entity(&name)).await {
        Ok(Ok(Some(entity))) => match serde_json::to_string(&entity) {
            Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
            Err(e) => {
                error!("/ui/node serialize error: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        },
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "entity not found").into_response(),
        Ok(Err(e)) => {
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
/// `both`, default both), and `token` (auth fallback).
async fn ui_expand_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(resp) = ui_data_gate(&state, &headers, &params) {
        return resp;
    }
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

    let kg = state.kg;
    let result =
        tokio::task::spawn_blocking(move || kg.neighbors(&name, direction, None, depth)).await;

    match result {
        Ok(Ok(json)) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        // An unknown entity is a client error (bad `name`), not a server fault.
        Ok(Err(MCSError::InvalidParams(msg))) => (StatusCode::NOT_FOUND, msg).into_response(),
        Ok(Err(e)) => {
            error!("/ui/expand error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(join_err) => {
            error!("/ui/expand task panicked: {join_err}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::server::MCPServer;
    use crate::tools::ToolCategory;

    /// A viewer state with no bearer token and the given scopes on the
    /// credential. The category flags are process-wide atomics, so this goes
    /// through the same entry point `src/main.rs` uses — and it enables both
    /// graph categories, because clearing `graph-write` here would race the
    /// dispatch-gate test in `src/server.rs`, which runs in this same binary.
    fn ui_state(dir: &tempfile::TempDir, scopes: &[ToolCategory]) -> HttpState {
        let config = Config {
            memory_file_path: dir.path().join("memory.db").to_string_lossy().into_owned(),
            enabled_categories: vec![ToolCategory::GraphRead, ToolCategory::GraphWrite],
            ..Config::default()
        };
        let kg = MCPServer::new_kg(config)
            .expect("test server builds")
            .graph();
        HttpState {
            kg,
            vs: None,
            auth_token: None,
            bearer_scopes: Arc::from(scopes),
            enabled_categories: Arc::from(&[ToolCategory::GraphRead, ToolCategory::GraphWrite][..]),
            oauth: None,
        }
    }

    /// The viewer reads the whole graph, so it needs the same `graph-read`
    /// scope as `read_graph`. Without it the endpoint must refuse, even though
    /// the category is enabled and the token is accepted.
    #[tokio::test]
    async fn ui_graph_refuses_a_credential_without_the_graph_read_scope() {
        let dir = tempfile::tempdir().unwrap();
        let state = ui_state(&dir, &[ToolCategory::Vectors]);
        let resp = ui_graph_handler(State(state), HeaderMap::new(), Query(HashMap::new())).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// Control: the same request with the scope is served.
    #[tokio::test]
    async fn ui_graph_serves_a_credential_holding_the_graph_read_scope() {
        let dir = tempfile::tempdir().unwrap();
        let state = ui_state(&dir, &[ToolCategory::GraphRead]);
        let resp = ui_graph_handler(State(state), HeaderMap::new(), Query(HashMap::new())).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
