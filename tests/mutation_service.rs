use mcp_memory::config::{Durability, SqliteTuning};
use mcp_memory::kg::GraphHandle;
use mcp_memory::types::Entity;
use memory_core::mutation::{
    ChangeOperation, MutationContext, MutationRequest, MutationService, ObservationUpdate,
};
use rusqlite::Connection;
use std::num::NonZeroUsize;

#[test]
fn committed_changes_keep_tombstones_and_only_effective_updates() {
    let dir = tempfile::tempdir().unwrap();
    let graph = GraphHandle::new(
        &dir.path().join("memory.db"),
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .unwrap();
    let service = MutationService::new(&graph);
    let created = service
        .apply(
            MutationRequest::CreateEntities {
                entities: vec![entity("a"), entity("b")],
            },
            MutationContext::local(),
        )
        .unwrap();
    assert!(!created.transaction_id.is_nil());
    assert_eq!(created.changes.len(), 2);
    assert_eq!(created.changes[0].operation, ChangeOperation::Create);
    let noop = service
        .apply(
            MutationRequest::UpsertEntities {
                entities: vec![entity("a")],
            },
            MutationContext::local(),
        )
        .unwrap();
    assert!(noop.changes.is_empty());
    let updated = service
        .apply(
            MutationRequest::AddObservations {
                observations: vec![ObservationUpdate {
                    entity_name: "a".into(),
                    contents: vec!["new".into()],
                }],
            },
            MutationContext::local(),
        )
        .unwrap();
    assert_eq!(updated.changes.len(), 1);
    assert_eq!(updated.changes[0].operation, ChangeOperation::Update);
    assert_eq!(
        updated.changes[0].before.as_ref().unwrap().observations,
        ["original"]
    );
    assert_eq!(
        updated.changes[0].after.as_ref().unwrap().observations,
        ["original", "new"]
    );
    let relation = mcp_memory::types::Relation {
        from: "a".into(),
        to: "b".into(),
        relation_type: "knows".into(),
    };
    let related = service
        .apply(
            MutationRequest::CreateRelations {
                relations: vec![relation.clone()],
            },
            MutationContext::local(),
        )
        .unwrap();
    assert_eq!(related.changes.len(), 2);
    for change in &related.changes {
        assert_eq!(change.operation, ChangeOperation::Update);
        assert_eq!(
            change.relation_delta.as_ref().unwrap().added.as_slice(),
            std::slice::from_ref(&relation)
        );
    }
    let deleted = service
        .apply(
            MutationRequest::DeleteEntities {
                names: vec!["a".into()],
            },
            MutationContext::local(),
        )
        .unwrap();
    assert_eq!(deleted.changes.len(), 2);
    let tombstone = deleted
        .changes
        .iter()
        .find(|c| c.operation == ChangeOperation::Delete)
        .unwrap();
    assert_eq!(tombstone.before.as_ref().unwrap().name, "a");
    assert!(tombstone.after.is_none());
    assert_eq!(
        graph
            .degree("b", mcp_memory::kg::Direction::Incoming)
            .unwrap(),
        0
    );
}

#[test]
fn observation_batch_and_context_validation_are_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let graph = GraphHandle::new(
        &dir.path().join("memory.db"),
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .unwrap();
    graph.create_entities(&[entity("a")]).unwrap();
    let service = MutationService::new(&graph);
    let result = service.apply(
        MutationRequest::AddObservations {
            observations: vec![
                ObservationUpdate {
                    entity_name: "a".into(),
                    contents: vec!["must rollback".into()],
                },
                ObservationUpdate {
                    entity_name: "missing".into(),
                    contents: vec!["bad".into()],
                },
            ],
        },
        MutationContext::local(),
    );
    assert!(result.is_err());
    assert_eq!(graph.get_entity("a").unwrap(), Some(entity("a")));
    let mut invalid = MutationContext::local();
    invalid.hop_count = 16;
    assert!(
        service
            .apply(
                MutationRequest::DeleteEntities {
                    names: vec!["a".into()]
                },
                invalid
            )
            .is_err()
    );
    assert_eq!(graph.get_entity("a").unwrap(), Some(entity("a")));
}

fn entity(name: &str) -> Entity {
    Entity {
        name: name.into(),
        entity_type: "test".into(),
        observations: vec!["original".into()],
    }
}

#[test]
fn mcp_observation_batch_rejects_late_invalid_item_without_partial_write() {
    let dir = tempfile::tempdir().unwrap();
    let graph = GraphHandle::new(
        &dir.path().join("memory.db"),
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .unwrap();
    graph.create_entities(&[entity("a")]).unwrap();
    let args = serde_json::json!({"observations": [{"entityName":"a","contents":["partial"]}, {"contents":["invalid"]}]});
    assert!(mcp_memory::actions::memory::handle_add_observations(&graph, Some(&args)).is_err());
    assert_eq!(graph.get_entity("a").unwrap(), Some(entity("a")));
    let valid = serde_json::json!({"observations": [{"entityName":"a","contents":["committed"]}]});
    let response =
        mcp_memory::actions::memory::handle_add_observations(&graph, Some(&valid)).unwrap();
    let text = response["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(text).unwrap(),
        serde_json::json!({"results":[{"entityName":"a","addedObservations":["committed"]}]})
    );
}

#[test]
fn late_entity_delete_failure_rolls_back_observations_and_stats() {
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
    graph.create_entities(&[entity("kept")]).unwrap();
    let probe = Connection::open(&path).unwrap();
    assert_eq!(
        probe
            .query_row("SELECT COUNT(*) FROM observation", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    probe.execute_batch("CREATE TRIGGER fail_delete BEFORE DELETE ON entity BEGIN SELECT RAISE(ABORT, 'forced late failure'); END;").unwrap();
    assert!(graph.delete_entities(&["kept".into()]).is_err());
    assert_eq!(
        probe
            .query_row("SELECT COUNT(*) FROM observation", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(graph.get_entity("kept").unwrap(), Some(entity("kept")));
    assert_eq!(graph.get_entity_count().unwrap(), 1);
}

#[test]
fn every_write_path_rolls_back_on_a_final_statement_failure() {
    use mcp_memory::types::Relation;
    let relation = Relation {
        from: "a".into(),
        to: "b".into(),
        relation_type: "defines".into(),
    };
    let requests = vec![
        MutationRequest::CreateEntities {
            entities: vec![entity("new")],
        },
        MutationRequest::UpsertEntities {
            entities: vec![Entity {
                name: "a".into(),
                entity_type: "changed".into(),
                observations: vec!["new".into()],
            }],
        },
        MutationRequest::DeleteEntities {
            names: vec!["a".into()],
        },
        MutationRequest::CreateRelations {
            relations: vec![Relation {
                from: "b".into(),
                to: "a".into(),
                relation_type: "reverse".into(),
            }],
        },
        MutationRequest::DeleteRelations {
            relations: vec![relation.clone()],
        },
        MutationRequest::AddObservations {
            observations: vec![ObservationUpdate {
                entity_name: "a".into(),
                contents: vec!["new".into()],
            }],
        },
        MutationRequest::DeleteObservations {
            observations: vec![ObservationUpdate {
                entity_name: "a".into(),
                contents: vec!["original".into()],
            }],
        },
        MutationRequest::MergeEntities {
            source: "a".into(),
            target: "b".into(),
        },
        MutationRequest::PurgeDefinedEntities { name: "a".into() },
        MutationRequest::Compact,
        MutationRequest::Wipe,
    ];
    for request in requests {
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
        graph.create_entities(&[entity("a"), entity("b")]).unwrap();
        graph
            .create_relations(std::slice::from_ref(&relation))
            .unwrap();
        let before = graph.export("json", 100).unwrap();
        let probe = Connection::open(&path).unwrap();
        assert_eq!(
            probe
                .query_row("SELECT COUNT(*) FROM entity", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        probe.execute_batch("CREATE TRIGGER fail_final BEFORE UPDATE ON graph_stat WHEN OLD.key='entity_seq' BEGIN SELECT RAISE(ABORT, 'forced final failure'); END;").unwrap();
        let failed = MutationService::new(&graph).apply(request, MutationContext::local());
        assert!(failed.is_err());
        assert_eq!(graph.export("json", 100).unwrap(), before);
        assert_eq!(graph.get_entity("a").unwrap(), Some(entity("a")));
        assert_eq!(graph.get_entity("b").unwrap(), Some(entity("b")));
        assert_eq!(graph.get_entity_count().unwrap(), 2);
        assert_eq!(graph.get_relation_count().unwrap(), 1);
        assert_eq!(graph.entity_type_counts(), [("test".into(), 2)]);
        assert_eq!(graph.relation_type_counts(), [("defines".into(), 1)]);
        assert_eq!(
            probe
                .query_row("SELECT SUM(obs_count) FROM entity", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        probe.execute_batch("DROP TRIGGER fail_final;").unwrap();
        graph.create_entities(&[entity("after-rollback")]).unwrap();
        assert_eq!(graph.get_entity_count().unwrap(), 3);
    }
}

#[test]
fn duplicate_relation_deletion_and_merge_keep_effective_counters() {
    use mcp_memory::types::Relation;
    let dir = tempfile::tempdir().unwrap();
    let graph = GraphHandle::new(
        &dir.path().join("memory.db"),
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .unwrap();
    graph
        .create_entities(&[entity("a"), entity("b"), entity("c")])
        .unwrap();
    let ac = Relation {
        from: "a".into(),
        to: "c".into(),
        relation_type: "link".into(),
    };
    let bc = Relation {
        from: "b".into(),
        to: "c".into(),
        relation_type: "link".into(),
    };
    graph.create_relations(&[ac.clone(), bc.clone()]).unwrap();
    graph.merge_entities("a", "b").unwrap();
    assert!(graph.get_entity("a").unwrap().is_none());
    assert_eq!(graph.get_relation_count().unwrap(), 1);
    assert_eq!(graph.relation_type_counts(), [("link".into(), 1)]);
    assert_eq!(
        graph
            .degree("c", mcp_memory::kg::Direction::Incoming)
            .unwrap(),
        1
    );
    graph.delete_relations(&[bc.clone(), bc, ac]).unwrap();
    assert_eq!(graph.get_relation_count().unwrap(), 0);
    assert!(graph.relation_type_counts().is_empty());
    assert_eq!(
        graph
            .degree("c", mcp_memory::kg::Direction::Incoming)
            .unwrap(),
        0
    );
    graph.delete_entities(&["b".into(), "b".into()]).unwrap();
    assert_eq!(graph.get_entity_count().unwrap(), 1);
    assert!(graph.get_entity("b").unwrap().is_none());
}
