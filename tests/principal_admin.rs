#![cfg(feature = "oauth")]
//! The approval waitlist: an unknown human is recorded at the callback and
//! sees the pending page while the waitlist is on, and the refusal page while
//! it is off. Task 7 adds the admin API over the same tables: principal CRUD
//! with built-ins immutable, the waitlist list, approve and dismiss, and
//! revoke-on-delete.
//!
//! The walk is the oauth_upstream one: authorize redirects to the fake
//! provider, `FakeIdp::login` follows it and signs an identity token, and the
//! callback answers with this server's page. The provider authenticates
//! `sub-1`; each waitlist test configures this server to allow somebody else,
//! so the callback is exactly the unknown-human branch.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};

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

/// A server whose principal[0] holds the admin scope, and a live admin token.
///
/// The token is walked: the admin-UI client registers, the authorize request
/// redirects through the fake provider, the callback renders the consent
/// page, the form is approved, and the code is exchanged.
async fn admin_server() -> (support::Server, String) {
    let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default()).await;
    let mut config = support::oauth_config(&idp.issuer);
    config.principals[0].scopes.push(mcpmem::principals::ADMIN_SCOPE.into());
    let server = support::server(Some(config), support::Scopes::all(), None).await;
    let token = support::flow::admin_access_token(&idp, &server).await;
    (server, token)
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

fn bearer_get(token: &str, path: &str) -> Request<Body> {
    Request::get(path)
        .header(header::AUTHORIZATION, bearer(token))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn an_anonymous_caller_is_challenged_not_refused() {
    let (server, _token) = admin_server().await;
    let res = server
        .request(Request::get("/ui/api/principals").body(Body::empty()).unwrap())
        .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let challenge = res
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .expect("the 401 is the RFC 6750 challenge")
        .to_str()
        .unwrap();
    assert!(
        challenge.starts_with("Bearer"),
        "the challenge is an RFC 6750 bearer challenge: {challenge}"
    );
}

#[tokio::test]
async fn list_marks_builtins_immutable_and_lists_runtime_rows() {
    let (server, token) = admin_server().await;
    let res = server.request(bearer_get(&token, "/ui/api/principals")).await;
    assert_eq!(res.status(), 200);
    let body = support::json(res).await;
    let items = body["principals"].as_array().unwrap();
    assert!(items.iter().any(|p| p["builtin"].as_bool() == Some(true)));
    assert!(
        items.iter().any(|p| p["name"] == "adam"),
        "the configured built-in is listed"
    );
    assert!(
        items.iter().all(|p| p.get("maskedByBuiltin").is_some()),
        "every view serializes the mask flag under maskedByBuiltin"
    );
    // The built-in owns an identity an admin cannot touch: PATCH and
    // DELETE on its id are refused.
    let builtin = items
        .iter()
        .find(|p| p["builtin"].as_bool() == Some(true))
        .unwrap();
    let id = builtin["id"].as_str().unwrap();
    let patch = server
        .request(
            Request::patch(format!("/ui/api/principals/{id}"))
                .header(header::AUTHORIZATION, bearer(&token))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"hacked"}"#.to_owned()))
                .unwrap(),
        )
        .await;
    assert_eq!(patch.status(), 409, "a built-in principal is immutable");
    let del = server
        .request(
            Request::delete(format!("/ui/api/principals/{id}"))
                .header(header::AUTHORIZATION, bearer(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(del.status(), 409, "a built-in principal is immutable");
}

#[tokio::test]
async fn create_update_delete_round_trip_and_delete_revokes() {
    let (server, token) = admin_server().await;

    // A name a real login could never mint tokens for: "ada" has no OAuth
    // client and no code. The family is planted the way the store writes
    // every minted token, so the delete's revoke has a family to reach.
    let family = mcpmem_oauth::new_token();
    let now = (server.oauth().now_us)();
    support::flow::with_store(&server, |store| {
        store
            .put_token(
                &family,
                mcpmem_oauth::store::TokenKind::Refresh,
                &mcpmem_oauth::store::Grant {
                    client_id: "c-ada".into(),
                    principal: "ada".into(),
                    scopes: vec!["graph-read".into()],
                    resource: format!("{}/mcp", support::PUBLIC_URL),
                    family: "ada-fam".into(),
                },
                now,
                now + 60 * 60 * 1_000_000,
            )
            .expect("the store writes the planted token");
    });

    let create = server
        .request(
            Request::post("/ui/api/principals")
                .header(header::AUTHORIZATION, bearer(&token))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"ada","iss":"https://idp.example","sub":"u-9","scopes":["graph-read"]}"#
                        .to_owned(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(create.status(), 201);
    let view = support::json(create).await;
    let id = view["id"].as_str().unwrap().to_owned();

    // The same identity again is a duplicate: the second create is refused
    // before the store is touched, with 409 and not the constraint error.
    let dup = server
        .request(
            Request::post("/ui/api/principals")
                .header(header::AUTHORIZATION, bearer(&token))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"ada","iss":"https://idp.example","sub":"u-9","scopes":["graph-read"]}"#
                        .to_owned(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(dup.status(), 409, "a duplicate runtime key is refused");

    // The runtime row lists under the same camelCase key, unmasked.
    let listed = server.request(bearer_get(&token, "/ui/api/principals")).await;
    let listed = support::json(listed).await;
    let row = listed["principals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"].as_str() == Some(&id))
        .expect("the created principal is listed");
    assert_eq!(row["maskedByBuiltin"], false);
    assert_eq!(row["builtin"], false);

    let patch = server
        .request(
            Request::patch(format!("/ui/api/principals/{id}"))
                .header(header::AUTHORIZATION, bearer(&token))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"scopes":["graph-read","graph-write"]}"#.to_owned()))
                .unwrap(),
        )
        .await;
    assert_eq!(patch.status(), 200);
    assert_eq!(support::json(patch).await["scopes"][1], "graph-write");

    let del = server
        .request(
            Request::delete(format!("/ui/api/principals/{id}"))
                .header(header::AUTHORIZATION, bearer(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(del.status(), 204);

    let outcome = support::flow::with_store(&server, |store| {
        store.take_refresh(&family, now + 2 * 60 * 60 * 1_000_000)
    })
    .expect("the store answers");
    assert!(
        matches!(outcome, mcpmem_oauth::store::RefreshOutcome::Unknown),
        "deleting the principal revoked its token family"
    );
}

#[tokio::test]
async fn a_non_admin_grant_is_refused() {
    let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default()).await;
    let config = support::oauth_config(&idp.issuer);
    // principals[0] holds only graph-read/graph-write — no admin.
    let server = support::server(Some(config), support::Scopes::all(), None).await;
    let token = support::flow::admin_access_token(&idp, &server).await;
    let res = server.request(bearer_get(&token, "/ui/api/principals")).await;
    assert_eq!(res.status(), 403);
    let challenge = res
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .expect("the 403 is the insufficient-scope challenge")
        .to_str()
        .unwrap();
    assert!(
        challenge.contains("insufficient_scope"),
        "the challenge names the refusal: {challenge}"
    );
}

/// `GET /ui/admin` and its two assets are static, like the viewer: the shell
/// and the stylesheet/script hold no data, so they are served without auth.
/// The JSON endpoints are the gate.
#[tokio::test]
async fn the_admin_page_and_assets_are_served() {
    let (server, _token) = admin_server().await;
    for (path, kind) in [
        ("/ui/admin", "text/html"),
        ("/ui/admin.js", "text/javascript"),
        ("/ui/admin.css", "text/css"),
    ] {
        let res = server
            .request(Request::get(path).body(Body::empty()).unwrap())
            .await;
        assert_eq!(res.status(), 200, "{path} serves");
        let ct = res
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.starts_with(kind), "{path} content type is {ct}");
    }
}

/// Register a waitlist-test client and walk one login that lands on the
/// pending page, the way Task 6's first test does. The caller then holds the
/// waitlist row for sub-1.
async fn walk_unknown_human_into_pending(
    idp: &support::fake_idp::FakeIdp,
    server: &support::Server,
) {
    let client_id = support::flow::register(
        server,
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
}

/// The one waitlist entry of `server`.
async fn waitlist_entry(server: &support::Server, token: &str) -> serde_json::Value {
    let res = server.request(bearer_get(token, "/ui/api/waitlist")).await;
    assert_eq!(res.status(), 200);
    let body = support::json(res).await;
    let entries = body["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "exactly the one walk is waitlisted");
    entries[0].clone()
}

#[tokio::test]
async fn waitlist_approve_promotes_and_dismiss_discards() {
    let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default()).await;
    let mut config = support::oauth_config(&idp.issuer);
    config.approval_waitlist = true;
    config.principals[0].scopes.push(mcpmem::principals::ADMIN_SCOPE.into());
    // The provider authenticates sub-1; this server allows somebody else, so
    // sub-1's login lands on the waitlist. That also means no login on this
    // server can reach the consent page — the fake provider authenticates one
    // subject only — so the admin token is planted rather than walked.
    config.principals[0].sub = "another-human".into();
    let server = support::server(Some(config), support::Scopes::all(), None).await;
    let token = support::flow::plant_admin_token(&server);

    walk_unknown_human_into_pending(&idp, &server).await;
    assert_eq!(support::flow::count_rows(&server, "principal_waitlist"), 1);

    let entry = waitlist_entry(&server, &token).await;
    let id = entry["id"].as_str().unwrap().to_owned();
    // The FakeIdp always stamps an email, so the entry name is the email.
    assert_eq!(entry["sub"], "sub-1");
    assert_eq!(entry["name"], support::fake_idp::EMAIL);

    let approve = server
        .request(
            Request::post(format!("/ui/api/waitlist/{id}/approve"))
                .header(header::AUTHORIZATION, bearer(&token))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"scopes":["graph-read"]}"#.to_owned()))
                .unwrap(),
        )
        .await;
    assert_eq!(approve.status(), 201);
    assert_eq!(support::json(approve).await["scopes"][0], "graph-read");
    assert_eq!(
        support::flow::count_rows(&server, "principal_waitlist"),
        0,
        "approve consumes the row"
    );

    // The promoted sub-1 now logs in normally: consent page, not pending.
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
    assert_eq!(hop.status(), StatusCode::OK, "the promoted human is not pending");
    let text = support::flow::body_text(hop).await;
    assert!(
        text.contains("name=\"scope\" value=\"graph-read\""),
        "the promoted human reaches the consent page"
    );
    assert_eq!(
        support::flow::count_rows(&server, "principal_waitlist"),
        0,
        "the second login is not waitlisted"
    );
}

#[tokio::test]
async fn waitlist_dismiss_discards_without_promoting() {
    let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default()).await;
    let mut config = support::oauth_config(&idp.issuer);
    config.approval_waitlist = true;
    config.principals[0].scopes.push(mcpmem::principals::ADMIN_SCOPE.into());
    config.principals[0].sub = "another-human".into();
    let server = support::server(Some(config), support::Scopes::all(), None).await;
    let token = support::flow::plant_admin_token(&server);

    walk_unknown_human_into_pending(&idp, &server).await;
    let entry = waitlist_entry(&server, &token).await;
    let id = entry["id"].as_str().unwrap().to_owned();

    let del = server
        .request(
            Request::delete(format!("/ui/api/waitlist/{id}"))
                .header(header::AUTHORIZATION, bearer(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(del.status(), 204);
    assert_eq!(
        support::flow::count_rows(&server, "principal_waitlist"),
        0,
        "dismiss removes the row"
    );
    assert_eq!(
        support::flow::count_rows(&server, "runtime_principal"),
        0,
        "dismiss must not create a principal"
    );
}