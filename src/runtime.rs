//! Adapts the existing MCP transport to the reusable runtime supervisor.

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
            // Each MCP process owns an independent immutable reader snapshot.
            // Refresh it on a bounded background task so a separately running
            // indexer role's durable publication becomes visible without ever
            // putting SQLite/deserialization on a request path.
            let refresher = server.vector_store();
            let refresh_task = tokio::spawn(async move {
                while let Some(vectors) = refresher.as_ref() {
                    let vectors = vectors.clone();
                    let _ =
                        tokio::task::spawn_blocking(move || vectors.reconcile_managed_snapshot())
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

#[cfg(feature = "webhooks")]
pub struct WebhookService {
    worker: Option<Arc<dyn mcpmem_webhook::WorkerPoll>>,
}

#[cfg(feature = "webhooks")]
impl WebhookService {
    pub const fn disabled() -> Self {
        Self { worker: None }
    }
    pub fn new(worker: Arc<dyn mcpmem_webhook::WorkerPoll>) -> Self {
        Self {
            worker: Some(worker),
        }
    }
    /// Builds the delivery worker from the resolved webhook configuration.
    ///
    /// An empty configuration produces a worker that refuses every endpoint.
    /// The allowlist is empty and no signing key exists, so every delivery
    /// fails the policy check.
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
        );
        Ok(Self {
            worker: Some(Arc::new(worker)),
        })
    }
    /// Runs one poll against the database. This is a test seam.
    ///
    /// The concrete worker's `run_once` is not reachable through the
    /// `WorkerPoll` trait object. The worker implements `poll` as `run_once`,
    /// so this method exposes the same run-once path.
    pub fn run_once(
        &self,
        now_us: i64,
    ) -> Result<mcpmem_webhook::DeliveryReport, crate::errors::MCSError> {
        let worker = self.worker.clone().ok_or_else(|| {
            crate::errors::MCSError::MemoryError("webhook worker is not configured".into())
        })?;
        worker
            .poll(now_us)
            .map_err(|error| crate::errors::MCSError::MemoryError(error.to_string()))
    }
}

#[cfg(feature = "webhooks")]
impl RoleService for WebhookService {
    fn run(&self) -> mcpmem_runtime::RoleFuture {
        let worker = self.worker.clone();
        Box::pin(async move {
            let worker = worker.ok_or_else(|| RuntimeError::RoleFailed {
                role: RuntimeRole::Webhooks,
                message: "webhook worker is not configured".into(),
            })?;
            loop {
                let worker = worker.clone();
                tokio::task::spawn_blocking(move || worker.poll(mcpmem_core::events::now_us()))
                    .await
                    .map_err(|error| RuntimeError::RoleFailed {
                        role: RuntimeRole::Webhooks,
                        message: error.to_string(),
                    })?
                    .map_err(|error| RuntimeError::RoleFailed {
                        role: RuntimeRole::Webhooks,
                        message: error.to_string(),
                    })?;
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        })
    }
}

#[cfg(feature = "indexer")]
pub struct IndexerService {
    database: std::path::PathBuf,
    vectors: Option<Arc<crate::vector_store::VectorStore>>,
    provider: Arc<mcpmem_indexer::ProviderRegistry>,
}

#[cfg(feature = "indexer")]
impl IndexerService {
    /// The request timeout of one embedding call. The registry and the worker
    /// are built by separate calls, so one constant serves both and the two
    /// cannot disagree.
    pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    /// Builds the provider registry from settings the caller already resolved:
    /// the environment layered over the configuration file.
    ///
    /// This is an associated function and not a step inside the constructor
    /// because the process also publishes the registry through
    /// [`crate::indexer_provider::init`]. The worker embeds an entity on write
    /// and a query tool embeds the query text. Both must call one provider, so
    /// the process builds one registry and hands it to both.
    ///
    /// It is `async` so that no caller can reach the hazard it hides. Each
    /// provider holds a `reqwest::blocking` client, and building one makes a
    /// temporary Tokio runtime and drops it again. A runtime dropped inside an
    /// async context panics, which killed startup for every operator who named
    /// a provider. The blocking pool is not an async context, so the build runs
    /// there.
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

    /// Takes the registry the caller already built. Every poll reuses it.
    pub fn with_provider(
        database: impl Into<std::path::PathBuf>,
        vectors: Option<Arc<crate::vector_store::VectorStore>>,
        provider: Arc<mcpmem_indexer::ProviderRegistry>,
    ) -> Self {
        Self {
            database: database.into(),
            vectors,
            provider,
        }
    }
}

#[cfg(feature = "indexer")]
impl RoleService for IndexerService {
    fn run(&self) -> mcpmem_runtime::RoleFuture {
        let database = self.database.clone();
        let timeout = Self::REQUEST_TIMEOUT;
        let vectors = self.vectors.clone();
        let provider = self.provider.clone();
        Box::pin(async move {
            loop {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros()
                    .min(i64::MAX as u128) as i64;
                let database = database.clone();
                let vectors = vectors.clone();
                let provider = provider.clone();
                tokio::task::spawn_blocking(move || {
                    let worker = mcpmem_indexer::IndexerWorker::new(database, provider, timeout);
                    worker.run_once(now).map_err(|error| error.to_string())?;
                    if let Some(vectors) = vectors {
                        // A rebuild completes over many polls. Until the last
                        // job commits, the `verify_full_scan` inside this call
                        // reports that the candidate is incomplete, and only a
                        // later poll can complete it. Ending the role here
                        // stopped every rebuild bigger than one poll, so the
                        // candidate never reached the serving state. A
                        // concurrent entity write can also enqueue a job
                        // between these two calls, so no error here is
                        // permanent, and the next poll retries it.
                        if let Err(error) = vectors.reconcile_managed_snapshot() {
                            tracing::debug!(%error, "managed snapshot is not published yet");
                        }
                    }
                    Ok::<_, String>(())
                })
                .await
                .map_err(|error| RuntimeError::RoleFailed {
                    role: RuntimeRole::Indexer,
                    message: error.to_string(),
                })?
                .map_err(|error| RuntimeError::RoleFailed {
                    role: RuntimeRole::Indexer,
                    message: error,
                })?;
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        })
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
