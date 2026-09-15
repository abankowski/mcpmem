//! Bounded, lease-fenced webhook delivery. Network implementation is injected.
use hmac::{Hmac, Mac};
use mcpmem_core::events::{EventDelivery, EventRepository, now_us};
use mcpmem_core::subscriptions::{SubscriptionRepository, WebhookSubscription};
use rusqlite::Connection;
use serde::Serialize;
use sha2::Sha256;
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use thiserror::Error;
use url::Url;

const LEASE_US: i64 = 30_000_000;
const MAX_ATTEMPTS: i64 = 8;
const MAX_BODY: usize = 65_536;

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("core: {0}")]
    Core(#[from] mcpmem_core::errors::MCSError),
    #[error("policy: {0}")]
    Policy(String),
    #[error("secret: {0}")]
    Secret(String),
    #[error("delivery: {0}")]
    Delivery(String),
}
#[derive(Clone, Debug)]
pub struct SigningKey(Vec<u8>);
impl SigningKey {
    pub fn new(bytes: Vec<u8>) -> Result<Self, WorkerError> {
        if bytes.is_empty() {
            Err(WorkerError::Secret("empty signing key".into()))
        } else {
            Ok(Self(bytes))
        }
    }
}
pub trait SecretProvider: Send + Sync {
    fn signing_key(&self, reference: &str) -> Result<SigningKey, WorkerError>;
}
pub trait Resolver: Send + Sync {
    fn resolve(&self, hostname: &str) -> Result<Vec<IpAddr>, WorkerError>;
}
pub struct SystemResolver;
impl Resolver for SystemResolver {
    fn resolve(&self, hostname: &str) -> Result<Vec<IpAddr>, WorkerError> {
        (hostname, 443)
            .to_socket_addrs()
            .map_err(|e| WorkerError::Policy(format!("DNS resolution failed: {e}")))
            .map(|addresses| addresses.map(|address| address.ip()).collect())
    }
}
pub struct StaticSecretProvider(pub BTreeMap<String, SigningKey>);
impl SecretProvider for StaticSecretProvider {
    fn signing_key(&self, reference: &str) -> Result<SigningKey, WorkerError> {
        self.0
            .get(reference)
            .cloned()
            .ok_or_else(|| WorkerError::Secret("secret reference is not configured".into()))
    }
}

/// The delivery policy the binary reads from its configuration file. The
/// allowlist names the HTTPS hostnames the worker may deliver to; a `secret`
/// reference maps to an already-loaded signing key. Empty is the fail-closed
/// default: no allowed host and no key means the worker refuses everything.
#[derive(Clone, Debug, Default)]
pub struct WebhookConfigFile {
    pub allowlist: BTreeSet<String>,
    pub secrets: BTreeMap<String, SigningKey>,
    /// `true` relaxes the address-class check: an allowlisted host may
    /// resolve to a private, loopback or link-local address. Strict by
    /// default (fail-closed against SSRF); an operator with split-horizon
    /// DNS opts in explicitly.
    pub allow_private_addresses: bool,
}
#[derive(Clone, Debug)]
pub struct ValidatedEndpoint {
    pub url: Url,
    pub address: SocketAddr,
}
#[derive(Clone, Debug)]
pub struct SignedRequest {
    pub body: Vec<u8>,
    pub event_id: String,
    pub timestamp_us: i64,
    pub signature: String,
}
#[derive(Clone, Copy, Debug)]
pub struct DeliveryResponse {
    pub status: u16,
    pub retry_after_us: Option<i64>,
}
pub trait DeliveryConnector: Send + Sync {
    fn send(
        &self,
        endpoint: &ValidatedEndpoint,
        request: SignedRequest,
    ) -> Result<DeliveryResponse, WorkerError>;
}
pub trait HttpsTransport: Send + Sync {
    fn post(
        &self,
        endpoint: &ValidatedEndpoint,
        request: SignedRequest,
    ) -> Result<DeliveryResponse, WorkerError>;
}
pub struct ReqwestTransport;
#[derive(Clone, Debug)]
pub struct RequestPlan {
    pub host: String,
    pub address: SocketAddr,
    pub follow_redirects: bool,
    pub idempotency_key: String,
    pub timestamp: String,
    pub signature: String,
}
pub fn request_plan(
    endpoint: &ValidatedEndpoint,
    request: &SignedRequest,
) -> Result<RequestPlan, WorkerError> {
    Ok(RequestPlan {
        host: endpoint
            .url
            .host_str()
            .ok_or_else(|| WorkerError::Policy("validated endpoint lacks hostname".into()))?
            .to_owned(),
        address: endpoint.address,
        follow_redirects: false,
        idempotency_key: request.event_id.clone(),
        timestamp: request.timestamp_us.to_string(),
        signature: request.signature.clone(),
    })
}
impl HttpsTransport for ReqwestTransport {
    fn post(
        &self,
        endpoint: &ValidatedEndpoint,
        request: SignedRequest,
    ) -> Result<DeliveryResponse, WorkerError> {
        let plan = request_plan(endpoint, &request)?;
        let client = reqwest::blocking::Client::builder()
            .redirect(if plan.follow_redirects {
                reqwest::redirect::Policy::limited(10)
            } else {
                reqwest::redirect::Policy::none()
            })
            .resolve(&plan.host, plan.address)
            .build()
            .map_err(|e| WorkerError::Delivery(e.to_string()))?;
        let response = client
            .post(endpoint.url.clone())
            .header("Idempotency-Key", plan.idempotency_key)
            .header("X-Memory-Timestamp", plan.timestamp)
            .header("X-Memory-Signature", plan.signature)
            .body(request.body)
            .send()
            .map_err(|e| WorkerError::Delivery(e.to_string()))?;
        let retry_after_us = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok())
            .map(|seconds| seconds.saturating_mul(1_000_000));
        Ok(DeliveryResponse {
            status: response.status().as_u16(),
            retry_after_us,
        })
    }
}
pub struct HttpsConnector<T = ReqwestTransport>(pub T);
impl HttpsConnector {
    pub const fn production() -> Self {
        Self(ReqwestTransport)
    }
}
impl<T: HttpsTransport> DeliveryConnector for HttpsConnector<T> {
    fn send(
        &self,
        endpoint: &ValidatedEndpoint,
        request: SignedRequest,
    ) -> Result<DeliveryResponse, WorkerError> {
        self.0.post(endpoint, request)
    }
}

pub fn validate_endpoint(
    endpoint: &str,
    allowlist: &BTreeSet<String>,
    resolver: &dyn Resolver,
) -> Result<ValidatedEndpoint, WorkerError> {
    validate_endpoint_inner(endpoint, allowlist, resolver, false)
}

/// The strict rules of [`validate_endpoint`], except the resolved addresses
/// may be private, loopback or link-local. An operator who runs split-horizon
/// DNS — a domain that is public on the internet but resolves to a LAN
/// address inside the network (router hairpin NAT) — opts into this per
/// config file. The allowlist, https, port 443, the per-attempt DNS pinning
/// and the redirect refusal all stay untouched.
pub fn validate_endpoint_allowing_private(
    endpoint: &str,
    allowlist: &BTreeSet<String>,
    resolver: &dyn Resolver,
) -> Result<ValidatedEndpoint, WorkerError> {
    validate_endpoint_inner(endpoint, allowlist, resolver, true)
}

fn validate_endpoint_inner(
    endpoint: &str,
    allowlist: &BTreeSet<String>,
    resolver: &dyn Resolver,
    allow_private: bool,
) -> Result<ValidatedEndpoint, WorkerError> {
    let url = Url::parse(endpoint).map_err(|e| WorkerError::Policy(e.to_string()))?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || url.username() != ""
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
        || url
            .host_str()
            .is_some_and(|host| host.parse::<IpAddr>().is_ok())
    {
        return Err(WorkerError::Policy(
            "endpoint must be https on port 443, with a hostname (not an IP address), no credentials, no fragment; a URL path is allowed".into(),
        ));
    }
    let hostname = url.host_str().expect("checked hostname");
    if !allowlist.contains(hostname) {
        return Err(WorkerError::Policy(
            "endpoint hostname is not allowlisted".into(),
        ));
    }
    let addresses = resolver.resolve(hostname)?;
    let ip = addresses
        .first()
        .copied()
        .ok_or_else(|| WorkerError::Policy("endpoint resolution returned no address".into()))?;
    if !allow_private && !is_public(ip) {
        return Err(WorkerError::Policy(
            "endpoint resolved to non-public address; set [webhooks] allow-private-addresses = true to permit a split-horizon DNS topology".into(),
        ));
    }
    if !allow_private && !addresses.iter().copied().all(is_public) {
        return Err(WorkerError::Policy(
            "endpoint resolution mixes public and non-public addresses; set [webhooks] allow-private-addresses = true to permit a split-horizon DNS topology".into(),
        ));
    }
    Ok(ValidatedEndpoint {
        url,
        address: SocketAddr::new(ip, 443),
    })
}
const fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            !(v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_broadcast()
                || v.is_unspecified()
                || v.is_multicast()
                || v.octets()[0] == 0
                || v.octets()[0] >= 224)
        }
        IpAddr::V6(v) => {
            !(v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || v.is_unique_local()
                || v.is_unicast_link_local())
        }
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct DeliveryReport {
    pub claimed: usize,
    pub completed: usize,
    pub retried: usize,
    pub dead: usize,
}
pub struct WebhookWorker<C, S, R> {
    database: PathBuf,
    connector: C,
    secrets: S,
    allowlist: BTreeSet<String>,
    resolver: R,
    lease_us: i64,
    allow_private_addresses: bool,
}
pub trait WorkerPoll: Send + Sync {
    fn poll(&self, now_us: i64) -> Result<DeliveryReport, WorkerError>;
    /// One-time startup audit of the registered subscriptions against the
    /// delivery-time policy. The default reports nothing, so a test fake
    /// keeps compiling.
    fn audit_subscriptions(&self) -> Result<Vec<SubscriptionAudit>, WorkerError> {
        Ok(Vec::new())
    }
}

/// The startup audit outcome for one registered subscription.
#[derive(Clone, Debug)]
pub struct SubscriptionAudit {
    pub subscription_id: String,
    pub endpoint: String,
    pub secret_ref: String,
    pub enabled: bool,
    /// `Err(reason)` when the delivery-time policy would refuse every
    /// attempt: the host is not allowlisted, the address checks fail, or
    /// the secret reference is not configured.
    pub outcome: std::result::Result<(), String>,
}
impl<C: DeliveryConnector, S: SecretProvider, R: Resolver> WebhookWorker<C, S, R> {
    pub fn new(
        database: impl AsRef<Path>,
        connector: C,
        secrets: S,
        allowlist: BTreeSet<String>,
        resolver: R,
    ) -> Self {
        Self {
            database: database.as_ref().to_path_buf(),
            connector,
            secrets,
            allowlist,
            resolver,
            lease_us: LEASE_US,
            allow_private_addresses: false,
        }
    }
    pub const fn with_lease_us(mut self, lease_us: i64) -> Self {
        self.lease_us = lease_us;
        self
    }
    /// Relax the address-class check for every endpoint this worker
    /// delivers to. The allowlist, https, port, DNS pinning and redirect
    /// refusal stay in force; only the "must resolve to a public address"
    /// requirement is skipped.
    pub const fn with_allow_private_addresses(mut self, allow: bool) -> Self {
        self.allow_private_addresses = allow;
        self
    }
    /// Validate the endpoint with this worker's address policy.
    fn policy_endpoint(&self, endpoint: &str) -> Result<ValidatedEndpoint, WorkerError> {
        if self.allow_private_addresses {
            validate_endpoint_allowing_private(endpoint, &self.allowlist, &self.resolver)
        } else {
            validate_endpoint(endpoint, &self.allowlist, &self.resolver)
        }
    }
    /// Audit every registered subscription against the delivery-time policy,
    /// without delivering anything. The supervisor logs the outcome once at
    /// startup, so a subscription that can never be delivered is visible
    /// before the first event arrives.
    pub fn audit_subscriptions(&self) -> Result<Vec<SubscriptionAudit>, WorkerError> {
        let conn = Connection::open(&self.database)?;
        mcpmem_core::schema::initialize_database(&conn)?;
        let subscriptions = SubscriptionRepository::new(&conn).list()?;
        Ok(subscriptions.into_iter().map(|s| self.audit(s)).collect())
    }
    fn audit(&self, subscription: WebhookSubscription) -> SubscriptionAudit {
        let outcome = if !subscription.enabled {
            Err("subscription is disabled".into())
        } else {
            self.policy_endpoint(&subscription.endpoint)
                .map(|_| ())
                .and_then(|_| {
                    self.secrets
                        .signing_key(&subscription.secret_ref)
                        .map(|_| ())
                })
                .map_err(|e| e.to_string())
        };
        SubscriptionAudit {
            subscription_id: subscription.subscription_id.to_string(),
            endpoint: subscription.endpoint,
            secret_ref: subscription.secret_ref,
            enabled: subscription.enabled,
            outcome,
        }
    }
    pub fn run_once(&self, now: i64) -> Result<DeliveryReport, WorkerError> {
        let conn = Connection::open(&self.database)?;
        mcpmem_core::schema::initialize_database(&conn)?;
        let events = EventRepository::new(&conn);
        let Some(delivery) = events.claim_due(now, self.lease_us)? else {
            return Ok(DeliveryReport::default());
        };
        let mut report = DeliveryReport {
            claimed: 1,
            ..DeliveryReport::default()
        };
        let Some(subscription) = SubscriptionRepository::new(&conn)
            .get(delivery.subscription_id)?
            .filter(|s| s.enabled)
        else {
            // A missing or disabled subscription cannot be delivered and
            // never will be: dead-letter the claim, as the delivery-time
            // policy arm does. The row keeps its state visible to the
            // operator instead of hanging as a leased claim forever.
            tracing::warn!(
                subscription_id = %delivery.subscription_id,
                event_id = %delivery.event.event_id,
                "webhook discarded: subscription missing or disabled"
            );
            if events.retry(
                &delivery,
                now_us(),
                now.saturating_add(1_000_000),
                "policy: subscription missing or disabled",
                true,
            )? {
                report.dead = 1;
            }
            return Ok(report);
        };
        tracing::debug!(
            subscription_id = %delivery.subscription_id,
            event_id = %delivery.event.event_id,
            attempts = delivery.attempts,
            "webhook delivery claimed"
        );
        let outcome = self.deliver(&subscription, &delivery, now);
        match outcome {
            Ok(response) if (200..300).contains(&response.status) => {
                tracing::info!(
                    subscription_id = %delivery.subscription_id,
                    endpoint = %subscription.endpoint,
                    event_id = %delivery.event.event_id,
                    status = response.status,
                    "webhook delivered"
                );
                if events.complete(&delivery, now_us())? {
                    report.completed = 1;
                }
            }
            Ok(response) => {
                let dead =
                    !(response.status == 408 || response.status == 429 || response.status >= 500)
                        || delivery.attempts >= MAX_ATTEMPTS;
                let delay = response
                    .retry_after_us
                    .unwrap_or(1_000_000)
                    .clamp(1_000_000, 3_600_000_000);
                if events.retry(
                    &delivery,
                    now_us(),
                    now.saturating_add(delay),
                    &format!("http {}", response.status),
                    dead,
                )? {
                    if dead {
                        tracing::warn!(
                            subscription_id = %delivery.subscription_id,
                            endpoint = %subscription.endpoint,
                            event_id = %delivery.event.event_id,
                            status = response.status,
                            attempts = delivery.attempts,
                            "webhook dead-lettered"
                        );
                        report.dead = 1
                    } else {
                        tracing::warn!(
                            subscription_id = %delivery.subscription_id,
                            endpoint = %subscription.endpoint,
                            event_id = %delivery.event.event_id,
                            status = response.status,
                            retry_delay_us = delay,
                            "webhook returned a retryable status"
                        );
                        report.retried = 1
                    }
                }
            }
            Err(error) => {
                let dead = delivery.attempts >= MAX_ATTEMPTS
                    || matches!(error, WorkerError::Policy(_) | WorkerError::Secret(_));
                match &error {
                    // A policy or secret refusal cannot succeed on a retry:
                    // the endpoint is refused, or the signing key is missing.
                    // The discard is the message an operator needs.
                    WorkerError::Policy(_) | WorkerError::Secret(_) => tracing::warn!(
                        subscription_id = %delivery.subscription_id,
                        endpoint = %subscription.endpoint,
                        event_id = %delivery.event.event_id,
                        %error,
                        "webhook discarded"
                    ),
                    _ => tracing::error!(
                        subscription_id = %delivery.subscription_id,
                        endpoint = %subscription.endpoint,
                        event_id = %delivery.event.event_id,
                        %error,
                        "webhook delivery attempt failed"
                    ),
                }
                if events.retry(
                    &delivery,
                    now_us(),
                    now.saturating_add(1_000_000),
                    &error.to_string(),
                    dead,
                )? {
                    if dead {
                        report.dead = 1
                    } else {
                        report.retried = 1
                    }
                }
            }
        }
        Ok(report)
    }
    fn deliver(
        &self,
        subscription: &WebhookSubscription,
        delivery: &EventDelivery,
        now: i64,
    ) -> Result<DeliveryResponse, WorkerError> {
        let endpoint = self.policy_endpoint(&subscription.endpoint)?;
        let body = envelope(delivery)?;
        let key = self.secrets.signing_key(&subscription.secret_ref)?;
        let signature = signature(&key, now, &body)?;
        self.connector.send(
            &endpoint,
            SignedRequest {
                body,
                event_id: delivery.event.event_id.to_string(),
                timestamp_us: now,
                signature,
            },
        )
    }
}
impl<C: DeliveryConnector, S: SecretProvider, R: Resolver> WorkerPoll for WebhookWorker<C, S, R> {
    fn poll(&self, now_us: i64) -> Result<DeliveryReport, WorkerError> {
        self.run_once(now_us)
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Envelope<'a> {
    version: u8,
    event_id: String,
    transaction_id: String,
    entity_id: i64,
    entity_revision: i64,
    operation: mcpmem_core::mutation::ChangeOperation,
    occurred_at_us: i64,
    origin: &'a str,
    correlation_id: String,
    causation_id: Option<String>,
    hop_count: u8,
    old_name: Option<&'a str>,
    new_name: Option<&'a str>,
}
fn envelope(delivery: &EventDelivery) -> Result<Vec<u8>, WorkerError> {
    let event = &delivery.event;
    let body = serde_json::to_vec(&Envelope {
        version: 2,
        event_id: event.event_id.to_string(),
        transaction_id: event.transaction_id.to_string(),
        entity_id: event.entity_id,
        entity_revision: event.entity_revision,
        operation: event.change.operation,
        occurred_at_us: event.occurred_at_us,
        origin: &event.provenance.origin,
        correlation_id: event.provenance.correlation_id.to_string(),
        causation_id: event.provenance.causation_id.map(|id| id.to_string()),
        hop_count: event.provenance.hop_count,
        old_name: (event.change.operation == mcpmem_core::mutation::ChangeOperation::Rename)
            .then_some(event.change.old_name.as_deref())
            .flatten(),
        new_name: (event.change.operation == mcpmem_core::mutation::ChangeOperation::Rename)
            .then_some(event.change.new_name.as_deref())
            .flatten(),
    })
    .map_err(mcpmem_core::errors::MCSError::from)?;
    if body.len() > MAX_BODY {
        return Err(WorkerError::Policy("envelope exceeds 64KiB".into()));
    }
    Ok(body)
}
fn signature(key: &SigningKey, timestamp: i64, body: &[u8]) -> Result<String, WorkerError> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(&key.0).map_err(|e| WorkerError::Secret(e.to_string()))?;
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    Ok(hex(mac.finalize().into_bytes().as_slice()))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Deliver one signed test event to a subscription's endpoint. It runs the
/// delivery-time policy (allowlist, DNS, public address), signs with the
/// subscription's secret reference, and posts through the connector. It
/// writes no outbox row and changes no delivery state; the caller reports
/// the response. The admin UI's Test button uses this seam.
pub fn deliver_test(
    connector: &dyn DeliveryConnector,
    secrets: &dyn SecretProvider,
    allowlist: &BTreeSet<String>,
    resolver: &dyn Resolver,
    allow_private: bool,
    subscription: &WebhookSubscription,
) -> Result<DeliveryResponse, WorkerError> {
    let endpoint = if allow_private {
        validate_endpoint_allowing_private(&subscription.endpoint, allowlist, resolver)?
    } else {
        validate_endpoint(&subscription.endpoint, allowlist, resolver)?
    };
    let key = secrets.signing_key(&subscription.secret_ref)?;
    let now = now_us();
    let (body, event_id) = test_envelope(now)?;
    let signature = signature(&key, now, &body)?;
    connector.send(
        &endpoint,
        SignedRequest {
            body,
            event_id,
            timestamp_us: now,
            signature,
        },
    )
}

/// A test envelope in the same shape as a delivery, marked by the `test-`
/// prefix in `event_id` and the `admin:webhook-test` origin. A receiver can
/// recognize the event and ignore it.
fn test_envelope(now: i64) -> Result<(Vec<u8>, String), WorkerError> {
    let event_id = format!("test-{now}");
    let body = serde_json::to_vec(&serde_json::json!({
        "version": 2,
        "eventId": event_id,
        "transactionId": event_id,
        "entityId": 0,
        "entityRevision": 0,
        "operation": "create",
        "occurredAtUs": now,
        "origin": "admin:webhook-test",
        "correlationId": event_id,
        "causationId": null,
        "hopCount": 0,
        "oldName": null,
        "newName": null,
    }))
    .map_err(|e| WorkerError::Delivery(e.to_string()))?;
    if body.len() > MAX_BODY {
        return Err(WorkerError::Policy("test envelope exceeds 64KiB".into()));
    }
    Ok((body, event_id))
}
