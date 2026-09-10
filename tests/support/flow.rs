//! The path a client walks from `GET /oauth/authorize` to the consent page,
//! and from there to `POST /oauth/consent`.
//!
//! Every test of consent, of the token endpoint and of the expiry windows
//! starts from the same three hops — register, authorize, come back through the
//! provider — so they live here once. The hops are real: a real registration
//! request, a real redirect to `support::fake_idp`, and a real identity token
//! verified by the callback. Nothing is planted straight into the store, so a
//! flow that these helpers can drive is a flow a browser can drive.
//!
//! Two entry points, because the two answers the callback can give are both
//! worth a test:
//!
//! - [`authorize_to_callback`] stops at whatever the callback answered, and so
//!   serves a test about a login that must never reach consent.
//! - [`authorize_to_consent`] asserts that answer is the consent page and
//!   parses it, and so serves every test about the page and the approval.
//!
//! The clock is fixed at [`NOW`] rather than left on the wall clock: an
//! authorization code has a lifetime, and a test that has to observe it moves
//! [`Authorized::clock`] instead of sleeping.

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use mcpmem_oauth::store::{CodeGrant, Store};

use super::fake_idp::{FakeIdp, IdpBehaviour};
use super::{Clock, Scopes, Server};

/// The one redirect URI the test client registers. It is Claude's real
/// callback: the tests assert on the redirect this server builds, and a URI
/// with a path and no query is the shape that check has to get right.
pub const CLIENT_REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";
/// The state the client sends to this server. Opaque here, and returned
/// unchanged on the redirect.
pub const CLIENT_STATE: &str = "the-client-state";
/// The name the test client registers under.
pub const CLIENT_NAME: &str = "Test client";
/// The PKCE challenge the client sends. Task 8 verifies a code against the
/// challenge stored with it, so the value has to survive the whole flow.
pub const CODE_CHALLENGE: &str = "the-client-challenge";
/// What the clock reads for the whole of a flow. A fixed value, so a code
/// minted at [`NOW`] is live at `NOW + 1` and no test races the wall clock.
pub const NOW: i64 = 1_700_000_000_000_000;

/// The flow up to the callback's answer, whatever that answer is.
///
/// It owns the server and the provider, so a test holds this value for as long
/// as it sends requests. One at a time per thread: it holds a
/// [`super::Server`], and two of those deadlock.
pub struct Callback {
    /// What `GET /oauth/callback` answered.
    pub response: Response<Body>,
    /// The login state, which is also the state the provider sent back.
    pub state: String,
    /// The identifier this server issued to the test client.
    pub client_id: String,
    server: Server,
    /// The provider must outlive the flow: the discovered `Provider` holds its
    /// URLs, and Task 8's token exchange runs against this server afterwards.
    idp: FakeIdp,
}

/// A flow parked on the consent page, with the page's own values read back out
/// of it — which is what a browser posts, rather than what the store holds.
pub struct Authorized {
    /// The rendered consent page.
    pub body: String,
    /// The single-use token the page carries, read from its hidden field.
    pub csrf: String,
    /// The login state the page carries, read from its hidden field.
    pub state: String,
    /// The state the client sent to this server, echoed on the redirect.
    pub client_state: String,
    /// The identifier this server issued to the test client.
    pub client_id: String,
    /// What the clock reads. Fixed for the whole flow.
    pub now: i64,
    server: Server,
    idp: FakeIdp,
}

/// Register a client, start a login, walk the provider, and come back.
///
/// It asserts only that the authorization request was accepted. What the
/// callback made of the identity token is the caller's to judge.
pub async fn authorize_to_callback(client_name: &str, scope: &str) -> Callback {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let server = super::server(
        Some(super::oauth_config(&idp.issuer)),
        Scopes::all(),
        Some(Clock::at(NOW)),
    )
    .await;
    let client_id = register(&server, client_name, CLIENT_REDIRECT).await;

    let started = server
        .request(authorize_request(&client_id, CLIENT_REDIRECT, scope))
        .await;
    assert_eq!(
        started.status(),
        StatusCode::FOUND,
        "the authorization request must be accepted: {}",
        body_text(started).await
    );
    let back = idp.login(&super::header(&started, "location")).await;

    let response = server
        .request(
            Request::get(format!(
                "/oauth/callback?code={}&state={}",
                back.code, back.state
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    Callback {
        response,
        state: back.state,
        client_id,
        server,
        idp,
    }
}

/// The whole flow up to a rendered consent page, under [`CLIENT_NAME`].
pub async fn authorize_to_consent(scope: &str) -> Authorized {
    authorize_to_callback(CLIENT_NAME, scope)
        .await
        .into_consent()
        .await
}

/// The whole flow up to a rendered consent page, under a chosen client name.
/// A client name reaches the page from an unauthenticated registration, so one
/// test registers a hostile one and reads the page it produces.
pub async fn authorize_to_consent_named(client_name: &str, scope: &str) -> Authorized {
    authorize_to_callback(client_name, scope)
        .await
        .into_consent()
        .await
}

impl Callback {
    /// Assert the callback answered the consent page, and read its values.
    pub async fn into_consent(self) -> Authorized {
        let Callback {
            response,
            state,
            client_id,
            server,
            idp,
        } = self;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the callback must answer the consent page"
        );
        let body = body_text(response).await;
        let csrf = hidden_field(&body, "csrf");
        let form_state = hidden_field(&body, "state");
        assert_eq!(
            form_state, state,
            "the page must carry the state of the login it belongs to"
        );
        Authorized {
            body,
            csrf,
            state: form_state,
            client_state: CLIENT_STATE.to_owned(),
            client_id,
            now: NOW,
            server,
            idp,
        }
    }

    /// Run `f` with the store locked. The store is private behind
    /// [`mcpmem::oauth_routes::OauthState::with_store`], and a test reads it
    /// the same way a handler does.
    pub fn with_store<T>(&self, f: impl FnOnce(&Store) -> T) -> T {
        self.server.oauth().with_store(f)
    }

    /// How many logins are in flight.
    pub fn count_logins(&self) -> i64 {
        self.with_store(|s| count(s, "oauth_login"))
    }
}

impl Authorized {
    /// The server behind this flow, for a request these helpers do not shape.
    pub const fn server(&self) -> &Server {
        &self.server
    }

    /// The provider behind this flow.
    pub const fn idp(&self) -> &FakeIdp {
        &self.idp
    }

    /// The clock this flow reads, for a test that has to age a code out.
    pub const fn clock(&self) -> &Clock {
        self.server.clock()
    }

    /// Run `f` with the store locked.
    pub fn with_store<T>(&self, f: impl FnOnce(&Store) -> T) -> T {
        self.server.oauth().with_store(f)
    }

    /// Post a consent form carrying exactly `fields`. A repeated `scope` is
    /// what a page with two ticked boxes sends, so the pairs are a list rather
    /// than a map.
    pub async fn post_consent(&self, fields: &[(&str, &str)]) -> Response<Body> {
        self.server
            .request(
                Request::post("/oauth/consent")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(form_body(fields)))
                    .unwrap(),
            )
            .await
    }

    /// Redeem a code, at a moment one microsecond after it was minted.
    pub fn take_code(&self, code: &str) -> CodeGrant {
        self.with_store(|s| s.take_code(code, self.now + 1))
            .expect("the store answers")
            .expect("the code was stored")
    }

    /// How many authorization codes the store holds.
    pub fn count_codes(&self) -> i64 {
        self.with_store(|s| count(s, "oauth_code"))
    }

    /// How many logins are in flight.
    pub fn count_logins(&self) -> i64 {
        self.with_store(|s| count(s, "oauth_login"))
    }
}

/// The `code` query parameter of a `Location` header.
pub fn code_from(location: &str) -> String {
    query_param(location, "code").expect("the redirect carries a code")
}

/// One query parameter of a `Location` header, decoded.
pub fn query_param(location: &str, name: &str) -> Option<String> {
    let url = reqwest::Url::parse(location).expect("the location is an absolute URL");
    url.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

/// Register one client and return the identifier this server issued.
pub async fn register(server: &Server, client_name: &str, redirect_uri: &str) -> String {
    let body = serde_json::json!({
        "client_name": client_name,
        "redirect_uris": [redirect_uri],
    })
    .to_string();
    let res = server
        .request(
            Request::post("/oauth/register")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::CREATED);
    super::json(res).await["client_id"]
        .as_str()
        .expect("the registration names the client")
        .to_owned()
}

/// A `GET /oauth/authorize` this server accepts, asking for `scope`.
pub fn authorize_request(client_id: &str, redirect_uri: &str, scope: &str) -> Request<Body> {
    let query = form_body(&[
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("state", CLIENT_STATE),
        ("code_challenge", CODE_CHALLENGE),
        ("code_challenge_method", "S256"),
        ("scope", scope),
        ("resource", &format!("{}/mcp", super::PUBLIC_URL)),
    ]);
    Request::get(format!("/oauth/authorize?{query}"))
        .body(Body::empty())
        .unwrap()
}

/// The body of a form carrying exactly `fields`, percent-encoded.
pub fn form_body(fields: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("https://form.invalid/").expect("a valid base");
    for (k, v) in fields {
        url.query_pairs_mut().append_pair(k, v);
    }
    url.query().unwrap_or_default().to_owned()
}

pub async fn body_text(res: Response<Body>) -> String {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).expect("the body is UTF-8")
}

/// The value of the hidden form field named `name`.
///
/// It reads the attributes in the order the template writes them, which is the
/// order a hidden field is written in every template here. A template that
/// reorders them fails this with a message naming the field, rather than
/// silently handing back an empty token.
fn hidden_field(body: &str, name: &str) -> String {
    let needle = format!("name=\"{name}\" value=\"");
    let start = body
        .find(&needle)
        .unwrap_or_else(|| panic!("the page carries no hidden field named {name}"))
        + needle.len();
    let rest = &body[start..];
    let end = rest
        .find('"')
        .unwrap_or_else(|| panic!("the hidden field {name} is unterminated"));
    rest[..end].to_owned()
}

fn count(store: &Store, table: &str) -> i64 {
    store
        .connection()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .expect("the store counts its rows")
}
