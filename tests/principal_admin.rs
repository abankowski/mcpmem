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
    assert!(
        text.contains("Sign-in pending"),
        "pending page names the state"
    );
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
    config.principals[0]
        .scopes
        .push(mcpmem::principals::ADMIN_SCOPE.into());
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

async fn assert_graph_tools_available(server: &support::Server, token: &str) {
    let response = server
        .request(
            Request::post("/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, bearer(token))
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = support::json(response).await;
    assert!(
        body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "read_graph"),
        "the credential must hold the graph-read scope: {body}"
    );
}

#[tokio::test]
async fn an_anonymous_caller_is_challenged_not_refused() {
    let (server, _token) = admin_server().await;
    let res = server
        .request(
            Request::get("/ui/api/principals")
                .body(Body::empty())
                .unwrap(),
        )
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
    let res = server
        .request(bearer_get(&token, "/ui/api/principals"))
        .await;
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

    // The provider cannot authenticate u-9 in this fixture. Plant a token
    // for its stable ID so the HTTP delete must revoke the correct family.
    let family = mcpmem_oauth::new_token();
    let principal_id = mcpmem::principals::human_id("https://idp.example", "u-9");
    let now = (server.oauth().now_us)();
    support::flow::with_store(&server, |store| {
        store
            .put_token(
                &family,
                mcpmem_oauth::store::TokenKind::Refresh,
                &mcpmem_oauth::store::Grant {
                    client_id: "c-ada".into(),
                    principal: principal_id.clone(),
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
    let listed = server
        .request(bearer_get(&token, "/ui/api/principals"))
        .await;
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
                .body(Body::from(
                    r#"{"scopes":["graph-read","graph-write"]}"#.to_owned(),
                ))
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
        store.take_refresh(&family, now + 1_000_000)
    })
    .expect("the store answers");
    assert!(
        matches!(outcome, mcpmem_oauth::store::RefreshOutcome::Unknown),
        "deleting the principal revoked its token family"
    );

    // A code exchange racing deletion can write a token after family revocation.
    // Even a stored, unexpired token must not authenticate an absent human.
    let late_token = mcpmem_oauth::new_token();
    support::flow::with_store(&server, |store| {
        store
            .put_token(
                &late_token,
                mcpmem_oauth::store::TokenKind::Access,
                &mcpmem_oauth::store::Grant {
                    client_id: "raced-client".into(),
                    principal: principal_id.clone(),
                    scopes: vec!["graph-read".into()],
                    resource: format!("{}/mcp", support::PUBLIC_URL),
                    family: mcpmem_oauth::new_token(),
                },
                now,
                now + 60 * 60 * 1_000_000,
            )
            .unwrap();
    });
    assert!(server.oauth().validate(&late_token).is_some());
    let refused = server
        .request(
            Request::post("/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, bearer(&late_token))
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn runtime_human_owner_created_first_refuses_admin_deletion() {
    let (server, admin_token) = admin_server().await;
    let created = server
        .request(
            Request::post("/ui/api/principals")
                .header(header::AUTHORIZATION, bearer(&admin_token))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"owner","iss":"https://idp.example","sub":"owner-1","scopes":["graph-read"]}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let deletion_id = support::json(created).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let owner_id = mcpmem::principals::human_id("https://idp.example", "owner-1");
    let registry =
        mcpmem::workspace::WorkspaceRegistry::open(&server.memory_db_path(), None).unwrap();
    let entered_init = std::sync::Barrier::new(2);
    let resume_init = std::sync::Barrier::new(2);
    let workspace = std::thread::scope(|scope| {
        let creation = scope.spawn(|| {
            registry.create(
                &owner_id,
                "owned graph",
                mcpmem::workspace::Visibility::Private,
                |_| {
                    entered_init.wait();
                    resume_init.wait();
                    Ok(())
                },
            )
        });
        entered_init.wait();
        resume_init.wait();
        creation.join().unwrap().unwrap()
    });
    assert!(
        registry
            .resolve(
                &owner_id,
                Some(&workspace.workspace_id),
                mcpmem::workspace::WorkspaceAccess::Owner
            )
            .is_ok(),
        "the principal owns the graph before deletion"
    );
    let owner_token = mcpmem_oauth::new_token();
    let now = (server.oauth().now_us)();
    support::flow::with_store(&server, |store| {
        store
            .put_token(
                &owner_token,
                mcpmem_oauth::store::TokenKind::Access,
                &mcpmem_oauth::store::Grant {
                    client_id: "owner-client".into(),
                    principal: owner_id.clone(),
                    scopes: vec!["graph-read".into()],
                    resource: format!("{}/mcp", support::PUBLIC_URL),
                    family: "owner-family".into(),
                },
                now,
                now + 60 * 60 * 1_000_000,
            )
            .unwrap();
    });
    assert_graph_tools_available(&server, &owner_token).await;
    let deleted = server
        .request(
            Request::delete(format!("/ui/api/principals/{deletion_id}"))
                .header(header::AUTHORIZATION, bearer(&admin_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(deleted.status(), StatusCode::CONFLICT);
    assert_graph_tools_available(&server, &owner_token).await;
    assert!(
        registry.registered_human(&owner_id).unwrap(),
        "the refused deletion must leave the human registered"
    );
    assert!(
        registry
            .resolve(
                &owner_id,
                Some(&workspace.workspace_id),
                mcpmem::workspace::WorkspaceAccess::Owner
            )
            .is_ok(),
        "a refused deletion must preserve the workspace owner"
    );
    let reopened =
        mcpmem::workspace::WorkspaceRegistry::open(&server.memory_db_path(), None).unwrap();
    assert!(
        reopened
            .resolve(
                &owner_id,
                Some(&workspace.workspace_id),
                mcpmem::workspace::WorkspaceAccess::Owner
            )
            .is_ok(),
        "the owner must remain valid on startup"
    );
}

#[tokio::test]
async fn runtime_human_owner_deleted_during_init_cannot_be_committed() {
    use std::sync::{Arc, Barrier};

    use mcpmem::workspace::{Visibility, WorkspaceAccess, WorkspaceError, WorkspaceRegistry};

    let (server, admin_token) = admin_server().await;
    let created = server
        .request(
            Request::post("/ui/api/principals")
                .header(header::AUTHORIZATION, bearer(&admin_token))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"raced owner","iss":"https://idp.example","sub":"owner-race","scopes":["graph-read"]}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let deletion_id = support::json(created).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let owner_id = mcpmem::principals::human_id("https://idp.example", "owner-race");
    let path = server.memory_db_path();
    let registry = Arc::new(WorkspaceRegistry::open(&path, None).unwrap());
    let entered_init = Arc::new(Barrier::new(2));
    let resume_init = Arc::new(Barrier::new(2));
    let create_registry = Arc::clone(&registry);
    let create_owner_id = owner_id.clone();
    let create_entered = Arc::clone(&entered_init);
    let create_resume = Arc::clone(&resume_init);
    let creation = tokio::task::spawn_blocking(move || {
        create_registry.create(
            &create_owner_id,
            "must not exist",
            Visibility::Private,
            |_| {
                create_entered.wait();
                create_resume.wait();
                Ok(())
            },
        )
    });
    entered_init.wait();
    let deleted = server
        .request(
            Request::delete(format!("/ui/api/principals/{deletion_id}"))
                .header(header::AUTHORIZATION, bearer(&admin_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    resume_init.wait();
    let created = creation.await.unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(
        matches!(&created, Err(WorkspaceError::InvalidInput(_))),
        "a deleted human must not become a workspace owner: {created:?}"
    );
    assert!(!registry.registered_human(&owner_id).unwrap());

    let sidecar = format!("{}.workspaces.sqlite", path.display());
    let (owners, defaults): (i64, i64) = rusqlite::Connection::open(sidecar)
        .unwrap()
        .query_row(
            "SELECT (SELECT count(*) FROM workspace WHERE owner_id=?1),
                    (SELECT count(*) FROM workspace_default WHERE principal_id=?1)",
            [&owner_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((owners, defaults), (0, 0));
    let reopened = WorkspaceRegistry::open(&path, None).expect("the registry must reopen");
    assert!(matches!(
        reopened.resolve(&owner_id, None, WorkspaceAccess::Owner),
        Err(WorkspaceError::NotFound)
    ));
}

#[tokio::test]
async fn deleting_a_non_owner_human_removes_private_grants_and_default_before_recreation() {
    use mcpmem::workspace::{Visibility, WorkspaceAccess, WorkspaceRegistry};

    let (server, admin_token) = admin_server().await;
    let create = || {
        Request::post("/ui/api/principals")
            .header(header::AUTHORIZATION, bearer(&admin_token))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"name":"guest","iss":"https://idp.example","sub":"guest-1","scopes":["graph-read"]}"#,
            ))
            .unwrap()
    };
    let first = server.request(create()).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let deletion_id = support::json(first).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let stable_id = mcpmem::principals::human_id("https://idp.example", "guest-1");
    let registry = WorkspaceRegistry::open(&server.memory_db_path(), None).unwrap();
    let graph = registry
        .create(
            "machine:local",
            "private grant",
            Visibility::Private,
            |_| Ok(()),
        )
        .unwrap();
    registry
        .grant("machine:local", &graph.workspace_id, &stable_id, "reader")
        .unwrap();
    registry
        .set_default(&stable_id, &graph.workspace_id)
        .unwrap();
    assert_eq!(
        registry
            .resolve(&stable_id, None, WorkspaceAccess::Read)
            .unwrap()
            .workspace_id,
        graph.workspace_id
    );

    let deleted = server
        .request(
            Request::delete(format!("/ui/api/principals/{deletion_id}"))
                .header(header::AUTHORIZATION, bearer(&admin_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let sidecar = format!("{}.workspaces.sqlite", server.memory_db_path().display());
    let counts: (i64, i64) = rusqlite::Connection::open(sidecar)
        .unwrap()
        .query_row(
            "SELECT (SELECT count(*) FROM workspace_grant WHERE principal_id=?1),
                    (SELECT count(*) FROM workspace_default WHERE principal_id=?1)",
            [&stable_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        counts,
        (0, 0),
        "deletion must remove the private grant and default"
    );

    let second = server.request(create()).await;
    assert_eq!(second.status(), StatusCode::CREATED);
    assert_eq!(support::json(second).await["id"], deletion_id);
    let listed = registry.list(&stable_id, None, 100).unwrap().workspaces;
    assert!(
        listed
            .iter()
            .all(|workspace| workspace.workspace_id != graph.workspace_id),
        "recreation must not restore private grants: {listed:?}"
    );
    assert!(
        registry
            .resolve(&stable_id, None, WorkspaceAccess::Read)
            .is_err()
    );
    assert!(
        registry
            .resolve(&stable_id, Some(&graph.workspace_id), WorkspaceAccess::Read)
            .is_err()
    );
}

#[tokio::test]
async fn deleting_a_human_invalidates_issued_codes_and_pending_consent() {
    let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default()).await;
    let mut config = support::oauth_config(&idp.issuer);
    config.principals[0].sub = "admin-subject".into();
    config.principals[0]
        .scopes
        .push(mcpmem::principals::ADMIN_SCOPE.into());
    let server = support::server(Some(config), support::Scopes::all(), None).await;
    let admin_token = support::flow::plant_admin_token(&server);
    let created = server
        .request(
            Request::post("/ui/api/principals")
                .header(header::AUTHORIZATION, bearer(&admin_token))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "name": "code holder",
                        "iss": &idp.issuer,
                        "sub": "sub-1",
                        "scopes": ["graph-read"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let deletion_id = support::json(created).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let stable_id = mcpmem::principals::human_id(&idp.issuer, "sub-1");
    let redirect = format!("{}/cb", support::PUBLIC_URL);
    let client_id = support::flow::register(&server, "pending-code-client", &redirect).await;
    let mut logins = Vec::new();
    for _ in 0..2 {
        let params = authorize_params(&client_id);
        let started = server
            .request(
                Request::get(format!(
                    "/oauth/authorize?{}",
                    support::flow::query_string(&as_pairs(&params))
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await;
        assert_eq!(started.status(), StatusCode::FOUND);
        let back = idp.login(&support::header(&started, "location")).await;
        let callback = server
            .request(
                Request::get(format!(
                    "/oauth/callback?code={}&state={}",
                    back.code, back.state
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await;
        assert_eq!(callback.status(), StatusCode::OK);
        let page = support::flow::body_text(callback).await;
        let csrf = page
            .split_once("name=\"csrf\" value=\"")
            .expect("the consent page provides a CSRF value")
            .1
            .split('"')
            .next()
            .unwrap()
            .to_owned();
        logins.push((back.state, csrf));
    }
    let (first_state, first_csrf) = logins.remove(0);
    let (pending_state, pending_csrf) = logins.remove(0);
    let approved = server
        .request(
            Request::post("/oauth/consent")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(support::flow::query_string(&[
                    ("csrf", &first_csrf),
                    ("state", &first_state),
                    ("scope", "graph-read"),
                ])))
                .unwrap(),
        )
        .await;
    assert_eq!(approved.status(), StatusCode::FOUND);
    let code = support::flow::code_from(&support::header(&approved, "location"));
    let (code_owner, code_expires, pending_count): (String, i64, i64) =
        support::flow::with_store(&server, |store| {
            let code_row = store
                .connection()
                .query_row(
                    "SELECT principal,expires_us FROM oauth_code WHERE code_digest=?1",
                    [mcpmem_oauth::digest(&code)],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            let pending = store
                .connection()
                .query_row(
                    "SELECT count(*) FROM oauth_login WHERE principal=?1",
                    [&stable_id],
                    |row| row.get(0),
                )
                .unwrap();
            (code_row.0, code_row.1, pending)
        });
    assert_eq!(code_owner, stable_id);
    assert!(code_expires > (server.oauth().now_us)(), "the code is live");
    assert_eq!(pending_count, 1, "another consent form is still open");

    let deleted = server
        .request(
            Request::delete(format!("/ui/api/principals/{deletion_id}"))
                .header(header::AUTHORIZATION, bearer(&admin_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let exchange = server
        .request(
            Request::post("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(support::flow::query_string(&[
                    ("grant_type", "authorization_code"),
                    ("code", &code),
                    ("redirect_uri", &redirect),
                    ("client_id", &client_id),
                    ("code_verifier", support::flow::CODE_VERIFIER),
                ])))
                .unwrap(),
        )
        .await;
    let exchange_status = exchange.status();
    let exchange_body = support::json(exchange).await;
    if exchange_status == StatusCode::OK {
        let access = exchange_body["access_token"].as_str().unwrap();
        let mcp = server
            .request(
                Request::post("/mcp")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, bearer(access))
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(mcp.status(), StatusCode::UNAUTHORIZED);
    }
    assert_eq!(
        exchange_status,
        StatusCode::BAD_REQUEST,
        "the deleted human's code must not be redeemable: {exchange_body}"
    );
    let pending = server
        .request(
            Request::post("/oauth/consent")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(support::flow::query_string(&[
                    ("csrf", &pending_csrf),
                    ("state", &pending_state),
                    ("scope", "graph-read"),
                ])))
                .unwrap(),
        )
        .await;
    assert_eq!(pending.status(), StatusCode::BAD_REQUEST);
    let (codes, logins): (i64, i64) = support::flow::with_store(&server, |store| {
        let count = |table: &str| {
            store
                .connection()
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE principal=?1"),
                    [&stable_id],
                    |row| row.get(0),
                )
                .unwrap()
        };
        (count("oauth_code"), count("oauth_login"))
    });
    assert_eq!((codes, logins), (0, 0));
}

/// The create endpoint trims like the file loader does: an untrimmed `iss`
/// or `sub` can never equal a provider claim, so a stray space would create
/// a principal that can never authenticate. The response must carry the
/// trimmed values, and a later duplicate submission with the trimmed key
/// must be the same principal — a 409, not a second row.
#[tokio::test]
async fn create_trims_name_iss_and_sub_like_the_file_loader() {
    let (server, token) = admin_server().await;

    let create = server
        .request(
            Request::post("/ui/api/principals")
                .header(header::AUTHORIZATION, bearer(&token))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"  ada  ","iss":" https://idp.example ","sub":"  u-9","scopes":["graph-read"]}"#
                        .to_owned(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(create.status(), 201);
    let view = support::json(create).await;
    assert_eq!(view["name"], "ada", "name is trimmed");
    assert_eq!(view["iss"], "https://idp.example", "iss is trimmed");
    assert_eq!(view["sub"], "u-9", "sub is trimmed");

    // The same identity, spelled with the trimmed key, is a duplicate.
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
    assert_eq!(dup.status(), 409, "the trimmed key is the same principal");
}

#[tokio::test]
async fn a_non_admin_grant_is_refused() {
    let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default()).await;
    let config = support::oauth_config(&idp.issuer);
    // principals[0] holds only graph-read/graph-write — no admin.
    let server = support::server(Some(config), support::Scopes::all(), None).await;
    let token = support::flow::admin_access_token(&idp, &server).await;
    let res = server
        .request(bearer_get(&token, "/ui/api/principals"))
        .await;
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

/// `GET /ui/admin` and the callback are static, like the viewer: the React
/// shell holds no data, so it is served without auth, and its assets come
/// from the checked manifest. The JSON endpoints are the gate.
#[tokio::test]
async fn the_admin_page_and_assets_are_served() {
    let (server, _token) = admin_server().await;
    for path in ["/ui/admin", "/ui/admin/callback"] {
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
        assert!(ct.starts_with("text/html"), "{path} content type is {ct}");
    }
    let manifest = std::fs::read_to_string(format!(
        "{}/ui/dist/ui-manifest.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("the built manifest ships with the repo");
    let manifest_value: serde_json::Value =
        serde_json::from_str::<serde_json::Value>(&manifest).expect("the manifest is JSON");
    let files: serde_json::Value = manifest_value["files"].clone();
    for (path, meta) in files.as_object().unwrap() {
        let expected = meta["contentType"]
            .as_str()
            .expect("a manifest content type")
            .to_owned();
        let res = server
            .request(Request::get(path.as_str()).body(Body::empty()).unwrap())
            .await;
        assert_eq!(res.status(), 200, "{path} serves the bundled asset");
        let ct = res
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, expected, "{path} content type is {ct}");
    }
}

/// The admin SPA derives its redirect from the page's own path, and the
/// seeded `mcpmem-admin-ui` client registers `{public_url}/ui/admin/callback`.
/// `/oauth/authorize` compares the two byte-for-byte, so a `--public-url`
/// with a path prefix keeps working only when the SPA's derivation carries
/// the prefix. The origin-only value is what the old SPA sent, and it must
/// be refused — that is the regression this test guards.
#[tokio::test]
async fn the_admin_redirect_keeps_a_public_url_path_prefix() {
    let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default()).await;
    let config = support::oauth_config_at(&format!("{}/mem", support::PUBLIC_URL), &idp.issuer);
    let server = support::server(Some(config), support::Scopes::all(), None).await;

    // What the fixed SPA sends on the page {prefix}/ui/admin:
    // location.origin + page path, a trailing "/callback" dropped if
    // present, then "/callback" appended.
    let prefixed = format!("{}/mem/ui/admin/callback", support::PUBLIC_URL);
    let origin_only = format!("{}/ui/admin/callback", support::PUBLIC_URL);
    let params = |redirect: String| {
        vec![
            ("response_type", "code".into()),
            ("client_id", "mcpmem-admin-ui".into()),
            ("redirect_uri", redirect),
            (
                "scope",
                format!("graph-read {}", mcpmem::principals::ADMIN_SCOPE),
            ),
            ("state", "st".into()),
            ("code_challenge", support::flow::code_challenge()),
            ("code_challenge_method", "S256".into()),
        ]
    };
    let started = server
        .request(
            Request::get(format!(
                "/oauth/authorize?{}",
                support::flow::query_string(&as_pairs(&params(prefixed)))
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(
        started.status(),
        StatusCode::FOUND,
        "the page-derived redirect matches the seeded client, so authorize accepts it"
    );
    let refused = server
        .request(
            Request::get(format!(
                "/oauth/authorize?{}",
                support::flow::query_string(&as_pairs(&params(origin_only)))
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(
        refused.status(),
        StatusCode::BAD_REQUEST,
        "the origin-only redirect drops the prefix and is refused byte-for-byte"
    );
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
    assert!(
        text.contains("Sign-in pending"),
        "pending page names the state"
    );
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
    config.principals[0]
        .scopes
        .push(mcpmem::principals::ADMIN_SCOPE.into());
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
    assert_eq!(
        hop.status(),
        StatusCode::OK,
        "the promoted human is not pending"
    );
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
    config.principals[0]
        .scopes
        .push(mcpmem::principals::ADMIN_SCOPE.into());
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
