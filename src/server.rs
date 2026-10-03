use serde_json::{Value, json};
use std::num::NonZeroUsize;
use std::path::Path;
#[cfg(feature = "code")]
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::error;

#[cfg(feature = "code")]
use crate::actions::code as code_actions;
use crate::actions::memory;
#[cfg(feature = "webhooks")]
use crate::actions::webhooks as webhooks_actions;
use crate::attachment_actions;
use crate::authz::{self, Principal};
use crate::config::Config;
use crate::errors::{MCSError, Result};
use crate::kg::GraphHandle;
use crate::protocol::{JsonRpcRequest, JsonRpcResponse};
use crate::taxonomy;
use crate::tools;
use crate::vector_actions;
use crate::vector_store::{VectorConfig, VectorStore};
use crate::workspace::{
    HandleSpec, Visibility, WorkspaceAccess, WorkspaceEntry, WorkspaceHandles, WorkspaceRecord,
    WorkspaceRegistry,
};

/// Outcome of processing a request: either a pre-escaped JSON Value (small
/// payloads) or a pre-serialized JSON *string* of the `result` field (avoids
/// a second serialization pass for large payloads such as `read_graph`).
enum HandlerResult {
    Value(Value),
    RawResult(String),
}

// The process has one MCP compatibility mode, fixed at startup. Core and UI
// reads always keep the canonical metadata-bearing model.
static LEGACY_OBSERVATIONS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn legacy_observations() -> bool {
    LEGACY_OBSERVATIONS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Explicit write envelopes keep legacy conversion out of arbitrary JSON data.
fn legacy_observation_input(tool: &str, args: Option<&Value>) -> Result<Option<Value>> {
    let Some(mut args) = args.cloned() else {
        return Ok(None);
    };
    let (envelope, field) = match tool {
        "create_entities" | "upsert_entities" => ("entities", "observations"),
        "add_observations" => ("observations", "contents"),
        "delete_observations" => ("deletions", "observations"),
        // Relation observation arrays land inside the `relations` envelope:
        // appends arrive in `contents`, deletes and create-time observations
        // in `observations`.
        "add_relation_observations" => ("relations", "contents"),
        "delete_relation_observations" | "create_relations" => ("relations", "observations"),
        _ => return Ok(Some(args)),
    };
    let entries = args
        .get_mut(envelope)
        .and_then(Value::as_array_mut)
        .ok_or_else(|| MCSError::InvalidParams(format!("Missing or invalid '{envelope}'")))?;
    for entry in entries {
        let observations = entry
            .get_mut(field)
            .and_then(Value::as_array_mut)
            .ok_or_else(|| MCSError::InvalidParams(format!("Missing or invalid '{field}'")))?;
        for observation in observations {
            let body = observation.as_str().ok_or_else(|| {
                MCSError::InvalidParams("Legacy observations must be strings".into())
            })?;
            *observation = json!({"body": body});
        }
    }
    Ok(Some(args))
}

fn legacy_graph_observations(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(legacy_graph_observations),
        Value::Object(object) => {
            for (key, value) in object {
                if key == "observations" || key == "addedObservations" {
                    if let Value::Array(observations) = value {
                        for observation in observations {
                            if let Some(body) = observation.get("body").and_then(Value::as_str) {
                                *observation = Value::String(body.into());
                            }
                        }
                    }
                } else {
                    legacy_graph_observations(value);
                }
            }
        }
        _ => {}
    }
}

fn legacy_observation_output(result: HandlerResult) -> Result<HandlerResult> {
    // RawResult already contains the MCP content envelope; its text is JSON
    // once more. Adapt the graph there, not the outer JSON-RPC envelope.
    let mut value = match result {
        HandlerResult::Value(value) => value,
        HandlerResult::RawResult(raw) => serde_json::from_str(&raw)?,
    };
    if let Some(contents) = value.get_mut("content").and_then(Value::as_array_mut) {
        for content in contents {
            if let Some(text) = content.get_mut("text")
                && let Some(raw) = text.as_str()
                && let Ok(mut graph) = serde_json::from_str::<Value>(raw)
            {
                legacy_graph_observations(&mut graph);
                *text = Value::String(serde_json::to_string(&graph)?);
            }
        }
    }
    Ok(HandlerResult::Value(value))
}

const BUFFER_CAPACITY: usize = 65536;
const NEWLINE: &[u8] = b"\n";
/// Maximum size of a single inbound JSON-RPC message (shared by all transports).
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

/// Process-wide exposure flags for graph and attachment categories. The graph
/// and attachment handlers consult these flags after the scope gate.
static GRAPH_READ_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static GRAPH_WRITE_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static ATTACHMENTS_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether read-only knowledge-graph access is enabled. Exposed to the HTTP
/// transport so the browser graph viewer (`/ui`) can be gated behind the same
/// `graph-read` permission that governs `read_graph`.
#[inline]
pub(crate) fn graph_read_enabled() -> bool {
    GRAPH_READ_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}
#[inline]
fn graph_write_enabled() -> bool {
    GRAPH_WRITE_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether this process offers attachment tools and attachment search results.
pub(crate) fn attachments_enabled() -> bool {
    ATTACHMENTS_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

enum LineRead {
    Line,
    Eof,
    TooLong,
}

/// Read one newline-terminated line, capping at `max` bytes. Both `buf` (the
/// byte accumulator) and `out` (the decoded line) are caller-owned and reused
/// across calls, so a long-lived connection performs no per-request line
/// allocation — only the decode copy into `out`'s retained capacity.
async fn read_line_capped<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    out: &mut String,
    max: usize,
) -> std::io::Result<LineRead>
where
    R: AsyncBufReadExt + Unpin,
{
    buf.clear();
    out.clear();
    let finish = |buf: &[u8], out: &mut String| -> std::io::Result<LineRead> {
        let s = std::str::from_utf8(buf)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "Non-UTF-8 input"))?;
        out.push_str(s);
        Ok(LineRead::Line)
    };
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(LineRead::Eof);
            }
            return finish(buf, out);
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(i) => {
                if buf.len() + i + 1 > max {
                    reader.consume(i + 1);
                    return Ok(LineRead::TooLong);
                }
                buf.extend_from_slice(&available[..=i]);
                reader.consume(i + 1);
                return finish(buf, out);
            }
            None => {
                let take = available.len();
                if buf.len() + take > max {
                    reader.consume(take);
                    return Ok(LineRead::TooLong);
                }
                buf.extend_from_slice(available);
                reader.consume(take);
            }
        }
    }
}

fn parse_error(msg: String) -> JsonRpcResponse {
    let mcp_error = MCSError::ParseError(msg);
    JsonRpcResponse::error(None, mcp_error.error_code(), mcp_error.to_string())
}

/// Dispatch one framed line (stdio / tcp). Returns the serialized response, or
/// `None` for a notification. The line transport serves the local machine
/// identity, so every graph, vector and webhook call resolves the workspace
/// the same way an authenticated HTTP call does.
pub fn dispatch_line(
    line: &str,
    registry: &WorkspaceRegistry,
    handles: &WorkspaceHandles,
) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Some(serde_json::to_string(&parse_error("Empty request".into())).unwrap());
    }
    let raw: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => return Some(serde_json::to_string(&parse_error(e.to_string())).unwrap()),
    };
    let req: JsonRpcRequest = match serde_json::from_value(raw) {
        Ok(r) => r,
        Err(e) => return Some(serde_json::to_string(&parse_error(e.to_string())).unwrap()),
    };
    req.id.as_ref()?;
    match process_request(&req, &authz::LOCAL_PRINCIPAL, registry, handles) {
        Ok(HandlerResult::Value(result)) => {
            let resp = JsonRpcResponse::success(req.id, result);
            Some(serde_json::to_string(&resp).unwrap())
        }
        Ok(HandlerResult::RawResult(result_json)) => {
            let id_json = serde_json::to_string(&req.id).unwrap();
            let mut out = String::with_capacity(64 + id_json.len() + result_json.len());
            out.push_str(r#"{"jsonrpc":"2.0","id":"#);
            out.push_str(&id_json);
            out.push_str(",\"result\":");
            out.push_str(&result_json);
            out.push('}');
            Some(out)
        }
        Err(e) => {
            let resp = JsonRpcResponse::error(req.id, e.error_code(), e.to_string());
            Some(serde_json::to_string(&resp).unwrap())
        }
    }
}

/// What the HTTP transport should send back. A scope failure is not a JSON-RPC
/// error body: it must become HTTP 403 with a `WWW-Authenticate` header.
#[derive(Debug)]
pub enum HttpOutcome {
    /// Only notifications; send 202 with no content.
    Accepted,
    /// Send this JSON body with 200.
    Body(Value),
    /// Send 403 and name every scope the request needed.
    InsufficientScope(Vec<&'static str>),
}

/// Dispatch a Streamable-HTTP POST body, which may be a single JSON-RPC message
/// or a batch array. [`HttpOutcome::Accepted`] means the body held only
/// notifications (HTTP 202, empty body); `Err` means the body was not valid
/// JSON.
///
/// The whole body is screened against `principal` before anything runs, so a
/// batch that holds one denied call returns
/// [`HttpOutcome::InsufficientScope`] for the *whole* batch — naming the scopes
/// of every denied call, sorted and deduplicated — and applies none of it.
///
/// Every tools/call resolves its own workspace through [`WorkspaceRegistry`]
/// — per call, never per batch — and executes on the handles the
/// [`WorkspaceHandles`] cache returns for that workspace's file.
pub fn dispatch_http_body(
    body: &str,
    principal: &Principal,
    registry: &WorkspaceRegistry,
    handles: &WorkspaceHandles,
) -> std::result::Result<HttpOutcome, String> {
    let value: Value = serde_json::from_str(body.trim()).map_err(|e| e.to_string())?;
    let denied = denied_scopes(&value, principal);
    if !denied.is_empty() {
        return Ok(HttpOutcome::InsufficientScope(denied));
    }
    match value {
        Value::Array(items) => {
            // Batches are rare and never huge — keep Value path for simplicity.
            let responses: Vec<Value> = items
                .into_iter()
                .filter_map(|v| process_value_http(v, principal, registry, handles))
                .collect();
            Ok(if responses.is_empty() {
                HttpOutcome::Accepted
            } else {
                HttpOutcome::Body(Value::Array(responses))
            })
        }
        other => Ok(
            match process_value_http(other, principal, registry, handles) {
                Some(value) => HttpOutcome::Body(value),
                None => HttpOutcome::Accepted,
            },
        ),
    }
}

/// The scopes a body needs but `principal` does not hold, sorted and
/// deduplicated. Screening happens before dispatch so a refused batch never
/// leaves half its calls applied.
fn denied_scopes(value: &Value, principal: &Principal) -> Vec<&'static str> {
    let mut denied: Vec<&'static str> = match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| denied_scope(item, principal))
            .collect(),
        other => denied_scope(other, principal).into_iter().collect(),
    };
    denied.sort_unstable();
    denied.dedup();
    denied
}

/// The scope one JSON-RPC message needs but `principal` lacks. The decision
/// itself belongs to [`authz::missing_scope`], so this screen and the gate in
/// [`handle_tools_call`] can never disagree.
fn denied_scope(value: &Value, principal: &Principal) -> Option<&'static str> {
    // A message with no id is a notification: it never executes, so it can
    // never be a scope failure, and it must not refuse the batch around it.
    // An explicit `"id":null` deserializes to `None` too, so it is one as well.
    value.get("id").filter(|id| !id.is_null())?;
    if value.get("method").and_then(Value::as_str)? != "tools/call" {
        return None;
    }
    let name = value.pointer("/params/name").and_then(Value::as_str)?;
    authz::missing_scope(principal, name)
}

/// Process one JSON-RPC message for the HTTP transport, converting any
/// `RawResult` back into a `Value` (acceptable since HTTP payloads are typically
/// much smaller in this context). `None` means the message was a notification.
fn process_value_http(
    value: Value,
    principal: &Principal,
    registry: &WorkspaceRegistry,
    handles: &WorkspaceHandles,
) -> Option<Value> {
    let req: JsonRpcRequest = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(e) => return Some(to_value(parse_error(e.to_string()))),
    };
    req.id.as_ref()?;
    match process_request(&req, principal, registry, handles) {
        Ok(HandlerResult::Value(result)) => {
            Some(to_value(JsonRpcResponse::success(req.id, result)))
        }
        Ok(HandlerResult::RawResult(result_json)) => {
            // Parse the pre-serialized result back into a Value for HTTP delivery.
            // This is a small extra cost for the HTTP transport; the stdio/TCP
            // path (dispatch_line) avoids it entirely.
            let result_val: Value = serde_json::from_str(&result_json).unwrap_or(Value::Null);
            Some(to_value(JsonRpcResponse::success(req.id, result_val)))
        }
        Err(e) => Some(to_value(JsonRpcResponse::error(
            req.id,
            e.error_code(),
            e.to_string(),
        ))),
    }
}

#[inline]
fn to_value(resp: JsonRpcResponse) -> Value {
    serde_json::to_value(resp).expect("JsonRpcResponse always serializes")
}

pub struct MCPServer {
    config: Arc<Config>,
    registry: Arc<WorkspaceRegistry>,
    kg: Arc<GraphHandle>,
    handles: Arc<WorkspaceHandles>,
    /// `Some` when vector support is enabled (`--vectors`); drives the extra
    /// `vector_*` / `hybrid_search` tools. `None` for a pure knowledge-graph server.
    vs: Option<Arc<VectorStore>>,
}

impl MCPServer {
    /// Build a server. The vector subsystem (usearch index + petgraph mirror) is
    /// only constructed when `config.vectors_enabled` is set; `vec_config` is
    /// ignored otherwise.
    pub fn new(config: Config, vec_config: VectorConfig) -> Result<Self> {
        if config.legacy_observations
            && !config
                .roles
                .roles()
                .contains(&crate::runtime::RuntimeRole::Mcp)
        {
            return Err(MCSError::InvalidParams(
                "--legacy-observations requires the mcp role".into(),
            ));
        }
        LEGACY_OBSERVATIONS.store(
            config.legacy_observations,
            std::sync::atomic::Ordering::Relaxed,
        );
        // Normalize the memory path once, before the registry and the graph
        // open it. The registry stores absolute paths, and the legacy-id
        // lookup below compares the memory path against them; with a relative
        // `-f` path the comparison must not see two different spellings of
        // one file.
        let path = {
            let raw = Path::new(&config.memory_file_path);
            if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                std::env::current_dir()?.join(raw)
            }
        };
        let registry = Arc::new(
            WorkspaceRegistry::open_with_principals(
                &path,
                config.legacy_owner_id.as_deref(),
                config
                    .oauth
                    .as_ref()
                    .map_or(&[][..], |oauth| oauth.principals.as_slice()),
                config.auth_token.is_some(),
            )
            .map_err(|error| MCSError::InvalidParams(error.to_string()))?,
        );
        let lru_cache = NonZeroUsize::new(config.lru_cache_size)
            .unwrap_or_else(|| NonZeroUsize::new(10000).expect("10000 > 0"));
        let kg = Arc::new(GraphHandle::new(
            &path,
            config.durability,
            config.sqlite_tuning(),
            lru_cache,
            config.read_pool_size,
        )?);

        let vs = if config.vectors_enabled {
            let store = Arc::new(VectorStore::with_config(&path, &vec_config)?);
            #[cfg(feature = "indexer")]
            if let Err(error) = store.load_managed_snapshot() {
                tracing::warn!(
                    %error,
                    "legacy vector snapshot not published yet; the indexer will publish it"
                );
            }
            Some(store)
        } else {
            None
        };

        // The legacy workspace is a registered row like any other; its entry
        // in the handle cache is pinned and holds the handles built above, so
        // a no-`workspaceId` call with the legacy default serves the same
        // store the server was built with.
        let legacy_id = registry
            .all_paths()
            .map_err(|error| MCSError::InvalidParams(error.to_string()))?
            .into_iter()
            .find(|(_, graph_path)| graph_path == &path)
            .map(|(id, _)| id)
            .ok_or_else(|| {
                MCSError::InvalidParams("the registry has no legacy workspace".into())
            })?;
        let handles = Arc::new(WorkspaceHandles::new(
            &legacy_id,
            Arc::clone(&kg),
            vs.clone(),
            HandleSpec {
                durability: config.durability,
                tuning: config.sqlite_tuning(),
                lru_cache_size: lru_cache,
                read_pool_size: config.read_pool_size,
                vector_dims: config.vectors_enabled.then_some(vec_config.dims),
            },
        ));

        // Publish the knowledge-graph exposure flags for the dispatch path.
        use crate::tools::ToolCategory;
        GRAPH_READ_ENABLED.store(
            config.enabled_categories.contains(&ToolCategory::GraphRead),
            std::sync::atomic::Ordering::Relaxed,
        );
        GRAPH_WRITE_ENABLED.store(
            config
                .enabled_categories
                .contains(&ToolCategory::GraphWrite),
            std::sync::atomic::Ordering::Relaxed,
        );
        ATTACHMENTS_ENABLED.store(
            config
                .enabled_categories
                .contains(&ToolCategory::Attachments),
            std::sync::atomic::Ordering::Relaxed,
        );
        attachment_actions::configure(&registry, &config.attachments, config.busy_timeout_ms);

        #[cfg(feature = "code")]
        {
            CODE_ENABLED.store(config.code_enabled, std::sync::atomic::Ordering::Relaxed);
            if config.code_enabled {
                // Per-project code databases live in a sibling directory keyed to
                // the main memory file, so distinct memory DBs never collide.
                let base = PathBuf::from(format!("{}.code", config.memory_file_path));
                crate::code_registry::init(
                    base.clone(),
                    config.durability,
                    config.sqlite_tuning(),
                    lru_cache,
                    config.read_pool_size,
                );
                // Code semantic search shares the per-project code databases: an
                // HNSW index is opened on the same files, keyed by symbol entity id.
                crate::code_vec_registry::init(base, config.code_embedding_dims);
            }
        }

        // The subscription tools open their own connection to the same
        // memory file, the way `OauthState` opens its own connection for
        // the OAuth tables. The delivery worker is a separate runtime role
        // and reads the memory path itself; this only serves the MCP tool
        // handlers below.
        #[cfg(feature = "webhooks")]
        crate::actions::webhooks::init(
            std::path::PathBuf::from(&config.memory_file_path),
            config.busy_timeout_ms,
        );

        #[cfg(feature = "code")]
        crate::repos::init(
            std::path::PathBuf::from(&config.memory_file_path),
            config.busy_timeout_ms,
        );

        Ok(Self {
            config: Arc::new(config),
            registry,
            kg,
            handles,
            vs,
        })
    }

    /// Convenience constructor for a pure knowledge-graph server (no vectors).
    pub fn new_kg(config: Config) -> Result<Self> {
        let mut config = config;
        config.vectors_enabled = false;
        Self::new(config, VectorConfig::new(0))
    }

    /// Expose the legacy graph handle for local callers and tests.
    pub fn graph(&self) -> Arc<GraphHandle> {
        Arc::clone(&self.kg)
    }
    /// The registry used for graph selection and ownership checks.
    pub fn workspace_registry(&self) -> Arc<WorkspaceRegistry> {
        Arc::clone(&self.registry)
    }

    /// The bounded per-workspace handle cache used by dispatch.
    pub fn workspace_handles(&self) -> Arc<WorkspaceHandles> {
        Arc::clone(&self.handles)
    }

    /// The shared vector store, if vector support is enabled.
    pub fn vector_store(&self) -> Option<Arc<VectorStore>> {
        self.vs.clone()
    }

    /// stdio transport: newline-delimited JSON-RPC over stdin/stdout.
    pub async fn run_stdio(&self) -> Result<()> {
        // No OAuth on the stdio transport: it serves one local process over a
        // pipe, `run_http` is the only path that opens an OAuth store, and so
        // there is nothing here to sweep.
        spawn_maintenance(self.kg.clone(), None);
        spawn_wal_flush(self.kg.clone(), self.config.wal_flush_ms);
        let stdin = tokio::io::stdin();
        let reader = BufReader::with_capacity(BUFFER_CAPACITY, stdin);
        let stdout = tokio::io::stdout();
        serve_line_conn(
            reader,
            stdout,
            Arc::clone(&self.registry),
            Arc::clone(&self.handles),
            self.config.stdio_concurrency,
        )
        .await
    }

    /// MCP Streamable HTTP transport (POST/GET `/mcp`, JSON or SSE responses).
    pub async fn run_http(&self, addr: &str) -> Result<()> {
        crate::config::Config::require_http_auth(
            self.config.oauth.is_some(),
            self.config.auth_token.as_deref(),
        )?;
        // The legacy graph handle migrated the schema during server setup.
        // Open the OAuth store before the maintenance task starts to sweep it.
        let oauth = match self.config.oauth.clone() {
            Some(cfg) => Some(Arc::new(crate::oauth_routes::OauthState::open(
                cfg,
                Path::new(&self.config.memory_file_path),
                self.config.busy_timeout_ms,
            )?)),
            None => None,
        };
        spawn_maintenance(self.kg.clone(), oauth.clone());
        spawn_wal_flush(self.kg.clone(), self.config.wal_flush_ms);
        crate::http::run(crate::http::HttpRunConfig {
            addr: addr.to_owned(),
            registry: self.workspace_registry(),
            handles: self.workspace_handles(),
            auth_token: self.config.auth_token.clone(),
            // Scopes granted to the static bearer token. Defaults to every
            // category, so a token holder keeps the reach it had before scopes
            // existed; `--static-bearer-scopes` narrows it.
            bearer_scopes: Arc::from(self.config.bearer_scopes.clone()),
            enabled_categories: Arc::from(self.config.enabled_categories.clone()),
            attachments: self.config.attachments.clone(),
            oauth,
            tls_cert: self.config.tls_cert.clone(),
            tls_key: self.config.tls_key.clone(),
        })
        .await
    }
}

/// Spawn a background task that fsyncs committed WAL frames every
/// `interval_ms` milliseconds via a non-blocking passive checkpoint, bounding
/// the durability window in async mode. A zero interval disables the task.
fn spawn_wal_flush(kg: Arc<GraphHandle>, interval_ms: u64) {
    if interval_ms == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
        interval.tick().await; // skip immediate first tick
        loop {
            interval.tick().await;
            let kg = kg.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = kg.checkpoint_passive() {
                    tracing::warn!("WAL flush error: {e}");
                }
            })
            .await
            .ok();
        }
    });
}

/// Spawn a background task that runs periodic database maintenance every
/// 5 minutes until the runtime shuts down.
///
/// It carries the OAuth store's maintenance too, on the same tick and the same
/// blocking thread rather than a second task: both are one SQLite statement
/// group against one file, five minutes apart, and a table that is swept only
/// while the graph is also being maintained is a table swept exactly as often
/// as it needs to be. `OauthState::maintain` holds the whole of that work, so
/// this function decides only *when*.
fn spawn_maintenance(kg: Arc<GraphHandle>, oauth: Option<Arc<crate::oauth_routes::OauthState>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(300));
        interval.tick().await; // skip immediate first tick
        loop {
            interval.tick().await;
            let kg = kg.clone();
            let oauth = oauth.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = kg.run_maintenance() {
                    tracing::warn!("Maintenance error: {e}");
                }
                // Under `spawn_blocking`, so taking the store lock here cannot
                // stall a request on the async reactor.
                if let Some(oauth) = oauth {
                    let removed = oauth.maintain();
                    if !removed.is_empty() {
                        tracing::info!(
                            swept = removed.swept,
                            evicted = removed.evicted,
                            "OAuth maintenance"
                        );
                    }
                }
            })
            .await
            .ok();
        }
    });
}

/// Drive one line-framed connection (stdio or a single TCP socket): read
/// newline-delimited JSON-RPC requests, write newline-delimited responses.
/// Notifications produce no output. Returns when the peer closes the stream.
/// The dispatch path (graph lock + optional fsync) is offloaded to
/// [`tokio::task::spawn_blocking`] to keep the async reactor responsive (C3).
async fn serve_line_conn<R, W>(
    mut reader: R,
    mut writer: W,
    registry: Arc<WorkspaceRegistry>,
    handles: Arc<WorkspaceHandles>,
    concurrency: usize,
) -> Result<()>
where
    R: AsyncBufReadExt + Unpin,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    let mut line = String::with_capacity(1024);
    let mut read_buf = Vec::with_capacity(1024);
    let concurrency = concurrency.max(1);

    // Requests dispatch on blocking threads, up to `concurrency` in flight
    // (the semaphore also back-pressures the read loop); a dedicated writer
    // task serializes responses. Responses may therefore be written in
    // completion order, not arrival order — JSON-RPC clients match on id, and
    // `concurrency = 1` restores strict ordering for clients that pipeline
    // order-dependent writes. Each response is one atomic channel message, so
    // lines never interleave.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(concurrency * 2);
    let writer_task = tokio::spawn(async move {
        let mut out = Vec::with_capacity(BUFFER_CAPACITY);
        while let Some(resp) = rx.recv().await {
            out.clear();
            out.extend_from_slice(resp.as_bytes());
            out.extend_from_slice(NEWLINE);
            if writer.write_all(&out).await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
    });
    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));

    loop {
        match read_line_capped(&mut reader, &mut read_buf, &mut line, MAX_REQUEST_BYTES).await {
            Ok(LineRead::Eof) => break,
            Ok(LineRead::Line) => {
                let Ok(permit) = Arc::clone(&sem).acquire_owned().await else {
                    break; // semaphore closed: unreachable, but don't spin
                };
                let line_copy = line.clone();
                let registry_clone = Arc::clone(&registry);
                let handles_clone = Arc::clone(&handles);
                let tx = tx.clone();
                tokio::spawn(async move {
                    let resp = tokio::task::spawn_blocking(move || {
                        dispatch_line(&line_copy, &registry_clone, &handles_clone)
                    })
                    .await;
                    drop(permit);
                    match resp {
                        Ok(Some(resp)) => {
                            let _ = tx.send(resp).await;
                        }
                        Ok(None) => {} // notification: no response
                        Err(join_err) => {
                            // The request's id was lost with the panic, so no
                            // error response can be correlated; log and let the
                            // client time out that one call.
                            error!("dispatch task panicked: {join_err}");
                        }
                    }
                });
            }
            Ok(LineRead::TooLong) => {
                let err = MCSError::InvalidParams("Request exceeds maximum size of 16MB".into());
                let response = JsonRpcResponse::error(None, err.error_code(), err.to_string());
                let resp = serde_json::to_string(&response).map_err(MCSError::JsonError)?;
                let _ = tx.send(resp).await;
                break;
            }
            Err(e) => {
                error!("IO error: {}", e);
                break;
            }
        }
    }
    // Let in-flight dispatches finish and drain their responses: tasks hold
    // `tx` clones, so the writer exits once the last one completes.
    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

fn process_request(
    req: &JsonRpcRequest,
    principal: &Principal,
    registry: &WorkspaceRegistry,
    handles: &WorkspaceHandles,
) -> Result<HandlerResult> {
    match req.method.as_str() {
        "initialize" => Ok(HandlerResult::Value(handle_initialize(
            req,
            handles.legacy_vs().is_some(),
        ))),
        "tools/list" => Ok(HandlerResult::Value(handle_tools_list(
            handles.legacy_vs().as_deref(),
            principal,
        ))),
        "tools/call" => handle_tools_call(req, principal, registry, handles),
        "ping" => Ok(HandlerResult::Value(Value::Null)),
        method if method.starts_with("notifications/") => {
            tracing::trace!("Received notification: {method}");
            Ok(HandlerResult::Value(Value::Null))
        }
        _ => Err(MCSError::MethodNotFound(req.method.clone())),
    }
}

/// MCP protocol revisions this server can speak, newest first (for `initialize`
/// version negotiation).
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Newest revision we implement; offered when the client requests an unknown one.
const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// `instructions` surfaced to the client and appended to the model's system prompt.
const SERVER_INSTRUCTIONS: &str = "Knowledge-graph memory MCP server. Entity names are unique and \
case-sensitive. Use `create_entities`/`create_relations` to build the graph, `add_observations` to \
attach facts, and `search_nodes`/`open_nodes`/`read_graph` to retrieve. Use `suggest_taxonomy` to \
find the closest existing taxonomy names before naming a new type. Prefer `upsert_entities` for \
idempotent writes and `merge_entities` to collapse duplicates. Tool failures are returned with \
`isError: true` rather than as protocol errors — read the message and retry.";

/// Extra guidance appended to [`SERVER_INSTRUCTIONS`] when vector support is on.
const VECTOR_INSTRUCTIONS: &str = " Vector search is enabled over the serving chunk index: \
use `vector_search_entities` or `hybrid_search` with an embedding, `vector_search_by_entity` for \
'more like this', and `semantic_search` when a server-side embedding service is configured.";

fn handle_initialize(req: &JsonRpcRequest, vectors_enabled: bool) -> Value {
    // Version negotiation: echo a supported requested revision, else offer latest.
    let protocol_version = req
        .params
        .as_ref()
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str)
        .filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(LATEST_PROTOCOL_VERSION);

    let instructions = if vectors_enabled {
        format!("{SERVER_INSTRUCTIONS}{VECTOR_INSTRUCTIONS}")
    } else {
        SERVER_INSTRUCTIONS.to_string()
    };

    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": "mcpmem",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": instructions
    })
}

/// Wrap a tool execution failure as an MCP `CallToolResult` with `isError: true`
/// so the model sees the message and can self-correct, instead of receiving an
/// opaque JSON-RPC protocol error. (Successful results are already content-
/// wrapped by the action handlers.)
#[inline]
fn tool_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true
    })
}

/// Constant-time bearer-token check. Accepts the raw token or a `Bearer <token>`
/// form; surrounding whitespace is trimmed.
pub fn token_matches(presented: &str, expected: &str) -> bool {
    use subtle::ConstantTimeEq;
    let presented = presented.trim();
    let presented = presented
        .strip_prefix("Bearer ")
        .unwrap_or(presented)
        .trim();
    presented.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// The base knowledge-graph tools, parsed from `tools.json` at build time.
fn base_tools() -> &'static Vec<Value> {
    static BASE: std::sync::OnceLock<Vec<Value>> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        serde_json::from_str(include_str!("../tools.json"))
            .expect("tools.json is valid JSON compiled at build time")
    })
}

/// The vector tools, parsed from `vector_tools.json` at build time.
fn vector_tools() -> &'static Vec<Value> {
    static VEC: std::sync::OnceLock<Vec<Value>> = std::sync::OnceLock::new();
    VEC.get_or_init(|| {
        serde_json::from_str(include_str!("../vector_tools.json"))
            .expect("vector_tools.json is valid JSON compiled at build time")
    })
}

/// The code-indexing tools, parsed from `code_tools.json` at build time.
#[cfg(feature = "code")]
fn code_tools() -> &'static Vec<Value> {
    static CODE: std::sync::OnceLock<Vec<Value>> = std::sync::OnceLock::new();
    CODE.get_or_init(|| {
        serde_json::from_str(include_str!("../code_tools.json"))
            .expect("code_tools.json is valid JSON compiled at build time")
    })
}

/// The webhook subscription-management tools, parsed from
/// `webhooks_tools.json` at build time.
#[cfg(feature = "webhooks")]
fn webhook_tools() -> &'static Vec<Value> {
    static HOOKS: std::sync::LazyLock<Vec<Value>> = std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../webhooks_tools.json"))
            .expect("webhooks_tools.json is valid JSON compiled at build time")
    });
    &HOOKS
}

/// `tools/list` advertises a tool only when its category is enabled and the
/// caller holds its scope. Attachment tools have their own category; they do
/// not inherit graph-read or graph-write from their operation type.
fn handle_tools_list(vs: Option<&VectorStore>, principal: &Principal) -> Value {
    let (read, write, attachments) = (
        graph_read_enabled(),
        graph_write_enabled(),
        attachments_enabled(),
    );
    let mut all: Vec<Value> = base_tools()
        .iter()
        .filter(|t| {
            t.get("name").and_then(Value::as_str).is_some_and(|n| {
                let category_on = match tools::category_of(n) {
                    Some(tools::ToolCategory::GraphRead) => read,
                    Some(tools::ToolCategory::GraphWrite) => write,
                    Some(tools::ToolCategory::Attachments) => attachments,
                    _ => false,
                };
                // The machine-admin tools need an admin human or local stdio
                // in addition to their graph-write category. Listing them for
                // a caller whose call would be refused is a false promise.
                let machine_ok =
                    !tools::is_machine_tool_name(n) || authz::may_manage_machines(principal);
                category_on && authz::allows_tool(principal, n) && machine_ok
            })
        })
        .cloned()
        .collect();
    if legacy_observations() {
        for tool in &mut all {
            let pointer = match tool["name"].as_str() {
                Some("create_entities" | "upsert_entities") => {
                    "/inputSchema/properties/entities/items/properties/observations/items"
                }
                Some("add_observations") => {
                    "/inputSchema/properties/observations/items/properties/contents/items"
                }
                Some("delete_observations") => {
                    "/inputSchema/properties/deletions/items/properties/observations/items"
                }
                Some("add_relation_observations") => {
                    "/inputSchema/properties/relations/items/properties/contents/items"
                }
                Some("delete_relation_observations" | "create_relations") => {
                    "/inputSchema/properties/relations/items/properties/observations/items"
                }
                _ => continue,
            };
            *tool
                .pointer_mut(pointer)
                .expect("compiled observation write schema") = json!({"type":"string"});
        }
    }
    if vs.is_some() {
        all.extend(
            vector_tools()
                .iter()
                .filter(|t| in_scope(t, principal) && vector_tool_listed(t, vs))
                .cloned(),
        );
    }
    #[cfg(feature = "code")]
    if code_enabled() {
        all.extend(
            code_tools()
                .iter()
                .filter(|t| in_scope(t, principal))
                .cloned(),
        );
    }
    #[cfg(feature = "webhooks")]
    if graph_write_enabled() {
        all.extend(
            webhook_tools()
                .iter()
                .filter(|t| in_scope(t, principal))
                .cloned(),
        );
    }
    json!({ "tools": all })
}

/// `true` when the caller's scopes cover the tool this manifest entry names.
#[inline]
fn in_scope(tool: &Value, principal: &Principal) -> bool {
    tool.get("name")
        .and_then(Value::as_str)
        .is_some_and(|n| authz::allows_tool(principal, n))
}

/// `false` for a vector tool this process cannot run. Only `semantic_search`
/// carries an availability condition: it is advertised when this process has
/// an embedding provider configured. Which workspace can actually serve a
/// call is validated per call against the selected workspace's serving
/// profile — the legacy store's profile alone must not hide the tool when a
/// registered workspace holds the profile.
#[inline]
fn vector_tool_listed(tool: &Value, _vs: Option<&VectorStore>) -> bool {
    match tool.get("name").and_then(Value::as_str) {
        Some(tools::SEMANTIC_SEARCH) => provider_configured(),
        _ => true,
    }
}

/// Whether an embedding provider is configured for this process. The
/// per-call handler decides whether the selected workspace's profile can
/// actually embed; listing only needs to know the tool could work somewhere.
#[cfg(feature = "indexer")]
fn provider_configured() -> bool {
    crate::indexer_provider::is_configured()
}
#[cfg(not(feature = "indexer"))]
const fn provider_configured() -> bool {
    false
}

/// Process-wide flag for the code-indexing subsystem, set once at server
/// startup from `config.code_enabled`. Code tools carry no per-request state
/// (unlike the vector store), so a global flag avoids threading a bool through
/// every dispatch signature.
#[cfg(feature = "code")]
static CODE_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "code")]
fn code_enabled() -> bool {
    CODE_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}
#[cfg(not(feature = "code"))]
const fn code_enabled() -> bool {
    false
}

/// Resolve the `workspaceId` argument for one call — or the caller's saved
/// default when it is absent — against the required access. The failure is a
/// tool error, so the caller sees an `isError` result rather than a protocol
/// error. The explicit ID never changes the caller's saved default, and the
/// default is consulted per call, so a batch resolves each call on its own.
fn resolve_for_call(
    tool_args: Option<&Value>,
    principal: &Principal,
    registry: &WorkspaceRegistry,
    access: WorkspaceAccess,
) -> std::result::Result<WorkspaceRecord, Value> {
    // An explicit `workspaceId` of the wrong type is an input error. It must
    // not read as "absent": a malformed selector must never fall back to the
    // caller's default, where a write could land unseen.
    let requested = match tool_args.and_then(|a| a.get("workspaceId")) {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| tool_error("'workspaceId' must be a string when present"))?,
        ),
    };
    registry
        .resolve(&principal.id, requested, access)
        .map_err(|e| tool_error(&e.to_string()))
}

/// Resolve one call's workspace and return the open handles for its file.
fn selected_handles(
    tool_args: Option<&Value>,
    principal: &Principal,
    registry: &WorkspaceRegistry,
    handles: &WorkspaceHandles,
    access: WorkspaceAccess,
) -> std::result::Result<(WorkspaceRecord, WorkspaceEntry), Value> {
    let record = resolve_for_call(tool_args, principal, registry, access)?;
    let entry = handles
        .get(&record)
        .map_err(|e| tool_error(&e.to_string()))?;
    Ok((record, entry))
}

fn handle_tools_call(
    req: &JsonRpcRequest,
    principal: &Principal,
    registry: &WorkspaceRegistry,
    handles: &WorkspaceHandles,
) -> Result<HandlerResult> {
    let tool_name = req
        .params
        .as_ref()
        .and_then(|p| p.get("name").and_then(|v| v.as_str()))
        .ok_or_else(|| MCSError::InvalidParams("Missing 'name' parameter".into()))?;

    // Scope gate. This is the authoritative check: it covers every transport,
    // not only the HTTP body screen.
    if let Some(scope) = authz::missing_scope(principal, tool_name) {
        return Err(MCSError::InsufficientScope {
            tool: tool_name.to_owned(),
            scope,
        });
    }

    let tool_args = req.params.as_ref().and_then(|p| p.get("arguments"));
    let adapted_args = if legacy_observations() {
        legacy_observation_input(tool_name, tool_args)?
    } else {
        None
    };
    let tool_args = adapted_args.as_ref().or(tool_args);

    // Workspace-management tools operate on the registry itself; they have no
    // graph selection of their own. They follow the same process-wide
    // category gate as the graph tools: reads need graph-read, mutations
    // need graph-write, regardless of what scopes the caller holds.
    if tools::is_management_tool_name(tool_name) {
        let category_enabled = if tools::is_write_tool(tool_name) {
            graph_write_enabled()
        } else {
            graph_read_enabled()
        };
        if !category_enabled {
            return Err(MCSError::MethodNotFound(tool_name.to_string()));
        }
        let result = handle_management_tool(tool_name, tool_args, principal, registry, handles);
        return Ok(result.unwrap_or_else(|e| {
            error!("Tool '{tool_name}' error: {e}");
            HandlerResult::Value(tool_error(&e.to_string()))
        }));
    }

    if tools::is_vector_tool_name(tool_name) {
        // The process-wide vector gate. The legacy entry carries the store the
        // server was built with, so its presence is the support signal.
        if handles.legacy_vs().is_none() {
            return Err(MCSError::MethodNotFound(format!(
                "{tool_name} (vector support disabled; start the server with --enable-vectors)"
            )));
        }
        let (_record, entry) = match selected_handles(
            tool_args,
            principal,
            registry,
            handles,
            WorkspaceAccess::Read,
        ) {
            Ok(pair) => pair,
            Err(err) => return Ok(HandlerResult::Value(err)),
        };
        let kg = &entry.kg;
        let Some(vs) = entry.vs.as_deref() else {
            return Err(MCSError::MethodNotFound(format!(
                "{tool_name} (vector support disabled; start the server with --enable-vectors)"
            )));
        };
        let allow_attachments = attachments_enabled() && principal.scopes.contains("attachments");
        let result = match tool_name {
            "vector_search_entities" => {
                vector_actions::handle_vector_search_entities(vs, kg, tool_args, allow_attachments)
                    .map(HandlerResult::RawResult)
            }
            "hybrid_search" => {
                vector_actions::handle_hybrid_search(vs, kg, tool_args, allow_attachments)
                    .map(HandlerResult::RawResult)
            }
            "vector_refresh_graph_cache" => {
                vector_actions::handle_refresh_graph_cache(vs, kg, tool_args)
                    .map(HandlerResult::Value)
            }
            "vector_store_stats" => vector_actions::handle_vector_store_stats(vs, kg, tool_args)
                .map(HandlerResult::Value),
            "vector_search_by_entity" => {
                vector_actions::handle_vector_search_by_entity(vs, kg, tool_args, allow_attachments)
                    .map(HandlerResult::RawResult)
            }
            "vector_mmr_search" => {
                vector_actions::handle_vector_mmr_search(vs, kg, tool_args, allow_attachments)
                    .map(HandlerResult::RawResult)
            }
            #[cfg(feature = "indexer")]
            tools::SEMANTIC_SEARCH => {
                vector_actions::handle_semantic_search(vs, kg, tool_args, allow_attachments)
                    .map(HandlerResult::RawResult)
            }
            // The name is a vector tool on every build, so dispatch must answer
            // for it here. Without the feature there is no handler, and a bare
            // "method not found" would not say why.
            #[cfg(not(feature = "indexer"))]
            tools::SEMANTIC_SEARCH => Err(MCSError::MethodNotFound(format!(
                "{tool_name} (this build has no embedding worker; rebuild with \
                 --features indexer)"
            ))),
            other => Err(MCSError::MethodNotFound(other.to_string())),
        };
        return Ok(result.unwrap_or_else(|e| {
            error!("Tool '{tool_name}' error: {e}");
            HandlerResult::Value(tool_error(&e.to_string()))
        }));
    }

    if tools::is_code_tool_name(tool_name) {
        if !code_enabled() {
            return Err(MCSError::MethodNotFound(format!(
                "{tool_name} (code indexing disabled; start the server with --enable-code)"
            )));
        }
        #[cfg(feature = "code")]
        {
            let result = match tool_name {
                "code_index" => {
                    code_actions::handle_code_index(tool_args).map(HandlerResult::Value)
                }
                "code_outline" => {
                    code_actions::handle_code_outline(tool_args).map(HandlerResult::Value)
                }
                "code_search" => {
                    code_actions::handle_code_search(tool_args).map(HandlerResult::Value)
                }
                "code_get_symbol" => {
                    code_actions::handle_code_get_symbol(tool_args).map(HandlerResult::Value)
                }
                "code_watch" => {
                    code_actions::handle_code_watch(tool_args).map(HandlerResult::Value)
                }
                "code_embed" => {
                    code_actions::handle_code_embed(tool_args).map(HandlerResult::Value)
                }
                "code_semantic_search" => {
                    code_actions::handle_code_semantic_search(tool_args).map(HandlerResult::Value)
                }
                "code_repo_add" => {
                    code_actions::handle_code_repo_add(tool_args).map(HandlerResult::Value)
                }
                "code_repo_list" => {
                    code_actions::handle_code_repo_list(tool_args).map(HandlerResult::Value)
                }
                "code_repo_reindex" => {
                    code_actions::handle_code_repo_reindex(tool_args).map(HandlerResult::Value)
                }
                "code_repo_remove" => {
                    code_actions::handle_code_repo_remove(tool_args).map(HandlerResult::Value)
                }
                other => Err(MCSError::MethodNotFound(other.to_string())),
            };
            return Ok(result.unwrap_or_else(|e| {
                error!("Tool '{tool_name}' error: {e}");
                HandlerResult::Value(tool_error(&e.to_string()))
            }));
        }
        #[cfg(not(feature = "code"))]
        return Err(MCSError::MethodNotFound(format!(
            "{tool_name} (built without the 'code' feature)"
        )));
    }

    if tools::is_webhook_tool_name(tool_name) {
        // Unlike `code_enabled()`, `graph_write_enabled()` does not already
        // encode "this build has no webhook feature": it is the ordinary
        // graph-write flag, and it can be on in a build that never compiled
        // this feature at all. Check the feature first, so a caller gets a
        // build-related reason instead of a bare "not found".
        #[cfg(not(feature = "webhooks"))]
        return Err(MCSError::MethodNotFound(format!(
            "{tool_name} (built without the 'webhooks' feature)"
        )));
        #[cfg(feature = "webhooks")]
        {
            if !graph_write_enabled() {
                return Err(MCSError::MethodNotFound(tool_name.to_string()));
            }
            // Subscriptions live in the workspace's own graph file, and only
            // the workspace owner may create or remove them — the graph-write
            // scope alone is not enough.
            let record =
                match resolve_for_call(tool_args, principal, registry, WorkspaceAccess::Owner) {
                    Ok(record) => record,
                    Err(err) => return Ok(HandlerResult::Value(err)),
                };
            let result = match tool_name {
                "webhook_add_subscription" => {
                    webhooks_actions::handle_webhook_add_subscription(tool_args, &record.graph_path)
                        .map(HandlerResult::Value)
                }
                "webhook_delete_subscription" => {
                    webhooks_actions::handle_webhook_delete_subscription(
                        tool_args,
                        &record.graph_path,
                    )
                    .map(HandlerResult::Value)
                }
                other => Err(MCSError::MethodNotFound(other.to_string())),
            };
            return Ok(result.unwrap_or_else(|e| {
                error!("Tool '{tool_name}' error: {e}");
                HandlerResult::Value(tool_error(&e.to_string()))
            }));
        }
    }

    if tools::is_attachment_tool_name(tool_name) {
        if !attachments_enabled() {
            return Err(MCSError::MethodNotFound(tool_name.to_owned()));
        }
        let access = match tool_name {
            "begin_attachment_upload"
            | "append_attachment_chunk"
            | "finish_attachment_upload"
            | "cancel_attachment_upload"
            | "delete_attachment" => WorkspaceAccess::Write,
            _ => WorkspaceAccess::Read,
        };
        let (record, _entry) =
            match selected_handles(tool_args, principal, registry, handles, access) {
                Ok(pair) => pair,
                Err(err) => return Ok(HandlerResult::Value(err)),
            };
        let result = attachment_actions::settings(registry).and_then(|settings| {
            attachment_actions::handle(
                tool_name,
                tool_args,
                &principal.id,
                &record.graph_path,
                &settings.limits,
                settings.busy_timeout_ms,
            )
        });
        return Ok(HandlerResult::Value(result.unwrap_or_else(|error| {
            error!("Tool '{tool_name}' error: {error}");
            tool_error(&error.to_string())
        })));
    }

    // Knowledge-graph category gate: a KG tool is reachable only if it exists
    // AND its category (graph-read for queries, graph-write for mutations) was
    // enabled at startup. Disabled tools are hidden from tools/list, so a call
    // to one is treated as an unknown method.
    let Some(meta) = tools::ALL_TOOLS.iter().find(|t| t.name == tool_name) else {
        return Err(MCSError::MethodNotFound(tool_name.to_string()));
    };
    let category_enabled = if meta.write {
        graph_write_enabled()
    } else {
        graph_read_enabled()
    };
    if !category_enabled {
        return Err(MCSError::MethodNotFound(tool_name.to_string()));
    }

    // One workspace per call: the explicit `workspaceId`, or the caller's
    // saved default. A write needs writer-or-owner access, a read needs any
    // access. Resolved here, before the handler runs, so a denied mutation
    // never executes.
    let access = if meta.write {
        WorkspaceAccess::Write
    } else {
        WorkspaceAccess::Read
    };
    let (_record, entry) = match selected_handles(tool_args, principal, registry, handles, access) {
        Ok(pair) => pair,
        Err(err) => return Ok(HandlerResult::Value(err)),
    };
    let kg = &entry.kg;
    let vs = entry.vs.as_deref();

    let result = match tool_name {
        // Raw-result handlers (large payloads, avoid second serialization pass).
        "read_graph" => memory::handle_read_graph(kg, tool_args).map(HandlerResult::RawResult),
        "search_nodes" => memory::handle_search_nodes(kg, tool_args).map(HandlerResult::RawResult),
        // Standard Value handlers.
        "create_entities" => {
            memory::handle_create_entities(kg, vs, tool_args).map(HandlerResult::Value)
        }
        "create_relations" => {
            memory::handle_create_relations(kg, vs, tool_args).map(HandlerResult::Value)
        }
        "add_observations" => {
            memory::handle_add_observations(kg, tool_args).map(HandlerResult::Value)
        }
        "add_relation_observations" => {
            memory::handle_add_relation_observations(kg, tool_args).map(HandlerResult::Value)
        }
        "delete_relation_observations" => {
            memory::handle_delete_relation_observations(kg, tool_args).map(HandlerResult::Value)
        }
        "set_attributes" => memory::handle_set_attributes(kg, tool_args).map(HandlerResult::Value),
        "delete_attributes" => {
            memory::handle_delete_attributes(kg, tool_args).map(HandlerResult::Value)
        }
        "delete_entities" => {
            let r = memory::handle_delete_entities(kg, tool_args);
            if r.is_ok()
                && let Some(vs) = vs
                && let Some(args) = tool_args
                    .and_then(|a| a.get("entityNames"))
                    .and_then(|v| v.as_array())
            {
                let names: Vec<String> = args
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                vs.invalidate_entity_cache(&names);
            }
            r.map(HandlerResult::Value)
        }
        "delete_observations" => {
            memory::handle_delete_observations(kg, tool_args).map(HandlerResult::Value)
        }
        "delete_relations" => {
            memory::handle_delete_relations(kg, tool_args).map(HandlerResult::Value)
        }
        "open_nodes" => memory::handle_open_nodes(kg, tool_args).map(HandlerResult::Value),
        "get_entity" => memory::handle_get_entity(kg, tool_args).map(HandlerResult::Value),
        "graph_stats" => memory::handle_graph_stats(kg).map(HandlerResult::Value),
        "search_relations" => {
            memory::handle_search_relations(kg, tool_args).map(HandlerResult::Value)
        }
        "find_path" => memory::handle_find_path(kg, tool_args).map(HandlerResult::Value),
        "compact" => memory::handle_compact(kg).map(HandlerResult::Value),
        "get_neighbors" => memory::handle_get_neighbors(kg, tool_args).map(HandlerResult::Value),
        "describe_entity" => {
            memory::handle_describe_entity(kg, tool_args).map(HandlerResult::Value)
        }
        "list_entity_types" => memory::handle_list_entity_types(kg).map(HandlerResult::Value),
        "list_relation_types" => memory::handle_list_relation_types(kg).map(HandlerResult::Value),
        "suggest_taxonomy" => {
            taxonomy::handle_suggest_taxonomy(vs, kg, tool_args).map(HandlerResult::Value)
        }
        "set_type_description" => {
            memory::handle_set_type_description(kg, tool_args).map(HandlerResult::Value)
        }
        "upsert_entities" => {
            memory::handle_upsert_entities(kg, vs, tool_args).map(HandlerResult::Value)
        }
        "export_graph" => memory::handle_export_graph(kg, tool_args).map(HandlerResult::Value),
        "merge_entities" => memory::handle_merge_entities(kg, tool_args).map(HandlerResult::Value),
        "rename_entity" => memory::handle_rename_entity(kg, tool_args).map(HandlerResult::Value),
        "extract_subgraph" => {
            memory::handle_extract_subgraph(kg, tool_args).map(HandlerResult::Value)
        }
        "batch_get_entities" => {
            memory::handle_batch_get_entities(kg, tool_args).map(HandlerResult::Value)
        }
        "find_all_paths" => memory::handle_find_all_paths(kg, tool_args).map(HandlerResult::Value),
        "entity_exists" => memory::handle_entity_exists(kg, tool_args).map(HandlerResult::Value),
        "degree" => memory::handle_degree(kg, tool_args).map(HandlerResult::Value),
        tool => Err(MCSError::MethodNotFound(tool.to_string())),
    };

    // Tool execution failures become isError CallToolResults so the model can
    // read the message and self-correct, instead of an opaque protocol error.
    let result = result.unwrap_or_else(|e| {
        error!("Tool '{tool_name}' error: {e}");
        HandlerResult::Value(tool_error(&e.to_string()))
    });
    if legacy_observations() {
        legacy_observation_output(result)
    } else {
        Ok(result)
    }
}

/// A required string argument.
fn str_arg<'a>(params: &'a Value, field: &str) -> Result<&'a str> {
    params
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| MCSError::InvalidParams(format!("Missing '{field}' parameter")))
}

/// A success-shaped text tool result; `content[0].text` holds the JSON.
fn text_result(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }] })
}

/// Map a registry failure onto the error category its cause belongs to.
/// Accepts the owned value because `map_err` hands ownership to its mapper.
#[allow(clippy::needless_pass_by_value)]
fn management_error(error: crate::workspace::WorkspaceError) -> MCSError {
    use crate::workspace::WorkspaceError;
    match error {
        WorkspaceError::NotFound
        | WorkspaceError::SelectionRequired
        | WorkspaceError::AccessDenied => MCSError::MemoryError(error.to_string()),
        WorkspaceError::InvalidInput(_) | WorkspaceError::Graph(_) => {
            MCSError::InvalidParams(error.to_string())
        }
        WorkspaceError::Storage(_) | WorkspaceError::Io(_) => {
            MCSError::MemoryError(error.to_string())
        }
    }
}

/// The workspace-management tool handlers (approved spec §"MCP contract").
///
/// Result shapes follow the spec table: the creator/view/visibility/grant
/// tools return a direct result object, and the list and revoke tools put
/// their JSON in `content[0].text` like the other tools. The machine-admin
/// tools additionally demand `authz::may_manage_machines`, on top of their
/// `graph-write` category scope.
fn handle_management_tool(
    tool_name: &str,
    tool_args: Option<&Value>,
    principal: &Principal,
    registry: &WorkspaceRegistry,
    handles: &WorkspaceHandles,
) -> Result<HandlerResult> {
    match tool_name {
        "create_workspace" => {
            let params =
                tool_args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
            let name = str_arg(params, "name")?;
            let visibility = match params.get("visibility").and_then(Value::as_str) {
                Some("private") => Visibility::Private,
                Some("public") => Visibility::Public,
                _ => {
                    return Err(MCSError::InvalidParams(
                        "'visibility' must be 'private' or 'public'".into(),
                    ));
                }
            };
            let view = registry
                .create(&principal.id, name, visibility, |path| {
                    handles.initialize_graph(path)
                })
                .map_err(management_error)?;
            // The caller becomes the owner, and the first workspace becomes
            // the caller's default.
            Ok(HandlerResult::Value(json!({ "workspace": view })))
        }
        "list_workspaces" => {
            let params = tool_args.unwrap_or(&Value::Null);
            // A typed cursor is an input error, not "no cursor": an opaque
            // selector must never silently restart from the first page.
            let cursor =
                match params.get("cursor") {
                    None => None,
                    Some(value) => Some(value.as_str().ok_or_else(|| {
                        MCSError::InvalidParams("'cursor' must be a string".into())
                    })?),
                };
            let limit = match params.get("limit") {
                None => 100,
                Some(value) => {
                    let limit = value.as_u64().ok_or_else(|| {
                        MCSError::InvalidParams("'limit' must be a positive integer".into())
                    })?;
                    if limit == 0 {
                        return Err(MCSError::InvalidParams(
                            "'limit' must be a positive integer".into(),
                        ));
                    }
                    limit as usize
                }
            };
            let page = registry
                .list(&principal.id, cursor, limit)
                .map_err(management_error)?;
            let text = serde_json::to_string(&page).map_err(MCSError::JsonError)?;
            Ok(HandlerResult::Value(text_result(text.as_str())))
        }
        "get_workspace" => {
            let params =
                tool_args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
            let workspace_id = str_arg(params, "workspaceId")?;
            let (record, view) = registry
                .view(&principal.id, workspace_id)
                .map_err(management_error)?;
            let mut workspace = serde_json::to_value(&view).map_err(MCSError::JsonError)?;
            if record.owner_id == principal.id {
                workspace["ownerId"] = json!(record.owner_id);
            }
            Ok(HandlerResult::Value(json!({ "workspace": workspace })))
        }
        "set_workspace_visibility" => {
            let params =
                tool_args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
            let workspace_id = str_arg(params, "workspaceId")?;
            let visibility = match params.get("visibility").and_then(Value::as_str) {
                Some("private") => Visibility::Private,
                Some("public") => Visibility::Public,
                _ => {
                    return Err(MCSError::InvalidParams(
                        "'visibility' must be 'private' or 'public'".into(),
                    ));
                }
            };
            let view = registry
                .set_visibility(&principal.id, workspace_id, visibility)
                .map_err(management_error)?;
            Ok(HandlerResult::Value(json!({ "workspace": view })))
        }
        "list_workspace_grants" => {
            let params =
                tool_args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
            let workspace_id = str_arg(params, "workspaceId")?;
            let grants = registry
                .grants(&principal.id, workspace_id)
                .map_err(management_error)?;
            let text =
                serde_json::to_string(&json!({ "grants": grants })).map_err(MCSError::JsonError)?;
            Ok(HandlerResult::Value(text_result(text.as_str())))
        }
        "grant_workspace_access" => {
            let params =
                tool_args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
            let workspace_id = str_arg(params, "workspaceId")?;
            let target = str_arg(params, "principalId")?;
            let role = str_arg(params, "role")?;
            registry
                .grant(&principal.id, workspace_id, target, role)
                .map_err(management_error)?;
            Ok(HandlerResult::Value(json!({
                "grant": { "principalId": target, "role": role }
            })))
        }
        "revoke_workspace_access" => {
            let params =
                tool_args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
            let workspace_id = str_arg(params, "workspaceId")?;
            let target = str_arg(params, "principalId")?;
            let revoked = registry
                .revoke(&principal.id, workspace_id, target)
                .map_err(management_error)?;
            let text = serde_json::to_string(&json!({ "revoked": revoked }))
                .map_err(MCSError::JsonError)?;
            Ok(HandlerResult::Value(text_result(text.as_str())))
        }
        "set_default_workspace" => {
            let params =
                tool_args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
            let workspace_id = str_arg(params, "workspaceId")?;
            registry
                .set_default(&principal.id, workspace_id)
                .map_err(management_error)?;
            let (_, view) = registry
                .view(&principal.id, workspace_id)
                .map_err(management_error)?;
            Ok(HandlerResult::Value(json!({ "workspace": view })))
        }
        "create_machine_account" => {
            require_machine_admin(principal, tool_name)?;
            let params =
                tool_args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
            let name = str_arg(params, "name")?;
            let scopes: Vec<String> = match params.get("scopes") {
                Some(value) => serde_json::from_value(value.clone())
                    .map_err(|e| MCSError::InvalidParams(format!("Invalid 'scopes': {e}")))?,
                None => return Err(MCSError::InvalidParams("Missing 'scopes' parameter".into())),
            };
            let (principal_id, token) = registry
                .create_machine(name, &scopes)
                .map_err(management_error)?;
            // The credential appears exactly once, in this result.
            let text = serde_json::to_string(&json!({
                "principalId": principal_id,
                "token": token,
            }))
            .map_err(MCSError::JsonError)?;
            Ok(HandlerResult::Value(text_result(text.as_str())))
        }
        "list_machine_accounts" => {
            require_machine_admin(principal, tool_name)?;
            let accounts = registry.list_machines().map_err(management_error)?;
            let text = serde_json::to_string(&json!({ "accounts": accounts }))
                .map_err(MCSError::JsonError)?;
            Ok(HandlerResult::Value(text_result(text.as_str())))
        }
        "revoke_machine_account" => {
            require_machine_admin(principal, tool_name)?;
            let params =
                tool_args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
            let principal_id = str_arg(params, "principalId")?;
            let revoked = registry
                .revoke_machine(principal_id)
                .map_err(management_error)?;
            let text = serde_json::to_string(&json!({ "revoked": revoked }))
                .map_err(MCSError::JsonError)?;
            Ok(HandlerResult::Value(text_result(text.as_str())))
        }
        other => Err(MCSError::MethodNotFound(other.to_string())),
    }
}

/// Machine-account tools need an admin human or trusted local stdio on top of
/// the `graph-write` category scope. Neither the static bearer nor an issued
/// machine may administer accounts, whatever scopes it holds.
fn require_machine_admin(principal: &Principal, tool_name: &str) -> Result<()> {
    if authz::may_manage_machines(principal) {
        return Ok(());
    }
    Err(MCSError::InsufficientScope {
        tool: tool_name.to_owned(),
        scope: crate::principals::ADMIN_SCOPE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything a dispatch test needs: the graph, the registry and the
    /// handle cache of one built server.
    struct ServerForTest {
        kg: Arc<GraphHandle>,
        registry: Arc<WorkspaceRegistry>,
        handles: Arc<WorkspaceHandles>,
    }

    /// A test server with both graph categories enabled, built the same way
    /// `src/main.rs` builds one. The static bearer is configured as a real
    /// account (writer on the legacy workspace, saved as default), so a
    /// bearer principal's dispatch resolves instead of failing selection.
    fn server_for_test(dir: &tempfile::TempDir) -> ServerForTest {
        let config = Config {
            memory_file_path: dir.path().join("memory.db").to_string_lossy().into_owned(),
            legacy_owner_id: Some("machine:local".into()),
            auth_token: Some("lib-test-bearer".into()),
            enabled_categories: vec![
                tools::ToolCategory::GraphRead,
                tools::ToolCategory::GraphWrite,
            ],
            ..Config::default()
        };
        let server = MCPServer::new_kg(config).expect("test server builds");
        let registry = server.workspace_registry();
        let legacy = registry
            .all_paths()
            .expect("registered paths")
            .into_iter()
            .find(|(_, path)| path.ends_with("memory.db"))
            .expect("the legacy workspace")
            .0;
        registry
            .grant("machine:local", &legacy, "machine:static", "writer")
            .expect("static bearer is a registered identity");
        registry
            .set_default("machine:static", &legacy)
            .expect("static bearer default saves");
        ServerForTest {
            kg: server.graph(),
            registry: server.workspace_registry(),
            handles: server.workspace_handles(),
        }
    }

    /// The gate inside the dispatcher is the authoritative one: it covers every
    /// transport, not only the HTTP body screen. Exercise it directly, because
    /// the HTTP path refuses a denied call before dispatch ever sees it.
    #[test]
    fn tools_call_refuses_a_tool_outside_the_principals_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let server = server_for_test(&dir);
        let principal = authz::bearer_principal(&[tools::ToolCategory::GraphRead]);
        let req: JsonRpcRequest = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "delete_entities", "arguments": { "entityNames": ["a"] } }
        }))
        .unwrap();

        let Err(err) = process_request(&req, &principal, &server.registry, &server.handles) else {
            panic!("expected a scope refusal");
        };
        assert_eq!(err.error_code(), -32002, "{err}");
        assert!(
            matches!(&err, MCSError::InsufficientScope { tool, scope }
                if tool == "delete_entities" && *scope == "graph-write"),
            "{err}"
        );

        // Control: the same request with the scope passes the gate. The
        // category flag is process-wide, so put it back afterwards.
        let allowed = authz::bearer_principal(&[tools::ToolCategory::GraphWrite]);
        let was_on = graph_write_enabled();
        GRAPH_WRITE_ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
        let outcome = process_request(&req, &allowed, &server.registry, &server.handles);
        GRAPH_WRITE_ENABLED.store(was_on, std::sync::atomic::Ordering::Relaxed);
        assert!(outcome.is_ok(), "control failed");
    }

    /// The manifest, the tools/list output and the dispatch arm must agree on
    /// `suggest_taxonomy`: the compiled manifest announces it as a read-only
    /// tool, the list exposes it under the graph-read gate, and the call
    /// returns type suggestions from a seeded graph. The graph-read flag is
    /// process-wide, so it is restored afterwards.
    #[test]
    fn suggest_taxonomy_is_listed_and_dispatched_for_a_seeded_graph() {
        let dir = tempfile::tempdir().unwrap();
        let server = server_for_test(&dir);
        let kg: Arc<GraphHandle> = Arc::clone(&server.kg);
        memory::handle_create_entities(
            &kg,
            None,
            Some(&json!({"entities":[
                {"name":"seed","entityType":"person","observations":[]}
            ]})),
        )
        .unwrap();

        let was_on = graph_read_enabled();
        GRAPH_READ_ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
        let principal = authz::bearer_principal(&[tools::ToolCategory::GraphRead]);

        // The compiled manifest advertises the tool with read annotations.
        let entry = base_tools()
            .iter()
            .find(|t| t["name"].as_str() == Some("suggest_taxonomy"))
            .expect("tools.json lists suggest_taxonomy");
        assert_eq!(entry["annotations"]["readOnlyHint"].as_bool(), Some(true));
        assert_eq!(
            entry["inputSchema"]["properties"]["kind"]["enum"]
                .as_array()
                .map(|a| a.len()),
            Some(4)
        );

        // tools/list exposes it under the graph-read gate.
        let listed = handle_tools_list(None, &principal);
        let names: Vec<String> = listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str().map(String::from))
            .collect();
        assert!(
            names.iter().any(|n| n == "suggest_taxonomy"),
            "tools/list must announce suggest_taxonomy"
        );

        // The dispatch arm answers with suggestions for a seeded graph.
        let req: JsonRpcRequest = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "suggest_taxonomy",
                "arguments": { "query": "persn" }
            }
        }))
        .unwrap();
        let Ok(HandlerResult::Value(result)) =
            process_request(&req, &principal, &server.registry, &server.handles)
        else {
            panic!("expected a suggestion value");
        };
        let suggestions = &result["suggestions"];
        assert!(suggestions.is_array());
        assert_eq!(suggestions[0]["name"], "person");

        // A blank query fails through the same dispatch arm, as an
        // isError tool result rather than a protocol error.
        let req: JsonRpcRequest = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "suggest_taxonomy",
                "arguments": { "query": "  " }
            }
        }))
        .unwrap();
        let Ok(HandlerResult::Value(result)) =
            process_request(&req, &principal, &server.registry, &server.handles)
        else {
            panic!("expected a tool error value");
        };
        assert_eq!(result["isError"].as_bool(), Some(true));
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("must not be empty or whitespace"), "{text}");
        GRAPH_READ_ENABLED.store(was_on, std::sync::atomic::Ordering::Relaxed);
    }
}
