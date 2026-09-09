use memory_core::graph::GraphHandle;
use memory_core::mutation::{
    ChangeOperation, MutationContext, MutationRequest, MutationService, ObservationUpdate,
};
use memory_core::storage::{Durability, SqliteTuning};
use memory_core::subscriptions::{SubscriptionRepository, WebhookSubscription};
use memory_core::types::{Entity, Relation};
use rusqlite::Connection;
use std::{collections::BTreeSet, num::NonZeroUsize, path::Path};

fn graph(path: &Path) -> GraphHandle {
    GraphHandle::new(
        path,
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .unwrap()
}

fn entity(name: &str) -> Entity {
    Entity {
        name: name.into(),
        entity_type: "test".into(),
        observations: vec!["original".into()],
    }
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap()
}

#[test]
fn graph_bootstrap_precedes_migration_statements() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ordering.db");
    let conn = Connection::open(&path).unwrap();
    // Model a pending migration's dependency on the graph without changing
    // append-only migration files: each ledger insert reads the prerequisite.
    conn.execute_batch(
        "CREATE TABLE schema_migration(version INTEGER PRIMARY KEY, checksum TEXT NOT NULL, applied_at_us INTEGER NOT NULL) STRICT;
         CREATE TRIGGER require_graph BEFORE INSERT ON schema_migration BEGIN
           SELECT count(*) FROM entity;
           SELECT count(*) FROM observation;
           SELECT count(*) FROM relation;
           SELECT count(*) FROM name_fts;
           SELECT count(*) FROM obs_fts;
           SELECT CASE WHEN (SELECT count(*) FROM graph_stat) != 5
             THEN RAISE(ABORT, 'graph statistics must be seeded before migrations') END;
         END;",
    ).unwrap();
    let graph = graph(&path);
    graph.create_entities(&[entity("ready")]).unwrap();
    assert_eq!(graph.get_entity("ready").unwrap(), Some(entity("ready")));
    assert_eq!(count(&conn, "schema_migration"), 2);
}

#[test]
fn pending_migrations_roll_back_together_after_later_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollback.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_migration(version INTEGER PRIMARY KEY, checksum TEXT NOT NULL, applied_at_us INTEGER NOT NULL) STRICT;
         CREATE TRIGGER reject_second BEFORE INSERT ON schema_migration
           WHEN new.version=2 BEGIN SELECT RAISE(ABORT, 'injected migration failure'); END;",
    ).unwrap();
    let error = GraphHandle::new(
        &path,
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        1,
    )
    .err()
    .expect("second migration must fail");
    assert!(error.to_string().contains("injected migration failure"));
    let observer = Connection::open(&path).unwrap();
    assert_eq!(count(&observer, "schema_migration"), 0);
    let pending_tables: i64 = observer.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name IN ('entity_revision','change_event','webhook_subscription')",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(pending_tables, 0, "no earlier migration DDL may remain");
    conn.execute_batch("DROP TRIGGER reject_second").unwrap();
    drop(graph(&path));
    assert_eq!(count(&observer, "schema_migration"), 2);
}

#[test]
fn initializer_preserves_legacy_graph_without_migration_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy-graph.db");
    let conn = Connection::open(&path).unwrap();
    // Independently construct the pre-ledger storage format with live rows.
    conn.execute_batch(
        "CREATE TABLE entity(id INTEGER PRIMARY KEY, name_hash INTEGER NOT NULL, name TEXT NOT NULL, type_id INTEGER NOT NULL,
            obs_count INTEGER NOT NULL DEFAULT 0, out_deg INTEGER NOT NULL DEFAULT 0, in_deg INTEGER NOT NULL DEFAULT 0,
            created_us INTEGER NOT NULL, updated_us INTEGER NOT NULL, flags INTEGER NOT NULL DEFAULT 0) STRICT;
         CREATE TABLE observation(id INTEGER PRIMARY KEY, entity_id INTEGER NOT NULL, idx INTEGER NOT NULL, body TEXT NOT NULL, created_us INTEGER NOT NULL) STRICT;
         CREATE TABLE relation(from_id INTEGER NOT NULL, to_id INTEGER NOT NULL, type_id INTEGER NOT NULL, created_us INTEGER NOT NULL) STRICT;
         CREATE TABLE type_dict(id INTEGER PRIMARY KEY, kind INTEGER NOT NULL, name TEXT NOT NULL, count INTEGER NOT NULL DEFAULT 0) STRICT;
         CREATE TABLE graph_stat(key TEXT NOT NULL PRIMARY KEY, value INTEGER NOT NULL) STRICT, WITHOUT ROWID;
         INSERT INTO type_dict VALUES(1,0,'test',1);
         INSERT INTO graph_stat VALUES('entities',1),('relations',0),('observations',1),('entity_seq',7),('obs_seq',9);
         INSERT INTO observation VALUES(9,7,0,'original',11);",
    ).unwrap();
    conn.execute(
        "INSERT INTO entity VALUES(7,?1,'legacy',1,1,0,0,11,11,0)",
        [memory_core::graph::name_hash("legacy")],
    )
    .unwrap();
    assert_eq!(count(&conn, "entity"), 1);
    assert_eq!(count(&conn, "observation"), 1);
    memory_core::schema::initialize_database(&conn).unwrap();
    memory_core::schema::initialize_database(&conn).unwrap();
    let graph = graph(&path);
    assert_eq!(graph.get_entity("legacy").unwrap(), Some(entity("legacy")));
    graph.create_entities(&[entity("new")]).unwrap();
    assert_eq!(count(&conn, "entity"), 2);
    assert_eq!(count(&conn, "observation"), 2);
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT id FROM entity WHERE name='new'", [], |r| r.get(0))
            .unwrap(),
        8
    );
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT value FROM graph_stat WHERE key='obs_seq'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        10
    );
    assert_eq!(
        count(&conn, "change_event"),
        1,
        "startup does not invent legacy events"
    );
}

#[test]
fn initializer_is_idempotent_and_preserves_historical_checksums_and_connection_tuning() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA cache_size=-321; PRAGMA foreign_keys=ON;")
        .unwrap();
    memory_core::schema::initialize_database(&conn).unwrap();
    let before: Vec<(i64, String, i64)> = conn
        .prepare("SELECT version,checksum,applied_at_us FROM schema_migration ORDER BY version")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(before.len(), 2);
    assert_eq!(
        before[0].1,
        "a48def8b25e9ecf813d3fa27a785893ba5de346a8b82f012cc543af2fecd5af2"
    );
    assert_eq!(
        before[1].1,
        "2c267315d89203d5223895a845b275d8add904310d2c0a5a7a5b0d1873b984ec"
    );
    memory_core::schema::initialize_database(&conn).unwrap();
    let after: Vec<(i64, String, i64)> = conn
        .prepare("SELECT version,checksum,applied_at_us FROM schema_migration ORDER BY version")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(after, before);
    for (pragma, expected) in [
        ("synchronous", 2),
        ("cache_size", -321),
        ("foreign_keys", 1),
    ] {
        assert_eq!(
            conn.query_row::<i64, _, _>(&format!("PRAGMA {pragma}"), [], |r| r.get(0))
                .unwrap(),
            expected
        );
    }
    assert_eq!(count(&conn, "graph_stat"), 5);
}

#[cfg(feature = "indexer")]
#[test]
fn indexer_bootstraps_fresh_graph_and_reopens_without_resetting_it() {
    struct UnusedProvider;
    impl indexer_worker::EmbeddingProvider for UnusedProvider {
        fn embed(
            &self,
            _: &memory_core::jobs::IndexProfile,
            _: &[indexer_worker::CanonicalDocument],
        ) -> Result<Vec<Vec<f32>>, indexer_worker::ProviderError> {
            panic!("empty queue must not call provider")
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("worker.db");
    let worker = indexer_worker::IndexerWorker::new(
        &path,
        UnusedProvider,
        std::time::Duration::from_secs(5),
    );
    assert_eq!(worker.run_once(1).unwrap().claimed, 0);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(count(&conn, "entity"), 0);
    assert_eq!(count(&conn, "graph_stat"), 5);
    let graph = graph(&path);
    graph.create_entities(&[entity("retained")]).unwrap();
    assert_eq!(worker.run_once(1).unwrap().claimed, 0);
    assert_eq!(
        graph.get_entity("retained").unwrap(),
        Some(entity("retained"))
    );
}

#[test]
fn effective_changes_commit_events_and_coalesce_tombstones_without_deliveries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = graph(&path);
    let conn = Connection::open(&path).unwrap();
    graph.create_entities(&[entity("a")]).unwrap();
    assert_eq!(count(&conn, "change_event"), 1);
    assert_eq!(count(&conn, "event_outbox"), 0);
    assert_eq!(count(&conn, "index_job"), 1);
    graph.upsert_entities(&[entity("a")]).unwrap();
    assert_eq!(count(&conn, "change_event"), 1);
    graph
        .upsert_entities(&[Entity {
            observations: vec!["changed".into()],
            ..entity("a")
        }])
        .unwrap();
    graph.delete_entities(&["a".into()]).unwrap();
    assert_eq!(count(&conn, "change_event"), 3);
    assert_eq!(count(&conn, "index_job"), 1);
    let row: (i64, String) = conn
        .query_row(
            "SELECT entity_revision, operation FROM index_job",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row, (3, "delete".into()));
    assert!(
        conn.execute("UPDATE change_event SET entity_revision=99", [])
            .is_err()
    );
}

#[test]
fn failed_mutation_rolls_back_graph_events_and_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = graph(&path);
    graph.create_entities(&[entity("a")]).unwrap();
    let conn = Connection::open(&path).unwrap();
    assert_eq!(count(&conn, "change_event"), 1);
    let result = MutationService::new(&graph).apply(
        MutationRequest::AddObservations {
            observations: vec![
                ObservationUpdate {
                    entity_name: "a".into(),
                    contents: vec!["must rollback".into()],
                },
                ObservationUpdate {
                    entity_name: "absent".into(),
                    contents: vec!["fail".into()],
                },
            ],
        },
        MutationContext::local(),
    );
    assert!(result.is_err());
    assert_eq!(
        graph.get_entity("a").unwrap().unwrap().observations,
        ["original"]
    );
    assert_eq!(count(&conn, "change_event"), 1);
    assert_eq!(count(&conn, "index_job"), 1);
}

#[test]
fn startup_rejects_changed_migration_and_preserves_legacy_vector_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE vector_embedding(entity_id INTEGER PRIMARY KEY, dims INTEGER, blob BLOB, model TEXT, created_us INTEGER); INSERT INTO vector_embedding VALUES(7,1,X'0000803F','old',1);").unwrap();
    drop(graph(&path));
    assert_eq!(count(&conn, "vector_embedding"), 1);
    assert_eq!(count(&conn, "profile_vector"), 0);
    let versions: Vec<i64> = conn
        .prepare("SELECT version FROM schema_migration ORDER BY version")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(versions, [1, 2]);
    drop(graph(&path));
    assert_eq!(count(&conn, "schema_migration"), 2);
    conn.execute("UPDATE schema_migration SET checksum='tampered'", [])
        .unwrap();
    assert!(
        GraphHandle::new(
            &path,
            Durability::Sync,
            SqliteTuning::default(),
            NonZeroUsize::new(32).unwrap(),
            1
        )
        .is_err()
    );
}

#[test]
fn separately_opened_writers_do_not_reuse_entity_ids_or_lose_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let first = graph(&path);
    let second = graph(&path);
    first.create_entities(&[entity("first")]).unwrap();
    second.create_entities(&[entity("second")]).unwrap();
    let conn = Connection::open(path).unwrap();
    assert_eq!(count(&conn, "entity"), 2);
    assert_eq!(count(&conn, "change_event"), 2);
}

#[test]
fn delivery_recovery_fences_expired_tokens_and_serializes_each_subscription() {
    use memory_core::events::EventRepository;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = graph(&path);
    graph.create_entities(&[entity("a"), entity("b")]).unwrap();
    let conn = Connection::open(&path).unwrap();
    let other = Connection::open(&path).unwrap();
    let repo = EventRepository::new(&conn);
    let other_repo = EventRepository::new(&other);
    let events: Vec<String> = conn
        .prepare("SELECT event_id FROM change_event ORDER BY entity_id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let subscription = uuid::Uuid::new_v4();
    for event in events {
        let event = uuid::Uuid::parse_str(&event).unwrap();
        repo.enqueue_delivery(event, subscription).unwrap();
        repo.enqueue_delivery(event, subscription).unwrap();
    }
    assert_eq!(count(&conn, "event_outbox"), 2);
    let expired = repo.claim_due(100, 10).unwrap().unwrap();
    assert!(other_repo.claim_due(105, 10).unwrap().is_none());
    let recovered = other_repo.claim_due(111, 10).unwrap().unwrap();
    assert!(recovered.lease.epoch > expired.lease.epoch);
    assert!(!repo.complete(&expired, 112).unwrap());
    assert!(other_repo.complete(&recovered, 112).unwrap());
    assert!(other_repo.complete(&recovered, 113).unwrap());
    assert!(repo.claim_due(114, 10).unwrap().is_some());
}

#[test]
fn raw_request_idempotency_replays_original_result_and_rejects_changed_bytes() {
    use memory_core::events::request_fingerprint;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = graph(&path);
    let service = MutationService::new(&graph);
    let mut context = MutationContext::local();
    context.idempotency_key = Some("request-1".into());
    let request = MutationRequest::CreateEntities {
        entities: vec![entity("a")],
    };
    let fingerprint = request_fingerprint("POST", "/api/v1/mutations", b"original");
    let first = service
        .apply_idempotent(request.clone(), context.clone(), &fingerprint)
        .unwrap();
    assert!(!first.replayed);
    graph.delete_entities(&["a".into()]).unwrap();
    let replay = service
        .apply_idempotent(request.clone(), context.clone(), &fingerprint)
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.changes.transaction_id, first.changes.transaction_id);
    assert!(graph.get_entity("a").unwrap().is_none());
    let changed = request_fingerprint("POST", "/api/v1/mutations", b" original");
    assert!(
        service
            .apply_idempotent(request, context, &changed)
            .unwrap_err()
            .to_string()
            .contains("idempotency_conflict")
    );
    assert_eq!(count(&Connection::open(path).unwrap(), "change_event"), 2);
}

fn profile() -> memory_core::jobs::IndexProfile {
    use memory_core::jobs::{DistanceMetric, IndexProfile, Normalization};
    IndexProfile {
        id: uuid::Uuid::new_v4(),
        store_key: "default".into(),
        provider_kind: "external".into(),
        model: "test".into(),
        dimensions: 2,
        representation_version: "entity-v1".into(),
        normalization: Normalization::None,
        distance_metric: DistanceMetric::Cosine,
        vector_encoding_version: "f32le-v1".into(),
    }
}

#[test]
fn profile_rebuild_preserves_serving_and_fences_stale_revision_commits() {
    use memory_core::jobs::{
        AnnGenerationRepository, IndexJobRepository, IndexProfileRegistry, StoreState,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = graph(&path);
    graph.create_entities(&[entity("a")]).unwrap();
    let conn = Connection::open(&path).unwrap();
    let registry = IndexProfileRegistry::new(&conn);
    let jobs = IndexJobRepository::new(&conn);
    let ann = AnnGenerationRepository::new(&conn);
    assert_eq!(registry.state("default").unwrap(), StoreState::LegacyCompat);
    registry.ensure_legacy_writes("default").unwrap();
    let candidate = profile();
    registry.begin_rebuild(&candidate).unwrap();
    assert!(
        registry
            .ensure_legacy_writes("default")
            .unwrap_err()
            .to_string()
            .contains("direct_vector_writes_disabled")
    );
    assert!(registry.activate(candidate.id).is_err());
    let expired = jobs.claim_due(100, 10).unwrap().unwrap();
    let recovered = jobs.claim_due(111, 20).unwrap().unwrap();
    assert!(recovered.lease.epoch > expired.lease.epoch);
    assert!(
        !jobs
            .commit_vector(&expired, 112, Some(&[1.0, 0.0]), "worker")
            .unwrap()
    );
    graph
        .upsert_entities(&[Entity {
            observations: vec!["new".into()],
            ..entity("a")
        }])
        .unwrap();
    assert!(
        !jobs
            .commit_vector(&recovered, 112, Some(&[1.0, 0.0]), "worker")
            .unwrap()
    );
    let current = jobs.claim_due(113, 20).unwrap().unwrap();
    assert_eq!(current.entity_revision, 2);
    assert!(
        jobs.commit_vector(&current, 114, Some(&[1.0, 0.0]), "worker")
            .unwrap()
    );
    assert!(
        jobs.commit_vector(&current, 115, Some(&[1.0, 0.0]), "worker")
            .unwrap()
    );
    ann.verify_full_scan(candidate.id).unwrap();
    assert!(registry.activate(candidate.id).is_err());
    let generation = ann.get(candidate.id).unwrap();
    assert!(
        ann.mark_published(candidate.id, generation.durable_generation)
            .unwrap()
    );
    registry.activate(candidate.id).unwrap();
    assert_eq!(
        registry.state("default").unwrap(),
        StoreState::Active(candidate.id)
    );
    let mut next = profile();
    next.model = "new-model".into();
    registry.begin_rebuild(&next).unwrap();
    registry.fail_rebuild(next.id, "provider failed").unwrap();
    assert_eq!(
        registry.serving_profile("default").unwrap(),
        Some(candidate.id)
    );
    assert!(registry.activate(next.id).is_err());
}

#[test]
fn empty_candidate_still_requires_an_explicit_reader_publication() {
    use memory_core::jobs::{AnnGenerationRepository, IndexProfileRegistry};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let _graph = graph(&path);
    let conn = Connection::open(path).unwrap();
    let registry = IndexProfileRegistry::new(&conn);
    let ann = AnnGenerationRepository::new(&conn);
    let candidate = profile();
    registry.begin_rebuild(&candidate).unwrap();
    ann.verify_full_scan(candidate.id).unwrap();
    assert!(registry.activate(candidate.id).is_err());
    assert!(ann.mark_published(candidate.id, 0).unwrap());
    registry.activate(candidate.id).unwrap();
}

#[test]
fn rename_persists_one_rename_event_and_matches_rename_subscriptions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = graph(&path);
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
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "INSERT INTO relation SELECT * FROM relation
         WHERE from_id=(SELECT id FROM entity WHERE name='old')
           AND to_id=(SELECT id FROM entity WHERE name='outgoing')
           AND type_id=(SELECT id FROM type_dict WHERE kind=1 AND name='out');",
    )
    .unwrap();
    let old_id: i64 = conn
        .query_row("SELECT id FROM entity WHERE name='old'", [], |row| {
            row.get(0)
        })
        .unwrap();
    let source_revision: i64 = conn
        .query_row(
            "SELECT revision FROM entity_revision WHERE entity_id=?1",
            [old_id],
            |row| row.get(0),
        )
        .unwrap();
    let neighbours: Vec<(i64, i64)> = conn
        .prepare(
            "SELECT entity_id, revision FROM entity_revision
             WHERE entity_id IN (SELECT id FROM entity WHERE name IN ('incoming','outgoing'))
             ORDER BY entity_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let neighbour_jobs: Vec<(i64, i64, String)> = conn
        .prepare(
            "SELECT entity_id, entity_revision, operation FROM index_job
             WHERE entity_id IN (SELECT id FROM entity WHERE name IN ('incoming','outgoing'))
             ORDER BY entity_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let subscribe = |event_operations| {
        let subscription = WebhookSubscription {
            subscription_id: uuid::Uuid::new_v4(),
            endpoint: "https://hooks.example.test/rename".into(),
            event_operations,
            entity_types: vec![],
            ignored_origins: vec![],
            consumer_origin: "consumer".into(),
            secret_ref: "vault://rename".into(),
            enabled: true,
        };
        SubscriptionRepository::new(&conn)
            .upsert(subscription.clone())
            .unwrap();
        subscription.subscription_id
    };
    let rename_subscription = subscribe(vec![ChangeOperation::Rename]);
    let unfiltered_subscription = subscribe(vec![]);
    let _create_subscription = subscribe(vec![ChangeOperation::Create]);
    let _update_subscription = subscribe(vec![ChangeOperation::Update]);
    let _delete_subscription = subscribe(vec![ChangeOperation::Delete]);
    assert_eq!(
        serde_json::from_str::<ChangeOperation>("\"rename\"").unwrap(),
        ChangeOperation::Rename,
        "operation filters accept exactly the rename value"
    );
    let before_events = count(&conn, "change_event");
    let before_jobs = count(&conn, "index_job");

    let committed = MutationService::new(&graph)
        .apply(
            MutationRequest::RenameEntity {
                old_name: "old".into(),
                new_name: "new".into(),
            },
            MutationContext::local(),
        )
        .unwrap();

    assert_eq!(committed.changes.len(), 1, "rename has no neighbour events");
    let change = &committed.changes[0];
    assert_eq!(change.operation, ChangeOperation::Rename);
    assert_eq!(change.old_name.as_deref(), Some("old"));
    assert_eq!(change.new_name.as_deref(), Some("new"));
    assert!(change.relation_delta.is_none());
    assert_eq!(count(&conn, "change_event"), before_events + 1);
    let durable_events: Vec<memory_core::events::ChangeEvent> = conn
        .prepare("SELECT payload FROM change_event WHERE transaction_id=?1")
        .unwrap()
        .query_map([committed.transaction_id.to_string()], |row| {
            row.get::<_, String>(0)
        })
        .unwrap()
        .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
        .collect();
    assert_eq!(durable_events.len(), 1, "no durable neighbour events");
    let event = &durable_events[0];
    assert_eq!(event.entity_id, old_id, "rename keeps the stable entity id");
    assert_eq!(event.entity_revision, source_revision + 1);
    assert_eq!(event.change.operation, ChangeOperation::Rename);
    assert_eq!(event.change.old_name.as_deref(), Some("old"));
    assert_eq!(event.change.new_name.as_deref(), Some("new"));
    assert_eq!(
        count(&conn, "event_outbox"),
        2,
        "rename filter and an unfiltered subscription receive rename"
    );
    let recipients: BTreeSet<String> = conn
        .prepare("SELECT subscription_id FROM event_outbox")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        recipients,
        BTreeSet::from([
            rename_subscription.to_string(),
            unfiltered_subscription.to_string()
        ]),
        "create, update, and delete filters receive no rename delivery"
    );
    for (entity_id, revision) in neighbours {
        assert_eq!(
            conn.query_row(
                "SELECT revision FROM entity_revision WHERE entity_id=?1",
                [entity_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            revision,
            "rename leaves neighbour revisions unchanged"
        );
    }
    let neighbour_jobs_after: Vec<(i64, i64, String)> = conn
        .prepare(
            "SELECT entity_id, entity_revision, operation FROM index_job
             WHERE entity_id IN (SELECT id FROM entity WHERE name IN ('incoming','outgoing'))
             ORDER BY entity_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(neighbour_jobs_after, neighbour_jobs);
    assert_eq!(
        conn.query_row("SELECT operation FROM index_job", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "upsert",
        "the coalesced current job remains an upsert"
    );
    assert_eq!(count(&conn, "index_job"), before_jobs);
}

#[test]
fn worker_retries_renewal_validation_and_restart_keep_durable_fences() {
    use memory_core::jobs::{
        AnnGenerationRepository, IndexJobRepository, IndexProfileRegistry, Normalization,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = graph(&path);
    graph.create_entities(&[entity("a")]).unwrap();
    let mut candidate = profile();
    candidate.normalization = Normalization::L2;
    let conn = Connection::open(&path).unwrap();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&candidate)
        .unwrap();
    let jobs = IndexJobRepository::new(&conn);
    let first = jobs.claim_due(100, 10).unwrap().unwrap();
    assert!(jobs.renew(&first, 105, 20).unwrap());
    assert!(jobs.claim_due(111, 10).unwrap().is_none());
    assert!(
        jobs.commit_vector(&first, 112, Some(&[f32::NAN, 0.0]), "worker")
            .is_err()
    );
    assert!(
        jobs.commit_vector(&first, 112, Some(&[1.0]), "worker")
            .is_err()
    );
    assert!(
        jobs.commit_vector(&first, 112, Some(&[2.0, 0.0]), "worker")
            .is_err()
    );
    assert_eq!(count(&conn, "profile_vector"), 0);
    assert!(jobs.retry(&first, 113, 200, "temporary", false).unwrap());
    assert!(jobs.claim_due(199, 10).unwrap().is_none());
    drop(conn);
    drop(graph);
    let reopened = self::graph(&path);
    let conn = Connection::open(&path).unwrap();
    let jobs = IndexJobRepository::new(&conn);
    let second = jobs.claim_due(200, 20).unwrap().unwrap();
    assert!(second.lease.epoch > first.lease.epoch);
    assert!(!jobs.renew(&first, 201, 10).unwrap());
    assert!(
        jobs.commit_vector(&second, 201, Some(&[1.0, 0.0]), "worker")
            .unwrap()
    );
    let blob: Vec<u8> = conn
        .query_row("SELECT blob FROM profile_vector", [], |r| r.get(0))
        .unwrap();
    assert_eq!(blob, [0, 0, 128, 63, 0, 0, 0, 0]);
    let ann = AnnGenerationRepository::new(&conn);
    assert_eq!(ann.get(candidate.id).unwrap().durable_generation, 1);
    assert!(!ann.mark_published(candidate.id, 0).unwrap());
    reopened.delete_entities(&["a".into()]).unwrap();
    let deletion = jobs.claim_due(202, 10).unwrap().unwrap();
    assert!(jobs.commit_vector(&deletion, 203, None, "worker").unwrap());
    assert_eq!(count(&conn, "profile_vector"), 0);
    assert_eq!(ann.get(candidate.id).unwrap().durable_generation, 2);
}

#[test]
fn wal_reads_continue_and_busy_writes_fail_within_configured_budget() {
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let tuning = SqliteTuning {
        busy_timeout_ms: 40,
        ..SqliteTuning::default()
    };
    let graph = GraphHandle::new(
        &path,
        Durability::Sync,
        tuning,
        NonZeroUsize::new(32).unwrap(),
        1,
    )
    .unwrap();
    graph.create_entities(&[entity("a")]).unwrap();
    let blocker = Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = Instant::now();
    assert_eq!(graph.get_entity("a").unwrap().unwrap().name, "a");
    assert!(start.elapsed() < Duration::from_secs(1));
    let start = Instant::now();
    assert!(graph.create_entities(&[entity("blocked")]).is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(count(&blocker, "entity"), 1);
    assert_eq!(count(&blocker, "change_event"), 1);
}

#[test]
#[ignore = "invoked as a separate process by concurrent_process_writers_keep_all_events"]
fn subprocess_writer_child() {
    let path = std::env::var_os("MEMORY_OUTBOX_TEST_DB").expect("parent supplies database");
    let prefix = std::env::var("MEMORY_OUTBOX_TEST_PREFIX").unwrap();
    let graph = graph(Path::new(&path));
    for index in 0..20 {
        graph
            .create_entities(&[entity(&format!("{prefix}-{index}"))])
            .unwrap();
    }
}

#[test]
fn concurrent_process_writers_keep_all_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    drop(graph(&path));
    let children: Vec<_> = (0..4)
        .map(|index| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "subprocess_writer_child", "--ignored"])
                .env("MEMORY_OUTBOX_TEST_DB", &path)
                .env("MEMORY_OUTBOX_TEST_PREFIX", index.to_string())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let conn = Connection::open(path).unwrap();
    assert_eq!(count(&conn, "entity"), 80);
    assert_eq!(count(&conn, "change_event"), 80);
    assert_eq!(count(&conn, "index_job"), 80);
}
