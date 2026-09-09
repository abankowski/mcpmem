//! Bounded, lease-fenced webhook delivery. Network implementation is injected.
use hmac::{Hmac, Mac};
use memory_core::events::{EventDelivery, EventRepository, now_us};
use memory_core::subscriptions::{SubscriptionRepository, WebhookSubscription};
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
    Core(#[from] memory_core::errors::MCSError),
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
            "endpoint must be https, port 443, hostname-only, without fragment".into(),
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
    if !is_public(ip) {
        return Err(WorkerError::Policy(
            "endpoint resolved to non-public address".into(),
        ));
    }
    if !addresses.iter().copied().all(is_public) {
        return Err(WorkerError::Policy(
            "endpoint resolution contains non-public address".into(),
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
}
pub trait WorkerPoll: Send + Sync {
    fn poll(&self, now_us: i64) -> Result<DeliveryReport, WorkerError>;
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
        }
    }
    pub const fn with_lease_us(mut self, lease_us: i64) -> Self {
        self.lease_us = lease_us;
        self
    }
    pub fn run_once(&self, now: i64) -> Result<DeliveryReport, WorkerError> {
        let conn = Connection::open(&self.database)?;
        memory_core::events::migrate(&conn)?;
        let events = EventRepository::new(&conn);
        let Some(delivery) = events.claim_due(now, self.lease_us)? else {
            return Ok(DeliveryReport::default());
        };
        let mut report = DeliveryReport {
            claimed: 1,
            ..DeliveryReport::default()
        };
        let outcome = SubscriptionRepository::new(&conn)
            .get(delivery.subscription_id)?
            .filter(|s| s.enabled)
            .ok_or_else(|| WorkerError::Policy("subscription missing or disabled".into()))
            .and_then(|subscription| self.deliver(&subscription, &delivery, now));
        match outcome {
            Ok(response) if (200..300).contains(&response.status) => {
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
                        report.dead = 1
                    } else {
                        report.retried = 1
                    }
                }
            }
            Err(error) => {
                let dead = delivery.attempts >= MAX_ATTEMPTS
                    || matches!(error, WorkerError::Policy(_) | WorkerError::Secret(_));
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
        let endpoint = validate_endpoint(&subscription.endpoint, &self.allowlist, &self.resolver)?;
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
    operation: memory_core::mutation::ChangeOperation,
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
        old_name: (event.change.operation == memory_core::mutation::ChangeOperation::Rename)
            .then_some(event.change.old_name.as_deref())
            .flatten(),
        new_name: (event.change.operation == memory_core::mutation::ChangeOperation::Rename)
            .then_some(event.change.new_name.as_deref())
            .flatten(),
    })
    .map_err(memory_core::errors::MCSError::from)?;
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
