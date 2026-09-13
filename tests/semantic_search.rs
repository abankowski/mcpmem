//! `semantic_search`: the read side of the embedding worker.
//!
//! The server embeds the query text itself, so the tool needs two things the
//! other vector tools do not: the `indexer` feature at build time, and an
//! embedding provider at run time. These tests cover what the server does when
//! one of them is missing. No test here calls a live embedding service, and no
//! test installs a provider — `indexer_provider` holds a process-wide cell, so
//! one installed provider would leak into every other test in this binary.

use mcpmem::config::Config;
use mcpmem::kg::GraphHandle;
use mcpmem::server::{HttpOutcome, MCPServer, dispatch_http_body};
use mcpmem::tools::{SEMANTIC_SEARCH, ToolCategory, category_of};
use mcpmem::vector_store::{VectorConfig, VectorStore};
use serde_json::Value;
use std::sync::Arc;

const DIMS: u32 = 8;

/// A server with the vector subsystem on. The category flags are process-wide,
/// so this goes through the same constructor `src/main.rs` uses.
fn vector_server(dir: &tempfile::TempDir) -> (Arc<GraphHandle>, Arc<VectorStore>) {
    let config = Config {
        memory_file_path: dir.path().join("memory.db").to_string_lossy().into_owned(),
        enabled_categories: vec![
            ToolCategory::GraphRead,
            ToolCategory::GraphWrite,
            ToolCategory::Vectors,
        ],
        vectors_enabled: true,
        ..Config::default()
    };
    let server = MCPServer::new(config, VectorConfig::new(DIMS)).expect("test server builds");
    let vs = server.vector_store().expect("vectors are enabled");
    (server.graph(), vs)
}

fn body_of(outcome: HttpOutcome) -> Value {
    match outcome {
        HttpOutcome::Body(v) => v,
        other => panic!("expected a body, got {other:?}"),
    }
}

/// The `content[0].text` of a tool result, whether it is an error or not.
fn result_text(value: &Value) -> String {
    value["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected tool text content, got {value}"))
        .to_owned()
}

fn call_tool(kg: &GraphHandle, vs: &VectorStore, name: &str, arguments: &Value) -> Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
    .to_string();
    body_of(
        dispatch_http_body(&body, kg, Some(vs), &mcpmem::authz::local_principal())
            .expect("the body is valid JSON"),
    )
}

fn call(kg: &GraphHandle, vs: &VectorStore, arguments: &Value) -> Value {
    call_tool(kg, vs, SEMANTIC_SEARCH, arguments)
}

fn listed_tool_names(kg: &GraphHandle, vs: &VectorStore) -> Vec<String> {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let v = body_of(
        dispatch_http_body(body, kg, Some(vs), &mcpmem::authz::local_principal())
            .expect("the body is valid JSON"),
    );
    v["result"]["tools"]
        .as_array()
        .expect("tools/list returns an array")
        .iter()
        .map(|t| {
            t["name"]
                .as_str()
                .expect("every tool has a name")
                .to_owned()
        })
        .collect()
}

/// The manifest the server compiles in.
fn manifest() -> Vec<Value> {
    serde_json::from_str(include_str!("../vector_tools.json")).expect("the manifest is valid JSON")
}

fn manifest_entry(name: &str) -> Value {
    manifest()
        .into_iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("the manifest declares no tool named {name}"))
}

/// Without this, the hiding test below would pass for the wrong reason: a
/// manifest that never declared the tool hides it just as well as the provider
/// gate does. This pins the declaration, so the hiding test proves a gate.
#[test]
fn the_manifest_declares_semantic_search() {
    let tool = manifest_entry(SEMANTIC_SEARCH);
    let schema = &tool["inputSchema"];
    assert_eq!(
        schema["required"],
        serde_json::json!(["queryText"]),
        "queryText is the only required argument: {tool}"
    );
    for key in [
        "queryText",
        "topK",
        "filter",
        "includeChunks",
        "textWeight",
        "vecWeight",
    ] {
        assert!(
            schema["properties"][key].is_object(),
            "the schema must declare {key}: {tool}"
        );
    }
    // The replaced `entityType` parameter must be gone from the manifest.
    assert!(
        schema["properties"]["entityType"].is_null(),
        "entityType is replaced by filter: {tool}"
    );
    // The filter whitelist is exactly the two owner kinds.
    assert_eq!(
        schema["properties"]["filter"]["properties"]["kind"]["enum"],
        serde_json::json!(["entity", "relation"]),
        "the filter kind enum: {tool}"
    );
    // The clamp is one constant in `vector_actions`, so the advertised cap must
    // be the cap the sibling vector tools advertise.
    assert_eq!(
        schema["properties"]["topK"]["maximum"],
        manifest_entry("vector_search_entities")["inputSchema"]["properties"]["topK"]["maximum"],
        "topK must advertise the same cap as the other vector tools"
    );
    // The description is the only place a client learns that it sends no vector
    // and that the server picks the model.
    let description = tool["description"].as_str().expect("a description");
    for phrase in ["embeds", "serving index profile", "indexer"] {
        assert!(
            description.contains(phrase),
            "the description must mention {phrase:?}: {description}"
        );
    }
}

/// The scope gate reads [`category_of`]. A name it does not know is an unknown
/// tool, not a refused one, so the name must classify on every build — with the
/// `indexer` feature and without it.
#[test]
fn semantic_search_is_always_a_vector_tool() {
    assert_eq!(category_of(SEMANTIC_SEARCH), Some(ToolCategory::Vectors));
}

/// No provider is configured in this process, so the server must not advertise
/// the tool. The other vector tools stay listed, which proves the filter hides
/// this one name and not the whole manifest.
#[test]
fn semantic_search_is_absent_from_tools_list_without_a_provider() {
    let dir = tempfile::tempdir().unwrap();
    let (kg, vs) = vector_server(&dir);
    let names = listed_tool_names(&kg, &vs);
    assert!(
        names.iter().any(|n| n == "hybrid_search"),
        "the vector tools must be listed: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == SEMANTIC_SEARCH),
        "semantic_search must be hidden without a provider: {names:?}"
    );
}

/// The provider cell is a process-wide `OnceLock`. This test runs the mismatch
/// in a child process, so its registry cannot change the no-provider tests.
#[cfg(feature = "indexer")]
const PROVIDER_KIND_MISMATCH_CHILD: &str = "MCPMEM_PROVIDER_KIND_MISMATCH_CHILD";
#[cfg(feature = "indexer")]
const PROVIDER_KIND_MATCH_CHILD: &str = "MCPMEM_PROVIDER_KIND_MATCH_CHILD";

#[cfg(feature = "indexer")]
fn activate_serving_profile(dir: &tempfile::TempDir, profile: &mcpmem_core::jobs::IndexProfile) {
    use mcpmem_core::jobs::{AnnGenerationRepository, IndexProfileRegistry};
    use rusqlite::Connection;

    let conn = Connection::open(dir.path().join("memory.db")).expect("open the test database");
    let registry = IndexProfileRegistry::new(&conn);
    let ann = AnnGenerationRepository::new(&conn);
    registry.begin_rebuild(profile).expect("start the profile");
    ann.verify_full_scan(profile.id)
        .expect("verify the empty generation");
    assert!(
        ann.mark_published(profile.id, 0)
            .expect("publish the empty generation")
    );
    registry.activate(profile.id).expect("activate the profile");
}

#[cfg(feature = "indexer")]
fn openai_profile() -> mcpmem_core::jobs::IndexProfile {
    use mcpmem_core::jobs::{DistanceMetric, IndexProfile, Normalization};
    use uuid::Uuid;

    IndexProfile {
        id: Uuid::new_v4(),
        store_key: "default".into(),
        provider_kind: "openai".into(),
        model: "test-model".into(),
        dimensions: DIMS,
        representation_version: "test-v1".into(),
        normalization: Normalization::None,
        distance_metric: DistanceMetric::Cosine,
        vector_encoding_version: "f32le-v1".into(),
    }
}

#[cfg(feature = "indexer")]
fn hide_tool_for_provider_kind_mismatch() {
    use mcpmem_indexer::{OllamaProvider, ProviderRegistry};
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("make a temporary database");
    let (kg, vs) = vector_server(&dir);
    let profile = openai_profile();
    activate_serving_profile(&dir, &profile);
    let serving = vs
        .serving_profile()
        .expect("read the serving profile")
        .expect("the profile is active");
    assert_eq!(serving.provider_kind, "openai");

    let ollama = OllamaProvider::new("http://127.0.0.1:11434", Duration::from_secs(1))
        .expect("make an Ollama provider without a request");
    mcpmem::indexer_provider::init(Arc::new(ProviderRegistry::new(
        Some(Arc::new(ollama)),
        None,
    )));

    let names = listed_tool_names(&kg, &vs);
    assert!(
        !names.iter().any(|name| name == SEMANTIC_SEARCH),
        "the registry has Ollama, but the serving profile names OpenAI: {names:?}"
    );
}

/// An available registry is not enough. The registry must hold the provider
/// the serving profile names, or the handler will fail after `tools/list`.
#[cfg(feature = "indexer")]
#[test]
fn semantic_search_is_hidden_when_registry_lacks_serving_provider() {
    if std::env::var_os(PROVIDER_KIND_MISMATCH_CHILD).is_some() {
        hide_tool_for_provider_kind_mismatch();
        return;
    }

    let status = std::process::Command::new(
        std::env::current_exe().expect("find the semantic search test binary"),
    )
    .arg("semantic_search_is_hidden_when_registry_lacks_serving_provider")
    .arg("--exact")
    .env(PROVIDER_KIND_MISMATCH_CHILD, "1")
    .status()
    .expect("run the isolated mismatch test");
    assert!(status.success(), "the isolated mismatch test must pass");
}

#[cfg(feature = "indexer")]
fn list_tool_for_matching_provider() {
    use mcpmem_indexer::{OpenAiCompatibleProvider, ProviderRegistry};
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("make a temporary database");
    let (kg, vs) = vector_server(&dir);
    let profile = openai_profile();
    activate_serving_profile(&dir, &profile);
    let openai = OpenAiCompatibleProvider::new(
        "http://127.0.0.1:8000/v1/embeddings".into(),
        "test-key".into(),
        Duration::from_secs(1),
    )
    .expect("make an OpenAI provider without a request");
    mcpmem::indexer_provider::init(Arc::new(ProviderRegistry::new(
        None,
        Some(Arc::new(openai)),
    )));

    let names = listed_tool_names(&kg, &vs);
    assert!(
        names.iter().any(|name| name == SEMANTIC_SEARCH),
        "the registry and the serving profile both name OpenAI: {names:?}"
    );
}

/// A matching registry must list the tool. This control keeps the mismatch
/// test from passing because a future gate hides semantic search everywhere.
#[cfg(feature = "indexer")]
#[test]
fn semantic_search_is_listed_when_registry_supports_serving_provider() {
    if std::env::var_os(PROVIDER_KIND_MATCH_CHILD).is_some() {
        list_tool_for_matching_provider();
        return;
    }

    let status = std::process::Command::new(
        std::env::current_exe().expect("find the semantic search test binary"),
    )
    .arg("semantic_search_is_listed_when_registry_supports_serving_provider")
    .arg("--exact")
    .env(PROVIDER_KIND_MATCH_CHILD, "1")
    .status()
    .expect("run the isolated matching-provider test");
    assert!(
        status.success(),
        "the isolated matching-provider test must pass"
    );
}

/// Hidden from `tools/list` is not enough. The name is still a known vector
/// tool, so a client that calls it anyway must be refused rather than served.
#[test]
fn a_hidden_semantic_search_call_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (kg, vs) = vector_server(&dir);
    let v = call(&kg, &vs, &serde_json::json!({ "queryText": "anything" }));
    let text = result_text(&v);
    assert!(v["error"].is_null(), "a scope refusal is not wanted: {v}");
    assert_eq!(
        v["result"]["isError"],
        Value::Bool(true),
        "an unusable tool must refuse the call: {v}"
    );
    // Without the feature the refusal names the build. With the feature and no
    // provider it names the configuration section. Both point at the indexer.
    assert!(
        text.contains("indexer"),
        "the refusal must say what is missing: {text}"
    );
}

/// No store adopts a profile in this test, so the store still serves none. The
/// message must send the operator to the configuration file, not to the client.
#[cfg(feature = "indexer")]
#[test]
fn a_missing_serving_profile_names_the_indexer_section() {
    let dir = tempfile::tempdir().unwrap();
    let (kg, vs) = vector_server(&dir);
    let v = call(
        &kg,
        &vs,
        &serde_json::json!({ "queryText": "graph database" }),
    );
    let text = result_text(&v);
    assert_eq!(v["result"]["isError"], Value::Bool(true), "{v}");
    assert!(
        text.contains("[indexer]") && text.contains("no index profile"),
        "the error must name the missing profile and the config section: {text}"
    );
    for word in ["provider", "model", "dimension"] {
        assert!(
            text.contains(word),
            "the error must name what to configure ({word}): {text}"
        );
    }
}

/// Whitespace embeds to a vector that means nothing, and the provider bills for
/// the call. The refusal must come before any of that, so it lands even with no
/// profile and no provider.
#[cfg(feature = "indexer")]
#[test]
fn an_empty_query_text_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (kg, vs) = vector_server(&dir);
    for blank in ["", "   ", "\t\n"] {
        let v = call(&kg, &vs, &serde_json::json!({ "queryText": blank }));
        let text = result_text(&v);
        assert_eq!(v["result"]["isError"], Value::Bool(true), "{blank:?}: {v}");
        assert!(
            text.contains("'queryText'") && text.contains("empty"),
            "the refusal must name the parameter: {text}"
        );
    }
}

/// A missing parameter is a different failure from a blank one, and both must
/// name `queryText`.
#[cfg(feature = "indexer")]
#[test]
fn a_missing_query_text_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (kg, vs) = vector_server(&dir);
    let v = call(&kg, &vs, &serde_json::json!({ "topK": 5 }));
    let text = result_text(&v);
    assert_eq!(v["result"]["isError"], Value::Bool(true), "{v}");
    assert!(text.contains("'queryText'"), "{text}");
}

/// Register one serving profile at `dims` and seed its generation marker,
/// exactly as the worker's first commit would. Shared by the filter tests
/// (profile dims == the store's `DIMS`) and the profile-vs-default test
/// (profile dims deliberately different from `DIMS`).
fn activate_test_profile(conn: &rusqlite::Connection, profile: &str, dims: u32) {
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
                "dimensions": dims,
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
}

/// The search tools share one filter contract: `filter.kind` limits the owner
/// kind, `filter.type` the owner type, and result rows carry `kind`. This file
/// cannot run a live `semantic_search` query (no provider is ever installed),
/// so the behaviour is exercised through `vector_search_entities`, which uses
/// the same handler core and needs no provider. A legacy store holds no
/// relation rows, so `kind: "relation"` must match nothing, not error.
fn seed_chunk_snapshot(dir: &tempfile::TempDir) {
    // Build a serving profile and chunk rows exactly as the worker would, via
    // an independent connection, then publish the snapshot on the store the
    // test holds. The 2.0.0 surface has no client ingestion tool: chunks are
    // the only vector source.
    let db = dir.path().join("memory.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let profile = "11111111-2222-3333-4444-555555555555";
    activate_test_profile(&conn, profile, DIMS);
    let seed = |name: &str, _etype: &str, value: f64| {
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
        let v: f32 = value as f32;
        let bytes: Vec<u8> = v.to_le_bytes().to_vec();
        let blob: Vec<u8> = (0..8)
            .flat_map(|_| bytes.iter().cloned())
            .collect::<Vec<_>>();
        conn.execute(
            "INSERT INTO chunk_vector(profile_id,kind,owner_kind,owner_id,chunk_index,type_id,owner_revision,blob,created_at_us,source)
             VALUES(?1,'identity','entity',?2,0,?3,1,?4,1,'test')",
            rusqlite::params![profile, entity_id, type_id, blob],
        )
        .unwrap();
    };
    seed("ada", "Person", 1.0);
    seed("acme", "Company", 0.1);
    drop(conn);
}

/// The search tools share one filter contract: `filter.kind` limits the owner
/// kind, `filter.type` the owner type, and result rows carry `kind`. This file
/// cannot run a live `semantic_search` query (no provider is ever installed),
/// so the behaviour is exercised through `vector_search_entities`, which uses
/// the same handler core and needs no provider.
#[test]
fn filter_selects_kind_and_type_before_ranking() {
    let dir = tempfile::tempdir().unwrap();
    let (kg, vs) = vector_server(&dir);
    let seed = |name: &str, etype: &str| {
        let v = call_tool(
            &kg,
            &vs,
            "create_entities",
            &serde_json::json!({
                "entities": [{"name": name, "entityType": etype, "observations": []}]
            }),
        );
        assert!(v["error"].is_null(), "seed {name}: {v}");
    };
    seed("ada", "Person");
    seed("acme", "Company");
    seed_chunk_snapshot(&dir);
    vs.reconcile_managed_snapshot().unwrap();

    // Without a filter both entities come back, kind-marked.
    let text = result_text(&call_tool(
        &kg,
        &vs,
        "vector_search_entities",
        &serde_json::json!({ "embedding": vec![1.0f64; DIMS as usize], "topK": 10 }),
    ));
    let rows = serde_json::from_str::<Value>(&text).expect("rows JSON")["results"]
        .as_array()
        .expect("results array")
        .clone();
    assert_eq!(rows.len(), 2, "both entities match unfiltered: {rows:?}");
    for row in &rows {
        assert_eq!(
            row["kind"].as_str(),
            Some("entity"),
            "kind on every row: {row:?}"
        );
    }

    // A `{kind, type}` filter narrows before ranking.
    let text = result_text(&call_tool(
        &kg,
        &vs,
        "vector_search_entities",
        &serde_json::json!({
            "embedding": vec![1.0f64; DIMS as usize],
            "topK": 10,
            "filter": { "kind": "entity", "type": "Person" },
        }),
    ));
    let rows = serde_json::from_str::<Value>(&text).expect("rows JSON")["results"]
        .as_array()
        .expect("results array")
        .clone();
    assert!(!rows.is_empty(), "a Person must match");
    for row in &rows {
        assert_eq!(row["kind"].as_str(), Some("entity"), "{row:?}");
        assert_eq!(row["entityType"].as_str(), Some("Person"), "{row:?}");
    }

    // A type filter without a kind also applies.
    let text = result_text(&call_tool(
        &kg,
        &vs,
        "vector_search_entities",
        &serde_json::json!({
            "embedding": vec![1.0f64; DIMS as usize],
            "topK": 10,
            "filter": { "type": "Person" },
        }),
    ));
    let rows = serde_json::from_str::<Value>(&text).expect("rows JSON")["results"]
        .as_array()
        .expect("results array")
        .clone();
    assert!(!rows.is_empty(), "a Person must match the type-only filter");
    for row in &rows {
        assert_eq!(row["entityType"].as_str(), Some("Person"), "{row:?}");
    }

    // The seeded snapshot holds no relation chunks, so `kind: "relation"`
    // matches nothing, without erroring.
    let text = result_text(&call_tool(
        &kg,
        &vs,
        "vector_search_entities",
        &serde_json::json!({
            "embedding": vec![1.0f64; DIMS as usize],
            "topK": 10,
            "filter": { "kind": "relation" },
        }),
    ));
    assert!(
        text.contains(r#""count":0"#),
        "no relation chunks are seeded: {text}"
    );
}

#[test]
fn filter_rejects_unknown_kind() {
    let dir = tempfile::tempdir().unwrap();
    let (kg, vs) = vector_server(&dir);
    let v = call_tool(
        &kg,
        &vs,
        "vector_search_entities",
        &serde_json::json!({
            "embedding": vec![1.0f64; DIMS as usize],
            "filter": { "kind": "property" },
        }),
    );
    let text = result_text(&v);
    assert_eq!(v["result"]["isError"], Value::Bool(true), "{v}");
    assert!(
        text.contains("entity") && text.contains("relation"),
        "the refusal must name the allowed kinds: {text}"
    );
}

/// A non-string `filter.kind` or `filter.type` must be refused, not silently
/// read as "no filter": a number or array passed by a buggy client would
/// otherwise widen the search and return a wrong result as if correct.
#[test]
fn filter_rejects_non_string_members() {
    let dir = tempfile::tempdir().unwrap();
    let (kg, vs) = vector_server(&dir);
    let v = call_tool(
        &kg,
        &vs,
        "vector_search_entities",
        &serde_json::json!({
            "embedding": vec![1.0f64; DIMS as usize],
            "filter": { "kind": 7 },
        }),
    );
    let text = result_text(&v);
    assert_eq!(v["result"]["isError"], Value::Bool(true), "{v}");
    assert!(
        text.contains("'filter.kind'"),
        "the refusal must name the member: {text}"
    );
    let v = call_tool(
        &kg,
        &vs,
        "vector_search_entities",
        &serde_json::json!({
            "embedding": vec![1.0f64; DIMS as usize],
            "filter": { "type": ["Person"] },
        }),
    );
    let text = result_text(&v);
    assert_eq!(v["result"]["isError"], Value::Bool(true), "{v}");
    assert!(
        text.contains("'filter.type'"),
        "the refusal must name the member: {text}"
    );
}

/// The identity-chunk gate and the stats `dims` must follow the serving
/// profile, not the CLI `--embedding-dims` default. The store here is built
/// at `DIMS` (8) while the profile embeds at 4: `vector_search_by_entity`
/// must still find the identity chunk, and `vector_store_stats` must report
/// the profile's 4, or a store configured through the config file (profile
/// 768, flag default 384) silently loses every identity-vector read.
#[test]
fn identity_and_stats_follow_the_serving_profile_dimension() {
    let dir = tempfile::tempdir().unwrap();
    let (kg, vs) = vector_server(&dir);
    let v = call_tool(
        &kg,
        &vs,
        "create_entities",
        &serde_json::json!({
            "entities": [
                {"name": "ada", "entityType": "Person", "observations": []},
                {"name": "acme", "entityType": "Company", "observations": []}
            ]
        }),
    );
    assert!(v["error"].is_null(), "seed entities: {v}");

    let db = dir.path().join("memory.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let profile = "11111111-2222-3333-4444-555555555555";
    // Profile at 4 dims while the store's own default is DIMS (8).
    activate_test_profile(&conn, profile, 4);
    let seed_identity = |conn: &rusqlite::Connection, name: &str, value: f32| {
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
        let blob: Vec<u8> = vec![value; 4]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        conn.execute(
            "INSERT INTO chunk_vector(profile_id,kind,owner_kind,owner_id,chunk_index,type_id,owner_revision,blob,created_at_us,source)
             VALUES(?1,'identity','entity',?2,0,?3,1,?4,1,'test')",
            rusqlite::params![profile, entity_id, type_id, blob],
        )
        .unwrap();
    };
    seed_identity(&conn, "ada", 1.0);
    seed_identity(&conn, "acme", 0.1);
    drop(conn);
    vs.reconcile_managed_snapshot().unwrap();

    // The identity chunk must serve at the profile dimension, not be dropped
    // by a gate built on the store's 8-dims default. ada's identity chunk
    // then finds acme (the default excludes the query owner itself).
    let text = result_text(&call_tool(
        &kg,
        &vs,
        "vector_search_by_entity",
        &serde_json::json!({ "entityName": "ada", "topK": 10 }),
    ));
    let rows = serde_json::from_str::<Value>(&text).expect("rows JSON")["results"]
        .as_array()
        .expect("results array")
        .clone();
    assert_eq!(rows.len(), 1, "ada finds one owner by identity: {text}");
    assert_eq!(
        rows[0]["name"].as_str(),
        Some("acme"),
        "the hit is acme: {text}"
    );

    // Stats report the dimension the store actually serves.
    let text = result_text(&call_tool(
        &kg,
        &vs,
        "vector_store_stats",
        &serde_json::json!({}),
    ));
    assert!(
        text.contains(r#""dims":4"#),
        "stats report the profile dimension, not the CLI default: {text}"
    );
}
