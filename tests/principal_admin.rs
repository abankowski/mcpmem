#![cfg(feature = "oauth")]
//! The approval waitlist: an unknown human is recorded at the callback and
//! sees the pending page while the waitlist is on, and the refusal page while
//! it is off. Task 7 extends this file with the admin API over the same
//! tables.
//!
//! The walk is the oauth_upstream one: authorize redirects to the fake
//! provider, `FakeIdp::login` follows it and signs an identity token, and the
//! callback answers with this server's page. The provider authenticates
//! `sub-1`; each test configures this server to allow somebody else, so the
//! callback is exactly the unknown-human branch.

use axum::body::Body;
use axum::http::{Request, StatusCode};

mod support;

/// The authorization request for the waitlist tests: one accepted request,
/// with a registered redirect and a PKCE challenge matching the verifier the
/// fake flow uses.
fn authorize_params(client_id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("response_type", "code".into()),
        ("client_id", client_id.into()),
        ("redirect_uri", format!("{}/cb", support::PUBLIC_URL)),
        ("scope", "graph-read".into()),
        ("state", "st".into()),
        ("code_challenge", support::flow::code_challenge()),
        ("code_challenge_method", "S256".into()),
    ]
}

fn as_pairs<'a>(params: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    params.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

#[tokio::test]
async fn an_unknown_human_is_recorded_and_sees_pending_when_waitlist_is_on() {
    let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default()).await;
    let mut config = support::oauth_config(&idp.issuer);
    config.approval_waitlist = true;
    // The provider authenticates sub-1; this server allows somebody else.
    config.principals[0].sub = "another-human".into();
    let server = support::server(Some(config), support::Scopes::all(), None).await;
    let client_id = support::flow::register(
        &server,
        "waitlist-test",
        &format!("{}/cb", support::PUBLIC_URL),
    )
    .await;

    let params = authorize_params(&client_id);
    let res = server
        .request(
            Request::get(format!(
                "/oauth/authorize?{}",
                support::flow::query_string(&as_pairs(&params))
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    let location = support::header(&res, "location");
    let back = idp.login(&location).await;
    let hop = server
        .request(
            Request::get(format!(
                "/oauth/callback?code={}&state={}",
                back.code, back.state
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(hop.status(), StatusCode::OK, "pending page, not a refusal");
    let text = support::flow::body_text(hop).await;
    assert!(text.contains("Sign-in pending"), "pending page names the state");
    assert_eq!(
        support::flow::count_rows(&server, "principal_waitlist"),
        1,
        "the unknown human must be on the waitlist"
    );
}

#[tokio::test]
async fn an_unknown_human_is_refused_when_waitlist_is_off() {
    let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default()).await;
    let mut config = support::oauth_config(&idp.issuer);
    // approval_waitlist stays false (the default).
    config.principals[0].sub = "another-human".into();
    let server = support::server(Some(config), support::Scopes::all(), None).await;
    let client_id = support::flow::register(
        &server,
        "refusal-test",
        &format!("{}/cb", support::PUBLIC_URL),
    )
    .await;

    let params = authorize_params(&client_id);
    let res = server
        .request(
            Request::get(format!(
                "/oauth/authorize?{}",
                support::flow::query_string(&as_pairs(&params))
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    let location = support::header(&res, "location");
    let back = idp.login(&location).await;
    let hop = server
        .request(
            Request::get(format!(
                "/oauth/callback?code={}&state={}",
                back.code, back.state
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(hop.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        support::flow::count_rows(&server, "principal_waitlist"),
        0,
        "a refusal must not write a waitlist row"
    );
}