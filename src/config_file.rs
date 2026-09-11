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

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use clap::parser::ValueSource;
use clap::{ArgMatches, ValueEnum};
use mcpmem_core::jobs::{DistanceMetric, Normalization};
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
    #[serde(default)]
    pub webhooks: WebhooksSection,
}

/// HTTP delivery configuration for the `webhooks` role. An empty section is
/// the fail-closed default: no hostname may be subscribed and no signing key
/// exists, so the worker refuses every delivery.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct WebhooksSection {
    pub allowlist: Option<Vec<String>>,
    pub secrets: Option<BTreeMap<String, String>>,
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
///
/// `provider`, `model` and `dimensions` define the index profile, and they
/// travel together. [`profile_spec`] refuses a section that names some of the
/// three and not all of them. A half-written profile would still start a
/// rebuild of every entity.
///
/// `normalization` and `metric` describe the vector the provider returns.
/// Each one has a default, so an operator may leave it out.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct IndexerSection {
    pub ollama_url: Option<String>,
    pub openai_url: Option<String>,
    pub openai_api_key_file: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub dimensions: Option<u32>,
    pub normalization: Option<String>,
    pub metric: Option<String>,
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
    // Bedrock names no URL and no key: it reads the standard AWS chain. So the
    // profile's provider kind is the only thing that can select it, and the
    // registry builds it only when this flag is set. Without the flag an
    // Ollama deployment on a `bedrock` build would fail to construct a
    // registry at all, because the AWS chain resolves on construction.
    let bedrock = profile_spec(file)?.is_some_and(|spec| spec.provider_kind == "bedrock");
    // The key file is read only when a profile actually selects OpenAI, or
    // when the environment already points at OpenAI without a key. A stale or
    // missing `openai-api-key-file` next to an Ollama deployment is a stray
    // path, not a reason to abort a process that never needed the file.
    let openai_selected = profile_spec(file)?
        .is_some_and(|spec| matches!(spec.provider_kind.as_str(), "openai" | "openai-compatible"));
    let openai_api_key = match (
        &environment.openai_api_key,
        &section.openai_api_key_file,
        openai_selected,
    ) {
        (None, Some(path), true) => Some(read_secret_file("indexer.openai-api-key-file", path)?),
        _ => None,
    };
    Ok(environment.or(mcpmem_indexer::ProviderSettings {
        ollama_url: section.ollama_url.clone(),
        openai_url: section.openai_url.clone(),
        openai_api_key,
        bedrock,
    }))
}

/// The [`StaticSecretProvider`] the [`WebhookConfigFile`] implies. The worker
/// needs one concrete provider, and the config file layer owns the map, so
/// constructing it here keeps the crate's provider construction honest.
#[cfg(any(feature = "webhooks", test))]
pub fn static_secret_provider(
    cfg: &mcpmem_webhook::WebhookConfigFile,
) -> mcpmem_webhook::StaticSecretProvider {
    mcpmem_webhook::StaticSecretProvider(cfg.secrets.clone())
}

/// The delivery policy the `[webhooks]` section names. The controller makes
/// this exact call, so the worker constructor and this function never drift
/// apart.
///
/// `allowlist` is the set of HTTPS hostnames the worker may deliver to.
/// `secrets` maps a `secret_ref` to the file that holds its signing key. The
/// keys are read here, once at startup, and an empty file or a missing file
/// stops the process: a webhook signed with an empty key would be forgeable,
/// and a missing key would dead-letter every delivery later, invisibly.
#[cfg(any(feature = "webhooks", test))]
pub fn webhook_worker_config(
    file: Option<&FileConfig>,
) -> Result<mcpmem_webhook::WebhookConfigFile> {
    let Some(section) = file.map(|file| &file.webhooks) else {
        return Ok(mcpmem_webhook::WebhookConfigFile::default());
    };
    let allowlist: BTreeSet<String> = section
        .allowlist
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let secrets = match section.secrets.as_ref() {
        None => BTreeMap::default(),
        Some(secrets) => secrets
            .iter()
            .map(|(reference, path)| {
                let contents = std::fs::read_to_string(path).map_err(|error| {
                    MCSError::InvalidParams(format!(
                        "config 'webhooks.secrets.{reference}': cannot read \
                             '{path}': {error}"
                    ))
                })?;
                let bytes = contents.trim().to_owned().into_bytes();
                if bytes.is_empty() {
                    return Err(MCSError::InvalidParams(format!(
                        "config 'webhooks.secrets.{reference}': '{path}' is empty; \
                         refusing a forgeable signing key"
                    )));
                }
                let key = mcpmem_webhook::SigningKey::new(bytes)
                    .map_err(|error| MCSError::InvalidParams(error.to_string()))?;
                Ok((reference.clone(), key))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
    };
    Ok(mcpmem_webhook::WebhookConfigFile { allowlist, secrets })
}

/// The index profile the `[indexer]` section names, already checked. Only
/// [`profile_spec`] builds one, so every value in it is valid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileSpec {
    pub provider_kind: String,
    pub model: String,
    pub dimensions: u32,
    pub normalization: Normalization,
    pub distance_metric: DistanceMetric,
}

impl ProfileSpec {
    /// Mints the profile. The identifier is fresh on every call, because a
    /// profile is immutable and a new one re-enqueues every entity. The caller
    /// compares fingerprints instead, and a fingerprint excludes the
    /// identifier, so a restart with an unchanged file starts no rebuild.
    #[must_use]
    pub fn to_profile(&self) -> mcpmem_core::jobs::IndexProfile {
        mcpmem_core::jobs::IndexProfile {
            id: uuid::Uuid::new_v4(),
            store_key: "default".to_string(),
            provider_kind: self.provider_kind.clone(),
            model: self.model.clone(),
            dimensions: self.dimensions,
            representation_version: "name+type+observations-v1".to_string(),
            normalization: self.normalization,
            distance_metric: self.distance_metric,
            vector_encoding_version: "f32le-v1".to_string(),
        }
    }
}

/// The spellings an operator writes, and the kind the profile carries. The
/// second column is what `ProviderRegistry::embed` dispatches on, so this table
/// is the whole mapping.
const PROVIDERS: &[(&str, &str)] = &[
    ("ollama", "ollama"),
    ("openai", "openai"),
    ("openai-compatible", "openai-compatible"),
    ("bedrock", "bedrock"),
];

const NORMALIZATIONS: &[(&str, Normalization)] =
    &[("none", Normalization::None), ("l2", Normalization::L2)];

const METRICS: &[(&str, DistanceMetric)] = &[
    ("cosine", DistanceMetric::Cosine),
    ("inner-product", DistanceMetric::InnerProduct),
    ("l2-squared", DistanceMetric::L2Squared),
];

/// The bound `mcpmem_core::jobs::IndexProfile::validate` applies. It is
/// repeated here to name the offending key; the core validator reports only
/// that the whole profile is invalid.
const MAX_DIMENSIONS: u32 = 65_536;

/// The longest name the core validator accepts. Only the model needs the check
/// here, because every other string in a profile comes from a table above.
const MAX_NAME_LEN: usize = 256;

/// A blank value counts as an unset key. An operator who empties a string
/// means the same as an operator who deletes the line.
fn named(raw: Option<&String>) -> Option<&str> {
    raw.map(|raw| raw.trim()).filter(|raw| !raw.is_empty())
}

/// Resolves one string key against its allowed-value table. The message names
/// the field and every allowed value, so the operator needs no second lookup.
fn parse_choice<T: Copy>(
    field: &str,
    value: Option<&str>,
    table: &[(&str, T)],
) -> Result<Option<T>> {
    let Some(value) = value else {
        return Ok(None);
    };
    table
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(value))
        .map(|(_, parsed)| Some(*parsed))
        .ok_or_else(|| {
            let allowed = table
                .iter()
                .map(|(name, _)| format!("'{name}'"))
                .collect::<Vec<_>>()
                .join(", ");
            MCSError::InvalidParams(format!(
                "config '{field}': unknown value '{value}'. Allowed values: {allowed}."
            ))
        })
}

/// The index profile the file names, or `None` when it names none. `None`
/// leaves the store in legacy compatibility, so a client keeps writing its own
/// vectors and the embedding worker stays idle.
///
/// Every rejection here is cheap, and every mistake that reaches the store is
/// not: adopting a profile re-enqueues every live entity and refuses a direct
/// `vector_upsert_embedding` write from that moment on.
pub fn profile_spec(file: Option<&FileConfig>) -> Result<Option<ProfileSpec>> {
    let Some(section) = file.map(|file| &file.indexer) else {
        return Ok(None);
    };

    // Both tables are checked before the early return below, because a
    // misspelled value has no other guard. `deny_unknown_fields` catches a
    // wrong key name and never a wrong value.
    let normalization = parse_choice(
        "indexer.normalization",
        named(section.normalization.as_ref()),
        NORMALIZATIONS,
    )?
    .unwrap_or(Normalization::L2);
    let distance_metric = parse_choice("indexer.metric", named(section.metric.as_ref()), METRICS)?
        .unwrap_or(DistanceMetric::Cosine);
    let provider_kind = parse_choice(
        "indexer.provider",
        named(section.provider.as_ref()),
        PROVIDERS,
    )?;

    let (provider_kind, model, dimensions) = match (
        provider_kind,
        named(section.model.as_ref()),
        section.dimensions,
    ) {
        (Some(provider_kind), Some(model), Some(dimensions)) => (provider_kind, model, dimensions),
        (None, None, None) => return Ok(None),
        (provider, model, dimensions) => {
            let missing = [
                ("indexer.provider", provider.is_none()),
                ("indexer.model", model.is_none()),
                ("indexer.dimensions", dimensions.is_none()),
            ]
            .into_iter()
            .filter(|(_, absent)| *absent)
            .map(|(field, _)| format!("'{field}'"))
            .collect::<Vec<_>>()
            .join(", ");
            return Err(MCSError::InvalidParams(format!(
                "config 'indexer': a profile needs 'provider', 'model' and \
                     'dimensions' together. Add {missing}."
            )));
        }
    };

    if dimensions == 0 || dimensions > MAX_DIMENSIONS {
        return Err(MCSError::InvalidParams(format!(
            "config 'indexer.dimensions': {dimensions} is out of range. \
             Use a value from 1 to {MAX_DIMENSIONS}."
        )));
    }
    // Titan Text Embeddings V2 returns one of three lengths. The provider
    // rejects any other length per request, so a wrong value here would fail
    // every job of a rebuild that already started.
    if provider_kind == "bedrock" && !matches!(dimensions, 256 | 512 | 1024) {
        return Err(MCSError::InvalidParams(format!(
            "config 'indexer.dimensions': the 'bedrock' provider accepts 256, 512 \
             or 1024 only, and not {dimensions}."
        )));
    }
    if model.len() > MAX_NAME_LEN || model.chars().any(char::is_control) {
        return Err(MCSError::InvalidParams(format!(
            "config 'indexer.model': the name must hold no control character, \
             and {MAX_NAME_LEN} bytes at most."
        )));
    }

    Ok(Some(ProfileSpec {
        provider_kind: provider_kind.to_string(),
        model: model.to_string(),
        dimensions,
        normalization,
        distance_metric,
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

    fn indexer_file(body: &str) -> FileConfig {
        toml::from_str(&format!("[indexer]\n{body}")).expect("the section must parse")
    }

    #[test]
    fn a_complete_indexer_section_names_a_profile() {
        let file = indexer_file(
            r#"provider = "ollama"
model = "nomic-embed-text"
dimensions = 768
"#,
        );
        let spec = profile_spec(Some(&file))
            .expect("a complete section must be accepted")
            .expect("a complete section must name a profile");
        assert_eq!(spec.provider_kind, "ollama");
        assert_eq!(spec.model, "nomic-embed-text");
        assert_eq!(spec.dimensions, 768);
        assert_eq!(spec.normalization, Normalization::L2);
        assert_eq!(spec.distance_metric, DistanceMetric::Cosine);
        // The core validator is what `adopt_profile` reaches, and it names no
        // field. Anything it refuses must be refused above instead.
        spec.to_profile()
            .validate()
            .expect("the minted profile must satisfy the core validator");
    }

    #[test]
    fn nothing_named_is_no_profile() {
        assert!(
            profile_spec(None)
                .expect("no file names no profile")
                .is_none(),
            "a deployment with no configuration file must keep legacy compatibility"
        );
        let file = indexer_file("provider = \"\"\nmodel = \"  \"\n");
        assert!(
            profile_spec(Some(&file))
                .expect("a blank value names no profile")
                .is_none(),
            "an empty string is not a name"
        );
    }

    #[test]
    fn a_missing_model_names_the_missing_key() {
        let file = indexer_file("provider = \"ollama\"\ndimensions = 768\n");
        let error = profile_spec(Some(&file)).expect_err("a half-written profile must be rejected");
        assert!(error.to_string().contains("indexer.model"), "{error}");
    }

    #[test]
    fn an_unknown_provider_lists_the_allowed_values() {
        let file = indexer_file("provider = \"llama\"\nmodel = \"m\"\ndimensions = 768\n");
        let error = profile_spec(Some(&file)).expect_err("an unknown provider must be rejected");
        let message = error.to_string();
        assert!(message.contains("indexer.provider"), "{message}");
        assert!(message.contains("'ollama'"), "{message}");
    }

    #[test]
    fn a_dimension_outside_the_range_is_rejected() {
        for body in [
            "provider = \"ollama\"\nmodel = \"m\"\ndimensions = 0\n",
            "provider = \"ollama\"\nmodel = \"m\"\ndimensions = 65537\n",
        ] {
            let file = indexer_file(body);
            let error =
                profile_spec(Some(&file)).expect_err("a length outside the range is refused");
            assert!(error.to_string().contains("indexer.dimensions"), "{error}");
            // The message must carry the whole bound, so the operator needs no
            // second lookup.
            assert!(error.to_string().contains("1 to 65536"), "{error}");
        }
    }

    /// The shape keys have a default, so a test that names none of them cannot
    /// tell a parsed value from a default one.
    #[test]
    fn the_shape_keys_are_read() {
        let file = indexer_file(
            r#"provider = "ollama"
model = "m"
dimensions = 8
normalization = "none"
metric = "inner-product"
"#,
        );
        let spec = profile_spec(Some(&file))
            .expect("both values are allowed")
            .expect("the section names a profile");
        assert_eq!(spec.normalization, Normalization::None);
        assert_eq!(spec.distance_metric, DistanceMetric::InnerProduct);

        // The two spellings the shipped example documents.
        let example = indexer_file(
            r#"provider = "ollama"
model = "m"
dimensions = 8
normalization = "l2"
metric = "cosine"
"#,
        );
        profile_spec(Some(&example)).expect("every documented spelling must be accepted");
    }

    #[test]
    fn bedrock_refuses_a_length_it_never_returns() {
        let file = indexer_file(
            r#"provider = "bedrock"
model = "amazon.titan-embed-text-v2:0"
dimensions = 768
"#,
        );
        let error = profile_spec(Some(&file)).expect_err("768 is not a Bedrock length");
        let message = error.to_string();
        assert!(message.contains("indexer.dimensions"), "{message}");
        assert!(message.contains("bedrock"), "{message}");
        assert!(message.contains("256, 512 or 1024"), "{message}");
        // The discriminating case: the guard refuses the length, and never the
        // provider.
        let allowed = indexer_file(
            r#"provider = "bedrock"
model = "amazon.titan-embed-text-v2:0"
dimensions = 1024
"#,
        );
        profile_spec(Some(&allowed)).expect("1024 is a Bedrock length");
    }
}
