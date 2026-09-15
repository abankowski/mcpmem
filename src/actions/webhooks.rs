//! MCP tool handlers for webhook subscription management.
//!
//! `webhook_add_subscription` and `webhook_delete_subscription` write and
//! delete rows in the `webhook_subscription` table. They never call the
//! delivery worker. [`mcpmem_webhook::WebhookWorker`] polls the same table on
//! its own schedule, and it delivers to an endpoint only after its own
//! allowlist and secret store accept it. A subscription's `secretRef` is a
//! name, never the secret text — the worker resolves the name through its
//! own `SecretProvider` at delivery time.
//!
//! This module opens its own connection to the memory database. It shares
//! the file with the knowledge graph and the delivery worker, but not their
//! connection: SQLite's WAL mode lets independent connections read and write
//! one file, the same way [`crate::oauth_routes::OauthState`] already opens
//! its own connection for the OAuth tables.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

use mcpmem_core::mutation::ChangeOperation;
use mcpmem_core::subscriptions::{SubscriptionRepository, WebhookSubscription};
use mcpmem_webhook::{Resolver, WorkerError};
use parking_lot::Mutex;
use rusqlite::Connection;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::errors::{MCSError, Result};

macro_rules! text_content {
    ($text:expr) => {
        json!({ "content": [{ "type": "text", "text": $text }] })
    };
}

/// Upper bound on `consumerOrigin`. This is tighter than
/// [`WebhookSubscription::validate`]'s own 256-byte ceiling, so a caller
/// learns about an over-long origin at the tool boundary, before the row
/// reaches the shared validator. The admin API shares the bound.
pub const MAX_CONSUMER_ORIGIN_BYTES: usize = 231;

/// Cap on `eventOperations` / `entityTypes` / `ignoredOrigins` entries per
/// request. A guard against an unbounded stored JSON array, not a functional
/// limit — a real subscription needs only a few of each. Shared by the MCP
/// tools and the admin API.
pub const MAX_LIST_ITEMS: usize = 256;

/// Where the subscription table lives. [`init`] sets this once at server
/// startup; each handler call opens a short-lived connection against it.
struct SubscriptionDb {
    path: PathBuf,
    busy_timeout_ms: u64,
}

static SUBSCRIPTION_DB: OnceLock<SubscriptionDb> = OnceLock::new();

/// Record the memory database path for the subscription tools. Call once
/// from [`crate::server::MCPServer::new`]. A later call has no effect: the
/// path does not change for the life of the process.
pub fn init(path: PathBuf, busy_timeout_ms: u64) {
    let _ = SUBSCRIPTION_DB.set(SubscriptionDb {
        path,
        busy_timeout_ms,
    });
}

/// Open a short-lived connection to the subscription table's database. A
/// handler holds it only for the span of one tool call, so a busy timeout is
/// enough to ride out a concurrent writer; the connection needs no pool.
/// The admin API opens the same short-lived connection, so the two surfaces
/// read and write one store.
pub fn open_connection() -> Result<Connection> {
    let db = SUBSCRIPTION_DB.get().ok_or_else(|| {
        MCSError::MemoryError("webhook subscription store not initialized".into())
    })?;
    let conn = Connection::open(&db.path).map_err(mcpmem_core::events::sql_error)?;
    conn.busy_timeout(Duration::from_millis(db.busy_timeout_ms))
        .map_err(mcpmem_core::events::sql_error)?;
    Ok(conn)
}

/// Make the process-wide subscription store usable from a test, whatever
/// whose `init` pinned it. The pinned path may point at another test's temp
/// dir, deleted when that test ended: `Connection::open` cannot recreate a
/// missing parent directory, so recreate it first, then bootstrap a fresh
/// database file with the same schema production gets. Idempotent on a live
/// migrated store.
#[doc(hidden)]
pub fn ensure_test_store() -> Result<()> {
    let db = SUBSCRIPTION_DB.get().ok_or_else(|| {
        MCSError::MemoryError("webhook subscription store not initialized".into())
    })?;
    if let Some(parent) = db.path.parent() {
        std::fs::create_dir_all(parent).map_err(MCSError::IoError)?;
    }
    let conn = open_connection()?;
    mcpmem_core::schema::initialize_database(&conn)?;
    mcpmem_core::events::migrate(&conn)?;
    Ok(())
}

/// Stands in for DNS resolution when a subscription is registered. It does
/// no network I/O. It reports one fixed, public documentation address
/// (RFC 5737 `TEST-NET-3`), so [`mcpmem_webhook::validate_endpoint`]'s
/// address-safety check always passes here. The worker resolves the real
/// address, against the real allowlist, before every delivery attempt — this
/// check exists only to catch a malformed endpoint early.
struct PlaceholderResolver;

impl Resolver for PlaceholderResolver {
    fn resolve(&self, _hostname: &str) -> std::result::Result<Vec<IpAddr>, WorkerError> {
        Ok(vec![IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1))])
    }
}

/// A one-shot test delivery kit for the admin UI. It holds the same policy
/// the delivery worker holds — allowlist, secrets, resolver, connector — so
/// a test request is judged by the delivery-time rules, not by the lax
/// registration-time shape check.
pub struct WebhookTestKit {
    allowlist: BTreeSet<String>,
    secrets: mcpmem_webhook::StaticSecretProvider,
    resolver: Arc<dyn mcpmem_webhook::Resolver>,
    connector: Arc<dyn mcpmem_webhook::DeliveryConnector>,
    allow_private: bool,
}

impl WebhookTestKit {
    /// The production kit: the configured allowlist and secrets, the system
    /// resolver, and the real HTTPS connector.
    pub fn production(
        allowlist: BTreeSet<String>,
        secrets: BTreeMap<String, mcpmem_webhook::SigningKey>,
        allow_private: bool,
    ) -> Self {
        Self {
            allowlist,
            secrets: mcpmem_webhook::StaticSecretProvider(secrets),
            resolver: Arc::new(mcpmem_webhook::SystemResolver),
            connector: Arc::new(mcpmem_webhook::HttpsConnector::production()),
            allow_private,
        }
    }

    /// A kit with an injected resolver and connector, for tests.
    #[doc(hidden)]
    pub fn for_test(
        allowlist: BTreeSet<String>,
        secrets: BTreeMap<String, mcpmem_webhook::SigningKey>,
        resolver: Arc<dyn mcpmem_webhook::Resolver>,
        connector: Arc<dyn mcpmem_webhook::DeliveryConnector>,
        allow_private: bool,
    ) -> Self {
        Self {
            allowlist,
            secrets: mcpmem_webhook::StaticSecretProvider(secrets),
            resolver,
            connector,
            allow_private,
        }
    }

    /// Deliver one signed test event to the subscription's endpoint and
    /// return the HTTP response.
    pub fn deliver(
        &self,
        subscription: &WebhookSubscription,
    ) -> std::result::Result<mcpmem_webhook::DeliveryResponse, WorkerError> {
        mcpmem_webhook::deliver_test(
            &*self.connector,
            &self.secrets,
            &self.allowlist,
            &*self.resolver,
            self.allow_private,
            subscription,
        )
    }
}

/// The process-wide test kit. Production sets it once at startup from the
/// `[webhooks]` section; a test sets its own kit before building a router.
static TEST_KIT: Mutex<Option<Arc<WebhookTestKit>>> = Mutex::new(None);

/// Record the kit the admin test endpoint will use. Production calls this
/// once at startup; a test calls it with an injected kit.
pub fn set_test_kit(kit: Option<Arc<WebhookTestKit>>) {
    *TEST_KIT.lock() = kit;
}

/// The current kit, when the server configured webhook delivery. `None`
/// means the admin Test button cannot deliver.
pub fn test_kit() -> Option<Arc<WebhookTestKit>> {
    TEST_KIT.lock().clone()
}

/// Serializes the kit-mutating window of tests that run in parallel in one
/// binary. `set_test_kit` alone leaves two tests' windows free to interleave,
/// so one test can observe another's kit. See [`with_test_kit`].
static TEST_KIT_SERIAL: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Run `f` with the process-wide kit set to `kit`, then reset it to `None`.
/// The whole window holds a lock shared by every kit-using test in the
/// binary, so parallel tests never observe one another's kit. The sync form
/// is for `#[test]` functions; the async form for `#[tokio::test]` — the
/// await must happen while the lock is held.
#[doc(hidden)]
pub fn with_test_kit<T>(kit: Option<Arc<WebhookTestKit>>, f: impl FnOnce() -> T) -> T {
    let _guard = TEST_KIT_SERIAL.blocking_lock();
    set_test_kit(kit);
    let result = f();
    set_test_kit(None);
    result
}

/// The async form of [`with_test_kit`], holding the shared lock across the
/// awaited work.
#[doc(hidden)]
pub async fn with_test_kit_async<T>(
    kit: Option<Arc<WebhookTestKit>>,
    f: impl Future<Output = T>,
) -> T {
    let _guard = TEST_KIT_SERIAL.lock().await;
    set_test_kit(kit);
    let result = f.await;
    set_test_kit(None);
    result
}

/// The names of the signing keys the configured kit holds, sorted. An
/// absent kit, or one holding no keys, answers an empty list: nothing in
/// this process constrains a `secretRef` at registration time.
pub fn configured_secret_refs() -> Vec<String> {
    let Some(kit) = test_kit() else {
        return Vec::new();
    };
    let mut names: Vec<String> = kit.secrets.0.keys().cloned().collect();
    names.sort();
    names
}

/// Accept a registration-time `secretRef` against the process-wide kit.
///
/// An absent kit, or a kit holding no signing keys, accepts every name: a
/// store-now deployment records the subscription first, and the delivery
/// process resolves the name at delivery time. A kit holding keys refuses a
/// name it does not hold, so a mistyped reference fails at registration —
/// the failure this guard exists to prevent dead-lettered every delivery
/// with `secret: secret reference is not configured`.
///
/// The refusal names every configured key: the operator's failure mode is a
/// typo, and the right spellings in the message fix a typo fastest.
pub fn validate_secret_ref(reference: &str) -> Result<()> {
    let names = configured_secret_refs();
    if names.is_empty() || names.iter().any(|name| name == reference) {
        return Ok(());
    }
    Err(MCSError::InvalidParams(format!(
        "secretRef '{reference}' is not configured; this server has signing keys: {}",
        names.join(", ")
    )))
}

/// Whether the running process starts the webhooks delivery role. The admin
/// UI shows this so a stored subscription is never mistaken for a delivered
/// one: the admin section is feature-gated, the worker is role-gated, and
/// the two can disagree when the operator forgets the role.
static DELIVERY_ROLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record whether the process runs the delivery worker. Production sets this
/// once at startup from the configured roles.
pub fn set_delivery_role(running: bool) {
    DELIVERY_ROLE.store(running, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the delivery worker runs in this process.
pub fn delivery_role() -> bool {
    DELIVERY_ROLE.load(std::sync::atomic::Ordering::Relaxed)
}

/// A best-effort host for an `https://host[:port][/path]` endpoint, read with
/// plain string search rather than a URL parser.
///
/// It only has to be right for that ordinary shape:
/// [`mcpmem_webhook::validate_endpoint`] re-parses `endpoint` itself and is
/// the sole authority on whether the shape is valid. A wrong guess here can
/// only make a well-formed endpoint fail closed. It can never let a
/// malformed one through, because `validate_endpoint` checks the format
/// before it ever checks the allowlist this guess feeds.
fn guess_allowlist_host(endpoint: &str) -> Option<String> {
    let starts_with_https = endpoint
        .get(..8)
        .is_some_and(|s| s.eq_ignore_ascii_case("https://"));
    if !starts_with_https {
        return None;
    }
    let after_scheme = &endpoint[8..];
    let host_end = after_scheme
        .find(|c| "/?#:".contains(c))
        .unwrap_or(after_scheme.len());
    let host = &after_scheme[..host_end];
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Validate an endpoint's URL shape with the worker's own rule: `https`,
/// port 443, no credentials, no fragment, and a hostname rather than a
/// literal address. This calls [`mcpmem_webhook::validate_endpoint`] itself,
/// rather than a second copy of its rules, so the two can never drift apart.
///
/// The real allowlist and DNS resolution stay the worker's job at delivery
/// time: this process never holds the configured allowlist, and a
/// registration call must not depend on the network. A single-entry
/// allowlist built from [`guess_allowlist_host`], together with
/// [`PlaceholderResolver`], neutralizes those two checks so only the URL
/// shape is judged here. The admin API calls the same rule, so the MCP
/// surface and the admin surface can never accept different endpoints.
pub fn validate_endpoint_shape(endpoint: &str) -> Result<()> {
    let mut allowlist = BTreeSet::new();
    if let Some(host) = guess_allowlist_host(endpoint) {
        allowlist.insert(host);
    }
    mcpmem_webhook::validate_endpoint(endpoint, &allowlist, &PlaceholderResolver)
        .map(|_| ())
        .map_err(|e| MCSError::InvalidParams(format!("invalid webhook endpoint: {e}")))
}

/// Parse a JSON array argument into a `Vec<T>`, defaulting to empty when the
/// field is absent. `field` names the argument in an error message.
fn opt_list<T: serde::de::DeserializeOwned>(params: &Value, field: &str) -> Result<Vec<T>> {
    let items: Vec<T> = match params.get(field) {
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| MCSError::InvalidParams(format!("Invalid '{field}': {e}")))?,
        None => Vec::new(),
    };
    if items.len() > MAX_LIST_ITEMS {
        return Err(MCSError::InvalidParams(format!(
            "Too many entries in '{field}' (max {MAX_LIST_ITEMS})"
        )));
    }
    Ok(items)
}

/// Register a webhook subscription. Stores `endpoint`, the event/entity
/// filters, and `secretRef` — never secret material — and returns the
/// generated `subscriptionId`.
pub fn handle_webhook_add_subscription(args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;

    let endpoint = params
        .get("endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'endpoint' parameter".into()))?;
    validate_endpoint_shape(endpoint)?;

    let consumer_origin = params
        .get("consumerOrigin")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'consumerOrigin' parameter".into()))?;
    if consumer_origin.len() > MAX_CONSUMER_ORIGIN_BYTES {
        return Err(MCSError::InvalidParams(format!(
            "consumerOrigin too long (max {MAX_CONSUMER_ORIGIN_BYTES} bytes)"
        )));
    }

    let secret_ref = params
        .get("secretRef")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'secretRef' parameter".into()))?;
    validate_secret_ref(secret_ref)?;

    let event_operations: Vec<ChangeOperation> = opt_list(params, "eventOperations")?;
    let entity_types: Vec<String> = opt_list(params, "entityTypes")?;
    let ignored_origins: Vec<String> = opt_list(params, "ignoredOrigins")?;
    let enabled = params
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let subscription = WebhookSubscription {
        subscription_id: Uuid::new_v4(),
        endpoint: endpoint.to_owned(),
        event_operations,
        entity_types,
        ignored_origins,
        consumer_origin: consumer_origin.to_owned(),
        secret_ref: secret_ref.to_owned(),
        enabled,
    };
    let subscription_id = subscription.subscription_id;

    let conn = open_connection()?;
    SubscriptionRepository::new(&conn).upsert(subscription)?;

    let text = serde_json::to_string(&json!({ "subscriptionId": subscription_id }))
        .map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

/// Delete a webhook subscription by id. Deleting an id that does not exist is
/// not an error: `deleted` reports whether a row was actually removed.
pub fn handle_webhook_delete_subscription(args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let subscription_id = params
        .get("subscriptionId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'subscriptionId' parameter".into()))?;
    let id = Uuid::parse_str(subscription_id)
        .map_err(|e| MCSError::InvalidParams(format!("Invalid 'subscriptionId': {e}")))?;

    let conn = open_connection()?;
    let deleted = SubscriptionRepository::new(&conn).delete(id)?;

    let text = serde_json::to_string(&json!({ "subscriptionId": id, "deleted": deleted }))
        .map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ordinary shape a real subscriber sends must pass.
    #[test]
    fn accepts_an_ordinary_https_endpoint() {
        assert!(validate_endpoint_shape("https://hooks.example.test/receive").is_ok());
    }

    /// A bare host with no path exercises the branch where
    /// `guess_allowlist_host` finds no delimiter at all.
    #[test]
    fn accepts_a_bare_host_with_no_path() {
        assert!(validate_endpoint_shape("https://hooks.example.test").is_ok());
    }

    #[test]
    fn rejects_a_non_https_scheme() {
        assert!(validate_endpoint_shape("http://hooks.example.test/receive").is_err());
    }

    /// Credentials in the URL make `guess_allowlist_host` cut the host at the
    /// wrong colon; the real check must still refuse the endpoint.
    #[test]
    fn rejects_credentials_in_the_url() {
        assert!(validate_endpoint_shape("https://user:pass@hooks.example.test/receive").is_err());
    }

    #[test]
    fn rejects_a_literal_ip_host() {
        assert!(validate_endpoint_shape("https://203.0.113.5/receive").is_err());
    }

    #[test]
    fn rejects_a_malformed_url() {
        assert!(validate_endpoint_shape("not a url").is_err());
    }

    /// A kit holding exactly the named signing keys. The names double as
    /// the key bytes: a test key's content never matters, only its presence
    /// in the map does. The resolver and connector are never reached — the
    /// kit's job here is to hold the names.
    fn kit_with(secret_names: &[&str]) -> Arc<WebhookTestKit> {
        let secrets = secret_names
            .iter()
            .map(|name| {
                (
                    (*name).to_owned(),
                    mcpmem_webhook::SigningKey::new(name.as_bytes().to_vec())
                        .expect("a non-empty test key"),
                )
            })
            .collect();
        Arc::new(WebhookTestKit::for_test(
            BTreeSet::new(),
            secrets,
            Arc::new(PlaceholderResolver),
            Arc::new(mcpmem_webhook::HttpsConnector::production()),
            false,
        ))
    }

    #[test]
    fn accepts_a_configured_secret_name() {
        with_test_kit(Some(kit_with(&["github", "n8n"])), || {
            assert!(
                validate_secret_ref("n8n").is_ok(),
                "a configured name must pass"
            );
        });
    }

    /// The refusal names the available keys, so an operator with a typo sees
    /// the right spellings in the message.
    #[test]
    fn rejects_an_unknown_name_when_the_kit_has_names() {
        with_test_kit(Some(kit_with(&["n8n", "stripe"])), || {
            let message = validate_secret_ref("github")
                .expect_err("an unknown name must be refused")
                .to_string();
            assert!(
                message.contains("secretRef 'github' is not configured"),
                "{message}"
            );
            assert!(message.contains("n8n"), "{message}");
            assert!(message.contains("stripe"), "{message}");
        });
    }

    #[test]
    fn accepts_any_name_when_the_kit_has_no_secrets() {
        with_test_kit(Some(kit_with(&[])), || {
            assert!(
                validate_secret_ref("anything").is_ok(),
                "an empty kit accepts any name"
            );
        });
    }

    #[test]
    fn accepts_any_name_when_the_kit_is_absent() {
        with_test_kit(None, || {
            assert!(validate_secret_ref("anything").is_ok());
        });
    }

    /// The list the admin API publishes is sorted, so the response shape does
    /// not depend on the map's iteration order.
    #[test]
    fn configured_secret_refs_are_sorted() {
        with_test_kit(Some(kit_with(&["zeta", "alpha", "mike"])), || {
            assert_eq!(configured_secret_refs(), vec!["alpha", "mike", "zeta"]);
        });
    }

    #[test]
    fn configured_secret_refs_are_empty_without_a_kit() {
        with_test_kit(None, || {
            assert!(configured_secret_refs().is_empty());
        });
    }

    /// The guard fires before the row is written: a refused reference must
    /// leave the store untouched, or every retry would keep failing at
    /// delivery time with a row the operator thought was fixed.
    #[test]
    fn add_subscription_refuses_an_unknown_secret_and_stores_no_row() {
        // Pin this test's own live temp dir when the process lock is free;
        // `ensure_test_store` repairs the store either way.
        let dir = tempfile::tempdir().unwrap();
        init(dir.path().join("memory.db"), 5_000);
        ensure_test_store().expect("the subscription store is usable");
        let conn = open_connection().expect("the subscription store opens");

        let result = with_test_kit(Some(kit_with(&["n8n"])), || {
            handle_webhook_add_subscription(Some(&json!({
                "endpoint": "https://hooks.example.test/receive",
                "consumerOrigin": "https://example.test",
                "secretRef": "stripe",
            })))
        });

        assert!(
            result.is_err(),
            "an unknown secretRef must be refused, got {result:?}"
        );
        let rows = SubscriptionRepository::new(&conn)
            .list()
            .expect("the store lists");
        assert!(
            rows.iter().all(|row| row.secret_ref != "stripe"),
            "a refused subscription must not be stored"
        );
    }
}
