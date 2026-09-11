#![cfg(feature = "oauth")]
//! Consent: what the page offers, what an approval turns into, and what a
//! refusal leaves behind.
//!
//! Every flow here is the real one. `support::flow` registers a client, sends a
//! real authorization request, walks `support::fake_idp`, and comes back
//! through the callback, so the consent page under test is the page a browser
//! would have been handed and the values posted back are the ones it carried.

use axum::http::StatusCode;
use mcpmem_oauth::consent::{escape_html, page};

mod support;
use support::flow::{self, Authorized, Flow};

/// The whole flow up to a rendered consent page, with every default.
async fn consent_for(scope: &str) -> Authorized {
    Flow::new(scope).into_consent().await
}

// ── The page ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_consent_page_offers_only_the_intersection() {
    // The principal in `support::principals` holds graph-read and graph-write.
    let a: Authorized = consent_for("graph-read graph-write code").await;
    assert!(a.body.contains(r#"value="graph-read""#));
    assert!(a.body.contains(r#"value="graph-write""#));
    assert!(!a.body.contains(r#"value="code""#), "code was not granted");
}

/// The client name arrives from an unauthenticated registration request and
/// reaches the page this server serves, so the escaping has to hold on the
/// route and not only in the function that renders it.
#[tokio::test]
async fn the_served_page_escapes_a_hostile_client_name() {
    let a = Flow::new("graph-read")
        .client_name("<script>alert(1)</script>")
        .into_consent()
        .await;
    assert!(!a.body.contains("<script>alert(1)</script>"));
    assert!(a.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
}

/// RFC 7591 section 2 makes `client_name` optional, so a client can register
/// without one — and then the page must name it by its identifier. A page that
/// names nobody is the more dangerous prompt of the two: an attacker would get
/// a blank "asks to use your memory graph" by omitting one field.
#[tokio::test]
async fn a_client_that_registered_no_name_is_named_by_its_identifier() {
    let a = Flow::new("graph-read")
        .without_client_name()
        .into_consent()
        .await;
    assert_named_by_identifier(&a);
}

/// A name of only whitespace is the same page as no name at all: HTML collapses
/// it, so the human reads `Authorize` and is asked to grant access to nobody.
/// `register` applies no trim and bounds only the length, so this body
/// registers as it stands.
#[tokio::test]
async fn a_client_whose_name_is_only_whitespace_is_named_by_its_identifier() {
    let a = Flow::new("graph-read")
        .client_name(" \t \n ")
        .into_consent()
        .await;
    assert_named_by_identifier(&a);
}

/// The page names the client by its identifier, and the element that names it
/// is not blank. Testing for `<strong></strong>` alone would pass for a name of
/// three spaces, which is the input that reopened this defect.
fn assert_named_by_identifier(a: &Authorized) {
    let named = format!("<strong>{}</strong>", a.client_id);
    assert!(
        a.body.contains(&named),
        "the page must name the client it cannot name by name: {}",
        a.body
    );
    assert!(
        a.body
            .contains(&format!("<h1>Authorize {}</h1>", a.client_id)),
        "the heading must name it too: {}",
        a.body
    );
}

/// A human who holds none of the scopes the client asked for has nothing to
/// consent to. The callback refuses, with the one page every refusal there
/// gets, and leaves no login for a later approval to find.
#[tokio::test]
async fn a_request_naming_only_scopes_the_principal_lacks_never_reaches_consent() {
    let c = Flow::new("code").into_callback().await;
    assert_eq!(c.response.status(), StatusCode::FORBIDDEN);
    assert!(c.response.headers().get("location").is_none());
    assert_eq!(
        flow::count_rows(c.server(), "oauth_login"),
        0,
        "the refused login is consumed"
    );
}

// ── Approval ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn approval_issues_a_code_for_only_the_approved_scopes() {
    let a = consent_for("graph-read graph-write").await;
    let code = a.approve(&["graph-read"]).await;
    let stored = a.code_grant(&code);
    assert_eq!(stored.grant.scopes, vec!["graph-read"]);
    // Reading the grant must not spend the code: Task 8 asserts on it and then
    // exchanges the same value at the token endpoint.
    assert_eq!(a.count_codes(), 1, "inspecting a grant must not spend it");
}

/// Task 8 exchanges the code, and every value it checks is written here. A
/// grant that names the wrong client, resource, redirect URI or challenge is a
/// code the right client cannot spend — or the wrong one can.
#[tokio::test]
async fn the_code_grant_binds_the_client_the_resource_and_the_challenge() {
    let a = consent_for("graph-read graph-write").await;
    let code = a.approve(&["graph-write", "graph-read"]).await;
    let stored = a.code_grant(&code);
    assert_eq!(stored.grant.client_id, a.client_id);
    assert_eq!(stored.grant.principal, "adam");
    assert_eq!(stored.grant.resource, "https://mem.example.com/mcp");
    assert!(
        !stored.grant.family.is_empty(),
        "a code opens a token family"
    );
    assert_eq!(stored.redirect_uri, flow::CLIENT_REDIRECT);
    // The challenge must be the digest of the verifier the client keeps, or
    // Task 8's first exchange cannot present anything this code accepts.
    assert_eq!(
        stored.code_challenge,
        mcpmem_oauth::s256_challenge(flow::CODE_VERIFIER)
    );
    // The offered order, not the order the form happened to send.
    assert_eq!(stored.grant.scopes, vec!["graph-read", "graph-write"]);
}

#[tokio::test]
async fn the_redirect_carries_the_client_state_and_the_iss_parameter() {
    let a = consent_for("graph-read").await;
    let res = a
        .post_consent(&[
            ("csrf", &a.csrf),
            ("state", &a.state),
            ("scope", "graph-read"),
        ])
        .await;
    let location = support::header(&res, "location");
    assert!(location.starts_with("https://claude.ai/api/mcp/auth_callback?"));
    assert!(location.contains("code="));
    assert!(location.contains(&format!("state={}", a.client_state())));
    assert!(location.contains("iss=https%3A%2F%2Fmem.example.com"));
}

/// A registered redirect URI may carry a query of its own, and RFC 6749
/// section 3.1.2 keeps it legal. The code is then appended with `&`, not `?`,
/// or the client receives one parameter named `tenant` whose value swallows the
/// code and loses the query it registered.
#[tokio::test]
async fn a_redirect_uri_that_already_carries_a_query_keeps_it() {
    let a = Flow::new("graph-read")
        .redirect_uri("https://app.example/cb?tenant=acme")
        .into_consent()
        .await;
    let res = a
        .post_consent(&[
            ("csrf", &a.csrf),
            ("state", &a.state),
            ("scope", "graph-read"),
        ])
        .await;
    let location = support::header(&res, "location");
    assert!(
        location.starts_with("https://app.example/cb?tenant=acme&"),
        "the registered query must survive: {location}"
    );
    assert_eq!(
        flow::query_param(&location, "tenant").as_deref(),
        Some("acme")
    );
    assert!(flow::query_param(&location, "code").is_some());
}

/// RFC 6749 section 4.1.1 makes `state` optional, and a PKCE client has no
/// need of it. The redirect must then carry no `state` at all: a server that
/// invented one would send a client a parameter it never chose, and a client
/// that checks for a `state` it did not send would refuse the code.
#[tokio::test]
async fn a_client_that_sent_no_state_gets_no_state_back() {
    let a = Flow::new("graph-read")
        .without_client_state()
        .into_consent()
        .await;
    let res = a
        .post_consent(&[
            ("csrf", &a.csrf),
            ("state", &a.state),
            ("scope", "graph-read"),
        ])
        .await;
    let location = support::header(&res, "location");
    assert!(
        flow::query_param(&location, "state").is_none(),
        "this server must not invent a state: {location}"
    );
    assert!(flow::query_param(&location, "code").is_some());
    assert_eq!(
        flow::query_param(&location, "iss").as_deref(),
        Some("https://mem.example.com")
    );
}

#[tokio::test]
async fn a_second_approval_finds_no_login_row() {
    let a = consent_for("graph-read").await;
    a.approve(&["graph-read"]).await;
    let second = a
        .post_consent(&[
            ("csrf", &a.csrf),
            ("state", &a.state),
            ("scope", "graph-read"),
        ])
        .await;
    assert_eq!(second.status(), StatusCode::BAD_REQUEST);
    assert_eq!(a.count_codes(), 1, "the second post mints nothing");
}

// ── Refusal ─────────────────────────────────────────────────────────────────

/// A wrong token is refused, and the login it named survives: a human whose
/// form went stale must be able to approve on the next attempt, and a stranger
/// guessing a state must not be able to end somebody's login.
#[tokio::test]
async fn a_wrong_csrf_token_is_refused() {
    let a = consent_for("graph-read").await;
    let res = a
        .post_consent(&[
            ("csrf", "not-the-token"),
            ("state", &a.state),
            ("scope", "graph-read"),
        ])
        .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(a.count_codes(), 0);

    let retry = a
        .post_consent(&[
            ("csrf", &a.csrf),
            ("state", &a.state),
            ("scope", "graph-read"),
        ])
        .await;
    assert_eq!(
        retry.status(),
        StatusCode::FOUND,
        "the refused post must leave the login in place"
    );
}

#[tokio::test]
async fn approving_a_scope_the_principal_lacks_is_refused() {
    let a = consent_for("graph-read").await;
    let res = a
        .post_consent(&[("csrf", &a.csrf), ("state", &a.state), ("scope", "code")])
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(a.count_codes(), 0);
}

/// The scope the client asked for and the principal does not hold is the one a
/// crafted form body will tick, because it is the one the page did not show.
/// The test above asks for `graph-read` alone, so it would also pass against a
/// server that never narrowed the login's scopes at all; this one asks for
/// `code` and then approves it.
#[tokio::test]
async fn approving_a_requested_scope_the_principal_lacks_is_refused() {
    let a = consent_for("graph-read code").await;
    let res = a
        .post_consent(&[
            ("csrf", &a.csrf),
            ("state", &a.state),
            ("scope", "graph-read"),
            ("scope", "code"),
        ])
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        a.count_codes(),
        0,
        "a partly-offered approval mints nothing"
    );
}

#[tokio::test]
async fn approving_nothing_is_refused() {
    let a = consent_for("graph-read").await;
    let res = a
        .post_consent(&[("csrf", &a.csrf), ("state", &a.state)])
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(a.count_codes(), 0);
}

/// The size of the list a crafted body can make this server allocate is this
/// endpoint's problem, exactly as it is `GET /oauth/authorize`'s. Every entry
/// here names an offered scope, so a server that truncated the list instead of
/// refusing would answer with a perfectly good code.
#[tokio::test]
async fn a_form_carrying_more_scopes_than_the_cap_is_refused() {
    let a = consent_for("graph-read").await;
    let mut fields = vec![("csrf", a.csrf.as_str()), ("state", a.state.as_str())];
    for _ in 0..17 {
        fields.push(("scope", "graph-read"));
    }
    let res = a.post_consent(&fields).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(a.count_codes(), 0);
}

// ── Denial ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn denial_redirects_with_access_denied() {
    let a = consent_for("graph-read").await;
    let forged = a
        .post_consent(&[
            ("csrf", "not-the-token"),
            ("state", &a.state),
            ("deny", "1"),
        ])
        .await;
    assert_eq!(
        forged.status(),
        StatusCode::FORBIDDEN,
        "a denial carries the same token as an approval"
    );

    let res = a
        .post_consent(&[("csrf", &a.csrf), ("state", &a.state), ("deny", "1")])
        .await;
    let location = support::header(&res, "location");
    assert!(location.contains("error=access_denied"));
    assert!(location.contains(&format!("state={}", a.client_state())));
    assert!(location.contains("iss=https%3A%2F%2Fmem.example.com"));
    assert_eq!(a.count_codes(), 0);
    assert_eq!(a.count_logins(), 0, "a denial ends the login");
}

// ── The redirect URI, compared exactly ──────────────────────────────────────

/// RFC 8252 section 7.3 lets an authorization server ignore the port of a
/// loopback redirect URI. This server does not, and
/// `mcpmem_oauth::registration::is_acceptable_redirect_uri` records the
/// decision and its price: a client that outlives its listener must register
/// again for the port it binds next.
#[tokio::test]
async fn a_loopback_redirect_uri_differing_only_in_port_is_refused() {
    let server = support::oauth_server().await;
    let client_id = flow::register(&server, flow::CLIENT_NAME, "http://127.0.0.1:41234/cb").await;
    let query = flow::query_string(&[
        ("response_type", "code"),
        ("client_id", &client_id),
        ("redirect_uri", "http://127.0.0.1:41235/cb"),
        ("code_challenge", &flow::code_challenge()),
        ("code_challenge_method", "S256"),
        ("scope", "graph-read"),
    ]);
    let res = server
        .request(
            axum::http::Request::get(format!("/oauth/authorize?{query}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(res.headers().get("location").is_none());
}

// ── Rendering ───────────────────────────────────────────────────────────────

#[test]
fn a_hostile_client_name_is_escaped() {
    let html = page(
        "<script>alert(1)</script>",
        "adam@example.com",
        &["graph-read".to_string()],
        "csrf-value",
        "state-value",
    );
    assert!(!html.contains("<script>alert(1)</script>"));
    assert!(html.contains("&lt;script&gt;"));
}

/// A value substituted into the page must not be substituted into again. A
/// client name that is itself a placeholder is the whole attack: a second pass
/// over the template would fill it in with whatever that placeholder names.
#[test]
fn a_client_name_that_looks_like_a_placeholder_stays_text() {
    let html = page(
        "{{csrf}}",
        "{{state}}",
        &["graph-read".to_string()],
        "the-secret-token",
        "the-login-state",
    );
    assert!(
        !html.contains("the-secret-token{{"),
        "the token appears once, in its own field"
    );
    assert_eq!(
        html.matches("the-secret-token").count(),
        1,
        "the client name must not be filled in as a placeholder: {html}"
    );
    assert_eq!(html.matches("the-login-state").count(), 1);
}

/// A placeholder the code never fills would ship to the human as source text.
#[test]
fn the_rendered_page_leaves_no_placeholder_behind() {
    let html = page(
        "Test client",
        "adam@example.com",
        &["graph-read".to_string(), "graph-write".to_string()],
        "csrf-value",
        "state-value",
    );
    assert!(!html.contains("{{"), "unfilled placeholder in: {html}");
}

#[test]
fn escape_html_covers_every_dangerous_character() {
    assert_eq!(escape_html(r#"<>&"'"#), "&lt;&gt;&amp;&quot;&#39;");
}
