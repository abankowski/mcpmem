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

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use mcpmem_core::mutation::ChangeOperation;
use mcpmem_core::subscriptions::{SubscriptionRepository, WebhookSubscription};
use mcpmem_webhook::{Resolver, WorkerError};
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
/// reaches the shared validator.
const MAX_CONSUMER_ORIGIN_BYTES: usize = 231;

/// Cap on `eventOperations` / `entityTypes` / `ignoredOrigins` entries per
/// request. A guard against an unbounded stored JSON array, not a functional
/// limit — a real subscription needs only a few of each.
const MAX_LIST_ITEMS: usize = 256;

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
fn open_connection() -> Result<Connection> {
    let db = SUBSCRIPTION_DB.get().ok_or_else(|| {
        MCSError::MemoryError("webhook subscription store not initialized".into())
    })?;
    let conn = Connection::open(&db.path).map_err(mcpmem_core::events::sql_error)?;
    conn.busy_timeout(Duration::from_millis(db.busy_timeout_ms))
        .map_err(mcpmem_core::events::sql_error)?;
    Ok(conn)
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
/// shape is judged here.
fn validate_endpoint_shape(endpoint: &str) -> Result<()> {
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
}
