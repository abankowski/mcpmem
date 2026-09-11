//! Bounded durable embedding worker. Network embedding happens outside the
//! fenced SQLite completion transaction.
#[cfg(feature = "bedrock")]
mod bedrock;
mod ollama;
mod openai;
mod provider;

#[cfg(feature = "bedrock")]
pub use bedrock::{AwsBedrockTransport, BedrockEmbeddingProvider, BedrockTransport};
pub use ollama::OllamaProvider;
pub use openai::OpenAiCompatibleProvider;
pub use provider::{CanonicalDocument, EmbeddingProvider, ProviderError};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mcpmem_core::jobs::{IndexJobRepository, IndexOperation, IndexProfileRegistry};
use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;

const LEASE_US: i64 = 30_000_000;

#[derive(Debug, Default, Eq, PartialEq)]
pub struct RunReport {
    pub claimed: usize,
    pub committed: usize,
    pub retried: usize,
}

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("core error: {0}")]
    Core(#[from] mcpmem_core::errors::MCSError),
}

pub struct IndexerWorker<P> {
    database: PathBuf,
    provider: P,
    timeout: Duration,
    lease_us: i64,
}

/// Strict provider dispatch from the immutable profile contract. Unsupported
/// kinds fail the job; they are never silently sent to another provider.
pub struct ProviderRegistry {
    ollama: Option<Arc<OllamaProvider>>,
    openai: Option<Arc<OpenAiCompatibleProvider>>,
    #[cfg(feature = "bedrock")]
    bedrock: Option<Arc<BedrockEmbeddingProvider>>,
}

impl ProviderRegistry {
    pub const fn new(
        ollama: Option<Arc<OllamaProvider>>,
        openai: Option<Arc<OpenAiCompatibleProvider>>,
    ) -> Self {
        Self {
            ollama,
            openai,
            #[cfg(feature = "bedrock")]
            bedrock: None,
        }
    }

    #[cfg(feature = "bedrock")]
    pub fn with_bedrock(mut self, bedrock: Arc<BedrockEmbeddingProvider>) -> Self {
        self.bedrock = Some(bedrock);
        self
    }

    /// Reads the three provider variables from the process environment.
    pub fn from_environment(timeout: Duration) -> Result<Self, ProviderError> {
        Self::from_settings(&ProviderSettings::from_environment(), timeout)
    }

    /// Builds the registry from settings the caller resolved. A server that
    /// merges a configuration file with the environment uses this entry point,
    /// so no code has to write back into the process environment.
    pub fn from_settings(
        settings: &ProviderSettings,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let ollama = settings
            .ollama_url
            .as_deref()
            .map(|url| OllamaProvider::new(url, timeout))
            .transpose()?
            .map(Arc::new);
        let openai = match (
            settings.openai_url.as_deref(),
            settings.openai_api_key.as_deref(),
        ) {
            (Some(url), Some(key)) => Some(Arc::new(OpenAiCompatibleProvider::new(
                url.to_string(),
                key.to_string(),
                timeout,
            )?)),
            (None, None) => None,
            _ => {
                return Err(ProviderError::Request(
                    "OpenAI-compatible URL and API key must be configured together".into(),
                ));
            }
        };
        let registry = Self::new(ollama, openai);
        // Bedrock resolves the AWS credential chain when it is constructed, and
        // that fails on a host with no AWS credentials. Building it
        // unconditionally therefore broke an Ollama-only deployment that merely
        // happened to compile the `bedrock` feature. It is built only when the
        // caller asks for it.
        #[cfg(feature = "bedrock")]
        let registry = if settings.bedrock {
            registry.with_bedrock(Arc::new(BedrockEmbeddingProvider::from_standard_chain(
                timeout,
            )?))
        } else {
            registry
        };
        Ok(registry)
    }
}

impl ProviderRegistry {
    /// Whether this registry could serve a profile of the given provider kind.
    /// The strict kind mapping lives here, where the provider fields are, so
    /// no caller outside the crate re-derives which kind answers to which
    /// provider.
    #[must_use]
    pub fn supports_provider_kind(&self, kind: &str) -> bool {
        match kind {
            "ollama" => self.ollama.is_some(),
            "openai" | "openai-compatible" => self.openai.is_some(),
            #[cfg(feature = "bedrock")]
            "bedrock" => self.bedrock.is_some(),
            _ => false,
        }
    }
}

/// Environment variable holding the Ollama base URL.
pub const OLLAMA_URL_ENV: &str = "MCP_MEMORY_OLLAMA_URL";
/// Environment variable holding the OpenAI-compatible embeddings endpoint.
pub const OPENAI_URL_ENV: &str = "MCP_MEMORY_OPENAI_URL";
/// Environment variable holding the key for that endpoint.
pub const OPENAI_API_KEY_ENV: &str = "MCP_MEMORY_OPENAI_API_KEY";

/// Which embedding services the worker may call. Every field is optional: an
/// absent field means the matching provider kind is not configured, and a job
/// that names it fails rather than reaching another provider.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProviderSettings {
    pub ollama_url: Option<String>,
    pub openai_url: Option<String>,
    pub openai_api_key: Option<String>,
    /// Whether the deployment wants the Bedrock provider. It names no URL and
    /// no key: it reads the standard AWS chain instead. So the caller states
    /// the intent here, and the profile's `provider_kind` is what sets it.
    pub bedrock: bool,
}

/// Hand-written so the key can never reach a log line through `{:?}`.
impl std::fmt::Debug for ProviderSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSettings")
            .field("ollama_url", &self.ollama_url)
            .field("openai_url", &self.openai_url)
            .field(
                "openai_api_key",
                &self.openai_api_key.as_ref().map(|_| "<redacted>"),
            )
            .field("bedrock", &self.bedrock)
            .finish()
    }
}

impl ProviderSettings {
    /// An empty variable counts as unset. A deployment that expands an unset
    /// shell variable writes an empty string, and treating that as a
    /// configured provider would hide the real setting behind it.
    pub fn from_environment() -> Self {
        let read = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
        Self {
            ollama_url: read(OLLAMA_URL_ENV),
            openai_url: read(OPENAI_URL_ENV),
            openai_api_key: read(OPENAI_API_KEY_ENV),
            // No environment variable selects Bedrock. The index profile names
            // the provider kind, so the server sets this from the profile.
            bedrock: false,
        }
    }

    /// True when the environment already answered every field, so no
    /// lower-priority source has to be read at all.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.ollama_url.is_some() && self.openai_url.is_some() && self.openai_api_key.is_some()
    }

    /// Fills only the fields this value leaves empty. The receiver is the
    /// higher-priority source, so the environment keeps precedence over a
    /// configuration file.
    #[must_use]
    pub fn or(mut self, lower: Self) -> Self {
        self.ollama_url = self.ollama_url.or(lower.ollama_url);
        self.openai_url = self.openai_url.or(lower.openai_url);
        self.openai_api_key = self.openai_api_key.or(lower.openai_api_key);
        self.bedrock = self.bedrock || lower.bedrock;
        self
    }
}

impl EmbeddingProvider for ProviderRegistry {
    /// Strict dispatch on the profile's provider kind. An unknown kind fails
    /// the call; it is never sent to another provider. Implementing the text
    /// primitive is enough: the trait's `embed` reduces documents to text and
    /// arrives here.
    fn embed_texts(
        &self,
        profile: &mcpmem_core::jobs::IndexProfile,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        match profile.provider_kind.as_str() {
            "ollama" => self
                .ollama
                .as_ref()
                .ok_or_else(|| ProviderError::Request("Ollama provider is not configured".into()))?
                .embed_texts(profile, texts),
            "openai" | "openai-compatible" => self
                .openai
                .as_ref()
                .ok_or_else(|| {
                    ProviderError::Request("OpenAI-compatible provider is not configured".into())
                })?
                .embed_texts(profile, texts),
            #[cfg(feature = "bedrock")]
            "bedrock" => self
                .bedrock
                .as_ref()
                .ok_or_else(|| ProviderError::Request("Bedrock provider is not configured".into()))?
                .embed_texts(profile, texts),
            kind => Err(ProviderError::Request(format!(
                "unsupported embedding provider '{kind}'"
            ))),
        }
    }
}

impl<P: EmbeddingProvider> IndexerWorker<P> {
    pub fn new(database: impl AsRef<Path>, provider: P, timeout: Duration) -> Self {
        Self {
            database: database.as_ref().to_path_buf(),
            provider,
            timeout,
            lease_us: LEASE_US,
        }
    }

    pub const fn with_lease_us(mut self, lease_us: i64) -> Self {
        self.lease_us = lease_us;
        self
    }

    pub fn run_once(&self, now_us: i64) -> Result<RunReport, WorkerError> {
        let conn = Connection::open(&self.database)?;
        conn.busy_timeout(self.timeout)?;
        mcpmem_core::schema::initialize_database(&conn)?;
        let jobs = IndexJobRepository::new(&conn);
        let Some(job) = jobs.claim_due(now_us, self.lease_us)? else {
            return Ok(RunReport::default());
        };
        let mut report = RunReport {
            claimed: 1,
            ..RunReport::default()
        };
        let profile = IndexProfileRegistry::new(&conn).get(job.profile_id)?;
        // Renew immediately before work and again immediately before the
        // fenced write using a fresh wall clock. An over-lease provider call
        // cannot commit merely because its claim timestamp was old.
        let fresh_before_provider = current_us();
        if !jobs.renew(&job, fresh_before_provider, self.lease_us)? {
            return Ok(report);
        }
        let outcome: Result<bool, String> = match job.operation {
            IndexOperation::Delete => jobs
                .commit_vector(&job, current_us(), None, "indexer")
                .map_err(|error| error.to_string()),
            IndexOperation::Upsert => {
                let document = canonical_document(&conn, job.entity_id, job.entity_revision)?;
                match document {
                    Some(document) => self
                        .provider
                        .embed(&profile, &[document])
                        .map_err(|error| error.to_string())
                        .and_then(|mut vectors| {
                            if vectors.len() != 1 {
                                return Err("provider returned wrong embedding count".into());
                            }
                            let before_commit = current_us();
                            if !jobs
                                .renew(&job, before_commit, self.lease_us)
                                .map_err(|error| error.to_string())?
                            {
                                return Ok(false);
                            }
                            jobs.commit_vector(
                                &job,
                                current_us(),
                                vectors.pop().as_deref(),
                                "indexer",
                            )
                            .map_err(|error| error.to_string())
                        }),
                    None => Ok(false),
                }
            }
        };
        match outcome {
            Ok(true) => report.committed = 1,
            Ok(false) => {}
            Err(error) => {
                let retry_now = current_us();
                let retry_at = retry_now.saturating_add(1_000_000);
                if jobs.retry(&job, retry_now, retry_at, &error, false)? {
                    report.retried = 1;
                }
            }
        }
        Ok(report)
    }
}

fn current_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(i64::MAX as u128) as i64
}

fn canonical_document(
    conn: &Connection,
    entity_id: i64,
    expected_revision: i64,
) -> Result<Option<CanonicalDocument>, rusqlite::Error> {
    let row: Option<(String, String, String, i64)> = conn.query_row(
        "SELECT e.name,t.name,COALESCE((SELECT json_group_array(o.body ORDER BY o.idx) FROM observation o WHERE o.entity_id=e.id),'[]'),r.revision FROM entity e JOIN type_dict t ON t.id=e.type_id JOIN entity_revision r ON r.entity_id=e.id WHERE e.id=?1 AND e.flags=0",
        [entity_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    ).optional()?;
    Ok(row.and_then(|(name, entity_type, observations, revision)| {
        (revision == expected_revision).then(|| CanonicalDocument {
            entity_id,
            revision,
            name,
            entity_type,
            observations: serde_json::from_str(&observations).unwrap_or_default(),
        })
    }))
}
