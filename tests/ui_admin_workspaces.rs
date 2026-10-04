#![cfg(feature = "oauth")]
//! The workspace and vector-stats adapters under `/ui/api/*`.
//!
//! The workspace routes keep the owner checks in the registry: an `admin`
//! scope never substitutes for workspace ownership, so an admin who does
//! not own a workspace gets the same 404 as an unknown id. The vector stats
//! route needs the `vectors` scope and a usable serving profile; a store
//! with no profile answers 503, and the payload carries only measured
//! fields.
//!
//! The in-process fixture drives the owner, admin and reader cases. The
//! measured profile state needs a real vector store, which no in-process
//! test state can build (`HttpState::for_test` disables vectors), so the
//! positive vector test spawns the built binary with `--enable-vectors`
//! and pre-seeds a serving profile, the fixture pattern of
//! `tests/ui_search_modes.rs`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use mcpmem::principals::{ADMIN_SCOPE, PrincipalEntry, human_id};
use mcpmem::workspace::{Visibility, WorkspaceRegistry};
use rusqlite::Connection;
use serde_json::{Value, json};

mod support;

/// The serving profile the seed activates: `openai-compatible`, model
/// `test-model`, `dimensions 4`. The store answers stats from this row; the
/// exact fingerprint matters only when a worker adopts a config-file
/// profile, which this test's server never does.
const PROFILE_ID: &str = "22222222-3333-4444-5555-666666666666";
const DIMS: u32 = 4;
const TEST_BEARER: &str = "ui-admin-workspaces-bearer";

/// Plant a live access token the way every minted token is stored, so the
/// bearer path validates it exactly like a walked one (the shape
/// `support::flow::plant_admin_token` uses). The provider is never involved.
fn plant(store: &mcpmem_oauth::store::Store, principal_id: &str, scopes: Vec<String>) -> String {
    let token = mcpmem_oauth::new_token();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_micros() as i64;
    store
        .put_token(
            &token,
            mcpmem_oauth::store::TokenKind::Access,
            &mcpmem_oauth::store::Grant {
                client_id: mcpmem_oauth::ADMIN_CLIENT_ID.to_owned(),
                principal: principal_id.to_owned(),
                scopes,
                resource: format!("{}/mcp", support::PUBLIC_URL),
                family: mcpmem_oauth::new_token(),
            },
            now,
            now + 60 * 60 * 1_000_000,
        )
        .expect("the store writes the planted token");
    token
}

/// One server with three stable humans: `owner` (graph-write, no admin),
/// `admin` (admin + graph-write, on top of graph-read) and `reader`
/// (graph-read only). The owner owns one private workspace; the admin owns
/// another, so the refusal tests can prove the routes answer for a real
/// owner and the 404s are ownership, not absence.
struct AdminFx {
    srv: support::Server,
    owner: String,
    admin: String,
    reader: String,
    owner_id: String,
    reader_id: String,
    workspace_id: String,
    admin_ws_id: String,
}

async fn fixture() -> AdminFx {
    let mut config = support::oauth_config("https://idp.invalid");
    config.principals = vec![
        PrincipalEntry {
            name: "adam".into(),
            iss: "https://idp.invalid".into(),
            sub: "sub-1".into(),
            label: Some("adam@example.com".into()),
            scopes: vec!["graph-read".into(), "graph-write".into()],
        },
        PrincipalEntry {
            name: "bianca".into(),
            iss: "https://idp.invalid".into(),
            sub: "sub-2".into(),
            label: Some("bianca@example.com".into()),
            scopes: vec![
                "graph-read".into(),
                "graph-write".into(),
                ADMIN_SCOPE.into(),
            ],
        },
        PrincipalEntry {
            name: "carol".into(),
            iss: "https://idp.invalid".into(),
            sub: "sub-3".into(),
            label: Some("carol@example.com".into()),
            scopes: vec!["graph-read".into()],
        },
    ];
    let srv = support::server(Some(config.clone()), support::Scopes::all(), None).await;
    let owner_id = human_id(&config.principals[0].iss, &config.principals[0].sub);
    let admin_id = human_id(&config.principals[1].iss, &config.principals[1].sub);
    let reader_id = human_id(&config.principals[2].iss, &config.principals[2].sub);
    let (owner, admin, reader) = srv.oauth().with_store(|store| {
        (
            plant(
                store,
                &owner_id,
                vec!["graph-read".to_owned(), "graph-write".to_owned()],
            ),
            plant(
                store,
                &admin_id,
                vec![
                    "graph-read".to_owned(),
                    "graph-write".to_owned(),
                    ADMIN_SCOPE.to_owned(),
                ],
            ),
            plant(store, &reader_id, vec!["graph-read".to_owned()]),
        )
    });
    let workspace_id = create_workspace(&srv, &owner, "Owner Private", "private").await;
    let admin_ws_id = create_workspace(&srv, &admin, "Admin Private", "private").await;
    AdminFx {
        srv,
        owner,
        admin,
        reader,
        owner_id,
        reader_id,
        workspace_id,
        admin_ws_id,
    }
}

/// Create one workspace through the MCP tool and return its id. The tool
/// path keeps this fixture honest: the registry and the adapter share one
/// store.
async fn create_workspace(
    srv: &support::Server,
    token: &str,
    name: &str,
    visibility: &str,
) -> String {
    let resp = mcp(
        srv,
        token,
        "create_workspace",
        json!({ "name": name, "visibility": visibility }),
    )
    .await;
    resp["result"]["workspace"]["workspaceId"]
        .as_str()
        .expect("the created workspace carries its id")
        .to_owned()
}

/// One `tools/call` as `token`, returning the JSON-RPC envelope.
async fn mcp(srv: &support::Server, token: &str, name: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
    .to_string();
    let res = srv
        .request(
            Request::post("/mcp")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK, "{name} must dispatch");
    support::json(res).await
}

/// One JSON request to a `/ui/api` route; return (status, parsed body).
async fn ui_call(
    srv: &support::Server,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (StatusCode, Value) {
    let builder = match method {
        "get" => Request::get(path),
        "post" => Request::post(path),
        "patch" => Request::patch(path),
        "delete" => Request::delete(path),
        other => panic!("unsupported method {other}"),
    };
    let builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    let builder = match body {
        Some(text) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(text.to_owned())),
        None => builder.body(Body::empty()),
    };
    let res = srv.request(builder.unwrap()).await;
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// A workspace owner without the admin scope lists, grants, revokes and
/// flips visibility on their own workspace. The token holds graph-read and
/// graph-write only: the routes must never ask for `admin`.
#[tokio::test]
async fn a_workspace_owner_manages_grants_and_visibility_without_admin_scope() {
    let fx = fixture().await;

    let (status, body) = ui_call(
        &fx.srv,
        &fx.owner,
        "get",
        &format!("/ui/api/workspaces/{}", fx.workspace_id),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the owner reads the workspace: {body}"
    );
    assert_eq!(body["workspace"]["workspaceId"], fx.workspace_id);
    assert_eq!(body["workspace"]["role"], "owner");

    // Visibility: private → public → private, each answer echoing the new
    // state, and a refused value is a bad request.
    let (status, body) = ui_call(
        &fx.srv,
        &fx.owner,
        "patch",
        &format!("/ui/api/workspaces/{}", fx.workspace_id),
        Some(r#"{"visibility":"public"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the owner flips visibility: {body}");
    assert_eq!(body["workspace"]["visibility"], "public");

    let (status, body) = ui_call(
        &fx.srv,
        &fx.owner,
        "patch",
        &format!("/ui/api/workspaces/{}", fx.workspace_id),
        Some(r#"{"visibility":"shared"}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "only private or public is accepted: {body}"
    );

    let (status, body) = ui_call(
        &fx.srv,
        &fx.owner,
        "patch",
        &format!("/ui/api/workspaces/{}", fx.workspace_id),
        Some(r#"{"visibility":"private"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the toggle goes back: {body}");
    assert_eq!(body["workspace"]["visibility"], "private");

    // Grants: an empty list, then one reader grant, then its revocation.
    let (status, body) = ui_call(
        &fx.srv,
        &fx.owner,
        "get",
        &format!("/ui/api/workspaces/{}/grants", fx.workspace_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the owner lists grants: {body}");
    assert_eq!(body["grants"].as_array().map(Vec::len), Some(0));

    let (status, body) = ui_call(
        &fx.srv,
        &fx.owner,
        "post",
        &format!("/ui/api/workspaces/{}/grants", fx.workspace_id),
        Some(&format!(
            r#"{{"principalId":"{}","role":"reader"}}"#,
            fx.reader_id
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "the owner grants: {body}");
    assert_eq!(body["grant"]["principalId"], fx.reader_id);
    assert_eq!(body["grant"]["role"], "reader");

    let (status, body) = ui_call(
        &fx.srv,
        &fx.owner,
        "get",
        &format!("/ui/api/workspaces/{}/grants", fx.workspace_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the grant is listed: {body}");
    let grants = body["grants"].as_array().expect("grants is an array");
    assert_eq!(grants.len(), 1, "exactly one grant: {body}");
    assert_eq!(grants[0]["principalId"], fx.reader_id);
    assert_eq!(grants[0]["role"], "reader");

    let (status, body) = ui_call(
        &fx.srv,
        &fx.owner,
        "delete",
        &format!(
            "/ui/api/workspaces/{}/grants/{}",
            fx.workspace_id, fx.reader_id
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the owner revokes: {body}");
    assert_eq!(body["revoked"], true);

    let (status, body) = ui_call(
        &fx.srv,
        &fx.owner,
        "get",
        &format!("/ui/api/workspaces/{}/grants", fx.workspace_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the list empties: {body}");
    assert_eq!(body["grants"].as_array().map(Vec::len), Some(0));
}

/// An admin who does not own a workspace gets the same 404 as an unknown
/// id on every ownership-gated route, and cannot read the private
/// workspace at all: the admin scope never substitutes for ownership.
#[tokio::test]
async fn admin_scope_does_not_substitute_for_workspace_ownership() {
    let fx = fixture().await;

    for (method, path, body) in [
        (
            "get",
            format!("/ui/api/workspaces/{}", fx.workspace_id),
            None,
        ),
        (
            "get",
            format!("/ui/api/workspaces/{}/grants", fx.workspace_id),
            None,
        ),
        (
            "post",
            format!("/ui/api/workspaces/{}/grants", fx.workspace_id),
            Some(format!(
                r#"{{"principalId":"{}","role":"reader"}}"#,
                fx.reader_id
            )),
        ),
        (
            "delete",
            format!(
                "/ui/api/workspaces/{}/grants/{}",
                fx.workspace_id, fx.reader_id
            ),
            None,
        ),
        (
            "patch",
            format!("/ui/api/workspaces/{}", fx.workspace_id),
            Some(r#"{"visibility":"public"}"#.to_owned()),
        ),
    ] {
        let (status, body) = ui_call(&fx.srv, &fx.admin, method, &path, body.as_deref()).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "admin without ownership is not-found on {method} {path}: {body}"
        );
    }

    // The same routes answer for the admin's own workspace, so the 404s
    // above are ownership refusals, not missing routes.
    let (status, body) = ui_call(
        &fx.srv,
        &fx.admin,
        "get",
        &format!("/ui/api/workspaces/{}/grants", fx.admin_ws_id),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the admin owns the other workspace: {body}"
    );
    assert_eq!(body["grants"].as_array().map(Vec::len), Some(0));
}

/// A reader who can read a workspace (through a grant) still cannot list
/// its grants: the list needs the owner, and the refusal is the same 404.
/// A grant write also needs the graph-write scope, refused before any
/// workspace lookup.
#[tokio::test]
async fn a_non_owner_reader_cannot_list_grants_of_a_workspace_they_can_read() {
    let fx = fixture().await;

    let grant = mcp(
        &fx.srv,
        &fx.owner,
        "grant_workspace_access",
        json!({
            "workspaceId": fx.workspace_id.clone(),
            "principalId": fx.reader_id.clone(),
            "role": "reader",
        }),
    )
    .await;
    assert_eq!(grant["result"]["grant"]["role"], "reader", "{grant}");

    let (status, body) = ui_call(
        &fx.srv,
        &fx.reader,
        "get",
        &format!("/ui/api/workspaces/{}", fx.workspace_id),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the reader reads the workspace: {body}"
    );

    let (status, body) = ui_call(
        &fx.srv,
        &fx.reader,
        "get",
        &format!("/ui/api/workspaces/{}/grants", fx.workspace_id),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "grants are owner-only, and the refusal hides the workspace: {body}"
    );

    // Missing graph-write scope is a 403 before the ownership check, the
    // ordered challenge the design pins for every `/ui/api` route.
    let (status, body) = ui_call(
        &fx.srv,
        &fx.reader,
        "post",
        &format!("/ui/api/workspaces/{}/grants", fx.workspace_id),
        Some(&format!(
            r#"{{"principalId":"{}","role":"reader"}}"#,
            fx.reader_id
        )),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the reader lacks the graph-write scope: {body}"
    );
}

/// Creating a workspace with a name the caller already owns answers 409;
/// a name the caller does not own yet creates a private workspace they own.
#[tokio::test]
async fn create_workspace_with_a_duplicate_name_conflicts() {
    let fx = fixture().await;

    let (status, body) = ui_call(
        &fx.srv,
        &fx.owner,
        "post",
        "/ui/api/workspaces",
        Some(r#"{"name":"Dup"}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the first create lands: {body}"
    );
    assert_eq!(body["workspace"]["name"], "Dup");
    assert_eq!(body["workspace"]["visibility"], "private");
    assert_eq!(body["workspace"]["role"], "owner");

    let (status, body) = ui_call(
        &fx.srv,
        &fx.owner,
        "post",
        "/ui/api/workspaces",
        Some(r#"{"name":"Dup"}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a duplicate owner name is a conflict: {body}"
    );

    let (status, body) = ui_call(
        &fx.srv,
        &fx.owner,
        "post",
        "/ui/api/workspaces",
        Some(r#"{"name":""}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an empty name is a bad request: {body}"
    );

    let (status, body) = ui_call(
        &fx.srv,
        &fx.reader,
        "post",
        "/ui/api/workspaces",
        Some(r#"{"name":"Dup"}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "creation needs the graph-write scope: {body}"
    );
}

/// Vector stats need the `vectors` scope: a token without it is refused
/// with 403, and no store exists in-process, so even a scoped token
/// answers the unavailable status.
#[tokio::test]
async fn vector_stats_need_the_vectors_scope_and_a_usable_profile() {
    let fx = fixture().await;

    let (status, body) = ui_call(
        &fx.srv,
        &fx.owner,
        "get",
        &format!("/ui/api/vectors/stats?workspaceId={}", fx.workspace_id),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the owner token holds no vectors scope: {body}"
    );

    let vec_token = fx.srv.oauth().with_store(|store| {
        plant(
            store,
            &fx.owner_id,
            vec!["graph-read".to_owned(), "vectors".to_owned()],
        )
    });
    let (status, body) = ui_call(
        &fx.srv,
        &vec_token,
        "get",
        &format!("/ui/api/vectors/stats?workspaceId={}", fx.workspace_id),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "the test store has no vector profile: {body}"
    );

    // An unknown workspace is 404, not 503: the workspace check precedes
    // the profile check.
    let (status, _) = ui_call(
        &fx.srv,
        &vec_token,
        "get",
        "/ui/api/vectors/stats?workspaceId=00000000-0000-0000-0000-000000000000",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Spawned-server vector stats ──────────────────────────────────────────

/// Mark the store's `default` registry row as serving `PROFILE_ID` and give
/// that profile one empty generation, so `VectorStore::serving_profile`
/// reports it. The shape matches the `activate_profile` helper in
/// `tests/ui_search_modes.rs`.
fn activate_profile(conn: &Connection) {
    conn.execute(
        "INSERT INTO index_profile VALUES(?1,'default',?2,?3,'Active')",
        rusqlite::params![
            PROFILE_ID,
            "ui-admin-stats-fixture",
            serde_json::to_string(&serde_json::json!({
                "id": PROFILE_ID,
                "store_key": "default",
                "provider_kind": "openai-compatible",
                "model": "test-model",
                "dimensions": DIMS,
                "representation_version": "chunks-identity+obs+relation-v2",
                "normalization": "L2",
                "distance_metric": "Cosine",
                "vector_encoding_version": "f32le-v1",
            }))
            .unwrap(),
        ],
    )
    .unwrap();
    conn.execute(
        "UPDATE index_profile_registry SET state='Active',serving_profile=?1 WHERE store_key='default'",
        [PROFILE_ID],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ann_generation(profile_id,durable_generation,published_generation) VALUES(?1,0,0)",
        [PROFILE_ID],
    )
    .unwrap();
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().unwrap().port()
}

fn remove_db_files(db_path: &str) {
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
}

/// A spawned server plus the ids of its two seeded workspaces, with file
/// cleanup on drop.
struct TestServer {
    child: Child,
    port: u16,
    db_path: String,
    log_path: String,
    config_path: String,
    /// The workspace whose graph file carries the serving profile.
    pub profiled_id: String,
    /// The workspace whose graph file carries no profile.
    pub bare_id: String,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        remove_db_files(&self.db_path);
        let _ = std::fs::remove_file(&self.log_path);
        let _ = std::fs::remove_file(&self.config_path);
    }
}

/// Spawn the built binary with graph and vector support enabled, over two
/// static-bearer workspaces: one whose graph file the seed gives a serving
/// profile, one left bare.
fn spawn_vector_server() -> TestServer {
    let port = free_port();
    let pid = std::process::id();
    let db_path = format!("/tmp/ui_admin_workspaces_{pid}_{port}.db");
    let log_path = format!("/tmp/ui_admin_workspaces_{pid}_{port}.log");
    let config_path = format!("/tmp/ui_admin_workspaces_{pid}_{port}.toml");
    remove_db_files(&db_path);
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_file(&config_path);

    let registry_path = std::path::PathBuf::from(&db_path);
    let registry =
        WorkspaceRegistry::open_with_principals(&registry_path, Some("machine:local"), &[], true)
            .expect("the seed registry opens");
    let profiled = registry
        .create(
            "machine:static",
            "profiled",
            Visibility::Private,
            |_| Ok(()),
        )
        .expect("the profiled workspace registers");
    let profiled_path = registry
        .all_paths()
        .expect("the registry lists paths")
        .into_iter()
        .find(|(id, _)| id == &profiled.workspace_id)
        .expect("the profiled workspace has a graph file")
        .1;
    let conn = Connection::open(&profiled_path).expect("the profiled graph opens");
    activate_profile(&conn);
    drop(conn);
    let bare = registry
        .create("machine:static", "bare", Visibility::Private, |_| Ok(()))
        .expect("the bare workspace registers");
    drop(registry);

    std::fs::write(&config_path, "[server]\nui = true\n").expect("write the test config");

    let bin =
        std::env::var("CARGO_BIN_EXE_mcpmem").unwrap_or_else(|_| "target/debug/mcpmem".into());
    let log = std::fs::File::create(&log_path).expect("create server log");
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
        .arg("--auth-token")
        .arg(TEST_BEARER)
        .arg("--config")
        .arg(&config_path)
        .arg("--log-level")
        .arg("info")
        .arg("--embedding-dims")
        .arg(DIMS.to_string())
        .arg("--enable-graph-read")
        .arg("--enable-graph-write")
        .arg("--enable-vectors");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    let mut child = cmd.spawn().expect("failed to spawn mcpmem");

    let bound = format!("http://127.0.0.1:{port}/mcp");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ready = std::fs::read_to_string(&log_path)
            .ok()
            .is_some_and(|out| out.contains(&bound));
        if ready && try_request(port, "/ui/api/workspaces?limit=10").is_some() {
            break;
        }
        let exit = child.try_wait().expect("poll server child");
        if exit.is_some() || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let output = std::fs::read_to_string(&log_path)
                .unwrap_or_else(|e| format!("<log unreadable: {e}>"));
            remove_db_files(&db_path);
            let _ = std::fs::remove_file(&log_path);
            let _ = std::fs::remove_file(&config_path);
            panic!("server did not start serving on 127.0.0.1:{port}: {output}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    TestServer {
        child,
        port,
        db_path,
        log_path,
        config_path,
        profiled_id: profiled.workspace_id,
        bare_id: bare.workspace_id,
    }
}

/// One GET as the static bearer; return None while the connection is
/// refused, which the startup health check expects until the child binds.
fn try_request(port: u16, path: &str) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\n");
    req.push_str("Host: 127.0.0.1\r\n");
    req.push_str("Accept: application/json\r\n");
    req.push_str(&format!("Authorization: Bearer {TEST_BEARER}\r\n"));
    req.push_str("Connection: close\r\n\r\n");
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
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Some((status, body))
}

/// With a serving profile active, the stats answer carries exactly the
/// measured fields — the counts and the profile's dimension — and no
/// invented model, status or refresh value. The profile-less workspace in
/// the same server answers 503.
#[test]
fn vector_stats_serve_only_measured_fields_with_a_serving_profile() {
    let srv = spawn_vector_server();

    let (status, body) = try_request(
        srv.port,
        &format!("/ui/api/vectors/stats?workspaceId={}", srv.profiled_id),
    )
    .expect("the server answers");
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).expect("stats payload is JSON");
    let fields = v.as_object().expect("stats payload is an object");
    assert_eq!(
        fields.len(),
        4,
        "exactly the measured fields, no invented ones: {v}"
    );
    assert_eq!(v["embeddingCount"], 0, "a fresh profile has no chunks: {v}");
    assert_eq!(v["dims"], DIMS, "the dimension is the profile's own: {v}");
    assert_eq!(v["petgraphNodes"], 0, "{v}");
    assert_eq!(v["petgraphEdges"], 0, "{v}");
    assert!(v.get("model").is_none(), "no model name is measured: {v}");
    assert!(v.get("lastRefresh").is_none(), "no refresh clock: {v}");
    assert!(v.get("status").is_none(), "no invented state: {v}");

    let (status, body) = try_request(
        srv.port,
        &format!("/ui/api/vectors/stats?workspaceId={}", srv.bare_id),
    )
    .expect("the server answers");
    assert_eq!(
        status, 503,
        "a workspace with vector support but no profile is unavailable: {body}"
    );
    assert!(
        body.contains("profile"),
        "the refusal names the missing profile: {body}"
    );
}
