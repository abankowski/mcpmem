//! Server-level tests of the soft taxonomy suggestion hook.
//!
//! The hook composes an additive per-object `taxonomySuggestions` field into
//! create/upsert responses. The field appears only when the authored type was
//! unknown before the write. The write itself never fails for an unknown type.

use mcpmem::actions::memory::{handle_create_entities, handle_create_relations, handle_upsert_entities};
use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::kg::GraphHandle;
use serde_json::{Value, json};
use std::num::NonZeroUsize;

/// A graph over a temporary database. `dir` stays alive for the whole test.
fn graph() -> (tempfile::TempDir, GraphHandle) {
    let dir = tempfile::tempdir().unwrap();
    let kg = GraphHandle::new(
        &dir.path().join("memory.db"),
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .unwrap();
    (dir, kg)
}

/// The inner JSON of a tool response: the parsed `content[0].text`.
fn body(response: Value) -> Value {
    serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn unknown_entity_type_gets_suggestions_known_type_gets_none() {
    let (_dir, graph) = graph();
    // Seed: the type "person" is now established.
    handle_create_entities(
        &graph,
        Some(&json!({"entities":[{"name":"seed","entityType":"person","observations":[]}]})),
    )
    .unwrap();

    // A misspelled new type must get "person" suggested, and the write succeeds.
    let response = handle_create_entities(
        &graph,
        Some(&json!({"entities":[{"name":"Ada","entityType":"persn","observations":[]}]})),
    )
    .unwrap();
    let entities = body(response);
    assert_eq!(entities[0]["name"], "Ada");
    let suggestions = &entities[0]["taxonomySuggestions"];
    assert!(suggestions.is_array(), "suggestions must be an array");
    assert_eq!(suggestions[0]["name"], "person");
    assert!(suggestions[0]["score"].as_f64().unwrap() > 0.0);

    // The authored type row exists after the write, so a post-commit raw
    // existence check could never flag it as unknown. The hook must judge the
    // type against the pre-write state.
    assert!(graph.entity_type_exists("persn"));

    // A call with a known type must not carry the key.
    let response = handle_create_entities(
        &graph,
        Some(&json!({"entities":[{"name":"Bob","entityType":"person","observations":[]}]})),
    )
    .unwrap();
    let entities = body(response);
    assert_eq!(entities[0]["name"], "Bob");
    assert!(entities[0].get("taxonomySuggestions").is_none());
}

#[test]
fn unknown_relation_type_is_created_without_error_and_gets_suggestions() {
    let (_dir, graph) = graph();
    handle_create_entities(
        &graph,
        Some(&json!({"entities":[
            {"name":"a","entityType":"person","observations":[]},
            {"name":"b","entityType":"person","observations":[]},
            {"name":"c","entityType":"person","observations":[]}
        ]})),
    )
    .unwrap();

    // The first relation type is unknown; the relation must still be created.
    let response = handle_create_relations(
        &graph,
        Some(&json!({"relations":[{"from":"a","to":"b","relationType":"knows"}]})),
    )
    .unwrap();
    let relations = body(response);
    assert_eq!(relations[0]["from"], "a");
    assert_eq!(relations[0]["to"], "b");
    assert_eq!(relations[0]["relationType"], "knows");
    assert!(relations[0].get("isError").is_none());
    assert!(relations[0]["taxonomySuggestions"].is_array());

    // A misspelled relation type gets the established type suggested.
    let response = handle_create_relations(
        &graph,
        Some(&json!({"relations":[{"from":"b","to":"a","relationType":"know_"}]})),
    )
    .unwrap();
    let relations = body(response);
    assert!(relations[0].get("isError").is_none());
    assert_eq!(relations[0]["taxonomySuggestions"][0]["name"], "knows");

    // A known relation type on a fresh pair must not carry the key.
    let response = handle_create_relations(
        &graph,
        Some(&json!({"relations":[{"from":"a","to":"c","relationType":"knows"}]})),
    )
    .unwrap();
    let relations = body(response);
    assert_eq!(relations[0]["from"], "a");
    assert_eq!(relations[0]["to"], "c");
    assert!(relations[0].get("isError").is_none());
    assert!(relations[0].get("taxonomySuggestions").is_none());
}

#[test]
fn upsert_suggests_for_a_new_type_and_stays_quiet_for_a_known_one() {
    let (_dir, graph) = graph();
    handle_create_entities(
        &graph,
        Some(&json!({"entities":[{"name":"seed","entityType":"person","observations":[]}]})),
    )
    .unwrap();

    // A new entity with a misspelled type gets suggestions.
    let response = handle_upsert_entities(
        &graph,
        Some(&json!({"entities":[{"name":"Ada","entityType":"persn","observations":[]}]})),
    )
    .unwrap();
    let upserted = body(response);
    assert_eq!(upserted["results"][0]["name"], "Ada");
    assert_eq!(upserted["results"][0]["taxonomySuggestions"][0]["name"], "person");

    // An existing entity with a known type must not carry the key.
    let response = handle_upsert_entities(
        &graph,
        Some(&json!({"entities":[{"name":"seed","entityType":"person","observations":[]}]})),
    )
    .unwrap();
    let upserted = body(response);
    assert_eq!(upserted["results"][0]["name"], "seed");
    assert!(upserted["results"][0].get("taxonomySuggestions").is_none());
}