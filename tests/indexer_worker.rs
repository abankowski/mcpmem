#![cfg(feature = "indexer")]

use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::kg::GraphHandle;
use mcpmem::types::EntityInput as Entity;
use mcpmem::vector_store::VectorStore;
use mcpmem_core::jobs::{DistanceMetric, IndexProfile, IndexProfileRegistry, Normalization};
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
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let profile = profile();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    let worker = IndexerWorker::new(&database, FixedProvider, Duration::from_secs(5));
    let report = worker.run_once(now_us()).unwrap();
    assert_eq!(report.committed, 1);
    let row: (i64, Vec<u8>) = conn
        .query_row(
            "SELECT entity_revision, blob FROM profile_vector WHERE profile_id=?1",
            [profile.id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row.0, 1);
    assert_eq!(row.1.len(), 8);
}

#[test]
fn worker_indexes_observation_bodies_without_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    graph
        .create_entities(&[Entity {
            name: "Ada".into(),
            entity_type: "Person".into(),
            observations: vec!["first programmer".into()],
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.execute(
        "UPDATE observation SET occurred_us=123, origin_entity_name='legacy source'",
        [],
    )
    .unwrap();
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
    assert_eq!(worker.run_once(now_us()).unwrap().committed, 1);
    let texts = captured.lock();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0], "Ada\nPerson\nfirst programmer");
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
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let profile = profile();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&profile)
        .unwrap();
    let first = mcpmem_core::jobs::IndexJobRepository::new(&conn)
        .claim_due(10, 1)
        .unwrap()
        .unwrap();
    let second = mcpmem_core::jobs::IndexJobRepository::new(&conn)
        .claim_due(12, 10)
        .unwrap()
        .unwrap();
    assert!(
        !mcpmem_core::jobs::IndexJobRepository::new(&conn)
            .commit_vector(&first, 12, Some(&[1.0, 1.0]), "test")
            .unwrap()
    );
    assert!(
        mcpmem_core::jobs::IndexJobRepository::new(&conn)
            .commit_vector(&second, 12, Some(&[1.0, 1.0]), "test")
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
        conn.query_row("SELECT COUNT(*) FROM profile_vector", [], |row| row
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
        conn.query_row("SELECT COUNT(*) FROM profile_vector", [], |r| r
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
            "SELECT state, attempts FROM index_job WHERE entity_id=1 AND profile_id=?1",
            [profile.id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "dead");
    assert!(attempts >= 8);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM profile_vector", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
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
    assert_eq!(vectors.search_embeddings(&[1.0, 1.0], 10).unwrap().len(), 0);
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
            "SELECT state, attempts FROM index_job WHERE entity_id=1 AND profile_id=?1",
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
            "SELECT blob FROM profile_vector WHERE profile_id=?1",
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
fn search_keeps_serving_snapshot_until_candidate_activation() {
    use mcpmem_core::jobs::{AnnGenerationRepository, IndexProfileRegistry};
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.db");
    let graph = setup(&database);
    graph
        .create_entities(&[Entity {
            name: "alice".into(),
            entity_type: "Person".into(),
            observations: vec![],
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    let first = profile();
    IndexProfileRegistry::new(&conn)
        .begin_rebuild(&first)
        .unwrap();
    let worker = IndexerWorker::new(&database, FixedProvider, Duration::from_secs(1));
    worker.run_once(now_us()).unwrap();
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
    let indexer_vectors = VectorStore::new(&database, 2).unwrap();
    let mcp_vectors = VectorStore::new(&database, 2).unwrap();
    indexer_vectors.reconcile_managed_snapshot().unwrap();
    mcp_vectors.reconcile_managed_snapshot().unwrap();
    assert_eq!(
        mcp_vectors
            .search_embeddings(&[1.0, 1.0], 10)
            .unwrap()
            .len(),
        1
    );

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
        }])
        .unwrap();
    for _ in 0..5 {
        worker.run_once(now_us()).unwrap();
    }
    assert_eq!(
        mcp_vectors
            .search_embeddings(&[1.0, 1.0], 10)
            .unwrap()
            .len(),
        1
    );
    indexer_vectors.reconcile_managed_snapshot().unwrap();
    assert_eq!(
        indexer_vectors
            .search_embeddings(&[1.0, 1.0], 10)
            .unwrap()
            .len(),
        2
    );
    // Simulates the MCP role's bounded background refresh in a separate process.
    assert_eq!(
        mcp_vectors
            .search_embeddings(&[1.0, 1.0], 10)
            .unwrap()
            .len(),
        1
    );
    mcp_vectors.reconcile_managed_snapshot().unwrap();
    assert_eq!(
        mcp_vectors
            .search_embeddings(&[1.0, 1.0], 10)
            .unwrap()
            .len(),
        2
    );
    assert!(
        matches!(IndexProfileRegistry::new(&conn).state("default").unwrap(), mcpmem_core::jobs::StoreState::Active(id) if id == second.id)
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
    assert_eq!(vectors.search_embeddings(&[1.0, 1.0], 10).unwrap().len(), 1);
    graph
        .create_entities(&[Entity {
            name: "bob".into(),
            entity_type: "Person".into(),
            observations: vec![],
        }])
        .unwrap();
    worker.run_once(now_us()).unwrap();
    assert_eq!(vectors.search_embeddings(&[1.0, 1.0], 10).unwrap().len(), 1);
    // Simulate a crash after durable commit and before reader swap. An idle
    // worker poll has no job, but reconciliation still restores the snapshot.
    let reopened = VectorStore::new(&database, 2).unwrap();
    worker.run_once(now_us()).unwrap();
    reopened.reconcile_managed_snapshot().unwrap();
    assert_eq!(
        reopened.search_embeddings(&[1.0, 1.0], 10).unwrap().len(),
        2
    );
}

fn taxonomy_fixture(dir: &Path) -> (std::path::PathBuf, rusqlite::Connection, IndexProfile) {
    let database = dir.join("memory.db");
    let conn = rusqlite::Connection::open(&database).unwrap();
    mcpmem_core::schema::initialize_database(&conn).unwrap();
    let profile = profile();
    IndexProfileRegistry::new(&conn).begin_rebuild(&profile).unwrap();
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
fn worker_removes_the_vector_row_for_a_taxonomy_delete_job() {
    let dir = tempfile::tempdir().unwrap();
    let (database, conn, profile) = taxonomy_fixture(dir.path());
    // A relation tombstone at revision 5 with a vector row to remove.
    conn.execute(
        "INSERT INTO taxonomy_relation(id, from_id, to_id, type_id, revision, deleted) VALUES(10, 1, 2, 3, 5, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO taxonomy_vector(profile_id, subject_kind, subject_id, subject_revision, blob, created_at_us, source) VALUES(?1, 2, 10, 5, X'0000000000000000', 1, 'seed')",
        [profile.id.to_string()],
    )
    .unwrap();
    seed_taxonomy_job(&conn, 2, 10, 5, "delete", &profile.id);
    let report = IndexerWorker::new(&database, FixedProvider, Duration::from_secs(5))
        .run_once(now_us())
        .unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.committed, 1);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM taxonomy_vector", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let state: String = conn
        .query_row(
            "SELECT state FROM taxonomy_job WHERE subject_kind=2 AND subject_id=10 AND profile_id=?1",
            [profile.id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "done");
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
    assert!(dead_seen, "the job must be dead-lettered after max attempts");
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
