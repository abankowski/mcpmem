//! The upstream OpenID Connect provider that authenticates the human.
//!
//! This server issues its own tokens; it never forwards an upstream one. The
//! provider's only job is to answer one question — *who is at the browser* —
//! and the answer is an identity token this module verifies and reduces to
//! [`IdentityClaims`]. Nothing else from the provider is kept: the upstream
//! access token is read and dropped.
//!
//! Everything here is behind the crate feature `upstream`. `mcpmem` depends on
//! this crate with no feature gate, and CI requires the graph-only tree
//! (`cargo tree --no-default-features -e normal`) to carry no HTTP client, so
//! the client may only arrive through a feature that a `--no-default-features`
//! build drops. See `.github/workflows/ci.yml`.
//!
//! # What is verified, and why each one
//!
//! - **The signature**, against the key the token names, fetched from the
//!   provider's own key set. Without it every claim below is a client-supplied
//!   string.
//! - **`iss`**, against the issuer the discovery document named — which is
//!   itself checked against the issuer the operator configured. A principal is
//!   keyed by `iss` plus `sub`, so a token from another provider claiming a
//!   known `sub` is exactly the confusion this check refuses.
//! - **`aud`**, against this server's client identifier. A token minted for
//!   another client of the same provider is not a login here.
//! - **`exp`**, with no leeway. The window is the login window, and it is
//!   minutes wide; a minute of grace on top of it buys nothing.
//! - **`nonce`**, against the value stored on the login row. This is what binds
//!   the identity token to the authorization request this server started, and
//!   it is why [`Provider::exchange`] takes the expected nonce rather than
//!   leaving the comparison to its caller.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

/// The longest document this module reads from a provider. A key set is a few
/// kilobytes and a discovery document less; the cap exists so a host that
/// streams forever cannot exhaust this process.
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;

/// How long any one request to the provider may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Why an upstream login failed.
///
/// The claim failures are separate variants rather than one `InvalidToken`
/// because each one means something different to the operator reading the log:
/// [`UpstreamError::Audience`] is a misconfigured client identifier,
/// [`UpstreamError::Nonce`] is a replayed or crossed login, and
/// [`UpstreamError::Signature`] is a forgery. None of them reaches the human,
/// whose page says only that the login failed.
#[derive(Debug, Error)]
pub enum UpstreamError {
    #[error("the provider could not be reached: {0}")]
    Transport(String),
    #[error("the provider published no usable discovery document: {0}")]
    Discovery(String),
    #[error("the token endpoint refused the exchange: {0}")]
    TokenEndpoint(String),
    #[error("the identity token is malformed: {0}")]
    Malformed(String),
    #[error("the identity token names a key the provider does not publish")]
    UnknownKey,
    #[error("the identity token does not carry the provider's signature")]
    Signature,
    #[error("the identity token was issued for another audience")]
    Audience,
    #[error("the identity token names another issuer")]
    Issuer,
    #[error("the identity token has expired")]
    Expired,
    #[error("the identity token answers another authorization request")]
    Nonce,
}

/// The identity the provider vouches for.
///
/// `iss` and `sub` together are the identity key; `src/principals.rs` matches
/// on exactly that pair. `email` is display text and nothing else: most
/// providers let a human change it, so it may not decide access.
#[derive(Clone, Debug, Deserialize)]
pub struct IdentityClaims {
    pub iss: String,
    pub sub: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub nonce: Option<String>,
}

/// The four members of a discovery document this server uses. Every one is
/// required: a document missing any of them cannot drive a login.
#[derive(Deserialize)]
struct DiscoveryDocument {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

/// One JSON Web Key. The members are those of the two key types in use at
/// OpenID Connect providers: RSA (`n`, `e`) and elliptic curve (`x`, `y`).
#[derive(Clone, Deserialize)]
struct Jwk {
    kty: String,
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    alg: Option<String>,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

#[derive(Deserialize)]
struct KeySet {
    keys: Vec<Jwk>,
}

/// What the token endpoint answers. The access token is deliberately absent:
/// this server never uses one, so it never reads one into a variable it might
/// later log.
#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
}

/// One configured upstream provider, discovered once and reused.
///
/// The key set is cached and refetched at most once per exchange, when a token
/// names a key the cache does not hold. That is what makes a provider's key
/// rotation invisible here: the first token signed by the new key pays one
/// extra request, and every later one is served from the cache.
pub struct Provider {
    /// The issuer identifier, exactly as the discovery document spells it.
    /// This is what an `iss` claim is compared against, and what a principal
    /// entry names.
    issuer: String,
    authorization_endpoint: Url,
    token_endpoint: Url,
    jwks_uri: Url,
    http: reqwest::Client,
    /// The published keys. The guard is never held across an `await`: every
    /// reader copies the key it wants out and drops the lock.
    keys: Mutex<Vec<Jwk>>,
}

impl Provider {
    /// Read `{issuer}/.well-known/openid-configuration` and the key set it
    /// names.
    ///
    /// The document must name `issuer` as its own issuer, as OpenID Connect
    /// Discovery 1.0 section 4.3 requires. Without that check the operator's
    /// `--oidc-issuer` and the `iss` claim this module verifies could name two
    /// different providers, and the verification below would prove nothing
    /// about the one the operator chose.
    ///
    /// No scheme check happens here. `--oidc-issuer` is already validated as an
    /// https URL by `crate::config::normalize_https_url` in `mcpmem`, and
    /// repeating the rule here would only fork it — and would put a loopback
    /// provider out of reach of the tests.
    pub async fn discover(issuer: &str) -> Result<Provider, UpstreamError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // A redirect moves the request off the host the operator named,
            // and every URL here comes from that host's own document.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| UpstreamError::Transport(e.to_string()))?;

        let url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let document: DiscoveryDocument = get_json(&http, &url).await?;
        // A trailing slash is the one difference tolerated, and only because
        // this server creates it: `--oidc-issuer` is normalized by
        // `crate::config::normalize_https_url` in `mcpmem`, which strips a
        // trailing slash, while several large providers publish one —
        // `https://tenant.auth0.com/` is the common case. Rejecting that pair
        // would refuse a correctly configured provider over a character that
        // cannot name a different origin. Nothing else is tolerated, and the
        // `iss` claim below is still compared against the provider's own
        // spelling, byte for byte.
        if document.issuer.trim_end_matches('/') != issuer.trim_end_matches('/') {
            return Err(UpstreamError::Discovery(format!(
                "the document at {url} names the issuer '{}'",
                document.issuer
            )));
        }
        let provider = Provider {
            issuer: document.issuer,
            authorization_endpoint: endpoint(
                "authorization_endpoint",
                &document.authorization_endpoint,
            )?,
            token_endpoint: endpoint("token_endpoint", &document.token_endpoint)?,
            jwks_uri: endpoint("jwks_uri", &document.jwks_uri)?,
            http,
            keys: Mutex::new(Vec::new()),
        };
        let keys = provider.fetch_keys().await?;
        *provider.lock() = keys;
        Ok(provider)
    }

    pub fn authorization_endpoint(&self) -> &str {
        self.authorization_endpoint.as_str()
    }

    pub fn token_endpoint(&self) -> &str {
        self.token_endpoint.as_str()
    }

    pub fn jwks_uri(&self) -> &str {
        self.jwks_uri.as_str()
    }

    /// The issuer identifier this provider publishes, which is what an `iss`
    /// claim and a principal entry are matched against.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The URL to send the human to.
    ///
    /// `redirect_uri` is this server's own callback, never the client's:
    /// the upstream code must land here, so that the human's identity is
    /// verified before any code is issued to a client.
    ///
    /// `scope` asks for `openid` and `email`. `openid` is what makes the
    /// response an OpenID Connect one at all, and `email` is display text for
    /// the consent screen and the log.
    pub fn authorize_url(
        &self,
        client_id: &str,
        redirect_uri: &str,
        state: &str,
        nonce: &str,
        challenge: &str,
    ) -> String {
        let mut url = self.authorization_endpoint.clone();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", "openid email")
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        url.into()
    }

    /// Exchange the upstream code for an identity token, verify it, and return
    /// what it says about the human.
    ///
    /// `client_secret` is sent only when the operator configured one: a public
    /// client that sends an empty secret is refused by some providers and
    /// silently downgraded by others.
    ///
    /// `expected_nonce` is the value on the login row. It is a parameter and
    /// not the caller's business, so that no call site can verify the audience
    /// and the expiry and then forget the one claim that binds this token to
    /// this login.
    pub async fn exchange(
        &self,
        code: &str,
        verifier: &str,
        client_id: &str,
        client_secret: Option<&str>,
        redirect_uri: &str,
        expected_nonce: &str,
    ) -> Result<IdentityClaims, UpstreamError> {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
        ];
        if let Some(secret) = client_secret {
            form.push(("client_secret", secret));
        }
        let response = self
            .http
            .post(self.token_endpoint.clone())
            .form(&form)
            .send()
            .await
            .map_err(|e| UpstreamError::Transport(e.to_string()))?;
        let status = response.status();
        let body = read_capped(response).await?;
        if !status.is_success() {
            return Err(UpstreamError::TokenEndpoint(format!(
                "HTTP {status}: {}",
                String::from_utf8_lossy(&body)
            )));
        }
        let token: TokenResponse = serde_json::from_slice(&body)
            .map_err(|e| UpstreamError::Malformed(format!("the token response: {e}")))?;
        self.verify(&token.id_token, client_id, expected_nonce)
            .await
    }

    /// Verify one identity token against the provider's keys and this
    /// server's client identifier.
    async fn verify(
        &self,
        id_token: &str,
        client_id: &str,
        expected_nonce: &str,
    ) -> Result<IdentityClaims, UpstreamError> {
        let header =
            decode_header(id_token).map_err(|e| UpstreamError::Malformed(e.to_string()))?;
        let kid = header.kid.as_deref();
        let jwk = match self.key(kid) {
            Some(jwk) => jwk,
            None => {
                // The provider rotated, or the token is a forgery naming a key
                // that never existed. One refetch tells the two apart, and the
                // cache holds the answer for every later token.
                let keys = self.fetch_keys().await?;
                *self.lock() = keys;
                self.key(kid).ok_or(UpstreamError::UnknownKey)?
            }
        };
        let (key, alg) = decoding_key(&jwk, header.alg)?;

        let mut validation = Validation::new(alg);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[client_id]);
        // The login window is minutes wide and this server sets both ends of
        // it. A default minute of clock grace would only widen the window a
        // stolen token is usable in.
        validation.leeway = 0;
        validation.required_spec_claims = ["exp", "iss", "aud", "sub"]
            .map(String::from)
            .into_iter()
            .collect::<HashSet<_>>();

        let claims = decode::<IdentityClaims>(id_token, &key, &validation)
            .map_err(|e| claim_error(&e))?
            .claims;
        if claims.nonce.as_deref() != Some(expected_nonce) {
            return Err(UpstreamError::Nonce);
        }
        Ok(claims)
    }

    /// The cached key with this identifier, or — when the token names none and
    /// the provider publishes exactly one key — that key.
    ///
    /// A token with no `kid` against a set of several keys is refused rather
    /// than tried against each: trying every key is how a verifier ends up
    /// accepting a signature from a key the provider retired.
    fn key(&self, kid: Option<&str>) -> Option<Jwk> {
        let keys = self.lock();
        match kid {
            Some(kid) => keys.iter().find(|k| k.kid.as_deref() == Some(kid)).cloned(),
            None => match keys.as_slice() {
                [only] => Some(only.clone()),
                _ => None,
            },
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Jwk>> {
        self.keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn fetch_keys(&self) -> Result<Vec<Jwk>, UpstreamError> {
        let set: KeySet = get_json(&self.http, self.jwks_uri.as_str()).await?;
        Ok(set.keys)
    }
}

/// One endpoint URL from a discovery document, parsed once so that no later
/// call has to handle a parse failure.
fn endpoint(name: &str, value: &str) -> Result<Url, UpstreamError> {
    Url::parse(value)
        .map_err(|e| UpstreamError::Discovery(format!("{name} is not a URL: {value}: {e}")))
}

/// The verifying key for one JWK, and the algorithm to verify with.
///
/// The algorithm comes from the key when the key names one, and only otherwise
/// from the token's own header — and either way it must match the key type.
/// That is the whole defence against algorithm confusion: a token whose header
/// says `HS256` cannot be verified with the provider's public key as an HMAC
/// secret, because an RSA or EC key admits no HMAC algorithm here.
fn decoding_key(
    jwk: &Jwk,
    header_alg: Algorithm,
) -> Result<(DecodingKey, Algorithm), UpstreamError> {
    let alg = match jwk.alg.as_deref() {
        Some(named) => named
            .parse::<Algorithm>()
            .map_err(|_| UpstreamError::Discovery(format!("unsupported key algorithm {named}")))?,
        None => header_alg,
    };
    let missing =
        |member: &str| UpstreamError::Discovery(format!("a {} key carries no '{member}'", jwk.kty));
    match jwk.kty.as_str() {
        "RSA" => {
            if !matches!(
                alg,
                Algorithm::RS256
                    | Algorithm::RS384
                    | Algorithm::RS512
                    | Algorithm::PS256
                    | Algorithm::PS384
                    | Algorithm::PS512
            ) {
                return Err(UpstreamError::Discovery(format!(
                    "an RSA key cannot verify {alg:?}"
                )));
            }
            let n = jwk.n.as_deref().ok_or_else(|| missing("n"))?;
            let e = jwk.e.as_deref().ok_or_else(|| missing("e"))?;
            let key = DecodingKey::from_rsa_components(n, e)
                .map_err(|e| UpstreamError::Discovery(e.to_string()))?;
            Ok((key, alg))
        }
        "EC" => {
            if !matches!(alg, Algorithm::ES256 | Algorithm::ES384) {
                return Err(UpstreamError::Discovery(format!(
                    "an EC key cannot verify {alg:?}"
                )));
            }
            let x = jwk.x.as_deref().ok_or_else(|| missing("x"))?;
            let y = jwk.y.as_deref().ok_or_else(|| missing("y"))?;
            let key = DecodingKey::from_ec_components(x, y)
                .map_err(|e| UpstreamError::Discovery(e.to_string()))?;
            Ok((key, alg))
        }
        other => Err(UpstreamError::Discovery(format!(
            "unsupported key type {other}"
        ))),
    }
}

/// One `jsonwebtoken` failure, as the reason an operator needs.
///
/// The match names each kind it reports rather than falling through to one
/// message, because [`UpstreamError::Signature`] and [`UpstreamError::Audience`]
/// send an operator to two different places.
fn claim_error(e: &jsonwebtoken::errors::Error) -> UpstreamError {
    use jsonwebtoken::errors::ErrorKind;
    match e.kind() {
        ErrorKind::InvalidSignature => UpstreamError::Signature,
        ErrorKind::ExpiredSignature => UpstreamError::Expired,
        ErrorKind::InvalidAudience => UpstreamError::Audience,
        ErrorKind::InvalidIssuer => UpstreamError::Issuer,
        _ => UpstreamError::Malformed(e.to_string()),
    }
}

/// Read one JSON document from the provider, bounded in size and in time.
async fn get_json<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
) -> Result<T, UpstreamError> {
    let response = http
        .get(url)
        .send()
        .await
        .map_err(|e| UpstreamError::Transport(e.to_string()))?;
    let status = response.status();
    let body = read_capped(response).await?;
    if !status.is_success() {
        return Err(UpstreamError::Discovery(format!("{url} answered {status}")));
    }
    serde_json::from_slice(&body).map_err(|e| UpstreamError::Discovery(format!("{url}: {e}")))
}

/// The body, up to [`MAX_DOCUMENT_BYTES`]. The read is chunked rather than one
/// `bytes()` call so that a host streaming without end is stopped at the cap
/// instead of at this process's memory.
async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, UpstreamError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| UpstreamError::Transport(e.to_string()))?
    {
        if body.len() + chunk.len() > MAX_DOCUMENT_BYTES {
            return Err(UpstreamError::Transport(format!(
                "the provider sent more than {MAX_DOCUMENT_BYTES} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
