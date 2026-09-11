//! HTTP-transport tests for the browser knowledge-graph viewer (`GET /ui` and
//! `GET /ui/graph`). These exercise the routes added alongside the MCP handlers:
//! the self-contained viewer shell, the JSON data endpoint it renders, and the
//! two gates on that data — the `graph-read` permission and the bearer token.
//!
//! A raw `TcpStream` is the client (no HTTP-client dependency), matching
//! `tests/vector_http.rs`.

use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

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

/// Remove everything one server put on disk: the database and its sidecars,
/// the code-index directory, and the captured output.
fn remove_server_files(db_path: &str, log_path: &str) {
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{db_path}{ext}"));
    }
    // The `code` category opens `<db>.code/<project>.code.db`, and creates that
    // directory at startup whether or not a project is ever indexed.
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

/// One spawn attempt on one port: either a server that is serving, or the
/// reason it never got there.
fn try_spawn_http_server(
    enable_args: &[&str],
    auth_token: Option<&str>,
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
        .arg("--transport")
        .arg("http")
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--log-level")
        // `info` so the child reports the address it bound; that report is the
        // only proof that the server answering on this port is ours.
        .arg("info")
        .args(enable_args)
        .stdin(Stdio::null())
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

/// Populate a tiny graph over authed/unauthed HTTP so `/ui/graph` has content.
fn seed_graph(port: u16, bearer: Option<&str>) {
    let create = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"create_entities","arguments":{"entities":[{"name":"Alice","entityType":"person","observations":[{"body":"likes hiking"}]},{"name":"Acme","entityType":"company","observations":[]}]}},"id":2}"#;
    let (status, _, _) = request(port, "POST", "/mcp", Some(create), bearer);
    assert_eq!(status, 200, "seed create_entities should succeed");

    let rel = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"create_relations","arguments":{"relations":[{"from":"Alice","to":"Acme","relationType":"works_at"}]}},"id":3}"#;
    let (status, _, _) = request(port, "POST", "/mcp", Some(rel), bearer);
    assert_eq!(status, 200, "seed create_relations should succeed");
}

#[test]
fn test_ui_shell_served_as_html() {
    let srv = spawn_http_server(&["--enable-all"], None);
    let (status, headers, body) = get(srv.port, "/ui", None);
    assert_eq!(status, 200, "GET /ui should return the viewer page");
    assert!(
        headers.to_lowercase().contains("content-type: text/html"),
        "viewer must be served as HTML, headers: {headers}"
    );
    assert!(
        body.contains("<title>"),
        "expected an HTML document: {body:.120}"
    );
    // The shell references its split CSS/JS assets by absolute path.
    assert!(
        body.contains("/ui/graph.css"),
        "shell should link the stylesheet"
    );
    assert!(
        body.contains("/ui/graph.js"),
        "shell should load the script"
    );
}

#[test]
fn test_ui_assets_served_with_content_types() {
    let srv = spawn_http_server(&["--enable-all"], None);

    let (status, headers, body) = get(srv.port, "/ui/graph.css", None);
    assert_eq!(status, 200, "GET /ui/graph.css should succeed");
    assert!(
        headers.to_lowercase().contains("content-type: text/css"),
        "CSS must be served as text/css, headers: {headers}"
    );
    assert!(!body.is_empty(), "stylesheet should not be empty");

    let (status, headers, body) = get(srv.port, "/ui/graph.js", None);
    assert_eq!(status, 200, "GET /ui/graph.js should succeed");
    assert!(
        headers.to_lowercase().contains("javascript"),
        "JS must be served with a javascript content-type, headers: {headers}"
    );
    // The script drives traversal via the /ui/expand endpoint.
    assert!(
        body.contains("/ui/expand"),
        "viewer should call /ui/expand to traverse"
    );
}

#[test]
fn test_ui_expand_returns_neighborhood() {
    let srv = spawn_http_server(&["--enable-all"], None);
    seed_graph(srv.port, None);

    // Expanding Alice must return her plus her neighbour Acme and the edge.
    let (status, headers, body) = get(srv.port, "/ui/expand?name=Alice", None);
    assert_eq!(status, 200, "GET /ui/expand should succeed: {body}");
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
    let srv = spawn_http_server(&["--enable-all"], None);
    let (status, _, _) = get(srv.port, "/ui/expand?name=DoesNotExist", None);
    assert_eq!(status, 404, "expanding a missing entity should be 404");
}

#[test]
fn test_ui_expand_requires_name_and_permission() {
    // Missing name → 400 (client error).
    let srv = spawn_http_server(&["--enable-all"], None);
    let (status, _, _) = get(srv.port, "/ui/expand", None);
    assert_eq!(status, 400, "expand without a name should be 400");
    drop(srv);

    // Read disabled → 403, same gate as /ui/graph.
    let srv = spawn_http_server(&["--enable-graph-write"], None);
    let (status, _, _) = get(srv.port, "/ui/expand?name=Alice", None);
    assert_eq!(status, 403, "graph-read disabled must forbid /ui/expand");
}

#[test]
fn test_ui_graph_returns_entities_and_relations() {
    let srv = spawn_http_server(&["--enable-all"], None);
    seed_graph(srv.port, None);

    let (status, headers, body) = get(srv.port, "/ui/graph", None);
    assert_eq!(status, 200, "GET /ui/graph should succeed: {body}");
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
    let srv = spawn_http_server(&["--enable-all"], None);
    seed_graph(srv.port, None);

    let (status, _, body) = get(srv.port, "/ui/graph?entityType=company", None);
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
    let srv = spawn_http_server(&["--enable-graph-write"], None);
    let (status, _, body) = get(srv.port, "/ui/graph", None);
    assert_eq!(status, 403, "graph-read disabled must forbid /ui/graph");
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
    let srv = spawn_http_server(&["--enable-all", "--static-bearer-scopes", "vectors"], None);
    let (status, headers, body) = get(srv.port, "/ui/graph", None);
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
    seed_graph(srv.port, Some("s3cret"));

    // No credentials → 401.
    let (status, _, _) = get(srv.port, "/ui/graph", None);
    assert_eq!(status, 401, "missing token must be rejected");

    // Wrong token via query → 401.
    let (status, _, _) = get(srv.port, "/ui/graph?token=nope", None);
    assert_eq!(status, 401, "wrong token must be rejected");

    // Correct token via the ?token= query fallback → 200.
    let (status, _, body) = get(srv.port, "/ui/graph?token=s3cret", None);
    assert_eq!(status, 200, "query-param token should be accepted: {body}");
    assert!(
        body.contains("Alice"),
        "authed graph should have data: {body}"
    );

    // Correct token via the Authorization header → 200.
    let (status, _, _) = get(srv.port, "/ui/graph", Some("s3cret"));
    assert_eq!(status, 200, "bearer header token should be accepted");

    // The shell itself carries no data, so it is reachable without a token.
    let (status, _, _) = get(srv.port, "/ui", None);
    assert_eq!(status, 200, "the /ui shell should not require auth");
}

/// Create `n` entities named `person_0000`..`person_(n-1)` in one batch.
fn seed_many(port: u16, n: usize) {
    let ents: Vec<String> = (0..n)
        .map(|i| {
            format!(
                r#"{{"name":"person_{i:04}","entityType":"person","observations":[{{"body":"note {i}"}}]}}"#
            )
        })
        .collect();
    let body = format!(
        r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"create_entities","arguments":{{"entities":[{}]}}}},"id":9}}"#,
        ents.join(",")
    );
    let (status, _, _) = request(port, "POST", "/mcp", Some(&body), None);
    assert_eq!(status, 200, "seed_many should succeed");
}

#[test]
fn test_ui_graph_pagination_cursor() {
    let srv = spawn_http_server(&["--enable-all"], None);
    seed_many(srv.port, 25);

    // First page: 10 of 25, more to come.
    let (_, _, body) = get(srv.port, "/ui/graph?limit=10&offset=0", None);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["entities"].as_array().unwrap().len(), 10);
    assert_eq!(v["page"]["offset"], 0);
    assert_eq!(v["page"]["returned"], 10);
    assert_eq!(v["page"]["hasMore"], true);
    assert_eq!(v["stats"]["entities"], 25);

    // Last page: offset 20 leaves 5, no more.
    let (_, _, body) = get(srv.port, "/ui/graph?limit=10&offset=20", None);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["entities"].as_array().unwrap().len(), 5);
    assert_eq!(v["page"]["hasMore"], false);
}

#[test]
fn test_ui_search_paginated_nodes_only() {
    let srv = spawn_http_server(&["--enable-all"], None);
    seed_many(srv.port, 25);

    let (status, headers, body) = get(srv.port, "/ui/search?q=person&limit=10&offset=0", None);
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
    let (_, _, body) = get(srv.port, "/ui/search?q=person&limit=10&offset=20", None);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["entities"].as_array().unwrap().len(), 5);
    assert_eq!(v["page"]["hasMore"], false);
}

#[test]
fn test_ui_search_prefix_and_permission() {
    let srv = spawn_http_server(&["--enable-all"], None);
    seed_graph(srv.port, None); // Alice (person), Acme (company)

    // A prefix ("Ac") matches "Acme" — search-as-you-type behaviour.
    let (status, _, body) = get(srv.port, "/ui/search?q=Ac", None);
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
    let srv = spawn_http_server(&["--enable-graph-write"], None);
    let (status, _, _) = get(srv.port, "/ui/search?q=x", None);
    assert_eq!(status, 403, "search must require graph-read");
}

#[test]
fn test_ui_graph_omits_observation_bodies() {
    let srv = spawn_http_server(&["--enable-all"], None);
    seed_graph(srv.port, None); // Alice has one observation ("likes hiking")

    let (status, _, body) = get(srv.port, "/ui/graph", None);
    assert_eq!(status, 200, "graph should succeed: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let alice = v["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Alice")
        .expect("Alice present");
    // The list payload carries a count, not the bodies — those lazy-load via /ui/node.
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
    let srv = spawn_http_server(&["--enable-all"], None);
    seed_graph(srv.port, None);

    let (status, headers, body) = get(srv.port, "/ui/node?name=Alice", None);
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
    let (status, _, _) = get(srv.port, "/ui/node?name=DoesNotExist", None);
    assert_eq!(status, 404, "unknown entity should be 404");
}

#[test]
fn test_ui_node_requires_graph_read() {
    let srv = spawn_http_server(&["--enable-graph-write"], None);
    let (status, _, _) = get(srv.port, "/ui/node?name=Alice", None);
    assert_eq!(status, 403, "graph-read disabled must forbid /ui/node");
}
