//! Shared fixtures for the OAuth integration tests.
//!
//! Every integration test target compiles this module separately and uses a
//! subset of it, so an item unused by one binary is expected rather than dead.
//! The module is small and single-purpose; keep it that way instead of relying
//! on the compiler to notice a helper nobody calls.
#![allow(dead_code)]

/// A real OpenID Connect provider on a loopback port, for the tests of the
/// upstream leg. It is a submodule rather than a second `mod` at each test
/// root so that the fixture stays one module with one `dead_code` allowance.
pub mod fake_idp;

/// The hops a client walks before it sees the consent page, the helpers that
/// read the page and post it back, and the hop from a rendered page to an
/// authorization code. Tasks that start from a consent page or from a code —
/// consent itself, the token exchange, the expiry windows — share this one
/// path rather than each rebuilding it.
pub mod flow;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response};
use http_body_util::BodyExt;
use mcpmem::config::OAuthConfig;
use mcpmem::http::{HttpState, TestSetup};
use mcpmem::oauth_routes::OauthState;
use mcpmem::principals::PrincipalEntry;
use mcpmem::tools::ToolCategory;
use tempfile::TempDir;

pub const PUBLIC_URL: &str = "https://mem.example.com";

/// One allowed human, holding graph-read and graph-write.
pub fn principals(iss: &str) -> Vec<PrincipalEntry> {
    vec![PrincipalEntry {
        name: "adam".into(),
        iss: iss.into(),
        sub: "sub-1".into(),
        label: Some("adam@example.com".into()),
        scopes: vec!["graph-read".into(), "graph-write".into()],
    }]
}

/// The configuration of a server published at [`PUBLIC_URL`].
pub fn oauth_config(upstream_issuer: &str) -> OAuthConfig {
    oauth_config_at(PUBLIC_URL, upstream_issuer)
}

/// The configuration of a server published at `public_url`. A `public_url` that
/// carries a path prefix is legal, and moves both well-known paths.
pub fn oauth_config_at(public_url: &str, upstream_issuer: &str) -> OAuthConfig {
    OAuthConfig {
        public_url: public_url.into(),
        oidc_issuer: upstream_issuer.into(),
        oidc_client_id: "mcpmem-test".into(),
        oidc_client_secret: None,
        principals: principals(upstream_issuer),
        cimd_allowed_domains: vec!["claude.ai".into(), "chatgpt.com".into()],
        trust_forwarded_proto: true,
    }
}

/// A clock a test moves, in place of the wall clock. Microseconds since the
/// epoch, the unit every `expires_us` column uses.
#[derive(Clone)]
pub struct Clock(Arc<AtomicI64>);

impl Clock {
    /// A clock reading `now_us`.
    pub fn at(now_us: i64) -> Clock {
        Clock(Arc::new(AtomicI64::new(now_us)))
    }

    /// What the clock reads now.
    pub fn now_us(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Move the clock forward. A test cannot wait out a token lifetime.
    pub fn advance_seconds(&self, seconds: i64) {
        self.0
            .fetch_add(seconds * 1_000_000, std::sync::atomic::Ordering::Relaxed);
    }

    fn as_fn(&self) -> Arc<dyn Fn() -> i64 + Send + Sync> {
        let cell = Arc::clone(&self.0);
        Arc::new(move || cell.load(Ordering::Relaxed))
    }
}

/// The two scope lists a server holds. Named fields, because both are
/// `Vec<ToolCategory>` and nothing else would show which is which.
pub struct Scopes {
    /// What the presented credential holds. The `/ui` gate reads these.
    pub bearer: Vec<ToolCategory>,
    /// What the server exposes, and so advertises as OAuth scopes.
    pub enabled: Vec<ToolCategory>,
}

impl Scopes {
    /// Every category, on both lists.
    pub fn all() -> Scopes {
        Scopes {
            bearer: ToolCategory::ALL.to_vec(),
            enabled: ToolCategory::ALL.to_vec(),
        }
    }

    /// Every category enabled, but a credential holding only `bearer`.
    pub fn bearer_holds(bearer: Vec<ToolCategory>) -> Scopes {
        Scopes {
            bearer,
            enabled: ToolCategory::ALL.to_vec(),
        }
    }
}

/// A router, the state behind it, the database directory, and the guard on the
/// process-wide tool-category flags.
///
/// Every field is private, and no accessor hands out anything that outlives the
/// struct. That is deliberate: a router used after its `Server` is gone would
/// hold an unlinked database directory and a released category guard, and
/// `let app = oauth_server().await.router;` is one token away from the call
/// shape the first draft of these tests used. Requests therefore go through
/// [`Server::request`], which borrows for the length of the call.
///
/// Hold the `Server` for the whole test. One at a time: see [`server`].
pub struct Server {
    router: Router,
    state: HttpState,
    /// `Some` when the server was built with an injected clock.
    clock: Option<Clock>,
    dir: TempDir,
    /// Building a server publishes `GRAPH_READ_ENABLED` and
    /// `GRAPH_WRITE_ENABLED` (`src/server.rs`), which are process-wide. Holding
    /// the server holds this guard, so no two servers in one test binary can
    /// disagree about the flags while either is in use.
    categories: tokio::sync::MutexGuard<'static, ()>,
}

impl Server {
    /// Send one request through the router and return the response.
    ///
    /// The router is cloned per call, as `oneshot` consumes it, and the clone
    /// never leaves this borrow.
    pub async fn request(&self, req: Request<Body>) -> Response<Body> {
        use tower::ServiceExt;
        self.router
            .clone()
            .oneshot(req)
            .await
            .expect("the router answers every request")
    }

    /// The OAuth state, for a test that inspects the store or the clock.
    pub const fn oauth(&self) -> &Arc<OauthState> {
        self.state.oauth().expect("this server has OAuth on")
    }

    /// The clock this server reads. Panics when none was injected.
    pub const fn clock(&self) -> &Clock {
        self.clock
            .as_ref()
            .expect("this server was built with a clock")
    }

    /// The directory holding the database, for a test that inspects the files.
    pub fn dir(&self) -> &std::path::Path {
        self.dir.path()
    }
}

/// The thread holding [`CATEGORY_FLAGS`], or `None` when the guard is free.
///
/// A `#[tokio::test]` drives its current-thread runtime on one libtest thread,
/// so an acquisition attempt from a thread that already holds the guard is
/// exactly the two-server case, and nothing else is. That makes the check
/// deterministic: no clock, and no false positive from a binary whose tests
/// queue for a long time.
static GUARD_HOLDER: std::sync::Mutex<Option<std::thread::ThreadId>> = std::sync::Mutex::new(None);

static CATEGORY_FLAGS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn holder() -> std::sync::MutexGuard<'static, Option<std::thread::ThreadId>> {
    GUARD_HOLDER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Drop for Server {
    fn drop(&mut self) {
        // Before `categories` drops, so the guard is never free while it is
        // still recorded as held.
        *holder() = None;
    }
}

/// Build a server over a fresh temporary database. `clock` replaces the wall
/// clock when given.
///
/// Waiting for another test's server is normal and unbounded: every server in a
/// binary serialises on the category guard. Waiting for your own is a deadlock,
/// and panics here.
pub async fn server(oauth: Option<OAuthConfig>, scopes: Scopes, clock: Option<Clock>) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("t.mcpmem");
    build(dir, db_path, oauth, None, None, scopes, clock).await
}

/// Like [`server`], and carrying `auth_token` as the static bearer token as
/// well. `--auth-token-file` and `--oidc-issuer` are independent flags, so a
/// deployment may configure both, and this is the only way to build that pair
/// behind the router.
pub async fn server_with_static_token(
    oauth: Option<OAuthConfig>,
    auth_token: &str,
    scopes: Scopes,
    clock: Option<Clock>,
) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("t.mcpmem");
    build(
        dir,
        db_path,
        oauth,
        Some(Arc::from(auth_token)),
        None,
        scopes,
        clock,
    )
    .await
}

/// Like [`server`], reading client identifier metadata documents with `fetch`.
///
/// The shipped fetcher speaks `https` to a host on the operator's allow-list,
/// which no loopback fixture can be, so a test that drives the authorization
/// endpoint with a metadata-document identifier supplies the document here.
pub async fn server_with_fetch(
    oauth: Option<OAuthConfig>,
    fetch: Arc<dyn mcpmem_oauth::registration::Fetch>,
    scopes: Scopes,
    clock: Option<Clock>,
) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("t.mcpmem");
    build(dir, db_path, oauth, None, Some(fetch), scopes, clock).await
}

/// A construction that panics inside `HttpState::for_test`: the database path
/// names a directory that does not exist, so opening the graph fails.
///
/// Only the test that proves a failed construction leaves the guard record
/// clean calls this. The signature claims a `Server` it never returns.
pub async fn server_that_fails_to_build() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("no-such-directory/t.mcpmem");
    build(dir, db_path, None, None, None, Scopes::all(), None).await
}

async fn build(
    dir: TempDir,
    db_path: std::path::PathBuf,
    oauth: Option<OAuthConfig>,
    auth_token: Option<Arc<str>>,
    metadata_fetch: Option<Arc<dyn mcpmem_oauth::registration::Fetch>>,
    scopes: Scopes,
    clock: Option<Clock>,
) -> Server {
    assert!(
        *holder() != Some(std::thread::current().id()),
        "one Server at a time: this thread already holds the tool-category \
         guard. The guard is not reentrant, so a test holding two Server \
         values waits forever. Drop the first before you build the second, or \
         split the test in two."
    );
    let categories = CATEGORY_FLAGS.lock().await;

    // Everything that can panic happens before the record. A panic here unwinds
    // `categories` and frees the guard, and no `Server` exists to clear the
    // record in `Drop`, so a record taken any earlier would outlive the guard
    // it describes — and every later test on this thread would then trip the
    // assertion above with a false cause.
    let state = HttpState::for_test(TestSetup {
        db_path,
        oauth,
        auth_token,
        metadata_fetch,
        bearer_scopes: scopes.bearer,
        enabled_categories: scopes.enabled,
        now_us: clock.as_ref().map(Clock::as_fn),
    });
    let router = mcpmem::http::router(state.clone());

    // Nothing between the lock and here awaits, so no reentrant call can
    // arrive in that window and the record is not needed until now.
    *holder() = Some(std::thread::current().id());
    Server {
        router,
        state,
        clock,
        dir,
        categories,
    }
}

/// OAuth on, every category enabled, and an upstream that is never reached.
pub async fn oauth_server() -> Server {
    oauth_server_with("https://idp.invalid").await
}

/// OAuth on, every category enabled, against the named upstream issuer.
pub async fn oauth_server_with(upstream_issuer: &str) -> Server {
    server(Some(oauth_config(upstream_issuer)), Scopes::all(), None).await
}

/// OAuth on, against a never-reached upstream, with the given scope lists.
pub async fn oauth_server_with_scopes(scopes: Scopes) -> Server {
    server(Some(oauth_config("https://idp.invalid")), scopes, None).await
}

/// OAuth on, every category enabled, reading a clock the test moves.
pub async fn oauth_server_with_clock(clock: Clock) -> Server {
    server(
        Some(oauth_config("https://idp.invalid")),
        Scopes::all(),
        Some(clock),
    )
    .await
}

/// OAuth on for a server published under a path prefix. Both well-known paths
/// move: the suffix a client appends is relative to the origin.
pub async fn oauth_server_at(public_url: &str) -> Server {
    server(
        Some(oauth_config_at(public_url, "https://idp.invalid")),
        Scopes::all(),
        None,
    )
    .await
}

/// OAuth on and no tool category enabled: what a bare `--oidc-issuer` with no
/// `--enable-*` flag produces.
pub async fn oauth_server_without_categories() -> Server {
    server(
        Some(oauth_config("https://idp.invalid")),
        Scopes {
            bearer: Vec::new(),
            enabled: Vec::new(),
        },
        None,
    )
    .await
}

/// OAuth off and no static token: the fully open server.
pub async fn open_server() -> Server {
    server(None, Scopes::all(), None).await
}

pub async fn json(res: Response<axum::body::Body>) -> serde_json::Value {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

pub fn header(res: &Response<axum::body::Body>, name: &str) -> String {
    res.headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing header {name}"))
        .to_str()
        .unwrap()
        .to_owned()
}
