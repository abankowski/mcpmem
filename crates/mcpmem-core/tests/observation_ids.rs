//! Durable observation ids across the read and write paths.
//!
//! Every observation row owns a stable id from the sequence cells. Equal
//! bodies keep distinct ids, so an id-keyed edit or delete can address one
//! row without touching its duplicates. Old stored payloads without the new
//! field must still deserialize.

use std::num::NonZeroUsize;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use mcpmem_core::graph::GraphHandle;
use mcpmem_core::mutation::{
    EntitySnapshot, MutationContext, MutationRequest, MutationService,
};
use mcpmem_core::storage::{Durability, SqliteTuning};
use mcpmem_core::types::{Entity, EntityInput, Observation, ObservationInput, RelationInput};

struct TestKg(GraphHandle, PathBuf);

impl Deref for TestKg {
    type Target = GraphHandle;

    fn deref(&self) -> &GraphHandle {
        &self.0
    }
}

impl Drop for TestKg {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.1);
        let _ = std::fs::remove_file(self.1.with_extension("db-wal"));
        let _ = std::fs::remove_file(self.1.with_extension("db-shm"));
    }
}

fn new_kg() -> TestKg {
    static COUNTER: AtomicU64 = AtomicU64::new(300_000);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "kg_observation_ids_{}_{}.db",
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
    let kg = GraphHandle::new(
        &path,
        Durability::Async,
        SqliteTuning::default(),
        NonZeroUsize::new(10000).unwrap(),
        4,
    )
    .expect("create test kg");
    TestKg(kg, path)
}

fn entity_with_bodies(name: &str, bodies: &[&str]) -> EntityInput {
    Entity {
        name: name.into(),
        entity_type: "Thing".into(),
        observations: bodies.iter().map(|b| ObservationInput::from(*b)).collect(),
        attributes: None,
    }
}

fn apply(kg: &GraphHandle, request: MutationRequest) {
    MutationService::new(kg)
        .apply(request, MutationContext::local())
        .expect("mutation applies");
}

fn observation_ids(kg: &GraphHandle, name: &str) -> Vec<i64> {
    kg.get_entity(name)
        .expect("entity read succeeds")
        .expect("entity exists")
        .observations
        .iter()
        .map(|o| o.observation_id.expect("read model carries the id"))
        .collect()
}

#[test]
fn observation_id_round_trips_through_node_read() {
    let kg = new_kg();
    kg.create_entities(&[entity_with_bodies("a", &["same", "same"])])
        .expect("create succeeds");
    let entity = kg.get_entity("a").expect("read succeeds").expect("entity exists");
    assert_eq!(entity.observations.len(), 2, "both equal bodies persist");
    let ids: Vec<i64> = entity
        .observations
        .iter()
        .map(|o| o.observation_id.expect("read model carries the id"))
        .collect();
    assert_ne!(ids[0], ids[1], "equal bodies keep distinct ids");
    // The wire field is camelCase and absent-tolerant.
    let text = serde_json::to_string(&entity).expect("entity serializes");
    assert!(text.contains(r#""observationId""#), "id appears on the wire");
    let decoded: Entity<Observation> = serde_json::from_str(&text).expect("wire value decodes");
    assert_eq!(decoded, entity, "id survives a serde round trip");
}

#[test]
fn entity_snapshot_without_observation_id_deserializes() {
    // Old stored events predate the id field; the snapshot deserializer
    // must tolerate their payloads unchanged.
    let payload = r#"{"entityId":1,"name":"a","entityType":"T","observations":[{"body":"fact","createdAtUs":1,"occurredAtUs":null,"originEntityName":null}],"attributes":{}}"#;
    let snapshot: EntitySnapshot = serde_json::from_str(payload).expect("old payload parses");
    assert_eq!(snapshot.observations.len(), 1);
    assert_eq!(
        snapshot.observations[0].observation_id, None,
        "absent id defaults to None"
    );
}

#[test]
fn delete_observation_by_id_removes_only_the_target_duplicate() {
    let kg = new_kg();
    kg.create_entities(&[entity_with_bodies("a", &["same", "same"])])
        .expect("create succeeds");
    let ids = observation_ids(&kg, "a");
    apply(
        &kg,
        MutationRequest::DeleteObservationById {
            entity_name: "a".into(),
            observation_id: ids[1],
        },
    );
    let entity = kg.get_entity("a").expect("read succeeds").expect("entity exists");
    assert_eq!(entity.observations.len(), 1, "exactly one row remains");
    assert_eq!(
        entity.observations[0].observation_id,
        Some(ids[0]),
        "the untargeted duplicate survives"
    );
}

#[test]
fn edit_observation_preserves_created_at_and_origin() {
    let kg = new_kg();
    kg.create_entities(&[Entity {
        name: "a".into(),
        entity_type: "Thing".into(),
        observations: vec![ObservationInput {
            body: "lexiconbody".into(),
            occurred_at_us: Some(500),
        }],
        attributes: None,
    }])
    .expect("create succeeds");
    let entity = kg.get_entity("a").expect("read succeeds").expect("entity exists");
    let id = entity.observations[0].observation_id.expect("read model carries the id");
    let created_at_us = entity.observations[0].created_at_us;
    apply(
        &kg,
        MutationRequest::EditObservation {
            entity_name: "a".into(),
            observation_id: id,
            body: "replacedbody".into(),
            occurred_at_us: Some(900),
        },
    );
    let entity = kg.get_entity("a").expect("read succeeds").expect("entity exists");
    assert_eq!(entity.observations.len(), 1);
    let obs = &entity.observations[0];
    assert_eq!(obs.body, "replacedbody", "body is replaced");
    assert_eq!(obs.created_at_us, created_at_us, "write time is preserved");
    assert_eq!(obs.origin_entity_name, None, "origin stays untouched");
    assert_eq!(obs.occurred_at_us, Some(900), "fact time is replaced");
    assert_eq!(obs.observation_id, Some(id), "the row keeps its id");
    // The FTS projection follows the edit.
    let hit: Vec<String> = kg
        .search_nodes_filtered("replacedbody", None, 0, 10)
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(hit, vec!["a"], "the new body is a hit");
    let miss: Vec<String> = kg
        .search_nodes_filtered("lexiconbody", None, 0, 10)
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(miss.is_empty(), "the old body text is not a hit");
}

#[test]
fn relation_observation_delete_by_id_targets_one_row() {
    let kg = new_kg();
    kg.create_entities(&[entity_with_bodies("a", &[]), entity_with_bodies("b", &[])])
        .expect("create succeeds");
    kg.create_relations(&[RelationInput {
        from: "a".into(),
        to: "b".into(),
        relation_type: "knows".into(),
        observations: vec![ObservationInput::from("same"), ObservationInput::from("same")],
        attributes: None,
    }])
    .expect("relation creates");
    let detail = kg
        .search_relations(Some("a"), Some("b"), Some("knows"), None, None)
        .expect("detail read succeeds");
    assert_eq!(detail.len(), 1, "one triple matches");
    let ids: Vec<i64> = detail[0]
        .observations
        .iter()
        .map(|o| o.observation_id.expect("relation detail carries the id"))
        .collect();
    assert_eq!(ids.len(), 2, "both equal relation bodies persist");
    assert_ne!(ids[0], ids[1], "equal relation bodies keep distinct ids");
    apply(
        &kg,
        MutationRequest::DeleteRelationObservationById {
            from: "a".into(),
            to: "b".into(),
            relation_type: "knows".into(),
            observation_id: ids[1],
        },
    );
    let detail = kg
        .search_relations(Some("a"), Some("b"), Some("knows"), None, None)
        .expect("detail read succeeds");
    assert_eq!(detail[0].observations.len(), 1, "exactly one row remains");
    assert_eq!(
        detail[0].observations[0].observation_id,
        Some(ids[0]),
        "the untargeted duplicate survives"
    );
}