#![cfg(feature = "oauth")]
#![cfg(feature = "webhooks")]
//! The webhook-subscription admin API: `/ui/api/webhooks` CRUD behind the
//! admin gate, mirroring the principals pages.
//!
//! The subscription store is a process-wide cell
//! (`actions::webhooks::init` records one database path for the life of the
//! process — see `webhook_tools.rs` for why a second server built later
//! would silently keep pointing at the first one's file), so every test here
//! shares one fixture and names its rows with unique endpoints. No test may
//! assert the store is globally empty, and none may depend on another test's
//! rows.

use std::sync::LazyLock;

use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use tower::ServiceExt;

use mcpmem::http::{HttpState, TestSetup, router};
use mcpmem::principals::ADMIN_SCOPE;
use mcpmem::tools::ToolCategory;

mod support;

/// One shared server plus two planted access tokens: `admin` holds the admin
/// scope, `plain` holds only `graph-read` so the gate's 403 side is testable.
struct Fixture {
    _dir: tempfile::TempDir,
    router: axum::Router,
    admin: String,
    plain: String,
}

/// Plant a live access token the way every minted token is stored, so the
/// bearer path validates it exactly like a walked one (the shape
/// `support::flow::plant_admin_token` uses). The provider is never involved.
fn plant(store: &mcpmem_oauth::store::Store, scopes: Vec<String>) -> String {
    let token = mcpmem_oauth::new_token();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_micros() as i64;
    store
        .put_token(
            &token,
            mcpmem_oauth::store::TokenKind::Access,
            &mcpmem_oauth::store::Grant {
                client_id: mcpmem_oauth::ADMIN_CLIENT_ID.to_owned(),
                principal: "adam".to_owned(),
                scopes,
                resource: format!("{}/mcp", support::PUBLIC_URL),
                family: mcpmem_oauth::new_token(),
            },
            now,
            now + 60 * 60 * 1_000_000,
        )
        .expect("the store writes the planted token");
    token
}

static FIXTURE: LazyLock<Fixture> = LazyLock::new(|| {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("t.mcpmem");
    let mut config = support::oauth_config("https://idp.invalid");
    // The principal the config allows holds the admin scope, as in
    // `principal_admin.rs::admin_server`. The issuer is never reached:
    // the tokens below are planted, not walked.
    config.principals[0].scopes.push(ADMIN_SCOPE.into());
    let state = HttpState::for_test(TestSetup {
        db_path,
        oauth: Some(config),
        auth_token: None,
        metadata_fetch: None,
        bearer_scopes: ToolCategory::ALL.to_vec(),
        enabled_categories: ToolCategory::ALL.to_vec(),
        now_us: None,
    });
    let (admin, plain) = state
        .oauth()
        .expect("the fixture server has OAuth on")
        .with_store(|store| {
            (
                plant(store, vec![ADMIN_SCOPE.to_owned()]),
                plant(store, vec!["graph-read".to_owned()]),
            )
        });
    Fixture {
        _dir: dir,
        router: router(state),
        admin,
        plain,
    }
});

/// Drive one request through a fresh clone of the shared router; `oneshot`
/// consumes the clone.
async fn drive(req: Request<Body>) -> Response<Body> {
    FIXTURE
        .router
        .clone()
        .oneshot(req)
        .await
        .expect("the router answers every request")
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

fn get(token: &str, path: impl Into<String>) -> Request<Body> {
    Request::get(path.into().as_str())
        .header(header::AUTHORIZATION, bearer(token))
        .body(Body::empty())
        .unwrap()
}

/// A JSON POST/PATCH/DELETE carrying the caller's bearer token.
fn json_call(
    method: &str,
    token: &str,
    path: impl Into<String>,
    body: Option<&str>,
) -> Request<Body> {
    let path = path.into();
    let builder = match method {
        "post" => Request::post(path.as_str()),
        "patch" => Request::patch(path.as_str()),
        "delete" => Request::delete(path.as_str()),
        other => panic!("unsupported method {other}"),
    };
    let builder = builder.header(header::AUTHORIZATION, bearer(token));
    let builder = match body {
        Some(text) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(text.to_owned())),
        None => builder.body(Body::empty()),
    };
    builder.unwrap()
}

/// The unique endpoint a test owns. The store is shared, so a test must only
/// assert about the rows it names with its own host.
fn endpoint(tag: &str) -> String {
    format!("https://{tag}.example.test/cb")
}

/// Create one fully-formed subscription and return its id.
async fn create_subscription(tag: &str) -> String {
    let body = format!(
        r#"{{"endpoint":"https://{tag}.example.test/cb","consumerOrigin":"consumer-{tag}","secretRef":"hooks-key","eventOperations":["create"],"entityTypes":["note"],"enabled":true}}"#
    );
    let res = drive(json_call(
        "post",
        &FIXTURE.admin,
        "/ui/api/webhooks",
        Some(body.as_str()),
    ))
    .await;
    assert_eq!(
        res.status(),
        StatusCode::CREATED,
        "the subscription is created"
    );
    support::json(res).await["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn anonymous_caller_is_challenged_not_refused() {
    let res = drive(
        Request::get("/ui/api/webhooks")
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
async fn a_token_without_admin_scope_is_refused() {
    let res = drive(get(&FIXTURE.plain, "/ui/api/webhooks")).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn list_reports_json_with_a_subscriptions_array() {
    let res = drive(get(&FIXTURE.admin, "/ui/api/webhooks")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = support::json(res).await;
    assert!(
        body["subscriptions"].as_array().is_some(),
        "the payload is a subscriptions array"
    );
}

#[tokio::test]
async fn create_and_list_a_subscription() {
    let tag = "create-list";
    let id = create_subscription(tag).await;
    let res = drive(get(&FIXTURE.admin, "/ui/api/webhooks")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = support::json(res).await;
    let row = body["subscriptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["subscriptionId"].as_str() == Some(id.as_str()))
        .expect("the created row is listed");
    assert_eq!(row["endpoint"].as_str().unwrap(), endpoint(tag));
    assert_eq!(
        row["consumerOrigin"].as_str().unwrap(),
        format!("consumer-{tag}")
    );
    assert_eq!(row["secretRef"].as_str().unwrap(), "hooks-key");
    assert_eq!(row["enabled"].as_bool(), Some(true));
    assert_eq!(row["eventOperations"][0].as_str().unwrap(), "create");
    assert_eq!(row["entityTypes"][0].as_str().unwrap(), "note");
}

#[tokio::test]
async fn create_refuses_a_malformed_endpoint() {
    let res = drive(
        json_call(
            "post",
            &FIXTURE.admin,
            "/ui/api/webhooks",
            Some(
                r#"{"endpoint":"http://hooks.example.test/cb","consumerOrigin":"app","secretRef":"k1"}"#,
            ),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_requires_the_origin_field() {
    let res = drive(json_call(
        "post",
        &FIXTURE.admin,
        "/ui/api/webhooks",
        Some(r#"{"endpoint":"https://hooks.example.test/cb"}"#),
    ))
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn patch_updates_fields_and_toggles_enabled() {
    let id = create_subscription("patch-toggle").await;

    let res = drive(json_call(
        "patch",
        &FIXTURE.admin,
        format!("/ui/api/webhooks/{id}"),
        Some(r#"{"enabled":false,"eventOperations":["rename"]}"#),
    ))
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let patched = support::json(res).await;
    assert_eq!(
        patched["enabled"].as_bool(),
        Some(false),
        "the toggle applies"
    );
    assert_eq!(patched["eventOperations"][0].as_str().unwrap(), "rename");

    // A refused patch leaves the row untouched: the read-back still shows
    // the toggled state, not the refused endpoint.
    let refused = drive(json_call(
        "patch",
        &FIXTURE.admin,
        format!("/ui/api/webhooks/{id}"),
        Some(r#"{"endpoint":"ftp://nope"}"#),
    ))
    .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let again = drive(get(&FIXTURE.admin, "/ui/api/webhooks")).await;
    let listed = support::json(again).await;
    let row = listed["subscriptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["subscriptionId"].as_str() == Some(id.as_str()))
        .unwrap();
    assert_eq!(
        row["enabled"].as_bool(),
        Some(false),
        "the refused patch changed nothing"
    );
}

#[tokio::test]
async fn patch_an_unknown_id_is_404() {
    let res = drive(json_call(
        "patch",
        &FIXTURE.admin,
        "/ui/api/webhooks/00000000-0000-0000-0000-000000000000",
        Some(r#"{"enabled":false}"#),
    ))
    .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_removes_and_a_second_delete_is_404() {
    let id = create_subscription("delete-removes").await;

    let first = drive(json_call(
        "delete",
        &FIXTURE.admin,
        format!("/ui/api/webhooks/{id}"),
        None,
    ))
    .await;
    assert_eq!(first.status(), StatusCode::NO_CONTENT);

    let listed = drive(get(&FIXTURE.admin, "/ui/api/webhooks")).await;
    let body = support::json(listed).await;
    assert!(
        !body["subscriptions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["subscriptionId"].as_str() == Some(id.as_str())),
        "the deleted row is gone from the list"
    );

    let second = drive(json_call(
        "delete",
        &FIXTURE.admin,
        format!("/ui/api/webhooks/{id}"),
        None,
    ))
    .await;
    assert_eq!(
        second.status(),
        StatusCode::NOT_FOUND,
        "a second delete names no row"
    );
}
