pub mod actions;
pub mod authz;
#[cfg(feature = "code")]
pub mod code;
#[cfg(feature = "code")]
pub mod code_registry;
#[cfg(feature = "code")]
pub mod code_vec;
#[cfg(feature = "code")]
pub mod code_vec_registry;
pub mod config;
pub mod config_file;
pub use mcpmem_core::errors;
pub mod http;
#[cfg(feature = "indexer")]
pub mod indexer_provider;
pub mod kg;
pub mod oauth_routes;
pub mod principals;
pub mod protocol;
#[cfg(feature = "code")]
pub mod repos;
pub mod runtime;
pub mod runtime_principals;
pub mod server;
pub mod taxonomy;
pub mod tls;
pub mod tools;
pub use mcpmem_core::types;
pub mod vector_actions;
pub mod vector_store;
pub mod watcher;

use clap::{Parser, ValueEnum};
use vector_store::VectorConfig;

/// Wire transport the server listens on. The JSON-RPC/MCP semantics are
/// identical across all three — only the framing differs.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum Transport {
    /// Newline-delimited JSON-RPC over stdin/stdout (default; for Claude
    /// Desktop / Claude Code and other process-spawning clients).
    Stdio,
    /// MCP Streamable HTTP: POST JSON-RPC to `/mcp` (responses as JSON or, when
    /// the client `Accept`s it, an SSE stream), plus `GET /mcp` for a standalone
    /// server→client SSE stream.
    Http,
}

#[derive(Parser, Debug)]
#[command(name = "MCP Memory Server")]
#[command(about = "Knowledge graph memory server for MCP — entities, relations, and observations persisted in SQLite with FTS5 search", long_about = None)]
#[command(version)]
pub struct Args {
    /// Path to the memory file
    #[arg(short = 'f', long = "memory-file")]
    pub memory_file: Option<String>,

    /// TOML configuration file. Falls back to the `MCP_MEMORY_CONFIG` env var.
    /// A command-line flag always wins over the file, and so does any
    /// environment variable the same setting reads.
    #[arg(long = "config", value_name = "PATH")]
    pub config: Option<String>,

    /// SQLite synchronous mode: `async` (default) or `sync`. Falls back to the
    /// `MCP_MEMORY_DURABILITY` env var.
    #[arg(long = "durability")]
    pub durability: Option<String>,

    /// Transport to listen on: stdio or http
    #[arg(short = 't', long = "transport", value_enum, default_value_t = Transport::Stdio)]
    pub transport: Transport,

    /// Address to bind for the `http` transport
    #[arg(short = 'b', long = "bind", default_value = "127.0.0.1:8080")]
    pub bind: String,

    /// Log level
    #[arg(short, long, default_value = "info")]
    pub log_level: String,

    /// Append logs to this file instead of stderr. The file is created when
    /// absent and never truncated; rotation and retention belong to the
    /// operator.
    #[arg(long = "log-file", value_name = "PATH")]
    pub log_file: Option<String>,

    /// Runtime roles to start in this process. Repeat the flag or separate roles with commas.
    /// Defaults to `mcp`, preserving the existing single-server behavior.
    #[arg(long = "role", value_delimiter = ',', value_name = "ROLE")]
    pub roles: Vec<String>,

    /// Deprecated MCP string observations adapter (1.x only; removed in 2.0.0).
    #[arg(long)]
    pub legacy_observations: bool,

    /// Bearer token required on the `http` (`Authorization` header) transport.
    /// Overrides `--auth-token-file` and the `MCP_MEMORY_AUTH_TOKEN` env var.
    /// stdio is never authenticated.
    #[arg(long = "auth-token")]
    pub auth_token: Option<String>,

    /// Path to a file whose trimmed contents are the bearer token. An empty
    /// file is rejected (fail closed). Ignored if `--auth-token` is set.
    #[arg(long = "auth-token-file")]
    pub auth_token_file: Option<String>,

    /// Canonical HTTPS URL of this server, for example `https://mem.example.com`.
    /// Required with `--oidc-issuer`. Never derived from the Host header.
    #[arg(long = "public-url")]
    pub public_url: Option<String>,

    /// Upstream OpenID Connect issuer. Turns the OAuth authorization server on.
    #[arg(long = "oidc-issuer")]
    pub oidc_issuer: Option<String>,

    /// Client identifier registered at the upstream provider.
    #[arg(long = "oidc-client-id")]
    pub oidc_client_id: Option<String>,

    /// File holding the upstream client secret. Omit for a public client.
    #[arg(long = "oidc-client-secret-file")]
    pub oidc_client_secret_file: Option<String>,

    /// JSON file listing the humans allowed to authorize, and their scopes.
    #[arg(long = "principals-file")]
    pub principals_file: Option<String>,

    /// Whole host allowed to host a client metadata document; a subdomain
    /// needs its own entry. Repeatable.
    #[arg(long = "cimd-allowed-domain", value_name = "DOMAIN")]
    pub cimd_allowed_domains: Vec<String>,

    /// A reverse proxy terminates TLS in front of this server. Stands in for
    /// --tls-cert/--tls-key, and makes the per-peer request limits count
    /// X-Forwarded-For instead of the connection address.
    #[arg(long = "oauth-trust-forwarded-proto")]
    pub oauth_trust_forwarded_proto: bool,

    /// Record a rejected would-be human and answer with a pending page,
    /// instead of refusing outright. Entries expire after
    /// `--approval-waitlist-ttl-seconds` and the list holds at most 25.
    #[arg(long = "approval-waitlist")]
    pub approval_waitlist: bool,

    /// How long a waitlist entry lives, measured from first sighting.
    /// 0 disables the TTL sweep; the 25-entry cap always applies.
    #[arg(long = "approval-waitlist-ttl-seconds")]
    pub approval_waitlist_ttl_seconds: Option<u64>,

    /// A scope a promoted entry starts with. Repeatable; defaults to
    /// graph-read.
    #[arg(long = "default-new-principal-scope")]
    pub default_new_principal_scopes: Option<Vec<String>>,

    /// Scopes granted to the static bearer token. Defaults to every category.
    #[arg(
        long = "static-bearer-scopes",
        value_delimiter = ',',
        value_name = "SCOPE"
    )]
    pub static_bearer_scopes: Vec<String>,

    /// SQLite mmap size in bytes (default: 64 MiB).
    #[arg(long = "mmap-size", default_value_t = 67108864)]
    pub mmap_size: i64,

    /// SQLite page size in bytes; power of two (default: 4096, matches the Linux
    /// page / filesystem block size). Only applies to a freshly-created database.
    #[arg(long = "page-size", default_value_t = 4096)]
    pub page_size: i64,

    /// SQLite page cache size in MiB (default: 32).
    #[arg(long = "cache-size-mb", default_value_t = 32)]
    pub cache_size_mb: i64,

    /// SQLite busy timeout in milliseconds (default: 5000).
    #[arg(long = "busy-timeout-ms", default_value_t = 5000)]
    pub busy_timeout_ms: u64,

    /// Interval in milliseconds for a background `wal_checkpoint(PASSIVE)` that
    /// bounds the durability window in async mode (default: 500). 0 disables it.
    #[arg(long = "wal-flush-ms", default_value_t = 500)]
    pub wal_flush_ms: u64,

    /// Entity-metadata LRU cache capacity (0 falls back to 10000).
    #[arg(long = "lru-cache-size", default_value_t = 10000)]
    pub lru_cache_size: usize,

    /// Max stdio requests dispatched concurrently (default: 8). Responses
    /// are correlated by JSON-RPC id and may arrive in completion order;
    /// set 1 for strict request/response ordering when pipelining
    /// order-dependent writes.
    #[arg(long = "stdio-concurrency", default_value_t = 8)]
    pub stdio_concurrency: usize,

    /// Number of read-only SQLite connections backing concurrent reads. WAL
    /// mode allows readers to run in parallel with each other and the single
    /// writer; a larger pool raises read concurrency at the cost of a little
    /// memory (each connection carries its own page cache). `0` (default)
    /// auto-scales to the CPU count, clamped to [1, 32].
    #[arg(long = "read-pool-size", default_value_t = 4)]
    pub read_pool_size: usize,

    /// Path to a PEM certificate chain to serve the `http` transport over TLS
    /// (HTTPS). Requires --tls-key. Falls back to the MCP_TLS_CERT env var.
    /// When unset, the `http` transport stays plaintext.
    #[arg(long = "tls-cert")]
    pub tls_cert: Option<String>,

    /// Path to the PEM private key matching --tls-cert. Falls back to the
    /// MCP_TLS_KEY env var.
    #[arg(long = "tls-key")]
    pub tls_key: Option<String>,

    // ── Tool exposure ────────────────────────────────────────────────────
    // No tools are exposed unless explicitly enabled. Each flag turns on one
    // category (hidden from tools/list and rejected from tools/call when its
    // category is disabled). Use --enable-all for every category at once.
    /// Expose ALL tool categories (overrides the individual --enable-* flags).
    #[arg(long = "enable-all", default_value_t = false)]
    pub enable_all: bool,

    /// Enable read-only knowledge-graph tools (queries, traversal, export).
    #[arg(long = "enable-graph-read", default_value_t = false)]
    pub enable_graph_read: bool,

    /// Enable knowledge-graph mutation tools (create/delete/merge/compact/upsert).
    #[arg(long = "enable-graph-write", default_value_t = false)]
    pub enable_graph_write: bool,

    /// Enable vector / semantic search: the `vector_*` and `hybrid_search` tools
    /// backed by the durable chunk index. The `--embedding-dims` flag only
    /// takes effect when this is set.
    #[arg(long = "enable-vectors", default_value_t = false)]
    pub enable_vectors: bool,

    /// Enable tree-sitter code-symbol indexing: the `code_*` tools that parse
    /// source files and store symbols (and call/define edges) in the graph.
    /// Only effective when built with the `code` feature (on by default).
    #[arg(long = "enable-code", default_value_t = false)]
    pub enable_code: bool,

    /// Embedding dimension for vector search (default: 384). Requires
    /// --enable-vectors. The serving index profile owns the dimension that
    /// actually validates chunk rows; this flag is the startup default.
    #[arg(long = "embedding-dims", default_value_t = 384)]
    pub embedding_dims: u32,

    /// Embedding dimension for code semantic search — the `code_embed` /
    /// `code_semantic_search` HNSW index (default: 768). Requires --enable-code.
    #[arg(long = "code-embedding-dims", default_value_t = 768)]
    pub code_embedding_dims: u32,
}

impl Args {
    /// Resolve the set of enabled tool categories from the `--enable-*` flags.
    /// `--enable-all` turns on every category; otherwise only the categories
    /// whose individual flag is set. With no flags, the result is empty and no
    /// tools are exposed.
    pub fn enabled_categories(&self) -> Vec<tools::ToolCategory> {
        use tools::ToolCategory as C;
        if self.enable_all {
            return C::ALL.to_vec();
        }
        let mut cats = Vec::new();
        let mut push = |on: bool, cat: C| {
            if on {
                cats.push(cat);
            }
        };
        push(self.enable_graph_read, C::GraphRead);
        push(self.enable_graph_write, C::GraphWrite);
        push(self.enable_vectors, C::Vectors);
        push(self.enable_code, C::Code);
        cats
    }

    /// Build the vector index configuration from the `--embedding-dims` flag.
    /// Only meaningful when `--enable-vectors` is set.
    pub const fn vector_config(&self) -> VectorConfig {
        VectorConfig {
            dims: self.embedding_dims,
        }
    }
}
