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

use mcpmem_core::jobs::{
    IndexJobRepository, IndexOperation, IndexProfile, IndexProfileRegistry, Normalization,
    TaxonomyJobRepository,
};
use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;

const LEASE_US: i64 = 30_000_000;
/// Failures allowed before a job is dead-lettered. Mirrors the webhook
/// worker's bound; a poisoned entity must not occupy the queue forever.
const MAX_ATTEMPTS: i64 = 8;

#[derive(Debug, Default, Eq, PartialEq)]
pub struct RunReport {
    pub claimed: usize,
    pub committed: usize,
    pub retried: usize,
    pub dead: usize,
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
            // No entity job is due. Serve a taxonomy job next, mirroring the
            // entity flow.
            return self.run_taxonomy_job(&conn, now_us);
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
                    Some(document) => self.embed_and_commit(
                        &profile,
                        document,
                        || {
                            jobs.renew(&job, current_us(), self.lease_us)
                                .map_err(|error| error.to_string())
                        },
                        |vector| {
                            jobs.commit_vector(&job, current_us(), Some(&vector), "indexer")
                                .map_err(|error| error.to_string())
                        },
                    ),
                    // The entity vanished or was superseded before this claim
                    // ran. Retrying the same text can never succeed, so it must
                    // not loop as a leased job forever: route it through the
                    // retry path, which bounds it and dead-letters.
                    None => Err(
                        "entity vanished or was superseded before embedding; nothing to index"
                            .into(),
                    ),
                }
            }
        };
        match outcome {
            Ok(true) => report.committed = 1,
            Ok(false) => {}
            Err(error) => {
                let retry_now = current_us();
                let retry_at = retry_now.saturating_add(1_000_000);
                let dead = job.attempts >= MAX_ATTEMPTS;
                if jobs.retry(&job, retry_now, retry_at, &error, dead)? {
                    if dead {
                        report.dead = 1;
                        tracing::error!(
                            entity_id = job.entity_id,
                            profile_id = %job.profile_id,
                            attempts = job.attempts,
                            %error,
                            "index job dead-lettered after max attempts; it will not block the store"
                        );
                    } else {
                        report.retried = 1;
                        tracing::warn!(
                            entity_id = job.entity_id,
                            profile_id = %job.profile_id,
                            attempts = job.attempts,
                            %error,
                            "index job embedding failed; scheduled for retry"
                        );
                    }
                }
            }
        }
        Ok(report)
    }

    /// Embed one document, then commit its vector under one lease renewal,
    /// mirroring the pre-embed and pre-commit renewals of the entity flow.
    /// Both job paths share this code so the lease and normalization gates
    /// cannot diverge.
    fn embed_and_commit<Renew, Commit>(
        &self,
        profile: &IndexProfile,
        document: CanonicalDocument,
        mut renew: Renew,
        commit: Commit,
    ) -> Result<bool, String>
    where
        Renew: FnMut() -> Result<bool, String>,
        Commit: FnOnce(Vec<f32>) -> Result<bool, String>,
    {
        self.provider
            .embed(profile, &[document])
            .map_err(|error| error.to_string())
            .and_then(|mut vectors| {
                if vectors.len() != 1 {
                    return Err("provider returned wrong embedding count".into());
                }
                let mut vector = vectors.pop().expect("len checked above");
                // The profile's L2 contract is a promise the worker
                // makes before storing: OpenAI returns vectors that
                // are only *roughly* unit-norm (measured off by up
                // to 5e-4), and the stored-vector validation demands
                // |norm-1| < 1e-4. Normalize here so a provider's
                // approximation cannot fail the gate. A zero vector
                // cannot be normalized and will be rejected by the
                // stored-vector validation.
                if profile.normalization == Normalization::L2 {
                    let norm: f64 = vector
                        .iter()
                        .map(|value| f64::from(*value).powi(2))
                        .sum::<f64>()
                        .sqrt();
                    if norm > 0.0 {
                        for value in &mut vector {
                            *value = (f64::from(*value) / norm) as f32;
                        }
                    }
                }
                if !renew()? {
                    return Ok(false);
                }
                commit(vector)
            })
    }

    /// Serve the next due taxonomy job, mirroring the entity flow. Runs only
    /// when no entity job is due, so one poll serves exactly one job.
    fn run_taxonomy_job(&self, conn: &Connection, now_us: i64) -> Result<RunReport, WorkerError> {
        let tax_jobs = TaxonomyJobRepository::new(conn);
        let Some(tax_job) = tax_jobs.claim_due(now_us, self.lease_us)? else {
            return Ok(RunReport::default());
        };
        let mut report = RunReport {
            claimed: 1,
            ..RunReport::default()
        };
        let profile = IndexProfileRegistry::new(conn).get(tax_job.profile_id)?;
        // Renew immediately before work and again immediately before the
        // fenced write using a fresh wall clock. An over-lease provider call
        // cannot commit merely because its claim timestamp was old.
        let fresh_before_provider = current_us();
        if !tax_jobs.renew(&tax_job, fresh_before_provider, self.lease_us)? {
            return Ok(report);
        }
        let outcome: Result<bool, String> = match tax_job.operation {
            IndexOperation::Delete => tax_jobs
                .commit_vector(&tax_job, current_us(), None, "indexer")
                .map_err(|error| error.to_string()),
            IndexOperation::Upsert => {
                let document = taxonomy_document(
                    conn,
                    tax_job.subject_kind,
                    tax_job.subject_id,
                    tax_job.subject_revision,
                )?;
                match document {
                    Some(document) => self.embed_and_commit(
                        &profile,
                        document,
                        || {
                            tax_jobs
                                .renew(&tax_job, current_us(), self.lease_us)
                                .map_err(|error| error.to_string())
                        },
                        |vector| {
                            tax_jobs
                                .commit_vector(&tax_job, current_us(), Some(&vector), "indexer")
                                .map_err(|error| error.to_string())
                        },
                    ),
                    // The subject vanished or was superseded before this claim
                    // ran. Route it through the retry path, which bounds it
                    // and dead-letters, as for a vanished entity.
                    None => Err(
                        "taxonomy subject vanished or was superseded before embedding; nothing to index"
                            .into(),
                    ),
                }
            }
        };
        match outcome {
            Ok(true) => report.committed = 1,
            Ok(false) => {}
            Err(error) => {
                let retry_now = current_us();
                let retry_at = retry_now.saturating_add(1_000_000);
                let dead = tax_job.attempts >= MAX_ATTEMPTS;
                if tax_jobs.retry(&tax_job, retry_now, retry_at, &error, dead)? {
                    if dead {
                        report.dead = 1;
                        tracing::error!(
                            subject_kind = tax_job.subject_kind,
                            subject_id = tax_job.subject_id,
                            profile_id = %tax_job.profile_id,
                            attempts = tax_job.attempts,
                            %error,
                            "taxonomy job dead-lettered after max attempts; it will not block the store"
                        );
                    } else {
                        report.retried = 1;
                        tracing::warn!(
                            subject_kind = tax_job.subject_kind,
                            subject_id = tax_job.subject_id,
                            profile_id = %tax_job.profile_id,
                            attempts = tax_job.attempts,
                            %error,
                            "taxonomy job embedding failed; scheduled for retry"
                        );
                    }
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

/// Build the canonical document for a taxonomy subject, in the fencing style
/// of [`canonical_document`].
///
/// `kind` is 0 for an entity type, 1 for a relation type and 2 for a relation
/// instance. A `None` return means the subject is missing or superseded, or
/// deleted for a relation instance. The worker routes that through the retry
/// path.
pub fn taxonomy_document(
    conn: &Connection,
    kind: i64,
    subject_id: i64,
    expected_revision: i64,
) -> Result<Option<CanonicalDocument>, rusqlite::Error> {
    match kind {
        0 => entity_type_document(conn, subject_id, expected_revision),
        1 => relation_type_document(conn, subject_id, expected_revision),
        2 => relation_instance_document(conn, subject_id, expected_revision),
        _ => Ok(None),
    }
}

/// Fence one `type_dict` row of `kind` on its `revision`.
fn fenced_type_name(
    conn: &Connection,
    kind: i64,
    subject_id: i64,
    expected_revision: i64,
) -> Result<Option<(String, i64)>, rusqlite::Error> {
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT name, revision FROM type_dict WHERE kind=?1 AND id=?2",
            [kind, subject_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((name, revision)) = row else {
        return Ok(None);
    };
    if revision != expected_revision {
        return Ok(None);
    }
    Ok(Some((name, revision)))
}

/// Kind 0: one entity type, its `entityType` name and its members.
fn entity_type_document(
    conn: &Connection,
    subject_id: i64,
    expected_revision: i64,
) -> Result<Option<CanonicalDocument>, rusqlite::Error> {
    let Some((name, revision)) = fenced_type_name(conn, 0, subject_id, expected_revision)? else {
        return Ok(None);
    };
    let mut stmt =
        conn.prepare("SELECT name FROM entity WHERE type_id=?1 AND flags=0 ORDER BY id LIMIT 128")?;
    let members: Vec<String> = stmt
        .query_map([subject_id], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(Some(CanonicalDocument {
        entity_id: subject_id,
        revision,
        name: format!("entityType: {}", name),
        entity_type: "".to_string(),
        observations: members,
    }))
}

/// Kind 1: one relation type, its `relationType` name and its distinct
/// endpoint-type triples.
fn relation_type_document(
    conn: &Connection,
    subject_id: i64,
    expected_revision: i64,
) -> Result<Option<CanonicalDocument>, rusqlite::Error> {
    let Some((name, revision)) = fenced_type_name(conn, 1, subject_id, expected_revision)? else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT DISTINCT ft.name, ty.name, tt.name
         FROM relation r
         JOIN entity f ON f.id = r.from_id
         JOIN type_dict ft ON ft.id = f.type_id
         JOIN entity t ON t.id = r.to_id
         JOIN type_dict tt ON tt.id = t.type_id
         JOIN type_dict ty ON ty.id = r.type_id
         WHERE r.type_id = ?1 AND f.flags = 0 AND t.flags = 0
         ORDER BY ft.name, tt.name
         LIMIT 128",
    )?;
    let triples: Vec<String> = stmt
        .query_map([subject_id], |r| {
            let from_type: String = r.get(0)?;
            let relation_type: String = r.get(1)?;
            let to_type: String = r.get(2)?;
            Ok(format!("{} {} {}", from_type, relation_type, to_type))
        })?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(Some(CanonicalDocument {
        entity_id: subject_id,
        revision,
        name: format!("relationType: {}", name),
        entity_type: "".to_string(),
        observations: triples,
    }))
}

/// Kind 2: one relation instance, fenced on its `taxonomy_relation` mirror
/// row. The document mirrors the entity shape: the formatted triple is the
/// name and its single observation line.
fn relation_instance_document(
    conn: &Connection,
    subject_id: i64,
    expected_revision: i64,
) -> Result<Option<CanonicalDocument>, rusqlite::Error> {
    let row: Option<(i64, i64)> = conn
        .query_row(
            "SELECT revision, deleted FROM taxonomy_relation WHERE id=?1",
            [subject_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((revision, deleted)) = row else {
        return Ok(None);
    };
    if deleted != 0 || revision != expected_revision {
        return Ok(None);
    }
    let row: Option<(String, String, String, String, String)> = conn
        .query_row(
            "SELECT f.name, ft.name, ty.name, t.name, tt.name
             FROM taxonomy_relation r
             JOIN entity f ON f.id = r.from_id
             JOIN type_dict ft ON ft.id = f.type_id
             JOIN entity t ON t.id = r.to_id
             JOIN type_dict tt ON tt.id = t.type_id
             JOIN type_dict ty ON ty.id = r.type_id
             WHERE r.id = ?1 AND f.flags = 0 AND t.flags = 0
             ORDER BY f.id, t.id
             LIMIT 1",
            [subject_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((from_name, from_type, relation_type, to_name, to_type)) = row else {
        return Ok(None);
    };
    let line = format!(
        "{} ({}) -[{}]-> {} ({})",
        from_name, from_type, relation_type, to_name, to_type,
    );
    Ok(Some(CanonicalDocument {
        entity_id: subject_id,
        revision,
        name: line.clone(),
        entity_type: "".to_string(),
        observations: vec![line],
    }))
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    /// One in-memory database with every migration applied.
    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        mcpmem_core::schema::initialize_database(&conn).unwrap();
        conn
    }

    fn seed_type(conn: &Connection, id: i64, kind: i64, name: &str, revision: i64) {
        conn.execute(
            "INSERT INTO type_dict(id, kind, name, revision) VALUES(?1, ?2, ?3, ?4)",
            params![id, kind, name, revision],
        )
        .unwrap();
    }

    fn seed_entity(conn: &Connection, id: i64, name: &str, type_id: i64) {
        conn.execute(
            "INSERT INTO entity(id, name_hash, name, type_id, created_us, updated_us) \
             VALUES(?1, 0, ?2, ?3, 1, 1)",
            params![id, name, type_id],
        )
        .unwrap();
    }

    fn seed_relation(conn: &Connection, from_id: i64, to_id: i64, type_id: i64) {
        conn.execute(
            "INSERT INTO relation(from_id, to_id, type_id, created_us) VALUES(?1, ?2, ?3, 1)",
            params![from_id, to_id, type_id],
        )
        .unwrap();
    }

    fn seed_taxonomy_relation(
        conn: &Connection,
        id: i64,
        from_id: i64,
        to_id: i64,
        type_id: i64,
        revision: i64,
        deleted: i64,
    ) {
        conn.execute(
            "INSERT INTO taxonomy_relation(id, from_id, to_id, type_id, revision, deleted) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, from_id, to_id, type_id, revision, deleted],
        )
        .unwrap();
    }

    #[test]
    fn entity_type_document_lists_its_members_in_id_order() {
        let conn = test_db();
        seed_type(&conn, 1, 0, "person", 7);
        seed_entity(&conn, 1, "ada", 1);
        seed_entity(&conn, 2, "grace", 1);
        seed_entity(&conn, 3, "alice", 1);
        let doc = taxonomy_document(&conn, 0, 1, 7)
            .unwrap()
            .expect("the seeded type must produce a document");
        assert_eq!(doc.entity_id, 1);
        assert_eq!(doc.revision, 7);
        assert_eq!(doc.name, "entityType: person");
        assert_eq!(doc.entity_type, "");
        assert_eq!(
            doc.observations,
            vec!["ada".to_string(), "grace".to_string(), "alice".to_string()]
        );
        assert_eq!(doc.text(), "entityType: person\n\nada\ngrace\nalice");
    }

    #[test]
    fn relation_type_document_lists_distinct_endpoint_type_triples() {
        let conn = test_db();
        seed_type(&conn, 1, 0, "person", 0);
        seed_type(&conn, 2, 0, "company", 0);
        seed_type(&conn, 3, 1, "works_at", 9);
        seed_entity(&conn, 1, "ada", 1);
        seed_entity(&conn, 2, "grace", 1);
        seed_entity(&conn, 3, "acme", 2);
        seed_entity(&conn, 4, "globex", 2);
        seed_relation(&conn, 1, 3, 3);
        seed_relation(&conn, 2, 3, 3);
        seed_relation(&conn, 4, 1, 3);
        let doc = taxonomy_document(&conn, 1, 3, 9)
            .unwrap()
            .expect("the seeded relation type must produce a document");
        assert_eq!(doc.name, "relationType: works_at");
        assert_eq!(doc.entity_type, "");
        assert_eq!(
            doc.observations,
            vec![
                "company works_at person".to_string(),
                "person works_at company".to_string(),
            ]
        );
        assert_eq!(
            doc.text(),
            "relationType: works_at\n\ncompany works_at person\nperson works_at company"
        );
    }

    #[test]
    fn relation_instance_document_formats_the_triple() {
        let conn = test_db();
        seed_type(&conn, 1, 0, "person", 0);
        seed_type(&conn, 2, 0, "company", 0);
        seed_type(&conn, 3, 1, "works_at", 0);
        seed_entity(&conn, 1, "ada lovelace", 1);
        seed_entity(&conn, 2, "acme ltd", 2);
        seed_taxonomy_relation(&conn, 1, 1, 2, 3, 5, 0);
        let doc = taxonomy_document(&conn, 2, 1, 5)
            .unwrap()
            .expect("the seeded relation must produce a document");
        assert_eq!(doc.entity_id, 1);
        assert_eq!(doc.revision, 5);
        assert_eq!(
            doc.name,
            "ada lovelace (person) -[works_at]-> acme ltd (company)"
        );
        assert_eq!(doc.entity_type, "");
        assert_eq!(
            doc.observations,
            vec!["ada lovelace (person) -[works_at]-> acme ltd (company)".to_string()]
        );
        assert_eq!(
            doc.text(),
            "ada lovelace (person) -[works_at]-> acme ltd (company)\n\n\
ada lovelace (person) -[works_at]-> acme ltd (company)"
        );
    }

    #[test]
    fn revision_mismatch_returns_none_for_every_kind() {
        let conn = test_db();
        seed_type(&conn, 1, 0, "person", 7);
        seed_type(&conn, 2, 0, "company", 0);
        seed_type(&conn, 3, 1, "works_at", 9);
        seed_entity(&conn, 1, "ada", 1);
        seed_entity(&conn, 2, "acme", 2);
        seed_taxonomy_relation(&conn, 1, 1, 2, 3, 5, 0);
        assert!(taxonomy_document(&conn, 0, 1, 8).unwrap().is_none());
        assert!(taxonomy_document(&conn, 1, 3, 1).unwrap().is_none());
        assert!(taxonomy_document(&conn, 2, 1, 6).unwrap().is_none());
    }

    #[test]
    fn deleted_taxonomy_relation_returns_none() {
        let conn = test_db();
        seed_type(&conn, 1, 0, "person", 0);
        seed_type(&conn, 2, 0, "company", 0);
        seed_type(&conn, 3, 1, "works_at", 0);
        seed_entity(&conn, 1, "ada", 1);
        seed_entity(&conn, 2, "acme", 2);
        seed_taxonomy_relation(&conn, 1, 1, 2, 3, 5, 1);
        assert!(taxonomy_document(&conn, 2, 1, 5).unwrap().is_none());
    }

    #[test]
    fn missing_subject_returns_none() {
        let conn = test_db();
        assert!(taxonomy_document(&conn, 0, 99, 1).unwrap().is_none());
        assert!(taxonomy_document(&conn, 1, 99, 1).unwrap().is_none());
        assert!(taxonomy_document(&conn, 2, 99, 1).unwrap().is_none());
    }

    #[test]
    fn entity_type_document_caps_members_at_128() {
        let conn = test_db();
        seed_type(&conn, 1, 0, "person", 1);
        for id in 1..=129 {
            seed_entity(&conn, id, &format!("member{}", id), 1);
        }
        let doc = taxonomy_document(&conn, 0, 1, 1)
            .unwrap()
            .expect("the seeded type must produce a document");
        assert_eq!(doc.observations.len(), 128);
        assert_eq!(doc.observations[0], "member1");
        assert_eq!(doc.observations[127], "member128");
    }

    #[test]
    fn relation_type_document_caps_distinct_triples_at_128() {
        let conn = test_db();
        seed_type(&conn, 130, 0, "target_type", 0);
        seed_type(&conn, 131, 1, "relates_to", 1);
        seed_entity(&conn, 200, "target", 130);
        for id in 1..=129 {
            seed_type(&conn, id, 0, &format!("from_type_{}", id), 0);
            seed_entity(&conn, id, &format!("from_{}", id), id);
            seed_relation(&conn, id, 200, 131);
        }
        let doc = taxonomy_document(&conn, 1, 131, 1)
            .unwrap()
            .expect("the seeded relation type must produce a document");
        assert_eq!(doc.observations.len(), 128);
        assert!(
            doc.observations
                .iter()
                .all(|line| line.ends_with(" relates_to target_type"))
        );
    }
}
