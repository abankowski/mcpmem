use mcpmem::actions::memory::handle_create_entities;
use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::kg::GraphHandle;
use mcpmem::types::{EntityInput, ObservationInput};
use mcpmem_core::events::{ChangeEvent, EventRepository};
use mcpmem_core::mutation::{MutationContext, MutationOutcome, MutationRequest, MutationService};
use rusqlite::Connection;
use serde_json::json;
use std::num::NonZeroUsize;

#[test]
fn observation_metadata_canonical_creation_and_strict_input() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = GraphHandle::new(
        &path,
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .unwrap();
    let result = handle_create_entities(&graph, None, Some(&json!({"entities":[{"name":"a","entityType":"test","observations":[{"body":"fact","occurredAtUs":42}]}]}))).unwrap();
    let entities: serde_json::Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(entities[0]["observations"][0]["body"], "fact");
    assert_eq!(entities[0]["observations"][0]["occurredAtUs"], 42);
    assert!(
        entities[0]["observations"][0]["createdAtUs"]
            .as_i64()
            .unwrap()
            > 0
    );
    assert!(entities[0]["observations"][0]["originEntityName"].is_null());
    let conn = Connection::open(&path).unwrap();
    let row: (i64, Option<i64>, Option<String>, i64) = conn
        .query_row(
            "SELECT created_us,origin_entity_id,origin_entity_name,occurred_us FROM observation",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(row.3, 42);
    assert_eq!((row.1, row.2), (None, None));
    for invalid in [
        json!("legacy"),
        json!({"body":"x","occurredAtUs":-1}),
        json!({"body":"x","occurredAtUs":1.5}),
        json!({"body":"x","occurredAtUs":18446744073709551615_u64}),
        json!({"body":"x","createdAtUs":1}),
        json!({"body":"x","originEntityName":"injected"}),
    ] {
        assert!(handle_create_entities(&graph, None, Some(&json!({"entities":[{"name":"invalid","entityType":"test","observations":[{"body":"valid"},invalid]}]}))).is_err());
        assert!(graph.get_entity("invalid").unwrap().is_none());
    }
}

#[test]
fn observation_metadata_historical_durable_payload_has_unknown_time() {
    let outcome: MutationOutcome = serde_json::from_value(json!({
        "changes":{"transactionId":"00000000-0000-0000-0000-000000000001","changes":[]},
        "result":{"Entity":{"name":"deleted","entityType":"test","observations":["historical"]}},
        "replayed":false
    }))
    .unwrap();
    let output = serde_json::to_value(outcome).unwrap();
    assert_eq!(
        output["result"]["Entity"]["observations"][0],
        json!({"body":"historical","createdAtUs":null,"occurredAtUs":null,"originEntityName":null})
    );

    let legacy_event_payload = json!({
        "eventId": "00000000-0000-0000-0000-000000000002",
        "transactionId": "00000000-0000-0000-0000-000000000003",
        "entityId": 9,
        "entityRevision": 1,
        "occurredAtUs": 10,
        "change": {
            "operation": "create",
            "before": null,
            "after": {
                "entityId": 9,
                "name": "historical",
                "entityType": "test",
                "observations": ["historical"]
            },
            "relation_delta": null
        },
        "provenance": serde_json::to_value(MutationContext::local()).unwrap()
    });
    let legacy_event: ChangeEvent = serde_json::from_value(legacy_event_payload.clone()).unwrap();
    let event_output = serde_json::to_value(&legacy_event).unwrap();
    assert_eq!(
        event_output["change"]["after"]["observations"][0],
        json!({"body":"historical","createdAtUs":null,"occurredAtUs":null,"originEntityName":null})
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = GraphHandle::new(
        &path,
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .unwrap();
    let storage = Connection::open(&path).unwrap();
    storage
        .execute(
            "INSERT INTO change_event VALUES(?1,?2,?3,?4,?5,?6)",
            rusqlite::params![
                legacy_event.event_id.to_string(),
                legacy_event.transaction_id.to_string(),
                legacy_event.entity_id,
                legacy_event.entity_revision,
                legacy_event.occurred_at_us,
                legacy_event_payload.to_string()
            ],
        )
        .unwrap();
    let persisted_event = EventRepository::new(&storage)
        .get(legacy_event.event_id)
        .unwrap()
        .expect("historical event exists");
    assert_eq!(
        serde_json::to_value(persisted_event).unwrap()["change"]["after"]["observations"][0],
        json!({"body":"historical","createdAtUs":null,"occurredAtUs":null,"originEntityName":null})
    );
    let fingerprint = "0".repeat(64);
    let response = json!({
        "changes":{"transactionId":"00000000-0000-0000-0000-000000000004","changes":[]},
        "result":{"Entity":{"name":"deleted","entityType":"test","observations":["historical"]}},
        "replayed":false
    });
    storage
        .execute(
            "INSERT INTO idempotency_record VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![
                "local",
                "legacy-response",
                fingerprint,
                response.to_string(),
                1_i64
            ],
        )
        .unwrap();
    let mut context = MutationContext::local();
    context.idempotency_key = Some("legacy-response".into());
    let replay = MutationService::new(&graph)
        .apply_idempotent(
            MutationRequest::CreateEntities { entities: vec![] },
            context,
            &fingerprint,
        )
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(
        serde_json::to_value(replay).unwrap()["result"]["Entity"]["observations"][0],
        json!({"body":"historical","createdAtUs":null,"occurredAtUs":null,"originEntityName":null})
    );
}

#[test]
fn observation_metadata_legacy_rows_keep_real_creation_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE observation(id INTEGER PRIMARY KEY,entity_id INTEGER NOT NULL,idx INTEGER NOT NULL,body TEXT NOT NULL,created_us INTEGER NOT NULL) STRICT; INSERT INTO observation VALUES(1,9,0,'old',123);").unwrap();
    mcpmem_core::schema::initialize_database(&conn).unwrap();
    let row: (i64,Option<i64>,Option<String>,Option<i64>) = conn.query_row("SELECT created_us,origin_entity_id,origin_entity_name,occurred_us FROM observation WHERE id=1", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(row, (123, None, None, None));
}

#[test]
fn merge_copies_source_provenance_without_rewriting_equal_target_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = GraphHandle::new(
        &path,
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .unwrap();
    graph
        .create_entities(&[
            EntityInput {
                name: "source".into(),
                entity_type: "test".into(),
                observations: vec![
                    ObservationInput {
                        body: "same".into(),
                        occurred_at_us: Some(11),
                    },
                    ObservationInput {
                        body: "copied".into(),
                        occurred_at_us: Some(12),
                    },
                ],
                attributes: None,
            },
            EntityInput {
                name: "target".into(),
                entity_type: "test".into(),
                observations: vec![ObservationInput {
                    body: "same".into(),
                    occurred_at_us: Some(99),
                }],
                attributes: None,
            },
        ])
        .unwrap();
    let conn = Connection::open(&path).unwrap();
    let source: (i64, i64) = conn
        .query_row(
            "SELECT e.id, o.created_us FROM entity e JOIN observation o ON o.entity_id=e.id WHERE e.name='source' AND o.body='copied'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    let merged = graph.merge_entities("source", "target").unwrap();
    assert_eq!(
        merged
            .observations
            .iter()
            .map(|observation| observation.body.as_str())
            .collect::<Vec<_>>(),
        ["same", "copied"]
    );
    let copied: (i64, Option<i64>, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT created_us, origin_entity_id, origin_entity_name, occurred_us FROM observation WHERE body='copied'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(copied.0, source.1, "merge keeps the source write time");
    assert_eq!(copied.1, Some(source.0));
    assert_eq!(copied.2.as_deref(), Some("source"));
    assert_eq!(copied.3, Some(12));
    let same: (Option<i64>, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT origin_entity_id, origin_entity_name, occurred_us FROM observation WHERE body='same'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        same,
        (None, None, Some(99)),
        "existing body keeps its metadata"
    );
    assert!(
        serde_json::to_value(merged).unwrap()["observations"][1]
            .get("originEntityId")
            .is_none(),
        "internal origin ids must never be serialized publicly"
    );
}

/// Legacy stored events predate the snapshot `attributes` field and the full
/// `RelationDetail` delta entries. Both must still deserialize: a snapshot
/// without `attributes`, and a `relation_delta` whose entries are bare
/// 3-field relation objects, have no observations or attributes.
#[test]
fn legacy_events_without_attributes_and_with_bare_relation_deltas_still_deserialize() {
    let legacy_event_payload = json!({
        "eventId": "00000000-0000-0000-0000-000000000002",
        "transactionId": "00000000-0000-0000-0000-000000000003",
        "entityId": 9,
        "entityRevision": 1,
        "occurredAtUs": 10,
        "change": {
            "operation": "update",
            "before": {
                "entityId": 9,
                "name": "historical",
                "entityType": "test",
                "observations": ["historical"]
            },
            "after": {
                "entityId": 9,
                "name": "historical",
                "entityType": "test",
                "observations": ["historical"]
            },
            "relationDelta": {
                "added": [
                    {"from": "a", "to": "b", "relationType": "knows"},
                    {"from": "a", "to": "c", "relationType": "knows",
                     "observations": [], "attributes": {}}
                ],
                "removed": [
                    {"from": "b", "to": "a", "relationType": "knows"}
                ]
            }
        },
        "provenance": serde_json::to_value(MutationContext::local()).unwrap()
    });
    let legacy_event: ChangeEvent = serde_json::from_value(legacy_event_payload).unwrap();
    let snapshot = legacy_event.change.after.expect("after snapshot");
    assert_eq!(
        snapshot.attributes, None,
        "a legacy snapshot without attributes deserializes to None"
    );
    let delta = legacy_event.change.relation_delta.expect("relation delta");
    assert_eq!(
        delta.added,
        vec![
            mcpmem::types::RelationDetail {
                from: "a".into(),
                to: "b".into(),
                relation_type: "knows".into(),
                observations: vec![],
                attributes: std::collections::BTreeMap::new(),
            },
            mcpmem::types::RelationDetail {
                from: "a".into(),
                to: "c".into(),
                relation_type: "knows".into(),
                observations: vec![],
                attributes: std::collections::BTreeMap::new(),
            },
        ],
        "bare legacy triples gain empty observations and attributes"
    );
    assert_eq!(
        delta.removed,
        vec![mcpmem::types::RelationDetail {
            from: "b".into(),
            to: "a".into(),
            relation_type: "knows".into(),
            observations: vec![],
            attributes: std::collections::BTreeMap::new(),
        }]
    );
    assert!(
        legacy_event.change.relation_change.is_none(),
        "a legacy event has no relation change payload"
    );
}
