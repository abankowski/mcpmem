//! A fake OpenID Connect provider, served in the test process.
//!
//! Every test of the upstream leg needs a provider that answers discovery,
//! redirects an authorization request, and signs an identity token. Mocking
//! `mcpmem_oauth::upstream::Provider` itself would prove nothing: the whole
//! point of that type is that it speaks to a real provider over real HTTP and
//! verifies a real signature. So this is a real provider — a real keypair, a
//! real JWKS, real `ES256` tokens — bound to an ephemeral loopback port.
//!
//! [`IdpBehaviour`] is the switch board. Its [`Default`] is a well-formed
//! provider, and each field breaks exactly one thing, so a test that expects
//! [`mcpmem_oauth::upstream::UpstreamError::Audience`] cannot accidentally
//! also have a stale nonce.
//!
//! The provider binds `127.0.0.1:0` and reads the port back from the listener,
//! rather than picking a free port and hoping it is still free — the pattern in
//! `tests/ui_http.rs:44-53` has to release the port before the child binds it,
//! and this provider runs here, so it can hold the listener the whole time.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::{Form, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, http::StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use parking_lot::Mutex;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};

/// The authorization code this provider hands out. One value: this provider
/// authenticates nobody, so there is nothing to tell two codes apart.
pub const CODE: &str = "fake-idp-code";
/// The subject of every identity token. `support::principals` allows it.
pub const SUBJECT: &str = "sub-1";
/// The audience of a well-formed identity token: the client identifier
/// `support::oauth_config` registers at this provider.
pub const AUDIENCE: &str = "mcpmem-test";
/// The email claim of every identity token.
pub const EMAIL: &str = "adam@example.com";
/// An issuer identifier belonging to nobody here, for the two tests that ask
/// what happens when a document or a token names another provider.
pub const FOREIGN_ISSUER: &str = "https://another-provider.example";

/// The key identifier the provider publishes first, and signs with unless
/// [`IdpBehaviour::rotate_kid_after_discovery`] is set.
const FIRST_KID: &str = "k1";
/// The key identifier the provider rotates to. The keypair does not change:
/// a rotation this test cares about is a key the verifier has not seen, and a
/// new `kid` on the same key is exactly that, with nothing else moving.
const ROTATED_KID: &str = "k2";

/// What this provider does wrong. Each field breaks one thing and nothing else.
pub struct IdpBehaviour {
    /// Sign the identity token with a keypair the JWKS never publishes.
    pub sign_with_foreign_key: bool,
    /// The `aud` claim. `None` is [`AUDIENCE`].
    pub audience: Option<String>,
    /// The `nonce` claim. `None` echoes the nonce of the authorization
    /// request, and is absent when no authorization request arrived.
    pub nonce: Option<String>,
    /// The `iss` claim of the identity token. `None` is this provider's own
    /// issuer identifier, which is what a well-formed token carries. A value
    /// here signs a token that names a provider other than the one the
    /// document and the key set belong to.
    pub token_issuer: Option<String>,
    /// How the discovery document spells this provider's issuer identifier.
    pub document_issuer: DocumentIssuer,
    /// `exp`, relative to now. Negative is an expired token.
    pub expires_in_seconds: i64,
    /// Publish [`FIRST_KID`] to the first reader of the key set and
    /// [`ROTATED_KID`] to every later one, and sign with [`ROTATED_KID`]. A
    /// verifier holding the first answer must refetch to verify anything.
    pub rotate_kid_after_discovery: bool,
}

/// A well-formed provider. `expires_in_seconds` is spelled out rather than
/// derived: a derived `0` is a token that expired the moment it was signed,
/// which is a trap for every test that does not name the field.
impl Default for IdpBehaviour {
    fn default() -> IdpBehaviour {
        IdpBehaviour {
            sign_with_foreign_key: false,
            audience: None,
            nonce: None,
            token_issuer: None,
            document_issuer: DocumentIssuer::Own,
            expires_in_seconds: 300,
            rotate_kid_after_discovery: false,
        }
    }
}

/// How the discovery document spells the issuer identifier.
///
/// One field with three values rather than two booleans: the three are
/// mutually exclusive spellings of one string, and two booleans would admit a
/// fourth combination that means nothing.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum DocumentIssuer {
    /// The base URL, which is what `--oidc-issuer` normalizes to.
    Own,
    /// The base URL and a trailing slash. Auth0 publishes this shape, and
    /// `--oidc-issuer` cannot carry it.
    WithTrailingSlash,
    /// [`FOREIGN_ISSUER`]: a document that claims to belong to somebody else.
    Foreign,
}

/// What the provider redirected back to the client.
pub struct Callback {
    pub code: String,
    pub state: String,
}

/// One P-256 keypair, in the two shapes this file needs: the signing key
/// `jsonwebtoken` wants, and the two public coordinates a JWK carries.
struct Keypair {
    signing: EncodingKey,
    x: String,
    y: String,
}

impl Keypair {
    /// Generate a keypair. `ring` produces the PKCS#8 document that
    /// `jsonwebtoken` hands straight back to `ring` when it signs, so the two
    /// agree on the encoding by construction.
    fn generate() -> Keypair {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .expect("generate a P-256 keypair");
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .expect("read back the generated keypair");
        // An uncompressed SEC1 point: 0x04, then the two 32-byte coordinates.
        let point = pair.public_key().as_ref();
        assert_eq!(point.len(), 65, "an uncompressed P-256 point is 65 bytes");
        Keypair {
            signing: EncodingKey::from_ec_der(pkcs8.as_ref()),
            x: URL_SAFE_NO_PAD.encode(&point[1..33]),
            y: URL_SAFE_NO_PAD.encode(&point[33..65]),
        }
    }
}

struct IdpState {
    behaviour: IdpBehaviour,
    issuer: String,
    /// The keypair the JWKS publishes.
    published: Keypair,
    /// The keypair that signs, which is `published` unless the behaviour says
    /// otherwise.
    signing: EncodingKey,
    jwks_requests: AtomicUsize,
    /// The nonce of the last authorization request, which the identity token
    /// echoes. `None` until one arrives.
    seen_nonce: Mutex<Option<String>>,
}

impl IdpState {
    /// The key identifier the identity token carries.
    const fn token_kid(&self) -> &'static str {
        if self.behaviour.rotate_kid_after_discovery {
            ROTATED_KID
        } else {
            FIRST_KID
        }
    }

    /// The issuer identifier this provider publishes.
    fn published_issuer(&self) -> String {
        match self.behaviour.document_issuer {
            DocumentIssuer::Own => self.issuer.clone(),
            DocumentIssuer::WithTrailingSlash => format!("{}/", self.issuer),
            DocumentIssuer::Foreign => FOREIGN_ISSUER.to_owned(),
        }
    }
}

/// A provider on a loopback port, and the task serving it.
pub struct FakeIdp {
    /// The issuer identifier, which is also the base URL: `http://127.0.0.1:p`.
    pub issuer: String,
    state: Arc<IdpState>,
    task: tokio::task::JoinHandle<()>,
    client: reqwest::Client,
}

/// The task holds the listener, so it must go when the provider does.
/// Otherwise a binary running many tests accumulates one live socket per test.
impl Drop for FakeIdp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeIdp {
    /// Start a provider and return once it is bound. Nothing here is
    /// asynchronous except the bind: the listener exists before this returns,
    /// so a request sent on the next line cannot lose a race with it.
    pub async fn start(behaviour: IdpBehaviour) -> FakeIdp {
        let published = Keypair::generate();
        let signing = if behaviour.sign_with_foreign_key {
            Keypair::generate().signing
        } else {
            published.signing.clone()
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral loopback port");
        let issuer = format!(
            "http://{}",
            listener.local_addr().expect("read back the bound port")
        );
        let state = Arc::new(IdpState {
            behaviour,
            issuer: issuer.clone(),
            published,
            signing,
            jwks_requests: AtomicUsize::new(0),
            seen_nonce: Mutex::new(None),
        });
        let router = Router::new()
            .route("/.well-known/openid-configuration", get(configuration))
            .route("/jwks", get(jwks))
            .route("/authorize", get(authorize))
            .route("/token", post(token))
            .with_state(Arc::clone(&state));
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("the fake provider serves until the test drops it");
        });
        FakeIdp {
            issuer,
            state,
            task,
            client: reqwest::Client::builder()
                // A redirect is the answer under test, not something to follow.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("build the test client"),
        }
    }

    /// How many times the key set has been read. The refetch after an unknown
    /// key identifier is the only way this reaches two.
    pub fn jwks_requests(&self) -> usize {
        self.state.jwks_requests.load(Ordering::SeqCst)
    }

    /// Walk the authorization request the way a browser would: send it, and
    /// read the code and the state out of the redirect. This is also what
    /// records the nonce the identity token echoes, so a test that skips it
    /// gets a token with no `nonce` claim.
    pub async fn login(&self, authorize_url: &str) -> Callback {
        let res = self
            .client
            .get(authorize_url)
            .send()
            .await
            .expect("the fake provider answers an authorization request");
        assert_eq!(
            res.status(),
            reqwest::StatusCode::FOUND,
            "an authorization request is answered with a redirect"
        );
        let location = res
            .headers()
            .get("location")
            .expect("the redirect names a location")
            .to_str()
            .expect("the location is ASCII")
            .to_owned();
        let url = reqwest::Url::parse(&location).expect("the location is an absolute URL");
        let mut code = None;
        let mut state = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        Callback {
            code: code.expect("the redirect carries a code"),
            state: state.expect("the redirect carries the state"),
        }
    }
}

async fn configuration(State(state): State<Arc<IdpState>>) -> Json<Value> {
    let issuer = &state.issuer;
    Json(json!({
        "issuer": state.published_issuer(),
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["ES256"],
        "code_challenge_methods_supported": ["S256"],
    }))
}

async fn jwks(State(state): State<Arc<IdpState>>) -> Json<Value> {
    let earlier = state.jwks_requests.fetch_add(1, Ordering::SeqCst);
    let kid = if state.behaviour.rotate_kid_after_discovery && earlier > 0 {
        ROTATED_KID
    } else {
        FIRST_KID
    };
    Json(json!({
        "keys": [{
            "kty": "EC",
            "crv": "P-256",
            "use": "sig",
            "alg": "ES256",
            "kid": kid,
            "x": state.published.x,
            "y": state.published.y,
        }],
    }))
}

/// Authenticate nobody and redirect at once, carrying the fixed code and the
/// state the request gave. The nonce is recorded for the token endpoint.
async fn authorize(
    State(state): State<Arc<IdpState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(redirect_uri) = q.get("redirect_uri") else {
        return (StatusCode::BAD_REQUEST, "no redirect_uri").into_response();
    };
    if let Some(nonce) = q.get("nonce") {
        *state.seen_nonce.lock() = Some(nonce.clone());
    }
    let mut back = reqwest::Url::parse(redirect_uri).expect("the redirect_uri is an absolute URL");
    back.query_pairs_mut()
        .append_pair("code", CODE)
        .append_pair("state", q.get("state").map_or("", String::as_str));
    (
        StatusCode::FOUND,
        [(axum::http::header::LOCATION, back.to_string())],
    )
        .into_response()
}

/// Exchange anything for an identity token. What the token says is
/// [`IdpBehaviour`]'s to decide; the request itself is not checked, because no
/// test here is about what this provider refuses.
async fn token(
    State(state): State<Arc<IdpState>>,
    Form(_form): Form<HashMap<String, String>>,
) -> Json<Value> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_secs() as i64;
    let nonce = state
        .behaviour
        .nonce
        .clone()
        .or_else(|| state.seen_nonce.lock().clone());
    let published = state.published_issuer();
    let mut claims = json!({
        "iss": state.behaviour.token_issuer.as_deref().unwrap_or(&published),
        "sub": SUBJECT,
        "aud": state.behaviour.audience.as_deref().unwrap_or(AUDIENCE),
        "email": EMAIL,
        "iat": now,
        "exp": now + state.behaviour.expires_in_seconds,
    });
    if let Some(nonce) = nonce {
        claims["nonce"] = json!(nonce);
    }
    let header = Header {
        alg: Algorithm::ES256,
        kid: Some(state.token_kid().to_owned()),
        ..Header::default()
    };
    let id_token = encode(&header, &claims, &state.signing).expect("sign the identity token");
    Json(json!({
        "access_token": "fake-idp-access-token",
        "token_type": "Bearer",
        "expires_in": 300,
        "id_token": id_token,
    }))
}
