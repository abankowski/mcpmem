use mcp_memory::config::{Durability, SqliteTuning};
use mcp_memory::kg::GraphHandle;
use mcp_memory::types::EntityInput as Entity;
use memory_core::mutation::{
    ChangeOperation, MutationContext, MutationRequest, MutationService, ObservationUpdate,
};
use memory_core::schema::initialize_database;
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
        updated.changes[0]
            .before
            .as_ref()
            .unwrap()
            .observations
            .iter()
            .map(|o| o.body.as_str())
            .collect::<Vec<_>>(),
        ["original"]
    );
    assert_eq!(
        updated.changes[0]
            .after
            .as_ref()
            .unwrap()
            .observations
            .iter()
            .map(|o| o.body.as_str())
            .collect::<Vec<_>>(),
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
    assert_original_entity(&graph, "a");
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
    assert_original_entity(&graph, "a");
}

fn entity(name: &str) -> Entity {
    Entity {
        name: name.into(),
        entity_type: "test".into(),
        observations: vec!["original".into()],
    }
}

fn assert_original_entity(graph: &GraphHandle, name: &str) {
    let actual = graph.get_entity(name).unwrap().expect("entity exists");
    assert_eq!(actual.name, name);
    assert_eq!(actual.entity_type, "test");
    assert_eq!(
        actual
            .observations
            .iter()
            .map(|observation| observation.body.as_str())
            .collect::<Vec<_>>(),
        ["original"]
    );
    assert!(actual.observations[0].created_at_us.is_some());
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
    let args = serde_json::json!({"observations": [{"entityName":"a","contents":[{"body":"partial"}]}, {"contents":[{"body":"invalid"}]}]});
    assert!(mcp_memory::actions::memory::handle_add_observations(&graph, Some(&args)).is_err());
    assert_original_entity(&graph, "a");
    let valid =
        serde_json::json!({"observations": [{"entityName":"a","contents":[{"body":"committed"}]}]});
    let response =
        mcp_memory::actions::memory::handle_add_observations(&graph, Some(&valid)).unwrap();
    let text = response["content"][0]["text"].as_str().unwrap();
    let response: serde_json::Value = serde_json::from_str(text).unwrap();
    let inserted = &response["results"][0]["addedObservations"][0];
    assert_eq!(inserted["body"], "committed");
    assert!(inserted["createdAtUs"].as_i64().is_some());
    assert!(inserted["occurredAtUs"].is_null());
    assert!(inserted["originEntityName"].is_null());
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
    assert_original_entity(&graph, "kept");
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
        MutationRequest::RenameEntity {
            old_name: "a".into(),
            new_name: "renamed".into(),
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
        assert_original_entity(&graph, "a");
        assert_original_entity(&graph, "b");
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
        // Wipe's FTS reset must roll back along with the content tables.
        for table in ["name_fts", "obs_fts"] {
            probe
                .execute_batch(&format!(
                    "INSERT INTO {table}({table},rank) VALUES('integrity-check',1);"
                ))
                .unwrap();
        }
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

#[test]
fn wipe_clears_fts_postings_and_preserves_integrity() {
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
    graph.create_entities(&[entity("needle")]).unwrap();
    let probe = Connection::open(&path).unwrap();
    for (table, term) in [("name_fts", "needle"), ("obs_fts", "original")] {
        let count: i64 = probe
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {table} MATCH ?1"),
                [term],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "non-empty {table} fixture");
    }
    graph.wipe().unwrap();
    for (table, term) in [("name_fts", "needle"), ("obs_fts", "original")] {
        let count: i64 = probe
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {table} MATCH ?1"),
                [term],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "{table} must not retain orphan postings");
        probe
            .execute_batch(&format!(
                "INSERT INTO {table}({table},rank) VALUES('integrity-check',1);"
            ))
            .unwrap();
    }
}

#[test]
fn legacy_duplicate_relation_rows_use_physical_counters_and_set_deltas() {
    use mcp_memory::types::Relation;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    // Make relation predate bootstrap: fresh databases now reject duplicates,
    // while an operator must still be able to inspect legacy duplicate rows.
    let legacy = Connection::open(&path).unwrap();
    legacy
        .execute_batch(
            "CREATE TABLE relation(
                 from_id INTEGER NOT NULL,
                 to_id INTEGER NOT NULL,
                 type_id INTEGER NOT NULL,
                 created_us INTEGER NOT NULL
             ) STRICT;",
        )
        .unwrap();
    initialize_database(&legacy).unwrap();
    drop(legacy);
    let graph = GraphHandle::new(
        &path,
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .unwrap();
    graph.create_entities(&[entity("a"), entity("b")]).unwrap();
    let relation = Relation {
        from: "a".into(),
        to: "b".into(),
        relation_type: "link".into(),
    };
    graph
        .create_relations(std::slice::from_ref(&relation))
        .unwrap();
    let probe = Connection::open(&path).unwrap();
    // Legacy storage permits repeated physical rows and counts each one.
    probe
        .execute_batch(
            "INSERT INTO relation SELECT * FROM relation;
         UPDATE graph_stat SET value=2 WHERE key='relations';
         UPDATE type_dict SET count=2 WHERE kind=1 AND name='link';
         UPDATE entity SET out_deg=2 WHERE name='a';
         UPDATE entity SET in_deg=2 WHERE name='b';",
        )
        .unwrap();
    assert_eq!(
        probe
            .query_row("SELECT COUNT(*) FROM relation", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(graph.get_relation_count().unwrap(), 2);
    MutationService::new(&graph)
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
    assert_eq!(
        graph
            .degree("a", mcp_memory::kg::Direction::Outgoing)
            .unwrap(),
        2
    );
    assert_eq!(graph.relation_type_counts(), [("link".into(), 2)]);
    let committed = MutationService::new(&graph)
        .apply(
            MutationRequest::DeleteRelations {
                relations: vec![relation.clone(), relation.clone()],
            },
            MutationContext::local(),
        )
        .unwrap();
    assert_eq!(
        probe
            .query_row("SELECT COUNT(*) FROM relation", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(graph.get_relation_count().unwrap(), 0);
    assert!(graph.relation_type_counts().is_empty());
    assert_eq!(
        graph
            .degree("a", mcp_memory::kg::Direction::Outgoing)
            .unwrap(),
        0
    );
    assert_eq!(
        graph
            .degree("b", mcp_memory::kg::Direction::Incoming)
            .unwrap(),
        0
    );
    assert_eq!(committed.changes.len(), 2);
    for change in committed.changes {
        let delta = change.relation_delta.unwrap();
        assert!(delta.added.is_empty());
        assert_eq!(delta.removed.as_slice(), std::slice::from_ref(&relation));
    }
}

#[test]
fn rename_preserves_the_stable_entity_and_its_incident_graph() {
    use mcp_memory::types::Relation;

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
        .create_entities(&[entity("old"), entity("incoming"), entity("outgoing")])
        .unwrap();
    graph
        .create_relations(&[
            Relation {
                from: "incoming".into(),
                to: "old".into(),
                relation_type: "in".into(),
            },
            Relation {
                from: "old".into(),
                to: "outgoing".into(),
                relation_type: "out".into(),
            },
            Relation {
                from: "old".into(),
                to: "old".into(),
                relation_type: "self".into(),
            },
        ])
        .unwrap();
    let probe = Connection::open(&path).unwrap();
    let before: (i64, i64, i64, i64, i64, i64) = probe
        .query_row(
            "SELECT id, name_hash, type_id, obs_count, out_deg, in_deg FROM entity WHERE name='old'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )
        .unwrap();
    let updated_us: i64 = probe
        .query_row(
            "SELECT updated_us FROM entity WHERE id=?1",
            [before.0],
            |row| row.get(0),
        )
        .unwrap();

    let renamed = graph.rename_entity("old", "new").unwrap();

    assert_eq!(renamed.name, "new");
    assert_eq!(renamed.entity_type, "test");
    assert_eq!(
        renamed
            .observations
            .iter()
            .map(|observation| observation.body.as_str())
            .collect::<Vec<_>>(),
        ["original"]
    );
    assert!(graph.get_entity("old").unwrap().is_none());
    assert_eq!(
        graph.search_nodes_filtered("old", None, 0, 10),
        Vec::<mcp_memory::types::Entity>::new()
    );
    assert_eq!(
        graph.search_nodes_filtered("new", None, 0, 10),
        vec![renamed]
    );
    let after: (i64, i64, i64, i64, i64, i64, i64) = probe
        .query_row(
            "SELECT id, name_hash, type_id, obs_count, out_deg, in_deg, updated_us FROM entity WHERE name='new'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
        )
        .unwrap();
    assert_eq!(after.0, before.0, "rename keeps the stable numeric id");
    assert_ne!(after.1, before.1, "rename replaces name_hash");
    assert_eq!(after.2, before.2, "rename keeps the entity type");
    assert_eq!(after.3, before.3, "rename keeps observation counters");
    assert_eq!(
        (after.4, after.5),
        (before.4, before.5),
        "rename keeps degrees"
    );
    assert_eq!(after.6, updated_us, "rename does not update updated_us");
    let relations: Vec<(String, String)> = probe
        .prepare(
            "SELECT source.name, target.name FROM relation \
             JOIN entity source ON source.id=relation.from_id \
             JOIN entity target ON target.id=relation.to_id \
             ORDER BY source.name, target.name",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        relations,
        [
            ("incoming".into(), "new".into()),
            ("new".into(), "new".into()),
            ("new".into(), "outgoing".into())
        ],
        "incoming, outgoing, and self relations survive"
    );
}

#[test]
fn rename_rejects_a_distinct_existing_target_and_same_name_is_eventless_noop() {
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
        .create_entities(&[entity("old"), entity("taken")])
        .unwrap();
    let probe = Connection::open(&path).unwrap();
    let before_events: i64 = probe
        .query_row("SELECT count(*) FROM change_event", [], |row| row.get(0))
        .unwrap();
    let before_jobs: i64 = probe
        .query_row("SELECT count(*) FROM index_job", [], |row| row.get(0))
        .unwrap();
    let err = graph.rename_entity("old", "taken").unwrap_err();
    assert!(matches!(
        err,
        memory_core::errors::MCSError::InvalidParams(message)
            if message == "Entity 'taken' already exists"
    ));
    assert_original_entity(&graph, "old");
    assert_eq!(
        probe
            .query_row(
                "SELECT count(*) FROM name_fts WHERE name_fts MATCH 'old'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1,
        "collision leaves the old FTS posting intact"
    );
    assert_eq!(
        probe
            .query_row("SELECT count(*) FROM change_event", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        before_events
    );
    assert_eq!(
        probe
            .query_row("SELECT count(*) FROM index_job", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        before_jobs
    );

    let result = MutationService::new(&graph)
        .apply(
            MutationRequest::RenameEntity {
                old_name: "old".into(),
                new_name: "old".into(),
            },
            MutationContext::local(),
        )
        .unwrap();
    assert!(
        result.changes.is_empty(),
        "same-name rename has no durable change"
    );
    assert_eq!(
        probe
            .query_row("SELECT count(*) FROM change_event", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        before_events
    );
    assert_eq!(
        probe
            .query_row("SELECT count(*) FROM index_job", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        before_jobs
    );
}
