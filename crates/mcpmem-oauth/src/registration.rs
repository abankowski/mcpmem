//! The two ways a client becomes known to this server.
//!
//! [`register`] is RFC 7591 dynamic client registration: the client posts its
//! metadata and this server issues the identifier. [`resolve_metadata_document`]
//! is the client identifier metadata document: the client publishes its
//! metadata at an https URL and presents that URL as its identifier.
//!
//! Both paths end in one [`ClientRecord`], and both apply one redirect-URI
//! rule. A registration is unauthenticated, so nothing here may trust a value
//! the request chose: the identifier, the source and both timestamps are
//! server-controlled, and [`RegistrationRequest`] is the only shape a request
//! body deserializes into.

use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

use crate::store::{ClientRecord, Store, StoreError};

/// Fetch one document over HTTP. The trait exists so a test supplies a
/// document without a network, and so the transport is chosen once, by the
/// caller that owns the process's HTTP client.
///
/// # Implementation requirements
///
/// [`resolve_metadata_document`] checks the scheme and the host before it
/// calls `get`, and nothing after that check constrains the request. An
/// implementation must therefore enforce the rest itself:
///
/// - `https` only, and no redirect followed — a redirect moves the response
///   off the host this server just authorized;
/// - a 64 KB body cap, and a 5 second timeout — the URL is client-chosen, so
///   the host may stream forever or never answer.
///
/// No implementation ships in this crate: `mcpmem-oauth` is a non-optional
/// dependency of `mcpmem`, and CI requires the graph-only build
/// (`cargo tree --no-default-features`) to carry no HTTP client
/// (`.github/workflows/ci.yml`). The real fetcher therefore arrives with the
/// upstream provider, behind a cargo feature, and never as a plain dependency
/// of this crate.
pub trait Fetch: Send + Sync {
    /// The body of `url`, or a message describing why it could not be read.
    fn get(&self, url: &str) -> Result<String, String>;
}

/// Why a registration was refused.
///
/// The first two arise from a registration request, the next four from a
/// metadata document, and the last from the store. `src/oauth_routes.rs` maps
/// each one to an RFC 7591 section 3.2.2 error code.
#[derive(Debug, Error)]
pub enum RegistrationError {
    #[error("the registration request is not usable client metadata")]
    InvalidClientMetadata,
    #[error("every redirect URI must use https, or be a loopback http URL")]
    InvalidRedirectUri,
    #[error("the metadata document URL is not an https URL on an allowed domain")]
    DomainNotAllowed,
    #[error("the metadata document is not usable client metadata")]
    MalformedDocument,
    #[error("the metadata document names a client_id other than its own URL")]
    MetadataMismatch,
    #[error("the metadata document could not be fetched: {0}")]
    Fetch(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Everything a registration request is allowed to choose.
///
/// Unknown members are ignored rather than refused, which is what makes this
/// type the guard it is: a body carrying `client_id`, `source` or `created_us`
/// registers a client with a server-issued identifier and server-set
/// timestamps, exactly as a body without them does. [`ClientRecord`] itself
/// does not implement `Deserialize`, so no other shape can appear here.
///
/// `client_name` is optional in RFC 7591 section 2 and stays optional here: a
/// client that sends none registers with an empty name. A consent screen must
/// therefore be ready to name a client by its identifier.
#[derive(Debug, Deserialize)]
struct RegistrationRequest {
    #[serde(default)]
    client_name: String,
    #[serde(default)]
    redirect_uris: Vec<String>,
}

/// The three members a client identifier metadata document must carry. All
/// three are required: a document is the client's own publication, and one
/// that omits any of them cannot be shown to a human or redirected to.
#[derive(Debug, Deserialize)]
struct MetadataDocument {
    client_id: String,
    client_name: String,
    redirect_uris: Vec<String>,
}

/// Register a client and return the RFC 7591 section 3.2.1 response body.
///
/// `now_us` is microseconds since the epoch, the unit the store keeps.
/// `client_id_issued_at` is seconds, the unit RFC 7591 states.
///
/// The response reports what this server does, not what the request asked
/// for. There is one grant set, one response type and no client secret, so
/// `grant_types`, `response_types` and `token_endpoint_auth_method` are stated
/// rather than echoed: RFC 7591 section 3.2.1 admits that, and echoing a
/// method this server does not implement would be a lie a client acts on.
pub fn register(store: &Store, body: &Value, now_us: i64) -> Result<Value, RegistrationError> {
    let request = RegistrationRequest::deserialize(body)
        .map_err(|_| RegistrationError::InvalidClientMetadata)?;
    if request.redirect_uris.is_empty() {
        return Err(RegistrationError::InvalidClientMetadata);
    }
    check_redirect_uris(&request.redirect_uris)?;

    let record = ClientRecord {
        client_id: crate::new_token(),
        client_name: request.client_name,
        redirect_uris: request.redirect_uris,
        source: ClientRecord::DCR.to_string(),
        created_us: now_us,
        last_used_us: now_us,
    };
    if !within_bounds(&record) {
        return Err(RegistrationError::InvalidClientMetadata);
    }
    store.put_client(&record)?;

    Ok(json!({
        "client_id": record.client_id,
        "client_id_issued_at": now_us / 1_000_000,
        "client_name": record.client_name,
        "redirect_uris": record.redirect_uris,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none"
    }))
}

/// Fetch and validate the client identifier metadata document at `url`, and
/// return the client it describes.
///
/// The order matters. The scheme and the host are checked first, and a URL
/// that fails either check produces no request at all: `url` arrives from an
/// authorization request, so a fetch before the check would make this server
/// an open proxy that any client can point at any host.
///
/// `client_id` must equal `url` byte for byte. That equality is the whole
/// binding between the identifier a client presents and the metadata this
/// server acts on; without it a document on an allowed domain could claim any
/// identifier, including one already registered.
///
/// # `allowed_domains`
///
/// One entry per host, matched as a **whole** host and case-insensitively.
/// `claude.ai` admits `https://claude.ai/…` and refuses
/// `https://auth.claude.ai/…`. A subdomain needs its own entry. A suffix
/// match would admit `claude.ai.evil.example`, which is why the rule is what
/// it is; the cost is that an operator must list each host the vendor
/// publishes on. `--cimd-allowed-domain` states the same rule.
pub fn resolve_metadata_document(
    url: &str,
    allowed_domains: &[String],
    fetch: &dyn Fetch,
    now_us: i64,
) -> Result<ClientRecord, RegistrationError> {
    let Some(host) = host(url, "https://") else {
        return Err(RegistrationError::DomainNotAllowed);
    };
    if !allowed_domains
        .iter()
        .any(|domain| domain.eq_ignore_ascii_case(host))
    {
        return Err(RegistrationError::DomainNotAllowed);
    }
    // Before the fetch, not only in `within_bounds` below. `url` is the
    // `client_id` an anonymous authorization request chose, and a URL past the
    // cap is refused whatever the document at it says — so paying for the
    // request first would let a caller spend this server's time and an
    // allow-listed host's bandwidth on a record that can never be stored.
    if url.len() > MAX_URL_BYTES {
        return Err(RegistrationError::MalformedDocument);
    }

    let body = fetch.get(url).map_err(RegistrationError::Fetch)?;
    let document: MetadataDocument =
        serde_json::from_str(&body).map_err(|_| RegistrationError::MalformedDocument)?;
    if document.client_id != url {
        return Err(RegistrationError::MetadataMismatch);
    }
    if document.client_name.is_empty() || document.redirect_uris.is_empty() {
        return Err(RegistrationError::MalformedDocument);
    }
    check_redirect_uris(&document.redirect_uris)?;

    let record = ClientRecord {
        client_id: document.client_id,
        client_name: document.client_name,
        redirect_uris: document.redirect_uris,
        source: ClientRecord::CIMD.to_string(),
        created_us: now_us,
        last_used_us: now_us,
    };
    if !within_bounds(&record) {
        return Err(RegistrationError::MalformedDocument);
    }
    Ok(record)
}

/// Every entry, not just the first: one plain-http entry in a list is the whole
/// interception the rule exists to prevent.
fn check_redirect_uris(uris: &[String]) -> Result<(), RegistrationError> {
    for uri in uris {
        if !is_acceptable_redirect_uri(uri) {
            return Err(RegistrationError::InvalidRedirectUri);
        }
    }
    Ok(())
}

/// An `https` URL, or a loopback `http` URL as RFC 8252 section 7.3 allows for
/// a native client that listens on an ephemeral port. Every other plain-http
/// URL is refused: the authorization code would cross the network in clear.
///
/// A URI carrying a fragment is refused, whatever its scheme. RFC 6749 section
/// 3.1.2 forbids one, and the consequence is not cosmetic: the comparison
/// against a presented `redirect_uri` is an exact string match, so a
/// registered fragment reaches the redirect this server builds. Appending
/// `?code=…` to `https://claude.ai/cb#f` puts the query inside the fragment,
/// where no browser sends it, and the client waits for a code it never
/// receives. A query component is legal and stays legal.
///
/// The loopback names are matched whole. A host that merely ends in
/// `localhost` — `evil.localhost` — is a public name someone else can own.
///
/// # The half of RFC 8252 section 7.3 this server declines
///
/// That section has two rules. This function follows the first: a loopback
/// `http` redirect URI is acceptable. The comparison against a presented
/// `redirect_uri` declines the second, which says an authorization server
/// "MUST allow any port to be specified at the time of the request" — here the
/// port is compared like every other byte.
///
/// The reason is that registration is dynamic and unauthenticated (RFC 7591),
/// so a native client registers *after* it has bound its port, one request
/// before it starts the authorization request. It can always register the URI
/// it will present, and one exact comparison is then the whole rule with
/// nothing to reason about.
///
/// The price is real and belongs next to the decision. This server implements
/// no RFC 7592 client management — the registration response carries no
/// `registration_access_token` and no `registration_client_uri` — so a client
/// cannot update its redirect URIs. A client that persists its `client_id`
/// across restarts and binds a fresh ephemeral port on the next run presents a
/// URI that identifier never registered, and gets `redirect_uri is not
/// registered for this client`. The recovery is to register again, which such a
/// client must therefore do once per port.
///
/// Do not relax the comparison to "comply with 7.3" without revisiting this: it
/// is a decision, not an oversight. `a_loopback_redirect_uri_differing_only_in_port_is_refused`
/// in `tests/oauth_consent.rs` is the test that records it.
fn is_acceptable_redirect_uri(uri: &str) -> bool {
    if uri.contains('#') {
        return false;
    }
    if host(uri, "https://").is_some() {
        return true;
    }
    host(uri, "http://")
        .is_some_and(|h| h == "127.0.0.1" || h == "[::1]" || h.eq_ignore_ascii_case("localhost"))
}

/// The longest `client_name` this server stores, in bytes.
const MAX_CLIENT_NAME_BYTES: usize = 256;
/// The most redirect URIs one client may register.
const MAX_REDIRECT_URIS: usize = 8;
/// The longest URL this server stores, in bytes: the conventional practical
/// URL limit. Every URL in a client record is bounded by it — each redirect
/// URI, and the `client_id` itself, which on the metadata-document path is the
/// document URL. One number, because one reason.
const MAX_URL_BYTES: usize = 2048;

/// Whether one client record fits in one reasonable row.
///
/// The argument is the record, not the fields, and that is the point: every
/// text a client chose is bounded here, and no call site can bound two of the
/// three and forget the third. `client_id` is server-issued on the
/// registration path and client-chosen on the metadata-document path, where it
/// is the document URL.
///
/// Registration is unauthenticated, so the size of one row is a cost a
/// stranger chooses, and nothing reclaims a row for thirty days —
/// [`Store::evict_clients`] is the only thing that does, and only for a client
/// that is idle and holds no token. The only cap above this point is the
/// transport's global body limit, which is 16 MiB and bounds a request rather
/// than a row; `crate::limits` bounds how many rows arrive, not how large one
/// is.
///
/// The `client_name` is also the text a consent screen asks a human to trust,
/// and an unbounded string is not that.
///
/// The caps are byte counts, not character counts: bytes are what the database
/// stores, and a multi-byte name is not entitled to more of them.
fn within_bounds(record: &ClientRecord) -> bool {
    record.client_id.len() <= MAX_URL_BYTES
        && record.client_name.len() <= MAX_CLIENT_NAME_BYTES
        && record.redirect_uris.len() <= MAX_REDIRECT_URIS
        && record
            .redirect_uris
            .iter()
            .all(|uri| uri.len() <= MAX_URL_BYTES)
}

/// The host of `url` when it uses `scheme`, or `None`.
///
/// `scheme` carries its `://`. The authority is read up to the first `/`, `?`
/// or `#`, so nothing in a path or a query is mistaken for a host.
///
/// An authority carrying userinfo is refused outright rather than parsed.
/// `https://claude.ai@evil.example/c.json` has host `evil.example`, and its
/// only purpose is to read as an allowed domain; no legitimate redirect URI or
/// metadata document URL carries credentials.
fn host<'a>(url: &'a str, scheme: &str) -> Option<&'a str> {
    let authority = url
        .strip_prefix(scheme)?
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('@') {
        return None;
    }
    // Drop a port, and leave a bracketed IPv6 literal whole: the last colon of
    // `[::1]` is not a port separator, and what follows it is not a number.
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    };
    (!host.is_empty()).then_some(host)
}
