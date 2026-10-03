//! Adapts the existing MCP transport to the reusable runtime supervisor.

#[cfg(any(feature = "indexer", feature = "webhooks", feature = "extractor"))]
use parking_lot::Mutex;
#[cfg(feature = "webhooks")]
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

pub use mcpmem_runtime::{
    AppServices, ConfigError, RoleFuture, RoleLifecycle, RoleService, RoleSet, RunningRoles,
    RuntimeComposition, RuntimeError, RuntimeRole,
};

use crate::Transport;
use crate::server::MCPServer;

pub struct McpTransportService {
    server: Arc<MCPServer>,
    transport: Transport,
    bind_addr: String,
}

impl McpTransportService {
    pub const fn new(server: Arc<MCPServer>, transport: Transport, bind_addr: String) -> Self {
        Self {
            server,
            transport,
            bind_addr,
        }
    }
}

impl RoleService for McpTransportService {
    fn run(&self) -> mcpmem_runtime::RoleFuture {
        let server = self.server.clone();
        let transport = self.transport;
        let bind_addr = self.bind_addr.clone();

        Box::pin(async move {
            // An indexer in another process can publish a new durable
            // snapshot. Refresh each open workspace, not only the legacy one.
            let refresher = server.workspace_handles();
            let refresh_enabled = refresher.legacy_vs().is_some();
            let refresh_task = tokio::spawn(async move {
                if !refresh_enabled {
                    return;
                }
                loop {
                    let handles = Arc::clone(&refresher);
                    let _ = tokio::task::spawn_blocking(move || {
                        for vectors in handles.cached_vectors() {
                            if let Err(error) = vectors.reconcile_managed_snapshot() {
                                tracing::debug!(%error, "vector snapshot refresh deferred");
                            }
                        }
                    })
                    .await;
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            });
            let outcome = match transport {
                Transport::Stdio => server.run_stdio().await,
                Transport::Http => server.run_http(&bind_addr).await,
            };
            refresh_task.abort();
            outcome.map_err(|error| RuntimeError::RoleFailed {
                role: RuntimeRole::Mcp,
                message: error.to_string(),
            })
        })
    }
}

/// Select one trusted graph path without materializing the full registry.
/// Rows are read anew so new workspaces enter the schedule without a process
/// restart; the cursor advances one row per turn and wraps.
#[cfg(any(feature = "indexer", feature = "webhooks", feature = "extractor"))]
fn next_registered_path(
    registry: &crate::workspace::WorkspaceRegistry,
    cursor: &mut usize,
) -> Result<Option<(String, std::path::PathBuf)>, crate::workspace::WorkspaceError> {
    registry.next_path(cursor)
}

#[cfg(feature = "webhooks")]
type WebhookFactory = dyn Fn(&std::path::Path) -> Arc<dyn mcpmem_webhook::WorkerPoll> + Send + Sync;

#[cfg(feature = "webhooks")]
#[derive(Clone)]
enum WebhookTarget {
    Disabled,
    Single(Arc<dyn mcpmem_webhook::WorkerPoll>),
    Workspaces(Arc<WorkspaceWebhookWorkers>),
}

#[cfg(feature = "webhooks")]
struct WorkspaceWebhookWorkers {
    registry: Arc<crate::workspace::WorkspaceRegistry>,
    factory: Arc<WebhookFactory>,
    state: Mutex<WebhookState>,
}

#[cfg(feature = "webhooks")]
#[derive(Default)]
struct WebhookState {
    cursor: usize,
    workers: HashMap<String, Arc<dyn mcpmem_webhook::WorkerPoll>>,
    recent: VecDeque<String>,
    audited: HashSet<String>,
    warned_empty: bool,
}

#[cfg(feature = "webhooks")]
impl WorkspaceWebhookWorkers {
    fn worker_for(
        &self,
        id: &str,
        path: &std::path::Path,
    ) -> (Arc<dyn mcpmem_webhook::WorkerPoll>, bool) {
        let mut state = self.state.lock();
        let worker = if let Some(worker) = state.workers.get(id) {
            Arc::clone(worker)
        } else {
            if state.workers.len() >= crate::workspace::WorkspaceHandles::BOUND
                && let Some(oldest) = state.recent.pop_front()
            {
                state.workers.remove(&oldest);
            }
            let worker = (self.factory)(path);
            state.workers.insert(id.to_owned(), Arc::clone(&worker));
            worker
        };
        state.recent.retain(|key| key != id);
        state.recent.push_back(id.to_owned());
        let audit = state.audited.insert(id.to_owned());
        (worker, audit)
    }

    fn audit_existing(&self) -> Result<(), crate::errors::MCSError> {
        for (id, path) in self
            .registry
            .all_paths()
            .map_err(|error| crate::errors::MCSError::MemoryError(error.to_string()))?
        {
            let (worker, audit) = self.worker_for(&id, &path);
            if audit {
                log_webhook_audits(&id, worker.audit_subscriptions());
            }
        }
        Ok(())
    }

    fn poll(&self, now_us: i64) -> Result<mcpmem_webhook::DeliveryReport, crate::errors::MCSError> {
        let selected = {
            let mut state = self.state.lock();
            next_registered_path(&self.registry, &mut state.cursor)
                .map_err(|error| crate::errors::MCSError::MemoryError(error.to_string()))?
        };
        let Some((id, path)) = selected else {
            let mut state = self.state.lock();
            if !state.warned_empty {
                tracing::warn!("webhook worker has no registered workspace to poll");
                state.warned_empty = true;
            }
            return Ok(mcpmem_webhook::DeliveryReport::default());
        };
        let (worker, audit) = self.worker_for(&id, &path);
        if audit {
            log_webhook_audits(&id, worker.audit_subscriptions());
        }
        worker
            .poll(now_us)
            .map_err(|error| crate::errors::MCSError::MemoryError(error.to_string()))
    }
}

#[cfg(feature = "webhooks")]
fn log_webhook_audits(
    workspace_id: &str,
    result: Result<Vec<mcpmem_webhook::SubscriptionAudit>, mcpmem_webhook::WorkerError>,
) {
    match result {
        Ok(audits) => {
            if audits.is_empty() {
                tracing::info!(workspace_id, "no webhook subscriptions are registered");
            }
            for audit in audits {
                match (audit.enabled, audit.outcome) {
                    (true, Ok(())) => tracing::info!(
                        workspace_id,
                        subscription_id = %audit.subscription_id,
                        endpoint = %audit.endpoint,
                        "webhook subscription ready"
                    ),
                    (true, Err(reason)) => tracing::warn!(
                        workspace_id,
                        subscription_id = %audit.subscription_id,
                        endpoint = %audit.endpoint,
                        secret_ref = %audit.secret_ref,
                        %reason,
                        "webhook subscription is misconfigured"
                    ),
                    (false, _) => tracing::info!(
                        workspace_id,
                        subscription_id = %audit.subscription_id,
                        endpoint = %audit.endpoint,
                        "webhook subscription registered but disabled"
                    ),
                }
            }
        }
        Err(error) => tracing::warn!(
            workspace_id,
            %error,
            "webhook subscription audit failed"
        ),
    }
}

#[cfg(feature = "webhooks")]
pub struct WebhookService {
    target: WebhookTarget,
}

#[cfg(feature = "webhooks")]
impl WebhookService {
    pub const fn disabled() -> Self {
        Self {
            target: WebhookTarget::Disabled,
        }
    }

    pub fn new(worker: Arc<dyn mcpmem_webhook::WorkerPoll>) -> Self {
        Self {
            target: WebhookTarget::Single(worker),
        }
    }

    /// Preserve the one-file constructor for callers with one graph.
    pub fn with_config(
        database: impl Into<std::path::PathBuf>,
        config: mcpmem_webhook::WebhookConfigFile,
    ) -> Result<Self, crate::errors::MCSError> {
        let worker = mcpmem_webhook::WebhookWorker::new(
            database.into(),
            mcpmem_webhook::HttpsConnector::production(),
            mcpmem_webhook::StaticSecretProvider(config.secrets),
            config.allowlist,
            mcpmem_webhook::SystemResolver,
        )
        .with_allow_private_addresses(config.allow_private_addresses)
        .with_max_body_bytes(config.max_body_bytes);
        log_webhook_audits("legacy", worker.audit_subscriptions());
        Ok(Self::new(Arc::new(worker)))
    }

    /// Poll one registered path per turn with real path-bound workers.
    pub fn with_workspaces(
        registry: Arc<crate::workspace::WorkspaceRegistry>,
        worker_factory: Arc<WebhookFactory>,
    ) -> Self {
        Self {
            target: WebhookTarget::Workspaces(Arc::new(WorkspaceWebhookWorkers {
                registry,
                factory: worker_factory,
                state: Mutex::new(WebhookState::default()),
            })),
        }
    }

    /// Audit all registered files at startup. New files get one audit at
    /// their first poll.
    pub fn with_workspace_config(
        registry: Arc<crate::workspace::WorkspaceRegistry>,
        config: mcpmem_webhook::WebhookConfigFile,
    ) -> Result<Self, crate::errors::MCSError> {
        let config = Arc::new(config);
        let worker_factory: Arc<WebhookFactory> = Arc::new(move |path| {
            Arc::new(
                mcpmem_webhook::WebhookWorker::new(
                    path,
                    mcpmem_webhook::HttpsConnector::production(),
                    mcpmem_webhook::StaticSecretProvider(config.secrets.clone()),
                    config.allowlist.clone(),
                    mcpmem_webhook::SystemResolver,
                )
                .with_allow_private_addresses(config.allow_private_addresses)
                .with_max_body_bytes(config.max_body_bytes),
            )
        });
        let service = Self::with_workspaces(registry, worker_factory);
        if let WebhookTarget::Workspaces(workers) = &service.target {
            workers.audit_existing()?;
        }
        Ok(service)
    }

    pub fn run_once(
        &self,
        now_us: i64,
    ) -> Result<mcpmem_webhook::DeliveryReport, crate::errors::MCSError> {
        match &self.target {
            WebhookTarget::Disabled => Err(crate::errors::MCSError::MemoryError(
                "webhook worker is not configured".into(),
            )),
            WebhookTarget::Single(worker) => worker
                .poll(now_us)
                .map_err(|error| crate::errors::MCSError::MemoryError(error.to_string())),
            WebhookTarget::Workspaces(workers) => workers.poll(now_us),
        }
    }
}

#[cfg(feature = "webhooks")]
impl RoleService for WebhookService {
    fn run(&self) -> mcpmem_runtime::RoleFuture {
        let target = self.target.clone();
        Box::pin(async move {
            if matches!(&target, WebhookTarget::Disabled) {
                return Err(RuntimeError::RoleFailed {
                    role: RuntimeRole::Webhooks,
                    message: "webhook worker is not configured".into(),
                });
            }
            loop {
                let target = target.clone();
                let result = tokio::task::spawn_blocking(move || {
                    WebhookService { target }.run_once(mcpmem_core::events::now_us())
                })
                .await
                .map_err(|error| RuntimeError::RoleFailed {
                    role: RuntimeRole::Webhooks,
                    message: error.to_string(),
                })?;
                if let Err(error) = result {
                    tracing::error!(
                        %error,
                        "webhook poll failed for one workspace; the next workspace stays scheduled"
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        })
    }
}

#[cfg(feature = "indexer")]
#[derive(Default)]
struct WorkspaceIndexerState {
    cursor: usize,
    adopted: std::collections::HashSet<String>,
    warned_empty: bool,
}

#[cfg(feature = "indexer")]
enum IndexerTarget {
    Single {
        database: std::path::PathBuf,
        vectors: Option<Arc<crate::vector_store::VectorStore>>,
    },
    Workspaces {
        registry: Arc<crate::workspace::WorkspaceRegistry>,
        handles: Arc<crate::workspace::WorkspaceHandles>,
        profile: Option<crate::config_file::ProfileSpec>,
        state: Mutex<WorkspaceIndexerState>,
    },
}

#[cfg(feature = "indexer")]
pub struct IndexerService {
    target: Arc<IndexerTarget>,
    provider: Arc<mcpmem_indexer::ProviderRegistry>,
}

#[cfg(feature = "indexer")]
fn run_indexer_graph<P: mcpmem_indexer::EmbeddingProvider>(
    path: &std::path::Path,
    vectors: Option<Arc<crate::vector_store::VectorStore>>,
    provider: P,
    now_us: i64,
    timeout: std::time::Duration,
) -> Result<(), String> {
    mcpmem_indexer::IndexerWorker::new(path, provider, timeout)
        .run_once(now_us)
        .map_err(|error| error.to_string())?;
    if let Some(vectors) = vectors {
        // A candidate can need more jobs before its full scan is valid.
        // Keep the role alive so the next turn can finish the rebuild.
        if let Err(error) = vectors.reconcile_managed_snapshot() {
            tracing::warn!(%error, "candidate snapshot publish deferred");
        }
    }
    Ok(())
}

#[cfg(feature = "indexer")]
fn run_indexer_workspace_turn<P: mcpmem_indexer::EmbeddingProvider>(
    registry: &crate::workspace::WorkspaceRegistry,
    handles: &crate::workspace::WorkspaceHandles,
    state: &mut WorkspaceIndexerState,
    provider: P,
    now_us: i64,
    timeout: std::time::Duration,
    profile: Option<&crate::config_file::ProfileSpec>,
) -> Result<(), String> {
    let selected =
        next_registered_path(registry, &mut state.cursor).map_err(|error| error.to_string())?;
    let Some((id, path)) = selected else {
        if !state.warned_empty {
            tracing::warn!("indexer has no registered workspace to poll");
            state.warned_empty = true;
        }
        return Ok(());
    };
    let entry = handles
        .get_by_registered_path(&id, &path)
        .map_err(|error| error.to_string())?;
    if let Some(profile) = profile
        && !state.adopted.contains(&id)
    {
        if let Some(vectors) = &entry.vs {
            vectors
                .adopt_profile(&profile.to_profile())
                .map_err(|error| error.to_string())?;
        }
        state.adopted.insert(id);
    }
    run_indexer_graph(&path, entry.vs, provider, now_us, timeout)
}

#[cfg(feature = "indexer")]
impl IndexerService {
    /// The worker and its provider use the same request timeout.
    pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    /// Build provider clients outside an async context.
    pub async fn provider_registry(
        settings: mcpmem_indexer::ProviderSettings,
    ) -> Result<Arc<mcpmem_indexer::ProviderRegistry>, crate::errors::MCSError> {
        tokio::task::spawn_blocking(move || {
            mcpmem_indexer::ProviderRegistry::from_settings(&settings, Self::REQUEST_TIMEOUT)
                .map(Arc::new)
                .map_err(|error| crate::errors::MCSError::MemoryError(error.to_string()))
        })
        .await
        .map_err(|error| crate::errors::MCSError::MemoryError(error.to_string()))?
    }

    /// Preserve the one-file constructor for callers with one graph.
    pub fn with_provider(
        database: impl Into<std::path::PathBuf>,
        vectors: Option<Arc<crate::vector_store::VectorStore>>,
        provider: Arc<mcpmem_indexer::ProviderRegistry>,
    ) -> Self {
        Self {
            target: Arc::new(IndexerTarget::Single {
                database: database.into(),
                vectors,
            }),
            provider,
        }
    }

    /// Read trusted graph paths anew on every bounded worker turn.
    pub fn with_workspaces(
        registry: Arc<crate::workspace::WorkspaceRegistry>,
        handles: Arc<crate::workspace::WorkspaceHandles>,
        provider: Arc<mcpmem_indexer::ProviderRegistry>,
        profile: Option<crate::config_file::ProfileSpec>,
    ) -> Self {
        Self {
            target: Arc::new(IndexerTarget::Workspaces {
                registry,
                handles,
                profile,
                state: Mutex::new(WorkspaceIndexerState::default()),
            }),
            provider,
        }
    }
}

#[cfg(feature = "indexer")]
fn indexer_role_loop<P: mcpmem_indexer::EmbeddingProvider + Clone + Send + Sync + 'static>(
    target: Arc<IndexerTarget>,
    provider: P,
) -> mcpmem_runtime::RoleFuture {
    Box::pin(async move {
        loop {
            let now_us = mcpmem_core::events::now_us();
            let target = Arc::clone(&target);
            let provider = provider.clone();
            let result = tokio::task::spawn_blocking(move || match target.as_ref() {
                IndexerTarget::Single { database, vectors } => run_indexer_graph(
                    database,
                    vectors.clone(),
                    provider,
                    now_us,
                    IndexerService::REQUEST_TIMEOUT,
                ),
                IndexerTarget::Workspaces {
                    registry,
                    handles,
                    profile,
                    state,
                } => run_indexer_workspace_turn(
                    registry,
                    handles,
                    &mut state.lock(),
                    provider,
                    now_us,
                    IndexerService::REQUEST_TIMEOUT,
                    profile.as_ref(),
                ),
            })
            .await
            .map_err(|error| RuntimeError::RoleFailed {
                role: RuntimeRole::Indexer,
                message: error.to_string(),
            })?;
            if let Err(error) = result {
                tracing::error!(
                    %error,
                    "indexer poll failed for one workspace; the next workspace stays scheduled"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    })
}

#[cfg(feature = "indexer")]
impl RoleService for IndexerService {
    fn run(&self) -> mcpmem_runtime::RoleFuture {
        indexer_role_loop(Arc::clone(&self.target), Arc::clone(&self.provider))
    }
}

/// Resolve the OCR provider for the extractor role from the `[ocr]` section
/// and the effective primary provider settings.
///
/// The resolution never fails startup: an invalid configuration produces a
/// provider that fails PDF jobs at stage `config`, while text extraction
/// keeps working with the same settings. The primary key follows the existing
/// `config_file::indexer_settings` precedence (environment, then file).
#[cfg(feature = "extractor")]
pub fn ocr_provider(
    ocr: Option<&crate::config_file::OcrSection>,
    file: Option<&crate::config_file::FileConfig>,
) -> Option<Arc<dyn mcpmem_extractor::OcrProvider>> {
    let primary = match crate::config_file::profile_spec(file) {
        Ok(spec) => mcpmem_extractor::PrimaryProvider {
            kind: spec.map(|spec| spec.provider_kind),
            api_key: crate::config_file::indexer_settings(file)
                .ok()
                .and_then(|settings| settings.openai_api_key),
        },
        Err(_) => mcpmem_extractor::PrimaryProvider::default(),
    };
    let config = ocr.map(|section| mcpmem_extractor::OcrConfig {
        provider: section.provider.clone(),
        model: section.model.clone(),
        vision_url: section.vision_url.clone(),
        api_key_file: section.api_key_file.clone(),
    });
    mcpmem_extractor::resolve_ocr(config.as_ref(), &primary)
}

#[cfg(feature = "extractor")]
#[derive(Default)]
struct WorkspaceExtractorState {
    cursor: usize,
    warned_empty: bool,
}

#[cfg(feature = "extractor")]
enum ExtractorTarget {
    /// The one-graph constructor, for callers with a single database.
    Single { database: std::path::PathBuf },
    /// Read trusted graph paths anew on every bounded worker turn.
    Workspaces {
        registry: Arc<crate::workspace::WorkspaceRegistry>,
        state: Mutex<WorkspaceExtractorState>,
    },
}

#[cfg(feature = "extractor")]
pub struct ExtractorService {
    target: Arc<ExtractorTarget>,
    ocr: Option<Arc<dyn mcpmem_extractor::OcrProvider>>,
}

#[cfg(feature = "extractor")]
fn run_extractor_graph(
    path: &std::path::Path,
    ocr: Option<Arc<dyn mcpmem_extractor::OcrProvider>>,
    now_us: i64,
) -> Result<mcpmem_extractor::ExtractionReport, String> {
    mcpmem_extractor::ExtractionWorker::new(path, ocr)
        .run_once(now_us)
        .map_err(|error| error.to_string())
}

#[cfg(feature = "extractor")]
fn run_extractor_workspace_turn(
    registry: &crate::workspace::WorkspaceRegistry,
    state: &mut WorkspaceExtractorState,
    ocr: Option<Arc<dyn mcpmem_extractor::OcrProvider>>,
    now_us: i64,
) -> Result<mcpmem_extractor::ExtractionReport, String> {
    let selected =
        next_registered_path(registry, &mut state.cursor).map_err(|error| error.to_string())?;
    let Some((id, path)) = selected else {
        if !state.warned_empty {
            tracing::warn!("extractor has no registered workspace to poll");
            state.warned_empty = true;
        }
        return Ok(mcpmem_extractor::ExtractionReport::default());
    };
    match run_extractor_graph(&path, ocr, now_us) {
        Ok(report) => {
            tracing::debug!(
                workspace_id = %id,
                report = ?report,
                "extractor turn finished"
            );
            Ok(report)
        }
        Err(error) => {
            tracing::error!(
                workspace_id = %id,
                %error,
                "extractor turn failed for one workspace; the next workspace stays scheduled"
            );
            Err(error)
        }
    }
}

#[cfg(feature = "extractor")]
impl ExtractorService {
    /// Preserve the one-file constructor for callers with one graph.
    pub fn new(
        database: impl Into<std::path::PathBuf>,
        ocr: Option<Arc<dyn mcpmem_extractor::OcrProvider>>,
    ) -> Self {
        Self {
            target: Arc::new(ExtractorTarget::Single {
                database: database.into(),
            }),
            ocr,
        }
    }

    /// Read trusted graph paths anew on every bounded worker turn.
    pub fn with_workspaces(
        registry: Arc<crate::workspace::WorkspaceRegistry>,
        ocr: Option<Arc<dyn mcpmem_extractor::OcrProvider>>,
    ) -> Self {
        Self {
            target: Arc::new(ExtractorTarget::Workspaces {
                registry,
                state: Mutex::new(WorkspaceExtractorState::default()),
            }),
            ocr,
        }
    }
}

#[cfg(feature = "extractor")]
fn extractor_role_loop(
    target: Arc<ExtractorTarget>,
    ocr: Option<Arc<dyn mcpmem_extractor::OcrProvider>>,
) -> mcpmem_runtime::RoleFuture {
    Box::pin(async move {
        loop {
            let now_us = mcpmem_core::events::now_us();
            let target = Arc::clone(&target);
            let ocr = ocr.clone();
            let result = tokio::task::spawn_blocking(move || match target.as_ref() {
                ExtractorTarget::Single { database } => run_extractor_graph(database, ocr, now_us),
                ExtractorTarget::Workspaces { registry, state } => {
                    run_extractor_workspace_turn(registry, &mut state.lock(), ocr, now_us)
                }
            })
            .await
            .map_err(|error| RuntimeError::RoleFailed {
                role: RuntimeRole::Extractor,
                message: error.to_string(),
            })?;
            if let Err(error) = result {
                tracing::error!(
                    %error,
                    "extractor poll failed for one workspace; the next workspace stays scheduled"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    })
}

#[cfg(feature = "extractor")]
impl RoleService for ExtractorService {
    fn run(&self) -> mcpmem_runtime::RoleFuture {
        extractor_role_loop(Arc::clone(&self.target), self.ocr.clone())
    }
}

#[cfg(all(test, feature = "webhooks"))]
mod webhook_tests {
    use super::*;
    #[tokio::test]
    async fn disabled_service_fails_observably() {
        let result = WebhookService::disabled().run().await;
        assert!(matches!(
            result,
            Err(RuntimeError::RoleFailed {
                role: RuntimeRole::Webhooks,
                ..
            })
        ));
    }
}

#[cfg(all(test, feature = "indexer"))]
mod workspace_indexer_tests {
    use super::*;
    use crate::config::{Durability, SqliteTuning};
    use crate::vector_store::VectorStore;
    use crate::workspace::{
        HandleSpec, Visibility, WorkspaceAccess, WorkspaceHandles, WorkspaceRegistry,
    };
    use mcpmem_core::graph::GraphHandle;
    use mcpmem_core::jobs::{
        ChunkKind, DistanceMetric, IndexProfile, IndexProfileRegistry, Normalization,
    };
    use mcpmem_core::types::EntityInput;
    use mcpmem_indexer::{EmbeddingProvider, ProviderError};
    use std::{num::NonZeroUsize, path::Path, time::Duration};

    struct FixedProvider;

    impl EmbeddingProvider for FixedProvider {
        fn embed_texts(
            &self,
            _: &IndexProfile,
            texts: &[String],
        ) -> Result<Vec<Vec<f32>>, ProviderError> {
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }
    }

    fn graph(path: &Path) -> GraphHandle {
        GraphHandle::new(
            path,
            Durability::Sync,
            SqliteTuning::default(),
            NonZeroUsize::new(8).unwrap(),
            1,
        )
        .unwrap()
    }

    fn profile(path: &Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        IndexProfileRegistry::new(&conn)
            .begin_rebuild(&IndexProfile {
                id: uuid::Uuid::new_v4(),
                store_key: "default".into(),
                provider_kind: "test".into(),
                model: "fixed".into(),
                dimensions: 2,
                representation_version: "v1".into(),
                normalization: Normalization::None,
                distance_metric: DistanceMetric::Cosine,
                vector_encoding_version: "f32le-v1".into(),
            })
            .unwrap();
    }

    fn entity(name: &str) -> EntityInput {
        EntityInput {
            name: name.into(),
            entity_type: "note".into(),
            observations: vec![],
            attributes: None,
        }
    }

    fn snapshot_owner_ids(
        handles: &WorkspaceHandles,
        record: &crate::workspace::WorkspaceRecord,
    ) -> Vec<i64> {
        // The worker path: the turn's own handle already reconciled its
        // in-memory snapshot, and a mid-rebuild graph must not be hydrated.
        handles
            .get_by_registered_path(&record.workspace_id, &record.graph_path)
            .unwrap()
            .vs
            .unwrap()
            .search_chunks(&[1.0, 0.0], 10, Some("entity"), None)
            .unwrap()
            .into_iter()
            .filter(|hit| hit.chunk_kind == ChunkKind::Identity)
            .map(|hit| hit.owner_id)
            .collect()
    }

    #[test]
    fn two_registered_indexer_turns_publish_only_the_selected_graphs_owner() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("legacy.sqlite");
        let registry = WorkspaceRegistry::open(&legacy_path, Some("machine:local")).unwrap();
        let first = registry
            .resolve("machine:local", None, WorkspaceAccess::Owner)
            .unwrap();
        let first_graph = Arc::new(graph(&first.graph_path));
        let handles = WorkspaceHandles::new(
            &first.workspace_id,
            Arc::clone(&first_graph),
            Some(Arc::new(VectorStore::new(&first.graph_path, 2).unwrap())),
            HandleSpec {
                durability: Durability::Sync,
                tuning: SqliteTuning::default(),
                lru_cache_size: NonZeroUsize::new(8).unwrap(),
                read_pool_size: 1,
                vector_dims: Some(2),
            },
        );
        let second_id = registry
            .create("machine:local", "second", Visibility::Private, |path| {
                drop(graph(path));
                Ok(())
            })
            .unwrap()
            .workspace_id;
        let second = registry
            .resolve("machine:local", Some(&second_id), WorkspaceAccess::Owner)
            .unwrap();
        assert_eq!(registry.all_paths().unwrap().len(), 2);

        // Different live owner IDs distinguish a wrong-file snapshot even if
        // both graphs start their SQLite sequences at one.
        let second_graph = graph(&second.graph_path);
        second_graph
            .create_entities(&[entity("retired-1"), entity("retired-2")])
            .unwrap();
        second_graph
            .delete_entities(&["retired-1".into(), "retired-2".into()])
            .unwrap();
        first_graph
            .create_entities(&[entity("first-only")])
            .unwrap();
        second_graph
            .create_entities(&[entity("second-only")])
            .unwrap();
        profile(&first.graph_path);
        profile(&second.graph_path);

        let mut state = WorkspaceIndexerState::default();
        let provider = Arc::new(FixedProvider);
        run_indexer_workspace_turn(
            &registry,
            &handles,
            &mut state,
            Arc::clone(&provider),
            mcpmem_core::events::now_us(),
            Duration::from_secs(5),
            None,
        )
        .unwrap();
        let one_turn = [
            snapshot_owner_ids(&handles, &first),
            snapshot_owner_ids(&handles, &second),
        ];
        assert_eq!(
            one_turn.iter().map(Vec::len).sum::<usize>(),
            1,
            "one poll must publish at most one registered graph"
        );

        run_indexer_workspace_turn(
            &registry,
            &handles,
            &mut state,
            provider,
            mcpmem_core::events::now_us(),
            Duration::from_secs(5),
            None,
        )
        .unwrap();
        assert_eq!(snapshot_owner_ids(&handles, &first), [1]);
        assert_eq!(snapshot_owner_ids(&handles, &second), [3]);
    }

    #[test]
    fn a_new_workspace_adopts_its_profile_and_processes_only_its_own_job() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("legacy.sqlite");
        let registry = WorkspaceRegistry::open(&legacy_path, Some("machine:local")).unwrap();
        let legacy = registry
            .resolve("machine:local", None, WorkspaceAccess::Owner)
            .unwrap();
        let legacy_graph = Arc::new(graph(&legacy_path));
        legacy_graph
            .create_entities(&[entity("legacy-only")])
            .unwrap();
        let handles = WorkspaceHandles::new(
            &legacy.workspace_id,
            Arc::clone(&legacy_graph),
            Some(Arc::new(VectorStore::new(&legacy_path, 2).unwrap())),
            HandleSpec {
                durability: Durability::Sync,
                tuning: SqliteTuning::default(),
                lru_cache_size: NonZeroUsize::new(8).unwrap(),
                read_pool_size: 1,
                vector_dims: Some(2),
            },
        );
        let new_id = registry
            .create("machine:local", "new", Visibility::Private, |path| {
                drop(graph(path));
                Ok(())
            })
            .unwrap()
            .workspace_id;
        let new_graph = registry
            .resolve("machine:local", Some(&new_id), WorkspaceAccess::Owner)
            .unwrap();
        graph(&new_graph.graph_path)
            .create_entities(&[entity("new-only")])
            .unwrap();
        let new_conn = rusqlite::Connection::open(&new_graph.graph_path).unwrap();
        assert!(matches!(
            IndexProfileRegistry::new(&new_conn)
                .state("default")
                .unwrap(),
            mcpmem_core::jobs::StoreState::LegacyCompat
        ));
        assert_eq!(
            new_conn
                .query_row(
                    "SELECT count(*) FROM chunk_index_job WHERE state='held'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1,
            "the write made a held job before the graph had a profile"
        );

        let profile = crate::config_file::ProfileSpec {
            provider_kind: "test".into(),
            model: "fixed".into(),
            dimensions: 2,
            normalization: Normalization::None,
            distance_metric: DistanceMetric::Cosine,
        };
        let paths = registry.all_paths().unwrap();
        assert_eq!(paths.len(), 2);
        // The scheduler advances in rowid (insertion) order: the new graph
        // was created last, so it sits at the final offset.
        let mut state = WorkspaceIndexerState {
            cursor: paths.len() - 1,
            ..WorkspaceIndexerState::default()
        };
        run_indexer_workspace_turn(
            &registry,
            &handles,
            &mut state,
            Arc::new(FixedProvider),
            mcpmem_core::events::now_us(),
            Duration::from_secs(5),
            Some(&profile),
        )
        .unwrap();
        assert!(matches!(
            IndexProfileRegistry::new(&new_conn)
                .state("default")
                .unwrap(),
            mcpmem_core::jobs::StoreState::Active(_)
        ));
        assert_eq!(
            new_conn
                .query_row(
                    "SELECT count(*) FROM chunk_index_job WHERE owner_kind='entity' AND state='done'
                     AND profile_id=(SELECT serving_profile FROM index_profile_registry WHERE store_key='default')",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1,
            "the adopted profile must process the new graph's entity job"
        );
        assert_eq!(snapshot_owner_ids(&handles, &new_graph), [1]);
        assert!(snapshot_owner_ids(&handles, &legacy).is_empty());
        let legacy_conn = rusqlite::Connection::open(&legacy_path).unwrap();
        assert_eq!(
            legacy_conn
                .query_row(
                    "SELECT count(*) FROM chunk_index_job WHERE state='held'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1,
            "the legacy write also made a held job"
        );
        assert_eq!(
            legacy_conn
                .query_row(
                    "SELECT count(*) FROM chunk_index_job WHERE state='done'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0,
            "a turn for the new graph must not process work in the legacy graph"
        );
    }

    #[test]
    fn an_evicted_workspace_reopens_with_its_published_vector_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("legacy.sqlite");
        let registry = WorkspaceRegistry::open(&legacy_path, Some("machine:local")).unwrap();
        let legacy = registry
            .resolve("machine:local", None, WorkspaceAccess::Owner)
            .unwrap();
        let handles = WorkspaceHandles::new(
            &legacy.workspace_id,
            Arc::new(graph(&legacy_path)),
            Some(Arc::new(VectorStore::new(&legacy_path, 2).unwrap())),
            HandleSpec {
                durability: Durability::Sync,
                tuning: SqliteTuning::default(),
                lru_cache_size: NonZeroUsize::new(8).unwrap(),
                read_pool_size: 1,
                vector_dims: Some(2),
            },
        );
        let target_id = registry
            .create("machine:local", "target", Visibility::Private, |path| {
                drop(graph(path));
                Ok(())
            })
            .unwrap()
            .workspace_id;
        let target = registry
            .resolve("machine:local", Some(&target_id), WorkspaceAccess::Owner)
            .unwrap();
        graph(&target.graph_path)
            .create_entities(&[entity("published-only")])
            .unwrap();
        profile(&target.graph_path);
        mcpmem_indexer::IndexerWorker::new(
            &target.graph_path,
            FixedProvider,
            Duration::from_secs(5),
        )
        .run_once(mcpmem_core::events::now_us())
        .unwrap();
        // Publish through a store outside the cache, the way the indexer
        // role does, so the profile becomes active and its generation is
        // marked published before the request path opens the file.
        let seed_store = VectorStore::new(&target.graph_path, 2).unwrap();
        seed_store.reconcile_managed_snapshot().unwrap();
        let original = handles.get(&target).unwrap().vs.unwrap();
        assert_eq!(snapshot_owner_ids(&handles, &target), [1]);

        for index in 0..WorkspaceHandles::BOUND {
            let id = registry
                .create(
                    "machine:local",
                    &format!("filler-{index}"),
                    Visibility::Private,
                    |path| {
                        drop(graph(path));
                        Ok(())
                    },
                )
                .unwrap()
                .workspace_id;
            let record = registry
                .resolve("machine:local", Some(&id), WorkspaceAccess::Read)
                .unwrap();
            drop(handles.get(&record).unwrap());
        }
        let probe = rusqlite::Connection::open(&target.graph_path).unwrap();
        // Open the store first: its WAL pragmas write, so the stability check
        // brackets only the hydration load itself.
        let standalone = VectorStore::new(&target.graph_path, 2).unwrap();
        let before_write_version: i64 = probe
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .unwrap();
        standalone.load_managed_snapshot().unwrap();
        assert_eq!(
            standalone
                .search_chunks(&[1.0, 0.0], 10, Some("entity"), None)
                .unwrap()
                .into_iter()
                .map(|hit| hit.owner_id)
                .collect::<Vec<_>>(),
            [1],
            "the read-only load must serve the published snapshot"
        );
        let after_write_version: i64 = probe
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .unwrap();
        assert_eq!(
            after_write_version, before_write_version,
            "snapshot hydration must not publish or modify a durable vector generation"
        );

        // The cache path delegates to the same read-only load: a reopened
        // handle serves the published snapshot on its very first search.
        let reopened = handles.get(&target).unwrap().vs.unwrap();
        assert!(
            !Arc::ptr_eq(&original, &reopened),
            "the target must have left the bounded handle cache"
        );
        assert_eq!(
            reopened
                .search_chunks(&[1.0, 0.0], 10, Some("entity"), None)
                .unwrap()
                .into_iter()
                .map(|hit| hit.owner_id)
                .collect::<Vec<_>>(),
            [1],
            "the first search after reopen must read the published snapshot"
        );
    }

    #[test]
    fn a_worker_cursor_wraps_and_sees_a_graph_registered_mid_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sqlite");
        let registry = WorkspaceRegistry::open(&path, Some("machine:local")).unwrap();
        registry
            .create("machine:local", "second", Visibility::Private, |path| {
                drop(graph(path));
                Ok(())
            })
            .unwrap();
        let mut cursor = 0;
        let first = next_registered_path(&registry, &mut cursor)
            .unwrap()
            .unwrap()
            .0;
        let second = next_registered_path(&registry, &mut cursor)
            .unwrap()
            .unwrap()
            .0;
        assert_ne!(first, second);
        assert_eq!(
            next_registered_path(&registry, &mut cursor)
                .unwrap()
                .unwrap()
                .0,
            first,
            "the two-graph cursor must wrap"
        );

        let new_id = registry
            .create("machine:local", "third", Visibility::Private, |path| {
                drop(graph(path));
                Ok(())
            })
            .unwrap()
            .workspace_id;
        let expected: std::collections::HashSet<_> = registry
            .all_paths()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(expected.len(), 3);
        let visited: std::collections::HashSet<_> = (0..expected.len())
            .map(|_| {
                next_registered_path(&registry, &mut cursor)
                    .unwrap()
                    .unwrap()
                    .0
            })
            .collect();
        assert_eq!(
            visited, expected,
            "each graph gets one turn after the new registration"
        );
        assert!(visited.contains(&new_id));
    }

    #[tokio::test]
    async fn indexer_role_keeps_scheduling_after_a_graph_turn_fails() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("legacy.sqlite");
        let registry =
            Arc::new(WorkspaceRegistry::open(&legacy_path, Some("machine:local")).unwrap());
        let legacy = registry
            .resolve("machine:local", None, WorkspaceAccess::Owner)
            .unwrap();
        let legacy_graph = Arc::new(graph(&legacy_path));
        let handles = Arc::new(WorkspaceHandles::new(
            &legacy.workspace_id,
            Arc::clone(&legacy_graph),
            Some(Arc::new(VectorStore::new(&legacy_path, 2).unwrap())),
            HandleSpec {
                durability: Durability::Sync,
                tuning: SqliteTuning::default(),
                lru_cache_size: NonZeroUsize::new(8).unwrap(),
                read_pool_size: 1,
                vector_dims: Some(2),
            },
        ));
        legacy_graph
            .create_entities(&[entity("survivor-a"), entity("survivor-b")])
            .unwrap();
        profile(&legacy_path);

        let broken_id = registry
            .create("machine:local", "broken", Visibility::Private, |path| {
                drop(graph(path));
                Ok(())
            })
            .unwrap()
            .workspace_id;
        let broken = registry
            .resolve("machine:local", Some(&broken_id), WorkspaceAccess::Owner)
            .unwrap();
        std::fs::remove_file(&broken.graph_path).unwrap();

        let target = Arc::new(IndexerTarget::Workspaces {
            registry: Arc::clone(&registry),
            handles: Arc::clone(&handles),
            profile: None,
            state: Mutex::new(WorkspaceIndexerState::default()),
        });
        let mut role = tokio::spawn(indexer_role_loop(target, Arc::new(FixedProvider)));
        tokio::select! {
            outcome = &mut role => panic!("one broken graph ended the indexer role: {outcome:?}"),
            embedded = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let conn = rusqlite::Connection::open(&legacy_path).unwrap();
                    let done: i64 = conn
                        .query_row(
                            "SELECT count(*) FROM chunk_vector
                             WHERE profile_id=(SELECT coalesce(serving_profile, candidate_profile)
                                               FROM index_profile_registry WHERE store_key='default')
                               AND owner_kind='entity'",
                            [],
                            |row| row.get(0),
                        )
                        .unwrap();
                    if done >= 2 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }) => embedded.expect("the healthy graph must finish its jobs across broken turns"),
        }
        role.abort();
    }
}

#[cfg(all(test, feature = "extractor"))]
mod workspace_extractor_tests {
    use super::*;
    use crate::config::{Durability, SqliteTuning};
    use crate::workspace::{Visibility, WorkspaceAccess, WorkspaceRegistry};
    use mcpmem_core::attachments::{AttachmentLimits, AttachmentRepository};
    use mcpmem_core::graph::GraphHandle;
    use mcpmem_core::types::EntityInput;
    use rusqlite::Connection;
    use sha2::{Digest, Sha256};
    use std::io::Cursor;
    use std::num::NonZeroUsize;
    use std::path::Path;
    use std::time::Duration;

    fn limits() -> AttachmentLimits {
        AttachmentLimits {
            max_bytes: 52_428_800,
            workspace_byte_budget: 268_435_456,
            allow_mime: vec!["text/*".into(), "application/pdf".into()],
        }
    }

    fn digest(data: &[u8]) -> [u8; 32] {
        Sha256::digest(data).into()
    }

    fn graph(path: &Path) -> GraphHandle {
        GraphHandle::new(
            path,
            Durability::Sync,
            SqliteTuning::default(),
            NonZeroUsize::new(8).unwrap(),
            1,
        )
        .unwrap()
    }

    fn entity(name: &str) -> EntityInput {
        EntityInput {
            name: name.into(),
            entity_type: "note".into(),
            observations: vec![],
            attributes: None,
        }
    }

    /// One live entity with one queued text attachment, as an upload would
    /// leave it.
    fn uploaded(path: &Path) -> i64 {
        let handle = graph(path);
        handle.create_entities(&[entity("doc")]).unwrap();
        drop(handle);
        let conn = Connection::open(path).unwrap();
        AttachmentRepository::new(&conn)
            .store_reader(
                1,
                "memo.txt",
                "text/plain",
                &mut Cursor::new(b"text\n".as_slice()),
                5,
                &digest(b"text\n"),
                &limits(),
                mcpmem_core::events::now_us(),
            )
            .unwrap()
    }

    fn entity_id(path: &Path) -> i64 {
        let conn = Connection::open(path).unwrap();
        conn.query_row("SELECT id FROM entity WHERE name='doc'", [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    fn status(path: &Path, attachment: i64) -> String {
        let conn = Connection::open(path).unwrap();
        conn.query_row(
            "SELECT status FROM attachment WHERE id=?1",
            [attachment],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn two_registered_extractor_turns_process_one_graph_each() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("legacy.sqlite");
        let registry = WorkspaceRegistry::open(&legacy_path, Some("machine:local")).unwrap();
        let legacy = registry
            .resolve("machine:local", None, WorkspaceAccess::Owner)
            .unwrap();
        let legacy_attachment = uploaded(&legacy.graph_path);
        let legacy_entity = entity_id(&legacy.graph_path);
        assert_eq!(legacy_entity, 1);
        let second_id = registry
            .create("machine:local", "second", Visibility::Private, |path| {
                drop(graph(path));
                Ok(())
            })
            .unwrap()
            .workspace_id;
        let second = registry
            .resolve("machine:local", Some(&second_id), WorkspaceAccess::Owner)
            .unwrap();
        let second_attachment = uploaded(&second.graph_path);
        assert_eq!(status(&legacy.graph_path, legacy_attachment), "uploaded");
        assert_eq!(status(&second.graph_path, second_attachment), "uploaded");

        let mut state = WorkspaceExtractorState::default();
        let first_turn = run_extractor_workspace_turn(
            &registry,
            &mut state,
            None,
            mcpmem_core::events::now_us(),
        )
        .unwrap();
        assert_eq!(first_turn.committed, 1);
        assert_eq!(status(&legacy.graph_path, legacy_attachment), "ready");
        assert_eq!(
            status(&second.graph_path, second_attachment),
            "uploaded",
            "one turn must process at most one workspace"
        );

        let second_turn = run_extractor_workspace_turn(
            &registry,
            &mut state,
            None,
            mcpmem_core::events::now_us(),
        )
        .unwrap();
        assert_eq!(second_turn.committed, 1);
        assert_eq!(status(&second.graph_path, second_attachment), "ready");
        assert_eq!(
            status(&legacy.graph_path, legacy_attachment),
            "ready",
            "a finished attachment is not reprocessed"
        );
    }

    #[tokio::test]
    async fn extractor_role_keeps_scheduling_after_a_graph_turn_fails() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("legacy.sqlite");
        let registry =
            Arc::new(WorkspaceRegistry::open(&legacy_path, Some("machine:local")).unwrap());
        let legacy = registry
            .resolve("machine:local", None, WorkspaceAccess::Owner)
            .unwrap();
        let attachment = uploaded(&legacy.graph_path);

        let broken_id = registry
            .create("machine:local", "broken", Visibility::Private, |path| {
                drop(graph(path));
                Ok(())
            })
            .unwrap()
            .workspace_id;
        let broken = registry
            .resolve("machine:local", Some(&broken_id), WorkspaceAccess::Owner)
            .unwrap();
        std::fs::remove_file(&broken.graph_path).unwrap();

        let target = Arc::new(ExtractorTarget::Workspaces {
            registry: Arc::clone(&registry),
            state: Mutex::new(WorkspaceExtractorState::default()),
        });
        let mut role = tokio::spawn(extractor_role_loop(target, None));
        tokio::select! {
            outcome = &mut role => panic!("one broken graph ended the extractor role: {outcome:?}"),
            ready = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if status(&legacy.graph_path, attachment) == "ready" {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }) => ready.expect("the healthy graph must finish extraction across broken turns"),
        }
        role.abort();
    }
}
