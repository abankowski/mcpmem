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
            match transport {
                Transport::Stdio => server.run_stdio().await,
                Transport::Http => server.run_http(&bind_addr).await,
            }
            .map_err(|error| RuntimeError::RoleFailed {
                role: RuntimeRole::Mcp,
                message: error.to_string(),
            })
        })
    }
}
