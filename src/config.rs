use crate::Transport;
use crate::errors::{MCSError, Result};
use crate::runtime::RoleSet;
use crate::tools::ToolCategory;
use std::sync::Arc;

pub use mcpmem_core::storage::{Durability, SqliteTuning};

#[derive(Debug, Clone)]
pub struct Config {
    pub memory_file_path: String,
    pub transport: Transport,
    pub bind_addr: String,
    pub durability: Durability,
    /// Optional bearer token required on the `tcp` and `http` transports. When
    /// `None`, those transports accept unauthenticated connections (stdio is
    /// always local and never authenticated).
    pub auth_token: Option<Arc<str>>,
    pub mmap_size: i64,
    /// `PRAGMA page_size` in bytes (fresh DB only).
    pub page_size: i64,
    /// `PRAGMA cache_size` magnitude in KiB.
    pub cache_size_kb: i64,
    /// `PRAGMA busy_timeout` in milliseconds.
    pub busy_timeout_ms: u64,
    /// Interval in milliseconds for the background `wal_checkpoint(PASSIVE)`
    /// flush. `0` disables it (rely on SQLite auto-checkpoint + maintenance).
    pub wal_flush_ms: u64,
    pub lru_cache_size: usize,
    /// Size of the read-only connection pool (concurrent reads). Always >= 1.
    pub read_pool_size: usize,
    /// Max stdio requests dispatched concurrently (>= 1). Responses are
    /// written in completion order (JSON-RPC ids correlate them); 1 restores
    /// strict request/response ordering.
    pub stdio_concurrency: usize,
    /// PEM certificate chain for serving the `http` transport over TLS (HTTPS).
    /// `None` (the default) keeps the transport plaintext. Engaged only when
    /// both `tls_cert` and `tls_key` are set.
    pub tls_cert: Option<std::path::PathBuf>,
    /// PEM private key matching `tls_cert`.
    pub tls_key: Option<std::path::PathBuf>,
    /// Enable the vector / semantic-search subsystem (`vector_*` + `hybrid_search`
    /// tools backed by a usearch HNSW index). Off by default. Derived from the
    /// `vectors` category being enabled.
    pub vectors_enabled: bool,
    /// Enable the tree-sitter code-symbol subsystem (`code_*` tools). Off by
    /// default. Only effective when built with the `code` feature. Derived from
    /// the `code` category being enabled.
    pub code_enabled: bool,
    /// Embedding dimension for the per-project code semantic-search HNSW index
    /// (`code_embed` / `code_semantic_search`). Default 768.
    pub code_embedding_dims: u32,
    /// Tool categories exposed by this server. Empty (the default) means no
    /// tools are advertised or callable until enabled with `--enable-*`.
    pub enabled_categories: Vec<ToolCategory>,
    /// Runtime roles selected for this process. Defaults to the existing MCP server.
    pub roles: RoleSet,
    /// MCP-only string observation adapter; deprecated and removed in 2.0.0.
    pub legacy_observations: bool,
    /// OAuth authorization server settings. `None` keeps OAuth off.
    pub oauth: Option<OAuthConfig>,
    /// Scopes granted to the static bearer principal.
    pub bearer_scopes: Vec<ToolCategory>,
}

/// Settings for the OAuth 2.1 authorization server. Present only when
/// `--oidc-issuer` is given, which is what turns OAuth on.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    /// Canonical HTTPS origin of this server, without a trailing slash. Never
    /// derived from the Host header.
    pub public_url: String,
    /// Upstream OpenID Connect issuer, without a trailing slash.
    pub oidc_issuer: String,
    pub oidc_client_id: String,
    /// `None` for a public client.
    pub oidc_client_secret: Option<Arc<str>>,
    /// The humans allowed to authorize, and the scopes each one may grant.
    pub principals: Vec<crate::principals::PrincipalEntry>,
    /// Domains allowed to host a client metadata document.
    pub cimd_allowed_domains: Vec<String>,
    /// Trust `X-Forwarded-Proto` from a reverse proxy that terminates TLS.
    pub trust_forwarded_proto: bool,
}

/// Resolve the read-only connection-pool size. `0` means "auto": scale to the
/// number of available CPUs (clamped to `[1, 32]` so a many-core host doesn't
/// open an unreasonable number of connections, each carrying its own page
/// cache). Any explicit value is honoured but floored at 1.
pub fn resolve_read_pool_size(requested: usize) -> usize {
    if requested == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 32)
    } else {
        requested.max(1)
    }
}

impl Config {
    /// Build the SQLite pragma tuning from this config, keeping the fixed
    /// `journal_size_limit` default.
    pub fn sqlite_tuning(&self) -> SqliteTuning {
        SqliteTuning {
            mmap_size: self.mmap_size,
            page_size: self.page_size,
            cache_size_kb: self.cache_size_kb,
            busy_timeout_ms: self.busy_timeout_ms,
            ..SqliteTuning::default()
        }
    }

    pub fn from_args(args: &super::Args) -> Result<Self> {
        let memory_file_path = args
            .memory_file
            .clone()
            .or_else(|| std::env::var("MEMORY_FILE_PATH").ok())
            .unwrap_or_else(|| "memory.mcpmem".to_string());

        // Resolve the auth token from --auth-token, then --auth-token-file, then
        // the MCP_MEMORY_AUTH_TOKEN env var. A configured-but-empty token file
        // is a hard error: fail closed rather than silently disabling auth.
        let auth_token: Option<Arc<str>> = if let Some(t) = args.auth_token.clone() {
            Some(Arc::from(t.as_str()))
        } else if let Some(path) = args.auth_token_file.clone() {
            let contents = std::fs::read_to_string(&path).map_err(|e| {
                MCSError::InvalidParams(format!("failed to read --auth-token-file '{path}': {e}"))
            })?;
            let token = contents.trim();
            if token.is_empty() {
                return Err(MCSError::InvalidParams(format!(
                    "--auth-token-file '{path}' is empty; refusing to start with auth disabled"
                )));
            }
            Some(Arc::from(token))
        } else {
            std::env::var("MCP_MEMORY_AUTH_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
                .map(|t| Arc::from(t.as_str()))
        };

        let durability = if let Ok(env) = std::env::var("MCP_MEMORY_DURABILITY") {
            env.parse().unwrap_or_else(|e| {
                tracing::warn!("MCP_MEMORY_DURABILITY parse failed: {e}; falling back to Async");
                Durability::Async
            })
        } else {
            Durability::Async
        };

        // TLS cert/key for the `http` transport, from CLI flags or env vars.
        // Both must be supplied together; one without the other is a hard error.
        let tls_cert = args
            .tls_cert
            .clone()
            .or_else(|| std::env::var("MCP_TLS_CERT").ok())
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from);
        let tls_key = args
            .tls_key
            .clone()
            .or_else(|| std::env::var("MCP_TLS_KEY").ok())
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from);
        if tls_cert.is_some() != tls_key.is_some() {
            return Err(MCSError::InvalidParams(
                "--tls-cert and --tls-key must be provided together (or both omitted for plaintext HTTP)"
                    .to_string(),
            ));
        }

        let enabled_categories = args.enabled_categories();
        let roles = if args.roles.is_empty() {
            RoleSet::mcp_only()
        } else {
            RoleSet::parse_csv(&args.roles.join(","))
                .map_err(|error| MCSError::InvalidParams(error.to_string()))?
        };

        if args.legacy_observations && !roles.roles().contains(&crate::runtime::RuntimeRole::Mcp) {
            return Err(MCSError::InvalidParams(
                "--legacy-observations requires the mcp role".into(),
            ));
        }

        let bearer_scopes = if args.static_bearer_scopes.is_empty() {
            ToolCategory::ALL.to_vec()
        } else {
            args.static_bearer_scopes
                .iter()
                .map(|s| s.parse::<ToolCategory>())
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(MCSError::InvalidParams)?
        };

        let oauth = if let Some(issuer) = args.oidc_issuer.clone() {
            if !roles.roles().contains(&crate::runtime::RuntimeRole::Mcp) {
                return Err(MCSError::InvalidParams(
                    "--oidc-issuer requires the mcp role".into(),
                ));
            }
            if tls_cert.is_none() && !args.oauth_trust_forwarded_proto {
                return Err(MCSError::InvalidParams(
                    "--oidc-issuer needs TLS; pass --tls-cert and --tls-key, or \
                     --oauth-trust-forwarded-proto behind a proxy that terminates TLS"
                        .into(),
                ));
            }
            let public_url = args.public_url.clone().ok_or_else(|| {
                MCSError::InvalidParams("--oidc-issuer requires --public-url".into())
            })?;
            let public_url = public_url.trim_end_matches('/').to_owned();
            if !public_url.starts_with("https://") {
                return Err(MCSError::InvalidParams(
                    "--public-url must use the https scheme".into(),
                ));
            }
            let client_id = args.oidc_client_id.clone().ok_or_else(|| {
                MCSError::InvalidParams("--oidc-issuer requires --oidc-client-id".into())
            })?;
            let principals_path = args.principals_file.clone().ok_or_else(|| {
                MCSError::InvalidParams("--oidc-issuer requires --principals-file".into())
            })?;
            let principals = crate::principals::load(&principals_path)?;
            let oidc_client_secret = match args.oidc_client_secret_file.clone() {
                None => None,
                Some(path) => {
                    let text = std::fs::read_to_string(&path).map_err(|e| {
                        MCSError::InvalidParams(format!(
                            "failed to read --oidc-client-secret-file '{path}': {e}"
                        ))
                    })?;
                    let secret = text.trim();
                    if secret.is_empty() {
                        return Err(MCSError::InvalidParams(format!(
                            "--oidc-client-secret-file '{path}' is empty"
                        )));
                    }
                    Some(Arc::from(secret))
                }
            };
            let cimd_allowed_domains = if args.cimd_allowed_domains.is_empty() {
                vec!["claude.ai".to_string(), "chatgpt.com".to_string()]
            } else {
                args.cimd_allowed_domains.clone()
            };
            Some(OAuthConfig {
                public_url,
                oidc_issuer: issuer.trim_end_matches('/').to_owned(),
                oidc_client_id: client_id,
                oidc_client_secret,
                principals,
                cimd_allowed_domains,
                trust_forwarded_proto: args.oauth_trust_forwarded_proto,
            })
        } else {
            None
        };

        Ok(Config {
            memory_file_path,
            transport: args.transport,
            bind_addr: args.bind.clone(),
            durability,
            auth_token,
            mmap_size: args.mmap_size,
            page_size: args.page_size,
            cache_size_kb: args.cache_size_mb.saturating_mul(1024),
            busy_timeout_ms: args.busy_timeout_ms,
            wal_flush_ms: args.wal_flush_ms,
            lru_cache_size: args.lru_cache_size,
            read_pool_size: resolve_read_pool_size(args.read_pool_size),
            stdio_concurrency: args.stdio_concurrency.max(1),
            tls_cert,
            tls_key,
            vectors_enabled: enabled_categories.contains(&ToolCategory::Vectors),
            code_enabled: enabled_categories.contains(&ToolCategory::Code),
            code_embedding_dims: args.code_embedding_dims,
            enabled_categories,
            roles,
            legacy_observations: args.legacy_observations,
            oauth,
            bearer_scopes,
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            memory_file_path: "memory.mcpmem".to_string(),
            transport: Transport::Stdio,
            bind_addr: "127.0.0.1:8080".to_string(),
            durability: Durability::Async,
            auth_token: None,
            mmap_size: 268435456,
            page_size: SqliteTuning::default().page_size,
            cache_size_kb: SqliteTuning::default().cache_size_kb,
            busy_timeout_ms: SqliteTuning::default().busy_timeout_ms,
            wal_flush_ms: 250,
            lru_cache_size: 10000,
            read_pool_size: 8,
            stdio_concurrency: 8,
            tls_cert: None,
            tls_key: None,
            vectors_enabled: false,
            code_enabled: false,
            code_embedding_dims: 768,
            enabled_categories: Vec::new(),
            roles: RoleSet::mcp_only(),
            legacy_observations: false,
            oauth: None,
            bearer_scopes: ToolCategory::ALL.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_read_pool_size_auto_scales_within_bounds() {
        let auto = resolve_read_pool_size(0);
        assert!((1..=32).contains(&auto), "auto pool {auto} out of [1,32]");
        assert_eq!(
            auto,
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .clamp(1, 32)
        );
    }

    #[test]
    fn test_resolve_read_pool_size_honours_explicit_values() {
        assert_eq!(resolve_read_pool_size(1), 1);
        assert_eq!(resolve_read_pool_size(8), 8);
        // A huge explicit value is honoured (only the auto path is clamped).
        assert_eq!(resolve_read_pool_size(100), 100);
    }
}
