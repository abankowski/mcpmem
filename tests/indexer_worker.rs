#![cfg(feature = "indexer")]

use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Duration;

use indexer_worker::{CanonicalDocument, EmbeddingProvider, IndexerWorker, ProviderError};
use mcp_memory::config::{Durability, SqliteTuning};
use mcp_memory::kg::GraphHandle;
use mcp_memory::types::Entity;
use mcp_memory::vector_store::VectorStore;
use memory_core::jobs::{DistanceMetric, IndexProfile, IndexProfileRegistry, Normalization};
use uuid::Uuid;

struct FixedProvider;

impl EmbeddingProvider for FixedProvider {
    fn embed(
        &self,
        profile: &IndexProfile,
        documents: &[CanonicalDocument],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        Ok(documents
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
    let first = memory_core::jobs::IndexJobRepository::new(&conn)
        .claim_due(10, 1)
        .unwrap()
        .unwrap();
    let second = memory_core::jobs::IndexJobRepository::new(&conn)
        .claim_due(12, 10)
        .unwrap()
        .unwrap();
    assert!(
        !memory_core::jobs::IndexJobRepository::new(&conn)
            .commit_vector(&first, 12, Some(&[1.0, 1.0]), "test")
            .unwrap()
    );
    assert!(
        memory_core::jobs::IndexJobRepository::new(&conn)
            .commit_vector(&second, 12, Some(&[1.0, 1.0]), "test")
            .unwrap()
    );
}

struct WrongDimensions;
impl EmbeddingProvider for WrongDimensions {
    fn embed(
        &self,
        _profile: &IndexProfile,
        documents: &[CanonicalDocument],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        Ok(documents.iter().map(|_| vec![1.0]).collect())
    }
}

struct SlowProvider;
impl EmbeddingProvider for SlowProvider {
    fn embed(
        &self,
        profile: &IndexProfile,
        documents: &[CanonicalDocument],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        std::thread::sleep(Duration::from_millis(2));
        Ok(documents
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
fn ollama_rejects_url_credentials() {
    assert!(
        indexer_worker::OllamaProvider::new("http://token@127.0.0.1:11434", Duration::from_secs(1))
            .is_err()
    );
}

#[test]
fn provider_registry_rejects_unknown_profile_kind() {
    let registry = indexer_worker::ProviderRegistry::new(None, None);
    let mut unknown = profile();
    unknown.provider_kind = "unknown".into();
    let error = registry.embed(&unknown, &[]).unwrap_err();
    assert!(error.to_string().contains("unsupported embedding provider"));
}

#[test]
#[cfg(not(feature = "bedrock"))]
fn provider_registry_rejects_bedrock_without_the_bedrock_feature() {
    let registry = indexer_worker::ProviderRegistry::new(None, None);
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
    use memory_core::jobs::{AnnGenerationRepository, IndexProfileRegistry};
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
        matches!(IndexProfileRegistry::new(&conn).state("default").unwrap(), memory_core::jobs::StoreState::Active(id) if id == second.id)
    );
}

#[test]
fn active_profile_snapshot_refreshes_after_a_durable_generation_change() {
    use memory_core::jobs::{AnnGenerationRepository, IndexProfileRegistry};
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
