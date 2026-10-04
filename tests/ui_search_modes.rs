//! `/ui/api/search`: the mode dispatcher tests.
//!
//! Every test spawns the real binary over a raw TCP stream, matching
//! `tests/ui_router.rs`. Each test seeds one private workspace owned by the
//! static bearer, then drives its graph through the search route.
//!
//! The direct, validation, scope and unavailable tests run in every build
//! with the `ui` feature. The semantic, hybrid, attachment and filter tests
//! need server-side query embedding, so they are `#[cfg(feature = "indexer")]`
//! and run with `cargo test --test ui_search_modes --features indexer`: the
//! spawned binary embeds through a fake loopback embeddings endpoint and a
//! pre-seeded serving profile, the fixture pattern of the taxonomy semantic
//! module. Without the feature those tests are compiled out and the
//! unavailable test still proves the 503 contract.

use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::{Child, Command, Stdio};
#[cfg(feature = "indexer")]
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

#[cfg(feature = "indexer")]
use parking_lot::Mutex;

use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::workspace::{Visibility, WorkspaceRegistry};
use mcpmem_core::attachments::{AttachmentLimits, AttachmentRepository};
use mcpmem_core::events::now_us;
use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};

const TEST_BEARER: &str = "ui-search-test-bearer";
const DIMS: u32 = 4;

/// The profile the config file's `[indexer]` section is expected to mint:
/// `provider_kind openai-compatible`, `model test-model`, `dimensions DIMS`,
/// `representation_version chunks-identity+obs+relation-v2`, `normalization
/// L2`, `distance_metric Cosine`, `vector_encoding_version f32le-v1`. The
/// seed activates one profile with this exact fingerprint under a chosen id,
/// so a server start with an unchanged config leaves it serving.
const PROFILE_ID: &str = "22222222-3333-4444-5555-666666666666";

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

fn graph(path: &Path) -> mcpmem::kg::GraphHandle {
    mcpmem::kg::GraphHandle::new(
        path,
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(8).unwrap(),
        1,
    )
    .unwrap()
}

/// One entity seed. `body` is an observation body; `None` seeds no
/// observation, so the FTS tests can distinguish name matches from body
/// matches.
fn entity(name: &str, entity_type: &str, body: Option<&str>) -> mcpmem::types::EntityInput {
    mcpmem::types::EntityInput {
        name: name.into(),
        entity_type: entity_type.into(),
        observations: body
            .map(|text| {
                vec![mcpmem::types::ObservationInput {
                    body: text.into(),
                    occurred_at_us: None,
                }]
            })
            .unwrap_or_default(),
        attributes: None,
    }
}

fn relation(from: &str, to: &str, rtype: &str, body: Option<&str>) -> mcpmem::types::RelationInput {
    mcpmem::types::RelationInput {
        from: from.into(),
        to: to.into(),
        relation_type: rtype.into(),
        observations: body
            .map(|text| {
                vec![mcpmem::types::ObservationInput {
                    body: text.into(),
                    occurred_at_us: None,
                }]
            })
            .unwrap_or_default(),
        attributes: None,
    }
}

fn limits() -> AttachmentLimits {
    AttachmentLimits {
        max_bytes: 52_428_800,
        workspace_byte_budget: 268_435_456,
        allow_mime: vec!["text/*".into()],
    }
}

/// One ready one-page text attachment on `entity_id`, with one chunk
/// segment. Returns the attachment id.
fn seed_attachment(path: &Path, entity_id: i64) -> i64 {
    let conn = Connection::open(path).unwrap();
    let text = "PAGE ONE exact excerpt".as_bytes();
    let id = AttachmentRepository::new(&conn)
        .store_reader(
            entity_id,
            "notes.txt",
            "text/plain",
            &mut std::io::Cursor::new(text),
            text.len() as i64,
            &Sha256::digest(text).into(),
            &limits(),
            now_us(),
        )
        .unwrap();
    conn.execute("UPDATE attachment SET status='ready' WHERE id=?1", [id])
        .unwrap();
    conn.execute(
        "INSERT INTO attachment_chunk VALUES(?1,0,1,0,'PAGE ONE exact excerpt')",
        [id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO attachment_text VALUES(?1,1,'PAGE ONE exact excerpt',23)",
        [id],
    )
    .unwrap();
    drop(conn);
    id
}

/// Seed one chunk row for the serving profile. `values` is the f32 vector.
fn seed_chunk(
    conn: &Connection,
    kind: &str,
    owner_kind: &str,
    owner_id: i64,
    type_id: i64,
    values: &[f32],
) {
    assert_eq!(values.len(), DIMS as usize, "one seed vector per dimension");
    let blob: Vec<u8> = values
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect::<Vec<_>>();
    conn.execute(
        "INSERT INTO chunk_vector(profile_id,kind,owner_kind,owner_id,chunk_index,type_id,owner_revision,blob,created_at_us,source)
         VALUES(?1,?2,?3,?4,0,?5,1,?6,1,'test')",
        rusqlite::params![
            PROFILE_ID,
            kind,
            owner_kind,
            owner_id,
            type_id,
            blob,
        ],
    )
    .unwrap();
}

/// Activate the serving profile under [`PROFILE_ID`] and publish its empty
/// generation, so the store's snapshot load at handle-open succeeds.
fn activate_profile(conn: &Connection) {
    conn.execute(
        "INSERT INTO index_profile VALUES(?1,'default',?2,?3,'Active')",
        rusqlite::params![
            PROFILE_ID,
            "ui-search-fixture",
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

fn entity_id(conn: &Connection, name: &str) -> i64 {
    conn.query_row(
        "SELECT id FROM entity WHERE name=?1 AND flags=0",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
}

fn entity_type_id(conn: &Connection, name: &str) -> i64 {
    conn.query_row(
        "SELECT id FROM type_dict WHERE kind=0 AND name=?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
}

fn relation_type_id(conn: &Connection, name: &str) -> i64 {
    conn.query_row(
        "SELECT id FROM type_dict WHERE kind=1 AND name=?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
}

fn relation_id(conn: &Connection, from: &str, to: &str) -> i64 {
    conn.query_row(
        "SELECT m.id FROM taxonomy_relation m
         JOIN entity f ON f.id=m.from_id AND f.name=?1
         JOIN entity t ON t.id=m.to_id AND t.name=?2
         WHERE m.deleted=0",
        rusqlite::params![from, to],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
}

/// The query vector the fake endpoint answers: all-ones, which L2-normalizes
/// to `[0.5; DIMS]`. A seed equal to that vector sits at distance 0; the
/// unit seed `[1.0, 0.0, ...]` sits one half-length away.
fn near() -> Vec<f32> {
    vec![0.5f32; DIMS as usize]
}

fn far() -> Vec<f32> {
    let mut v = vec![0.0f32; DIMS as usize];
    v[0] = 1.0;
    v
}

// ── Fake embeddings endpoint ─────────────────────────────────────────────

/// One loopback OpenAI-compatible embeddings endpoint, shared by every
/// spawned server in this binary. It records each request and answers
/// all-ones vectors at [`DIMS`] dimensions, so the distance ranking stays
/// fixed across runs.
#[cfg(feature = "indexer")]
struct FakeEmbeddings {
    url: String,
    state: Arc<Mutex<usize>>,
}

#[cfg(feature = "indexer")]
static FAKE: LazyLock<Arc<FakeEmbeddings>> = LazyLock::new(|| Arc::new(FakeEmbeddings::start()));

#[cfg(feature = "indexer")]
fn fake_embeddings() -> &'static FakeEmbeddings {
    FAKE.as_ref()
}

#[cfg(feature = "indexer")]
impl FakeEmbeddings {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake embeddings port");
        let addr = listener
            .local_addr()
            .expect("read back the fake embeddings port");
        let state = Arc::new(Mutex::new(0usize));
        let thread_state = Arc::clone(&state);
        let _ = std::thread::Builder::new()
            .name("ui-search-embeddings-fake".into())
            .spawn(move || {
                loop {
                    let Some((conn, _)) = listener.accept().ok() else {
                        continue;
                    };
                    let state = Arc::clone(&thread_state);
                    std::thread::spawn(move || serve_embed(conn, &state));
                }
            });
        Self {
            url: format!("http://{addr}/v1/embeddings"),
            state,
        }
    }

    /// How many embeddings requests every spawned server made.
    fn recorded(&self) -> usize {
        self.state.lock().clone()
    }
}

/// Serve one embeddings request: read the body, answer all-ones embeddings,
/// then close. The recorded count is the "the server embedded the query"
/// signal the mode tests assert on.
#[cfg(feature = "indexer")]
fn serve_embed(mut conn: TcpStream, state: &Arc<Mutex<usize>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut headers_end = None;
    loop {
        match conn.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if headers_end.is_none() {
            headers_end = buf
                .windows(4)
                .position(|window| window == b"\r\n\r\n".as_slice());
        }
        let Some(end) = headers_end else { continue };
        let head = String::from_utf8_lossy(&buf[..end]);
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("Content-Length")
                    .then(|| value.trim().parse::<usize>().ok())
            })
            .flatten()
            .unwrap_or(0);
        if buf.len() < end + 4 + content_length {
            continue;
        }
        let mut calls = state.lock();
        *calls += 1;
        drop(calls);
        let embedding = vec![1.0f64; DIMS as usize];
        let payload = serde_json::to_string(&serde_json::json!({
            "data": [{"embedding": embedding}],
        }))
        .unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{payload}",
            payload.len()
        );
        let _ = conn.write_all(response.as_bytes());
        return;
    }
}

/// The `[indexer]` section pointing at the fake endpoint, plus the key file
/// path it names. Only the indexer-gated tests pass `with_indexer = true`,
/// so the no-feature build can never reach the phantom URL.
fn indexer_toml(db_path: &str) -> (String, String) {
    #[cfg(feature = "indexer")]
    let url = fake_embeddings().url.clone();
    #[cfg(not(feature = "indexer"))]
    let url = "<<no-indexer-feature>>";
    let key_path = format!("{db_path}.key");
    std::fs::write(&key_path, b"test-key\n").expect("write the provider key file");
    let section = format!(
        "[indexer]\nprovider = \"openai-compatible\"\nmodel = \"test-model\"\n\
         dimensions = {DIMS}\nopenai-url = \"{url}\"\nopenai-api-key-file = \"{key}\"\n",
        url = url,
        key = key_path
    );
    (section, key_path)
}

// ── Spawned server harness ───────────────────────────────────────────────

/// A spawned server plus the paths its process uses, for cleanup on drop.
struct TestServer {
    child: Child,
    port: u16,
    db_path: String,
    log_path: String,
    config_path: String,
    /// The workspace id every search request names. The registry seeds it in
    /// the test process, owned by the static bearer.
    pub workspace_id: String,
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

/// Spawn the built binary over `extra` flags, with optional `[indexer]`
/// configuration, after `seed` fills the named workspace's graph file.
fn spawn(extra: &[&str], with_indexer: bool, seed: impl FnOnce(&Path)) -> TestServer {
    let port = free_port();
    let pid = std::process::id();
    let db_path = format!("/tmp/ui_search_modes_{pid}_{port}.db");
    let log_path = format!("/tmp/ui_search_modes_{pid}_{port}.log");
    let config_path = format!("/tmp/ui_search_modes_{pid}_{port}.toml");
    remove_db_files(&db_path);
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_file(&config_path);

    // One private workspace owned by the static bearer: the search route
    // resolves the bearer's explicit `workspaceId`, so the owner must be the
    // credential the request presents.
    let registry_path = std::path::PathBuf::from(&db_path);
    // The spawned server enables the static token, so the seed registry must
    // register the static bearer too (`WorkspaceRegistry::open` would not).
    let registry =
        WorkspaceRegistry::open_with_principals(&registry_path, Some("machine:local"), &[], true)
            .expect("the test registry opens");
    let created = registry
        .create(
            "machine:static",
            "search-fixture",
            Visibility::Private,
            |_| Ok(()),
        )
        .expect("the seed workspace registers");
    let graph_path = registry
        .all_paths()
        .expect("the registry lists paths")
        .into_iter()
        .find(|(id, _)| *id == created.workspace_id)
        .expect("the seed workspace has a graph file")
        .1;
    seed(&graph_path);

    let mut cfg = String::from("[server]\nui = true\n");
    if with_indexer {
        let (section, _key_path) = indexer_toml(&db_path);
        cfg.push_str(&section);
    }
    std::fs::write(&config_path, cfg).expect("write the test config");

    let bin =
        std::env::var("CARGO_BIN_EXE_mcpmem").unwrap_or_else(|_| "target/debug/mcpmem".into());
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
        .arg("--auth-token")
        .arg(TEST_BEARER)
        .arg("--config")
        .arg(&config_path)
        .arg("--log-level")
        .arg("info");
    cmd.args(extra);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    let mut child = cmd.spawn().expect("failed to spawn mcpmem");

    // The child must print the address it bound, and the router must answer
    // one request: bound is not yet serving, and a 200 proves `axum::serve`
    // is dispatching.
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
        workspace_id: created.workspace_id,
    }
}

/// Send one GET request; return None when the connection is refused, which
/// the startup health check expects until the child binds.
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
    let (_headers, body) = text
        .split_once("\r\n\r\n")
        .map(|(h, b)| (h.to_string(), b.to_string()))
        .unwrap_or_default();
    Some((status, body))
}

fn search(port: u16, ws: &str, query: &str, mode: &str, scope: &str, extra: &str) -> (u16, Value) {
    let path = format!(
        "/ui/api/search?workspaceId={0}&q={1}&mode={2}&scope={3}{4}",
        url_encode(ws),
        url_encode(query),
        mode,
        scope,
        extra,
    );
    let (status, body) = get(port, path.as_str());
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

fn get(port: u16, path: &str) -> (u16, String) {
    try_request(port, path).expect("the server answers")
}

/// URL-encode the characters the test queries send: a space becomes `%20`,
/// everything else passes through literally.
fn url_encode(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c == ' ' {
                "%20".to_string()
            } else {
                c.to_string()
            }
        })
        .collect::<String>()
}

// ── Direct mode ──────────────────────────────────────────────────────────

/// Direct nodes: the query matches the name FTS index and the envelope
/// matches the spec: `{results, count, elapsedMs}` with `kind`-marked rows.
#[cfg(feature = "ui")]
#[test]
fn direct_nodes_match_name_fts_and_the_spec_envelope() {
    let srv = spawn(
        &[
            "--enable-graph-read",
            "--enable-graph-write",
            "--enable-vectors",
        ],
        false,
        |path| {
            let kg = graph(path);
            kg.create_entities(&[
                entity("einstein", "person", Some("wrote about physics")),
                entity("acme", "company", None),
            ])
            .unwrap();
            kg.create_relations(&[relation("einstein", "acme", "works_at", None)])
                .unwrap();
        },
    );

    let (status, body) = search(
        srv.port,
        &srv.workspace_id,
        "einstein",
        "direct",
        "nodes",
        "",
    );
    assert_eq!(status, 200, "{body}");
    let results = body["results"]
        .as_array()
        .expect("results is an array: {body}");
    assert_eq!(results.len(), 1, "one name match: {body}");
    let row = &results[0];
    assert_eq!(row["kind"].as_str(), Some("entity"), "{row}");
    assert_eq!(row["name"].as_str(), Some("einstein"), "{row}");
    assert_eq!(row["entityType"].as_str(), Some("person"), "{row}");
    assert!(row.get("score").is_none(), "no invented FTS score: {row}");
    assert_eq!(
        body["count"].as_u64(),
        Some(1),
        "count is the row count: {body}"
    );
    assert!(
        body.get("elapsedMs").is_some_and(|v| v.as_u64().is_some()),
        "elapsedMs is a number: {body}"
    );
    assert!(
        body.get("entities").is_none() && body.get("relations").is_none(),
        "the envelope has no graph members: {body}"
    );
}

/// Direct relations: the query matches relation-observation FTS and the hit
/// carries the structured triple.
#[cfg(feature = "ui")]
#[test]
fn direct_relations_match_observation_fts_with_a_triple() {
    let srv = spawn(
        &[
            "--enable-graph-read",
            "--enable-graph-write",
            "--enable-vectors",
        ],
        false,
        |path| {
            let kg = graph(path);
            kg.create_entities(&[
                entity("ada", "person", None),
                entity("bob", "person", None),
                entity("carol", "person", None),
                entity("dan", "person", None),
            ])
            .unwrap();
            kg.create_relations(&[
                relation("ada", "bob", "knows", Some("met at the physics camp")),
                relation("carol", "dan", "knows", Some("piano teacher")),
            ])
            .unwrap();
        },
    );

    let (status, body) = search(
        srv.port,
        &srv.workspace_id,
        "physics",
        "direct",
        "relations",
        "",
    );
    assert_eq!(status, 200, "{body}");
    let results = body["results"]
        .as_array()
        .expect("results is an array: {body}");
    assert_eq!(results.len(), 1, "one observation match: {body}");
    let row = &results[0];
    assert_eq!(row["kind"].as_str(), Some("relation"), "{row}");
    assert_eq!(row["from"].as_str(), Some("ada"), "{row}");
    assert_eq!(row["to"].as_str(), Some("bob"), "{row}");
    assert_eq!(row["relationType"].as_str(), Some("knows"), "{row}");
    assert!(
        row.get("name").is_none(),
        "a relation row has no name member: {row}"
    );
    assert_eq!(body["count"].as_u64(), Some(1), "{body}");
}

// ── Validation and scope gates ───────────────────────────────────────────

/// An unknown mode or scope, a `k` outside 10/20/50, and a blank query are
/// 400; an absent `k` defaults to 10 and `k=20` is a legal page.
#[cfg(feature = "ui")]
#[test]
fn bad_mode_scope_and_k_are_400_and_k_defaults_to_10() {
    let srv = spawn(
        &[
            "--enable-graph-read",
            "--enable-graph-write",
            "--enable-vectors",
        ],
        false,
        |path| {
            let kg = graph(path);
            kg.create_entities(&[entity("einstein", "person", None)])
                .unwrap();
        },
    );
    let ws = &srv.workspace_id;

    let (status, body) = search(srv.port, ws, "einstein", "garbage", "nodes", "");
    assert_eq!(status, 400, "unknown mode: {body}");
    assert_eq!(body["code"].as_str(), Some("bad_request"), "{body}");

    let (status, body) = search(srv.port, ws, "einstein", "direct", "garbage", "");
    assert_eq!(status, 400, "unknown scope: {body}");
    assert_eq!(body["code"].as_str(), Some("bad_request"), "{body}");

    let (status, body) = search(srv.port, ws, "einstein", "direct", "nodes", "&k=7");
    assert_eq!(status, 400, "k outside 10/20/50: {body}");

    let (status, body) = search(srv.port, ws, "  ", "direct", "nodes", "");
    assert_eq!(status, 400, "blank query: {body}");

    let (status, body) = search(srv.port, ws, "einstein", "direct", "nodes", "&k=20");
    assert_eq!(status, 200, "k=20 is a legal page: {body}");
    assert_eq!(
        body["results"]
            .as_array()
            .expect("results is an array")
            .len(),
        1,
        "{body}"
    );
}

/// The semantic and hybrid modes need the `vectors` scope; the refusal must
/// come before any profile or provider check, and direct mode stays usable.
#[cfg(feature = "ui")]
#[test]
fn semantic_requires_the_vectors_scope() {
    // The bearer holds only graph-read and graph-write here (the static
    // bearer defaults to every category, so the scope list must be narrowed).
    let srv = spawn(
        &[
            "--enable-graph-read",
            "--enable-graph-write",
            "--static-bearer-scopes",
            "graph-read,graph-write",
        ],
        false,
        |path| {
            let kg = graph(path);
            kg.create_entities(&[entity("einstein", "person", None)])
                .unwrap();
        },
    );
    let (status, body) = search(
        srv.port,
        &srv.workspace_id,
        "einstein",
        "semantic",
        "nodes",
        "",
    );
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["code"].as_str(), Some("insufficient_scope"), "{body}");
    let (status, _) = search(
        srv.port,
        &srv.workspace_id,
        "einstein",
        "hybrid",
        "nodes",
        "",
    );
    assert_eq!(status, 403, "hybrid needs the same scope");
    let (status, _) = search(
        srv.port,
        &srv.workspace_id,
        "einstein",
        "direct",
        "nodes",
        "",
    );
    assert_eq!(
        status, 200,
        "direct mode still works without the vectors scope"
    );
}

// ── Unavailable profile ──────────────────────────────────────────────────

/// No serving profile is seeded and no `[indexer]` section configures a
/// provider, so semantic and hybrid answer 503 with `code=unavailable` and
/// direct mode keeps working.
#[cfg(feature = "ui")]
#[test]
fn unavailable_profile_serves_503_but_direct_still_works() {
    let srv = spawn(
        &[
            "--enable-graph-read",
            "--enable-graph-write",
            "--enable-vectors",
        ],
        false,
        |path| {
            let kg = graph(path);
            kg.create_entities(&[entity("einstein", "person", None)])
                .unwrap();
        },
    );
    for mode in ["semantic", "hybrid"] {
        let (status, body) = search(srv.port, &srv.workspace_id, "einstein", mode, "nodes", "");
        assert_eq!(status, 503, "{mode}: {body}");
        assert_eq!(body["code"].as_str(), Some("unavailable"), "{mode}: {body}");
    }
    let (status, body) = search(
        srv.port,
        &srv.workspace_id,
        "einstein",
        "direct",
        "nodes",
        "",
    );
    assert_eq!(
        status, 200,
        "direct mode must not depend on the profile: {body}"
    );
    assert_eq!(
        body["results"]
            .as_array()
            .expect("results is an array")
            .len(),
        1,
        "{body}"
    );
}

// ── Semantic mode (needs the indexer feature) ────────────────────────────

/// The fake provider embeds the query, and a relation hit carries the
/// structured triple with no parsed name. The nearer relation ranks first.
#[cfg(all(feature = "ui", feature = "indexer"))]
#[test]
fn semantic_embeds_the_query_and_returns_structured_relation_triples() {
    let srv = spawn(
        &[
            "--enable-graph-read",
            "--enable-graph-write",
            "--enable-vectors",
        ],
        true,
        |path| {
            let kg = graph(path);
            kg.create_entities(&[
                entity("ada", "person", None),
                entity("bob", "person", None),
                entity("carol", "person", None),
                entity("dan", "person", None),
            ])
            .unwrap();
            kg.create_relations(&[
                relation("ada", "bob", "knows", None),
                relation("carol", "dan", "knows", None),
            ])
            .unwrap();
            let conn = Connection::open(path).unwrap();
            activate_profile(&conn);
            seed_chunk(
                &conn,
                "relation",
                "relation",
                relation_id(&conn, "ada", "bob"),
                relation_type_id(&conn, "knows"),
                &near(),
            );
            seed_chunk(
                &conn,
                "relation",
                "relation",
                relation_id(&conn, "carol", "dan"),
                relation_type_id(&conn, "knows"),
                &far(),
            );
            drop(conn);
        },
    );

    let before = fake_embeddings().recorded();
    let (status, body) = search(
        srv.port,
        &srv.workspace_id,
        "physics",
        "semantic",
        "relations",
        "",
    );
    assert_eq!(status, 200, "{body}");
    assert!(
        fake_embeddings().recorded() > before,
        "the server embedded the query through the fake provider"
    );
    let results = body["results"]
        .as_array()
        .expect("results is an array: {body}");
    assert_eq!(results.len(), 2, "both relations rank: {body}");
    assert_eq!(
        results[0]["from"].as_str(),
        Some("ada"),
        "the nearer relation ranks first: {body}"
    );
    assert_eq!(results[0]["to"].as_str(), Some("bob"), "{body}");
    assert_eq!(results[0]["relationType"].as_str(), Some("knows"), "{body}");
    assert_eq!(results[0]["kind"].as_str(), Some("relation"), "{body}");
    assert!(
        results[0].get("name").is_none(),
        "no parsed name on a relation row: {body}"
    );
    assert!(
        results[0].get("textScore").is_none(),
        "pure vector rows carry no textScore: {body}"
    );
    assert!(
        results[0].get("vecScore").is_none(),
        "pure vector rows carry no vecScore: {body}"
    );
    assert_eq!(body["count"].as_u64(), Some(2), "{body}");
}

/// Hybrid fuses the FTS5 and vector rankings, so every row reports the fused
/// `score` plus the two components the fusion produced.
#[cfg(all(feature = "ui", feature = "indexer"))]
#[test]
fn hybrid_fusion_returns_text_and_vector_scores() {
    let srv = spawn(
        &[
            "--enable-graph-read",
            "--enable-graph-write",
            "--enable-vectors",
        ],
        true,
        |path| {
            let kg = graph(path);
            kg.create_entities(&[
                entity("einstein", "person", Some("wrote about physics")),
                entity("acme", "company", None),
            ])
            .unwrap();
            let conn = Connection::open(path).unwrap();
            activate_profile(&conn);
            seed_chunk(
                &conn,
                "identity",
                "entity",
                entity_id(&conn, "einstein"),
                entity_type_id(&conn, "person"),
                &near(),
            );
            seed_chunk(
                &conn,
                "identity",
                "entity",
                entity_id(&conn, "acme"),
                entity_type_id(&conn, "company"),
                &far(),
            );
            drop(conn);
        },
    );

    let (status, body) = search(
        srv.port,
        &srv.workspace_id,
        "physics",
        "hybrid",
        "nodes",
        "",
    );
    assert_eq!(status, 200, "{body}");
    let results = body["results"]
        .as_array()
        .expect("results is an array: {body}");
    assert!(
        !results.is_empty(),
        "the name match fuses into the top rows: {body}"
    );
    assert_eq!(results[0]["name"].as_str(), Some("einstein"), "{body}");
    for row in results {
        assert!(
            row.get("textScore").is_some_and(|v| v.as_f64().is_some()),
            "{row}"
        );
        assert!(
            row.get("vecScore").is_some_and(|v| v.as_f64().is_some()),
            "{row}"
        );
        assert!(
            row.get("score").is_some_and(|v| v.as_f64().is_some()),
            "{row}"
        );
    }
    // The FTS half matched einstein's observation, so its text score is
    // positive; a vector-only row reports zero rather than a missing member.
    assert!(
        results[0]["textScore"].as_f64().unwrap() > 0.0,
        "the text half contributed: {body}"
    );
    let acme = results
        .into_iter()
        .find(|row| row["name"].as_str() == Some("acme"))
        .expect("acme ranks with a vector-only score");
    assert_eq!(acme["textScore"].as_f64(), Some(0.0), "{body}");
}

/// The seed for the attachment consent tests: one entity with one ready file
/// whose chunk vector sits at distance 0.
fn seed_attachment_graph(path: &Path) {
    let kg = graph(path);
    kg.create_entities(&[entity("doc", "note", None)]).unwrap();
    drop(kg);
    let conn = Connection::open(path).unwrap();
    activate_profile(&conn);
    let file = seed_attachment(path, entity_id(&conn, "doc"));
    seed_chunk(
        &conn,
        "attachment",
        "attachment",
        file,
        entity_type_id(&conn, "note"),
        &near(),
    );
    drop(conn);
}

/// File hits need both vectors and attachments consent. With the attachments
/// scope the vector path returns the file row with the file members and no
/// parsed name; with vectors only the file is excluded before ranking and no
/// excerpt appears anywhere in the body.
#[cfg(all(feature = "ui", feature = "indexer"))]
#[test]
fn attachments_consent_gates_file_hits_before_ranking() {
    // With every scope the file hit appears, carrying the file members.
    let srv = spawn(&["--enable-all"], true, seed_attachment_graph);
    let (status, body) = search(
        srv.port,
        &srv.workspace_id,
        "physics",
        "semantic",
        "nodes",
        "",
    );
    assert_eq!(status, 200, "{body}");
    let results = body["results"]
        .as_array()
        .expect("results is an array: {body}");
    assert_eq!(results.len(), 1, "the file is the only owner: {body}");
    let row = &results[0];
    assert_eq!(row["kind"].as_str(), Some("attachment"), "{row}");
    assert_eq!(row["filename"].as_str(), Some("notes.txt"), "{row}");
    assert_eq!(row["entityName"].as_str(), Some("doc"), "{row}");
    assert!(
        row["attachmentId"].as_u64().is_some(),
        "attachmentId is a number: {row}"
    );
    assert_eq!(row["page"].as_u64(), Some(1), "{row}");
    assert_eq!(
        row["excerpt"].as_str(),
        Some("PAGE ONE exact excerpt"),
        "{row}"
    );
    assert!(
        row["score"].as_f64().is_some(),
        "the file carries its vector score: {row}"
    );
    assert!(
        row.get("name").is_none() && row.get("entityType").is_none(),
        "a file hit never echoes a parsed name: {row}"
    );

    // With vectors but no attachments scope the same index is searched and
    // the file is excluded before ranking: no row and no excerpt.
    let srv = spawn(
        &[
            "--enable-graph-read",
            "--enable-graph-write",
            "--enable-vectors",
        ],
        true,
        seed_attachment_graph,
    );
    let (status, body) = search(
        srv.port,
        &srv.workspace_id,
        "physics",
        "semantic",
        "nodes",
        "",
    );
    assert_eq!(status, 200, "{body}");
    let results = body["results"]
        .as_array()
        .expect("results is an array: {body}");
    assert!(
        results
            .iter()
            .all(|row| row["kind"].as_str() != Some("attachment")),
        "no file row may rank without attachments consent: {body}"
    );
    let text = serde_json::to_string(&body).expect("the body serializes");
    assert!(!text.contains("excerpt"), "no excerpt may leak: {text}");
}

/// From, to and relationType filters reach the vector path, which applies
/// them before ranking: a nearer non-matching relation cannot occupy the
/// rank the farther matching relation takes.
#[cfg(all(feature = "ui", feature = "indexer"))]
#[test]
fn relation_filters_apply_before_ranking() {
    let srv = spawn(
        &[
            "--enable-graph-read",
            "--enable-graph-write",
            "--enable-vectors",
        ],
        true,
        |path| {
            let kg = graph(path);
            kg.create_entities(&[
                entity("ada", "person", None),
                entity("bob", "person", None),
                entity("carol", "person", None),
                entity("dan", "person", None),
                entity("eve", "person", None),
                entity("abe", "person", None),
            ])
            .unwrap();
            kg.create_relations(&[
                relation("ada", "bob", "knows", None),
                relation("carol", "dan", "knows", None),
                relation("eve", "abe", "mentors", None),
            ])
            .unwrap();
            let conn = Connection::open(path).unwrap();
            activate_profile(&conn);
            let knows = relation_type_id(&conn, "knows");
            let mentors = relation_type_id(&conn, "mentors");
            seed_chunk(
                &conn,
                "relation",
                "relation",
                relation_id(&conn, "ada", "bob"),
                knows,
                &near(),
            );
            seed_chunk(
                &conn,
                "relation",
                "relation",
                relation_id(&conn, "carol", "dan"),
                knows,
                &far(),
            );
            seed_chunk(
                &conn,
                "relation",
                "relation",
                relation_id(&conn, "eve", "abe"),
                mentors,
                &near(),
            );
            drop(conn);
        },
    );
    let ws = &srv.workspace_id;

    // No filter: all three relations rank.
    let (status, body) = search(srv.port, ws, "physics", "semantic", "relations", "");
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["results"]
            .as_array()
            .expect("results is an array")
            .len(),
        3,
        "{body}"
    );

    // from=carol: the nearer ada->bob is filtered before ranking, so the
    // farther carol->dan is the only row.
    let (status, body) = search(
        srv.port,
        ws,
        "physics",
        "semantic",
        "relations",
        "&from=carol",
    );
    assert_eq!(status, 200, "{body}");
    let results = body["results"]
        .as_array()
        .expect("results is an array: {body}");
    assert_eq!(
        results.len(),
        1,
        "the nearer relation cannot crowd out the match: {body}"
    );
    assert_eq!(results[0]["from"].as_str(), Some("carol"), "{body}");
    assert_eq!(results[0]["to"].as_str(), Some("dan"), "{body}");

    // to=bob: only ada->bob matches.
    let (_status, body) = search(srv.port, ws, "physics", "semantic", "relations", "&to=bob");
    let results = body["results"]
        .as_array()
        .expect("results is an array: {body}");
    assert_eq!(results.len(), 1, "{body}");
    assert_eq!(results[0]["from"].as_str(), Some("ada"), "{body}");

    // relationType=knows keeps both knows relations and drops mentors.
    let (_status, body) = search(
        srv.port,
        ws,
        "physics",
        "semantic",
        "relations",
        "&relationType=knows",
    );
    let results = body["results"]
        .as_array()
        .expect("results is an array: {body}");
    assert_eq!(results.len(), 2, "{body}");
    assert!(
        results
            .iter()
            .all(|row| row["relationType"].as_str() == Some("knows")),
        "{body}"
    );
}
