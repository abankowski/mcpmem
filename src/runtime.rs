//! Adapts the existing MCP transport to the reusable runtime supervisor.

use std::sync::Arc;

pub use memory_runtime::{
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
    fn run(&self) -> memory_runtime::RoleFuture {
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
    worker: Option<Arc<dyn webhook_worker::WorkerPoll>>,
}

#[cfg(feature = "webhooks")]
impl WebhookService {
    pub const fn disabled() -> Self {
        Self { worker: None }
    }
    pub fn new(worker: Arc<dyn webhook_worker::WorkerPoll>) -> Self {
        Self {
            worker: Some(worker),
        }
    }
}

#[cfg(feature = "webhooks")]
impl RoleService for WebhookService {
    fn run(&self) -> memory_runtime::RoleFuture {
        let worker = self.worker.clone();
        Box::pin(async move {
            let worker = worker.ok_or_else(|| RuntimeError::RoleFailed {
                role: RuntimeRole::Webhooks,
                message: "webhook role selected without configured worker ports".into(),
            })?;
            loop {
                let worker = worker.clone();
                tokio::task::spawn_blocking(move || worker.poll(memory_core::events::now_us()))
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
    timeout: std::time::Duration,
    vectors: Option<Arc<crate::vector_store::VectorStore>>,
    provider: Arc<indexer_worker::ProviderRegistry>,
}

#[cfg(feature = "indexer")]
impl IndexerService {
    pub fn from_environment(
        database: impl Into<std::path::PathBuf>,
        vectors: Option<Arc<crate::vector_store::VectorStore>>,
    ) -> Result<Self, crate::errors::MCSError> {
        let timeout = std::time::Duration::from_secs(10);
        let provider = indexer_worker::ProviderRegistry::from_environment(timeout)
            .map_err(|error| crate::errors::MCSError::MemoryError(error.to_string()))?;
        Ok(Self::with_provider(
            database,
            vectors,
            Arc::new(provider),
            timeout,
        ))
    }

    fn with_provider(
        database: impl Into<std::path::PathBuf>,
        vectors: Option<Arc<crate::vector_store::VectorStore>>,
        provider: Arc<indexer_worker::ProviderRegistry>,
        timeout: std::time::Duration,
    ) -> Self {
        Self {
            database: database.into(),
            timeout,
            vectors,
            provider,
        }
    }
}

#[cfg(feature = "indexer")]
impl RoleService for IndexerService {
    fn run(&self) -> memory_runtime::RoleFuture {
        let database = self.database.clone();
        let timeout = self.timeout;
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
                    let worker = indexer_worker::IndexerWorker::new(database, provider, timeout);
                    worker.run_once(now).map_err(|error| error.to_string())?;
                    if let Some(vectors) = vectors {
                        vectors
                            .reconcile_managed_snapshot()
                            .map_err(|error| error.to_string())?;
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

#[cfg(all(test, feature = "indexer"))]
mod tests {
    use super::*;

    #[test]
    fn indexer_service_reuses_its_provider_registry_across_polls() {
        let provider = Arc::new(indexer_worker::ProviderRegistry::new(None, None));
        let service = IndexerService::with_provider(
            "memory.db",
            None,
            Arc::clone(&provider),
            std::time::Duration::from_secs(1),
        );

        assert!(Arc::ptr_eq(&provider, &service.provider));
        let first_poll = Arc::clone(&service.provider);
        let second_poll = Arc::clone(&service.provider);
        assert!(Arc::ptr_eq(&first_poll, &second_poll));
    }
}

#[cfg(all(test, feature = "webhooks"))]
mod webhook_tests {
    use super::*;
    #[tokio::test]
    async fn selected_service_fails_observably_without_worker_ports() {
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
