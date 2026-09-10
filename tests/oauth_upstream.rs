#![cfg(feature = "oauth")]
//! The upstream OpenID Connect leg: discovery, the authorization redirect, the
//! callback, and the verification of the identity token that comes back.
//!
//! Everything here runs against `support::fake_idp`, a real provider on a
//! loopback port with a real keypair. The identity token is therefore a real
//! `ES256` token, and a test that expects a refusal gets it from the verifier
//! rather than from a stub that was told to fail.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mcpmem_oauth::upstream::{Provider, UpstreamError};

mod support;
use support::fake_idp::{DocumentIssuer, FakeIdp, IdpBehaviour};

const CLIENT_ID: &str = "mcpmem-test";
const REDIRECT: &str = "https://mem.example.com/oauth/callback";

// ── The provider ────────────────────────────────────────────────────────────

#[tokio::test]
async fn discovery_reads_the_three_endpoints() {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    assert_eq!(
        p.authorization_endpoint(),
        format!("{}/authorize", idp.issuer)
    );
    assert_eq!(p.token_endpoint(), format!("{}/token", idp.issuer));
    assert_eq!(p.jwks_uri(), format!("{}/jwks", idp.issuer));
}

#[tokio::test]
async fn a_valid_identity_token_yields_its_claims() {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    let back = idp
        .login(&p.authorize_url(
            CLIENT_ID,
            REDIRECT,
            "the-state",
            "the-nonce",
            "the-challenge",
        ))
        .await;
    assert_eq!(back.state, "the-state", "the provider echoes the state");
    let claims = p
        .exchange(
            &back.code,
            "the-verifier",
            CLIENT_ID,
            None,
            REDIRECT,
            "the-nonce",
        )
        .await
        .unwrap();
    assert_eq!(claims.iss, idp.issuer);
    assert_eq!(claims.sub, "sub-1");
    assert_eq!(claims.email.as_deref(), Some("adam@example.com"));
    assert_eq!(claims.nonce.as_deref(), Some("the-nonce"));
}

#[tokio::test]
async fn an_identity_token_signed_by_another_key_is_refused() {
    let idp = FakeIdp::start(IdpBehaviour {
        sign_with_foreign_key: true,
        ..IdpBehaviour::default()
    })
    .await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    let back = idp
        .login(&p.authorize_url(CLIENT_ID, REDIRECT, "s", "n", "c"))
        .await;
    let err = p
        .exchange(&back.code, "v", CLIENT_ID, None, REDIRECT, "n")
        .await
        .unwrap_err();
    assert!(matches!(err, UpstreamError::Signature), "was: {err:?}");
}

#[tokio::test]
async fn an_identity_token_with_the_wrong_audience_is_refused() {
    let idp = FakeIdp::start(IdpBehaviour {
        audience: Some("someone-else".into()),
        ..IdpBehaviour::default()
    })
    .await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    let back = idp
        .login(&p.authorize_url(CLIENT_ID, REDIRECT, "s", "n", "c"))
        .await;
    let err = p
        .exchange(&back.code, "v", CLIENT_ID, None, REDIRECT, "n")
        .await
        .unwrap_err();
    assert!(matches!(err, UpstreamError::Audience), "was: {err:?}");
}

/// `--oidc-issuer` is normalized without a trailing slash, and several large
/// providers publish one. The pair must still discover, or a correctly
/// configured Auth0 tenant is unreachable — and the `iss` claim must then be
/// matched against the provider's own spelling, which is what a principals
/// file has to name.
#[tokio::test]
async fn a_provider_publishing_a_trailing_slash_still_discovers() {
    let idp = FakeIdp::start(IdpBehaviour {
        document_issuer: DocumentIssuer::WithTrailingSlash,
        ..IdpBehaviour::default()
    })
    .await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    assert_eq!(p.issuer(), format!("{}/", idp.issuer));
    let back = idp
        .login(&p.authorize_url(CLIENT_ID, REDIRECT, "s", "n", "c"))
        .await;
    let claims = p
        .exchange(&back.code, "v", CLIENT_ID, None, REDIRECT, "n")
        .await
        .unwrap();
    assert_eq!(claims.iss, format!("{}/", idp.issuer));
}

/// A document that claims to belong to another provider is refused at
/// discovery. That check is the binding between the issuer the operator
/// configured and everything verified afterwards: without it, the `iss`
/// comparison below would be against whatever the document said about itself.
#[tokio::test]
async fn a_document_that_names_another_issuer_is_refused() {
    let idp = FakeIdp::start(IdpBehaviour {
        document_issuer: DocumentIssuer::Foreign,
        ..IdpBehaviour::default()
    })
    .await;
    let Err(err) = Provider::discover(&idp.issuer).await else {
        panic!("a document naming another issuer must not discover");
    };
    assert!(matches!(err, UpstreamError::Discovery(_)), "was: {err:?}");
}

/// A principal is keyed by `iss` plus `sub`, so a token that names another
/// issuer while carrying a known subject is the confusion the `iss` check
/// exists to refuse. The signature is this provider's, and the audience is
/// right: only the issuer is wrong.
#[tokio::test]
async fn an_identity_token_naming_another_issuer_is_refused() {
    let idp = FakeIdp::start(IdpBehaviour {
        token_issuer: Some(support::fake_idp::FOREIGN_ISSUER.into()),
        ..IdpBehaviour::default()
    })
    .await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    let back = idp
        .login(&p.authorize_url(CLIENT_ID, REDIRECT, "s", "n", "c"))
        .await;
    let err = p
        .exchange(&back.code, "v", CLIENT_ID, None, REDIRECT, "n")
        .await
        .unwrap_err();
    assert!(matches!(err, UpstreamError::Issuer), "was: {err:?}");
}

#[tokio::test]
async fn an_identity_token_with_a_stale_nonce_is_refused() {
    let idp = FakeIdp::start(IdpBehaviour {
        nonce: Some("an-old-nonce".into()),
        ..IdpBehaviour::default()
    })
    .await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    let back = idp
        .login(&p.authorize_url(CLIENT_ID, REDIRECT, "s", "the-new-nonce", "c"))
        .await;
    let err = p
        .exchange(&back.code, "v", CLIENT_ID, None, REDIRECT, "the-new-nonce")
        .await
        .unwrap_err();
    assert!(matches!(err, UpstreamError::Nonce), "was: {err:?}");
}

#[tokio::test]
async fn an_expired_identity_token_is_refused() {
    let idp = FakeIdp::start(IdpBehaviour {
        expires_in_seconds: -60,
        ..IdpBehaviour::default()
    })
    .await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    let back = idp
        .login(&p.authorize_url(CLIENT_ID, REDIRECT, "s", "n", "c"))
        .await;
    let err = p
        .exchange(&back.code, "v", CLIENT_ID, None, REDIRECT, "n")
        .await
        .unwrap_err();
    assert!(matches!(err, UpstreamError::Expired), "was: {err:?}");
}

#[tokio::test]
async fn an_unknown_key_identifier_refetches_the_key_set_once() {
    let idp = FakeIdp::start(IdpBehaviour {
        rotate_kid_after_discovery: true,
        ..IdpBehaviour::default()
    })
    .await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    let back = idp
        .login(&p.authorize_url(CLIENT_ID, REDIRECT, "s", "n", "c"))
        .await;
    let claims = p
        .exchange(&back.code, "v", CLIENT_ID, None, REDIRECT, "n")
        .await
        .unwrap();
    assert_eq!(claims.sub, "sub-1");
    assert_eq!(idp.jwks_requests(), 2, "one initial fetch, one refetch");
}

/// A key identifier that never appears refetches once and then gives up. The
/// test above proves the refetch happens; this one proves it happens *once*,
/// which is what stops a stream of forged tokens from becoming a stream of
/// requests to the provider.
#[tokio::test]
async fn an_identity_token_whose_key_never_appears_is_refused_after_one_refetch() {
    let idp = FakeIdp::start(IdpBehaviour {
        sign_with_foreign_key: true,
        rotate_kid_after_discovery: true,
        ..IdpBehaviour::default()
    })
    .await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    let back = idp
        .login(&p.authorize_url(CLIENT_ID, REDIRECT, "s", "n", "c"))
        .await;
    // The rotated key identifier *is* published on the refetch, so the token is
    // found and refused on its signature rather than on its key identifier.
    let err = p
        .exchange(&back.code, "v", CLIENT_ID, None, REDIRECT, "n")
        .await
        .unwrap_err();
    assert!(matches!(err, UpstreamError::Signature), "was: {err:?}");
    assert_eq!(idp.jwks_requests(), 2, "one initial fetch, one refetch");
}

// ── The two routes ──────────────────────────────────────────────────────────

const CLIENT_REDIRECT: &str = "https://client.example/cb";

/// Register one client and return the identifier this server issued.
async fn register(server: &support::Server) -> String {
    let body = format!(r#"{{"client_name":"Test client","redirect_uris":["{CLIENT_REDIRECT}"]}}"#);
    let res = server
        .request(
            Request::post("/oauth/register")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::CREATED);
    support::json(res).await["client_id"]
        .as_str()
        .expect("the registration names the client")
        .to_owned()
}

/// A `GET /oauth/authorize` carrying exactly `params`, percent-encoded.
fn authorize_request(params: &[(&str, &str)]) -> Request<Body> {
    let mut url = reqwest::Url::parse("https://mem.example.com/oauth/authorize").unwrap();
    for (k, v) in params {
        url.query_pairs_mut().append_pair(k, v);
    }
    Request::get(format!(
        "{}?{}",
        url.path(),
        url.query().expect("the request carries a query")
    ))
    .body(Body::empty())
    .unwrap()
}

/// The parameters of a request this server accepts, with `client_id` filled in.
fn good_params(client_id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("client_id", client_id.to_owned()),
        ("redirect_uri", CLIENT_REDIRECT.to_owned()),
        ("state", "the-client-state".to_owned()),
        ("code_challenge", "the-client-challenge".to_owned()),
        ("code_challenge_method", "S256".to_owned()),
        ("scope", "graph-read".to_owned()),
        ("resource", "https://mem.example.com/mcp".to_owned()),
    ]
}

/// `good_params`, with one value replaced. A refusal test changes exactly one
/// thing, so nothing else can be the reason for the answer.
fn params_with(client_id: &str, key: &str, value: &str) -> Vec<(&'static str, String)> {
    let mut params = good_params(client_id);
    for p in &mut params {
        if p.0 == key {
            p.1 = value.to_owned();
        }
    }
    params
}

fn as_pairs<'a>(params: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    params.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

async fn body_text(res: axum::response::Response<Body>) -> String {
    use http_body_util::BodyExt;
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).expect("the body is UTF-8")
}

#[tokio::test]
async fn the_authorize_endpoint_redirects_the_human_to_the_upstream_provider() {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let server = support::oauth_server_with(&idp.issuer).await;
    let client_id = register(&server).await;

    let params = good_params(&client_id);
    let res = server.request(authorize_request(&as_pairs(&params))).await;

    assert_eq!(res.status(), StatusCode::FOUND);
    let location = support::header(&res, "location");
    let url = reqwest::Url::parse(&location).unwrap();
    assert_eq!(
        &location[..idp.issuer.len() + "/authorize".len()],
        format!("{}/authorize", idp.issuer)
    );
    let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
    assert_eq!(q.get("client_id").map(String::as_str), Some("mcpmem-test"));
    assert_eq!(
        q.get("redirect_uri").map(String::as_str),
        Some("https://mem.example.com/oauth/callback"),
        "the upstream redirects back to this server, never to the client"
    );
    assert_eq!(
        q.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    let upstream_state = q.get("state").expect("the redirect carries a state");
    assert!(!upstream_state.is_empty());
    assert_ne!(
        upstream_state, "the-client-state",
        "the state sent upstream is this server's, not the client's"
    );
    assert!(q.contains_key("nonce"));
    assert!(q.contains_key("code_challenge"));

    // The login row holds what the token endpoint will need in Task 8.
    let now = (server.oauth().now_us)();
    let login = server
        .oauth()
        .with_store(|s| s.take_login(upstream_state, now))
        .unwrap()
        .expect("the authorize endpoint wrote a login row");
    assert_eq!(login.client_id, client_id);
    assert_eq!(login.redirect_uri, CLIENT_REDIRECT);
    assert_eq!(login.client_state.as_deref(), Some("the-client-state"));
    assert_eq!(login.code_challenge, "the-client-challenge");
    assert_eq!(login.resource, "https://mem.example.com/mcp");
    assert_eq!(login.scopes, vec!["graph-read".to_string()]);
    assert!(login.principal.is_none());
}

#[tokio::test]
async fn an_unknown_client_is_refused_without_a_redirect() {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let server = support::oauth_server_with(&idp.issuer).await;
    let params = good_params("never-registered");
    let res = server.request(authorize_request(&as_pairs(&params))).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(
        res.headers().get("location").is_none(),
        "a refusal must never redirect"
    );
}

#[tokio::test]
async fn a_redirect_uri_that_is_not_registered_is_refused_without_a_redirect() {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let server = support::oauth_server_with(&idp.issuer).await;
    let client_id = register(&server).await;
    // A prefix of a registered entry, which is the match a lax comparison lets
    // through and the one that hands the code to another host's path.
    let params = params_with(&client_id, "redirect_uri", "https://client.example/cb/evil");
    let res = server.request(authorize_request(&as_pairs(&params))).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(res.headers().get("location").is_none());
}

#[tokio::test]
async fn a_plain_code_challenge_method_is_refused() {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let server = support::oauth_server_with(&idp.issuer).await;
    let client_id = register(&server).await;
    let params = params_with(&client_id, "code_challenge_method", "plain");
    let res = server.request(authorize_request(&as_pairs(&params))).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(res.headers().get("location").is_none());
}

#[tokio::test]
async fn a_resource_this_server_does_not_protect_is_refused() {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let server = support::oauth_server_with(&idp.issuer).await;
    let client_id = register(&server).await;
    let params = params_with(&client_id, "resource", "https://mem.example.com/other");
    let res = server.request(authorize_request(&as_pairs(&params))).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(res.headers().get("location").is_none());
}

/// A request with no `resource` is accepted: RFC 8707 makes the indicator
/// optional, and a client that omits it gets this server's one resource.
#[tokio::test]
async fn an_absent_resource_is_accepted() {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let server = support::oauth_server_with(&idp.issuer).await;
    let client_id = register(&server).await;
    let params: Vec<_> = good_params(&client_id)
        .into_iter()
        .filter(|p| p.0 != "resource")
        .collect();
    let res = server.request(authorize_request(&as_pairs(&params))).await;
    assert_eq!(res.status(), StatusCode::FOUND);
}

/// Walk the whole leg: this server redirects, the provider redirects back, and
/// the callback verifies the identity token and names the human.
#[tokio::test]
async fn the_callback_records_the_authenticated_human_on_the_login() {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let server = support::oauth_server_with(&idp.issuer).await;
    let client_id = register(&server).await;

    let params = good_params(&client_id);
    let started = server.request(authorize_request(&as_pairs(&params))).await;
    let back = idp.login(&support::header(&started, "location")).await;

    let res = server
        .request(
            Request::get(format!(
                "/oauth/callback?code={}&state={}",
                back.code, back.state
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);

    let now = (server.oauth().now_us)();
    let login = server
        .oauth()
        .with_store(|s| s.take_login(&back.state, now))
        .unwrap()
        .expect("the callback leaves the login row for the consent step");
    assert_eq!(login.principal.as_deref(), Some("adam"));
    assert_eq!(login.client_id, client_id);
}

/// Every refusal at the callback is one page. An anonymous caller learns
/// nothing about who is allowed: not from the status, not from the body, and
/// not from a redirect, because there is none.
///
/// Both requests go to one server, because two `Server` values on one thread
/// deadlock — and comparing the two answers is the whole assertion, so they
/// cannot be split into two tests without pinning the page text instead.
#[tokio::test]
async fn every_refusal_at_the_callback_is_the_same_page() {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let mut config = support::oauth_config(&idp.issuer);
    // The provider authenticates `sub-1`. This server allows somebody else.
    config.principals[0].sub = "another-human".into();
    let server = support::server(Some(config), support::Scopes::all(), None).await;
    let client_id = register(&server).await;

    let params = good_params(&client_id);
    let started = server.request(authorize_request(&as_pairs(&params))).await;
    let back = idp.login(&support::header(&started, "location")).await;

    let refused_human = server
        .request(
            Request::get(format!(
                "/oauth/callback?code={}&state={}",
                back.code, back.state
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    let unknown_state = server
        .request(
            Request::get("/oauth/callback?code=whatever&state=never-issued")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

    assert_eq!(refused_human.status(), StatusCode::FORBIDDEN);
    assert_eq!(unknown_state.status(), StatusCode::FORBIDDEN);
    assert!(refused_human.headers().get("location").is_none());
    assert!(unknown_state.headers().get("location").is_none());
    assert_eq!(
        body_text(refused_human).await,
        body_text(unknown_state).await
    );
}

/// A callback for a human this server does not allow leaves nothing behind:
/// the login row is consumed whatever the outcome, so the state cannot be
/// replayed into a second exchange.
#[tokio::test]
async fn a_refused_callback_consumes_the_login() {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let mut config = support::oauth_config(&idp.issuer);
    config.principals[0].sub = "another-human".into();
    let server = support::server(Some(config), support::Scopes::all(), None).await;
    let client_id = register(&server).await;

    let params = good_params(&client_id);
    let started = server.request(authorize_request(&as_pairs(&params))).await;
    let back = idp.login(&support::header(&started, "location")).await;
    let path = format!("/oauth/callback?code={}&state={}", back.code, back.state);
    let first = server
        .request(Request::get(&path).body(Body::empty()).unwrap())
        .await;
    assert_eq!(first.status(), StatusCode::FORBIDDEN);

    let now = (server.oauth().now_us)();
    assert!(
        server
            .oauth()
            .with_store(|s| s.take_login(&back.state, now))
            .unwrap()
            .is_none(),
        "a refused login must not survive its callback"
    );
}

/// The callback with no parameters is a malformed request, not an identity
/// outcome, so it says so. Nothing about a human is disclosed either way.
#[tokio::test]
async fn a_callback_without_a_code_is_refused_as_a_bad_request() {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let server = support::oauth_server_with(&idp.issuer).await;
    let res = server
        .request(
            Request::get("/oauth/callback?state=whatever")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(res.headers().get("location").is_none());
}

/// Both routes answer 404 when OAuth is off, like every other OAuth route.
#[tokio::test]
async fn the_upstream_routes_are_absent_from_a_server_without_oauth() {
    let server = support::open_server().await;
    for path in ["/oauth/authorize", "/oauth/callback"] {
        let res = server
            .request(Request::get(path).body(Body::empty()).unwrap())
            .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
    }
}
