//! HTTP-transport tests for the browser mutation gateway
//! (`POST /ui/api/mutations`).
//!
//! The gateway is the one write entry the browser UI uses. Every test drives
//! the real router through `tests/support`, with OAuth tokens planted in the
//! store — the same fixture shape `tests/attachment_http.rs` uses. The four
//! actors are: the owner (a workspace writer), a reader (read grant only), a
//! stranger (no grant), and an unscoped writer (no `graph-write` scope).
//!
//! Error bodies follow the design contract: `{code,message}` with 401 for an
//! invalid token, 403 for a missing scope or a read-only grant, 404 for an
//! unknown or denied workspace (identical body for both), 409 for a name or
//! relation conflict, and 400 for an unknown operation or a bad payload.

#![cfg(all(feature = "oauth", feature = "ui"))]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use mcpmem::principals::PrincipalEntry;
use mcpmem_oauth::store::{Grant, Store, TokenKind};
use serde_json::{Value, json};

mod support;

/// Plant one access token for one principal in the OAuth store.
fn plant(store: &Store, principal: &str, scopes: &[&str]) -> String {
    let token = mcpmem_oauth::new_token();
    let now = mcpmem_core::events::now_us();
    store
        .put_token(
            &token,
            TokenKind::Access,
            &Grant {
                client_id: mcpmem_oauth::ADMIN_CLIENT_ID.to_owned(),
                principal: principal.to_owned(),
                scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
                resource: format!("{}/mcp", support::PUBLIC_URL),
                family: mcpmem_oauth::new_token(),
            },
            now,
            now + 3_600_000_000,
        )
        .unwrap();
    token
}

struct Fixture {
    server: support::Server,
    owner: String,
    reader: String,
    stranger: String,
    unscoped: String,
    workspace: String,
}

/// One private workspace for the owner; the reader holds a read grant; the
/// stranger holds none; the unscoped writer holds no `graph-write` scope.
async fn fixture() -> Fixture {
    let mut oauth = support::oauth_config("https://idp.invalid");
    for (name, sub) in [
        ("reader", "sub-2"),
        ("unscoped", "sub-3"),
        ("stranger", "sub-4"),
    ] {
        oauth.principals.push(PrincipalEntry {
            name: name.into(),
            iss: "https://idp.invalid".into(),
            sub: sub.into(),
            label: None,
            scopes: vec!["graph-read".into(), "graph-write".into()],
        });
    }
    oauth.principals[2].scopes = vec!["graph-read".into()];
    let server = support::server(Some(oauth.clone()), support::Scopes::all(), None).await;
    let (owner, reader, unscoped, stranger) = server.oauth().with_store(|store| {
        let id = |index: usize| {
            mcpmem::principals::human_id(&oauth.principals[index].iss, &oauth.principals[index].sub)
        };
        (
            plant(store, &id(0), &["graph-read", "graph-write"]),
            plant(store, &id(1), &["graph-read", "graph-write"]),
            plant(store, &id(2), &["graph-read"]),
            plant(store, &id(3), &["graph-read", "graph-write"]),
        )
    });
    let created = mcp(
        &server,
        &owner,
        "create_workspace",
        json!({"name": "fixture", "visibility": "private"}),
    )
    .await;
    assert!(
        created.get("error").is_none() && created["result"]["isError"].as_bool() != Some(true),
        "create_workspace failed: {created}"
    );
    let workspace = created["result"]["workspace"]["workspaceId"]
        .as_str()
        .expect("a workspace id")
        .to_owned();
    let reader_id =
        mcpmem::principals::human_id(&oauth.principals[1].iss, &oauth.principals[1].sub);
    let grant = mcp(
        &server,
        &owner,
        "grant_workspace_access",
        json!({"workspaceId": workspace, "principalId": reader_id, "role": "reader"}),
    )
    .await;
    assert_eq!(grant["result"]["grant"]["role"], "reader", "{grant}");
    Fixture {
        server,
        owner,
        reader,
        stranger,
        unscoped,
        workspace,
    }
}

/// One MCP tool call for the fixture, through the real router.
async fn mcp(server: &support::Server, token: &str, tool: &str, arguments: Value) -> Value {
    let response = server
        .request(
            Request::post("/mcp")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"jsonrpc":"2.0","method":"tools/call",
                           "params":{"name":tool,"arguments":arguments},"id":1})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).expect("the MCP answer is JSON")
}

/// Seed the entities and one seeded relation for the fixture.
async fn seed_graph(fx: &Fixture, relation: Value) {
    let created = mcp(
        &fx.server,
        &fx.owner,
        "create_entities",
        json!({
            "workspaceId": fx.workspace,
            "entities": [
                {"name": "Alice", "entityType": "person", "observations": []},
                {"name": "Bob", "entityType": "person", "observations": []},
            ],
        }),
    )
    .await;
    assert!(
        created.get("error").is_none() && created["result"]["isError"].as_bool() != Some(true),
        "seed create_entities failed: {created}"
    );
    let linked = mcp(
        &fx.server,
        &fx.owner,
        "create_relations",
        json!({"workspaceId": fx.workspace, "relations": [relation]}),
    )
    .await;
    assert!(
        linked.get("error").is_none() && linked["result"]["isError"].as_bool() != Some(true),
        "seed create_relations failed: {linked}"
    );
}

/// `POST /ui/api/mutations` with the given operation and payload.
async fn post_mutation(
    server: &support::Server,
    token: Option<&str>,
    workspace: &str,
    operation: &str,
    payload: Value,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut request = Request::post("/ui/api/mutations")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"workspaceId": workspace, "operation": operation, "payload": payload})
                .to_string(),
        ))
        .unwrap();
    if let Some(token) = token {
        request.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
    }
    let response = server.request(request).await;
    let headers = response.headers().clone();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&body).expect("the answer is JSON"),
    )
}

/// One authenticated GET and its body. Non-JSON bodies (the text 404s of the
/// viewer routes) come back as `Value::Null`.
async fn get_json(server: &support::Server, token: &str, path: &str) -> (StatusCode, Value) {
    let response = server
        .request(
            Request::get(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn writer_creates_entity_and_adds_observation_then_reads_back() {
    let fx = fixture().await;

    let (status, _, body) = post_mutation(
        &fx.server,
        Some(&fx.owner),
        &fx.workspace,
        "createEntity",
        json!({"name": "Carol", "entityType": "person",
               "observations": [{"body": "hello", "occurredAtUs": 11}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true, "{body}");

    let (status, _, body) = post_mutation(
        &fx.server,
        Some(&fx.owner),
        &fx.workspace,
        "addObservation",
        json!({"entityName": "Carol", "body": "second", "occurredAtUs": 22}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let path = format!("/ui/api/node?workspaceId={}&name=Carol", fx.workspace);
    let (status, body) = get_json(&fx.server, &fx.owner, &path).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "Carol");
    assert_eq!(body["entityType"], "person");
    let observations = body["observations"].as_array().expect("observations array");
    assert_eq!(observations.len(), 2, "{body}");
    assert_eq!(observations[0]["body"], "hello");
    assert_eq!(observations[0]["occurredAtUs"], 11);
    assert_eq!(observations[1]["body"], "second");
    assert_eq!(observations[1]["occurredAtUs"], 22);
    for observation in observations {
        assert!(
            observation["observationId"].as_i64().is_some(),
            "every browser observation carries its id: {observation}"
        );
        assert!(
            observation["createdAtUs"].as_i64().is_some(),
            "every browser observation carries its creation time: {observation}"
        );
    }
}

#[tokio::test]
async fn reader_posting_a_mutation_is_403() {
    let fx = fixture().await;

    let (status, headers, body) = post_mutation(
        &fx.server,
        Some(&fx.reader),
        &fx.workspace,
        "createEntity",
        json!({"name": "Sneaky", "entityType": "person", "observations": []}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "permission_denied", "{body}");
    assert!(
        headers.get(header::WWW_AUTHENTICATE).is_none(),
        "a grant refusal is not an OAuth challenge"
    );

    let path = format!("/ui/api/node?workspaceId={}&name=Sneaky", fx.workspace);
    let (status, body) = get_json(&fx.server, &fx.reader, &path).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the mutation did not run: {body}"
    );
}

#[tokio::test]
async fn writer_without_graph_write_scope_is_403_before_any_workspace_lookup() {
    let fx = fixture().await;

    // The workspace id names nothing. A 404 would prove the workspace was
    // resolved before the scope, which the contract forbids.
    let (status, headers, body) = post_mutation(
        &fx.server,
        Some(&fx.unscoped),
        "00000000-0000-0000-0000-000000000000",
        "createEntity",
        json!({"name": "X", "entityType": "person", "observations": []}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "insufficient_scope", "{body}");
    let challenge = headers
        .get(header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .expect("the 403 carries the RFC 6750 challenge");
    assert!(
        challenge.contains("insufficient_scope") && challenge.contains("graph-write"),
        "the challenge names the missing scope: {challenge}"
    );
}

#[tokio::test]
async fn delete_observation_targets_one_of_two_equal_bodies_by_id() {
    let fx = fixture().await;
    let (status, _, _) = post_mutation(
        &fx.server,
        Some(&fx.owner),
        &fx.workspace,
        "createEntity",
        json!({"name": "Carol", "entityType": "person",
               "observations": [{"body": "same body"}, {"body": "same body"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed create failed");

    let path = format!("/ui/api/node?workspaceId={}&name=Carol", fx.workspace);
    let (status, body) = get_json(&fx.server, &fx.owner, &path).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let observations = body["observations"].as_array().expect("observations array");
    assert_eq!(observations.len(), 2, "two equal bodies seeded: {body}");
    let first_id = observations[0]["observationId"].as_i64().unwrap();
    let second_id = observations[1]["observationId"].as_i64().unwrap();
    assert_ne!(first_id, second_id, "equal bodies still get distinct ids");

    let (status, _, body) = post_mutation(
        &fx.server,
        Some(&fx.owner),
        &fx.workspace,
        "deleteObservation",
        json!({"entityName": "Carol", "observationId": first_id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = get_json(&fx.server, &fx.owner, &path).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let observations = body["observations"].as_array().expect("observations array");
    assert_eq!(observations.len(), 1, "exactly one body survives: {body}");
    assert_eq!(observations[0]["body"], "same body");
    assert_eq!(
        observations[0]["observationId"].as_i64(),
        Some(second_id),
        "the survivor is the observation the request did not name"
    );
}

#[tokio::test]
async fn edit_observation_preserves_created_at_and_origin() {
    let fx = fixture().await;
    let created = mcp(
        &fx.server,
        &fx.owner,
        "create_entities",
        json!({
            "workspaceId": fx.workspace,
            "entities": [
                {"name": "src", "entityType": "person", "observations": [{"body": "carry"}]},
                {"name": "tgt", "entityType": "person", "observations": []},
            ],
        }),
    )
    .await;
    assert!(
        created.get("error").is_none() && created["result"]["isError"].as_bool() != Some(true),
        "seed failed: {created}"
    );
    let merged = mcp(
        &fx.server,
        &fx.owner,
        "merge_entities",
        json!({"workspaceId": fx.workspace, "source": "src", "target": "tgt"}),
    )
    .await;
    assert!(
        merged.get("error").is_none() && merged["result"]["isError"].as_bool() != Some(true),
        "merge failed: {merged}"
    );

    let path = format!("/ui/api/node?workspaceId={}&name=tgt", fx.workspace);
    let (status, body) = get_json(&fx.server, &fx.owner, &path).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let before = &body["observations"][0];
    let observation_id = before["observationId"].as_i64().expect("an observation id");
    let created_at = before["createdAtUs"].as_i64().expect("a creation time");
    assert_eq!(
        before["originEntityName"], "src",
        "the seed names the merge origin"
    );

    let (status, _, body) = post_mutation(
        &fx.server,
        Some(&fx.owner),
        &fx.workspace,
        "editObservation",
        json!({"entityName": "tgt", "observationId": observation_id,
               "body": "edited", "occurredAtUs": 99}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = get_json(&fx.server, &fx.owner, &path).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let after = &body["observations"][0];
    assert_eq!(after["body"], "edited");
    assert_eq!(after["occurredAtUs"], 99);
    assert_eq!(
        after["createdAtUs"].as_i64(),
        Some(created_at),
        "the edit keeps the original creation time"
    );
    assert_eq!(
        after["originEntityName"], "src",
        "the edit keeps the merge origin"
    );
}

#[tokio::test]
async fn reverse_relation_swaps_the_triple_and_keeps_children() {
    let fx = fixture().await;
    seed_graph(
        &fx,
        json!({"from": "Alice", "to": "Bob", "relationType": "works_at",
               "observations": [{"body": "joined", "occurredAtUs": 5}],
               "attributes": {"since": "2020"}}),
    )
    .await;

    let path = format!(
        "/ui/api/relation?workspaceId={}&from=Alice&to=Bob&relationType=works_at",
        fx.workspace
    );
    let (status, body) = get_json(&fx.server, &fx.owner, &path).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["observations"][0]["body"], "joined");
    assert_eq!(body["attributes"]["since"], "2020");

    let (status, _, body) = post_mutation(
        &fx.server,
        Some(&fx.owner),
        &fx.workspace,
        "reverseRelation",
        json!({"from": "Alice", "to": "Bob", "relationType": "works_at"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let path = format!(
        "/ui/api/relation?workspaceId={}&from=Bob&to=Alice&relationType=works_at",
        fx.workspace
    );
    let (status, body) = get_json(&fx.server, &fx.owner, &path).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["observations"][0]["body"], "joined",
        "the observation follows the triple"
    );
    assert_eq!(
        body["attributes"]["since"], "2020",
        "the attribute follows the triple"
    );
    let old = format!(
        "/ui/api/relation?workspaceId={}&from=Alice&to=Bob&relationType=works_at",
        fx.workspace
    );
    let (status, _) = get_json(&fx.server, &fx.owner, &old).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the old triple is gone");
}

#[tokio::test]
async fn change_relation_type_retypes_in_one_transaction() {
    let fx = fixture().await;
    seed_graph(
        &fx,
        json!({"from": "Alice", "to": "Bob", "relationType": "works_at",
               "observations": [{"body": "joined"}],
               "attributes": {"since": "2020"}}),
    )
    .await;

    let (status, _, body) = post_mutation(
        &fx.server,
        Some(&fx.owner),
        &fx.workspace,
        "changeRelationType",
        json!({"from": "Alice", "to": "Bob",
               "relationType": "works_at", "newRelationType": "likes"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let path = format!(
        "/ui/api/relation?workspaceId={}&from=Alice&to=Bob&relationType=likes",
        fx.workspace
    );
    let (status, body) = get_json(&fx.server, &fx.owner, &path).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["observations"][0]["body"], "joined");
    assert_eq!(body["attributes"]["since"], "2020");
    let old = format!(
        "/ui/api/relation?workspaceId={}&from=Alice&to=Bob&relationType=works_at",
        fx.workspace
    );
    let (status, _) = get_json(&fx.server, &fx.owner, &old).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the old type is gone");
}

#[tokio::test]
async fn reverse_relation_self_loop_is_a_named_noop() {
    let fx = fixture().await;
    seed_graph(
        &fx,
        json!({"from": "Alice", "to": "Alice", "relationType": "self"}),
    )
    .await;

    let (status, _, body) = post_mutation(
        &fx.server,
        Some(&fx.owner),
        &fx.workspace,
        "reverseRelation",
        json!({"from": "Alice", "to": "Alice", "relationType": "self"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "bad_request", "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("self-loop"),
        "the refusal names the no-op: {body}"
    );

    let path = format!(
        "/ui/api/relation?workspaceId={}&from=Alice&to=Alice&relationType=self",
        fx.workspace
    );
    let (status, _) = get_json(&fx.server, &fx.owner, &path).await;
    assert_eq!(status, StatusCode::OK, "the self-loop still exists");
}

#[tokio::test]
async fn duplicate_names_answer_409_with_a_safe_message() {
    let fx = fixture().await;
    seed_graph(
        &fx,
        json!({"from": "Alice", "to": "Bob", "relationType": "works_at"}),
    )
    .await;

    let (status, _, body) = post_mutation(
        &fx.server,
        Some(&fx.owner),
        &fx.workspace,
        "createRelation",
        json!({"from": "Alice", "to": "Bob", "relationType": "works_at"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "conflict", "{body}");
    let message = body["message"].as_str().expect("a message");
    assert!(
        message.contains("already exists"),
        "the message names the conflict without internals: {message}"
    );

    let (status, _, body) = post_mutation(
        &fx.server,
        Some(&fx.owner),
        &fx.workspace,
        "createEntity",
        json!({"name": "Alice", "entityType": "person", "observations": []}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["message"].as_str().unwrap().contains("already exists"));

    let (status, _, body) = post_mutation(
        &fx.server,
        Some(&fx.owner),
        &fx.workspace,
        "renameEntity",
        json!({"oldName": "Alice", "newName": "Bob"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["message"].as_str().unwrap().contains("already exists"));
}

#[tokio::test]
async fn unknown_operation_is_400_and_unknown_or_denied_workspace_is_404() {
    let fx = fixture().await;

    let (status, _, body) = post_mutation(
        &fx.server,
        Some(&fx.owner),
        &fx.workspace,
        "frobnicate",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "bad_request", "{body}");

    let (status, _, unknown) = post_mutation(
        &fx.server,
        Some(&fx.owner),
        "00000000-0000-0000-0000-000000000000",
        "createEntity",
        json!({"name": "X", "entityType": "person", "observations": []}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{unknown}");
    assert_eq!(unknown["code"], "not_found", "{unknown}");

    // The stranger cannot see the workspace at all, so the answer must not
    // reveal that the workspace exists: the identical 404 body.
    let (status, _, denied) = post_mutation(
        &fx.server,
        Some(&fx.stranger),
        &fx.workspace,
        "createEntity",
        json!({"name": "X", "entityType": "person", "observations": []}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{denied}");
    assert_eq!(
        unknown, denied,
        "unknown and denied workspaces answer the same body"
    );
}

#[tokio::test]
async fn unauthenticated_mutation_is_401_with_the_oauth_challenge() {
    let fx = fixture().await;

    let (status, headers, body) = post_mutation(
        &fx.server,
        None,
        &fx.workspace,
        "createEntity",
        json!({"name": "X", "entityType": "person", "observations": []}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "unauthorized", "{body}");
    let challenge = headers
        .get(header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .expect("the 401 carries the RFC 6750 challenge");
    assert!(
        challenge.starts_with("Bearer") && challenge.contains("resource_metadata"),
        "the challenge names the authorization server: {challenge}"
    );
}
