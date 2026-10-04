//! HTTP tests for the two graph-inspector detail routes: the type
//! catalogues (`GET /ui/api/types`) and the one-node inspect payload
//! (`GET /ui/api/node`).
//!
//! These tests spawn the real binary and drive it over a raw TCP stream,
//! matching `tests/ui_router.rs`. Every fixture registers its own workspace
//! through MCP and seeds it with an explicit `workspaceId`: the static
//! bearer owns no default workspace, and every request carries the selected
//! `workspaceId` — the shape the shipped viewer sends.
//!
//! The approved shapes come from the spec:
//!
//! - `/ui/api/types` returns `{entities:[{type,count,desc?}],
//!   relations:[{type,count,desc?}]}` from the selected workspace, and keeps
//!   a registered zero-member type listed when a description exists.
//! - `/ui/api/node` returns the `describe_entity` snapshot: observations
//!   with their stable `observationId`, degree, and every incident relation
//!   triple — never a page-derived guess.

use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const TEST_BEARER: &str = "graph-detail-test-bearer";

struct TestServer {
    child: Child,
    port: u16,
    db_path: String,
    log_path: String,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        remove_server_files(&self.db_path, &self.log_path);
    }
}

/// Remove the graph file, registry, workspace directory, and the log this
/// test server's process used.
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
/// The port is only *probably* still free by the time the child binds it, so
/// [`spawn_http_server`] retries on a fresh port when the child dies on the
/// address.
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

/// Spawn the built binary with the given `--enable-*` flags and the static
/// bearer, so every graph tool the fixtures seed over MCP is enabled.
fn spawn_http_server(enable_args: &[&str], bearer: Option<&str>) -> TestServer {
    let mut races = Vec::new();
    for attempt in 1..=SPAWN_ATTEMPTS {
        match try_spawn(enable_args, bearer) {
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
    enable_args: &[&str],
    bearer: Option<&str>,
) -> std::result::Result<TestServer, StartupFailure> {
    let port = free_port();
    let pid = std::process::id();
    let db_path = format!("/tmp/ui_graph_detail_{pid}_{port}.db");
    let log_path = format!("/tmp/ui_graph_detail_{pid}_{port}.log");
    remove_server_files(&db_path, &log_path);

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
        .args(enable_args);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    if let Some(tok) = bearer {
        cmd.arg("--auth-token").arg(tok);
    }

    let mut child = cmd.spawn().expect("failed to spawn mcpmem");

    // The child must print the address it bound, and the router must answer
    // one request: bound is not yet serving, and any HTTP status proves
    // `axum::serve` is dispatching.
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

/// One JSON-RPC `tools/call` over `/mcp`; return the parsed `result`.
///
/// The seed calls below ignore the result content; the assert on the HTTP
/// status and the result key is what turns a failed seed into a legible test
/// failure instead of a cascade of 404s.
fn mcp_call(port: u16, tool: &str, arguments: &str) {
    let body = format!(
        r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"{tool}","arguments":{arguments}}},"id":1}}"#
    );
    let (status, _, raw) = request(port, "POST", "/mcp", Some(&body), Some(TEST_BEARER));
    assert_eq!(status, 200, "seed {tool} should succeed: {raw:.160}");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("jsonrpc body");
    assert!(
        v.get("result").is_some(),
        "seed {tool} must return a result: {raw:.160}"
    );
}

/// Register one private workspace for the bearer over MCP and return its id.
fn register_workspace(port: u16) -> String {
    let create = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"create_workspace","arguments":{"name":"fixture","visibility":"private"}},"id":1}"#;
    let (status, _, body) = request(port, "POST", "/mcp", Some(create), Some(TEST_BEARER));
    assert_eq!(status, 200, "seed create_workspace should succeed: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("jsonrpc body");
    let result = v
        .get("result")
        .unwrap_or_else(|| panic!("no result in create_workspace: {body}"));
    result["workspace"]["workspaceId"]
        .as_str()
        .expect("the workspace id is returned")
        .to_owned()
}

/// Populate the inspector fixture in a fresh workspace and return its id:
/// Alice (one observation, one attribute) with exactly one incident relation
/// in each direction — the degree and triple clauses the node detail pins.
fn seed_inspector_graph(port: u16) -> String {
    let ws = register_workspace(port);
    mcp_call(
        port,
        "create_entities",
        &format!(
            r#"{{"workspaceId":"{ws}","entities":[{{"name":"Alice","entityType":"person","observations":[{{"body":"likes hiking"}}]}},{{"name":"Acme","entityType":"company","observations":[]}},{{"name":"Zed","entityType":"person","observations":[]}}]}}"#
        ),
    );
    mcp_call(
        port,
        "create_relations",
        &format!(
            r#"{{"workspaceId":"{ws}","relations":[{{"from":"Alice","to":"Acme","relationType":"works_at"}},{{"from":"Zed","to":"Alice","relationType":"reports_to"}}]}}"#
        ),
    );
    mcp_call(
        port,
        "set_attributes",
        &format!(
            r#"{{"workspaceId":"{ws}","targets":[{{"ownerKind":"entity","entityName":"Alice","attributes":{{"department":"engineering"}}}}]}}"#
        ),
    );
    ws
}

/// Populate a fresh workspace whose type catalogues cover every clause of
/// the `/ui/api/types` contract: member types with and without a stored
/// description, and described zero-member types on both sides. Returns the
/// workspace id.
fn seed_catalogues(port: u16) -> String {
    let ws = register_workspace(port);
    mcp_call(
        port,
        "create_entities",
        &format!(
            r#"{{"workspaceId":"{ws}","entities":[{{"name":"Alice","entityType":"person","observations":[]}},{{"name":"Acme","entityType":"company","observations":[]}},{{"name":"Zed","entityType":"company","observations":[]}}]}}"#
        ),
    );
    mcp_call(
        port,
        "create_relations",
        &format!(
            r#"{{"workspaceId":"{ws}","relations":[{{"from":"Alice","to":"Acme","relationType":"works_at"}},{{"from":"Acme","to":"Zed","relationType":"partners"}}]}}"#
        ),
    );
    for (kind, name, description) in [
        ("entityType", "person", "A human actor"),
        ("entityType", "plant", "A living organism"),
        ("relationType", "works_at", "Employment at a company"),
        ("relationType", "mentions", "A reference in prose"),
    ] {
        mcp_call(
            port,
            "set_type_description",
            &format!(
                r#"{{"workspaceId":"{ws}","kind":"{kind}","name":"{name}","description":"{description}"}}"#
            ),
        );
    }
    ws
}

/// The catalogue row for one type name, or `None` when the type is absent.
/// `catalogue` is the `entities` or `relations` array of a `/ui/api/types`
/// payload.
fn row_of(catalogue: &serde_json::Value, type_name: &str) -> Option<serde_json::Value> {
    catalogue
        .as_array()
        .expect("a catalogue is an array")
        .iter()
        .find(|item| item["type"].as_str() == Some(type_name))
        .cloned()
}

// ── /ui/api/types ─────────────────────────────────────────────────────────

/// Both catalogues answer with counts, stored descriptions, and the
/// registered zero-member types a description keeps listed. A type without a
/// description omits the `desc` key — the browser schema types it optional,
/// never nullable.
#[cfg(feature = "ui")]
#[test]
fn types_catalogue_reports_counts_descriptions_and_zero_member_types() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws = seed_catalogues(srv.port);

    let (status, _headers, body) = get(
        srv.port,
        &format!("/ui/api/types?workspaceId={ws}"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 200, "GET /ui/api/types should succeed: {body:.120}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("types payload is JSON");
    assert!(
        v["entities"].is_array() && v["relations"].is_array(),
        "the payload carries both catalogues: {body}"
    );

    // The entity catalogue.
    let person = row_of(&v["entities"], "person").expect("person is catalogued: {body}");
    assert_eq!(person["count"].as_i64(), Some(1), "one person: {body}");
    assert_eq!(
        person.get("desc").and_then(|d| d.as_str()),
        Some("A human actor"),
        "a described member type keeps its description: {body}"
    );
    let company = row_of(&v["entities"], "company").expect("company is catalogued: {body}");
    assert_eq!(company["count"].as_i64(), Some(2), "two companies: {body}");
    assert_eq!(
        company.get("desc"),
        None,
        "a type without a description omits the key: {body}"
    );
    let plant = row_of(&v["entities"], "plant").expect("plant is catalogued: {body}");
    assert_eq!(
        plant["count"].as_i64(),
        Some(0),
        "a described zero-member type carries count 0: {body}"
    );
    assert_eq!(
        plant.get("desc").and_then(|d| d.as_str()),
        Some("A living organism"),
        "the zero-member type keeps its description: {body}"
    );

    // The relation catalogue, the same three clauses.
    let works_at = row_of(&v["relations"], "works_at").expect("works_at is catalogued: {body}");
    assert_eq!(works_at["count"].as_i64(), Some(1), "one works_at: {body}");
    assert_eq!(
        works_at.get("desc").and_then(|d| d.as_str()),
        Some("Employment at a company"),
        "a described relation type keeps its description: {body}"
    );
    let partners = row_of(&v["relations"], "partners").expect("partners is catalogued: {body}");
    assert_eq!(partners["count"].as_i64(), Some(1), "one partners: {body}");
    assert_eq!(
        partners.get("desc"),
        None,
        "a relation type without a description omits the key: {body}"
    );
    let mentions = row_of(&v["relations"], "mentions").expect("mentions is catalogued: {body}");
    assert_eq!(
        mentions["count"].as_i64(),
        Some(0),
        "a described zero-member relation type carries count 0: {body}"
    );
}

/// The catalogue reads the selected workspace only: one workspace's types
/// never leak into another's, and an unknown workspace is the same 404 every
/// viewer data route answers.
#[cfg(feature = "ui")]
#[test]
fn types_catalogue_reads_only_the_selected_workspace() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws_a = register_workspace(srv.port);
    let ws_b = register_workspace(srv.port);
    mcp_call(
        srv.port,
        "create_entities",
        &format!(
            r#"{{"workspaceId":"{ws_a}","entities":[{{"name":"Alice","entityType":"person","observations":[]}}]}}"#
        ),
    );
    mcp_call(
        srv.port,
        "create_entities",
        &format!(
            r#"{{"workspaceId":"{ws_b}","entities":[{{"name":"Rover","entityType":"robot","observations":[]}}]}}"#
        ),
    );

    let (status, _headers, body) = get(
        srv.port,
        &format!("/ui/api/types?workspaceId={ws_a}"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 200, "GET /ui/api/types should succeed: {body:.120}");
    let a: serde_json::Value = serde_json::from_str(&body).expect("types payload is JSON");
    assert!(
        row_of(&a["entities"], "person").is_some(),
        "workspace A lists its own entity type: {body}"
    );
    assert!(
        row_of(&a["entities"], "robot").is_none(),
        "workspace A never leaks workspace B's type: {body}"
    );

    let (status, _headers, body) = get(
        srv.port,
        &format!("/ui/api/types?workspaceId={ws_b}"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 200, "GET /ui/api/types should succeed: {body:.120}");
    let b: serde_json::Value = serde_json::from_str(&body).expect("types payload is JSON");
    assert!(
        row_of(&b["entities"], "robot").is_some(),
        "workspace B lists its own entity type: {body}"
    );
    assert!(
        row_of(&b["entities"], "person").is_none(),
        "workspace B never leaks workspace A's type: {body}"
    );

    // An unknown workspace is the same 404 every viewer data route answers.
    let (status, _headers, _body) = get(
        srv.port,
        "/ui/api/types?workspaceId=00000000-0000-0000-0000-000000000000",
        Some(TEST_BEARER),
    );
    assert_eq!(status, 404, "an unknown workspace must be a 404");
}

// ── /ui/api/node ──────────────────────────────────────────────────────────

/// The node detail is the `describe_entity` snapshot: observations carry
/// their stable `observationId`, the degree counts both directions, and
/// every incident relation triple is present — the plain entity payload the
/// route used to return carries none of those.
#[cfg(feature = "ui")]
#[test]
fn node_detail_carries_degree_incident_triples_and_observation_ids() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws = seed_inspector_graph(srv.port);

    let (status, _headers, body) = get(
        srv.port,
        &format!("/ui/api/node?workspaceId={ws}&name=Alice"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 200, "GET /ui/api/node should succeed: {body:.120}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("node payload is JSON");
    assert_eq!(v["name"], "Alice", "the entity keeps its name: {body}");
    assert_eq!(
        v["entityType"], "person",
        "the entity keeps its type: {body}"
    );
    assert_eq!(
        v["attributes"]["department"], "engineering",
        "the entity keeps its attributes: {body}"
    );

    // Observations carry their stable row ids, for the keyed edits the
    // mutation schema names.
    let observations = v["observations"]
        .as_array()
        .expect("observations is an array: {body}");
    assert_eq!(observations.len(), 1, "Alice has one observation: {body}");
    assert_eq!(
        observations[0]["body"], "likes hiking",
        "the body survives: {body}"
    );
    assert!(
        observations[0]["observationId"].as_i64().is_some(),
        "an observation carries its id: {body}"
    );

    // The incident triples come from the describe_entity snapshot: one
    // outgoing and one incoming edge for Alice.
    let relations = v["relations"]
        .as_array()
        .expect("relations is an array: {body}");
    assert!(
        relations.iter().any(|r| {
            r["from"].as_str() == Some("Alice")
                && r["to"].as_str() == Some("Acme")
                && r["relationType"].as_str() == Some("works_at")
        }),
        "the outgoing triple is incident: {body}"
    );
    assert!(
        relations.iter().any(|r| {
            r["from"].as_str() == Some("Zed")
                && r["to"].as_str() == Some("Alice")
                && r["relationType"].as_str() == Some("reports_to")
        }),
        "the incoming triple is incident: {body}"
    );

    // The degree is the snapshot's, one count per direction.
    assert_eq!(
        v["degree"].get("in").and_then(|d| d.as_i64()),
        Some(1),
        "one incoming relation: {body}"
    );
    assert_eq!(
        v["degree"].get("out").and_then(|d| d.as_i64()),
        Some(1),
        "one outgoing relation: {body}"
    );

    // The neighbours complete the snapshot; the inspector draws the
    // selection ring for exactly them.
    let neighbors = v["neighbors"]
        .as_array()
        .expect("neighbors is an array: {body}");
    assert_eq!(neighbors.len(), 2, "two incident neighbours: {body}");
    assert!(
        neighbors.iter().any(|n| n.as_str() == Some("Acme")),
        "{body}"
    );
    assert!(
        neighbors.iter().any(|n| n.as_str() == Some("Zed")),
        "{body}"
    );
}

/// An absent node is a 404, the same answer every unknown graph read gives.
#[cfg(feature = "ui")]
#[test]
fn node_detail_absent_entity_is_404() {
    let srv = spawn_http_server(&["--enable-all"], Some(TEST_BEARER));
    let ws = register_workspace(srv.port);
    let (status, _headers, _body) = get(
        srv.port,
        &format!("/ui/api/node?workspaceId={ws}&name=Nobody"),
        Some(TEST_BEARER),
    );
    assert_eq!(status, 404, "an absent node must be a 404");
}
