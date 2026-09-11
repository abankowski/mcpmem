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
    // The webhook worker polls the memory database directly, in its own
    // connection. An empty `[webhooks]` section is a valid fail-closed
    // worker: it refuses every delivery until the operator names an
    // allowlisted host and a signing key.
    #[cfg(feature = "webhooks")]
    let services = if config
        .roles
        .roles()
        .contains(&runtime::RuntimeRole::Webhooks)
    {
        let worker = runtime::WebhookService::with_config(
            config.memory_file_path.clone(),
            crate::config_file::webhook_worker_config(file.as_ref().map(|(_, f)| f))?,
        )?;
        services.with_webhooks(Arc::new(worker))
    } else {
        services
    };
    // The registry is published for the whole process, not only for the
    // worker. `semantic_search` embeds the query text on read, and an operator
    // may split the roles across two hosts: one `--role mcp`, one
    // `--role indexer`. The MCP host still has to embed every query. Tying the
    // registry to the worker role would hide the tool in that shape, and in the
    // default single-role shape too.
    #[cfg(feature = "indexer")]
    let loaded = file.as_ref().map(|(_, loaded)| loaded);
    // Adoption uses this validated profile after the provider registry exists.
    #[cfg(feature = "indexer")]
    let spec = config_file::profile_spec(loaded)?;
    #[cfg(feature = "indexer")]
    let profile_uses_bedrock = spec
        .as_ref()
        .is_some_and(|spec| spec.provider_kind == "bedrock");
    #[cfg(feature = "indexer")]
    if profile_uses_bedrock && !cfg!(feature = "bedrock") {
        return Err(mcpmem::errors::MCSError::InvalidParams(
            "config 'indexer.provider': 'bedrock' needs the `bedrock` Cargo feature".into(),
        )
        .into());
    }
    #[cfg(feature = "indexer")]
    let provider = {
        let environment = mcpmem_indexer::ProviderSettings::from_environment();
        let file_names_endpoint = loaded.is_some_and(|file| {
            file.indexer.ollama_url.is_some() || file.indexer.openai_url.is_some()
        });
        let file_supplies_openai_key = environment.openai_url.is_some()
            && environment.openai_api_key.is_none()
            && loaded.is_some_and(|file| file.indexer.openai_api_key_file.is_some());
        // Read the key file only when an endpoint can use it. A key by itself
        // names no provider, so a stale key file must not stop this process.
        let settings = if file_names_endpoint || profile_uses_bedrock || file_supplies_openai_key {
            config_file::indexer_settings(loaded)?
        } else {
            environment
        };
        // Publication decides whether a query tool is advertised at all, so it
        // must never publish a registry that holds no provider.
        //
        // A named endpoint is the signal for every provider that has one. A key
        // on its own names none, and `from_settings` rejects a key that arrives
        // without its URL.
        //
        // Bedrock names no endpoint. The validated profile selects it after the
        // feature check above.
        let bedrock = profile_uses_bedrock;
        if settings.ollama_url.is_some() || settings.openai_url.is_some() || bedrock {
            let provider = runtime::IndexerService::provider_registry(settings).await?;
            mcpmem::indexer_provider::init(Arc::clone(&provider));
            info!("Embedding provider published: this process can embed a query text");
            provider
        } else {
            // Nothing is named, so nothing is published, and a query tool that
            // cannot work stays hidden. The worker still needs an instance. An
            // empty registry fails every job it claims, with its own error text.
            Arc::new(mcpmem_indexer::ProviderRegistry::new(None, None))
        }
    };
    #[cfg(feature = "indexer")]
    let services = if config
        .roles
        .roles()
        .contains(&runtime::RuntimeRole::Indexer)
    {
        let vectors = mcp_server.vector_store();
        adopt_index_profile(spec, vectors.as_deref())?;
        services.with_indexer(Arc::new(runtime::IndexerService::with_provider(
            config.memory_file_path.clone(),
            vectors,
            provider,
        )))
    } else {
        // A rebuild fills a queue that only the worker drains, so a process
        // without the `indexer` role adopts nothing. A query text can still be
        // embedded here, because publication above is not tied to the role.
        if file
            .as_ref()
            .is_some_and(|(_, loaded)| !loaded.indexer.is_empty())
        {
            tracing::warn!(
                "config file section [indexer] is set, but this process runs no `indexer` role: \
                 it adopts no index profile and embeds no entity on write. Add \
                 --role mcp,indexer, or run the `indexer` role in another process."
            );
        }
        services
    };
    let services = Arc::new(services);
    let running_roles = runtime::RuntimeComposition::start(config.roles.clone(), services)?;
    info!(roles = ?running_roles.lifecycle(), "Runtime roles started");
    running_roles.wait_for_shutdown().await?;

    info!("Server shutdown complete");
    Ok(())
}

/// Moves the vector store onto the index profile that the configuration file
/// names.
///
/// Startup calls this on every boot. An unchanged configuration costs nothing,
/// because [`mcpmem::vector_store::VectorStore::adopt_profile`] compares the
/// profile fingerprint and reports `Unchanged`.
///
/// An error stops startup on purpose. A half-configured vector space would
/// answer a search over one embedding space with vectors from another.
#[cfg(feature = "indexer")]
fn adopt_index_profile(
    spec: Option<config_file::ProfileSpec>,
    vectors: Option<&mcpmem::vector_store::VectorStore>,
) -> Result<()> {
    use mcpmem::vector_store::AdoptOutcome;

    let Some(spec) = spec else {
        return Ok(());
    };
    let Some(vectors) = vectors else {
        tracing::warn!(
            "config file section [indexer] names an index profile, but the vector subsystem is \
             off, so no profile is adopted. Enable it with --enable-vectors or --enable-all."
        );
        return Ok(());
    };
    let profile = spec.to_profile();
    match vectors.adopt_profile(&profile)? {
        AdoptOutcome::Unchanged => tracing::debug!(
            provider = %profile.provider_kind,
            model = %profile.model,
            dimensions = profile.dimensions,
            "the store already serves this index profile"
        ),
        AdoptOutcome::RebuildInProgress => info!(
            provider = %profile.provider_kind,
            model = %profile.model,
            dimensions = profile.dimensions,
            "a rebuild into this index profile is already running; the worker continues it"
        ),
        AdoptOutcome::RebuildStarted => tracing::warn!(
            provider = %profile.provider_kind,
            model = %profile.model,
            dimensions = profile.dimensions,
            "index profile adopted: every live entity is queued for embedding, and a direct \
             vector write is refused from now on"
        ),
        AdoptOutcome::PreviousRebuildFailed(reason) => tracing::error!(
            provider = %profile.provider_kind,
            model = %profile.model,
            dimensions = profile.dimensions,
            reason = %reason,
            "the last rebuild into this index profile failed and stays failed; the store keeps \
             serving the profile it served before"
        ),
    }
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
