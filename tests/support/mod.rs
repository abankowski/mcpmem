//! Shared fixtures for the OAuth integration tests.

use std::sync::Arc;

use axum::Router;
use axum::http::Response;
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

pub fn oauth_config(upstream_issuer: &str) -> OAuthConfig {
    OAuthConfig {
        public_url: PUBLIC_URL.into(),
        oidc_issuer: upstream_issuer.into(),
        oidc_client_id: "mcpmem-test".into(),
        oidc_client_secret: None,
        principals: principals(upstream_issuer),
        cimd_allowed_domains: vec!["claude.ai".into(), "chatgpt.com".into()],
        trust_forwarded_proto: true,
    }
}

/// The tool-category flags `dispatch_http_body` reads are process-wide atomics
/// (`src/server.rs`), and building a server publishes them. Hold this lock for
/// the whole body of any test that reads the flags — one that asserts on
/// `tools/list` or calls a tool — and of any test that enables something other
/// than every category. Tests that enable every category need no lock: they
/// publish the value the readers expect.
pub async fn category_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// A router, the state behind it, and the temporary directory holding the
/// database. Keep the whole struct for the length of the test: dropping it
/// removes the directory.
pub struct Server {
    pub router: Router,
    pub state: HttpState,
    dir: TempDir,
}

impl Server {
    /// The OAuth state, for a test that inspects the store or the clock.
    pub const fn oauth(&self) -> &Arc<OauthState> {
        self.state.oauth().expect("this server has OAuth on")
    }

    /// Let the directory outlive this handle, and keep only the router. For a
    /// test that never reads the database again; the operating system removes
    /// the directory.
    pub fn keep(self) -> Router {
        let _ = self.dir.keep();
        self.router
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
}

/// Build a server over a fresh temporary database.
fn server(oauth: Option<OAuthConfig>, scopes: Scopes) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let state = HttpState::for_test(TestSetup {
        db_path: dir.path().join("t.mcpmem"),
        oauth,
        bearer_scopes: scopes.bearer,
        enabled_categories: scopes.enabled,
        now_us: None,
    });
    Server {
        router: mcpmem::http::router(state.clone()),
        state,
        dir,
    }
}

/// OAuth on, every category enabled, and an upstream that is never reached.
pub async fn oauth_server() -> Server {
    oauth_server_with("https://idp.invalid").await
}

/// OAuth on, every category enabled, against the named upstream issuer.
pub async fn oauth_server_with(upstream_issuer: &str) -> Server {
    server(Some(oauth_config(upstream_issuer)), Scopes::all())
}

/// The router of [`oauth_server`], for a test that needs nothing else.
pub async fn oauth_router() -> Router {
    oauth_server().await.keep()
}

/// OAuth on and no tool category enabled: what a bare `--oidc-issuer` with no
/// `--enable-*` flag produces. Hold [`category_lock`] around this one.
pub async fn oauth_server_without_categories() -> Server {
    server(
        Some(oauth_config("https://idp.invalid")),
        Scopes {
            bearer: Vec::new(),
            enabled: Vec::new(),
        },
    )
}

/// OAuth off and no static token: the fully open server.
pub async fn open_server() -> Server {
    server(None, Scopes::all())
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
