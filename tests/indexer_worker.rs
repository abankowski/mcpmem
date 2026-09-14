#![cfg(feature = "indexer")]

use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::kg::GraphHandle;
use mcpmem::types::EntityInput as Entity;
use mcpmem::vector_store::{TaxonomyKind, VectorStore};
use mcpmem_core::jobs::{
    ChunkKind, DistanceMetric, IndexProfile, IndexProfileRegistry, Normalization, OwnerKind,
};
use mcpmem_indexer::{EmbeddingProvider, IndexerWorker, ProviderError};
use uuid::Uuid;

struct FixedProvider;

impl EmbeddingProvider for FixedProvider {
    fn embed_texts(
        &self,
        profile: &IndexProfile,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        Ok(texts
            .iter()
            .map(|_| vec![1.0; profile.dimensions as usize])
            .collect())
    }
}

/// Records the text the worker sends. Text is what the provider receives, so
/// recording it proves what reaches the model, not how the worker assembled it.
#[derive(Clone)]
struct RecordingProvider(Arc<Mutex<Vec<String>>>);

impl EmbeddingProvider for RecordingProvider {
    fn embed_texts(
        &self,
        profile: &IndexProfile,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        self.0.lock().extend_from_slice(texts);
        Ok(texts
            .iter()
            .map(|_| vec![1.0; profile.dimensions as usize])
            .collect())
    }
}

fn profile() -> IndexProfile {
    IndexProfile {
        id: Uuid::new_v4(),
        store_key: "default".into(),
        provider_kind: "test".into(),
        model: "fixed".into(),
        dimensions: 2,
        representation_version: "v1".into(),
        normalization: Normalization::None,
        distance_metric: DistanceMetric::Cosine,
        vector_encoding_version: "f32le-v1".into(),
    }
}

fn setup(path: &Path) -> GraphHandle {
    GraphHandle::new(
        path,
        Durability::Async,
        SqliteTuning::default(),
        NonZeroUsize::new(16).unwrap(),
        1,
    )
    .unwrap()
}

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64
}

fn chunk_rows(conn: &rusqlite::Connection, profile: &str, kind: &str) -> usize {
    conn.query_row(
        "SELECT COUNT(*) FROM chunk_vector WHERE profile_id=?1 AND kind=?2",
        [profile, kind],
        |r| r.get::<_, i64>(0),
    )
    .unwrap() as usize
}

fn entity_id_of(conn: &rusqlite::Connection, name: &str) -> i64 {
    conn.query_row("SELECT id FROM entity WHERE name=?1", [name], |r| r.get(0))
        .unwrap()
}

fn revision_of(conn: &rusqlite::Connection, name: &str) -> i64 {
    conn.query_row(
        "SELECT r.revision FROM entity_revision r JOIN entity e ON e.id=r.entity_id WHERE e.name=?1",
        [name],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn worker_commits_latest_canonical_revision() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    graph
        .create_entities(&[Entity {
            name: "Exact Name".into(),
            entity_type: "Person".into(),
            observations: vec!["first".into()],
            attributes: None,
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let profile = profile();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let worker = IndexerWorker::new(
        &database,
        RecordingProvider(Arc::clone(&captured)),
        Duration::from_secs(5),
    );
    let report = worker.run_once(now_us()).unwrap();
    assert_eq!(report.committed, 1);
    // The provider receives the chunk texts, not the joined document: the
    // identity chunk carries name and type, each observation is its own chunk.
    let texts = captured.lock();
    assert_eq!(texts.len(), 2);
    assert_eq!(texts[0], "Exact Name\nPerson");
    assert_eq!(texts[1], "first");
    drop(texts);
    assert_eq!(chunk_rows(&conn, &profile.id.to_string(), "identity"), 1);
    assert_eq!(chunk_rows(&conn, &profile.id.to_string(), "observation"), 1);
    let (kind, owner_kind, owner_revision): (String, String, i64) = conn
        .query_row(
            "SELECT kind, owner_kind, owner_revision FROM chunk_vector WHERE profile_id=?1 AND chunk_index=0",
            [profile.id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (kind.as_str(), owner_kind.as_str(), owner_revision),
        ("identity", "entity", 1)
    );
}

#[test]
fn canonical_document_splits_into_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    graph
        .create_entities(&[Entity {
            name: "Ada".into(),
            entity_type: "Person".into(),
            observations: vec!["first programmer".into()],
            attributes: None,
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let Ok(Some(doc)) = mcpmem_indexer::canonical_document_for_tests(
        &conn,
        entity_id_of(&conn, "Ada"),
        revision_of(&conn, "Ada"),
    ) else {
        panic!("Ada must canonicalize");
    };
    let chunks = doc.chunks();
    assert_eq!(
        chunks,
        vec![
            (ChunkKind::Identity, "Ada\nPerson".to_string()),
            (ChunkKind::Observation, "first programmer".to_string()),
        ],
    );
}

#[test]
fn relation_chunk_text_is_the_formatted_triple() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    graph
        .create_entities(&[
            Entity {
                name: "ada".into(),
                entity_type: "Person".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "bob".into(),
                entity_type: "Person".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();
    graph
        .create_relations(&[mcpmem::types::Relation {
            from: "ada".into(),
            to: "bob".into(),
            relation_type: "knows".into(),
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let (mirror_id, revision): (i64, i64) = conn
        .query_row(
            "SELECT m.id, m.revision FROM taxonomy_relation m
             JOIN entity f ON f.id=m.from_id JOIN entity t ON t.id=m.to_id
             JOIN type_dict d ON d.id=m.type_id
             WHERE f.name='ada' AND t.name='bob' AND d.name='knows' AND d.kind=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let Ok(Some(text)) = mcpmem_indexer::relation_chunk_text(&conn, mirror_id, revision) else {
        panic!("the mirror must canonicalize");
    };
    assert_eq!(text, "ada\nknows\nbob");
}

#[test]
fn commit_chunks_replaces_owner_rows_and_bumps_generation() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    graph
        .create_entities(&[Entity {
            name: "Ada".into(),
            entity_type: "Person".into(),
            observations: vec!["first programmer".into()],
            attributes: None,
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let profile = mcpmem_core::jobs::IndexProfile {
        id: Uuid::new_v4(),
        store_key: "default".into(),
        provider_kind: "fixed".into(),
        model: "unit".into(),
        dimensions: 2,
        representation_version: "chunks-identity+obs+relation-v2".into(),
        normalization: mcpmem_core::jobs::Normalization::None,
        distance_metric: mcpmem_core::jobs::DistanceMetric::Cosine,
        vector_encoding_version: "f32le-v1".into(),
    };
    mcpmem_core::jobs::IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    let entity_id = entity_id_of(&conn, "Ada");
    let revision = revision_of(&conn, "Ada");
    let repo = mcpmem_core::jobs::IndexJobRepository::new(&conn);
    // The fence requires the exact leased row, so claim the job the rebuild
    // enqueued rather than synthesizing a lease by hand.
    let job = repo.claim_due(now_us(), 10_000_000).unwrap().unwrap();
    assert_eq!(job.owner_kind, OwnerKind::Entity);
    assert_eq!(job.owner_id, entity_id);
    assert_eq!(job.owner_revision, revision);
    let vectors: &[&(ChunkKind, &[f32])] = &[
        &(ChunkKind::Identity, &[0.5f32, 0.5]),
        &(ChunkKind::Observation, &[0.1f32, 0.9]),
    ];
    let committed = repo
        .commit_chunks(&job, now_us() + 1, Some(vectors), "test")
        .unwrap();
    assert!(committed, "lease and revision are current");
    let rows: Vec<(String, String, i64, i64)> = conn
        .prepare(
            "SELECT kind, owner_kind, owner_id, chunk_index FROM chunk_vector
             WHERE profile_id=?1 ORDER BY chunk_index",
        )
        .unwrap()
        .query_map([profile.id.to_string()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows.len(), 2, "one identity and one observation chunk");
    assert_eq!(rows[0].0, "identity", "identity chunk comes first");
    assert_eq!(rows[0].1, "entity", "chunk rows carry the owner kind");
    assert_eq!(rows[1].0, "observation");
    let generation: i64 = conn
        .query_row(
            "SELECT durable_generation FROM ann_generation WHERE profile_id=?1",
            [profile.id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(generation, 1, "chunk commit bumps the durable generation");
}

#[test]
fn commit_chunks_commits_a_relation_owner_and_bumps_the_kind_generation() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    let conn = rusqlite::Connection::open(&database).unwrap();
    let profile = profile();
    mcpmem_core::jobs::IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    // The funnels enqueue for the candidate profile, so the mirror is
    // created after the rebuild starts.
    graph
        .create_entities(&[
            Entity {
                name: "ada".into(),
                entity_type: "Person".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "bob".into(),
                entity_type: "Person".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();
    graph
        .create_relations(&[mcpmem::types::Relation {
            from: "ada".into(),
            to: "bob".into(),
            relation_type: "knows".into(),
        }])
        .unwrap();
    let repo = mcpmem_core::jobs::IndexJobRepository::new(&conn);
    let mut relation_job = None;
    for _ in 0..3 {
        let job = repo.claim_due(now_us(), 10_000_000).unwrap().unwrap();
        if job.owner_kind == OwnerKind::Relation {
            relation_job = Some(job);
            break;
        }
    }
    let job = relation_job.expect("the relation funnel enqueues a claimable job");
    {
        let vector = [1.0f32, 0.0];
        let owned = [(ChunkKind::Relation, vector.as_slice())];
        let refs: Vec<_> = owned.iter().collect();
        assert!(
            repo.commit_chunks(&job, now_us() + 1, Some(&refs), "indexer")
                .unwrap()
        );
    }
    let (kind, owner_kind, type_id, owner_revision): (String, String, i64, i64) = conn
        .query_row(
            "SELECT kind, owner_kind, type_id, owner_revision FROM chunk_vector
             WHERE profile_id=?1 AND owner_kind='relation'",
            [profile.id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (kind.as_str(), owner_kind.as_str()),
        ("relation", "relation")
    );
    let mirror_type: i64 = conn
        .query_row(
            "SELECT type_id FROM taxonomy_relation WHERE id=?1",
            [job.owner_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        type_id, mirror_type,
        "the chunk row carries the mirror type"
    );
    assert_eq!(owner_revision, job.owner_revision);
    let kind_generation: i64 = conn
        .query_row(
            "SELECT durable_generation FROM taxonomy_ann_generation WHERE profile_id=?1 AND subject_kind=2",
            [profile.id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        kind_generation, 1,
        "a relation commit advances the kind-2 generation"
    );
    // The delete funnel for the same owner removes the chunk rows.
    graph
        .delete_relations(&[mcpmem::types::Relation {
            from: "ada".into(),
            to: "bob".into(),
            relation_type: "knows".into(),
        }])
        .unwrap();
    let mut deletion = None;
    for _ in 0..3 {
        let job = repo.claim_due(now_us(), 10_000_000).unwrap().unwrap();
        if job.owner_kind == OwnerKind::Relation {
            deletion = Some(job);
            break;
        }
    }
    let deletion = deletion.expect("the tombstone funnel enqueues a claimable delete");
    assert_eq!(
        deletion.operation,
        mcpmem_core::jobs::IndexOperation::Delete
    );
    {
        let vector = [1.0f32, 0.0];
        let owned = [(ChunkKind::Relation, vector.as_slice())];
        let refs: Vec<_> = owned.iter().collect();
        assert!(
            repo.commit_chunks(&deletion, now_us() + 1, Some(&refs), "indexer")
                .is_err(),
            "a delete must not carry chunks"
        );
    }
    assert!(
        repo.commit_chunks(&deletion, now_us() + 2, None, "indexer")
            .unwrap(),
        "the tombstoned mirror passes the delete fence"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM chunk_vector WHERE profile_id=?1 AND owner_kind='relation'",
            [profile.id.to_string()],
            |r| r.get::<_, i64>(0),
        )
        .unwrap(),
        0,
        "the delete commit removes the owner's chunk rows"
    );
}

#[test]
fn commit_chunks_refuses_a_payload_that_does_not_match_the_operation() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    graph
        .create_entities(&[Entity {
            name: "a".into(),
            entity_type: "Person".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let profile = profile();
    mcpmem_core::jobs::IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    let repo = mcpmem_core::jobs::IndexJobRepository::new(&conn);
    let job = repo.claim_due(now_us(), 10_000_000).unwrap().unwrap();
    assert_eq!(job.operation, mcpmem_core::jobs::IndexOperation::Upsert);
    // An upsert without chunks errors before any row is touched: it would
    // otherwise wipe the owner's chunk rows and complete.
    assert!(
        repo.commit_chunks(&job, now_us() + 1, None, "test")
            .is_err(),
        "an upsert needs at least one chunk"
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM chunk_vector", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0,
        "the refused commit touches no chunk rows"
    );
    // The refusal leaves the lease intact, so the correct payload commits on
    // the same job.
    {
        let vector = [1.0f32, 0.0];
        let owned = [(ChunkKind::Identity, vector.as_slice())];
        let refs: Vec<_> = owned.iter().collect();
        assert!(
            repo.commit_chunks(&job, now_us() + 2, Some(&refs), "test")
                .unwrap()
        );
    }
}

#[test]
fn stale_claim_cannot_commit_after_a_newer_claimant() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    graph
        .create_entities(&[Entity {
            name: "a".into(),
            entity_type: "Person".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let profile = profile();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    let repo = mcpmem_core::jobs::IndexJobRepository::new(&conn);
    let first = repo.claim_due(10, 1).unwrap().unwrap();
    let second = repo.claim_due(12, 10).unwrap().unwrap();
    assert!(
        !repo
            .commit_chunks(
                &first,
                12,
                Some(&[&(ChunkKind::Identity, &[1.0f32, 1.0])]),
                "test"
            )
            .unwrap()
    );
    assert!(
        repo.commit_chunks(
            &second,
            12,
            Some(&[&(ChunkKind::Identity, &[1.0f32, 1.0])]),
            "test"
        )
        .unwrap()
    );
}

struct WrongDimensions;
impl EmbeddingProvider for WrongDimensions {
    fn embed_texts(
        &self,
        _profile: &IndexProfile,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        Ok(texts.iter().map(|_| vec![1.0]).collect())
    }
}

struct SlowProvider;
impl EmbeddingProvider for SlowProvider {
    fn embed_texts(
        &self,
        profile: &IndexProfile,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        std::thread::sleep(Duration::from_millis(2));
        Ok(texts
            .iter()
            .map(|_| vec![1.0; profile.dimensions as usize])
            .collect())
    }
}

#[test]
fn expired_provider_call_cannot_commit_using_its_claim_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    graph
        .create_entities(&[Entity {
            name: "a".into(),
            entity_type: "Person".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let profile = profile();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    let report = IndexerWorker::new(&database, SlowProvider, Duration::from_secs(1))
        .with_lease_us(1)
        .run_once(now_us())
        .unwrap();
    assert_eq!(report.committed, 0);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM chunk_vector", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn profile_dimension_mismatch_is_retried_without_a_vector_write() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    graph
        .create_entities(&[Entity {
            name: "a".into(),
            entity_type: "Person".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let profile = profile();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    let report = IndexerWorker::new(&database, WrongDimensions, Duration::from_secs(1))
        .run_once(now_us())
        .unwrap();
    assert_eq!(report.retried, 1);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM chunk_vector", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn persistent_failure_dead_letters_and_stops_blocking_the_full_scan() {
    use mcpmem_core::jobs::{AnnGenerationRepository, IndexProfileRegistry};
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    graph
        .create_entities(&[Entity {
            name: "poisoned".into(),
            entity_type: "Person".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let profile = profile();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    let worker = IndexerWorker::new(&database, WrongDimensions, Duration::from_secs(1));
    // Each poll claims the same job and fails it. Every failure is a retry
    // until the attempt budget is spent, then the job is dead-lettered. The
    // worker schedules each retry 1s ahead, so the synthetic clock must
    // advance past it or no later poll will ever claim the job again.
    let start = now_us();
    let mut dead_seen = false;
    for i in 0..24 {
        let report = worker.run_once(start + i * 2_000_000).unwrap();
        dead_seen |= report.dead == 1;
        if dead_seen {
            break;
        }
    }
    assert!(
        dead_seen,
        "the job must be dead-lettered after max attempts"
    );
    let (state, attempts): (String, i64) = conn
        .query_row(
            "SELECT state, attempts FROM chunk_index_job WHERE owner_kind='entity' AND owner_id=1 AND profile_id=?1",
            [profile.id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "dead");
    assert!(attempts >= 8);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM chunk_vector", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0,
        "the dead-letter path leaves no chunk rows behind"
    );
    // The dead-lettered entity must not block the candidate any more: the
    // full scan verifies, and no vector was written for it.
    let generation = AnnGenerationRepository::new(&conn)
        .get(profile.id)
        .unwrap()
        .durable_generation;
    AnnGenerationRepository::new(&conn)
        .verify_full_scan(profile.id)
        .unwrap();
    AnnGenerationRepository::new(&conn)
        .mark_published(profile.id, generation)
        .unwrap();
    IndexProfileRegistry::new(&conn)
        .activate(profile.id)
        .unwrap();
    let vectors = VectorStore::new(&database, 2).unwrap();
    vectors.reconcile_managed_snapshot().unwrap();
    let hits = vectors
        .search_chunks(&[1.0f32, 1.0f32], 10, None, None)
        .unwrap();
    assert!(
        hits.is_empty(),
        "a dead-lettered poison must not stay served"
    );
    // A later write to the same entity re-enqueues it with a fresh budget.
    graph
        .add_observations(
            "poisoned",
            &[mcpmem::types::ObservationInput {
                body: "changed".into(),
                occurred_at_us: None,
            }],
        )
        .unwrap();
    let (state, attempts): (String, i64) = conn
        .query_row(
            "SELECT state, attempts FROM chunk_index_job WHERE owner_kind='entity' AND owner_id=1 AND profile_id=?1",
            [profile.id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "pending");
    assert_eq!(attempts, 0);
}

#[test]
fn worker_normalizes_l2_vectors_before_commit() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    graph
        .create_entities(&[Entity {
            name: "a".into(),
            entity_type: "Person".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let mut profile = profile();
    profile.normalization = Normalization::L2;
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    // A provider that returns approximately-unit vectors, as OpenAI does
    // (measured off by up to 5e-4). The stored-vector gate demands
    // |norm-1| < 1e-4; the worker must normalize before committing.
    struct ApproxUnit;
    impl EmbeddingProvider for ApproxUnit {
        fn embed_texts(
            &self,
            _profile: &IndexProfile,
            texts: &[String],
        ) -> Result<Vec<Vec<f32>>, ProviderError> {
            Ok(texts.iter().map(|_| vec![0.9996441, 0.0003]).collect())
        }
    }
    let report = IndexerWorker::new(&database, ApproxUnit, Duration::from_secs(1))
        .run_once(now_us())
        .unwrap();
    assert_eq!(report.committed, 1);
    let blob: Vec<u8> = conn
        .query_row(
            "SELECT blob FROM chunk_vector WHERE profile_id=?1 AND kind='identity' AND chunk_index=0",
            [profile.id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    let (chunks, _) = blob.as_chunks::<4>();
    let vector: Vec<f32> = chunks.iter().map(|b| f32::from_le_bytes(*b)).collect();
    let norm: f64 = vector.iter().map(|x| f64::from(*x).powi(2)).sum();
    assert!(
        (norm - 1.0).abs() < 1e-6,
        "stored vector must be unit norm, got {norm}"
    );
    assert_eq!(
        chunk_rows(&conn, &profile.id.to_string(), "identity"),
        1,
        "the entity commits exactly one identity chunk"
    );
}

#[test]
fn ollama_rejects_url_credentials() {
    assert!(
        mcpmem_indexer::OllamaProvider::new("http://token@127.0.0.1:11434", Duration::from_secs(1))
            .is_err()
    );
}

#[test]
fn provider_registry_rejects_unknown_profile_kind() {
    let registry = mcpmem_indexer::ProviderRegistry::new(None, None);
    let mut unknown = profile();
    unknown.provider_kind = "unknown".into();
    let error = registry.embed(&unknown, &[]).unwrap_err();
    assert!(error.to_string().contains("unsupported embedding provider"));
}

#[test]
#[cfg(not(feature = "bedrock"))]
fn provider_registry_rejects_bedrock_without_the_bedrock_feature() {
    let registry = mcpmem_indexer::ProviderRegistry::new(None, None);
    let mut bedrock = profile();
    bedrock.provider_kind = "bedrock".into();
    let error = registry.embed(&bedrock, &[]).unwrap_err();
    assert_eq!(
        error.to_string(),
        "provider request failed: unsupported embedding provider 'bedrock'"
    );
}

#[test]
fn candidate_rebuild_commits_into_both_profiles_and_activates() {
    use mcpmem_core::jobs::{AnnGenerationRepository, IndexProfileRegistry};
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    graph
        .create_entities(&[Entity {
            name: "alice".into(),
            entity_type: "Person".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let first = profile();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&first)
        .unwrap();
    let worker = IndexerWorker::new(&database, FixedProvider, Duration::from_secs(1));
    worker.run_once(now_us()).unwrap();
    assert_eq!(
        chunk_rows(&conn, &first.id.to_string(), "identity"),
        1,
        "the candidate commits one identity chunk per entity"
    );
    AnnGenerationRepository::new(&conn)
        .verify_full_scan(first.id)
        .unwrap();
    let generation = AnnGenerationRepository::new(&conn)
        .get(first.id)
        .unwrap()
        .durable_generation;
    assert!(
        AnnGenerationRepository::new(&conn)
            .mark_published(first.id, generation)
            .unwrap()
    );
    IndexProfileRegistry::new(&conn).activate(first.id).unwrap();

    let mut second = profile();
    second.model = "fixed-v2".into();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&second)
        .unwrap();
    graph
        .create_entities(&[Entity {
            name: "bob".into(),
            entity_type: "Person".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
    for _ in 0..5 {
        worker.run_once(now_us()).unwrap();
    }
    // Enqueues route to every serving AND candidate profile, so the rebuild
    // commits bob's chunk into the still-serving profile as well. The first
    // profile's existing alice chunk is never replaced or removed.
    assert_eq!(
        chunk_rows(&conn, &first.id.to_string(), "identity"),
        2,
        "writes during a rebuild also land in the still-serving profile"
    );
    assert_eq!(
        chunk_rows(&conn, &second.id.to_string(), "identity"),
        2,
        "the candidate commits alice and bob"
    );
    assert_eq!(
        chunk_rows(&conn, &second.id.to_string(), "observation"),
        0,
        "neither entity has observations, so no observation chunks"
    );
    // The candidate lifecycle still runs: reconciliation verifies the chunk
    // gate and activates once the full scan is current.
    let store = VectorStore::new(&database, 2).unwrap();
    store.reconcile_managed_snapshot().unwrap();
    assert!(
        matches!(IndexProfileRegistry::new(&conn).state("default").unwrap(), mcpmem_core::jobs::StoreState::Active(id) if id == second.id)
    );
    assert_eq!(
        chunk_rows(&conn, &second.id.to_string(), "identity"),
        2,
        "activation leaves the candidate's chunk rows in place"
    );
}

#[test]
fn active_profile_snapshot_refreshes_after_a_durable_generation_change() {
    use mcpmem_core::jobs::{AnnGenerationRepository, IndexProfileRegistry};
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    graph
        .create_entities(&[Entity {
            name: "alice".into(),
            entity_type: "Person".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let profile = profile();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    let worker = IndexerWorker::new(&database, FixedProvider, Duration::from_secs(1));
    worker.run_once(now_us()).unwrap();
    AnnGenerationRepository::new(&conn)
        .verify_full_scan(profile.id)
        .unwrap();
    let generation = AnnGenerationRepository::new(&conn)
        .get(profile.id)
        .unwrap()
        .durable_generation;
    AnnGenerationRepository::new(&conn)
        .mark_published(profile.id, generation)
        .unwrap();
    IndexProfileRegistry::new(&conn)
        .activate(profile.id)
        .unwrap();
    let vectors = VectorStore::new(&database, 2).unwrap();
    vectors.reconcile_managed_snapshot().unwrap();
    assert_eq!(
        chunk_rows(&conn, &profile.id.to_string(), "identity"),
        1,
        "one identity chunk after the first commit"
    );
    graph
        .create_entities(&[Entity {
            name: "bob".into(),
            entity_type: "Person".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
    worker.run_once(now_us()).unwrap();
    assert_eq!(
        chunk_rows(&conn, &profile.id.to_string(), "identity"),
        2,
        "the second entity adds its identity chunk"
    );
    // Simulate a crash after durable commit and before reader swap. An idle
    // worker poll has no job; the durable chunk rows survive either way.
    let reopened = VectorStore::new(&database, 2).unwrap();
    worker.run_once(now_us()).unwrap();
    reopened.reconcile_managed_snapshot().unwrap();
    assert_eq!(
        chunk_rows(&conn, &profile.id.to_string(), "identity"),
        2,
        "reconciliation restores nothing that the commit did not persist"
    );
}

fn taxonomy_fixture(dir: &Path) -> (std::path::PathBuf, rusqlite::Connection, IndexProfile) {
    let database = dir.join("memory.db");
    let conn = rusqlite::Connection::open(&database).unwrap();
    mcpmem_core::schema::initialize_database(&conn).unwrap();
    let profile = profile();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    (database, conn, profile)
}

fn seed_taxonomy_job(
    conn: &rusqlite::Connection,
    kind: i64,
    id: i64,
    revision: i64,
    operation: &str,
    profile: &Uuid,
) {
    conn.execute(
        "INSERT INTO taxonomy_job(subject_kind, subject_id, profile_id, subject_revision, operation)
         VALUES(?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![kind, id, profile.to_string(), revision, operation],
    )
    .unwrap();
}

#[test]
fn worker_embeds_a_pending_taxonomy_job_into_taxonomy_vector() {
    let dir = tempfile::tempdir().unwrap();
    let (database, conn, profile) = taxonomy_fixture(dir.path());
    conn.execute(
        "INSERT INTO type_dict(id, kind, name, revision) VALUES(1, 0, 'person', 3)",
        [],
    )
    .unwrap();
    seed_taxonomy_job(&conn, 0, 1, 3, "upsert", &profile.id);
    let report = IndexerWorker::new(&database, FixedProvider, Duration::from_secs(5))
        .run_once(now_us())
        .unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.committed, 1);
    let (kind, id, revision, blob): (i64, i64, i64, Vec<u8>) = conn
        .query_row(
            "SELECT subject_kind, subject_id, subject_revision, blob FROM taxonomy_vector WHERE profile_id=?1",
            [profile.id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!((kind, id, revision), (0, 1, 3));
    assert_eq!(blob.len(), 8);
    let state: String = conn
        .query_row(
            "SELECT state FROM taxonomy_job WHERE subject_kind=0 AND subject_id=1 AND profile_id=?1",
            [profile.id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "done");
}

#[test]
fn stale_taxonomy_job_fails_the_fence_without_a_vector_row() {
    let dir = tempfile::tempdir().unwrap();
    let (database, conn, profile) = taxonomy_fixture(dir.path());
    conn.execute(
        "INSERT INTO type_dict(id, kind, name, revision) VALUES(1, 0, 'person', 5)",
        [],
    )
    .unwrap();
    // The job names a revision three behind the source row. The document
    // fence reads the mismatch as vanished, so the worker must route the job
    // through the retry path without writing a vector.
    seed_taxonomy_job(&conn, 0, 1, 2, "upsert", &profile.id);
    let report = IndexerWorker::new(&database, FixedProvider, Duration::from_secs(5))
        .run_once(now_us())
        .unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.committed, 0);
    assert_eq!(report.retried, 1);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM taxonomy_vector", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let (state, attempts, last_error, next_attempt): (String, i64, Option<String>, i64) = conn
        .query_row(
            "SELECT state, attempts, last_error, next_attempt_us FROM taxonomy_job WHERE subject_kind=0 AND subject_id=1 AND profile_id=?1",
            [profile.id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(state, "pending");
    assert_eq!(attempts, 1);
    assert!(last_error.is_some());
    assert!(next_attempt > 0);
}

#[test]
fn vanished_taxonomy_subject_retries_then_dead_letters() {
    let dir = tempfile::tempdir().unwrap();
    let (database, conn, profile) = taxonomy_fixture(dir.path());
    // The subject has no type_dict row, so the document fence reads it as
    // vanished. A stale vector row stays until the job is dead-lettered,
    // mirroring the entity dead-letter cleanup.
    conn.execute(
        "INSERT INTO taxonomy_vector(profile_id, subject_kind, subject_id, subject_revision, blob, created_at_us, source) VALUES(?1, 0, 1, 3, X'0000000000000000', 1, 'seed')",
        [profile.id.to_string()],
    )
    .unwrap();
    seed_taxonomy_job(&conn, 0, 1, 3, "upsert", &profile.id);
    let worker = IndexerWorker::new(&database, FixedProvider, Duration::from_secs(1));
    // Each poll claims the same job and fails it. Every failure is a retry
    // until the attempt budget is spent, then the job is dead-lettered. The
    // worker schedules each retry 1s ahead, so the synthetic clock must
    // advance past it or no later poll will ever claim the job again.
    let start = now_us();
    let mut dead_seen = false;
    for i in 0..24 {
        let report = worker.run_once(start + i * 2_000_000).unwrap();
        dead_seen |= report.dead == 1;
        if dead_seen {
            break;
        }
    }
    assert!(
        dead_seen,
        "the job must be dead-lettered after max attempts"
    );
    let (state, attempts): (String, i64) = conn
        .query_row(
            "SELECT state, attempts FROM taxonomy_job WHERE subject_kind=0 AND subject_id=1 AND profile_id=?1",
            [profile.id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "dead");
    assert!(attempts >= 8);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM taxonomy_vector", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

fn entity(name: &str, entity_type: &str) -> Entity {
    Entity {
        name: name.into(),
        entity_type: entity_type.into(),
        observations: vec![],
        attributes: None,
    }
}

fn relation(from: &str, to: &str, relation_type: &str) -> mcpmem::types::Relation {
    mcpmem::types::Relation {
        from: from.into(),
        to: to.into(),
        relation_type: relation_type.into(),
    }
}

/// Begins a candidate rebuild for a fresh profile on the graph database, the
/// way the reconcile test does. Mutations enqueue their chunk and taxonomy
/// jobs against the candidate; the worker then serves them.
fn seed_profile(database: &Path) -> IndexProfile {
    let conn = rusqlite::Connection::open(database).unwrap();
    let profile = profile();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    profile
}

fn vs_of(database: &Path) -> VectorStore {
    VectorStore::new(database, 2).unwrap()
}

#[test]
fn taxonomy_relation_snapshot_derives_from_chunk_vector() {
    // Seed entities + relation through the graph, run the worker until the
    // relation chunk job commits, then assert search_taxonomy kind=2 finds
    // it. The kind-2 snapshot reads chunk_vector, never taxonomy_vector.
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    seed_profile(&database);
    graph
        .create_entities(&[entity("ada", "Person"), entity("bob", "Person")])
        .unwrap();
    graph
        .create_relations(&[relation("ada", "bob", "knows")])
        .unwrap();
    let worker = IndexerWorker::new(&database, FixedProvider, Duration::from_secs(5));
    // Chunk jobs claim before taxonomy jobs, and the entity jobs before the
    // relation job. Drain the queue: ada, bob and the relation each get one
    // chunk job, then the two type subjects get one taxonomy job each.
    let mut claimed = 0;
    for _ in 0..6 {
        claimed += worker.run_once(now_us()).unwrap().claimed;
    }
    assert_eq!(claimed, 5, "three chunk jobs and two taxonomy jobs drain");
    let store = vs_of(&database);
    store.reconcile_managed_snapshot().unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    store
        .adopt_taxonomy(&IndexProfileRegistry::new(&conn), false)
        .unwrap();
    let hits = store
        .search_taxonomy(TaxonomyKind::Relation, &[1.0, 0.0], 5)
        .unwrap();
    assert_eq!(hits.len(), 1, "one relation chunk in the kind-2 snapshot");
}

#[test]
fn reconcile_adopts_taxonomy_after_the_worker_cycle() {
    // D10 vertical slice: a write enqueues a taxonomy subject through the
    // mutation path, the worker embeds it, and reconcile_managed_snapshot
    // adopts the kind so search_taxonomy serves it. The taxonomy commit
    // lands after the entity snapshot was published, so adoption must also
    // run on a poll where the entity state did not move.
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    let conn = rusqlite::Connection::open(&database).unwrap();
    let profile = profile();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    // The write names one new entity type, which enqueues a kind-0 taxonomy
    // subject (and an entity job for the entity itself).
    graph
        .create_entities(&[Entity {
            name: "alice".into(),
            entity_type: "Person".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
    let worker = IndexerWorker::new(&database, FixedProvider, Duration::from_secs(5));
    let vectors = VectorStore::new(&database, 2).unwrap();
    // One poll embeds the entity; the reconcile after it publishes the empty
    // candidate (no taxonomy vector yet) and adopts an empty taxonomy.
    assert_eq!(worker.run_once(now_us()).unwrap().committed, 1);
    vectors.reconcile_managed_snapshot().unwrap();
    assert_eq!(
        chunk_rows(&conn, &profile.id.to_string(), "identity"),
        1,
        "the entity commit lands one identity chunk"
    );
    assert!(
        vectors
            .search_taxonomy(TaxonomyKind::EntityType, &[1.0, 1.0], 10)
            .unwrap()
            .is_empty()
    );
    // The next poll embeds the taxonomy subject. Its commit advances only
    // the kind generation; the entity snapshot still matches the profile.
    assert_eq!(worker.run_once(now_us()).unwrap().committed, 1);
    {
        let (kind, subject): (i64, i64) = conn
            .query_row(
                "SELECT subject_kind, subject_id FROM taxonomy_vector WHERE profile_id=?1",
                [profile.id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((kind, subject), (0, 1));
    }
    vectors.reconcile_managed_snapshot().unwrap();
    let hits = vectors
        .search_taxonomy(TaxonomyKind::EntityType, &[1.0, 1.0], 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, 1);
    assert!(hits[0].1.abs() < 1e-6);
    // The entity side is untouched by the taxonomy adoption.
    assert_eq!(
        chunk_rows(&conn, &profile.id.to_string(), "identity"),
        1,
        "the taxonomy commit does not disturb the entity chunk"
    );
}
