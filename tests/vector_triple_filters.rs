//! Task 5: `vector_triple_filters`.
//!
//! A relation result row carries a structured triple `{from, to,
//! relationType}`. The old formatted `from -> TYPE -> to` name string is
//! gone. The `filter.from` / `filter.to` / `filter.relationType` members
//! exclude a relation before ranking and before the candidate pool
//! truncation.
//!
//! The suite drives `vector_search_entities` and `vector_mmr_search` through
//! their shared handler core. `semantic_search` itself needs an embedding
//! provider, which is never installed in this binary.

use mcpmem::config::Config;
use mcpmem::server::{HttpOutcome, MCPServer, dispatch_http_body};
use mcpmem::tools::ToolCategory;
use mcpmem::vector_actions::{handle_vector_mmr_search, handle_vector_search_entities};
use mcpmem::vector_store::{VectorConfig, VectorStore};
use mcpmem::workspace::{WorkspaceAccess, WorkspaceHandles, WorkspaceRegistry};
use serde_json::Value;
use std::sync::Arc;

const DIMS: u32 = 8;
const PROFILE: &str = "11111111-2222-3333-4444-555555555555";

/// A server with the vector subsystem on, plus its registry and handle
/// cache (workspace selection resolves through them).
struct TestServer {
    vs: Arc<VectorStore>,
    registry: Arc<WorkspaceRegistry>,
    handles: Arc<WorkspaceHandles>,
}

fn vector_server(dir: &tempfile::TempDir) -> TestServer {
    let config = Config {
        memory_file_path: dir.path().join("memory.db").to_string_lossy().into_owned(),
        legacy_owner_id: Some("machine:local".into()),
        enabled_categories: vec![
            ToolCategory::GraphRead,
            ToolCategory::GraphWrite,
            ToolCategory::Vectors,
        ],
        vectors_enabled: true,
        ..Config::default()
    };
    let server = MCPServer::new(config, VectorConfig::new(DIMS)).expect("test server builds");
    TestServer {
        vs: server.vector_store().expect("vectors are enabled"),
        registry: server.workspace_registry(),
        handles: server.workspace_handles(),
    }
}

fn body_of(outcome: HttpOutcome) -> Value {
    match outcome {
        HttpOutcome::Body(v) => v,
        other => panic!("expected a body, got {other:?}"),
    }
}

fn call_tool(s: &TestServer, name: &str, arguments: &Value) -> Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
    .to_string();
    body_of(
        dispatch_http_body(
            &body,
            &mcpmem::authz::local_principal(),
            &s.registry,
            &s.handles,
        )
        .expect("the body is valid JSON"),
    )
}

/// Push one search through `vector_search_entities` and return the rows.
fn searched_rows(s: &TestServer, arguments: &Value) -> Vec<Value> {
    let text = searched_text(s, arguments);
    rows_of(&text)
}

fn searched_text(s: &TestServer, arguments: &Value) -> String {
    let record = s
        .registry
        .resolve("machine:local", None, WorkspaceAccess::Read)
        .unwrap();
    let kg = s.handles.get(&record).unwrap().kg;
    handle_vector_search_entities(&s.vs, &kg, Some(arguments), true).expect("the search succeeds")
}

fn rows_of(text: &str) -> Vec<Value> {
    let envelope: Value = serde_json::from_str(text).expect("tool response");
    let body = envelope["content"][0]["text"]
        .as_str()
        .expect("content text");
    serde_json::from_str::<Value>(body).expect("rows JSON")["results"]
        .as_array()
        .expect("results array")
        .clone()
}

/// Seed entities and relations through the real mutation tools. The
/// `type_dict` rows and the `taxonomy_relation` mirror rows then exist
/// exactly as a live graph creates them.
fn seed_graph(s: &TestServer, entities: &[(&str, &str)], relations: &[(&str, &str, &str)]) {
    let created = call_tool(
        s,
        "create_entities",
        &serde_json::json!({
            "entities": entities.iter().map(|(name, etype)| serde_json::json!({
                "name": name, "entityType": etype, "observations": []
            })).collect::<Vec<_>>(),
        }),
    );
    assert!(created["error"].is_null(), "seed entities: {created}");
    let linked = call_tool(
        s,
        "create_relations",
        &serde_json::json!({
            "relations": relations.iter().map(|(from, to, rtype)| serde_json::json!({
                "from": from, "to": to, "relationType": rtype
            })).collect::<Vec<_>>(),
        }),
    );
    assert!(linked["error"].is_null(), "seed relations: {linked}");
}

/// Register the serving profile at `DIMS`, exactly as the worker's first
/// commit would.
fn activate_test_profile(conn: &rusqlite::Connection) {
    conn.execute(
        "INSERT INTO index_profile VALUES(?1,'default',?2,?3,'Active')",
        rusqlite::params![
            PROFILE,
            "test-fixture",
            serde_json::to_string(&serde_json::json!({
                "id": PROFILE,
                "store_key": "default",
                "provider_kind": "test",
                "model": "test",
                "dimensions": DIMS,
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
        [PROFILE],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ann_generation(profile_id) VALUES(?1)",
        [PROFILE],
    )
    .unwrap();
}

/// The 8-float blob with the given leading values, L2 distances against the
/// query `[1.0, 0.0, ...]` ordered by the first value.
fn blob(values: &[f32]) -> Vec<u8> {
    let mut vec = vec![0.0f32; DIMS as usize];
    vec[..values.len()].copy_from_slice(values);
    vec.into_iter().flat_map(f32::to_le_bytes).collect()
}

/// Seed one relation chunk row for the mirror triple `<from> -> <rtype> -> <to>`.
fn seed_relation_chunk(
    conn: &rusqlite::Connection,
    from: &str,
    to: &str,
    rtype: &str,
    values: &[f32],
) {
    let relation_id: i64 = conn
        .query_row(
            "SELECT m.id FROM taxonomy_relation m
             JOIN entity f ON f.id=m.from_id AND f.name=?1
             JOIN entity t ON t.id=m.to_id AND t.name=?2
             WHERE m.deleted=0",
            rusqlite::params![from, to],
            |r| r.get(0),
        )
        .unwrap();
    let type_id: i64 = conn
        .query_row(
            "SELECT id FROM type_dict WHERE kind=1 AND name=?1",
            [rtype],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO chunk_vector(profile_id,kind,owner_kind,owner_id,chunk_index,type_id,owner_revision,blob,created_at_us,source)
         VALUES(?1,'relation','relation',?2,0,?3,1,?4,1,'test')",
        rusqlite::params![PROFILE, relation_id, type_id, blob(values)],
    )
    .unwrap();
}

/// Seed one identity chunk row for an entity.
fn seed_entity_chunk(conn: &rusqlite::Connection, name: &str, values: &[f32]) {
    let entity_id: i64 = conn
        .query_row(
            "SELECT id FROM entity WHERE name=?1 AND flags=0",
            [name],
            |r| r.get(0),
        )
        .unwrap();
    let type_id: i64 = conn
        .query_row(
            "SELECT t.id FROM entity e JOIN type_dict t ON t.id=e.type_id WHERE e.id=?1",
            [entity_id],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO chunk_vector(profile_id,kind,owner_kind,owner_id,chunk_index,type_id,owner_revision,blob,created_at_us,source)
         VALUES(?1,'identity','entity',?2,0,?3,1,?4,1,'test')",
        rusqlite::params![PROFILE, entity_id, type_id, blob(values)],
    )
    .unwrap();
}

/// 1. A relation hit carries the structured triple and no `name` member,
///    and the old formatted `from -> TYPE -> to` string is gone from the
///    row. An entity row keeps its `name` member.
#[test]
fn relation_rows_carry_structured_triples() {
    let dir = tempfile::tempdir().unwrap();
    let s = vector_server(&dir);
    seed_graph(
        &s,
        &[("ada", "Person"), ("bob", "Person")],
        &[("ada", "bob", "knows")],
    );
    let conn = rusqlite::Connection::open(dir.path().join("memory.db")).unwrap();
    activate_test_profile(&conn);
    seed_relation_chunk(&conn, "ada", "bob", "knows", &[1.0, 0.0]);
    seed_entity_chunk(&conn, "ada", &[0.9, 0.0]);
    drop(conn);
    s.vs.reconcile_managed_snapshot().unwrap();

    let rows = searched_rows(
        &s,
        &serde_json::json!({
            "embedding": vec![1.0f64; DIMS as usize],
            "topK": 10,
            "filter": { "kind": "relation" },
        }),
    );
    assert_eq!(rows.len(), 1, "one relation matches: {rows:?}");
    let row = &rows[0];
    assert_eq!(row["kind"], "relation", "{row:?}");
    assert_eq!(row["from"], "ada", "{row:?}");
    assert_eq!(row["to"], "bob", "{row:?}");
    assert_eq!(row["relationType"], "knows", "{row:?}");
    assert!(
        row.get("name").is_none(),
        "a relation row carries the triple, not a name: {row:?}"
    );
    assert!(
        !row.to_string().contains("ada -> knows -> bob"),
        "the formatted name string must be gone: {row:?}"
    );

    // The entity row keeps its `name` member in the same result set.
    let rows = searched_rows(
        &s,
        &serde_json::json!({
            "embedding": vec![1.0f64; DIMS as usize],
            "topK": 10,
        }),
    );
    assert_eq!(
        rows.len(),
        2,
        "relation and entity rows both rank: {rows:?}"
    );
    let entity = rows
        .iter()
        .find(|r| r["kind"] == "entity")
        .expect("ada ranks as an entity");
    assert_eq!(entity["name"], "ada", "{entity:?}");
    assert!(entity.get("from").is_none(), "{entity:?}");
}

/// 2. `from` / `to` / `relationType` exclude a relation before ranking.
///    Seed two relations of one type, filter `from` to one, and only that
///    relation can appear. `topK` is honored after the filter.
#[test]
fn triple_filters_exclude_relations_before_ranking() {
    let dir = tempfile::tempdir().unwrap();
    let s = vector_server(&dir);
    seed_graph(
        &s,
        &[
            ("ada", "Person"),
            ("bob", "Person"),
            ("carol", "Person"),
            ("dan", "Person"),
        ],
        &[("ada", "bob", "knows"), ("carol", "dan", "knows")],
    );
    let conn = rusqlite::Connection::open(dir.path().join("memory.db")).unwrap();
    activate_test_profile(&conn);
    // ada->bob is the nearer relation.
    seed_relation_chunk(&conn, "ada", "bob", "knows", &[1.0, 0.0]);
    seed_relation_chunk(&conn, "carol", "dan", "knows", &[0.5, 0.0]);
    drop(conn);
    s.vs.reconcile_managed_snapshot().unwrap();

    let rows = searched_rows(
        &s,
        &serde_json::json!({
            "embedding": vec![1.0f64; DIMS as usize],
            "topK": 2,
            "filter": { "kind": "relation", "from": "carol" },
        }),
    );
    assert_eq!(
        rows.len(),
        1,
        "only the carol->dan relation can appear: {rows:?}"
    );
    assert_eq!(rows[0]["from"], "carol", "{rows:?}");
    assert_eq!(rows[0]["to"], "dan", "{rows:?}");

    // `topK=1` still returns the one surviving relation.
    let rows = searched_rows(
        &s,
        &serde_json::json!({
            "embedding": vec![1.0f64; DIMS as usize],
            "topK": 1,
            "filter": { "kind": "relation", "from": "carol" },
        }),
    );
    assert_eq!(rows.len(), 1, "k is honored after the filter: {rows:?}");
    assert_eq!(rows[0]["from"], "carol", "{rows:?}");

    // A filter that matches nothing is an empty result, not an error.
    let rows = searched_rows(
        &s,
        &serde_json::json!({
            "embedding": vec![1.0f64; DIMS as usize],
            "topK": 10,
            "filter": { "kind": "relation", "from": "nobody" },
        }),
    );
    assert!(rows.is_empty(), "no match is an empty result: {rows:?}");

    // The members combine as AND: carol->bob does not exist.
    let rows = searched_rows(
        &s,
        &serde_json::json!({
            "embedding": vec![1.0f64; DIMS as usize],
            "topK": 10,
            "filter": { "kind": "relation", "from": "carol", "to": "bob", "relationType": "knows" },
        }),
    );
    assert!(rows.is_empty(), "carol->bob does not exist: {rows:?}");
}

/// 3. The filter applies before the fetch_k pool, not after. Seed more
///    candidates than fetchK and filter to one. That one must still rank.
///    Applied after the pool, the near non-matching relations would occupy
///    both slots. The matching one would never reach the MMR pool.
#[test]
fn triple_filter_applies_before_the_fetch_k_pool() {
    let dir = tempfile::tempdir().unwrap();
    let s = vector_server(&dir);
    seed_graph(
        &s,
        &[
            ("ada", "Person"),
            ("bob", "Person"),
            ("carol", "Person"),
            ("dan", "Person"),
            ("eve", "Person"),
            ("frank", "Person"),
        ],
        &[
            ("ada", "bob", "knows"),
            ("carol", "dan", "knows"),
            ("eve", "frank", "knows"),
        ],
    );
    let conn = rusqlite::Connection::open(dir.path().join("memory.db")).unwrap();
    activate_test_profile(&conn);
    seed_relation_chunk(&conn, "ada", "bob", "knows", &[1.0, 0.0]);
    seed_relation_chunk(&conn, "carol", "dan", "knows", &[0.9, 0.0]);
    // The only relation that matches the filter is the farthest one.
    seed_relation_chunk(&conn, "eve", "frank", "knows", &[0.8, 0.0]);
    drop(conn);
    s.vs.reconcile_managed_snapshot().unwrap();

    let record = s
        .registry
        .resolve("machine:local", None, WorkspaceAccess::Read)
        .unwrap();
    let kg = s.handles.get(&record).unwrap().kg;
    let query = serde_json::json!([1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    let rows = rows_of(
        &handle_vector_mmr_search(
            &s.vs,
            &kg,
            Some(&serde_json::json!({
                "embedding": query, "topK": 1, "fetchK": 2,
                "filter": { "kind": "relation", "from": "eve" },
            })),
            true,
        )
        .expect("the MMR search succeeds"),
    );
    assert_eq!(
        rows.len(),
        1,
        "the eve->frank relation must survive fetchK=2: {rows:?}"
    );
    assert_eq!(rows[0]["from"], "eve", "{rows:?}");
    assert_eq!(rows[0]["to"], "frank", "{rows:?}");
    assert_eq!(rows[0]["relationType"], "knows", "{rows:?}");
}

/// A non-string `filter.from` / `filter.to` / `filter.relationType` must
/// be refused, not silently read as "no filter". A buggy client passes a
/// number, and a wrong result would look like a correct one.
#[test]
fn triple_filter_members_must_be_strings() {
    let dir = tempfile::tempdir().unwrap();
    let s = vector_server(&dir);
    let record = s
        .registry
        .resolve("machine:local", None, WorkspaceAccess::Read)
        .unwrap();
    let kg = s.handles.get(&record).unwrap().kg;
    for key in ["from", "to", "relationType"] {
        let mut filter = serde_json::json!({ "kind": "relation" });
        filter[key] = serde_json::json!(3);
        let err = handle_vector_search_entities(
            &s.vs,
            &kg,
            Some(&serde_json::json!({
                "embedding": vec![1.0f64; DIMS as usize],
                "filter": filter,
            })),
            true,
        )
        .expect_err("a non-string triple member must be refused");
        assert!(
            err.to_string().contains(&format!("'filter.{key}'")),
            "{key}: {err}"
        );
    }
}
