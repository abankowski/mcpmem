#![cfg(feature = "oauth")]
//! The token endpoint, and the transport that consumes what it issues.
//!
//! One test walks the whole path a connector walks, from the 401 that tells it
//! this server has an authorization server to a `tools/list` answered under an
//! issued token. The rest are the refusals: a code that cannot be redeemed, a
//! token that names another resource, a refresh token presented twice, a scope
//! the human never granted.
//!
//! Every flow here is the real one. `support::flow` registers a client, sends a
//! real authorization request, walks `support::fake_idp`, comes back through
//! the callback and posts a real consent form, so the code these tests spend is
//! the code a browser would have delivered.
//!
//! One `Server` at a time per thread: each flow is dropped before the next is
//! built, which is why no test here holds two at once.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;

mod support;
use support::flow::{self, Authorized, CODE_VERIFIER, Flow, Tokens};
use support::{PUBLIC_URL, header, json as json_body};

/// The resource identifier this server protects. One spelling here, against
/// the one spelling in `OauthState::resource`.
fn resource() -> String {
    format!("{PUBLIC_URL}/mcp")
}

/// The metadata URL the challenge points a client at.
fn resource_metadata() -> String {
    format!("{PUBLIC_URL}/.well-known/oauth-protected-resource")
}

/// A `POST /mcp` body a caller with any scope may send.
const TOOLS_LIST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

/// A flow parked on an authorization code, and the code itself. It asks for
/// exactly `scopes` and the human grants exactly `scopes`.
async fn to_code(scopes: &[&str]) -> (Authorized, String) {
    let authorized = Flow::new(&scopes.join(" ")).into_consent().await;
    let code = authorized.approve(scopes).await;
    (authorized, code)
}

/// A flow holding a live access token and the refresh token beside it.
async fn to_tokens(scopes: &[&str]) -> (Authorized, Tokens) {
    let (authorized, code) = to_code(scopes).await;
    let tokens = authorized.exchange(&code).await;
    (authorized, tokens)
}

// ── The whole path ──────────────────────────────────────────────────────────

/// Every hop a connector makes, in the order it makes them, with an assertion
/// on each answer.
///
/// One test rather than ten, because each hop consumes what the one before it
/// produced: there is no way to assert on the token exchange without having
/// walked to a code, and splitting the walk would only hide that behind a
/// fixture.
#[tokio::test]
async fn a_client_discovers_this_server_registers_and_gets_a_scoped_token() {
    let flow = Flow::new("graph-read graph-write").start().await;

    // 1. An anonymous call is refused, and the refusal says where to look.
    let challenge = flow
        .server()
        .request(
            Request::post("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(TOOLS_LIST))
                .unwrap(),
        )
        .await;
    assert_eq!(challenge.status(), StatusCode::UNAUTHORIZED);
    let www_authenticate = header(&challenge, "www-authenticate");
    assert!(
        www_authenticate.contains(&format!("resource_metadata=\"{}\"", resource_metadata())),
        "{www_authenticate}"
    );

    // 2. The document that header names.
    let res = flow
        .server()
        .request(
            Request::get("/.well-known/oauth-protected-resource")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let document = json_body(res).await;
    assert_eq!(document["resource"], resource());
    assert_eq!(document["authorization_servers"][0], PUBLIC_URL);

    // 3. The authorization server it points at.
    let res = flow
        .server()
        .request(
            Request::get("/.well-known/oauth-authorization-server")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let document = json_body(res).await;
    assert_eq!(document["issuer"], PUBLIC_URL);
    assert_eq!(
        document["token_endpoint"],
        format!("{PUBLIC_URL}/oauth/token")
    );

    // 4. Registration, which is open and hands back an identifier.
    let client_id = flow.register().await;
    assert!(!client_id.is_empty());

    // 5. The authorization request goes to the upstream provider.
    let res = flow.authorize(&client_id).await;
    assert_eq!(res.status(), StatusCode::FOUND);
    let upstream = header(&res, "location");
    assert!(
        upstream.starts_with(&flow.idp().issuer),
        "the login must go to the configured provider: {upstream}"
    );

    // 6. The provider authenticates the human and sends them back.
    let back = flow.idp().login(&upstream).await;

    // 7. The callback names the human and asks them what to grant.
    let res = flow.callback(&back.code, &back.state).await;
    assert_eq!(res.status(), StatusCode::OK);
    let authorized = flow
        .into_stage(res, back.state, client_id)
        .into_consent()
        .await;
    assert!(!authorized.csrf.is_empty(), "the page carries a csrf token");

    // 8. The human grants one of the two scopes they were offered.
    let code = authorized.approve(&["graph-read"]).await;

    // 9. The code becomes a token pair.
    let res = authorized.token(&[("code", code.as_str())]).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["token_type"], "Bearer");
    assert_eq!(res.body["expires_in"], 3600);
    assert_eq!(res.body["scope"], "graph-read");
    assert!(res.body["refresh_token"].is_string(), "{}", res.body);
    let access_token = res.body["access_token"]
        .as_str()
        .expect("the response carries an access token")
        .to_owned();

    // 10. The transport answers under that token, and lists only what the
    //     human granted — not the graph-write half the client asked for.
    let names = authorized.tool_names(&access_token).await;
    assert!(names.contains(&"read_graph".to_owned()), "{names:?}");
    assert!(!names.contains(&"delete_entities".to_owned()), "{names:?}");
}

// ── The authorization code grant ────────────────────────────────────────────

#[tokio::test]
async fn a_wrong_code_verifier_is_refused() {
    let (authorized, code) = to_code(&["graph-read"]).await;
    let res = authorized
        .token(&[
            ("code", code.as_str()),
            ("code_verifier", "another-verifier"),
        ])
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.body["error"], "invalid_grant");
}

/// RFC 6749 section 4.1.2, both halves: a code used more than once is denied,
/// **and** the tokens already issued from it are revoked.
///
/// A code reaches the client through a browser redirect, so it lands in
/// history, in a referrer and in every proxy log on the path. A second
/// presentation is the only evidence this server ever gets that the code
/// leaked, and by then one of the two parties is holding a live pair. The
/// refresh path treats the same signal the same way.
#[tokio::test]
async fn a_replayed_authorization_code_is_refused_and_kills_the_family() {
    let (authorized, code) = to_code(&["graph-read"]).await;
    let first = authorized.token(&[("code", code.as_str())]).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let access = first.body["access_token"]
        .as_str()
        .expect("the exchange carries an access token")
        .to_owned();
    let refresh = first.body["refresh_token"]
        .as_str()
        .expect("the exchange carries a refresh token")
        .to_owned();
    assert_eq!(
        authorized.mcp_tools_list(&access).await.status,
        StatusCode::OK,
        "the winner's token works before the replay"
    );

    let second = authorized.token(&[("code", code.as_str())]).await;
    assert_eq!(second.status, StatusCode::BAD_REQUEST);
    assert_eq!(second.body["error"], "invalid_grant");

    assert_eq!(
        authorized.mcp_tools_list(&access).await.status,
        StatusCode::UNAUTHORIZED,
        "the access token the first exchange issued must die with the family"
    );
    let after = authorized.refresh(&refresh).await;
    assert_eq!(after.status, StatusCode::BAD_REQUEST);
    assert_eq!(after.body["error"], "invalid_grant");
}

/// `mcpmem_oauth::consent::CODE_TTL_US` is sixty seconds, so a code is dead at
/// sixty-one. The clock moves; nothing sleeps.
#[tokio::test]
async fn an_expired_authorization_code_is_refused() {
    let (authorized, code) = to_code(&["graph-read"]).await;
    authorized.clock().advance_seconds(61);
    let res = authorized.token(&[("code", code.as_str())]).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.body["error"], "invalid_grant");
}

/// One character, the case of the last letter of the path. RFC 6749 section
/// 4.1.3 compares the two redirect URIs, and a comparison that normalises
/// anything is a comparison an attacker picks the normalisation of.
#[tokio::test]
async fn a_redirect_uri_that_differs_by_one_character_is_refused() {
    let (authorized, code) = to_code(&["graph-read"]).await;
    let res = authorized
        .token(&[
            ("code", code.as_str()),
            ("redirect_uri", "https://claude.ai/api/mcp/auth_callbacK"),
        ])
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.body["error"], "invalid_grant");
}

/// The code is bound to the client the authorization request named, so a
/// second registered client cannot redeem it — and registration is open, so
/// standing that second client up costs an attacker one request.
#[tokio::test]
async fn a_code_redeemed_by_another_client_is_refused() {
    let (authorized, code) = to_code(&["graph-read"]).await;
    let other = flow::register(authorized.server(), "Another client", flow::CLIENT_REDIRECT).await;
    let res = authorized
        .token(&[("code", code.as_str()), ("client_id", other.as_str())])
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.body["error"], "invalid_grant");
}

/// This server advertises two grant types and issues no others. A third that
/// merely fell through to the code path would be a grant nobody reviewed.
#[tokio::test]
async fn an_unknown_grant_type_is_refused() {
    let (authorized, code) = to_code(&["graph-read"]).await;
    let res = authorized
        .token(&[("code", code.as_str()), ("grant_type", "password")])
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.body["error"], "unsupported_grant_type");
}

/// RFC 6749 section 3.1 forbids a repeated parameter. A server that silently
/// takes the first or the last is a server two parties can read differently,
/// and the parameter worth smuggling is the verifier.
#[tokio::test]
async fn a_repeated_parameter_is_refused() {
    let (authorized, code) = to_code(&["graph-read"]).await;
    let res = authorized
        .post_form(
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("client_id", authorized.client_id.as_str()),
                ("redirect_uri", authorized.redirect_uri.as_str()),
                ("code", code.as_str()),
                ("code_verifier", CODE_VERIFIER),
                ("code_verifier", "another-verifier"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.body["error"], "invalid_request");
}

/// The same request without the repeat, so the test above is about the repeat
/// and not about the shape of the form.
#[tokio::test]
async fn the_same_form_without_the_repeat_is_accepted() {
    let (authorized, code) = to_code(&["graph-read"]).await;
    let res = authorized
        .post_form(
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("client_id", authorized.client_id.as_str()),
                ("redirect_uri", authorized.redirect_uri.as_str()),
                ("code", code.as_str()),
                ("code_verifier", CODE_VERIFIER),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

/// RFC 8707: a request that names a resource this server does not protect is
/// refused before a human is ever shown a page, because the token it would
/// lead to could not be bound to what the client asked for.
#[tokio::test]
async fn an_authorize_request_naming_another_resource_is_refused() {
    let flow = Flow::new("graph-read")
        .resource("https://other.example/mcp")
        .start()
        .await;
    let client_id = flow.register().await;
    let res = flow.authorize(&client_id).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

// ── The refresh grant ───────────────────────────────────────────────────────

/// RFC 9700 section 4.14.2: a rotated refresh token presented twice means one
/// of the two holders is not the client, and the server cannot tell which. So
/// both die, and so does everything else in the family — including the access
/// token the legitimate first refresh just received.
#[tokio::test]
async fn a_reused_refresh_token_kills_the_family() {
    let (authorized, tokens) = to_tokens(&["graph-read"]).await;
    let first = authorized.refresh(&tokens.refresh_token).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let new_access = first.body["access_token"]
        .as_str()
        .expect("the refresh carries an access token")
        .to_owned();
    assert_eq!(
        authorized.mcp_tools_list(&new_access).await.status,
        StatusCode::OK,
        "the refreshed token works before the replay"
    );

    let replay = authorized.refresh(&tokens.refresh_token).await;
    assert_eq!(replay.status, StatusCode::BAD_REQUEST);
    assert_eq!(replay.body["error"], "invalid_grant");

    assert_eq!(
        authorized.mcp_tools_list(&new_access).await.status,
        StatusCode::UNAUTHORIZED,
        "the whole family must die, including the token the first refresh issued"
    );
}

/// A refresh outlives the access token it replaces, which is the only reason
/// the grant exists: thirty days against one hour. At an hour and a second the
/// access token is dead and the refresh still answers.
#[tokio::test]
async fn a_refresh_token_outlives_the_access_token_it_replaces() {
    let (authorized, tokens) = to_tokens(&["graph-read"]).await;
    authorized.clock().advance_seconds(3601);
    assert_eq!(
        authorized.mcp_tools_list(&tokens.access_token).await.status,
        StatusCode::UNAUTHORIZED,
        "an access token lives one hour"
    );

    let res = authorized.refresh(&tokens.refresh_token).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let access = res.body["access_token"]
        .as_str()
        .expect("the refresh carries an access token")
        .to_owned();
    assert_eq!(
        authorized.mcp_tools_list(&access).await.status,
        StatusCode::OK
    );
}

/// A live refresh token presented by a client it was not issued to is the
/// same signal as a replay: two parties hold it and this server cannot tell
/// which one is the client. Registration is open, so standing up the second
/// client costs an attacker one request — which is exactly why the grant's
/// own `client_id` has to be checked rather than assumed.
#[tokio::test]
async fn a_refresh_token_presented_by_another_client_kills_the_family() {
    let (authorized, tokens) = to_tokens(&["graph-read"]).await;
    let other = flow::register(authorized.server(), "Another client", flow::CLIENT_REDIRECT).await;

    let res = authorized
        .post_form(
            "/oauth/token",
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", tokens.refresh_token.as_str()),
                ("client_id", other.as_str()),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.body["error"], "invalid_grant");

    assert_eq!(
        authorized.mcp_tools_list(&tokens.access_token).await.status,
        StatusCode::UNAUTHORIZED,
        "the family the presented token belongs to must be dead"
    );
}

/// The refresh carries the grant forward unchanged. One that re-derived the
/// scopes from anywhere but the spent token could widen them, and the human is
/// no longer at the keyboard to be asked.
#[tokio::test]
async fn a_refresh_carries_the_granted_scopes_and_nothing_more() {
    let (authorized, tokens) = to_tokens(&["graph-read"]).await;
    let res = authorized.refresh(&tokens.refresh_token).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["scope"], "graph-read");

    let access = res.body["access_token"]
        .as_str()
        .expect("the refresh carries an access token")
        .to_owned();
    let names = authorized.tool_names(&access).await;
    assert!(names.contains(&"read_graph".to_owned()), "{names:?}");
    assert!(!names.contains(&"delete_entities".to_owned()), "{names:?}");
}

#[tokio::test]
async fn an_unknown_refresh_token_is_refused() {
    let (authorized, _tokens) = to_tokens(&["graph-read"]).await;
    let res = authorized.refresh("not-a-refresh-token").await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.body["error"], "invalid_grant");
}

/// An access token is not a refresh token, whatever family it belongs to.
/// Without the kind check the one-hour credential would buy a thirty-day one.
#[tokio::test]
async fn an_access_token_cannot_be_refreshed() {
    let (authorized, tokens) = to_tokens(&["graph-read"]).await;
    let res = authorized.refresh(&tokens.access_token).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.body["error"], "invalid_grant");
}

// ── The resource server ─────────────────────────────────────────────────────

/// A token this server would otherwise accept, carrying another audience.
///
/// RFC 8707 exists so a token stolen from one resource cannot be spent at
/// another, and the check has to be on the request path: a store that hands
/// back the grant and a transport that does not read its `resource` is the
/// same as no check at all.
#[tokio::test]
async fn a_token_bound_to_another_resource_is_refused() {
    let (authorized, tokens) = to_tokens(&["graph-read"]).await;
    assert_eq!(
        authorized.mcp_tools_list(&tokens.access_token).await.status,
        StatusCode::OK,
        "the control must pass before the audience is rewritten"
    );

    authorized.rebind_resource("https://other.example/mcp");
    let res = authorized.mcp_tools_list(&tokens.access_token).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

/// The 403 a connector parses. The order and the spelling are the contract:
/// this assertion is the definition of the header.
#[tokio::test]
async fn a_read_only_token_calling_a_write_tool_gets_403_with_the_scope() {
    let (authorized, tokens) = to_tokens(&["graph-read"]).await;
    let res = authorized
        .mcp_call(
            &tokens.access_token,
            "delete_entities",
            json!({ "entityNames": ["a"] }),
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert_eq!(
        res.www_authenticate,
        format!(
            "Bearer error=\"insufficient_scope\", scope=\"graph-write\", \
             resource_metadata=\"{}\"",
            resource_metadata()
        )
    );
}

/// The list and the gate must agree: a server that advertises a tool it would
/// refuse teaches the model to call it.
#[tokio::test]
async fn a_read_only_token_does_not_see_write_tools_in_the_list() {
    let (authorized, tokens) = to_tokens(&["graph-read"]).await;
    let names = authorized.tool_names(&tokens.access_token).await;
    assert!(names.contains(&"read_graph".to_owned()), "{names:?}");
    assert!(!names.contains(&"delete_entities".to_owned()), "{names:?}");
}

// ── The viewer's data endpoints ─────────────────────────────────────────────
//
// An OAuth token in the `Authorization` header reaches `/ui/graph` and its
// three neighbours. Before Task 8 they answered 401 for every OAuth
// deployment with no static token, so this is the one externally reachable
// authorization path this work adds, and these two tests are its whole
// coverage: `tests/ui_http.rs` spawns the binary without OAuth.
//
// The gate is `authz::allows_tool(principal, "read_graph")`, so the viewer and
// the tool it stands for cannot come to disagree.

#[tokio::test]
async fn a_graph_read_token_may_read_the_viewer_graph() {
    let (authorized, tokens) = to_tokens(&["graph-read"]).await;
    let res = authorized
        .get("/ui/graph", Some(&tokens.access_token))
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body["entities"].is_array(), "{}", res.body);
}

/// The viewer reads the whole graph, so a token that holds only the write
/// scope is refused — the same answer `read_graph` itself would give, and with
/// the same challenge. A scripted viewer client learns what to ask its human
/// for from this header and from nowhere else.
#[tokio::test]
async fn a_write_only_token_is_refused_by_the_viewer() {
    let (authorized, tokens) = to_tokens(&["graph-write"]).await;
    let res = authorized
        .get("/ui/graph", Some(&tokens.access_token))
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert_eq!(
        res.www_authenticate,
        format!(
            "Bearer error=\"insufficient_scope\", scope=\"graph-read\", \
             resource_metadata=\"{}\"",
            resource_metadata()
        )
    );
}

/// And no token at all is the same 401 challenge `/mcp` sends, so a scripted
/// viewer client can discover the authorization server from here too.
#[tokio::test]
async fn the_viewer_refuses_an_anonymous_request_with_the_challenge() {
    let (authorized, _tokens) = to_tokens(&["graph-read"]).await;
    let res = authorized.get("/ui/graph", None).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert!(
        res.www_authenticate.contains("resource_metadata="),
        "{}",
        res.www_authenticate
    );
}

/// Both scopes granted, so the write tool is listed and it answers. Without
/// this the tests above would pass on a transport that refuses everything.
#[tokio::test]
async fn a_token_holding_both_scopes_may_write() {
    let (authorized, tokens) = to_tokens(&["graph-read", "graph-write"]).await;
    let names = authorized.tool_names(&tokens.access_token).await;
    assert!(names.contains(&"delete_entities".to_owned()), "{names:?}");

    let res = authorized
        .mcp_call(
            &tokens.access_token,
            "create_entities",
            json!({ "entities": [
                { "name": "alpha", "entityType": "thing", "observations": [] }
            ] }),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body["result"].is_object(), "{}", res.body);
}

#[tokio::test]
async fn an_unknown_token_is_refused_with_the_challenge() {
    let (authorized, _tokens) = to_tokens(&["graph-read"]).await;
    let res = authorized.mcp_tools_list("not-a-token").await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert!(
        res.www_authenticate.contains("resource_metadata="),
        "{}",
        res.www_authenticate
    );
}

#[tokio::test]
async fn an_expired_access_token_is_refused() {
    let (authorized, tokens) = to_tokens(&["graph-read"]).await;
    authorized.clock().advance_seconds(3601);
    let res = authorized.mcp_tools_list(&tokens.access_token).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

// ── Revocation ──────────────────────────────────────────────────────────────

/// Presenting the **access** token. RFC 7009 section 2.1 lets a server revoke
/// the whole family from either half, and this server does.
#[tokio::test]
async fn revoking_the_access_token_ends_the_session() {
    let (authorized, tokens) = to_tokens(&["graph-read"]).await;
    assert_eq!(
        authorized.mcp_tools_list(&tokens.access_token).await.status,
        StatusCode::OK
    );

    let revoked = authorized.revoke(&tokens.access_token).await;
    assert_eq!(revoked.status, StatusCode::OK);
    assert_eq!(
        authorized.mcp_tools_list(&tokens.access_token).await.status,
        StatusCode::UNAUTHORIZED
    );

    let after = authorized.refresh(&tokens.refresh_token).await;
    assert_eq!(
        after.status,
        StatusCode::BAD_REQUEST,
        "the refresh token is in the family that was revoked"
    );
    assert_eq!(after.body["error"], "invalid_grant");
}

/// Presenting the **refresh** token, which is what a client that has been idle
/// for an hour still holds. Its access token has expired, and
/// `Store::family_of` answers `None` for an expired token, so a logout that
/// presented that one would be a silent no-op.
#[tokio::test]
async fn revoking_the_refresh_token_ends_the_session_after_the_access_token_expired() {
    let (authorized, tokens) = to_tokens(&["graph-read"]).await;
    authorized.clock().advance_seconds(3601);

    let revoked = authorized.revoke(&tokens.refresh_token).await;
    assert_eq!(revoked.status, StatusCode::OK);
    let after = authorized.refresh(&tokens.refresh_token).await;
    assert_eq!(after.status, StatusCode::BAD_REQUEST);
    assert_eq!(after.body["error"], "invalid_grant");
}

/// RFC 7009 section 2.2: a token the server does not recognise is not an
/// error. A client cleaning up after itself must not have to tell the two
/// cases apart, and an error would tell a guesser which guesses were tokens.
#[tokio::test]
async fn revoking_an_unknown_token_succeeds() {
    let (authorized, _tokens) = to_tokens(&["graph-read"]).await;
    let res = authorized.revoke("not-a-token").await;
    assert_eq!(res.status, StatusCode::OK);
}

/// A revocation request with no `token` is malformed, not a no-op. RFC 7009
/// section 2.1 makes the parameter required, and a client whose value went
/// missing must hear about it rather than believe it logged out.
#[tokio::test]
async fn a_revocation_with_no_token_is_refused() {
    let (authorized, _tokens) = to_tokens(&["graph-read"]).await;
    let res = authorized.post_form("/oauth/revoke", &[]).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.body["error"], "invalid_request");
}

// ── The bounds on what an anonymous caller costs ────────────────────────────

/// `POST /oauth/register` writes a client row for whoever asks, so the number
/// of rows one peer may write in a minute is bounded.
///
/// Four things in one test, because each needs the state the one before it
/// left: the allowance is spent, the refusal carries `Retry-After`, a second
/// peer is counted separately, and the window rolls. Splitting them would
/// rebuild the same twenty requests three more times.
#[tokio::test]
async fn registration_is_rate_limited_per_peer() {
    let flow = Flow::fresh().await;
    for i in 0..20 {
        let res = flow.register_from("203.0.113.7").await;
        assert_eq!(res.status, StatusCode::CREATED, "request {i} must pass");
    }

    let blocked = flow.register_from("203.0.113.7").await;
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        !blocked.retry_after.is_empty(),
        "a 429 must say when to come back"
    );

    // Another peer is unaffected by the first peer's window.
    let other = flow.register_from("203.0.113.8").await;
    assert_eq!(other.status, StatusCode::CREATED);

    // The window rolls over.
    flow.server().clock().advance_seconds(61);
    let later = flow.register_from("203.0.113.7").await;
    assert_eq!(later.status, StatusCode::CREATED);
}

/// `GET /oauth/callback` is the sixth endpoint an anonymous caller reaches,
/// and the most expensive: it reads the store on every request, and a request
/// naming a live login makes **this** server call somebody else's provider.
///
/// A state this server never issued is refused without any of that, which is
/// what makes the loop cheap enough to be worth bounding — a caller can send
/// it as fast as the socket allows.
#[tokio::test]
async fn the_callback_is_rate_limited_per_peer() {
    let flow = Flow::fresh().await;
    for i in 0..60 {
        let res = flow
            .callback_from("203.0.113.7", "any-code", "never-issued")
            .await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "request {i} must reach the refusal page"
        );
    }

    let blocked = flow
        .callback_from("203.0.113.7", "any-code", "never-issued")
        .await;
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        !blocked.retry_after.is_empty(),
        "a 429 must say when to come back"
    );

    let other = flow
        .callback_from("203.0.113.8", "any-code", "never-issued")
        .await;
    assert_eq!(other.status, StatusCode::FORBIDDEN);
}

/// The bucket is an address, not the header text that named it.
///
/// Two consequences, one test. Text that is not an address must not mint a
/// bucket at all: a caller that sends a fresh 8 KB string each time would
/// otherwise both bypass the limit and grow the map by the size of its own
/// header. And one address spelled two ways must be one bucket, or the split
/// is a free doubling of the allowance for anyone who knows how RFC 5952
/// compresses a zero run.
#[tokio::test]
async fn the_bucket_is_the_parsed_address_and_nothing_else() {
    let flow = Flow::fresh().await;
    for i in 0..20 {
        let res = flow.register_from("not-an-address").await;
        assert_eq!(res.status, StatusCode::CREATED, "request {i} must pass");
    }
    let blocked = flow.register_from("also-not-an-address").await;
    assert_eq!(
        blocked.status,
        StatusCode::TOO_MANY_REQUESTS,
        "text that is not an address must fall to the shared bucket"
    );

    for i in 0..20 {
        let res = flow.register_from("2001:db8::1").await;
        assert_eq!(res.status, StatusCode::CREATED, "request {i} must pass");
    }
    let blocked = flow
        .register_from("2001:0db8:0000:0000:0000:0000:0000:0001")
        .await;
    assert_eq!(
        blocked.status,
        StatusCode::TOO_MANY_REQUESTS,
        "one address spelled two ways must be one bucket"
    );
}

/// One host is one bucket however the proxy spelled it.
///
/// Three pairs, because each spelling arrives from a real proxy and each was a
/// separate bucket:
///
/// - **Brackets.** `[2001:db8::1]` is how a proxy writes a 16-byte address in
///   this header. An unstripped bracket fails the parse outright, so the
///   caller falls back to the connection address — the proxy's — and *every*
///   client behind that proxy then shares one bucket. That is the damaging
///   direction: not a caller with too much allowance, but a whole deployment
///   with one.
/// - **The v4-mapped form.** `::ffff:203.0.113.7` and `203.0.113.7` are one
///   host. `IpAddr::to_string` keeps them apart; `IpAddr::to_canonical` does
///   not.
/// - **Both at once**, which is what a proxy that writes mapped addresses in
///   brackets sends.
///
/// Twenty requests spend one bucket and the twenty-first names the same host
/// another way, so a refusal is the assertion that the two are one key.
#[tokio::test]
async fn one_host_is_one_bucket_however_the_proxy_spells_it() {
    let flow = Flow::fresh().await;
    for (spent, same_host) in [
        ("[2001:db8::1]", "2001:db8::1"),
        ("::ffff:203.0.113.7", "203.0.113.7"),
        ("[::ffff:203.0.113.8]", "203.0.113.8"),
    ] {
        for i in 0..20 {
            let res = flow.register_from(spent).await;
            assert_eq!(
                res.status,
                StatusCode::CREATED,
                "request {i} from {spent} must pass"
            );
        }
        let blocked = flow.register_from(same_host).await;
        assert_eq!(
            blocked.status,
            StatusCode::TOO_MANY_REQUESTS,
            "{spent} and {same_host} are one host and must be one bucket"
        );
    }
}

/// A clock that steps backwards must not leave every bucket full until it
/// catches up.
///
/// The window is fixed, so the roll is decided by how far `now` is from the
/// window's start. A wall clock that an operator or an NTP step moves back an
/// hour makes that distance negative, and a limiter that only rolls forward
/// then refuses every request for an hour — a self-inflicted outage on the
/// endpoints a connector needs to recover.
#[tokio::test]
async fn a_backward_clock_step_does_not_freeze_the_window() {
    let flow = Flow::fresh().await;
    for i in 0..20 {
        let res = flow.register_from("203.0.113.7").await;
        assert_eq!(res.status, StatusCode::CREATED, "request {i} must pass");
    }
    assert_eq!(
        flow.register_from("203.0.113.7").await.status,
        StatusCode::TOO_MANY_REQUESTS
    );

    flow.server().clock().advance_seconds(-61);
    assert_eq!(
        flow.register_from("203.0.113.7").await.status,
        StatusCode::CREATED,
        "a clock that stepped backwards must open a fresh window, not freeze one"
    );
}

/// Presenting a client at `GET /oauth/authorize` records the use the eviction
/// measures, so a client in daily service is not evicted for being thirty days
/// old.
///
/// Two clients registered together and one of them used, because the eviction
/// has to keep one and take the other: a test that only kept would pass
/// against an eviction that never runs, and one that only evicted would pass
/// against a `last_used_us` nothing writes.
///
/// Neither client ever reaches a token, which is deliberate — the token
/// subquery would otherwise decide the outcome and the last-use half would go
/// untested.
#[tokio::test]
async fn authorizing_records_the_use_the_eviction_measures() {
    const DAY: i64 = 24 * 60 * 60;
    let flow = Flow::fresh().await;
    let used = flow.register().await;
    let forgotten = flow.register().await;
    assert_eq!(flow::count_rows(flow.server(), "oauth_client"), 2);

    // Twenty-nine days later, one of them comes back.
    flow.server().clock().advance_seconds(29 * DAY);
    assert_eq!(flow.authorize(&used).await.status(), StatusCode::FOUND);

    // Two days after that: thirty-one days since both registered, two days
    // since the one was used.
    flow.server().clock().advance_seconds(2 * DAY);
    assert_eq!(flow.server().oauth().maintain().evicted, 1);
    assert_eq!(flow::count_rows(flow.server(), "oauth_client"), 1);
    assert_eq!(
        flow.authorize(&used).await.status(),
        StatusCode::FOUND,
        "the client that was used must still be registered"
    );
    assert_eq!(
        flow.authorize(&forgotten).await.status(),
        StatusCode::BAD_REQUEST,
        "the client that was never used must be gone"
    );
}

/// A granted token exchange records the client's use.
///
/// Asserted on the column rather than through the eviction, and that is not a
/// shortcut: a client that has just been granted a token **holds** one, so the
/// eviction's token subquery decides its fate for as long as that token lives
/// and no end-to-end outcome can tell the two implementations apart. The
/// column has a second consumer — an operator asking which registrations are
/// in use, which the runbook shows — and that one is real.
#[tokio::test]
async fn a_granted_token_exchange_records_the_use() {
    let (authorized, code) = to_code(&["graph-read"]).await;
    let authorized_at = authorized.now();

    authorized.clock().advance_seconds(30);
    let granted = authorized.exchange(&code).await;
    assert!(!granted.access_token.is_empty());
    assert_eq!(
        last_used(&authorized),
        authorized_at + 30_000_000,
        "a granted exchange must record the moment it was granted"
    );
}

/// A **refused** token request does not.
///
/// This is the half that makes the touch safe rather than merely present: a
/// refused request names a `client_id` too, and honouring that would let
/// anybody keep any registration alive for ever with one well-formed refusal a
/// month — the exact opposite of what the eviction exists for.
#[tokio::test]
async fn a_refused_token_request_does_not_record_a_use() {
    let (authorized, code) = to_code(&["graph-read"]).await;
    let authorized_at = authorized.now();

    authorized.clock().advance_seconds(30);
    let refused = authorized
        .token(&[
            ("code", code.as_str()),
            ("code_verifier", "the-wrong-verifier"),
        ])
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        last_used(&authorized),
        authorized_at,
        "a refused exchange must not refresh the registration"
    );
}

/// The `last_used_us` of this flow's client, which is what
/// `Store::evict_clients` measures and what an operator reads.
fn last_used(authorized: &Authorized) -> i64 {
    let client_id = authorized.client_id.clone();
    flow::with_store(authorized.server(), |store| {
        store
            .get_client(&client_id)
            .expect("the store reads its own client")
            .expect("this flow registered a client")
            .last_used_us
    })
}

/// Without `--oauth-trust-forwarded-proto`, `X-Forwarded-For` buys a caller
/// nothing.
///
/// This is the whole difference between a limit and a decoration: the header
/// is client-chosen, so a server that reads it from an untrusted hop lets one
/// caller mint a fresh bucket per request and send as many registrations as it
/// likes. Twenty-one requests, each naming a different address, and the last
/// one must still be refused.
///
/// **What they share here is the unknown bucket, not a connection.** A
/// `oneshot` request carries no `ConnectInfo`, so with the header ignored
/// there is no address left and `oauth_routes::UNKNOWN_PEER` is what counts
/// them — and that is exactly as much as this test can prove. That the real
/// transport supplies a connection address instead is `crate::http::run`'s
/// job, and it is verified by hand against the running binary rather than
/// here; see the task report.
///
/// It builds its own server rather than using `Flow`, because
/// `support::oauth_config` trusts the proxy as every real deployment behind
/// one does, and this is the other deployment.
#[tokio::test]
async fn a_forwarded_for_header_from_an_untrusted_hop_does_not_change_the_bucket() {
    let mut config = support::oauth_config("https://idp.invalid");
    config.trust_forwarded_proto = false;
    let server = support::server(
        Some(config),
        support::Scopes::all(),
        Some(support::Clock::at(flow::NOW)),
    )
    .await;

    let register = |peer: &str| {
        server.request(
            Request::post("/oauth/register")
                .header("content-type", "application/json")
                .header("x-forwarded-for", peer)
                .body(Body::from(
                    r#"{"client_name":"Claude","redirect_uris":["https://claude.ai/api/mcp/auth_callback"]}"#,
                ))
                .unwrap(),
        )
    };
    for i in 0..20 {
        let res = register(&format!("203.0.113.{i}")).await;
        assert_eq!(res.status(), StatusCode::CREATED, "request {i} must pass");
    }
    let blocked = register("203.0.113.99").await;
    assert_eq!(
        blocked.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a self-chosen address must not open a second allowance"
    );
}

/// The periodic maintenance removes what has expired and nothing else.
///
/// It calls `OauthState::maintain`, which is the whole of what the five-minute
/// tick in `src/server.rs` runs — so this covers the scheduled work rather
/// than the store method under it, and a tick that read the wrong clock would
/// fail here.
///
/// It is asserted through the transport as well as through the row count,
/// because the two answer different questions: the count says the row went,
/// and `tools/list` says the credential a client is holding still works. A
/// sweep that took the live pair would pass a count assertion written the
/// obvious way.
#[tokio::test]
async fn the_maintenance_removes_expired_rows_and_keeps_live_ones() {
    let (authorized, tokens) = to_tokens(&["graph-read"]).await;
    let live = tokens.access_token.clone();

    // A second, already-expired family.
    authorized.insert_expired_family("dead");
    assert_eq!(
        authorized.count("oauth_token"),
        3,
        "one live pair plus one dead access"
    );

    let removed = authorized.server().oauth().maintain().swept;
    assert_eq!(removed, 1);
    assert_eq!(
        authorized.mcp_tools_list(&live).await.status,
        StatusCode::OK,
        "the live pair must survive the sweep"
    );

    // After an hour every access token is gone, and the refresh token remains.
    authorized.clock().advance_seconds(3601);
    let removed = authorized.server().oauth().maintain().swept;
    assert!(removed >= 1, "the expired access token must go");
    assert_eq!(
        authorized.mcp_tools_list(&live).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        authorized.count("oauth_token"),
        1,
        "the refresh token outlives the access token"
    );
}

// ── The paths OAuth must not have broken ────────────────────────────────────

/// With neither OAuth nor a static token the server stays open, which is the
/// behaviour every deployment that predates this work has. The static bearer
/// half runs against the real binary, in `tests/ui_http.rs`.
///
/// It asserts the scopes rather than the status, because the status cannot
/// see the thing that changed. A principal holding nothing at all also gets
/// 200 here: `tools/list` filters by scope and answers an empty array. What
/// deviation 2 decided is *which* scopes the open caller holds — the
/// configured `bearer_scopes`, not every category — and only the list shows
/// it.
#[tokio::test]
async fn an_open_server_grants_the_configured_scopes_with_no_credential() {
    let server = support::open_server().await;
    let res = server
        .request(
            Request::post("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(TOOLS_LIST))
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);

    let body = json_body(res).await;
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .expect("tools/list answers an array of tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    // `support::open_server` configures every category on the bearer list, so
    // the open caller holds both halves of the graph.
    assert!(names.contains(&"read_graph"), "{names:?}");
    assert!(names.contains(&"delete_entities"), "{names:?}");
}

/// The static bearer token this server configures beside OAuth.
const STATIC_TOKEN: &str = "a-static-bearer-token";

/// R15's other half: a server running OAuth **and** `--auth-token-file`.
///
/// The runbook and the README both sell this deployment, and `principal_of` is
/// a two-arm fallthrough — the OAuth arm first, the constant-time static
/// compare second. Only a fixture carrying both credentials can reach the
/// second arm with OAuth on, and the ordering matters in both directions: an
/// issued token must not be compared against the static one, and a static
/// token must not be refused because the OAuth lookup came first and failed.
///
/// One test rather than three, because all three requests need the one server
/// that holds both credentials, and two `Server` values on one thread
/// deadlock.
#[tokio::test]
async fn a_static_bearer_works_beside_oauth_and_neither_credential_shadows_the_other() {
    let authorized = Flow::new("graph-read")
        .static_token(STATIC_TOKEN)
        .into_consent()
        .await;
    let code = authorized.approve(&["graph-read"]).await;
    let tokens = authorized.exchange(&code).await;

    assert_eq!(
        authorized.mcp_tools_list(&tokens.access_token).await.status,
        StatusCode::OK,
        "an issued token must still be resolved as one with a static token configured"
    );
    assert_eq!(
        authorized.mcp_tools_list(STATIC_TOKEN).await.status,
        StatusCode::OK,
        "the static bearer must still be honoured with OAuth on"
    );
    assert_eq!(
        authorized.mcp_tools_list("neither-credential").await.status,
        StatusCode::UNAUTHORIZED,
        "a token that is neither must fall through both arms"
    );
}

/// The static bearer's scopes are the operator's `--static-bearer-scopes`, and
/// they are not the scopes of the human's grant. A server holding both
/// credentials must keep the two principals apart.
#[tokio::test]
async fn the_static_bearer_holds_its_own_scopes_beside_an_oauth_grant() {
    let authorized = Flow::new("graph-read")
        .static_token(STATIC_TOKEN)
        .into_consent()
        .await;
    let code = authorized.approve(&["graph-read"]).await;
    let tokens = authorized.exchange(&code).await;

    let granted = authorized.tool_names(&tokens.access_token).await;
    assert!(granted.contains(&"read_graph".to_owned()), "{granted:?}");
    assert!(
        !granted.contains(&"delete_entities".to_owned()),
        "the human granted graph-read alone: {granted:?}"
    );

    // `support::Scopes::all` puts every category on the static list, so the
    // static caller reaches the write tools the human's grant does not.
    let static_names = authorized.tool_names(STATIC_TOKEN).await;
    assert!(
        static_names.contains(&"delete_entities".to_owned()),
        "the static bearer keeps its own scopes: {static_names:?}"
    );
}
