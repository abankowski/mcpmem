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

#[cfg(feature = "indexer")]
pub struct IndexerService {
    database: std::path::PathBuf,
    timeout: std::time::Duration,
    vectors: Option<Arc<crate::vector_store::VectorStore>>,
}

#[cfg(feature = "indexer")]
impl IndexerService {
    pub fn from_environment(
        database: impl Into<std::path::PathBuf>,
        vectors: Option<Arc<crate::vector_store::VectorStore>>,
    ) -> Result<Self, crate::errors::MCSError> {
        Ok(Self {
            database: database.into(),
            timeout: std::time::Duration::from_secs(10),
            vectors,
        })
    }
}

#[cfg(feature = "indexer")]
impl RoleService for IndexerService {
    fn run(&self) -> memory_runtime::RoleFuture {
        let database = self.database.clone();
        let timeout = self.timeout;
        let vectors = self.vectors.clone();
        Box::pin(async move {
            loop {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros()
                    .min(i64::MAX as u128) as i64;
                let database = database.clone();
                let vectors = vectors.clone();
                tokio::task::spawn_blocking(move || {
                    let provider = indexer_worker::ProviderRegistry::from_environment(timeout)
                        .map_err(|error| error.to_string())?;
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
