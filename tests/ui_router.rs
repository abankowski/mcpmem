//! Router-matrix tests for the optional browser UI module.
//!
//! These tests spawn the real binary and drive it over a raw TCP stream,
//! matching `tests/ui_http.rs`. The matrix has four properties:
//!
//! - Runtime UI on: pages, assets, and the `/ui/api/*` groups answer.
//! - The legacy JSON URLs are gone: the old `/ui/*` data routes answer 404,
//!   or serve the page shell where a page now lives.
//! - The data lives under `/ui/api/*`: graph and search answer with the spec
//!   envelope.
//! - Runtime UI off: every page, asset, and `/ui/api/*` route answers 404.
//!
//! The last three properties also hold in a build without the `ui` feature,
//! so a `#[cfg(not(feature = "ui"))]` companion asserts the absence there.

use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const TEST_BEARER: &str = "router-test-bearer";

struct TestServer {
    child: Child,
    port: u16,
    db_path: String,
    log_path: String,
    config_path: String,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        remove_server_files(&self.db_path, &self.log_path, &self.config_path);
    }
}

/// Remove the graph file, registry, workspace directory, and the two
/// temporary files this test server's process used.
fn remove_server_files(db_path: &str, log_path: &str, config_path: &str) {
    for ext in [
        "",
        "-wal",
        "-shm",
        ".workspaces.sqlite",
        ".workspaces.sqlite-wal",
        ".workspaces.sqlite-shm",
    ] {
        let _ = std::fs::remove_file(format!("{db_path}{ext}"));
    }
    let _ = std::fs::remove_dir_all(format!("{db_path}.workspaces"));
    let _ = std::fs::remove_dir_all(format!("{db_path}.code"));
    let _ = std::fs::remove_file(log_path);
    let _ = std::fs::remove_file(config_path);
}

/// Grab a currently-free localhost port by binding to :0 and releasing it.
///
/// The port is only *probably* still free by the time the child binds it, so
/// [`spawn`] retries on a fresh port when the child dies on the address.
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().unwrap().port()
}

/// Why one spawn attempt never reached a serving state.
struct StartupFailure {
    port: u16,
    elapsed: Duration,
    polls: usize,
    /// What the last health-check poll saw.
    last: String,
    /// `None` while the child was still running when the attempt was abandoned.
    exit: Option<ExitStatus>,
    /// Everything the child printed on either stream.
    output: String,
}

impl StartupFailure {
    /// A child that exited complaining about the address lost the race in
    /// [`free_port`]. That is the only retryable failure.
    fn lost_port_race(&self) -> bool {
        self.exit.is_some() && self.output.contains("Address already in use")
    }
}

impl std::fmt::Display for StartupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let child = match self.exit {
            Some(status) => format!("child exited: {status}"),
            None => "child still running".to_string(),
        };
        let output = self.output.trim();
        let output = if output.is_empty() {
            "<no output>"
        } else {
            output
        };
        write!(
            f,
            "server did not start serving on 127.0.0.1:{} after {:?} \
             ({} polls, last response: {}); {child}; child output:\n{output}",
            self.port, self.elapsed, self.polls, self.last
        )
    }
}

/// How many ports a spawn will try before giving up.
const SPAWN_ATTEMPTS: usize = 5;

/// Spawn the built binary with `--enable-all`, the static bearer, and a
/// config file whose `[server] ui` key is `ui_text`.
///
/// The config file is how the runtime switch is set: the binary under test
/// is the packaged one, and a flag on the command line would bypass the
/// file layer the operator uses.
fn spawn(ui_text: &str, bearer: Option<&str>) -> TestServer {
    let mut races = Vec::new();
    for attempt in 1..=SPAWN_ATTEMPTS {
        match try_spawn(ui_text, bearer) {
            Ok(server) => return server,
            Err(failure) if failure.lost_port_race() => {
                races.push(format!("attempt {attempt}: {failure}"));
            }
            Err(failure) => {
                panic!("attempt {attempt} of {SPAWN_ATTEMPTS}: {failure}")
            }
        }
    }
    panic!(
        "no usable port after {SPAWN_ATTEMPTS} attempts:\n{}",
        races.join("\n\n")
    );
}

/// One spawn attempt on one port: either a server that is serving, or the
/// reason it never got there.
fn try_spawn(
    ui_text: &str,
    bearer: Option<&str>,
) -> std::result::Result<TestServer, StartupFailure> {
    let port = free_port();
    let pid = std::process::id();
    let db_path = format!("/tmp/ui_router_{pid}_{port}.db");
    let log_path = format!("/tmp/ui_router_{pid}_{port}.log");
    let config_path = format!("/tmp/ui_router_{pid}_{port}.toml");
    remove_server_files(&db_path, &log_path, &config_path);
    std::fs::write(&config_path, format!("[server]\nui = {ui_text}\n"))
        .expect("write the test config");

    let bin =
        std::env::var("CARGO_BIN_EXE_mcpmem").unwrap_or_else(|_| "target/debug/mcpmem".into());

    // Both streams share one file rather than a pipe: a failure can then quote
    // them in order, and neither can fill a pipe buffer while the test is
    // polling instead of reading.
    let log = File::create(&log_path).expect("create server log");
    let log_err = log.try_clone().expect("clone server log handle");

    let mut cmd = Command::new(&bin);
    cmd.arg("-f")
        .arg(&db_path)
        .arg("--legacy-owner-id")
        .arg("machine:local")
        .arg("--transport")
        .arg("http")
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--log-level")
        .arg("info")
        .arg("--enable-all")
        .arg("--config")
        .arg(&config_path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    if let Some(tok) = bearer {
        cmd.arg("--auth-token").arg(tok);
    }

    let mut child = cmd.spawn().expect("failed to spawn mcpmem");

    // The child must print the address it bound, and the router must answer
    // one request: bound is not yet serving, and a 200 proves `axum::serve`
    // is dispatching. The status answer for `/ui` differs with the switch,
    // so any HTTP status ends the poll.
    let bound = format!("http://127.0.0.1:{port}/mcp");
    let started = Instant::now();
    let deadline = started + Duration::from_secs(10);
    let mut polls = 0usize;
    let mut last;
    loop {
        polls += 1;
        if std::fs::read_to_string(&log_path).is_ok_and(|out| out.contains(&bound)) {
            match try_request(port, "GET", "/ui", None, None) {
                Some((_code, _headers, _body)) => {
                    return Ok(TestServer {
                        child,
                        port,
                        db_path,
                        log_path,
                        config_path,
                    });
                }
                None => last = "connection refused".to_string(),
            }
        } else {
            last = "child has not reported a bound listener".to_string();
        }
        let exit = child.try_wait().expect("poll server child");
        if exit.is_some() || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let output = std::fs::read_to_string(&log_path)
                .unwrap_or_else(|e| format!("<log unreadable: {e}>"));
            remove_server_files(&db_path, &log_path, &config_path);
            return Err(StartupFailure {
                port,
                elapsed: started.elapsed(),
                polls,
                last,
                exit,
                output,
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Send one HTTP request over a fresh connection; return (status, headers, body).
fn request(
    port: u16,
    method: &str,
    path: &str,
    json_body: Option<&str>,
    bearer: Option<&str>,
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let mut req = format!("{method} {path} HTTP/1.1\r\n");
    req.push_str("Host: 127.0.0.1\r\n");
    req.push_str("Accept: application/json, text/html\r\n");
    if let Some(tok) = bearer {
        req.push_str(&format!("Authorization: Bearer {tok}\r\n"));
    }
    if let Some(body) = json_body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("Connection: close\r\n\r\n");
    if let Some(body) = json_body {
        req.push_str(body);
    }

    stream.write_all(req.as_bytes()).expect("write request");
    stream.flush().unwrap();

    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).to_string();

    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);

    let (headers, body) = text
        .split_once("\r\n\r\n")
        .map(|(h, b)| (h.to_string(), b.to_string()))
        .unwrap_or_default();

    (status, headers, body)
}

/// Like [`request`], but returns `None` if the connection is refused — used
/// by the startup health check, where refusals are expected until the child
/// binds.
fn try_request(
    port: u16,
    method: &str,
    path: &str,
    json_body: Option<&str>,
    bearer: Option<&str>,
) -> Option<(u16, String, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let mut req = format!("{method} {path} HTTP/1.1\r\n");
    req.push_str("Host: 127.0.0.1\r\n");
    req.push_str("Accept: application/json, text/html\r\n");
    if let Some(tok) = bearer {
        req.push_str(&format!("Authorization: Bearer {tok}\r\n"));
    }
    if let Some(body) = json_body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("Connection: close\r\n\r\n");
    if let Some(body) = json_body {
        req.push_str(body);
    }

    stream.write_all(req.as_bytes()).expect("write request");
    stream.flush().unwrap();

    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).to_string();

    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);

    let (headers, body) = text
        .split_once("\r\n\r\n")
        .map(|(h, b)| (h.to_string(), b.to_string()))
        .unwrap_or_default();

    Some((status, headers, body))
}

fn get(port: u16, path: &str, bearer: Option<&str>) -> (u16, String, String) {
    request(port, "GET", path, None, bearer)
}

/// The built asset manifest, read from the repository the test runs in. The
/// server embeds exactly this file, so the routes must answer it verbatim.
fn manifest() -> serde_json::Value {
    let text = std::fs::read_to_string(format!(
        "{}/ui/dist/ui-manifest.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("the built manifest ships with the repo");
    serde_json::from_str(&text).expect("the manifest is JSON")
}

/// Every path the off-state tests probe: the four page routes, the manifest
/// assets, and the `/ui/api/*` routes.
fn api_probes() -> Vec<String> {
    let mut probes: Vec<String> = vec![
        "/ui".into(),
        "/ui/search".into(),
        "/ui/admin".into(),
        "/ui/admin/callback".into(),
        "/ui/api/graph".into(),
        "/ui/api/search".into(),
        "/ui/api/node".into(),
        "/ui/api/expand".into(),
        "/ui/api/relation".into(),
        "/ui/api/workspaces".into(),
        "/ui/api/attachments".into(),
        "/ui/api/principals".into(),
        "/ui/api/waitlist".into(),
    ];
    for (path, _meta) in manifest()["files"].as_object().unwrap() {
        probes.push(path.clone());
    }
    probes
}

// ── Runtime UI on ──────────────────────────────────────────────────────────

/// With the feature compiled and the runtime switch on, the four pages answer
/// with the React shell, and every manifest asset answers with its manifest
/// content type and byte count.
#[cfg(feature = "ui")]
#[test]
fn pages_and_assets_serve_with_runtime_ui_on() {
    let srv = spawn("true", Some(TEST_BEARER));
    for page in ["/ui", "/ui/search", "/ui/admin", "/ui/admin/callback"] {
        let (status, headers, _body) = get(srv.port, page, None);
        assert_eq!(status, 200, "GET {page} should serve the shell");
        assert!(
            headers.to_lowercase().contains("content-type: text/html"),
            "{page} must be served as HTML: {headers}"
        );
    }
    for (path, meta) in manifest()["files"].as_object().unwrap() {
        let expected_type = meta["contentType"]
            .as_str()
            .expect("the manifest names a content type");
        let expected_bytes = meta["bytes"]
            .as_u64()
            .expect("the manifest names a byte count") as usize;
        let (status, headers, body) = get(srv.port, path.as_str(), None);
        assert_eq!(status, 200, "GET {path} should serve the asset: {body:.80}");
        assert!(
            headers
                .to_lowercase()
                .contains(&format!("content-type: {}", expected_type)),
            "{path} must keep its manifest content type: {headers}"
        );
        assert_eq!(
            body.len(),
            expected_bytes,
            "{path} must serve exactly the built bytes"
        );
    }
}

/// The old JSON contracts under `/ui/*` are gone: the data routes answer 404,
/// and the one URL that is now a page never answers with the old envelope.
#[cfg(feature = "ui")]
#[test]
fn legacy_json_urls_are_gone() {
    let srv = spawn("true", Some(TEST_BEARER));
    // A valid graph-read token is attached so the failure mode is "route
    // absent", never a credential refusal.
    for legacy in [
        "/ui/graph",
        "/ui/node",
        "/ui/expand",
        "/ui/workspaces",
        "/ui/attachments",
        "/ui/attachments/1",
        "/ui/graph?workspaceId=00000000-0000-0000-0000-000000000000&limit=10",
    ] {
        let (status, _, body) = get(srv.port, legacy, Some(TEST_BEARER));
        assert_eq!(
            status, 404,
            "the legacy JSON route {legacy} is gone: {body:.80}"
        );
    }
    // `/ui/search` is a page now: the same URL that answered the search JSON
    // answers the shell, and the old envelope never appears.
    let (status, headers, body) = get(srv.port, "/ui/search?q=Alice", Some(TEST_BEARER));
    assert_eq!(status, 200, "the search page answers at the page URL");
    assert!(
        headers.to_lowercase().contains("content-type: text/html"),
        "the search URL serves the shell: {headers}"
    );
    assert!(
        !body.contains("\"entities\""),
        "the old search JSON envelope is gone from /ui/search"
    );
}

/// The data moved under `/ui/api/*`: graph and search answer with the spec
/// envelope for a caller holding graph-read.
#[cfg(feature = "ui")]
#[test]
fn data_moves_under_api() {
    let srv = spawn("true", Some(TEST_BEARER));
    let ws = register_workspace(srv.port);
    let create = format!(
        r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"create_entities","arguments":{{"workspaceId":"{ws}","entities":[{{"name":"Alice","entityType":"person","observations":[{{"body":"likes hiking"}}]}}]}}}},"id":2}}"#
    );
    let (status, _, body) = request(srv.port, "POST", "/mcp", Some(&create), Some(TEST_BEARER));
    assert_eq!(status, 200, "seed create_entities should succeed: {body}");

    let (status, _headers, body) = get(
        srv.port,
        &format!("/ui/api/graph?workspaceId={ws}"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 200, "GET /ui/api/graph should succeed: {body:.120}");
    let graph: serde_json::Value = serde_json::from_str(&body).expect("graph payload is JSON");
    assert!(graph["entities"].is_array(), "the payload carries entities");
    assert!(
        graph["relations"].is_array(),
        "the payload carries relations"
    );
    assert!(
        graph["entityTypes"].is_array(),
        "the payload carries the legend"
    );
    assert!(graph["stats"].is_object(), "the payload carries stats");
    assert!(
        graph["page"].is_object(),
        "the payload carries the pagination cursor"
    );

    let (status, _headers, body) = get(
        srv.port,
        &format!("/ui/api/search?workspaceId={ws}&q=Ali"),
        Some(TEST_BEARER),
    );
    assert_eq!(
        status, 200,
        "GET /ui/api/search should succeed: {body:.120}"
    );
    let search: serde_json::Value = serde_json::from_str(&body).expect("search payload is JSON");
    let results = search["results"]
        .as_array()
        .expect("search carries results: {body}");
    assert!(
        results.iter().any(|row| {
            row["kind"].as_str() == Some("entity") && row["name"].as_str() == Some("Alice")
        }),
        "search finds the seeded node: {body}"
    );
    assert!(
        search["count"].as_u64().unwrap() >= 1,
        "count reflects the matches: {body}"
    );
    assert!(
        search["elapsedMs"].as_u64().is_some(),
        "elapsedMs is a number: {body}"
    );

    // The relation adapter answers one exact triple: seed Acme and the
    // works_at edge, then read the triple back with its detail shape.
    let acme = format!(
        r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"create_entities","arguments":{{"workspaceId":"{ws}","entities":[{{"name":"Acme","entityType":"company","observations":[]}}]}}}},"id":3}}"#
    );
    let (status, _, body) = request(srv.port, "POST", "/mcp", Some(&acme), Some(TEST_BEARER));
    assert_eq!(status, 200, "seed Acme should succeed: {body}");
    let works_at = format!(
        r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"create_relations","arguments":{{"workspaceId":"{ws}","relations":[{{"from":"Alice","to":"Acme","relationType":"works_at"}}]}}}},"id":4}}"#
    );
    let (status, _, body) = request(srv.port, "POST", "/mcp", Some(&works_at), Some(TEST_BEARER));
    assert_eq!(status, 200, "seed create_relations should succeed: {body}");

    let (status, _headers, body) = get(
        srv.port,
        &format!("/ui/api/relation?workspaceId={ws}&from=Alice&to=Acme&relationType=works_at"),
        Some(TEST_BEARER),
    );
    assert_eq!(
        status, 200,
        "GET /ui/api/relation should succeed: {body:.120}"
    );
    let relation: serde_json::Value =
        serde_json::from_str(&body).expect("relation payload is JSON");
    assert_eq!(relation["from"], "Alice", "the triple keeps its from");
    assert_eq!(relation["to"], "Acme", "the triple keeps its to");
    assert_eq!(
        relation["relationType"], "works_at",
        "the triple keeps its relationType"
    );
    assert!(
        relation["observations"].is_array(),
        "the triple carries its observations"
    );
    assert!(
        relation["attributes"].is_object(),
        "the triple carries its attributes"
    );

    // An absent triple is a 404, the same answer an unknown workspace gives.
    let (status, _headers, _body) = get(
        srv.port,
        &format!("/ui/api/relation?workspaceId={ws}&from=Alice&to=Zed&relationType=works_at"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 404, "an unknown relation triple must be a 404");
}

/// The parsed JSON of one JSON-RPC tool result. `create_workspace` returns
/// its object directly (`result.workspace`), so the helper reads that shape.
#[cfg(feature = "ui")]
fn workspace_of(body: &str) -> serde_json::Value {
    let v: serde_json::Value = serde_json::from_str(body).expect("jsonrpc body");
    let result = v
        .get("result")
        .unwrap_or_else(|| panic!("no result: {body}"));
    result["workspace"].clone()
}

/// Register one private workspace for the bearer over MCP and return its id.
#[cfg(feature = "ui")]
fn register_workspace(port: u16) -> String {
    let create = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"create_workspace","arguments":{"name":"fixture","visibility":"private"}},"id":1}"#;
    let (status, _, body) = request(port, "POST", "/mcp", Some(create), Some(TEST_BEARER));
    assert_eq!(status, 200, "seed create_workspace should succeed: {body}");
    let ws = workspace_of(&body);
    ws["workspaceId"]
        .as_str()
        .expect("the workspace id is returned")
        .to_owned()
}

// ── Runtime UI off ─────────────────────────────────────────────────────────

/// With the runtime switch off, the module registers nothing: every page,
/// every asset, and every `/ui/api/*` route answers 404, even for a caller
/// holding every static-bearer scope.
#[cfg(feature = "ui")]
#[test]
fn runtime_ui_off_returns_404_everywhere() {
    let srv = spawn("false", Some(TEST_BEARER));
    for path in api_probes() {
        let (status, _, body) = get(srv.port, path.as_str(), Some(TEST_BEARER));
        assert_eq!(
            status, 404,
            "GET {path} must be a plain 404 with the UI off: {body:.80}"
        );
    }
}

// ── The build without the feature ──────────────────────────────────────────

/// A build without the `ui` feature has no module and no route: the same
/// probes all answer 404. This is the `--no-default-features` companion of
/// the runtime-off test.
#[cfg(not(feature = "ui"))]
#[test]
fn routes_are_absent_without_the_ui_feature() {
    // `ui = false` never refuses: an explicit true is what a build without
    // the feature refuses at startup (see `tests/ui_switches.rs`).
    let srv = spawn("false", Some(TEST_BEARER));
    for path in api_probes() {
        let (status, _, body) = get(srv.port, path.as_str(), Some(TEST_BEARER));
        assert_eq!(
            status, 404,
            "GET {path} must be absent without the feature: {body:.80}"
        );
    }
}

// ── The OAuth store ────────────────────────────────────────────────────────

/// What a seed test wants the config to hold, named here so the two tests
/// below share one spelling.
fn oauth_config() -> mcpmem::config::OAuthConfig {
    mcpmem::config::OAuthConfig {
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

/// A test server over a fresh database with OAuth on and the given runtime
/// UI switch. The store must live as long as the state.
fn seed_state(ui_enabled: bool) -> (mcpmem::http::HttpState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let state = mcpmem::http::HttpState::for_test(mcpmem::http::TestSetup {
        db_path: dir.path().join("t.mcpmem"),
        oauth: Some(oauth_config()),
        auth_token: None,
        metadata_fetch: None,
        bearer_scopes: Vec::new(),
        enabled_categories: Vec::new(),
        now_us: None,
        ui_enabled,
    });
    (state, dir)
}

/// The reserved browser clients are seeded only when the runtime switch is
/// on: with the feature compiled and `ui` off, the OAuth store holds no
/// `mcpmem-admin-ui` and no `mcpmem-graph-viewer` row.
#[cfg(feature = "ui")]
#[test]
fn oauth_store_does_not_seed_browser_clients_when_ui_is_off() {
    let (state, _dir) = seed_state(false);
    let oauth = state.oauth().expect("oauth is on");
    for id in [mcpmem_oauth::ADMIN_CLIENT_ID, mcpmem_oauth::GRAPH_CLIENT_ID] {
        let row = oauth.with_store(|s| s.get_client(id).expect("the store answers"));
        assert_eq!(
            row, None,
            "the {id} client must not be seeded while the UI is off"
        );
    }
}

/// The control: with the feature compiled and the switch on, both reserved
/// clients exist.
#[cfg(feature = "ui")]
#[test]
fn oauth_store_seeds_browser_clients_when_ui_is_on() {
    let (state, _dir) = seed_state(true);
    let oauth = state.oauth().expect("oauth is on");
    for id in [mcpmem_oauth::ADMIN_CLIENT_ID, mcpmem_oauth::GRAPH_CLIENT_ID] {
        let row = oauth.with_store(|s| s.get_client(id).expect("the store answers"));
        assert_eq!(
            row.map(|r| r.client_id),
            Some(id.to_owned()),
            "the {id} client is seeded while the UI is on"
        );
    }
}
