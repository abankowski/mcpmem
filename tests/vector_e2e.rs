use std::process::{Command, Stdio};
use std::sync::Once;
use std::sync::atomic::{AtomicU32, Ordering};

static DB_COUNTER: AtomicU32 = AtomicU32::new(0);
static CLEANUP: Once = Once::new();

/// Remove any orphaned test DB files left over from prior runs.
fn cleanup_orphaned_dbs() {
    CLEANUP.call_once(|| {
        if let Ok(entries) = std::fs::read_dir("/tmp") {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str())
                    && name.starts_with("vec_e2e_")
                    && (name.ends_with(".db")
                        || name.ends_with(".db-wal")
                        || name.ends_with(".db-shm"))
                {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    });
}

struct VecClient {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    db_path: String,
}

impl Drop for VecClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", self.db_path, ext));
        }
    }
}

fn spawn_vec_server() -> VecClient {
    spawn_vec_server_with(&[])
}

/// Spawn a stdio server, appending `extra` CLI args after the defaults.
fn spawn_vec_server_with(extra: &[&str]) -> VecClient {
    cleanup_orphaned_dbs();
    let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let db_path = format!("/tmp/vec_e2e_{n}.db");
    for ext in ["", "-wal", "-shm"] {
        let p = format!("{db_path}{ext}");
        let _ = std::fs::remove_file(&p);
    }

    let bin =
        std::env::var("CARGO_BIN_EXE_mcpmem").unwrap_or_else(|_| "target/debug/mcpmem".into());

    let mut cmd = Command::new(&bin);
    cmd.arg("-f")
        .arg(&db_path)
        .arg("--transport")
        .arg("stdio")
        .arg("--enable-all")
        .arg("--log-level")
        .arg("error");
    // Default to tiny embeddings unless the test picks its own dimension
    // (clap rejects a flag given twice, so this cannot be an override).
    if !extra.contains(&"--embedding-dims") {
        cmd.arg("--embedding-dims").arg("4");
    }
    cmd.args(extra);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn mcpmem");

    VecClient {
        stdin: child.stdin.take().unwrap(),
        stdout: child.stdout.take().unwrap(),
        child,
        db_path,
    }
}

impl VecClient {
    fn send(&mut self, msg: &str) {
        use std::io::Write;
        writeln!(self.stdin, "{msg}").expect("write to stdin");
        self.stdin.flush().expect("flush stdin");
    }

    fn recv(&mut self) -> String {
        use std::io::{BufRead, BufReader};
        let mut buf = String::new();
        BufReader::new(&mut self.stdout)
            .read_line(&mut buf)
            .expect("read from stdout");
        buf.trim().to_string()
    }

    fn call_tool(&mut self, name: &str, args: &serde_json::Value) -> serde_json::Value {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": args
            },
            "id": 2
        });
        let line = serde_json::to_string(&req).expect("serialize request");
        self.send(&line);
        let resp = self.recv();
        serde_json::from_str(&resp).expect("parse response")
    }

    fn tool_text(&mut self, name: &str, args: &serde_json::Value) -> String {
        let resp = self.call_tool(name, args);
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| {
                if let Some(is_err) = resp["result"]["isError"].as_bool()
                    && is_err
                {
                    panic!(
                        "Tool '{name}' returned isError: {}",
                        resp["result"]["content"][0]["text"]
                            .as_str()
                            .unwrap_or("unknown error")
                    );
                }
                panic!("expected result.content[0].text, got: {resp}")
            })
            .to_string()
    }

    fn send_raw(&mut self, raw: &str) -> String {
        self.send(raw);
        self.recv()
    }

    fn initialize(&mut self) {
        let resp = self.send_raw(
            r#"{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2025-11-25"},"id":1}"#,
        );
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert!(v.get("result").is_some(), "initialize failed: {resp}");
    }

    fn assert_tools_list(&mut self) {
        let resp = self.send_raw(r#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#);
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        let tools = v["result"]["tools"]
            .as_array()
            .expect("tools/list should return array");
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(
            names.contains(&"create_entities"),
            "missing KG tool: {names:?}"
        );
        assert!(
            names.contains(&"vector_search_entities"),
            "missing vector_search tool: {names:?}"
        );
        assert!(
            names.contains(&"hybrid_search"),
            "missing hybrid_search tool: {names:?}"
        );
        // The 2.0.0 surface: the six client ingestion tools are gone from the
        // manifest, so they must not be listed.
        for dead in [
            "vector_upsert_embedding",
            "vector_batch_upsert",
            "vector_delete_embedding",
            "vector_get_embedding",
            "vector_recommend",
            "vector_reindex",
        ] {
            assert!(
                !names.contains(&dead),
                "{dead} must be gone from tools/list: {names:?}"
            );
        }
    }
}

fn make_embedding(dims: usize, value: f64) -> Vec<f64> {
    vec![value; dims]
}

/// f32 little-endian blob bytes for `value` repeated `dims` times.
fn f32le_blob(dims: usize, value: f64) -> Vec<u8> {
    let v: f32 = value as f32;
    let bytes: Vec<u8> = v.to_le_bytes().to_vec();
    (0..dims)
        .flat_map(|_| bytes.iter().cloned())
        .collect::<Vec<_>>()
}

/// Seed one serving profile with chunk rows, then let the server's snapshot
/// refresher (250 ms poll) publish it. Entities must already exist in the
/// knowledge graph; their ids are resolved from the `entity` table.
fn seed_chunk_rows(c: &mut VecClient, entities: &[(&str, &str, &str, &[f64])]) {
    // entities: (name, entityType, chunkKind, vector)
    let conn = rusqlite::Connection::open(&c.db_path).unwrap();
    let profile = "11111111-2222-3333-4444-555555555555";
    conn.execute(
        "INSERT INTO index_profile VALUES(?1,'default',?2,?3,'Active')",
        rusqlite::params![
            profile,
            "test-fixture",
            serde_json::to_string(&serde_json::json!({
                "id": profile,
                "store_key": "default",
                "provider_kind": "test",
                "model": "test",
                "dimensions": 4,
                "representation_version": "v1",
                "normalization": "None",
                "distance_metric": "L2Squared",
                "vector_encoding_version": "f32le-v1",
            }))
            .unwrap(),
        ],
    )
    .unwrap();
    conn.execute(
        "UPDATE index_profile_registry SET state='Active',serving_profile=?1 WHERE store_key='default'",
        [profile],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ann_generation(profile_id) VALUES(?1)",
        [profile],
    )
    .unwrap();
    for (name, _etype, kind, vector) in entities.iter() {
        let entity_id: i64 = conn
            .query_row(
                "SELECT id FROM entity WHERE name=?1 AND flags=0",
                [name],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        let type_id: i64 = conn
            .query_row(
                "SELECT t.id FROM entity e JOIN type_dict t ON t.id=e.type_id WHERE e.id=?1",
                [entity_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        let chunk_index = if matches!(*kind, "observation") { 1 } else { 0 };
        conn.execute(
            "INSERT INTO chunk_vector(profile_id,kind,owner_kind,owner_id,chunk_index,type_id,owner_revision,blob,created_at_us,source)
             VALUES(?1,?2,'entity',?3,?4,?5,1,?6,1,'test')",
            rusqlite::params![
                profile,
                kind,
                entity_id,
                i64::from(chunk_index),
                type_id,
                f32le_blob(4, vector[0]),
            ],
        )
        .unwrap();
    }
    drop(conn);
}

/// Wait (up to ~3s) until the server's snapshot refresher has published the
/// seeded chunk rows, polling `vector_store_stats` for `want`.
fn wait_for_stats(c: &mut VecClient, want: &str) -> String {
    for _ in 0..30 {
        let text = c.tool_text("vector_store_stats", &serde_json::json!({}));
        if text.contains(want) {
            return text;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let text = c.tool_text("vector_store_stats", &serde_json::json!({}));
    panic!("stats never matched {want:?}: {text}")
}

fn assert_tool_error(c: &mut VecClient, name: &str, args: &serde_json::Value) {
    let resp = c.call_tool(name, args);
    let is_err = resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(is_err, "expected isError for {name}, got: {resp}");
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[test]
fn test_vector_e2e_initialize() {
    let mut c = spawn_vec_server();
    c.initialize();
}

#[test]
fn test_vector_e2e_tools_list() {
    let mut c = spawn_vec_server();
    c.initialize();
    c.assert_tools_list();
}

#[test]
fn test_vector_e2e_search_empty_store() {
    let mut c = spawn_vec_server();
    // No chunks have been published: an empty result, not an error.
    let text = c.tool_text(
        "vector_search_entities",
        &serde_json::json!({
            "embedding": make_embedding(4, 1.0),
            "topK": 5
        }),
    );
    assert!(text.contains(r#""count":0"#), "empty store: {text}");
}

#[test]
fn test_vector_e2e_store_stats() {
    let mut c = spawn_vec_server();
    let text = c.tool_text("vector_store_stats", &serde_json::json!({}));
    assert!(
        text.contains("embeddingCount"),
        "stats should show count: {text}"
    );
    assert!(text.contains("dims"), "stats should show dims: {text}");
    // 2.0.0 shape: the ANN fields are gone.
    for dead in ["indexKind", "indexCapacity", "indexMemoryBytes"] {
        assert!(
            !text.contains(dead),
            "{dead} must be gone from stats: {text}"
        );
    }
}

#[test]
fn test_vector_e2e_stats_from_chunk_snapshot() {
    let mut c = spawn_vec_server();
    c.tool_text(
        "create_entities",
        &serde_json::json!({"entities": [
            {"name": "alice", "entityType": "person", "observations": [{"body": "likes math"}]},
            {"name": "bob", "entityType": "person", "observations": []}
        ]}),
    );
    // Identity + observation for alice, identity for bob = 3 chunk rows.
    seed_chunk_rows(
        &mut c,
        &[
            ("alice", "person", "identity", &[1.0, 0.0, 0.0, 0.0]),
            ("alice", "person", "observation", &[0.9, 0.1, 0.0, 0.0]),
            ("bob", "person", "identity", &[0.0, 1.0, 0.0, 0.0]),
        ],
    );
    let text = wait_for_stats(&mut c, r#""embeddingCount":3"#);
    assert!(
        text.contains(r#""embeddingCount":3"#),
        "stats report the serving snapshot chunk rows: {text}"
    );
    assert!(text.contains(r#""dims":4"#), "stats report dims=4: {text}");
    // The graph mirror starts empty; refresh_graph_cache fills it from the
    // live entity table (the legacy vector_embedding source is gone).
    let refreshed = c.tool_text("vector_refresh_graph_cache", &serde_json::json!({}));
    assert!(
        refreshed.contains(r#""nodes":2"#),
        "graph cache covers both entities: {refreshed}"
    );
    let text = c.tool_text("vector_store_stats", &serde_json::json!({}));
    assert!(
        text.contains(r#""petgraphNodes":2"#),
        "stats report petgraphNodes=2: {text}"
    );
    assert!(
        text.contains(r#""petgraphEdges":0"#),
        "stats report petgraphEdges=0: {text}"
    );
}

#[test]
fn test_vector_e2e_search_by_entity() {
    let mut c = spawn_vec_server();
    c.tool_text(
        "create_entities",
        &serde_json::json!({"entities": [
            {"name": "a", "entityType": "doc", "observations": []},
            {"name": "b", "entityType": "doc", "observations": []}
        ]}),
    );
    seed_chunk_rows(
        &mut c,
        &[
            ("a", "doc", "identity", &[1.0, 0.0, 0.0, 0.0]),
            ("b", "doc", "identity", &[0.9, 0.1, 0.0, 0.0]),
        ],
    );
    wait_for_stats(&mut c, r#""embeddingCount":2"#);
    let text = c.tool_text(
        "vector_search_by_entity",
        &serde_json::json!({"entityName": "a", "topK": 5}),
    );
    assert!(!text.contains("\"a\""), "should exclude self: {text}");
    assert!(text.contains("\"b\""), "should find similar b: {text}");
}

#[test]
fn test_vector_e2e_hybrid_search() {
    let mut c = spawn_vec_server();
    c.tool_text(
        "create_entities",
        &serde_json::json!({"entities": [
            {"name": "Einstein", "entityType": "scientist", "observations": [{"body": "physics"}]},
            {"name": "Mozart", "entityType": "musician", "observations": [{"body": "music"}]}
        ]}),
    );
    seed_chunk_rows(
        &mut c,
        &[
            ("Einstein", "scientist", "identity", &[1.0, 0.0, 0.0, 0.0]),
            ("Mozart", "musician", "identity", &[0.0, 1.0, 0.0, 0.0]),
        ],
    );
    wait_for_stats(&mut c, r#""embeddingCount":2"#);
    let text = c.tool_text(
        "hybrid_search",
        &serde_json::json!({
            "queryText": "physics",
            "queryEmbedding": [1.0, 0.0, 0.0, 0.0],
            "textWeight": 0.5,
            "vecWeight": 0.5,
            "topK": 5
        }),
    );
    assert!(
        text.contains("Einstein"),
        "hybrid should find Einstein: {text}"
    );
    assert!(
        text.contains(r#""kind":"entity""#),
        "hybrid rows must be kind-marked: {text}"
    );
}

#[test]
fn test_vector_e2e_refresh_graph_cache() {
    let mut c = spawn_vec_server();
    c.tool_text(
        "create_entities",
        &serde_json::json!({"entities": [
            {"name": "alice", "entityType": "person", "observations": []},
            {"name": "bob", "entityType": "person", "observations": []}
        ]}),
    );
    c.tool_text(
        "create_relations",
        &serde_json::json!({"relations": [
            {"from": "alice", "to": "bob", "relationType": "knows"}
        ]}),
    );
    let text = c.tool_text("vector_refresh_graph_cache", &serde_json::json!({}));
    assert!(
        text.contains(r#""nodes":2"#),
        "refresh covers the entity table: {text}"
    );
    assert!(
        text.contains(r#""edges":1"#),
        "refresh covers relations: {text}"
    );
}

#[test]
fn test_vector_e2e_topk_clamped_on_empty_store() {
    let mut c = spawn_vec_server();
    let text = c.tool_text(
        "vector_search_entities",
        &serde_json::json!({
            "embedding": make_embedding(4, 1.0),
            "topK": 100000
        }),
    );
    assert!(
        text.contains(r#""count":0"#),
        "topK far above the cap must clamp, not error, on an empty store: {text}"
    );
    // With a seeded snapshot the clamp must still bound the result count.
    c.tool_text(
        "create_entities",
        &serde_json::json!({"entities": [
            {"name": "s0", "entityType": "doc", "observations": []},
            {"name": "s1", "entityType": "doc", "observations": []},
            {"name": "s2", "entityType": "doc", "observations": []},
            {"name": "s3", "entityType": "doc", "observations": []}
        ]}),
    );
    seed_chunk_rows(
        &mut c,
        &[
            ("s0", "doc", "identity", &[1.0, 0.0, 0.0, 0.0]),
            ("s1", "doc", "identity", &[0.8, 0.2, 0.0, 0.0]),
            ("s2", "doc", "identity", &[0.6, 0.4, 0.0, 0.0]),
            ("s3", "doc", "identity", &[0.4, 0.6, 0.0, 0.0]),
        ],
    );
    wait_for_stats(&mut c, r#""embeddingCount":4"#);
    let text = c.tool_text(
        "vector_search_entities",
        &serde_json::json!({
            "embedding": make_embedding(4, 1.0),
            "topK": 3
        }),
    );
    assert!(
        text.contains(r#""count":3"#),
        "topK=3 must truncate the seeded results: {text}"
    );
}

#[test]
fn test_vector_e2e_search_missing_embedding() {
    let mut c = spawn_vec_server();
    assert_tool_error(
        &mut c,
        "vector_search_entities",
        &serde_json::json!({"topK": 5}),
    );
}

#[test]
fn test_vector_e2e_hybrid_missing_params() {
    let mut c = spawn_vec_server();
    // Missing queryEmbedding
    assert_tool_error(
        &mut c,
        "hybrid_search",
        &serde_json::json!({"queryText": "physics"}),
    );
    // Missing queryText
    assert_tool_error(
        &mut c,
        "hybrid_search",
        &serde_json::json!({"queryEmbedding": make_embedding(4, 1.0)}),
    );
}

#[test]
fn test_vector_e2e_unknown_tool() {
    let mut c = spawn_vec_server();
    let resp = c.call_tool("vector_does_not_exist", &serde_json::json!({}));
    assert!(
        resp.get("error").is_some(),
        "unknown tool should be a protocol error: {resp}"
    );
    // The removed 2.0.0 tools are unknown, not refused.
    let resp = c.call_tool("vector_upsert_embedding", &serde_json::json!({}));
    assert!(
        resp.get("error").is_some(),
        "a removed tool must be an unknown tool: {resp}"
    );
}

#[test]
fn test_vector_e2e_kg_tools_still_work() {
    let mut c = spawn_vec_server();
    let text = c.tool_text(
        "create_entities",
        &serde_json::json!({"entities": [
            {"name": "test", "entityType": "test", "observations": [{"body": "obs"}]}
        ]}),
    );
    assert!(!text.contains("error"), "KG create should work: {text}");
    let text = c.tool_text("search_nodes", &serde_json::json!({"query": "test"}));
    assert!(text.contains("test"), "KG search should work: {text}");
    let text = c.tool_text("graph_stats", &serde_json::json!({}));
    assert!(text.contains("entities"), "KG stats should work: {text}");
}
