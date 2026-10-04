//! HTTP-transport tests for the browser knowledge-graph viewer (`GET /ui` and
//! `GET /ui/api/graph`). These exercise the routes added alongside the MCP handlers:
//! the self-contained viewer shell, the JSON data endpoint it renders, and the
//! two gates on that data — the `graph-read` permission and the bearer token.
//!
//! A raw `TcpStream` is the client (no HTTP-client dependency), matching
//! `tests/vector_http.rs`. The attachment inspector exercises byte-exact
//! uploads and downloads, where a real client is the shape the browser uses,
//! so those tests drive `reqwest` (already a dev-dependency) instead.
//!
//! Fixture shape after the workspaces contract: the legacy graph is owned by
//! `machine:local`, so the static bearer is a fresh identity with *no default
//! workspace and no grant*. Every fixture therefore registers its own
//! workspace through MCP (`create_workspace`) and seeds it with an explicit
//! `workspaceId`, and every viewer data request carries the selected
//! `workspaceId` — exactly what the shipped viewer dropdown sends. A request
//! without a `workspaceId` and without a default is the approved
//! selection-required error, never a silent read of the legacy graph.

use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const TEST_BEARER: &str = "viewer-test-bearer";

struct HttpServer {
    child: Child,
    port: u16,
    db_path: String,
    /// File both of the child's output streams were redirected to.
    log_path: String,
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        remove_server_files(&self.db_path, &self.log_path);
    }
}

/// Remove the graph file, registry, workspace directory, code index, and output
/// for this test server's own database path.
fn remove_server_files(db_path: &str, log_path: &str) {
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
}

/// Grab a currently-free localhost port by binding to :0 and releasing it.
///
/// The port is only *probably* still free by the time the child binds it: the
/// listener is gone before the child starts, so another thread in this binary
/// — or any other process — can take it in between. [`spawn_http_server`]
/// therefore treats a child that died on the address as retryable.
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().unwrap().port()
}

/// Why one spawn attempt never reached a serving state.
///
/// `exit` and `output` are what make the failure legible: without them a child
/// that died on a failed bind is indistinguishable from a child that is merely
/// slow, because both surface only as the deadline expiring.
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
    /// [`free_port`]: the port was free when the test read it and taken by the
    /// time the child bound it. That is the only retryable failure — every
    /// other exit means the server itself is broken.
    fn lost_port_race(&self) -> bool {
        self.exit.is_some() && self.output.contains("Address already in use")
    }
}

impl fmt::Display for StartupFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

/// How many ports a spawn will try before giving up; see
/// [`StartupFailure::lost_port_race`].
const SPAWN_ATTEMPTS: usize = 5;

/// Spawn a server with the given `--enable-*` category flags and optional token.
///
/// Retries on a fresh port when the child lost the port race, and panics with
/// the child's own output on any other failure.
fn spawn_http_server(enable_args: &[&str], auth_token: Option<&str>) -> HttpServer {
    let mut races = Vec::new();
    for attempt in 1..=SPAWN_ATTEMPTS {
        match try_spawn_http_server(enable_args, auth_token) {
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

/// Like [`spawn_http_server`], but with an extra `--role <role>` argument.
#[cfg(feature = "extractor")]
fn spawn_http_server_with_role(
    enable_args: &[&str],
    auth_token: Option<&str>,
    role: Option<&str>,
) -> HttpServer {
    let mut races = Vec::new();
    for attempt in 1..=SPAWN_ATTEMPTS {
        match try_spawn_http_server_with_role(enable_args, auth_token, role) {
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
fn try_spawn_http_server(
    enable_args: &[&str],
    auth_token: Option<&str>,
) -> Result<HttpServer, StartupFailure> {
    try_spawn_http_server_impl(enable_args, auth_token, None)
}

/// Like [`try_spawn_http_server`], but with an extra `--role <role>` argument.
#[cfg(feature = "extractor")]
fn try_spawn_http_server_with_role(
    enable_args: &[&str],
    auth_token: Option<&str>,
    role: Option<&str>,
) -> Result<HttpServer, StartupFailure> {
    try_spawn_http_server_impl(enable_args, auth_token, role)
}

/// One spawn attempt on one port: either a server that is serving, or the
/// reason it never got there.
fn try_spawn_http_server_impl(
    enable_args: &[&str],
    auth_token: Option<&str>,
    role: Option<&str>,
) -> Result<HttpServer, StartupFailure> {
    let port = free_port();
    let pid = std::process::id();
    let db_path = format!("/tmp/ui_http_{pid}_{port}.db");
    let log_path = format!("/tmp/ui_http_{pid}_{port}.log");
    remove_server_files(&db_path, &log_path);

    let bin =
        std::env::var("CARGO_BIN_EXE_mcpmem").unwrap_or_else(|_| "target/debug/mcpmem".into());

    // Both streams share one file rather than a pipe: a failure can then quote
    // them in order, and neither can fill a pipe buffer while the test is
    // polling instead of reading. The file is removed with the database.
    let log = File::create(&log_path).expect("create server log");
    let log_err = log.try_clone().expect("clone server log handle");

    let mut cmd = Command::new(&bin);
    cmd.arg("-f")
        .arg(&db_path)
        .arg("--legacy-owner-id")
        // The legacy workspace belongs to `machine:local`. The static bearer
        // is therefore a fresh identity with no default: every fixture below
        // registers its own workspace through MCP and selects it explicitly,
        // and a request without a `workspaceId` is the approved
        // selection-required error.
        .arg("machine:local")
        .arg("--transport")
        .arg("http")
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--log-level")
        // `info` so the child reports the address it bound; that report is the
        // only proof that the server answering on this port is ours.
        .arg("info")
        .args(enable_args);
    if let Some(role) = role {
        cmd.arg("--role").arg(role);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    if let Some(tok) = auth_token {
        cmd.arg("--auth-token").arg(tok);
    }

    let mut child = cmd.spawn().expect("failed to spawn mcpmem");

    // Two conditions, in order. First the child must print the address it
    // bound: a 200 alone proves only that *something* owns this port, and
    // whatever won the race in `free_port` would answer just as happily —
    // adopting it would run the test against a process it neither owns nor can
    // kill. Then `GET /ui`, which needs no auth or permission, must return 200:
    // bound is not yet serving, and a 200 means `axum::serve` is accepting and
    // dispatching (avoids a first-request race).
    let bound = format!("http://127.0.0.1:{port}/mcp");
    let started = Instant::now();
    let deadline = started + Duration::from_secs(10);
    let mut polls = 0usize;
    // Assigned by the poll below, which always runs before any read of it.
    let mut last;
    loop {
        polls += 1;
        if std::fs::read_to_string(&log_path).is_ok_and(|out| out.contains(&bound)) {
            match try_request(port, "GET", "/ui", None, None) {
                Some((200, _, _)) => {
                    return Ok(HttpServer {
                        child,
                        port,
                        db_path,
                        log_path,
                    });
                }
                Some((code, _, body)) => last = format!("HTTP {code} ({} body bytes)", body.len()),
                None => last = "connection refused".to_string(),
            }
        } else {
            last = "child has not reported a bound listener".to_string();
        }
        // A child that has exited will never start serving, so give up on the
        // spot rather than spending the rest of the deadline on a corpse.
        let exit = child.try_wait().expect("poll server child");
        if exit.is_some() || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let output = std::fs::read_to_string(&log_path)
                .unwrap_or_else(|e| format!("<log unreadable: {e}>"));
            remove_server_files(&db_path, &log_path);
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

#[test]
fn http_refuses_startup_without_oauth_or_bearer() {
    let failed = match try_spawn_http_server(&["--enable-all"], None) {
        Ok(_server) => panic!("HTTP served without OAuth or a bearer credential"),
        Err(failed) => failed,
    };
    assert!(
        failed.exit.is_some(),
        "HTTP must exit without a listener: {failed}"
    );
    assert!(
        !failed.lost_port_race(),
        "this was a port collision: {failed}"
    );
    let reason = failed.output.to_ascii_lowercase();
    assert!(
        reason.contains("oauth") && reason.contains("bearer"),
        "the startup error must name both supported credentials: {failed}"
    );
}

/// Send one HTTP request over a fresh connection; return (status, headers, body).
/// Panics if the connection is refused (use [`try_request`] to tolerate that).
fn request(
    port: u16,
    method: &str,
    path: &str,
    json_body: Option<&str>,
    bearer: Option<&str>,
) -> (u16, String, String) {
    try_request(port, method, path, json_body, bearer).expect("connect")
}

/// Like [`request`], but returns `None` if the connection is refused — used by
/// the startup health check, where refusals are expected until the server binds.
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

/// The parsed JSON of one JSON-RPC tool result. `create_workspace` returns its
/// object directly (`result.workspace`), so the helper reads that shape.
fn workspace_of(body: &str) -> serde_json::Value {
    let v: serde_json::Value = serde_json::from_str(body).expect("jsonrpc body");
    let result = v
        .get("result")
        .unwrap_or_else(|| panic!("no result: {body}"));
    result["workspace"].clone()
}

/// Register one private workspace for `bearer` over MCP and return its id.
///
/// The static bearer has no default workspace (the legacy graph is owned by
/// `machine:local`), so registration is explicit: this is the "register and
/// seed through an explicit workspaceId" fixture the approved contract
/// describes.
fn register_workspace(port: u16, bearer: Option<&str>) -> String {
    let create = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"create_workspace","arguments":{"name":"fixture","visibility":"private"}},"id":1}"#;
    let (status, _, body) = request(port, "POST", "/mcp", Some(create), bearer);
    assert_eq!(status, 200, "seed create_workspace should succeed: {body}");
    let ws = workspace_of(&body);
    ws["workspaceId"]
        .as_str()
        .expect("the workspace id is returned")
        .to_owned()
}

/// Populate a tiny graph in `bearer`'s own workspace: register the workspace
/// and seed Alice/Acme with an explicit `workspaceId`. Returns the id.
fn seed_graph(port: u16, bearer: Option<&str>) -> String {
    let ws = register_workspace(port, bearer);

    let create = format!(
        r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"create_entities","arguments":{{"workspaceId":"{ws}","entities":[{{"name":"Alice","entityType":"person","observations":[{{"body":"likes hiking"}}]}},{{"name":"Acme","entityType":"company","observations":[]}}]}}}},"id":2}}"#
    );
    let (status, _, _) = request(port, "POST", "/mcp", Some(&create), bearer);
    assert_eq!(status, 200, "seed create_entities should succeed");

    let rel = format!(
        r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"create_relations","arguments":{{"workspaceId":"{ws}","relations":[{{"from":"Alice","to":"Acme","relationType":"works_at"}}]}}}},"id":3}}"#
    );
    let (status, _, _) = request(port, "POST", "/mcp", Some(&rel), bearer);
    assert_eq!(status, 200, "seed create_relations should succeed");
    ws
}

/// The built asset manifest, read from the repository the test runs in. The
/// server embeds exactly this file, so the routes must answer it verbatim.
fn ui_manifest() -> serde_json::Value {
    let text = std::fs::read_to_string(format!(
        "{}/ui/dist/ui-manifest.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("the built manifest ships with the repo");
    serde_json::from_str(&text).expect("the manifest is JSON")
}

#[test]
fn test_ui_shell_served_as_html() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let (status, headers, body) = get(srv.port, "/ui", None);
    assert_eq!(status, 200, "GET /ui should return the viewer page");
    assert!(
        headers.to_lowercase().contains("content-type: text/html"),
        "viewer must be served as HTML, headers: {headers}"
    );
    // The React shell mounts its app and loads exactly the bundled assets the
    // manifest lists; the legacy hand-written assets are gone.
    let manifest = ui_manifest();
    assert!(
        body.contains("<div id=\"root\">"),
        "expected the React shell: {body:.120}"
    );
    for (path, _meta) in manifest["files"].as_object().unwrap() {
        let name = path
            .strip_prefix("/ui/assets/")
            .expect("a manifest asset path keeps its prefix");
        assert!(
            body.contains(name),
            "the shell should load the bundled asset {name}: {body:.120}"
        );
    }
    for legacy in [
        "/ui/graph.css",
        "/ui/graph.js",
        "/ui/nav.css",
        "/ui/admin.js",
        "/ui/admin.css",
    ] {
        assert!(
            !body.contains(legacy),
            "the shell must not reference the deleted legacy asset {legacy}"
        );
    }
}

#[test]
fn test_ui_assets_served_with_content_types() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));

    for (path, meta) in ui_manifest()["files"].as_object().unwrap() {
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

    // An unknown asset name is a 404: the manifest is the checked list.
    let (status, _headers, _body) = get(srv.port, "/ui/assets/nope-1234.js", None);
    assert_eq!(status, 404, "an unknown asset name must be a 404");
}

#[test]
fn test_ui_expand_returns_neighborhood() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws = seed_graph(srv.port, Some(TEST_BEARER));

    // Expanding Alice must return her plus her neighbour Acme and the edge.
    let (status, headers, body) = get(
        srv.port,
        &format!("/ui/api/expand?workspaceId={ws}&name=Alice"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 200, "GET /ui/api/expand should succeed: {body}");
    assert!(
        headers
            .to_lowercase()
            .contains("content-type: application/json"),
        "expand data must be JSON, headers: {headers}"
    );
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let names: Vec<&str> = v["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"Alice") && names.contains(&"Acme"),
        "got {names:?}"
    );
    assert!(
        v["relations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["relationType"] == "works_at")
    );
}

#[test]
fn test_ui_expand_unknown_entity_is_404() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws = register_workspace(srv.port, Some(TEST_BEARER));
    let (status, _, _) = get(
        srv.port,
        &format!("/ui/api/expand?workspaceId={ws}&name=DoesNotExist"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 404, "expanding a missing entity should be 404");
}

#[test]
fn test_ui_expand_requires_name_and_permission() {
    // Missing name → 400 (client error).
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws = register_workspace(srv.port, Some(TEST_BEARER));
    let (status, _, _) = get(
        srv.port,
        &format!("/ui/api/expand?workspaceId={ws}"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 400, "expand without a name should be 400");
    drop(srv);

    // Read disabled → 403, same gate as /ui/api/graph.
    let srv = spawn_http_server(&["--enable-graph-write"], Some(TEST_BEARER));
    let ws = register_workspace(srv.port, Some(TEST_BEARER));
    let (status, _, _) = get(
        srv.port,
        &format!("/ui/api/expand?workspaceId={ws}&name=Alice"),
        Some(TEST_BEARER),
    );
    assert_eq!(
        status, 403,
        "graph-read disabled must forbid /ui/api/expand"
    );
}

#[test]
fn test_ui_graph_returns_entities_and_relations() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws = seed_graph(srv.port, Some(TEST_BEARER));

    let (status, headers, body) = get(
        srv.port,
        &format!("/ui/api/graph?workspaceId={ws}"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 200, "GET /ui/api/graph should succeed: {body}");
    assert!(
        headers
            .to_lowercase()
            .contains("content-type: application/json"),
        "graph data must be JSON, headers: {headers}"
    );
    let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON payload");
    assert!(
        v["entities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "Alice")
    );
    assert!(
        v["relations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["relationType"] == "works_at")
    );
    // Legend + stats are injected by the handler on top of read_graph's shape.
    assert!(
        v["entityTypes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["type"] == "person")
    );
    assert_eq!(v["stats"]["entities"], 2);
    assert_eq!(v["stats"]["relations"], 1);
    // Pagination cursor drives the viewer's Prev/Next controls.
    assert_eq!(v["page"]["offset"], 0);
    assert_eq!(v["page"]["returned"], 2);
    assert_eq!(v["page"]["hasMore"], false);
}

#[test]
fn test_ui_graph_entity_type_filter() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws = seed_graph(srv.port, Some(TEST_BEARER));

    let (status, _, body) = get(
        srv.port,
        &format!("/ui/api/graph?workspaceId={ws}&entityType=company"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 200, "filtered graph should succeed: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let names: Vec<&str> = v["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["Acme"],
        "filter should return only company entities"
    );
}

#[test]
fn test_ui_graph_requires_graph_read() {
    // Write enabled but read disabled: the viewer's data endpoint is forbidden.
    let srv = spawn_http_server(&["--enable-graph-write"], Some(TEST_BEARER));
    let ws = register_workspace(srv.port, Some(TEST_BEARER));
    let (status, _, body) = get(
        srv.port,
        &format!("/ui/api/graph?workspaceId={ws}"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 403, "graph-read disabled must forbid /ui/api/graph");
    assert!(
        body.contains("graph-read"),
        "403 body should explain the missing permission: {body}"
    );
}

/// The static bearer's scopes and the server's enabled categories are two
/// lists of the same type, wired side by side in `HttpRunConfig`. Here every
/// category is enabled, so the viewer's category gate passes, but the
/// credential holds only `vectors`, so the scope gate must refuse. Swap the two
/// fields and the credential silently holds every category, and this case
/// answers 200.
#[test]
fn test_ui_graph_honours_static_bearer_scopes_not_enabled_categories() {
    let srv = spawn_http_server(
        &["--enable-all", "--static-bearer-scopes", "vectors"],
        Some(TEST_BEARER),
    );
    // A credential without graph-write cannot even register a workspace, so the
    // request names an id the scope gate must refuse before it is resolved.
    let (status, headers, body) = get(
        srv.port,
        "/ui/api/graph?workspaceId=00000000-0000-0000-0000-000000000000",
        Some(TEST_BEARER),
    );
    assert_eq!(
        status, 403,
        "a credential without graph-read must be refused: {body}"
    );
    assert!(
        headers.to_lowercase().contains(
            "www-authenticate: bearer error=\"insufficient_scope\", scope=\"graph-read\""
        ),
        "the refusal must come from the scope gate with its challenge, not from \
         the category gate: {headers}"
    );
}

#[test]
fn test_ui_graph_auth_gate() {
    let srv = spawn_http_server(&["--enable-all"], Some("s3cret"));
    let ws = seed_graph(srv.port, Some("s3cret"));

    // No credentials → 401.
    let (status, _, _) = get(srv.port, &format!("/ui/api/graph?workspaceId={ws}"), None);
    assert_eq!(status, 401, "missing token must be rejected");

    // Wrong token via query → 401.
    let (status, _, _) = get(
        srv.port,
        &format!("/ui/api/graph?workspaceId={ws}&token=nope"),
        None,
    );
    assert_eq!(status, 401, "wrong token must be rejected");

    // Correct token via the ?token= query fallback → 200.
    let (status, _, body) = get(
        srv.port,
        &format!("/ui/api/graph?workspaceId={ws}&token=s3cret"),
        None,
    );
    assert_eq!(status, 200, "query-param token should be accepted: {body}");
    assert!(
        body.contains("Alice"),
        "authed graph should have data: {body}"
    );

    // Correct token via the Authorization header → 200.
    let (status, _, _) = get(
        srv.port,
        &format!("/ui/api/graph?workspaceId={ws}"),
        Some("s3cret"),
    );
    assert_eq!(status, 200, "bearer header token should be accepted");

    // The shell itself carries no data, so it is reachable without a token.
    let (status, _, _) = get(srv.port, "/ui", None);
    assert_eq!(status, 200, "the /ui shell should not require auth");
}

/// A viewer request without a `workspaceId` must not silently read the legacy
/// graph: the static bearer has no default workspace, and the approved
/// contract answers the distinct selection-required error that makes the
/// viewer show "select a workspace" instead of a graph.
#[test]
fn test_ui_graph_without_workspace_id_and_without_default_is_selection_required() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let (status, _, body) = get(srv.port, "/ui/api/graph", Some(TEST_BEARER));
    assert_eq!(
        status, 400,
        "no workspaceId and no default is the selection-required error, not a read: {body}"
    );
    assert!(
        body.to_lowercase().contains("selection required"),
        "the error must be the distinct selection-required shape: {body}"
    );
}

/// Unknown and malformed workspace ids have their approved error shapes: an
/// unknown id is the same not-found as an inaccessible graph, and a malformed
/// id is an input error.
#[test]
fn test_ui_graph_unknown_workspace_id_is_not_found() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    // The positive selector works: the fixture's own workspace reads.
    let ws = register_workspace(srv.port, Some(TEST_BEARER));
    let (status, _, _) = get(
        srv.port,
        &format!("/ui/api/graph?workspaceId={ws}"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 200, "the owned workspace id should read");

    let (status, _, _) = get(
        srv.port,
        "/ui/api/graph?workspaceId=7f4c5a1e-0000-4000-8000-000000000000",
        Some(TEST_BEARER),
    );
    assert_eq!(status, 404, "an unknown workspace id must be not-found");

    let (status, _, _) = get(
        srv.port,
        "/ui/api/graph?workspaceId=not-a-uuid",
        Some(TEST_BEARER),
    );
    assert_eq!(
        status, 400,
        "a malformed workspace id is an input error, not a graph lookup"
    );
}

/// Create `n` entities named `person_0000`..`person_(n-1)` in `workspace_id`.
fn seed_many(port: u16, n: usize, workspace_id: &str) {
    let ents: Vec<String> = (0..n)
        .map(|i| {
            format!(
                r#"{{"name":"person_{i:04}","entityType":"person","observations":[{{"body":"note {i}"}}]}}"#
            )
        })
        .collect();
    let body = format!(
        r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"create_entities","arguments":{{"workspaceId":"{workspace_id}","entities":[{}]}}}},"id":9}}"#,
        ents.join(",")
    );
    let (status, _, _) = request(port, "POST", "/mcp", Some(&body), Some(TEST_BEARER));
    assert_eq!(status, 200, "seed_many should succeed");
}

#[test]
fn test_ui_graph_pagination_cursor() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws = register_workspace(srv.port, Some(TEST_BEARER));
    seed_many(srv.port, 25, &ws);

    // First page: 10 of 25, more to come.
    let (_, _, body) = get(
        srv.port,
        &format!("/ui/api/graph?workspaceId={ws}&limit=10&offset=0"),
        Some(TEST_BEARER),
    );
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["entities"].as_array().unwrap().len(), 10);
    assert_eq!(v["page"]["offset"], 0);
    assert_eq!(v["page"]["returned"], 10);
    assert_eq!(v["page"]["hasMore"], true);
    assert_eq!(v["stats"]["entities"], 25);

    // Last page: offset 20 leaves 5, no more.
    let (_, _, body) = get(
        srv.port,
        &format!("/ui/api/graph?workspaceId={ws}&limit=10&offset=20"),
        Some(TEST_BEARER),
    );
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["entities"].as_array().unwrap().len(), 5);
    assert_eq!(v["page"]["hasMore"], false);
}

#[test]
fn test_ui_search_paginated_nodes_only() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws = register_workspace(srv.port, Some(TEST_BEARER));
    seed_many(srv.port, 25, &ws);

    let (status, headers, body) = get(
        srv.port,
        &format!("/ui/api/search?workspaceId={ws}&q=person&limit=10&offset=0"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 200, "search should succeed: {body}");
    assert!(
        headers
            .to_lowercase()
            .contains("content-type: application/json"),
        "search data must be JSON, headers: {headers}"
    );
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["entities"].as_array().unwrap().len(),
        10,
        "first search page"
    );
    assert_eq!(v["page"]["hasMore"], true, "25 matches → more pages");
    // Search returns matched nodes only; the user expands for relationships.
    assert_eq!(v["relations"].as_array().unwrap().len(), 0);

    // Second page paginates the same query.
    let (_, _, body) = get(
        srv.port,
        &format!("/ui/api/search?workspaceId={ws}&q=person&limit=10&offset=20"),
        Some(TEST_BEARER),
    );
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["entities"].as_array().unwrap().len(), 5);
    assert_eq!(v["page"]["hasMore"], false);
}

#[test]
fn test_ui_search_prefix_and_permission() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws = seed_graph(srv.port, Some(TEST_BEARER)); // Alice (person), Acme (company)

    // A prefix ("Ac") matches "Acme" — search-as-you-type behaviour.
    let (status, _, body) = get(
        srv.port,
        &format!("/ui/api/search?workspaceId={ws}&q=Ac"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let names: Vec<&str> = v["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"Acme"),
        "prefix search should find Acme, got {names:?}"
    );
    drop(srv);

    // Same graph-read gate as the rest of the viewer.
    let srv = spawn_http_server(&["--enable-graph-write"], Some(TEST_BEARER));
    let ws = register_workspace(srv.port, Some(TEST_BEARER));
    let (status, _, _) = get(
        srv.port,
        &format!("/ui/api/search?workspaceId={ws}&q=x"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 403, "search must require graph-read");
}

#[test]
fn test_ui_graph_omits_observation_bodies() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws = seed_graph(srv.port, Some(TEST_BEARER)); // Alice has one observation ("likes hiking")

    let (status, _, body) = get(
        srv.port,
        &format!("/ui/api/graph?workspaceId={ws}"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 200, "graph should succeed: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let alice = v["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Alice")
        .expect("Alice present");
    // The list payload carries a count, not the bodies — those lazy-load via /ui/api/node.
    assert_eq!(
        alice["obsCount"], 1,
        "Alice's observation count should be present"
    );
    assert!(
        alice.get("observations").is_none(),
        "list payload must omit observation bodies: {alice}"
    );
}

#[test]
fn test_ui_node_lazy_loads_observations() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws = seed_graph(srv.port, Some(TEST_BEARER));

    let (status, headers, body) = get(
        srv.port,
        &format!("/ui/api/node?workspaceId={ws}&name=Alice"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 200, "node fetch should succeed: {body}");
    assert!(
        headers
            .to_lowercase()
            .contains("content-type: application/json"),
        "node data must be JSON, headers: {headers}"
    );
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["name"], "Alice");
    assert_eq!(v["entityType"], "person");
    // The UI consumes the canonical structured read model, independently of
    // the MCP legacy adapter.
    let obs: Vec<&str> = v["observations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["body"].as_str().unwrap())
        .collect();
    assert_eq!(
        obs,
        vec!["likes hiking"],
        "node fetch should return observation bodies"
    );

    // Unknown entity → 404.
    let (status, _, _) = get(
        srv.port,
        &format!("/ui/api/node?workspaceId={ws}&name=DoesNotExist"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 404, "unknown entity should be 404");
}

#[test]
fn test_ui_node_requires_graph_read() {
    let srv = spawn_http_server(&["--enable-graph-write"], Some(TEST_BEARER));
    let ws = register_workspace(srv.port, Some(TEST_BEARER));
    let (status, _, _) = get(
        srv.port,
        &format!("/ui/api/node?workspaceId={ws}&name=Alice"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 403, "graph-read disabled must forbid /ui/api/node");
}

/// A filename belongs to one entity in one workspace. A different workspace
/// must never show the first workspace's file, even with the same entity name.
#[tokio::test]
async fn attachment_inspector_routes_keep_files_in_the_selected_workspace() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let first = seed_graph(srv.port, Some(TEST_BEARER));
    let other = register_workspace(srv.port, Some(TEST_BEARER));
    let create = format!(
        r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"create_entities","arguments":{{"workspaceId":"{other}","entities":[{{"name":"Alice","entityType":"person","observations":[]}}]}}}},"id":4}}"#
    );
    let (status, _, body) = request(srv.port, "POST", "/mcp", Some(&create), Some(TEST_BEARER));
    assert_eq!(status, 200, "seed second workspace: {body}");
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{}/ui/api/attachments", srv.port);
    let file = "notes for Alice\nαβγ\n";
    let first_url = format!("{base}?workspaceId={first}&entityName=Alice&filename=notes.txt");

    let uploaded = client
        .post(&first_url)
        .bearer_auth(TEST_BEARER)
        .header(reqwest::header::CONTENT_TYPE, "text/plain")
        .body(file)
        .send()
        .await
        .expect("upload request");
    let status = uploaded.status();
    assert!(status.is_success(), "raw file upload: {status}");
    let upload: serde_json::Value = uploaded.json().await.unwrap();
    let id = upload["attachmentId"].as_i64().expect("attachmentId");
    assert_eq!(upload["status"], "uploaded");

    let duplicate = client
        .post(&first_url)
        .bearer_auth(TEST_BEARER)
        .header(reqwest::header::CONTENT_TYPE, "text/plain")
        .body("different content")
        .send()
        .await
        .unwrap();
    assert_eq!(
        duplicate.status(),
        reqwest::StatusCode::CONFLICT,
        "a duplicate filename must not replace an existing attachment"
    );

    let list = |ws: &str, name: &str| {
        format!(
            "{base}?workspaceId={ws}&entityName={}",
            url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>()
        )
    };
    let first_list: serde_json::Value = client
        .get(list(&first, "Alice"))
        .bearer_auth(TEST_BEARER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first_list["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(first_list["attachments"][0]["attachmentId"], id);
    assert_eq!(first_list["attachments"][0]["filename"], "notes.txt");
    assert_eq!(first_list["attachments"][0]["sizeBytes"], file.len());
    assert_eq!(first_list["attachments"][0]["status"], "uploaded");
    assert_eq!(first_list["attachments"][0]["revision"], 1);
    assert_eq!(first_list["attachments"][0]["pageCount"], 0);
    assert!(first_list["attachments"][0]["errorStage"].is_null());
    assert!(first_list["attachments"][0]["lastError"].is_null());

    let empty_other: serde_json::Value = client
        .get(list(&other, "Alice"))
        .bearer_auth(TEST_BEARER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(empty_other["attachments"], serde_json::json!([]));
    let empty_entity: serde_json::Value = client
        .get(list(&first, "Acme"))
        .bearer_auth(TEST_BEARER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(empty_entity["attachments"], serde_json::json!([]));

    let metadata_url = format!("{base}/{id}?workspaceId={first}");
    let metadata: serde_json::Value = client
        .get(&metadata_url)
        .bearer_auth(TEST_BEARER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metadata["entityName"], "Alice");
    assert_eq!(metadata["filename"], "notes.txt");
    assert_eq!(metadata["attachmentId"], id);
    assert!(
        metadata.get("content").is_none(),
        "metadata excludes the blob"
    );

    let downloaded = client
        .get(format!("{base}/{id}/download?workspaceId={first}"))
        .bearer_auth(TEST_BEARER)
        .send()
        .await
        .unwrap();
    assert_eq!(downloaded.status(), reqwest::StatusCode::OK);
    assert_eq!(
        downloaded.bytes().await.unwrap().as_ref(),
        file.as_bytes(),
        "the browser downloads the original raw bytes"
    );
    let wrong_workspace = client
        .get(format!("{base}/{id}/download?workspaceId={other}"))
        .bearer_auth(TEST_BEARER)
        .send()
        .await
        .unwrap();
    assert_ne!(
        wrong_workspace.status(),
        reqwest::StatusCode::OK,
        "an id from another graph must not download the original file"
    );

    let deleted = client
        .delete(&metadata_url)
        .bearer_auth(TEST_BEARER)
        .send()
        .await
        .unwrap();
    assert!(
        deleted.status().is_success(),
        "delete: {}",
        deleted.text().await.unwrap()
    );
    let after: serde_json::Value = client
        .get(list(&first, "Alice"))
        .bearer_auth(TEST_BEARER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["attachments"], serde_json::json!([]));
}

/// The page endpoint returns Unicode-scalar offsets, not byte offsets.
/// A local extractor makes the text page available without an OCR provider.
#[cfg(feature = "extractor")]
#[tokio::test]
async fn attachment_inspector_reads_extracted_page_by_character_offset() {
    let srv =
        spawn_http_server_with_role(&["--enable-all"], Some(TEST_BEARER), Some("mcp,extractor"));
    let ws = seed_graph(srv.port, Some(TEST_BEARER));
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{}/ui/api/attachments", srv.port);
    let uploaded = client
        .post(format!(
            "{base}?workspaceId={ws}&entityName=Alice&filename=greek.txt"
        ))
        .bearer_auth(TEST_BEARER)
        .header(reqwest::header::CONTENT_TYPE, "text/plain")
        .body("αβγδεζ")
        .send()
        .await
        .unwrap();
    assert_eq!(uploaded.status(), reqwest::StatusCode::CREATED);
    let payload: serde_json::Value = uploaded.json().await.unwrap();
    let id = payload["attachmentId"].as_i64().expect("attachmentId");
    let list_url = format!("{base}?workspaceId={ws}&entityName=Alice");

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let metadata: serde_json::Value = client
            .get(&list_url)
            .bearer_auth(TEST_BEARER)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let row = &metadata["attachments"][0];
        if row["status"] == "ready" {
            assert_eq!(row["pageCount"], 1);
            assert!(row["lastError"].is_null());
            break;
        }
        assert_ne!(row["status"], "error", "text extraction failed: {metadata}");
        assert!(
            Instant::now() < deadline,
            "text extraction timed out: {metadata}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let page_url = format!("{base}/{id}/pages?workspaceId={ws}&page=1");
    let first: serde_json::Value = client
        .get(format!("{page_url}&offset=1&maxChars=2"))
        .bearer_auth(TEST_BEARER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["page"], 1);
    assert_eq!(first["offset"], 1);
    assert_eq!(first["text"], "βγ");
    assert_eq!(first["nextOffset"], 3);
    assert_eq!(first["eof"], false);

    let second: serde_json::Value = client
        .get(format!("{page_url}&offset=3&maxChars=4096"))
        .bearer_auth(TEST_BEARER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(second["text"], "δεζ");
    assert_eq!(second["nextOffset"], 6);
    assert_eq!(second["eof"], true);
}

/// A zero-byte text file is a valid upload: the browser produces a body of
/// exactly zero bytes for an empty file, and the repository stores
/// `expectedBytes == 0` the same way the MCP contract does (zero segments,
/// no vector work).
#[tokio::test]
async fn attachment_inspector_accepts_a_zero_byte_text_file() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws = seed_graph(srv.port, Some(TEST_BEARER));
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{}/ui/api/attachments", srv.port);

    let uploaded = client
        .post(format!(
            "{base}?workspaceId={ws}&entityName=Alice&filename=empty.txt"
        ))
        .bearer_auth(TEST_BEARER)
        .header(reqwest::header::CONTENT_TYPE, "text/plain")
        .body("")
        .send()
        .await
        .expect("zero-byte upload request");
    assert_eq!(
        uploaded.status(),
        reqwest::StatusCode::CREATED,
        "a zero-byte text file must upload"
    );
    let payload: serde_json::Value = uploaded.json().await.unwrap();
    let id = payload["attachmentId"].as_i64().expect("attachmentId");
    assert_eq!(payload["status"], "uploaded");

    let list_url = format!("{base}?workspaceId={ws}&entityName=Alice");
    let list: serde_json::Value = client
        .get(&list_url)
        .bearer_auth(TEST_BEARER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["attachments"][0]["attachmentId"], id);
    assert_eq!(list["attachments"][0]["sizeBytes"], 0);
    assert_eq!(list["attachments"][0]["pageCount"], 0);
    assert_eq!(list["attachments"][0]["status"], "uploaded");

    let downloaded = client
        .get(format!("{base}/{id}/download?workspaceId={ws}"))
        .bearer_auth(TEST_BEARER)
        .send()
        .await
        .unwrap();
    assert_eq!(downloaded.status(), reqwest::StatusCode::OK);
    assert_eq!(
        downloaded.bytes().await.unwrap().as_ref(),
        b"",
        "the zero-byte file downloads empty"
    );
}
