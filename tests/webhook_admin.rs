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

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use parking_lot::Mutex;
use tower::ServiceExt;

use mcpmem::http::{HttpState, TestSetup, router};
use mcpmem::principals::ADMIN_SCOPE;
use mcpmem::tools::ToolCategory;
use mcpmem_core::subscriptions::{SubscriptionRepository, WebhookSubscription};
use mcpmem_webhook::{
    DeliveryConnector, DeliveryResponse, Resolver, SignedRequest, SigningKey, ValidatedEndpoint,
    WorkerError,
};

mod support;

/// A resolver that answers every hostname with a public documentation
/// address, so the delivery-time public-address check passes.
struct StubResolver;

impl Resolver for StubResolver {
    fn resolve(&self, _hostname: &str) -> Result<Vec<IpAddr>, WorkerError> {
        Ok(vec![IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1))])
    }
}

/// A connector that records the request it saw and answers a fixed status.
/// The tests assert what the kit actually sent, not only the HTTP mapping.
struct RecordingConnector {
    last: Mutex<Option<(String, String)>>,
    attempts: AtomicU64,
}

impl DeliveryConnector for RecordingConnector {
    fn send(
        &self,
        _endpoint: &ValidatedEndpoint,
        request: SignedRequest,
    ) -> Result<DeliveryResponse, WorkerError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        *self.last.lock() = Some((request.event_id, request.signature));
        Ok(DeliveryResponse {
            status: 200,
            retry_after_us: None,
        })
    }
}

/// One shared server plus two planted access tokens: `admin` holds the admin
/// scope, `plain` holds only `graph-read` so the gate's 403 side is testable.
struct Fixture {
    _dir: tempfile::TempDir,
    router: axum::Router,
    admin: String,
    plain: String,
    /// The connector the shared test kit drives. The test route reads it to
    /// verify what a test delivery actually sent.
    stub: Arc<RecordingConnector>,
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
    // The test kit the admin Test button drives: `hooks.example.test` is the
    // only allowlisted host and `hooks-key` the only signing secret, matching
    // the endpoints and secret refs the tests plant. The kit is process-wide,
    // so these tests run serially (the pre-flight runs every suite with
    // `--test-threads=1`).
    let stub = Arc::new(RecordingConnector {
        last: Mutex::new(None),
        attempts: AtomicU64::new(0),
    });
    let kit = mcpmem::actions::webhooks::WebhookTestKit::for_test(
        ["hooks.example.test".to_owned()].into_iter().collect(),
        BTreeMap::from([(
            "hooks-key".to_owned(),
            SigningKey::new(b"test-signing-key".to_vec()).expect("the key is non-empty"),
        )]),
        Arc::new(StubResolver),
        Arc::clone(&stub) as Arc<dyn DeliveryConnector>,
        false,
    );
    mcpmem::actions::webhooks::set_test_kit(Some(Arc::new(kit)));
    Fixture {
        _dir: dir,
        router: router(state),
        admin,
        plain,
        stub,
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
    assert_eq!(
        body["deliveryRole"].as_bool(),
        Some(false),
        "the fixture runs no delivery role, and the UI must be told exactly that"
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

#[tokio::test]
async fn test_delivery_requires_admin_scope() {
    let res = drive(json_call(
        "post",
        &FIXTURE.plain,
        "/ui/api/webhooks/00000000-0000-0000-0000-000000000000/test",
        None,
    ))
    .await;
    assert_eq!(
        res.status(),
        StatusCode::FORBIDDEN,
        "the gate runs before the id is even parsed"
    );
}

#[tokio::test]
async fn test_delivery_unknown_id_is_404() {
    let res = drive(json_call(
        "post",
        &FIXTURE.admin,
        "/ui/api/webhooks/00000000-0000-0000-0000-000000000000/test",
        None,
    ))
    .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delivery_sends_a_signed_test_event_and_reports_status() {
    let created = drive(json_call(
        "post",
        &FIXTURE.admin,
        "/ui/api/webhooks",
        Some(r#"{"endpoint":"https://hooks.example.test/delivery-probe","consumerOrigin":"probe","secretRef":"hooks-key"}"#),
    ))
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let id = support::json(created).await["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();
    FIXTURE.stub.attempts.store(0, Ordering::SeqCst);
    *FIXTURE.stub.last.lock() = None;

    let res = drive(json_call(
        "post",
        &FIXTURE.admin,
        format!("/ui/api/webhooks/{id}/test"),
        None,
    ))
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = support::json(res).await;
    assert_eq!(body["status"].as_u64(), Some(200));
    assert_eq!(body["ok"].as_bool(), Some(true));
    assert!(
        body["latencyUs"].as_u64().is_some(),
        "the response carries the measured latency"
    );

    // The connector saw exactly one request, carrying a test-marked event
    // id and a real signature — the contract the receiver checks.
    assert_eq!(
        FIXTURE.stub.attempts.load(Ordering::SeqCst),
        1,
        "one delivery was attempted"
    );
    let (event_id, signature) = FIXTURE
        .stub
        .last
        .lock()
        .clone()
        .expect("the connector recorded the request");
    assert!(
        event_id.starts_with("test-"),
        "the event id marks the event as a test: {event_id}"
    );
    assert_eq!(
        signature.len(),
        64,
        "the delivery is signed with the subscription's secret"
    );
}

#[tokio::test]
async fn test_delivery_refuses_a_host_outside_the_allowlist() {
    let created = drive(json_call(
        "post",
        &FIXTURE.admin,
        "/ui/api/webhooks",
        Some(r#"{"endpoint":"https://outside.example.test/cb","consumerOrigin":"probe","secretRef":"hooks-key"}"#),
    ))
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let id = support::json(created).await["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();

    let res = drive(json_call(
        "post",
        &FIXTURE.admin,
        format!("/ui/api/webhooks/{id}/test"),
        None,
    ))
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = support::json(res).await;
    assert!(
        body["error"].as_str().unwrap().contains("not allowlisted"),
        "{body}"
    );
    assert_eq!(
        FIXTURE.stub.attempts.load(Ordering::SeqCst),
        0,
        "nothing was sent"
    );
}

/// The API now refuses to register an unknown secret ref (the guard's own
/// test covers that), so the delivery-time refusal path needs a row planted
/// behind the guard: this is the situation of a subscription created before
/// the guard, or on a host whose config lacks the name. The worker's test
/// route must still refuse it.
#[tokio::test]
async fn test_delivery_refuses_an_unknown_secret_ref() {
    let db = FIXTURE._dir.path().join("t.mcpmem");
    let conn = rusqlite::Connection::open(&db).expect("the fixture database opens");
    let subscription = WebhookSubscription {
        subscription_id: uuid::Uuid::new_v4(),
        endpoint: "https://hooks.example.test/cb".to_owned(),
        event_operations: vec![],
        entity_types: vec![],
        ignored_origins: vec![],
        consumer_origin: "probe".to_owned(),
        secret_ref: "missing-ref".to_owned(),
        enabled: true,
    };
    let id = subscription.subscription_id.to_string();
    SubscriptionRepository::new(&conn)
        .upsert(subscription)
        .expect("the planted row is valid");

    let res = drive(json_call(
        "post",
        &FIXTURE.admin,
        format!("/ui/api/webhooks/{id}/test"),
        None,
    ))
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = support::json(res).await;
    assert!(
        body["error"].as_str().unwrap().contains("not configured"),
        "{body}"
    );
}
