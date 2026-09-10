//! The path a client walks from `GET /oauth/authorize` to the consent page,
//! and from there to an authorization code.
//!
//! Every test of consent, of the token endpoint and of the expiry windows
//! starts from the same hops — register, authorize, come back through the
//! provider, approve — so they live here once. The hops are real: a real
//! registration request, a real redirect to `support::fake_idp`, and a real
//! identity token verified by the callback. Nothing is planted straight into
//! the store, so a flow that these helpers can drive is a flow a browser can
//! drive.
//!
//! [`Flow`] is the entry point, and every request this file sends is built from
//! it. Its defaults are one accepted request; each method changes exactly one
//! thing, so a test that wants a client with no name cannot accidentally also
//! change the redirect URI. Two stopping points:
//!
//! - [`Flow::into_callback`] stops at whatever the callback answered, and so
//!   serves a test about a login that must never reach consent.
//! - [`Flow::into_consent`] asserts that answer is the consent page and parses
//!   it, and so serves every test about the page and the decision.
//!
//! From a consent page, [`Authorized::approve`] returns the authorization code
//! itself: that is the hop every token test begins with.
//!
//! The clock is fixed at [`NOW`] rather than left on the wall clock: an
//! authorization code has a lifetime, and a test that has to observe it moves
//! [`Authorized::clock`] instead of sleeping.

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use mcpmem_oauth::store::{CodeGrant, Grant, Store};

use super::fake_idp::{FakeIdp, IdpBehaviour};
use super::{Clock, Scopes, Server};

/// The redirect URI the test client registers unless a test names another. It
/// is Claude's real callback, and it carries a path and no query.
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

/// One authorization request, and what the client registered to make it.
///
/// Build it with [`Flow::new`] and change one thing at a time. Every field is
/// something a conforming client may vary, and each of the four that are
/// optional in the specifications — the client name, the client's state, the
/// redirect URI's own query, the requested scope — has a test that needs it.
pub struct Flow {
    /// `None` means the registration body carries no `client_name` at all,
    /// which RFC 7591 section 2 allows.
    client_name: Option<String>,
    redirect_uri: String,
    scope: String,
    /// `None` means the authorization request sends no `state`, which RFC 6749
    /// section 4.1.1 allows.
    client_state: Option<String>,
}

impl Flow {
    /// A flow one accepted authorization request wide, asking for `scope`.
    pub fn new(scope: &str) -> Flow {
        Flow {
            client_name: Some(CLIENT_NAME.to_owned()),
            redirect_uri: CLIENT_REDIRECT.to_owned(),
            scope: scope.to_owned(),
            client_state: Some(CLIENT_STATE.to_owned()),
        }
    }

    /// Register under `name`. A client name reaches the consent page from an
    /// unauthenticated registration request, so one test registers a hostile
    /// one and reads the page it produces.
    pub fn client_name(mut self, name: &str) -> Flow {
        self.client_name = Some(name.to_owned());
        self
    }

    /// Register with no `client_name` member at all.
    pub fn without_client_name(mut self) -> Flow {
        self.client_name = None;
        self
    }

    /// Register and present `uri` instead of [`CLIENT_REDIRECT`].
    pub fn redirect_uri(mut self, uri: &str) -> Flow {
        self.redirect_uri = uri.to_owned();
        self
    }

    /// Send no `state` on the authorization request.
    pub fn without_client_state(mut self) -> Flow {
        self.client_state = None;
        self
    }

    /// Register a client, start a login, walk the provider, and come back.
    ///
    /// It asserts only that the authorization request was accepted. What the
    /// callback made of the identity token is the caller's to judge.
    pub async fn into_callback(self) -> Callback {
        let idp = FakeIdp::start(IdpBehaviour::default()).await;
        let server = super::server(
            Some(super::oauth_config(&idp.issuer)),
            Scopes::all(),
            Some(Clock::at(NOW)),
        )
        .await;
        let client_id = register_body(&server, &self.registration_body()).await;

        let started = server.request(self.authorize_request(&client_id)).await;
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
            client_state: self.client_state,
            server,
            idp,
        }
    }

    /// The whole flow up to a rendered consent page.
    pub async fn into_consent(self) -> Authorized {
        self.into_callback().await.into_consent().await
    }

    /// The registration body this flow sends. A flow with no client name omits
    /// the member, rather than sending an empty string: the two are the same to
    /// this server, and only one of them is what a real client sends.
    fn registration_body(&self) -> String {
        match &self.client_name {
            Some(name) => serde_json::json!({
                "client_name": name,
                "redirect_uris": [&self.redirect_uri],
            }),
            None => serde_json::json!({ "redirect_uris": [&self.redirect_uri] }),
        }
        .to_string()
    }

    /// A `GET /oauth/authorize` this server accepts.
    fn authorize_request(&self, client_id: &str) -> Request<Body> {
        let resource = format!("{}/mcp", super::PUBLIC_URL);
        let mut fields = vec![
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("code_challenge", CODE_CHALLENGE),
            ("code_challenge_method", "S256"),
            ("scope", self.scope.as_str()),
            ("resource", resource.as_str()),
        ];
        if let Some(client_state) = &self.client_state {
            fields.push(("state", client_state));
        }
        Request::get(format!("/oauth/authorize?{}", query_string(&fields)))
            .body(Body::empty())
            .unwrap()
    }
}

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
    client_state: Option<String>,
    server: Server,
    /// The provider must outlive the flow: the discovered `Provider` holds its
    /// URLs, and Task 8's token exchange runs against this server afterwards.
    idp: FakeIdp,
}

impl Callback {
    /// The server behind this flow. Store reads go through
    /// [`with_store`] and [`count_rows`], which every stopping point shares.
    pub const fn server(&self) -> &Server {
        &self.server
    }

    /// Assert the callback answered the consent page, and read its values.
    pub async fn into_consent(self) -> Authorized {
        let Callback {
            response,
            state,
            client_id,
            client_state,
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
            client_state,
            client_id,
            now: NOW,
            server,
            idp,
        }
    }
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
    /// The state the client sent to this server, or `None` when it sent none.
    pub client_state: Option<String>,
    /// The identifier this server issued to the test client.
    pub client_id: String,
    /// What the clock reads. Fixed for the whole flow.
    pub now: i64,
    server: Server,
    idp: FakeIdp,
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

    /// The state the client sent, which every accepted redirect echoes.
    /// Panics for a flow built with [`Flow::without_client_state`].
    pub fn client_state(&self) -> &str {
        self.client_state
            .as_deref()
            .expect("this flow sent a client state")
    }

    /// Post a consent form carrying exactly `fields`. A repeated `scope` is
    /// what a page with two ticked boxes sends, so the pairs are a list rather
    /// than a map.
    pub async fn post_consent(&self, fields: &[(&str, &str)]) -> Response<Body> {
        self.server
            .request(
                Request::post("/oauth/consent")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(query_string(fields)))
                    .unwrap(),
            )
            .await
    }

    /// Approve exactly `scopes` and return the authorization code the redirect
    /// carried. The hop from a rendered page to a code, which every test of the
    /// token endpoint starts from.
    pub async fn approve(&self, scopes: &[&str]) -> String {
        let mut fields = vec![("csrf", self.csrf.as_str()), ("state", self.state.as_str())];
        fields.extend(scopes.iter().map(|s| ("scope", *s)));
        let res = self.post_consent(&fields).await;
        assert_eq!(
            res.status(),
            StatusCode::FOUND,
            "the approval must be accepted"
        );
        code_from(&super::header(&res, "location"))
    }

    /// The grant stored under `code`, read **without** spending it.
    ///
    /// A test asserts on a grant and then exchanges the same code at the token
    /// endpoint, so this must not consume the row: `Store::take_code` is a
    /// `DELETE ... RETURNING`, and an inspection through it would make the
    /// exchange that follows fail with `invalid_grant`. It reads by digest, the
    /// way the store keys the row, and ignores the expiry — an expired code is
    /// still a row, and whether it may be redeemed is the endpoint's answer to
    /// give.
    pub fn code_grant(&self, code: &str) -> CodeGrant {
        with_store(&self.server, |store| {
            store
                .connection()
                .query_row(
                    "SELECT client_id, principal, scopes, resource, family,
                            redirect_uri, code_challenge
                     FROM oauth_code WHERE code_digest = ?1",
                    [mcpmem_oauth::digest(code)],
                    |r| {
                        Ok(CodeGrant {
                            grant: Grant {
                                client_id: r.get("client_id")?,
                                principal: r.get("principal")?,
                                scopes: serde_json::from_str(&r.get::<_, String>("scopes")?)
                                    .expect("the store wrote a scope list"),
                                resource: r.get("resource")?,
                                family: r.get("family")?,
                            },
                            redirect_uri: r.get("redirect_uri")?,
                            code_challenge: r.get("code_challenge")?,
                        })
                    },
                )
                .expect("the code was stored")
        })
    }

    /// How many authorization codes the store holds.
    pub fn count_codes(&self) -> i64 {
        count_rows(&self.server, "oauth_code")
    }

    /// How many logins are in flight.
    pub fn count_logins(&self) -> i64 {
        count_rows(&self.server, "oauth_login")
    }
}

/// Run `f` with the store locked. The store is private behind
/// [`mcpmem::oauth_routes::OauthState::with_store`], and a test reads it the
/// same way a handler does.
pub fn with_store<T>(server: &Server, f: impl FnOnce(&Store) -> T) -> T {
    server.oauth().with_store(f)
}

/// How many rows `table` holds.
pub fn count_rows(server: &Server, table: &str) -> i64 {
    with_store(server, |store| {
        store
            .connection()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .expect("the store counts its rows")
    })
}

/// The `code` query parameter of a `Location` header.
pub fn code_from(location: &str) -> String {
    query_param(location, "code").expect("the redirect carries a code")
}

/// One query parameter of a `Location` header, decoded, or `None` when the
/// redirect carries no such parameter.
pub fn query_param(location: &str, name: &str) -> Option<String> {
    let url = url::Url::parse(location).expect("the location is an absolute URL");
    url.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

/// Register one client from a raw JSON body, and return the identifier this
/// server issued.
pub async fn register_body(server: &Server, body: &str) -> String {
    let res = server
        .request(
            Request::post("/oauth/register")
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::CREATED);
    super::json(res).await["client_id"]
        .as_str()
        .expect("the registration names the client")
        .to_owned()
}

/// Register one named client for `redirect_uri`.
pub async fn register(server: &Server, client_name: &str, redirect_uri: &str) -> String {
    let body = serde_json::json!({
        "client_name": client_name,
        "redirect_uris": [redirect_uri],
    })
    .to_string();
    register_body(server, &body).await
}

/// `fields` as a percent-encoded query string, which is also the body of a
/// form. A repeated key stays repeated.
pub fn query_string(fields: &[(&str, &str)]) -> String {
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in fields {
        out.append_pair(key, value);
    }
    out.finish()
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
