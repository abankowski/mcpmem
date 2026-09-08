use anyhow::Result;
use clap::Parser;
use mcp_memory::{config, runtime, server};
use std::sync::Arc;
use tracing::info;

fn main() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(inner_main())
}

async fn inner_main() -> Result<()> {
    let args = mcp_memory::Args::parse();

    // Install the rustls `ring` crypto provider as the process default up front
    // (idempotent) so the HTTPS transport can build its TLS config. See src/tls.rs.
    mcp_memory::tls::ensure_crypto_provider();

    init_tracing(&args.log_level)?;

    info!("Starting MCP Memory Server");
    info!("Version: {}", env!("CARGO_PKG_VERSION"));

    let config = Arc::new(config::Config::from_args(&args)?);
    info!("Memory file: {}", config.memory_file_path);

    // Tool exposure: nothing is advertised unless a category was enabled.
    if config.enabled_categories.is_empty() {
        tracing::warn!(
            "No tool categories enabled — the server will expose ZERO tools. \
             Enable categories with --enable-<category> (e.g. --enable-graph-read \
             --enable-graph-write) or expose everything with --enable-all."
        );
    } else {
        let slugs: Vec<&str> = config.enabled_categories.iter().map(|c| c.slug()).collect();
        info!("Tool categories enabled: {}", slugs.join(", "));
    }

    let mcp_server = Arc::new(server::MCPServer::new(
        (*config).clone(),
        args.vector_config(),
    )?);
    info!("Server initialized successfully");

    let transport = runtime::McpTransportService::new(
        Arc::clone(&mcp_server),
        config.transport,
        config.bind_addr.clone(),
    );
    let services = runtime::AppServices::new(Arc::new(transport));
    #[cfg(feature = "indexer")]
    let services = if config
        .roles
        .roles()
        .contains(&runtime::RuntimeRole::Indexer)
    {
        services.with_indexer(Arc::new(runtime::IndexerService::from_environment(
            config.memory_file_path.clone(),
            mcp_server.vector_store(),
        )?))
    } else {
        services
    };
    let services = Arc::new(services);
    let running_roles = runtime::RuntimeComposition::start(config.roles.clone(), services)?;
    info!(roles = ?running_roles.lifecycle(), "Runtime roles started");
    running_roles.wait_for_shutdown().await?;

    info!("Server shutdown complete");
    Ok(())
}

fn init_tracing(log_level: &str) -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(log_level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();

    Ok(())
}
