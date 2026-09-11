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
    /// Canonical HTTPS URL of this server: scheme and host lowercased, no
    /// trailing slash, no query and no fragment. A path prefix is legal — the
    /// MCP authorization specification names `https://mcp.example.com/server/mcp`
    /// as a canonical resource URI. Never derived from the Host header.
    pub public_url: String,
    /// Upstream OpenID Connect issuer, normalized the same way as `public_url`.
    pub oidc_issuer: String,
    pub oidc_client_id: String,
    /// `None` for a public client.
    pub oidc_client_secret: Option<Arc<str>>,
    /// The humans allowed to authorize, and the scopes each one may grant.
    pub principals: Vec<crate::principals::PrincipalEntry>,
    /// Hosts allowed to host a client metadata document. Each entry is matched
    /// as a whole host, case-insensitively: `claude.ai` does not admit
    /// `auth.claude.ai`, which needs its own entry. See
    /// `mcpmem_oauth::registration::resolve_metadata_document`.
    pub cimd_allowed_domains: Vec<String>,
    /// Declares that a reverse proxy terminates TLS in front of this server.
    ///
    /// Two effects, and neither of them reads `X-Forwarded-Proto`. The header
    /// is not read anywhere in this workspace, and it does not need to be: the
    /// canonical URL every document and every token is bound to comes from
    /// `--public-url`, never from the request.
    ///
    /// - It stands in for `--tls-cert`/`--tls-key` at startup, so that
    ///   `--oidc-issuer` is not refused for want of TLS this process does not
    ///   terminate.
    /// - It selects `X-Forwarded-For` as the peer source the per-peer request
    ///   limits count against (`crate::oauth_routes::Peer`). That is the whole
    ///   of its runtime effect, and it is why the process must then be bound
    ///   where only the proxy can reach it: a caller that connects directly
    ///   writes its own header and so chooses its own bucket.
    pub trust_forwarded_proto: bool,
}

/// Normalize an HTTPS URL given on the command line so that later string
/// comparisons (the `aud` claim, the resource indicator, a concatenated
/// discovery path) can be plain equality: strip the trailing slash, lowercase
/// the scheme and the host, and refuse anything that cannot identify one
/// server. `flag` names the flag in the message. A path prefix is kept, and
/// keeps its case; a query, a fragment or a userinfo part is refused, because
/// RFC 8707 forbids all three in a resource indicator and the OpenID Connect
/// issuer rule forbids them in an issuer identifier.
fn normalize_https_url(flag: &str, value: &str) -> Result<String> {
    if value.contains('?') || value.contains('#') {
        return Err(MCSError::InvalidParams(format!(
            "{flag} must carry no query string and no fragment"
        )));
    }
    let (scheme, rest) = value
        .split_once("://")
        .ok_or_else(|| MCSError::InvalidParams(format!("{flag} must be an absolute https URL")))?;
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(MCSError::InvalidParams(format!(
            "{flag} must use the https scheme"
        )));
    }
    // Trim below the scheme, not above it: trimming the whole value would eat
    // the `//` of a bare `https://` and report a missing scheme instead of a
    // missing host.
    let rest = rest.trim_end_matches('/');
    let (host, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    if host.is_empty() {
        return Err(MCSError::InvalidParams(format!("{flag} must name a host")));
    }
    if host.contains('@') {
        // Lowercasing the authority would silently rewrite a case-sensitive
        // password, and neither specification allows userinfo here anyway.
        return Err(MCSError::InvalidParams(format!(
            "{flag} must carry no userinfo; remove the part before the '@'"
        )));
    }
    Ok(format!("https://{}{path}", host.to_ascii_lowercase()))
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

        // A `--no-default-features` build compiles out `/oauth/authorize`,
        // `/oauth/callback`, `/oauth/consent`, `/oauth/token` and
        // `/oauth/revoke` (`src/oauth_routes::attach`) while the two discovery
        // documents and `POST /oauth/register` still answer. Serving half an
        // authorization server is worse than refusing to start: a connector
        // discovers this server, registers, and then fails at the login hop
        // with nothing to read. It fails closed on `/mcp` — no credential can
        // ever be issued — so this is a startup footgun rather than an
        // exposure, and a refusal here is the only answer an operator can act
        // on.
        #[cfg(not(feature = "oauth"))]
        if args.oidc_issuer.is_some() {
            return Err(MCSError::InvalidParams(
                "--oidc-issuer needs the `oauth` feature; this build serves no \
                 authorization, consent or token endpoint"
                    .into(),
            ));
        }

        let oauth = if let Some(issuer) = args.oidc_issuer.clone() {
            if !roles.roles().contains(&crate::runtime::RuntimeRole::Mcp) {
                return Err(MCSError::InvalidParams(
                    "--oidc-issuer requires the mcp role".into(),
                ));
            }
            if args.transport != crate::Transport::Http {
                return Err(MCSError::InvalidParams(
                    "--oidc-issuer requires --transport http; stdio is local and \
                     already holds every scope"
                        .into(),
                ));
            }
            if tls_cert.is_none() && !args.oauth_trust_forwarded_proto {
                return Err(MCSError::InvalidParams(
                    "--oidc-issuer needs TLS; pass --tls-cert and --tls-key, or \
                     --oauth-trust-forwarded-proto behind a proxy that terminates TLS"
                        .into(),
                ));
            }
            let oidc_issuer = normalize_https_url("--oidc-issuer", &issuer)?;
            let public_url = args.public_url.clone().ok_or_else(|| {
                MCSError::InvalidParams("--oidc-issuer requires --public-url".into())
            })?;
            let public_url = normalize_https_url("--public-url", &public_url)?;
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
                oidc_issuer,
                oidc_client_id: client_id,
                oidc_client_secret,
                principals,
                cimd_allowed_domains,
                trust_forwarded_proto: args.oauth_trust_forwarded_proto,
            })
        } else {
            // Fail closed, as `--auth-token-file` does above: an OAuth flag
            // without `--oidc-issuer` is a misconfiguration, not a no-op. The
            // principals file would never be opened, so its errors would go
            // unseen while the server ran with OAuth off.
            if args.public_url.is_some()
                || args.oidc_client_id.is_some()
                || args.oidc_client_secret_file.is_some()
                || args.principals_file.is_some()
                || !args.cimd_allowed_domains.is_empty()
                || args.oauth_trust_forwarded_proto
            {
                return Err(MCSError::InvalidParams(
                    "the OAuth flags need --oidc-issuer, which turns OAuth on".into(),
                ));
            }
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
