#![cfg(all(feature = "oauth", feature = "ui"))]
//! The error-envelope contract for `/ui/api/*`.
//!
//! The approved design pins every non-success UI API body as `{code,message}`
//! with a stable snake-case code and the HTTP status. This suite checks
//! authentication, scope, input, hidden-workspace, conflict, MIME, byte-cap,
//! and unavailable-service responses. It also pins the exact
//! `WWW-Authenticate` challenge where one applies, and keeps a positive
//! request beside the hidden-workspace case.
//!
//! The suite is red before the implementation stage: the shared `json_error`
//! helper emits `{error}` and several adapters answer plain text. A missing
//! or changed envelope must fail these tests.

use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use http_body_util::BodyExt;
use mcpmem::principals::{PrincipalEntry, human_id};
use mcpmem::tools::ToolCategory;
use serde_json::{Value, json};

mod support;

/// A private workspace owned by one token, plus an outsider with `graph-read`
/// and no grant, and a token that holds `graph-write` only.
struct ContractFixture {
    server: support::Server,
    owner: String,
    /// Holds `graph-write` only: the shared data gate must refuse it.
    unscoped: String,
    /// Holds `graph-read` and `attachments`, with no access to the workspace.
    outsider: String,
    workspace: String,
}

async fn contract_fixture() -> ContractFixture {
    let mut config = support::oauth_config("https://idp.invalid");
    for (name, sub, scopes) in [
        ("Writer", "sub-2", vec!["graph-write".into()]),
        (
            "Outsider",
            "sub-3",
            vec!["graph-read".into(), "attachments".into()],
        ),
    ] {
        config.principals.push(PrincipalEntry {
            name: name.into(),
            iss: "https://idp.invalid".into(),
            sub: sub.into(),
            label: None,
            scopes,
        });
    }
    let server = support::server(Some(config.clone()), support::Scopes::all(), None).await;
    let (owner, unscoped, outsider) = server.oauth().with_store(|store| {
        let id =
            |index: usize| human_id(&config.principals[index].iss, &config.principals[index].sub);
        (
            support::plant(
                store,
                &id(0),
                &["graph-read", "graph-write", "attachments", "vectors"],
            ),
            support::plant(store, &id(1), &["graph-write"]),
            support::plant(store, &id(2), &["graph-read", "attachments"]),
        )
    });
    let created = support::mcp(
        &server,
        &owner,
        "create_workspace",
        json!({"name": "Private", "visibility": "private"}),
    )
    .await;
    let workspace = created["result"]["workspace"]["workspaceId"]
        .as_str()
        .expect("a workspace id")
        .to_owned();
    let seeded = support::mcp(
        &server,
        &owner,
        "create_entities",
        json!({
            "workspaceId": workspace,
            "entities": [
                { "name": "Alice", "entityType": "person", "observations": [] },
            ],
        }),
    )
    .await;
    assert!(
        seeded.get("error").is_none() && seeded["result"]["isError"].as_bool() != Some(true),
        "seeding failed: {seeded}"
    );
    ContractFixture {
        server,
        owner,
        unscoped,
        outsider,
        workspace,
    }
}

/// One `GET` with a bearer token through the fixture router.
async fn get(server: &support::Server, token: &str, path: &str) -> Response<Body> {
    support::send(server, token, "GET", path, Body::empty(), None).await
}

/// One raw-body upload through the fixture router.
async fn upload(
    server: &support::Server,
    token: &str,
    workspace: &str,
    entity: &str,
    filename: &str,
    mime: &str,
    body: Body,
) -> Response<Body> {
    support::send(
        server,
        token,
        "POST",
        &support::upload_path(workspace, entity, filename),
        body,
        Some(mime),
    )
    .await
}

/// Assert one `{code,message}` error body: application/json, exactly the two
/// keys, the stable snake-case code, and a non-empty message. A legacy
/// `{error}` body or a plain-text response fails this assertion.
async fn assert_error_envelope(
    response: Response<Body>,
    expected_status: StatusCode,
    expected_code: &str,
) -> Value {
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|value| value.to_str().unwrap_or("").to_owned())
        .unwrap_or_default();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => panic!(
            "the error body must be JSON: {error}: {}",
            String::from_utf8_lossy(&bytes)
        ),
    };
    assert_eq!(
        status, expected_status,
        "the status of {expected_code}: {body}"
    );
    assert!(
        content_type.starts_with("application/json"),
        "the envelope is JSON, not plain text ({content_type}): {body}"
    );
    let keys = body.as_object().map(|object| object.len()).unwrap_or(0);
    assert_eq!(
        keys, 2,
        "the envelope carries exactly code and message: {body}"
    );
    assert_eq!(body["code"], expected_code, "the stable code: {body}");
    let message = body["message"]
        .as_str()
        .unwrap_or_else(|| panic!("the message is a string: {body}"));
    assert!(!message.is_empty(), "the message is not empty: {body}");
    body
}

/// A missing or wrong token is 401. The body is the `unauthorized` envelope
/// and the challenge is the bare RFC 6750 `Bearer`.
#[tokio::test]
async fn missing_or_wrong_token_answers_401_with_the_envelope() {
    let server = support::static_server().await;
    for token in ["", "wrong-token"] {
        let response = get(&server, token, "/ui/api/graph").await;
        assert_eq!(
            support::header(&response, header::WWW_AUTHENTICATE.as_str()),
            "Bearer",
            "the 401 keeps the bare challenge"
        );
        assert_error_envelope(response, StatusCode::UNAUTHORIZED, "unauthorized").await;
    }
}

/// A credential without `graph-read` is 403. The body is the
/// `insufficient_scope` envelope, and the challenge names the exact scope
/// with the fixed parameter order.
#[tokio::test]
async fn missing_graph_read_answers_403_with_the_envelope_and_exact_challenge() {
    let server = support::server_with_static_token(
        None,
        support::STATIC_BEARER,
        support::Scopes::bearer_holds(vec![ToolCategory::GraphWrite]),
        None,
    )
    .await;
    let response = get(
        &server,
        support::STATIC_BEARER,
        "/ui/api/graph?workspaceId=00000000-0000-0000-0000-000000000000",
    )
    .await;
    assert_eq!(
        support::header(&response, header::WWW_AUTHENTICATE.as_str()),
        "Bearer error=\"insufficient_scope\", scope=\"graph-read\"",
        "the challenge keeps its exact parameter order and scope"
    );
    assert_error_envelope(response, StatusCode::FORBIDDEN, "insufficient_scope").await;
}

/// The scope refusal precedes any workspace lookup: an unknown workspace id
/// must not become a 404 for a caller the scope gate already refuses.
#[tokio::test]
async fn scope_refusal_precedes_any_workspace_lookup() {
    let fx = contract_fixture().await;
    let response = get(
        &fx.server,
        &fx.unscoped,
        "/ui/api/graph?workspaceId=00000000-0000-0000-0000-000000000000",
    )
    .await;
    let challenge = support::header(&response, header::WWW_AUTHENTICATE.as_str());
    assert!(
        challenge.starts_with("Bearer error=\"insufficient_scope\", scope=\"graph-read\""),
        "the challenge names graph-read with the fixed parameter order: {challenge}"
    );
    assert!(
        challenge.contains("resource_metadata="),
        "OAuth adds the discovery pointer: {challenge}"
    );
    assert_error_envelope(response, StatusCode::FORBIDDEN, "insufficient_scope").await;
}

/// A missing name and a non-positive page limit are client errors: 400 with
/// the `bad_request` envelope.
#[tokio::test]
async fn input_errors_answer_400_bad_request() {
    let fx = contract_fixture().await;
    let missing_name = get(
        &fx.server,
        &fx.owner,
        &format!("/ui/api/node?workspaceId={}", fx.workspace),
    )
    .await;
    let body = assert_error_envelope(missing_name, StatusCode::BAD_REQUEST, "bad_request").await;
    assert!(
        body["message"].as_str().unwrap().contains("name"),
        "the message names the missing input: {body}"
    );

    let bad_limit = get(
        &fx.server,
        &fx.owner,
        &format!("/ui/api/workspaces?workspaceId={}&limit=0", fx.workspace),
    )
    .await;
    let body = assert_error_envelope(bad_limit, StatusCode::BAD_REQUEST, "bad_request").await;
    assert!(
        body["message"].as_str().unwrap().contains("limit"),
        "the message names the invalid input: {body}"
    );
}

/// A denied private workspace and an unknown id are the same not-found, and
/// the owner's own workspace still reads beside them.
#[tokio::test]
async fn denied_and_unknown_workspaces_share_the_not_found_envelope() {
    let fx = contract_fixture().await;
    let positive = get(
        &fx.server,
        &fx.owner,
        &format!("/ui/api/graph?workspaceId={}", fx.workspace),
    )
    .await;
    assert_eq!(
        positive.status(),
        StatusCode::OK,
        "the owned workspace reads"
    );

    let denied = get(
        &fx.server,
        &fx.outsider,
        &format!("/ui/api/graph?workspaceId={}", fx.workspace),
    )
    .await;
    let denied_body = assert_error_envelope(denied, StatusCode::NOT_FOUND, "not_found").await;

    let missing = get(
        &fx.server,
        &fx.outsider,
        "/ui/api/graph?workspaceId=00000000-0000-0000-0000-000000000000",
    )
    .await;
    let missing_body = assert_error_envelope(missing, StatusCode::NOT_FOUND, "not_found").await;
    assert_eq!(
        denied_body, missing_body,
        "denied and unknown workspaces share one body"
    );
}

/// Re-uploading one filename to one entity is a name conflict: 409 with the
/// `conflict` envelope.
#[tokio::test]
async fn duplicate_upload_conflicts_with_409() {
    let fx = contract_fixture().await;
    let first = upload(
        &fx.server,
        &fx.owner,
        &fx.workspace,
        "Alice",
        "notes.txt",
        "text/plain",
        Body::from("first"),
    )
    .await;
    assert_eq!(
        first.status(),
        StatusCode::CREATED,
        "the first upload stores"
    );
    let duplicate = upload(
        &fx.server,
        &fx.owner,
        &fx.workspace,
        "Alice",
        "notes.txt",
        "text/plain",
        Body::from("second"),
    )
    .await;
    assert_error_envelope(duplicate, StatusCode::CONFLICT, "conflict").await;
}

/// A MIME type outside the allowlist is 415 with the
/// `unsupported_media_type` envelope.
#[tokio::test]
async fn disallowed_mime_answers_415_unsupported_media_type() {
    let fx = contract_fixture().await;
    let response = upload(
        &fx.server,
        &fx.owner,
        &fx.workspace,
        "Alice",
        "x.bin",
        "application/octet-stream",
        Body::from("bad"),
    )
    .await;
    assert_error_envelope(
        response,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
    )
    .await;
}

/// An advertised byte count above the per-file cap (52,428,800) is 413 with
/// the `payload_too_large` envelope. The cap check reads the header and
/// rejects the request before it reads the body.
#[tokio::test]
async fn oversize_upload_answers_413_payload_too_large() {
    let fx = contract_fixture().await;
    let request = Request::post(support::upload_path(
        &fx.workspace,
        "Alice",
        "too-large.txt",
    ))
    .header(header::AUTHORIZATION, format!("Bearer {}", fx.owner))
    .header(header::CONTENT_TYPE, "text/plain")
    .header(header::CONTENT_LENGTH, "52428801")
    .body(Body::from("x"))
    .unwrap();
    let response = fx.server.request(request).await;
    assert_error_envelope(response, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large").await;
}

/// A semantic search this build cannot serve is 503 with the `unavailable`
/// envelope: an optional service off is a service-level fact, not a client
/// error.
#[tokio::test]
async fn unavailable_service_answers_503_unavailable() {
    let fx = contract_fixture().await;
    let response = get(
        &fx.server,
        &fx.owner,
        &format!(
            "/ui/api/search?workspaceId={}&q=alice&mode=semantic",
            fx.workspace
        ),
    )
    .await;
    assert_error_envelope(response, StatusCode::SERVICE_UNAVAILABLE, "unavailable").await;
}
