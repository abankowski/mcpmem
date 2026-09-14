use std::sync::Arc;

#[cfg(feature = "indexer")]
use clap::Parser;

use mcpmem::authz::local_principal;
use mcpmem::config::Config;
use mcpmem::kg::GraphHandle;
#[cfg(all(feature = "indexer", feature = "webhooks"))]
use mcpmem::runtime::{AppServices, RoleFuture, RoleLifecycle, RoleService, RuntimeComposition};
use mcpmem::runtime::{ConfigError, RoleSet, RuntimeRole};
use mcpmem::server::{HttpOutcome, MCPServer, dispatch_http_body};
use mcpmem::tools::ToolCategory;
use serde_json::Value;

#[cfg(feature = "indexer")]
use mcpmem_core::jobs::{DistanceMetric, IndexProfile, IndexProfileRegistry, Normalization};
#[cfg(feature = "indexer")]
use uuid::Uuid;

#[test]
fn parses_mcp_role() {
    let roles = RoleSet::parse_csv("mcp").expect("mcp is always compiled");

    assert_eq!(roles.roles(), &[RuntimeRole::Mcp]);
}

#[cfg(all(feature = "indexer", feature = "webhooks"))]
#[test]
fn parses_all_compiled_roles() {
    let roles = RoleSet::parse_csv("mcp,indexer,webhooks")
        .expect("all selected roles are compiled for this test");

    assert_eq!(
        roles.roles(),
        &[
            RuntimeRole::Mcp,
            RuntimeRole::Indexer,
            RuntimeRole::Webhooks
        ]
    );
}

#[cfg(feature = "indexer")]
#[test]
fn parses_indexer_when_the_feature_is_compiled() {
    let roles = RoleSet::parse_csv("indexer").expect("indexer feature is compiled");

    assert_eq!(roles.roles(), &[RuntimeRole::Indexer]);
}

#[cfg(feature = "indexer")]
#[test]
fn rejects_legacy_observations_for_a_worker_only_role() {
    let args =
        mcpmem::Args::try_parse_from(["mcpmem", "--role", "indexer", "--legacy-observations"])
            .expect("CLI syntax is valid");

    let error = mcpmem::config::Config::from_args(&args)
        .expect_err("legacy observation transport requires MCP");
    assert_eq!(
        error.to_string(),
        "Invalid params: --legacy-observations requires the mcp role"
    );
}

#[cfg(feature = "webhooks")]
#[test]
fn parses_webhooks_when_the_feature_is_compiled() {
    let roles = RoleSet::parse_csv("webhooks").expect("webhooks feature is compiled");

    assert_eq!(roles.roles(), &[RuntimeRole::Webhooks]);
}

#[test]
fn rejects_duplicate_roles() {
    let result = RoleSet::parse_csv("mcp,mcp");

    assert_eq!(result, Err(ConfigError::DuplicateRole(RuntimeRole::Mcp)));
}

#[test]
fn rejects_an_empty_role_list() {
    let result = RoleSet::parse_csv("   ");

    assert_eq!(result, Err(ConfigError::EmptyRoleSet));
}

#[cfg(not(feature = "indexer"))]
#[test]
fn rejects_indexer_when_the_feature_is_not_compiled() {
    let result = RoleSet::parse_csv("indexer");

    assert_eq!(
        result,
        Err(ConfigError::RoleNotCompiled(RuntimeRole::Indexer))
    );
}

#[cfg(not(feature = "webhooks"))]
#[test]
fn rejects_webhooks_when_the_feature_is_not_compiled() {
    let result = RoleSet::parse_csv("webhooks");

    assert_eq!(
        result,
        Err(ConfigError::RoleNotCompiled(RuntimeRole::Webhooks))
    );
}

#[cfg(all(feature = "indexer", feature = "webhooks"))]
struct ImmediateMcpService;

#[cfg(all(feature = "indexer", feature = "webhooks"))]
struct ImmediateWebhookService;

#[cfg(all(feature = "indexer", feature = "webhooks"))]
impl RoleService for ImmediateWebhookService {
    fn run(&self) -> RoleFuture {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(all(feature = "indexer", feature = "webhooks"))]
impl RoleService for ImmediateMcpService {
    fn run(&self) -> RoleFuture {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(all(feature = "indexer", feature = "webhooks"))]
#[tokio::test]
async fn supervises_selected_roles_and_stops_with_mcp() {
    let roles = RoleSet::parse_csv("mcp,indexer,webhooks").expect("features are compiled");
    let services = Arc::new(
        AppServices::new(Arc::new(ImmediateMcpService))
            .with_webhooks(Arc::new(ImmediateWebhookService)),
    );

    let running = RuntimeComposition::start(roles, services).expect("roles start");
    assert_eq!(
        running.lifecycle(),
        &[
            RoleLifecycle {
                role: RuntimeRole::Mcp,
                state: mcpmem_core::LifecycleState::Running,
            },
            RoleLifecycle {
                role: RuntimeRole::Indexer,
                state: mcpmem_core::LifecycleState::Running,
            },
            RoleLifecycle {
                role: RuntimeRole::Webhooks,
                state: mcpmem_core::LifecycleState::Running,
            },
        ]
    );

    running
        .wait_for_shutdown()
        .await
        .expect("mcp exits cleanly");
}

// ---- MCP round-trip harness for the relation-observation / attribute tools ----
//
// The new tools are ordinary knowledge-graph tools, so their integration
// tests run through the same in-process dispatcher the MCP endpoint uses:
// build a graph with both categories enabled, then send JSON-RPC `tools/call`
// bodies to `dispatch_http_body` and inspect the parsed tool payload.

/// A graph whose `graph-read` and `graph-write` categories are both enabled.
/// Those flags are process-wide, so this goes through the same entry point
/// `src/main.rs` uses rather than setting the atomics directly.
fn test_graph(dir: &tempfile::TempDir) -> Arc<GraphHandle> {
    let config = Config {
        memory_file_path: dir.path().join("memory.db").to_string_lossy().into_owned(),
        enabled_categories: vec![ToolCategory::GraphRead, ToolCategory::GraphWrite],
        ..Config::default()
    };
    MCPServer::new_kg(config)
        .expect("test server builds")
        .graph()
}

/// A graph over an explicitly named database, for tests that must open the
/// database directly (the chunk-index-job counting test).
fn test_graph_at(database: &std::path::PathBuf) -> Arc<GraphHandle> {
    let config = Config {
        memory_file_path: database.to_string_lossy().into_owned(),
        enabled_categories: vec![ToolCategory::GraphRead, ToolCategory::GraphWrite],
        ..Config::default()
    };
    MCPServer::new_kg(config)
        .expect("test server builds")
        .graph()
}

fn body_of(outcome: HttpOutcome) -> Value {
    match outcome {
        HttpOutcome::Body(v) => v,
        other => panic!("expected a body, got {other:?}"),
    }
}

/// Runs one `tools/call` through the MCP dispatch. Returns the full JSON-RPC
/// response value, error envelope included.
fn call_raw(kg: &GraphHandle, name: &str, arguments: &Value) -> Value {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    });
    let v = body_of(
        dispatch_http_body(
            &req.to_string(),
            kg,
            None,
            &local_principal(),
        )
        .expect("valid JSON body dispatches"),
    );
    assert!(v["error"].is_null(), "{name} must not be a protocol error: {v}");
    v
}

/// Runs one `tools/call` and returns the payload inside `content[0].text`.
fn call_payload(kg: &GraphHandle, name: &str, arguments: &Value) -> Value {
    let text = call_text(kg, name, arguments);
    serde_json::from_str::<Value>(&text).expect("tool text is JSON")
}

/// Runs one `tools/call` and returns its text payload. Unit-text tools (the
/// delete family) put a plain sentence there, so the text is the contract.
fn call_text(kg: &GraphHandle, name: &str, arguments: &Value) -> String {
    let raw = call_raw(kg, name, arguments);
    // A successful tool result carries no `isError` key at all; an error
    // result carries `true`.
    assert_eq!(
        raw["result"]["isError"].as_bool().unwrap_or(false),
        false,
        "{name} must succeed: {raw}"
    );
    raw["result"]["content"][0]["text"]
        .as_str()
        .expect("a tool result carries text")
        .to_owned()
}

#[test]
fn relation_observation_tools_round_trip_through_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let kg = test_graph(&dir);

    let created = call_payload(
        &kg,
        "create_entities",
        &serde_json::json!({
            "entities": [
                {"name": "alice", "entityType": "person", "observations": []},
                {"name": "acme", "entityType": "company", "observations": []}
            ]
        }),
    );
    assert_eq!(created[0]["name"].as_str(), Some("alice"), "{created}");

    // create_relations accepts the new optional observations and attributes.
    let created = call_payload(
        &kg,
        "create_relations",
        &serde_json::json!({
            "relations": [{
                "from": "alice",
                "to": "acme",
                "relationType": "employs",
                "observations": [{"body": "contract signed in 2026"}],
                "attributes": {"status": "active"}
            }]
        }),
    );
    assert_eq!(created[0]["from"].as_str(), Some("alice"), "{created}");

    // add_relation_observations reports the appended observations per triple.
    let added = call_payload(
        &kg,
        "add_relation_observations",
        &serde_json::json!({
            "relations": [{
                "from": "alice",
                "to": "acme",
                "relationType": "employs",
                "contents": [{"body": "contract renewed"}]
            }]
        }),
    );
    let row = added["results"][0].clone();
    assert_eq!(row["from"].as_str(), Some("alice"), "{added}");
    assert_eq!(row["to"].as_str(), Some("acme"), "{added}");
    assert_eq!(row["relationType"].as_str(), Some("employs"), "{added}");
    let bodies: Vec<&str> = row["addedObservations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|o| o["body"].as_str())
        .collect();
    assert_eq!(bodies, vec!["contract renewed"], "{added}");

    // search_relations with a query returns the detail row, carrying both the
    // observations and the attributes map.
    let found = call_payload(
        &kg,
        "search_relations",
        &serde_json::json!({"query": "contract"}),
    );
    let row = found
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["from"].as_str() == Some("alice") && r["to"].as_str() == Some("acme"))
        .expect("the query finds the relation");
    assert_eq!(row["relationType"].as_str(), Some("employs"), "{found}");
    assert_eq!(row["attributes"]["status"].as_str(), Some("active"), "{found}");
    let bodies: Vec<&str> = row["observations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|o| o["body"].as_str())
        .collect();
    assert!(bodies.contains(&"contract signed in 2026"), "{found}");
    assert!(bodies.contains(&"contract renewed"), "{found}");

    // set_attributes returns the applied post-state: the re-read detail shows
    // the merged map.
    let set = call_payload(
        &kg,
        "set_attributes",
        &serde_json::json!({
            "targets": [{
                "ownerKind": "relation",
                "from": "alice",
                "to": "acme",
                "relationType": "employs",
                "attributes": {"priority": "high"}
            }]
        }),
    );
    let row = set["results"][0].clone();
    assert_eq!(row["from"].as_str(), Some("alice"), "{set}");
    assert_eq!(row["attributes"]["status"].as_str(), Some("active"), "{set}");
    assert_eq!(row["attributes"]["priority"].as_str(), Some("high"), "{set}");

    // delete_relation_observations removes exactly the named bodies.
    let text = call_text(
        &kg,
        "delete_relation_observations",
        &serde_json::json!({
            "relations": [{
                "from": "alice",
                "to": "acme",
                "relationType": "employs",
                "observations": [{"body": "contract renewed"}]
            }]
        }),
    );
    assert_eq!(text, "Relation observations deleted successfully", "{text}");
    let found = call_payload(
        &kg,
        "search_relations",
        &serde_json::json!({"query": "renewed"}),
    );
    assert_eq!(
        found.as_array().unwrap().len(),
        0,
        "the deleted observation must no longer match: {found}"
    );
    let found = call_payload(
        &kg,
        "search_relations",
        &serde_json::json!({"query": "contract"}),
    );
    let bodies: Vec<&str> = found[0]["observations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|o| o["body"].as_str())
        .collect();
    assert_eq!(bodies, vec!["contract signed in 2026"], "{found}");
}

#[test]
fn attribute_tools_cover_entities_and_relations() {
    let dir = tempfile::tempdir().unwrap();
    let kg = test_graph(&dir);

    call_payload(
        &kg,
        "create_entities",
        &serde_json::json!({
            "entities": [{
                "name": "widget",
                "entityType": "product",
                "observations": [],
                "attributes": {"color": "red"}
            }]
        }),
    );
    call_payload(
        &kg,
        "create_entities",
        &serde_json::json!({
            "entities": [{
                "name": "acme",
                "entityType": "company",
                "observations": []
            }]
        }),
    );

    // Entity owner: set_attributes returns the applied post-state — old keys
    // stay, new keys land.
    let set = call_payload(
        &kg,
        "set_attributes",
        &serde_json::json!({
            "targets": [{
                "ownerKind": "entity",
                "entityName": "widget",
                "attributes": {"weight": "5"}
            }]
        }),
    );
    assert_eq!(set["results"][0]["name"].as_str(), Some("widget"), "{set}");
    assert_eq!(set["results"][0]["attributes"]["color"].as_str(), Some("red"), "{set}");
    assert_eq!(set["results"][0]["attributes"]["weight"].as_str(), Some("5"), "{set}");

    // get_entity and describe_entity include the attributes map.
    let entity = call_payload(&kg, "get_entity", &serde_json::json!({"name": "widget"}));
    assert_eq!(entity["attributes"]["weight"].as_str(), Some("5"), "{entity}");
    let described = call_payload(
        &kg,
        "describe_entity",
        &serde_json::json!({"name": "widget"}),
    );
    assert_eq!(described["attributes"]["color"].as_str(), Some("red"), "{described}");

    // delete_attributes removes exactly the named keys.
    let text = call_text(
        &kg,
        "delete_attributes",
        &serde_json::json!({
            "targets": [{"ownerKind": "entity", "entityName": "widget", "keys": ["color"]}]
        }),
    );
    assert_eq!(text, "Attributes deleted successfully", "{text}");
    let entity = call_payload(&kg, "get_entity", &serde_json::json!({"name": "widget"}));
    assert_eq!(entity["attributes"].get("color"), None, "{entity}");
    assert_eq!(entity["attributes"]["weight"].as_str(), Some("5"), "{entity}");

    // upsert_entities persists its attributes too: the new map overwrites
    // the given keys and leaves the rest of the stored map alone. The
    // mutation result rows carry no attributes (core entity snapshots), so
    // persistence is read back through get_entity.
    call_payload(
        &kg,
        "upsert_entities",
        &serde_json::json!({"entities": [{
            "name": "widget",
            "entityType": "product",
            "observations": [],
            "attributes": {"color": "blue"}
        }]}),
    );
    let entity = call_payload(&kg, "get_entity", &serde_json::json!({"name": "widget"}));
    assert_eq!(entity["attributes"]["color"].as_str(), Some("blue"), "{entity}");
    assert_eq!(
        entity["attributes"]["weight"].as_str(),
        Some("5"),
        "upsert keeps the keys outside its map: {entity}"
    );

    // Relation owner: the same round trip through the triple shape.
    call_payload(
        &kg,
        "create_relations",
        &serde_json::json!({
            "relations": [{"from": "widget", "to": "acme", "relationType": "sells"}]
        }),
    );
    let set = call_payload(
        &kg,
        "set_attributes",
        &serde_json::json!({
            "targets": [{
                "ownerKind": "relation",
                "from": "widget",
                "to": "acme",
                "relationType": "sells",
                "attributes": {"price": "10"}
            }]
        }),
    );
    assert_eq!(set["results"][0]["from"].as_str(), Some("widget"), "{set}");
    assert_eq!(set["results"][0]["attributes"]["price"].as_str(), Some("10"), "{set}");

    call_text(
        &kg,
        "delete_attributes",
        &serde_json::json!({
            "targets": [{
                "ownerKind": "relation",
                "from": "widget",
                "to": "acme",
                "relationType": "sells",
                "keys": ["price"]
            }]
        }),
    );
    let found = call_payload(
        &kg,
        "search_relations",
        &serde_json::json!({
            "from": "widget",
            "to": "acme",
            "relationType": "sells"
        }),
    );
    assert_eq!(found[0]["attributes"].get("price"), None, "{found}");
}

#[test]
fn attribute_tools_enforce_owner_kind_and_target_exclusivity() {
    let dir = tempfile::tempdir().unwrap();
    let kg = test_graph(&dir);
    call_payload(
        &kg,
        "create_entities",
        &serde_json::json!({
            "entities": [
                {"name": "a", "entityType": "t", "observations": []},
                {"name": "b", "entityType": "t", "observations": []}
            ]
        }),
    );

    for (name, arguments) in [
        (
            "set_attributes",
            serde_json::json!({"targets": [
                {"ownerKind": "bogus", "entityName": "a", "attributes": {}}
            ]}),
        ),
        (
            "set_attributes",
            serde_json::json!({"targets": [
                {"ownerKind": "entity", "from": "a", "to": "b", "relationType": "r",
                 "attributes": {}}
            ]}),
        ),
        (
            "set_attributes",
            serde_json::json!({"targets": [
                {"ownerKind": "relation", "entityName": "a", "attributes": {}}
            ]}),
        ),
        (
            "set_attributes",
            serde_json::json!({"targets": [
                {"ownerKind": "entity", "entityName": "a", "attributes": {"": "v"}}
            ]}),
        ),
        (
            "delete_attributes",
            serde_json::json!({"targets": [
                {"ownerKind": "entity", "from": "a", "to": "b", "relationType": "r",
                 "keys": ["k"]}
            ]}),
        ),
        (
            "delete_attributes",
            serde_json::json!({"targets": [
                {"ownerKind": "entity", "entityName": "a", "keys": [""]}
            ]}),
        ),
    ] {
        let raw = call_raw(&kg, name, &arguments);
        assert_eq!(
            raw["result"]["isError"].as_bool(),
            Some(true),
            "{name} with {arguments} must be refused"
        );
    }

    // The request cap is part of the shared target validation: one target
    // past MAX_RELATIONS_PER_REQUEST is refused wholesale.
    let too_many = (0..=1000)
        .map(|_| {
            serde_json::json!({"ownerKind": "entity", "entityName": "a", "attributes": {}})
        })
        .collect::<Vec<Value>>();
    let too_many_args = serde_json::json!({"targets": too_many});
    for name in ["set_attributes", "delete_attributes"] {
        let raw = call_raw(&kg, name, &too_many_args);
        assert_eq!(
            raw["result"]["isError"].as_bool(),
            Some(true),
            "{name} with 1001 targets must be refused"
        );
    }

    // Key and value length caps from the shared validators.
    let long_key: String = "k".repeat(1025);
    let long_value: String = "v".repeat(65537);
    let long_key_payload = serde_json::json!({"targets": [
        {"ownerKind": "entity", "entityName": "a",
         "attributes": {long_key: "v"}},
    ]});
    let raw = call_raw(&kg, "set_attributes", &long_key_payload);
    assert_eq!(
        raw["result"]["isError"].as_bool(),
        Some(true),
        "an over-length attribute key must be refused"
    );
    let long_value_payload = serde_json::json!({"targets": [
        {"ownerKind": "entity", "entityName": "a",
         "attributes": {"k": long_value}},
    ]});
    let raw = call_raw(&kg, "set_attributes", &long_value_payload);
    assert_eq!(
        raw["result"]["isError"].as_bool(),
        Some(true),
        "an over-length attribute value must be refused"
    );

    // Control: a well-formed target passes the same gate.
    let set = call_payload(
        &kg,
        "set_attributes",
        &serde_json::json!({
            "targets": [{
                "ownerKind": "entity",
                "entityName": "a",
                "attributes": {"k": "v"}
            }]
        }),
    );
    assert_eq!(set["results"][0]["attributes"]["k"].as_str(), Some("v"), "{set}");
}

/// Attribute writes are the documented offline exception (REQ-ATTR-OFFLINE):
/// they must not bump revisions or queue chunk-index jobs. The control is
/// that observation-carrying writes on the same graph do enqueue: the profile
/// is registered as rebuilding, so created owners receive job rows and the
/// unchanged count below proves a real gate rather than an empty queue.
#[cfg(feature = "indexer")]
#[test]
fn attribute_writes_never_enqueue_index_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let kg = test_graph_at(&database);

    let conn = rusqlite::Connection::open(&database).unwrap();
    let profile = IndexProfile {
        id: Uuid::new_v4(),
        store_key: "default".into(),
        provider_kind: "test".into(),
        model: "fixed".into(),
        dimensions: 2,
        representation_version: "v1".into(),
        normalization: Normalization::None,
        distance_metric: DistanceMetric::Cosine,
        vector_encoding_version: "f32le-v1".into(),
    };
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .expect("profile rebuild starts");
    let job_count = |conn: &rusqlite::Connection| {
        conn.query_row(
            "SELECT COUNT(*) FROM chunk_index_job WHERE profile_id=?1",
            [profile.id.to_string()],
            |r| r.get::<_, i64>(0),
        )
        .unwrap() as usize
    };

    // Observation writes enqueue: the seeded owners get job rows.
    call_payload(
        &kg,
        "create_entities",
        &serde_json::json!({
            "entities": [
                {"name": "alice", "entityType": "person", "observations": [{"body": "runs"}]},
                {"name": "acme", "entityType": "company", "observations": []}
            ]
        }),
    );
    call_payload(
        &kg,
        "create_relations",
        &serde_json::json!({
            "relations": [{
                "from": "alice",
                "to": "acme",
                "relationType": "employs",
                "observations": [{"body": "writes checks"}]
            }]
        }),
    );
    let before = job_count(&conn);
    assert!(before > 0, "the rebuilding profile must hold job rows for the seeds");

    // Attribute writes (set and delete, entity and relation owners) must not
    // change the queue at all.
    call_payload(
        &kg,
        "set_attributes",
        &serde_json::json!({
            "targets": [
                {"ownerKind": "entity", "entityName": "alice", "attributes": {"a": "1"}},
                {"ownerKind": "relation", "from": "alice", "to": "acme",
                 "relationType": "employs", "attributes": {"b": "2"}}
            ]
        }),
    );
    call_text(
        &kg,
        "delete_attributes",
        &serde_json::json!({
            "targets": [
                {"ownerKind": "entity", "entityName": "alice", "keys": ["a"]},
                {"ownerKind": "relation", "from": "alice", "to": "acme",
                 "relationType": "employs", "keys": ["b"]}
            ]
        }),
    );
    assert_eq!(
        job_count(&conn),
        before,
        "attribute writes must not enqueue chunk-index jobs"
    );
}
