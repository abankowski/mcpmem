use anyhow::Result;
use mcpmem::{config, config_file, runtime, server};
use std::sync::Arc;
use tracing::info;

fn main() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(inner_main())
}

async fn inner_main() -> Result<()> {
    // One entry point, shared with `tests/config_file.rs`: parse the command
    // line, then layer the configuration file under it. A flag stays ahead of
    // a file value because the merge records what the command line actually
    // carried, rather than comparing a parsed value against its default.
    let (args, file) = config_file::resolve(std::env::args_os())?;

    // Install the rustls `ring` crypto provider as the process default up front
    // (idempotent) so the HTTPS transport can build its TLS config. See src/tls.rs.
    mcpmem::tls::ensure_crypto_provider();

    init_tracing(&args.log_level)?;

    info!("Starting MCP Memory Server");
    info!("Version: {}", env!("CARGO_PKG_VERSION"));

    if let Some((path, loaded)) = file.as_ref() {
        info!("Configuration file: {}", path.display());
        if cfg!(not(feature = "indexer")) && !loaded.indexer.is_empty() {
            tracing::warn!(
                "config file section [indexer] ignored: this build carries no `indexer` Cargo feature"
            );
        }
    }
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
    #[cfg(feature = "webhooks")]
    let services = if config
        .roles
        .roles()
        .contains(&runtime::RuntimeRole::Webhooks)
    {
        services.with_webhooks(Arc::new(runtime::WebhookService::disabled()))
    } else {
        services
    };
    #[cfg(feature = "indexer")]
    let services = if config
        .roles
        .roles()
        .contains(&runtime::RuntimeRole::Indexer)
    {
        let settings = config_file::indexer_settings(file.as_ref().map(|(_, f)| f))?;
        services.with_indexer(Arc::new(runtime::IndexerService::with_settings(
            config.memory_file_path.clone(),
            mcp_server.vector_store(),
            &settings,
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
