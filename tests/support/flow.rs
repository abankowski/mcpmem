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
use serde_json::Value;

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
/// The PKCE verifier the test client keeps. It never leaves the client: a
/// token request presents this, and the endpoint recomputes the challenge from
/// it. 44 unreserved characters, inside RFC 7636 section 4.1's 43 to 128.
pub const CODE_VERIFIER: &str = "the-client-verifier-for-the-mcpmem-test-flow";
/// The PKCE challenge the client sends, which is the S256 digest of
/// [`CODE_VERIFIER`].
///
/// Derived, never written out. `GET /oauth/authorize` stores this value
/// verbatim under `code_challenge_method=S256`, so a token request must
/// present a verifier whose digest equals it. A literal here would be the
/// digest of nothing, and no exchange could ever succeed against it.
///
/// A function rather than a constant, because a `const` cannot hash.
pub fn code_challenge() -> String {
    mcpmem_oauth::s256_challenge(CODE_VERIFIER)
}
/// What the clock reads for the whole of a flow. A fixed value, so a code
/// minted at [`NOW`] is live at `NOW + 1` and no test races the wall clock.
pub const NOW: i64 = 1_700_000_000_000_000;

/// One authorization request, and what the client registered to make it.
///
/// Build it with [`Flow::new`] and change one thing at a time. Every field is
/// something a conforming client may vary, and each of the five that are
/// optional in the specifications — the client name, the client's state, the
/// redirect URI's own query, the requested scope, the resource — has a test
/// that needs it.
pub struct Flow {
    /// `None` means the registration body carries no `client_name` at all,
    /// which RFC 7591 section 2 allows.
    client_name: Option<String>,
    redirect_uri: String,
    scope: String,
    /// `None` means the authorization request sends no `state`, which RFC 6749
    /// section 4.1.1 allows.
    client_state: Option<String>,
    /// The resource the authorization request names, under RFC 8707. It is
    /// the one this server protects unless a test names another.
    resource: String,
    /// The static bearer token this server also carries, when a test needs the
    /// deployment the runbook sells: `--auth-token-file` beside
    /// `--oidc-issuer`. `None` is OAuth alone.
    static_token: Option<String>,
}

impl Flow {
    /// A flow one accepted authorization request wide, asking for `scope`.
    pub fn new(scope: &str) -> Flow {
        Flow {
            client_name: Some(CLIENT_NAME.to_owned()),
            redirect_uri: CLIENT_REDIRECT.to_owned(),
            scope: scope.to_owned(),
            client_state: Some(CLIENT_STATE.to_owned()),
            resource: format!("{}/mcp", super::PUBLIC_URL),
            static_token: None,
        }
    }

    /// Configure `token` as the static bearer token beside OAuth.
    pub fn static_token(mut self, token: &str) -> Flow {
        self.static_token = Some(token.to_owned());
        self
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

    /// Name `resource` on the authorization request instead of the one this
    /// server protects. A token is bound to that value, so a request that
    /// asks for a different one must never reach a human.
    pub fn resource(mut self, resource: &str) -> Flow {
        self.resource = resource.to_owned();
        self
    }

    /// Stand the server and the provider up, and send nothing yet.
    ///
    /// The conformance walk needs this: its first hops are the 401 challenge
    /// on `/mcp` and the two discovery documents, and all of them happen
    /// before any client exists.
    pub async fn start(self) -> Started {
        let idp = FakeIdp::start(IdpBehaviour::default()).await;
        let config = super::oauth_config(&idp.issuer);
        let clock = Some(Clock::at(NOW));
        let server = match &self.static_token {
            None => super::server(Some(config), Scopes::all(), clock).await,
            Some(token) => {
                super::server_with_static_token(Some(config), token, Scopes::all(), clock).await
            }
        };
        Started {
            flow: self,
            server,
            idp,
        }
    }

    /// The server and the provider, up, with no client registered and this
    /// flow's defaults behind them.
    ///
    /// [`Flow::start`] with nothing to configure. A test about what an
    /// anonymous caller may do to `POST /oauth/register` needs the router and
    /// no client, and naming that here keeps the scope — which such a test
    /// never reaches — out of it.
    pub async fn fresh() -> Started {
        Flow::new("graph-read").start().await
    }

    /// Register a client, start a login, walk the provider, and come back.
    ///
    /// It asserts only that the authorization request was accepted. What the
    /// callback made of the identity token is the caller's to judge. Every
    /// hop is a [`Started`] method, so the conformance test can send exactly
    /// these requests and assert on each answer itself.
    pub async fn into_callback(self) -> Callback {
        let flow = self.start().await;
        let client_id = flow.register().await;

        let started = flow.authorize(&client_id).await;
        assert_eq!(
            started.status(),
            StatusCode::FOUND,
            "the authorization request must be accepted: {}",
            body_text(started).await
        );
        let back = flow.idp().login(&super::header(&started, "location")).await;

        let response = flow.callback(&back.code, &back.state).await;
        flow.into_stage(response, back.state, client_id)
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

    /// A `GET /oauth/authorize` this server accepts, unless a builder has
    /// changed one of its values.
    fn authorize_request(&self, client_id: &str) -> Request<Body> {
        let challenge = code_challenge();
        let mut fields = vec![
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("scope", self.scope.as_str()),
            ("resource", self.resource.as_str()),
        ];
        if let Some(client_state) = &self.client_state {
            fields.push(("state", client_state));
        }
        Request::get(format!("/oauth/authorize?{}", query_string(&fields)))
            .body(Body::empty())
            .unwrap()
    }
}

/// The server and the provider, up, with no request sent yet.
///
/// Each method here is one hop, and it returns what that hop answered rather
/// than judging it. [`Flow::into_callback`] chains exactly these calls and
/// adds the assertions a flow that must succeed needs, so the conformance
/// test can send the same requests and make its own assertions at every one.
pub struct Started {
    flow: Flow,
    server: Server,
    idp: FakeIdp,
}

impl Started {
    /// The server this flow runs against, for the discovery hops that come
    /// before any client exists.
    pub const fn server(&self) -> &Server {
        &self.server
    }

    /// The upstream provider this server was configured against.
    pub const fn idp(&self) -> &FakeIdp {
        &self.idp
    }

    /// `POST /oauth/register`, and the identifier this server issued.
    pub async fn register(&self) -> String {
        register_body(&self.server, &self.flow.registration_body()).await
    }

    /// `POST /oauth/register` as a caller at `peer`, and the whole answer
    /// rather than the identifier.
    ///
    /// The peer travels in `X-Forwarded-For`, which is where this server reads
    /// it from: `support::oauth_config` sets `trust_forwarded_proto`, as every
    /// deployment behind a TLS-terminating proxy does. A `oneshot` request
    /// carries no connection, so it is also the only address a test can name.
    ///
    /// It returns a [`Reply`] because a refused registration is the point: the
    /// status and `Retry-After` are what a rate-limited caller sees, and
    /// [`Started::register`] asserts the identifier it cannot have.
    pub async fn register_from(&self, peer: &str) -> Reply {
        let res = self
            .server
            .request(
                Request::post("/oauth/register")
                    .header("content-type", "application/json")
                    .header("x-forwarded-for", peer)
                    .body(Body::from(self.flow.registration_body()))
                    .unwrap(),
            )
            .await;
        Reply::of(res).await
    }

    /// `GET /oauth/authorize`.
    pub async fn authorize(&self, client_id: &str) -> Response<Body> {
        self.server
            .request(self.flow.authorize_request(client_id))
            .await
    }

    /// `GET /oauth/callback`, the hop the provider redirects the human to.
    pub async fn callback(&self, code: &str, state: &str) -> Response<Body> {
        self.server
            .request(
                Request::get(format!("/oauth/callback?code={code}&state={state}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
    }

    /// `GET /oauth/callback` as a caller at `peer`, and the whole answer.
    ///
    /// The peer travels in `X-Forwarded-For`, for the reason
    /// [`Started::register_from`] gives. The callback is anonymous — whoever
    /// holds the URL can send it — so a test about what one caller may cost
    /// this endpoint needs to name the caller.
    pub async fn callback_from(&self, peer: &str, code: &str, state: &str) -> Reply {
        let res = self
            .server
            .request(
                Request::get(format!("/oauth/callback?code={code}&state={state}"))
                    .header("x-forwarded-for", peer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        Reply::of(res).await
    }

    /// Carry the callback's answer into the stage that owns the server, so a
    /// test that drove the hops itself can go on to consent.
    pub fn into_stage(
        self,
        response: Response<Body>,
        state: String,
        client_id: String,
    ) -> Callback {
        Callback {
            response,
            state,
            client_id,
            client_state: self.flow.client_state,
            redirect_uri: self.flow.redirect_uri,
            server: self.server,
            idp: self.idp,
        }
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
    redirect_uri: String,
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
            redirect_uri,
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
            redirect_uri,
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
    /// The redirect URI the client registered, and so the one an accepted
    /// token request repeats.
    pub redirect_uri: String,
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

    /// What the clock reads now, which is what a store call a test makes must
    /// pass. It moves with [`Authorized::clock`], so a helper reading it after
    /// an `advance_seconds` sees the moved value — which a constant captured
    /// when the page rendered would not.
    pub fn now(&self) -> i64 {
        self.clock().now_us()
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

    /// `POST /oauth/token` as an authorization code exchange, carrying this
    /// flow's defaults with `fields` replacing any key it names.
    ///
    /// The defaults are one accepted exchange apart from `code`, which every
    /// caller supplies, so a test that changes the verifier changes only the
    /// verifier. An unknown key is added rather than dropped: a test about a
    /// parameter this server does not send by default needs to send it.
    pub async fn token(&self, fields: &[(&str, &str)]) -> Reply {
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("client_id", self.client_id.as_str()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("code_verifier", CODE_VERIFIER),
        ];
        for (key, value) in fields {
            match form.iter_mut().find(|pair| pair.0 == *key) {
                Some(pair) => pair.1 = value,
                None => form.push((key, value)),
            }
        }
        self.post_form("/oauth/token", &form).await
    }

    /// Exchange `code` for a live pair, and assert the exchange succeeded.
    /// The hop every test of a bearer token starts from.
    pub async fn exchange(&self, code: &str) -> Tokens {
        let res = self.token(&[("code", code)]).await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "the exchange must succeed: {}",
            res.body
        );
        Tokens {
            access_token: string_member(&res.body, "access_token"),
            refresh_token: string_member(&res.body, "refresh_token"),
        }
    }

    /// `POST /oauth/token` as a refresh grant.
    pub async fn refresh(&self, refresh_token: &str) -> Reply {
        self.post_form(
            "/oauth/token",
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", self.client_id.as_str()),
            ],
        )
        .await
    }

    /// `POST /oauth/revoke`. RFC 7009 takes either kind of token, so the
    /// caller names which one it is presenting.
    pub async fn revoke(&self, token: &str) -> Reply {
        self.post_form(
            "/oauth/revoke",
            &[("token", token), ("client_id", self.client_id.as_str())],
        )
        .await
    }

    /// `POST path` with `fields` as the whole form body, verbatim. A repeated
    /// key stays repeated, which is what a test of RFC 6749 section 3.1 needs
    /// and what [`Authorized::token`] cannot produce.
    pub async fn post_form(&self, path: &str, fields: &[(&str, &str)]) -> Reply {
        let res = self
            .server
            .request(
                Request::post(path)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(query_string(fields)))
                    .unwrap(),
            )
            .await;
        Reply::of(res).await
    }

    /// `GET path` as the holder of `token` when there is one, for the
    /// viewer's data endpoints. `None` is the anonymous request.
    pub async fn get(&self, path: &str, token: Option<&str>) -> Reply {
        let mut req = Request::get(path);
        if let Some(token) = token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let res = self.server.request(req.body(Body::empty()).unwrap()).await;
        Reply::of(res).await
    }

    /// `POST /mcp` carrying `body`, as the holder of `token` when there is
    /// one. `None` is the anonymous request the 401 challenge answers.
    pub async fn mcp(&self, token: Option<&str>, body: &str) -> Reply {
        let mut req = Request::post("/mcp").header("content-type", "application/json");
        if let Some(token) = token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let res = self
            .server
            .request(req.body(Body::from(body.to_owned())).unwrap())
            .await;
        Reply::of(res).await
    }

    /// `tools/list` as the holder of `token`.
    pub async fn mcp_tools_list(&self, token: &str) -> Reply {
        self.mcp(
            Some(token),
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        )
        .await
    }

    /// `tools/call` for `tool` as the holder of `token`.
    ///
    /// The whole body is screened against the caller's scopes before anything
    /// is dispatched (`mcpmem::server::dispatch_http_body`), so a refused call
    /// never reaches the tool's own argument checks and `arguments` matters
    /// only for a call this principal may make.
    pub async fn mcp_call(&self, token: &str, tool: &str, arguments: Value) -> Reply {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        });
        self.mcp(Some(token), &body.to_string()).await
    }

    /// The tool names `tools/list` advertises to the holder of `token`.
    pub async fn tool_names(&self, token: &str) -> Vec<String> {
        let res = self.mcp_tools_list(token).await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "tools/list must answer: {}",
            res.body
        );
        res.body["result"]["tools"]
            .as_array()
            .expect("tools/list answers an array of tools")
            .iter()
            .map(|tool| string_member(tool, "name"))
            .collect()
    }

    /// Rewrite the audience of every token this flow has been issued.
    ///
    /// That is what a token minted by another instance of this server would
    /// carry, and it is the only way to produce one here: this server binds
    /// every token it issues to its own resource, so no request can ask for
    /// another.
    pub fn rebind_resource(&self, resource: &str) {
        with_store(&self.server, |store| {
            let rows = store
                .connection()
                .execute("UPDATE oauth_token SET resource = ?1", [resource])
                .expect("the store rewrites the audience");
            assert!(rows > 0, "this flow has been issued no token to rebind");
        });
    }

    /// The grant stored under `code`, read **without** spending it.
    ///
    /// A test asserts on a grant and then exchanges the same code at the token
    /// endpoint, so this must not spend the row: `Store::take_code` marks the
    /// row spent, and an inspection through it would make the exchange that
    /// follows fail with `invalid_grant` — and revoke the family with it. It
    /// reads by digest, the way the store keys the row, and ignores both the
    /// expiry and `spent`: whether a code may be redeemed is the endpoint's
    /// answer to give, not this reader's.
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

    /// How many rows `table` holds. The general form of [`Authorized::count_codes`]
    /// and [`Authorized::count_logins`], for a test that names more than one
    /// table in one assertion.
    pub fn count(&self, table: &str) -> i64 {
        count_rows(&self.server, table)
    }

    /// Write one access token belonging to `family` that expired a second ago.
    ///
    /// Planted rather than walked, and it is the one thing in this file that
    /// is: the flow that mints an expired token is a flow whose clock has
    /// already passed the expiry, and by then every token it holds is expired
    /// too. A sweep test needs one dead row **beside** a live one, so the dead
    /// one is written directly.
    ///
    /// The value is derived from `family` rather than random, so a test can
    /// present it. It is no use to its holder: the row is expired, and
    /// `Store::find_access` refuses it.
    pub fn insert_expired_family(&self, family: &str) {
        let now = self.now();
        with_store(&self.server, |store| {
            store
                .put_token(
                    &format!("{family}-access-token"),
                    mcpmem_oauth::store::TokenKind::Access,
                    &Grant {
                        client_id: self.client_id.clone(),
                        principal: "someone-who-left".to_owned(),
                        scopes: vec!["graph-read".to_owned()],
                        resource: format!("{}/mcp", super::PUBLIC_URL),
                        family: family.to_owned(),
                    },
                    now - 2_000_000,
                    now - 1_000_000,
                )
                .expect("the store writes a token row");
        });
    }
}

/// What one request answered, in the four parts a test here asks about.
pub struct Reply {
    pub status: StatusCode,
    /// The body parsed as JSON, or `Value::Null` when it was not JSON. The
    /// token endpoint answers JSON on every path, including its refusals; the
    /// transport answers text on some, and a test that reads the body of one
    /// of those is reading the wrong thing.
    pub body: Value,
    /// The `WWW-Authenticate` header, or the empty string when the response
    /// carried none.
    ///
    /// A `String` rather than an `Option<String>` because every assertion on
    /// it is either an equality against the whole header or a substring
    /// check, and an empty header value is not a thing this server sends —
    /// so the empty string means *absent* with nothing to confuse it with.
    pub www_authenticate: String,
    /// The `Retry-After` header, or the empty string when the response carried
    /// none. Empty means *absent*, for the reason `www_authenticate` gives.
    pub retry_after: String,
}

impl Reply {
    async fn of(res: Response<Body>) -> Reply {
        let status = res.status();
        let header = |name: axum::http::HeaderName| {
            res.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        let www_authenticate = header(axum::http::header::WWW_AUTHENTICATE);
        let retry_after = header(axum::http::header::RETRY_AFTER);
        let text = body_text(res).await;
        Reply {
            status,
            body: serde_json::from_str(&text).unwrap_or(Value::Null),
            www_authenticate,
            retry_after,
        }
    }
}

/// The pair one accepted token request hands back.
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
}

/// The string member `name` of `value`, or a panic naming the whole body.
fn string_member(value: &Value, name: &str) -> String {
    value[name]
        .as_str()
        .unwrap_or_else(|| panic!("the body must carry a string {name}: {value}"))
        .to_owned()
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
