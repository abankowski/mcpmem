//! The `--config` TOML file.
//!
//! The file is the lowest-priority source of settings. Precedence is
//! **command-line flag, then environment variable, then file, then the clap
//! default**, and every rule here follows from that one sentence:
//!
//! - a value is applied only when its flag was absent from the command line,
//!   which [`ExplicitArgs`] decides from clap's own value source rather than by
//!   comparing against defaults;
//! - a value whose setting also reads an environment variable is applied only
//!   when that variable is unset, so an ambient deployment override still wins.
//!
//! The file never carries a secret. It names the file that holds one, exactly
//! as `--auth-token-file` and `--oidc-client-secret-file` already do.

use std::collections::HashSet;
use std::path::Path;

use clap::parser::ValueSource;
use clap::{ArgMatches, ValueEnum};
use serde::Deserialize;

use crate::errors::{MCSError, Result};
use crate::{Args, Transport, VecIndex, VecMetric, VecQuant};

/// Environment variable naming the configuration file.
pub const CONFIG_PATH_ENV: &str = "MCP_MEMORY_CONFIG";

/// Environment variables that a setting reads directly. A file value loses to
/// any of them, so the name lives here once and is used by both the merge below
/// and [`crate::config::Config::from_args`].
pub mod env_keys {
    pub const MEMORY_FILE: &str = "MEMORY_FILE_PATH";
    pub const AUTH_TOKEN: &str = "MCP_MEMORY_AUTH_TOKEN";
    pub const DURABILITY: &str = "MCP_MEMORY_DURABILITY";
    pub const TLS_CERT: &str = "MCP_TLS_CERT";
    pub const TLS_KEY: &str = "MCP_TLS_KEY";
}

/// The argument ids clap saw on the command line. Anything else came from a
/// default, so the file may fill it.
pub struct ExplicitArgs {
    explicit: HashSet<String>,
    known: HashSet<String>,
}

impl ExplicitArgs {
    pub fn from_matches(matches: &ArgMatches) -> Self {
        Self {
            explicit: matches
                .ids()
                .filter(|id| matches.value_source(id.as_str()) == Some(ValueSource::CommandLine))
                .map(|id| id.as_str().to_string())
                .collect(),
            known: <Args as clap::CommandFactory>::command()
                .get_arguments()
                .map(|arg| arg.get_id().to_string())
                .collect(),
        }
    }

    /// Only for a test that drives `apply` directly.
    #[cfg(test)]
    fn none() -> Self {
        Self {
            explicit: HashSet::new(),
            known: <Args as clap::CommandFactory>::command()
                .get_arguments()
                .map(|arg| arg.get_id().to_string())
                .collect(),
        }
    }

    /// An id the command does not define would silently report "absent", and
    /// the file would then outrank the command line for that setting. Renaming
    /// an `Args` field compiles clean, so the assertion is the only thing that
    /// catches it.
    fn absent(&self, id: &str) -> bool {
        debug_assert!(
            self.known.contains(id),
            "config merge names an unknown clap id '{id}'"
        );
        !self.explicit.contains(id)
    }
}

/// Probes whether a setting's environment variable is unset. An **empty**
/// variable counts as unset, because every consumer in `Config::from_args`
/// filters an empty value away. Treating it as present made both sources
/// defer to the other: `MCP_MEMORY_AUTH_TOKEN=""` blocked the file's
/// `auth-token-file` and then filtered itself out, and the server started
/// unauthenticated. The same shape downgraded a TLS deployment to plaintext.
pub type EnvProbe<'a> = &'a dyn Fn(&str) -> bool;

/// The production probe. `apply` takes it as a parameter, so a test can prove
/// the precedence rule without mutating process state.
pub fn env_absent(key: &str) -> bool {
    value_absent(std::env::var_os(key).as_deref())
}

/// The rule itself, without the process lookup, so a test can prove the empty
/// case without touching the environment.
#[must_use]
pub fn value_absent(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_none_or(std::ffi::OsStr::is_empty)
}

fn assign<T>(slot: &mut T, value: Option<T>, allow: bool) {
    if allow && let Some(value) = value {
        *slot = value;
    }
}

fn assign_vec<T>(slot: &mut Vec<T>, value: Option<Vec<T>>, allow: bool) {
    match value {
        Some(value) if allow && !value.is_empty() => *slot = value,
        _ => {}
    }
}

fn parse_enum<T: ValueEnum>(field: &str, raw: Option<&String>) -> Result<Option<T>> {
    raw.map(|raw| {
        T::from_str(raw, true)
            .map_err(|error| MCSError::InvalidParams(format!("config '{field}': {error}")))
    })
    .transpose()
}

#[cfg(any(feature = "indexer", test))]
fn read_secret_file(field: &str, path: &str) -> Result<String> {
    let contents = std::fs::read_to_string(path).map_err(|error| {
        MCSError::InvalidParams(format!("config '{field}': cannot read '{path}': {error}"))
    })?;
    let secret = contents.trim();
    if secret.is_empty() {
        return Err(MCSError::InvalidParams(format!(
            "config '{field}': '{path}' is empty"
        )));
    }
    Ok(secret.to_string())
}

/// The whole file. Every section and every key is optional; an unknown key is
/// an error rather than a silent typo.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub storage: StorageSection,
    #[serde(default)]
    pub tools: ToolsSection,
    #[serde(default)]
    pub vectors: VectorsSection,
    #[serde(default)]
    pub security: SecuritySection,
    #[serde(default)]
    pub oauth: OAuthSection,
    #[serde(default)]
    pub indexer: IndexerSection,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ServerSection {
    pub memory_file: Option<String>,
    pub transport: Option<String>,
    pub bind: Option<String>,
    pub log_level: Option<String>,
    pub roles: Option<Vec<String>>,
    pub legacy_observations: Option<bool>,
    pub stdio_concurrency: Option<usize>,
    pub read_pool_size: Option<usize>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct StorageSection {
    pub durability: Option<String>,
    pub mmap_size: Option<i64>,
    pub page_size: Option<i64>,
    pub cache_size_mb: Option<i64>,
    pub busy_timeout_ms: Option<u64>,
    pub wal_flush_ms: Option<u64>,
    pub lru_cache_size: Option<usize>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ToolsSection {
    pub all: Option<bool>,
    pub graph_read: Option<bool>,
    pub graph_write: Option<bool>,
    pub vectors: Option<bool>,
    pub code: Option<bool>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct VectorsSection {
    pub embedding_dims: Option<u32>,
    pub code_embedding_dims: Option<u32>,
    pub index: Option<String>,
    pub metric: Option<String>,
    pub quantization: Option<String>,
    pub connectivity: Option<usize>,
    pub expansion_add: Option<usize>,
    pub expansion_search: Option<usize>,
    pub ivf_nlist: Option<usize>,
    pub ivf_nprobe: Option<usize>,
    pub tq_bits: Option<u32>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct SecuritySection {
    pub auth_token_file: Option<String>,
    pub static_bearer_scopes: Option<Vec<String>>,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct OAuthSection {
    pub public_url: Option<String>,
    pub issuer: Option<String>,
    pub client_id: Option<String>,
    pub client_secret_file: Option<String>,
    pub principals_file: Option<String>,
    pub cimd_allowed_domains: Option<Vec<String>>,
    pub trust_forwarded_proto: Option<bool>,
}

/// Settings for the embedding worker. They apply only to a build carrying the
/// `indexer` Cargo feature; on any other build the server warns and ignores
/// them, so one file can serve several deployments.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct IndexerSection {
    pub ollama_url: Option<String>,
    pub openai_url: Option<String>,
    pub openai_api_key_file: Option<String>,
}

impl IndexerSection {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

impl FileConfig {
    /// Reads and parses the file. A missing file is an error: the operator
    /// named it, so silence would hide a typo in the path.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            MCSError::InvalidParams(format!(
                "cannot read config file '{}': {error}",
                path.display()
            ))
        })?;
        Self::parse(&text, path)
    }

    /// The `Display` of a TOML error quotes the offending source line. The
    /// natural mistake here is writing a secret where a path belongs, so the
    /// quote is dropped and only the message and the line number survive.
    fn parse(text: &str, path: &Path) -> Result<Self> {
        toml::from_str(text).map_err(|error| {
            let line = error
                .span()
                .map_or(0, |span| text[..span.start].lines().count());
            MCSError::InvalidParams(format!(
                "invalid config file '{}' at line {line}: {}",
                path.display(),
                error.message()
            ))
        })
    }

    /// Resolves the file path from `--config`, then from `MCP_MEMORY_CONFIG`.
    /// Returns `None` when neither names one; there is no implicit search path,
    /// so a stray file in the working directory can never change a deployment.
    ///
    /// `env` is the raw variable, passed in rather than read here, so a test
    /// needs no process state. It is an `OsString`, so a path that is not UTF-8
    /// still works instead of being silently dropped.
    pub fn resolve_path(
        args: &Args,
        env: Option<std::ffi::OsString>,
    ) -> Option<std::path::PathBuf> {
        args.config
            .clone()
            .map(std::path::PathBuf::from)
            .or_else(|| env.filter(|value| !value.is_empty()).map(Into::into))
    }

    /// Fills every argument the command line left at its default.
    pub fn apply(&self, args: &mut Args, cli: &ExplicitArgs, env: EnvProbe<'_>) -> Result<()> {
        let server = &self.server;
        assign(
            &mut args.memory_file,
            server.memory_file.clone().map(Some),
            cli.absent("memory_file") && env(env_keys::MEMORY_FILE),
        );
        assign(
            &mut args.transport,
            parse_enum::<Transport>("server.transport", server.transport.as_ref())?,
            cli.absent("transport"),
        );
        assign(&mut args.bind, server.bind.clone(), cli.absent("bind"));
        assign(
            &mut args.log_level,
            server.log_level.clone(),
            cli.absent("log_level"),
        );
        assign_vec(&mut args.roles, server.roles.clone(), cli.absent("roles"));
        assign(
            &mut args.legacy_observations,
            server.legacy_observations,
            cli.absent("legacy_observations"),
        );
        assign(
            &mut args.stdio_concurrency,
            server.stdio_concurrency,
            cli.absent("stdio_concurrency"),
        );
        assign(
            &mut args.read_pool_size,
            server.read_pool_size,
            cli.absent("read_pool_size"),
        );

        let storage = &self.storage;
        assign(
            &mut args.durability,
            storage.durability.clone().map(Some),
            cli.absent("durability") && env(env_keys::DURABILITY),
        );
        assign(
            &mut args.mmap_size,
            storage.mmap_size,
            cli.absent("mmap_size"),
        );
        assign(
            &mut args.page_size,
            storage.page_size,
            cli.absent("page_size"),
        );
        assign(
            &mut args.cache_size_mb,
            storage.cache_size_mb,
            cli.absent("cache_size_mb"),
        );
        assign(
            &mut args.busy_timeout_ms,
            storage.busy_timeout_ms,
            cli.absent("busy_timeout_ms"),
        );
        assign(
            &mut args.wal_flush_ms,
            storage.wal_flush_ms,
            cli.absent("wal_flush_ms"),
        );
        assign(
            &mut args.lru_cache_size,
            storage.lru_cache_size,
            cli.absent("lru_cache_size"),
        );

        let tools = &self.tools;
        assign(&mut args.enable_all, tools.all, cli.absent("enable_all"));
        assign(
            &mut args.enable_graph_read,
            tools.graph_read,
            cli.absent("enable_graph_read"),
        );
        assign(
            &mut args.enable_graph_write,
            tools.graph_write,
            cli.absent("enable_graph_write"),
        );
        assign(
            &mut args.enable_vectors,
            tools.vectors,
            cli.absent("enable_vectors"),
        );
        assign(&mut args.enable_code, tools.code, cli.absent("enable_code"));

        let vectors = &self.vectors;
        assign(
            &mut args.embedding_dims,
            vectors.embedding_dims,
            cli.absent("embedding_dims"),
        );
        assign(
            &mut args.code_embedding_dims,
            vectors.code_embedding_dims,
            cli.absent("code_embedding_dims"),
        );
        assign(
            &mut args.vec_index,
            parse_enum::<VecIndex>("vectors.index", vectors.index.as_ref())?,
            cli.absent("vec_index"),
        );
        assign(
            &mut args.vec_metric,
            parse_enum::<VecMetric>("vectors.metric", vectors.metric.as_ref())?,
            cli.absent("vec_metric"),
        );
        assign(
            &mut args.vec_quantization,
            parse_enum::<VecQuant>("vectors.quantization", vectors.quantization.as_ref())?,
            cli.absent("vec_quantization"),
        );
        assign(
            &mut args.vec_connectivity,
            vectors.connectivity,
            cli.absent("vec_connectivity"),
        );
        assign(
            &mut args.vec_expansion_add,
            vectors.expansion_add,
            cli.absent("vec_expansion_add"),
        );
        assign(
            &mut args.vec_expansion_search,
            vectors.expansion_search,
            cli.absent("vec_expansion_search"),
        );
        assign(
            &mut args.ivf_nlist,
            vectors.ivf_nlist,
            cli.absent("ivf_nlist"),
        );
        assign(
            &mut args.ivf_nprobe,
            vectors.ivf_nprobe,
            cli.absent("ivf_nprobe"),
        );
        assign(&mut args.tq_bits, vectors.tq_bits, cli.absent("tq_bits"));

        let security = &self.security;
        assign(
            &mut args.auth_token_file,
            security.auth_token_file.clone().map(Some),
            cli.absent("auth_token_file") && cli.absent("auth_token") && env(env_keys::AUTH_TOKEN),
        );
        // An empty list reads as "grant nothing" and would do the opposite:
        // `Config::from_args` maps an empty scope list to every category. The
        // command line cannot express it, so only the file can reach the trap.
        if security
            .static_bearer_scopes
            .as_ref()
            .is_some_and(Vec::is_empty)
        {
            return Err(MCSError::InvalidParams(
                "config 'security.static-bearer-scopes': an empty list grants every scope; omit the key instead"
                    .into(),
            ));
        }
        assign_vec(
            &mut args.static_bearer_scopes,
            security.static_bearer_scopes.clone(),
            cli.absent("static_bearer_scopes"),
        );
        assign(
            &mut args.tls_cert,
            security.tls_cert.clone().map(Some),
            cli.absent("tls_cert") && env(env_keys::TLS_CERT),
        );
        assign(
            &mut args.tls_key,
            security.tls_key.clone().map(Some),
            cli.absent("tls_key") && env(env_keys::TLS_KEY),
        );

        let oauth = &self.oauth;
        assign(
            &mut args.public_url,
            oauth.public_url.clone().map(Some),
            cli.absent("public_url"),
        );
        assign(
            &mut args.oidc_issuer,
            oauth.issuer.clone().map(Some),
            cli.absent("oidc_issuer"),
        );
        assign(
            &mut args.oidc_client_id,
            oauth.client_id.clone().map(Some),
            cli.absent("oidc_client_id"),
        );
        assign(
            &mut args.oidc_client_secret_file,
            oauth.client_secret_file.clone().map(Some),
            cli.absent("oidc_client_secret_file"),
        );
        assign(
            &mut args.principals_file,
            oauth.principals_file.clone().map(Some),
            cli.absent("principals_file"),
        );
        assign_vec(
            &mut args.cimd_allowed_domains,
            oauth.cimd_allowed_domains.clone(),
            cli.absent("cimd_allowed_domains"),
        );
        assign(
            &mut args.oauth_trust_forwarded_proto,
            oauth.trust_forwarded_proto,
            cli.absent("oauth_trust_forwarded_proto"),
        );

        Ok(())
    }
}

/// Parses the command line, then layers the configuration file under it.
/// `main` and the tests both call this, so no test can pass while the real
/// startup path is broken.
pub fn resolve(
    argv: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<(Args, Option<(std::path::PathBuf, FileConfig)>)> {
    let matches = <Args as clap::CommandFactory>::command().get_matches_from(argv);
    let mut args = <Args as clap::FromArgMatches>::from_arg_matches(&matches)
        .map_err(|error| MCSError::InvalidParams(error.to_string()))?;
    let explicit = ExplicitArgs::from_matches(&matches);
    let file = match FileConfig::resolve_path(&args, std::env::var_os(CONFIG_PATH_ENV)) {
        Some(path) => {
            let file = FileConfig::load(&path)?;
            file.apply(&mut args, &explicit, &env_absent)?;
            Some((path, file))
        }
        None => None,
    };
    Ok((args, file))
}

/// The embedding-provider settings the file carries, layered under the
/// environment. The key file is read only when the environment leaves a hole,
/// so a stale `openai-api-key-file` cannot stop a startup that never needed it.
#[cfg(feature = "indexer")]
pub fn indexer_settings(file: Option<&FileConfig>) -> Result<mcpmem_indexer::ProviderSettings> {
    let environment = mcpmem_indexer::ProviderSettings::from_environment();
    let Some(section) = file.map(|file| &file.indexer) else {
        return Ok(environment);
    };
    if environment.is_complete() {
        return Ok(environment);
    }
    let openai_api_key = match (&environment.openai_api_key, &section.openai_api_key_file) {
        (None, Some(path)) => Some(read_secret_file("indexer.openai-api-key-file", path)?),
        _ => None,
    };
    Ok(environment.or(mcpmem_indexer::ProviderSettings {
        ollama_url: section.ollama_url.clone(),
        openai_url: section.openai_url.clone(),
        openai_api_key,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn rejects_an_unknown_key() {
        let error = toml::from_str::<FileConfig>("[server]\nbindd = \"0.0.0.0:1\"\n")
            .expect_err("an unknown key must not parse");
        assert!(error.to_string().contains("bindd"), "{error}");
    }

    #[test]
    fn rejects_an_unknown_section() {
        let error = toml::from_str::<FileConfig>("[srver]\nbind = \"0.0.0.0:1\"\n")
            .expect_err("an unknown section must not parse");
        assert!(error.to_string().contains("srver"), "{error}");
    }

    #[test]
    fn reports_the_field_on_a_bad_enum_value() {
        let file: FileConfig =
            toml::from_str("[vectors]\nindex = \"quantum\"\n").expect("parses as a string");
        let mut args = Args::parse_from(["mcpmem"]);
        let cli = ExplicitArgs::none();
        let error = file
            .apply(&mut args, &cli, &|_| true)
            .expect_err("an unknown backend must be rejected");
        assert!(error.to_string().contains("vectors.index"), "{error}");
    }

    #[test]
    fn an_empty_secret_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("key");
        std::fs::write(&path, "   \n").expect("write");
        let error = read_secret_file("indexer.openai-api-key-file", path.to_str().expect("utf8"))
            .expect_err("an empty secret file must be rejected");
        assert!(error.to_string().contains("is empty"), "{error}");
    }
}
