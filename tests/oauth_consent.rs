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
use support::flow::{self, Authorized, authorize_to_consent};

// ── The page ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_consent_page_offers_only_the_intersection() {
    // The principal in `support::principals` holds graph-read and graph-write.
    let a: Authorized = authorize_to_consent("graph-read graph-write code").await;
    assert!(a.body.contains(r#"value="graph-read""#));
    assert!(a.body.contains(r#"value="graph-write""#));
    assert!(!a.body.contains(r#"value="code""#), "code was not granted");
}

/// The client name arrives from an unauthenticated registration request and
/// reaches the page this server serves, so the escaping has to hold on the
/// route and not only in the function that renders it.
#[tokio::test]
async fn the_served_page_escapes_a_hostile_client_name() {
    let a = flow::authorize_to_consent_named("<script>alert(1)</script>", "graph-read").await;
    assert!(!a.body.contains("<script>alert(1)</script>"));
    assert!(a.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
}

/// A human who holds none of the scopes the client asked for has nothing to
/// consent to. The callback refuses, with the one page every refusal there
/// gets, and leaves no login for a later approval to find.
#[tokio::test]
async fn a_request_naming_only_scopes_the_principal_lacks_never_reaches_consent() {
    let c = flow::authorize_to_callback(flow::CLIENT_NAME, "code").await;
    assert_eq!(c.response.status(), StatusCode::FORBIDDEN);
    assert!(c.response.headers().get("location").is_none());
    assert_eq!(c.count_logins(), 0, "the refused login is consumed");
}

// ── Approval ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn approval_issues_a_code_for_only_the_approved_scopes() {
    let a = authorize_to_consent("graph-read graph-write").await;
    let res = a
        .post_consent(&[
            ("csrf", &a.csrf),
            ("state", &a.state),
            ("scope", "graph-read"),
        ])
        .await;
    assert_eq!(res.status(), StatusCode::FOUND);
    let code = flow::code_from(&support::header(&res, "location"));
    let stored = a.take_code(&code);
    assert_eq!(stored.grant.scopes, vec!["graph-read"]);
}

/// Task 8 exchanges the code, and every value it checks is written here. A
/// grant that names the wrong client, resource, redirect URI or challenge is a
/// code the right client cannot spend — or the wrong one can.
#[tokio::test]
async fn the_code_grant_binds_the_client_the_resource_and_the_challenge() {
    let a = authorize_to_consent("graph-read graph-write").await;
    let res = a
        .post_consent(&[
            ("csrf", &a.csrf),
            ("state", &a.state),
            ("scope", "graph-write"),
            ("scope", "graph-read"),
        ])
        .await;
    assert_eq!(res.status(), StatusCode::FOUND);
    let stored = a.take_code(&flow::code_from(&support::header(&res, "location")));
    assert_eq!(stored.grant.client_id, a.client_id);
    assert_eq!(stored.grant.principal, "adam");
    assert_eq!(stored.grant.resource, "https://mem.example.com/mcp");
    assert!(
        !stored.grant.family.is_empty(),
        "a code opens a token family"
    );
    assert_eq!(stored.redirect_uri, flow::CLIENT_REDIRECT);
    assert_eq!(stored.code_challenge, flow::CODE_CHALLENGE);
    // The offered order, not the order the form happened to send.
    assert_eq!(stored.grant.scopes, vec!["graph-read", "graph-write"]);
}

#[tokio::test]
async fn the_redirect_carries_the_client_state_and_the_iss_parameter() {
    let a = authorize_to_consent("graph-read").await;
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
    assert!(location.contains(&format!("state={}", a.client_state)));
    assert!(location.contains("iss=https%3A%2F%2Fmem.example.com"));
}

#[tokio::test]
async fn a_second_approval_finds_no_login_row() {
    let a = authorize_to_consent("graph-read").await;
    let first = a
        .post_consent(&[
            ("csrf", &a.csrf),
            ("state", &a.state),
            ("scope", "graph-read"),
        ])
        .await;
    assert_eq!(first.status(), StatusCode::FOUND);
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
    let a = authorize_to_consent("graph-read").await;
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
    let a = authorize_to_consent("graph-read").await;
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
    let a = authorize_to_consent("graph-read code").await;
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
    let a = authorize_to_consent("graph-read").await;
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
    let a = authorize_to_consent("graph-read").await;
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
    let a = authorize_to_consent("graph-read").await;
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
    assert!(location.contains(&format!("state={}", a.client_state)));
    assert!(location.contains("iss=https%3A%2F%2Fmem.example.com"));
    assert_eq!(a.count_codes(), 0);
    assert_eq!(a.count_logins(), 0, "a denial ends the login");
}

// ── The redirect URI, compared exactly ──────────────────────────────────────

/// RFC 8252 section 7.3 lets an authorization server ignore the port of a
/// loopback redirect URI. This server does not: registration is dynamic, so a
/// native client registers the port it has just bound, and the comparison stays
/// one exact string match with nothing to reason about.
#[tokio::test]
async fn a_loopback_redirect_uri_differing_only_in_port_is_refused() {
    let server = support::oauth_server().await;
    let client_id = flow::register(&server, flow::CLIENT_NAME, "http://127.0.0.1:41234/cb").await;
    let res = server
        .request(flow::authorize_request(
            &client_id,
            "http://127.0.0.1:41235/cb",
            "graph-read",
        ))
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
