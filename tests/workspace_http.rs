#![cfg(feature = "oauth")]
//! HTTP viewer tests for the approved workspace contract: list visibility,
//! not-found reads, grant revocation, the selection-required error, and the
//! explicit `workspaceId` that drives every viewer data route.
//!
//! Two OAuth humans drive these tests: `owner` (graph-read + graph-write,
//! creates and seeds two workspaces) and `reader` (graph-read only, no
//! default). The legacy graph is owned by `machine:local`, so neither human
//! holds any implicit access: every viewer request carries an explicit
//! `workspaceId`, exactly what the shipped dropdown sends.
//!
//! The viewer routes do not resolve `workspaceId` yet (Task 5 step 3), so the
//! requests below answer from the legacy graph — empty, and wrong. A comment
//! marks every assertion that is red for that reason. `/ui/workspaces` does
//! not exist yet and answers 404. The tests turn green when Task 5 resolves
//! one graph per viewer request and adds the workspace list route.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use mcpmem::principals::PrincipalEntry;
use serde_json::{Value, json};

mod support;

/// Plant a live access token the way every minted token is stored, so the
/// bearer path validates it exactly like a walked one (the shape
/// `support::flow::plant_admin_token` uses). The provider is never involved.
fn plant(store: &mcpmem_oauth::store::Store, principal_id: &str, scopes: Vec<String>) -> String {
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
                principal: principal_id.to_owned(),
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

/// One server with two stable humans: the owner (holds graph-write) and the
/// reader (holds graph-read and nothing else). The owner owns one private and
/// one public workspace; each holds its own seed rows.
struct Viewer {
    srv: support::Server,
    owner: String,
    reader: String,
    reader_id: String,
    private_id: String,
    public_id: String,
}

async fn viewer() -> Viewer {
    let mut config = support::oauth_config("https://idp.invalid");
    config.principals.push(PrincipalEntry {
        name: "bob".into(),
        iss: "https://idp.invalid".into(),
        sub: "sub-2".into(),
        label: Some("bob@example.com".into()),
        scopes: vec!["graph-read".into(), "graph-write".into()],
    });
    let srv = support::server(Some(config.clone()), support::Scopes::all(), None).await;
    let owner_id = mcpmem::principals::human_id(&config.principals[0].iss, &config.principals[0].sub);
    let reader_id = mcpmem::principals::human_id(&config.principals[1].iss, &config.principals[1].sub);
    let (owner, reader) = srv.oauth().with_store(|store| {
        (
            plant(
                store,
                &owner_id,
                vec!["graph-read".to_owned(), "graph-write".to_owned()],
            ),
            plant(store, &reader_id, vec!["graph-read".to_owned()]),
        )
    });
    let private_id = create_workspace(&srv, &owner, "Private", "private").await;
    let public_id = create_workspace(&srv, &owner, "Public", "public").await;
    seed(&srv, &owner, &private_id, "Alice", "Acme").await;
    seed(&srv, &owner, &public_id, "Zed", "Zeta").await;
    Viewer {
        srv,
        owner,
        reader,
        reader_id,
        private_id,
        public_id,
    }
}

async fn create_workspace(
    srv: &support::Server,
    token: &str,
    name: &str,
    visibility: &str,
) -> String {
    let resp = mcp(
        srv,
        token,
        "create_workspace",
        json!({ "name": name, "visibility": visibility }),
    )
    .await;
    resp["result"]["workspace"]["workspaceId"]
        .as_str()
        .expect("the created workspace carries its id")
        .to_owned()
}

/// Seed one person with an observation and one company it works at.
async fn seed(srv: &support::Server, token: &str, ws: &str, person: &str, company: &str) {
    let resp = mcp(
        srv,
        token,
        "create_entities",
        json!({
            "workspaceId": ws,
            "entities": [
                { "name": person, "entityType": "person", "observations": [{ "body": "likes hiking" }] },
                { "name": company, "entityType": "company", "observations": [] },
            ],
        }),
    )
    .await;
    assert!(
        resp.get("error").is_none() && resp["result"]["isError"].as_bool() != Some(true),
        "seeding {person} into {ws} failed: {resp}"
    );
    let resp = mcp(
        srv,
        token,
        "create_relations",
        json!({
            "workspaceId": ws,
            "relations": [{ "from": person, "to": company, "relationType": "works_at" }],
        }),
    )
    .await;
    assert!(
        resp.get("error").is_none() && resp["result"]["isError"].as_bool() != Some(true),
        "linking {person}→{company} in {ws} failed: {resp}"
    );
}

/// One `tools/call` as `token`, returning the JSON-RPC envelope.
async fn mcp(srv: &support::Server, token: &str, name: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
    .to_string();
    let res = srv
        .request(
            Request::post("/mcp")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "{name} must dispatch over HTTP"
    );
    support::json(res).await
}

/// One viewer GET as `token`; return (status, raw body).
async fn get(srv: &support::Server, token: &str, path: &str) -> (StatusCode, String) {
    let res = srv
        .request(
            Request::get(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// The `workspaceId`s on one `/ui/workspaces` JSON body.
fn listed_ids(body: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(body).expect("workspace list payload is JSON");
    v["workspaces"]
        .as_array()
        .expect("the list carries a workspaces array")
        .iter()
        .map(|w| w["workspaceId"].as_str().expect("workspaceId").to_owned())
        .collect()
}

/// RED: `/ui/workspaces` does not exist yet, so every listing below is a 404;
/// the assertions about rows cannot even run until the route lands. A public
/// graph appears in an unrelated caller's list; a private graph does not.
#[tokio::test]
async fn list_shows_public_and_hides_private_from_an_unrelated_caller() {
    let fx = viewer().await;

    // The owner sees both of their graphs.
    let (status, body) = get(&fx.srv, &fx.owner, "/ui/workspaces?limit=100").await;
    assert_eq!(status, 200, "the owner list should answer: {body}");
    let ids = listed_ids(&body);
    assert!(
        ids.contains(&fx.private_id) && ids.contains(&fx.public_id),
        "the owner sees both graphs: {ids:?}"
    );

    // The unrelated reader sees the public graph — with its name, visibility
    // and the caller's role — and never the private one.
    let (status, body) = get(&fx.srv, &fx.reader, "/ui/workspaces?limit=100").await;
    assert_eq!(status, 200, "an authed caller can list workspaces: {body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    let workspaces = v["workspaces"].as_array().unwrap();
    let ids = listed_ids(&body);
    assert!(
        ids.contains(&fx.public_id),
        "the public graph appears in another caller's list: {ids:?}"
    );
    assert!(
        !ids.contains(&fx.private_id),
        "the private graph stays absent from an unrelated list: {ids:?}"
    );
    let public = workspaces
        .iter()
        .find(|w| w["workspaceId"] == fx.public_id)
        .expect("the public row is listed");
    assert_eq!(public["name"], "Public");
    assert_eq!(public["visibility"], "public");
    assert_eq!(public["role"], "public");
    assert_eq!(public["isDefault"], false);
}

/// RED: the viewer routes ignore `workspaceId` today, so each request below
/// answers 200 from the legacy graph instead of the approved not-found.
/// Unknown ids and an inaccessible private id return the same shape, so the
/// route leaks neither a name nor an owner.
#[tokio::test]
async fn viewer_read_on_an_inaccessible_graph_is_not_found() {
    let fx = viewer().await;

    let (status, _) = get(
        &fx.srv,
        &fx.reader,
        &format!("/ui/graph?workspaceId={}", fx.private_id),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a private graph the reader cannot access is not-found");

    let (status, _) = get(
        &fx.srv,
        &fx.reader,
        "/ui/graph?workspaceId=00000000-0000-0000-0000-000000000000",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an unknown workspace id is the same not-found"
    );

    let (status, _) = get(
        &fx.srv,
        &fx.reader,
        &format!("/ui/search?workspaceId={}&q=Alice", fx.private_id),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "search on an inaccessible graph is not-found"
    );
}

/// RED: the readable/granted half still answers from the legacy graph (empty),
/// so the content assertions fail, and the post-revoke refusal is still a 200.
#[tokio::test]
async fn revoking_a_grant_changes_the_next_viewer_response() {
    let fx = viewer().await;

    // The owner grants the reader access; the next read and the next list
    // must both open up.
    let grant = mcp(
        &fx.srv,
        &fx.owner,
        "grant_workspace_access",
        json!({
            "workspaceId": fx.private_id.clone(),
            "principalId": fx.reader_id.clone(),
            "role": "reader",
        }),
    )
    .await;
    assert_eq!(grant["result"]["grant"]["role"], "reader", "{grant}");

    let (status, body) = get(
        &fx.srv,
        &fx.reader,
        &format!("/ui/graph?workspaceId={}", fx.private_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a granted reader can read the graph: {body}");
    assert!(
        body.contains("Alice"),
        "the granted read answers with the private graph's rows: {body}"
    );

    let (status, body) = get(
        &fx.srv,
        &fx.reader,
        &format!("/ui/node?workspaceId={}&name=Alice", fx.private_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "node read after grant: {body}");
    assert!(
        body.contains("likes hiking"),
        "the node is the private graph's Alice: {body}"
    );

    let (status, body) = get(
        &fx.srv,
        &fx.reader,
        &format!("/ui/expand?workspaceId={}&name=Alice", fx.private_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "expand after grant: {body}");
    assert!(
        body.contains("Acme"),
        "the neighbourhood is the private graph's: {body}"
    );

    let (status, body) = get(&fx.srv, &fx.reader, "/ui/workspaces?limit=100").await;
    assert_eq!(status, StatusCode::OK, "the list after grant: {body}");
    assert!(
        listed_ids(&body).contains(&fx.private_id),
        "the granted graph is listed for the reader"
    );

    // Revoke: the next viewer response must be refused again, and the graph
    // must leave the list.
    let revoked = mcp(
        &fx.srv,
        &fx.owner,
        "revoke_workspace_access",
        json!({ "workspaceId": fx.private_id.clone(), "principalId": fx.reader_id.clone() }),
    )
    .await;
    let text: Value = serde_json::from_str(
        revoked["result"]["content"][0]["text"]
            .as_str()
            .expect("revoke reports its text result"),
    )
    .expect("revoke text is JSON");
    assert_eq!(text["revoked"], true, "the grant is revoked");

    let (status, _) = get(
        &fx.srv,
        &fx.reader,
        &format!("/ui/graph?workspaceId={}", fx.private_id),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "revocation blocks the next read, not a later one"
    );

    // The shape assertion extends the same not-found to node and expand; the
    // graph read above is the assertion that fails red before resolution
    // lands.
    let (status, _) = get(
        &fx.srv,
        &fx.reader,
        &format!("/ui/node?workspaceId={}&name=Alice", fx.private_id),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "node read after revocation");
    let (status, _) = get(
        &fx.srv,
        &fx.reader,
        &format!("/ui/expand?workspaceId={}&name=Alice", fx.private_id),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "expand after revocation");

    let (status, body) = get(&fx.srv, &fx.reader, "/ui/workspaces?limit=100").await;
    assert_eq!(status, StatusCode::OK, "the list after revoke: {body}");
    assert!(
        !listed_ids(&body).contains(&fx.private_id),
        "the revoked graph leaves the reader's list: {body}"
    );
}

/// RED: without a `workspaceId` the routes still answer 200 from the legacy
/// graph, so the distinct error never appears. The reader has no default and,
/// before any grant, no access to anything — the error must be selection
/// required, never a fallback to the public graph.
#[tokio::test]
async fn missing_workspace_id_without_a_default_is_selection_required() {
    let fx = viewer().await;

    let (status, body) = get(&fx.srv, &fx.reader, "/ui/graph").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "no id and no default is the selection-required error, not a read: {body}"
    );
    assert!(
        body.to_lowercase().contains("workspace selection required"),
        "the error is the distinct selection-required shape: {body}"
    );

    let (status, body) = get(&fx.srv, &fx.reader, "/ui/search?q=Alice").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "search without a selection is refused too: {body}"
    );
    assert!(
        body.to_lowercase().contains("workspace selection required"),
        "{body}"
    );
}

/// RED: the viewer routes ignore `workspaceId`, so every request below answers
/// from the legacy graph — empty, and wrong — instead of the selected one.
/// The same request shape drives graph, search, node and expand.
#[tokio::test]
async fn explicit_workspace_id_drives_graph_search_node_and_expand() {
    let fx = viewer().await;

    // Graph: each id returns only that graph's rows.
    let (status, body) = get(
        &fx.srv,
        &fx.owner,
        &format!("/ui/graph?workspaceId={}", fx.private_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "private graph read: {body}");
    assert!(
        body.contains("Alice"),
        "the private graph payload names Alice: {body}"
    );
    assert!(
        !body.contains("Zed"),
        "the private graph never mixes public nodes: {body}"
    );

    let (status, body) = get(
        &fx.srv,
        &fx.owner,
        &format!("/ui/graph?workspaceId={}", fx.public_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "public graph read: {body}");
    assert!(
        body.contains("Zed"),
        "the public graph payload names Zed: {body}"
    );
    assert!(
        !body.contains("Alice"),
        "the public graph never mixes private nodes: {body}"
    );

    // Search: the query runs against the selected file.
    let (status, body) = get(
        &fx.srv,
        &fx.owner,
        &format!("/ui/search?workspaceId={}&q=Ali", fx.private_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "private search: {body}");
    assert!(
        body.contains("Alice"),
        "search in the private graph finds Alice: {body}"
    );

    let (status, body) = get(
        &fx.srv,
        &fx.owner,
        &format!("/ui/search?workspaceId={}&q=Zed", fx.public_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "public search: {body}");
    assert!(
        body.contains("Zed"),
        "search in the public graph finds Zed: {body}"
    );

    // Node: the same name exists in only one file.
    let (status, body) = get(
        &fx.srv,
        &fx.owner,
        &format!("/ui/node?workspaceId={}&name=Alice", fx.private_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "node in the private graph: {body}");
    assert!(
        body.contains("likes hiking"),
        "the node is the private graph's Alice: {body}"
    );
    let (status, _) = get(
        &fx.srv,
        &fx.owner,
        &format!("/ui/node?workspaceId={}&name=Alice", fx.public_id),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "Alice exists only in the private graph"
    );

    // Expand: the neighbourhood comes from the selected file.
    let (status, body) = get(
        &fx.srv,
        &fx.owner,
        &format!("/ui/expand?workspaceId={}&name=Alice", fx.private_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "expand in the private graph: {body}");
    assert!(
        body.contains("Acme"),
        "the neighbourhood is the private graph's: {body}"
    );
    let (status, _) = get(
        &fx.srv,
        &fx.owner,
        &format!("/ui/expand?workspaceId={}&name=Alice", fx.public_id),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the neighbourhood of Alice is private"
    );
}