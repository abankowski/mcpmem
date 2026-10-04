#![cfg(all(feature = "oauth", feature = "ui"))]
//! The HTTP contract for `GET /ui/api/session`.
//! The route has no graph-read gate. It returns credential scopes and server
//! features as separate facts, and resolves only a selected or saved workspace.

use axum::body::Body;
use axum::http::{Response, StatusCode, header};
use mcpmem::principals::{PrincipalEntry, human_id};
use mcpmem::tools::ToolCategory;
use serde_json::{Value, json};

mod support;

async fn session(server: &support::Server, token: Option<&str>, path: &str) -> Response<Body> {
    support::send(
        server,
        token.unwrap_or(""),
        "GET",
        path,
        Body::empty(),
        None,
    )
    .await
}

async fn create_workspace(
    server: &support::Server,
    token: &str,
    name: &str,
    visibility: &str,
) -> String {
    let created = support::mcp(
        server,
        token,
        "create_workspace",
        json!({"name": name, "visibility": visibility}),
    )
    .await;
    created["result"]["workspace"]["workspaceId"]
        .as_str()
        .unwrap_or_else(|| panic!("workspace creation failed: {created}"))
        .to_owned()
}

struct RoleFixture {
    server: support::Server,
    owner: String,
    writer: String,
    reader: String,
    outsider: String,
    private_id: String,
    public_id: String,
}

async fn role_fixture() -> RoleFixture {
    let mut config = support::oauth_config("https://idp.invalid");
    for (name, sub) in [
        ("Writer", "sub-2"),
        ("Reader", "sub-3"),
        ("Outsider", "sub-4"),
    ] {
        config.principals.push(PrincipalEntry {
            name: name.into(),
            iss: "https://idp.invalid".into(),
            sub: sub.into(),
            label: None,
            scopes: vec!["graph-read".into()],
        });
    }
    let server = support::server(Some(config.clone()), support::Scopes::all(), None).await;
    let (owner, writer, reader, outsider) = server.oauth().with_store(|store| {
        let id =
            |index: usize| human_id(&config.principals[index].iss, &config.principals[index].sub);
        (
            support::plant(store, &id(0), &["graph-read", "graph-write"]),
            support::plant(store, &id(1), &["graph-read"]),
            support::plant(store, &id(2), &["graph-read"]),
            support::plant(store, &id(3), &["graph-read"]),
        )
    });
    let private_id = create_workspace(&server, &owner, "Private", "private").await;
    let public_id = create_workspace(&server, &owner, "Public", "public").await;
    for (index, role) in [(1, "writer"), (2, "reader")] {
        let principal_id = human_id(&config.principals[index].iss, &config.principals[index].sub);
        let grant = support::mcp(
            &server,
            &owner,
            "grant_workspace_access",
            json!({"workspaceId": private_id, "principalId": principal_id, "role": role}),
        )
        .await;
        assert_eq!(grant["result"]["grant"]["role"], role, "{grant}");
    }
    RoleFixture {
        server,
        owner,
        writer,
        reader,
        outsider,
        private_id,
        public_id,
    }
}

#[tokio::test]
async fn session_requires_a_header_and_rejects_a_valid_query_token() {
    let server = support::static_server().await;
    let missing = session(&server, None, "/ui/api/session").await;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        support::header(&missing, header::WWW_AUTHENTICATE.as_str()),
        "Bearer"
    );

    let query = session(
        &server,
        None,
        &format!("/ui/api/session?token={}", support::STATIC_BEARER),
    )
    .await;
    assert_eq!(query.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        support::header(&query, header::WWW_AUTHENTICATE.as_str()),
        "Bearer"
    );
}

#[tokio::test]
async fn static_session_has_exact_keys_null_name_and_null_workspace_role() {
    let server = support::server_with_static_token(
        None,
        support::STATIC_BEARER,
        support::Scopes::bearer_holds(vec![ToolCategory::GraphRead]),
        None,
    )
    .await;
    let response = session(&server, Some(support::STATIC_BEARER), "/ui/api/session").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        support::json(response).await,
        json!({
            "scopes": ["graph-read"],
            "principalName": null,
            "workspaceRole": null,
            "features": {
                "vectors": true,
                "attachments": true,
                "code": cfg!(feature = "code"),
                "webhooks": cfg!(feature = "webhooks"),
            },
        })
    );
}

#[tokio::test]
async fn human_session_uses_the_registered_name_not_the_label_or_identity() {
    let mut config = support::oauth_config("https://idp.invalid");
    config.principals[0].name = "Ada Lovelace".into();
    config.principals[0].label = Some("ada@example.com".into());
    let server = support::server(Some(config.clone()), support::Scopes::all(), None).await;
    let file_id = human_id(&config.principals[0].iss, &config.principals[0].sub);
    let file_token = server
        .oauth()
        .with_store(|store| support::plant(store, &file_id, &["graph-read", "graph-write"]));
    let response = session(&server, Some(&file_token), "/ui/api/session").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        support::json(response).await,
        json!({
            "scopes": ["graph-read", "graph-write"],
            "principalName": "Ada Lovelace",
            "workspaceRole": null,
            "features": {
                "vectors": true,
                "attachments": true,
                "code": cfg!(feature = "code"),
                "webhooks": cfg!(feature = "webhooks"),
            },
        })
    );

    server.oauth().with_principals(|store| {
        store
            .create(
                "https://idp.invalid",
                "runtime-sub",
                "Rita Runtime",
                Some("different@example.com"),
                &["graph-read".into()],
            )
            .expect("register a runtime human");
    });
    let runtime_id = human_id("https://idp.invalid", "runtime-sub");
    let runtime_token = server
        .oauth()
        .with_store(|store| support::plant(store, &runtime_id, &["graph-read"]));
    let response = session(&server, Some(&runtime_token), "/ui/api/session").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        support::json(response).await["principalName"],
        "Rita Runtime"
    );
}

#[tokio::test]
async fn session_resolves_owner_writer_reader_and_public_without_a_list_lookup() {
    let fx = role_fixture().await;
    for (token, id, expected) in [
        (&fx.owner, &fx.private_id, "owner"),
        (&fx.writer, &fx.private_id, "writer"),
        (&fx.reader, &fx.private_id, "reader"),
        (&fx.outsider, &fx.public_id, "public"),
    ] {
        let path = format!("/ui/api/session?workspaceId={id}");
        let response = session(&fx.server, Some(token), &path).await;
        assert_eq!(response.status(), StatusCode::OK, "{expected} on {id}");
        assert_eq!(support::json(response).await["workspaceRole"], expected);
    }

    // Creation saves the owner's first workspace as their default.
    let default = session(&fx.server, Some(&fx.owner), "/ui/api/session").await;
    assert_eq!(default.status(), StatusCode::OK);
    assert_eq!(support::json(default).await["workspaceRole"], "owner");

    // Grants and public visibility do not create a saved selection.
    let none = session(&fx.server, Some(&fx.reader), "/ui/api/session").await;
    assert_eq!(none.status(), StatusCode::OK);
    let body = support::json(none).await;
    assert!(body.as_object().unwrap().contains_key("workspaceRole"));
    assert_eq!(body["workspaceRole"], Value::Null);
}

#[tokio::test]
async fn inaccessible_and_missing_explicit_workspaces_have_the_same_404() {
    let fx = role_fixture().await;
    let denied = session(
        &fx.server,
        Some(&fx.outsider),
        &format!("/ui/api/session?workspaceId={}", fx.private_id),
    )
    .await;
    let missing = session(
        &fx.server,
        Some(&fx.outsider),
        "/ui/api/session?workspaceId=00000000-0000-0000-0000-000000000000",
    )
    .await;
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(support::json(denied).await, support::json(missing).await);
}

#[tokio::test]
async fn credential_scopes_do_not_turn_disabled_server_categories_on() {
    let server = support::server_with_static_token(
        None,
        support::STATIC_BEARER,
        support::Scopes {
            bearer: vec![
                ToolCategory::Vectors,
                ToolCategory::Attachments,
                ToolCategory::Code,
                ToolCategory::GraphRead,
            ],
            enabled: vec![],
        },
        None,
    )
    .await;
    let response = session(&server, Some(support::STATIC_BEARER), "/ui/api/session").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        support::json(response).await,
        json!({
            "scopes": ["attachments", "code", "graph-read", "vectors"],
            "principalName": null,
            "workspaceRole": null,
            "features": {
                "vectors": false,
                "attachments": false,
                "code": false,
                "webhooks": cfg!(feature = "webhooks"),
            },
        })
    );
}

#[tokio::test]
async fn admin_only_session_does_not_require_graph_read_or_a_workspace() {
    let config = support::oauth_config("https://idp.invalid");
    let server = support::server(
        Some(config.clone()),
        support::Scopes {
            bearer: vec![],
            enabled: vec![],
        },
        None,
    )
    .await;
    let id = human_id(&config.principals[0].iss, &config.principals[0].sub);
    let token = server
        .oauth()
        .with_store(|store| support::plant(store, &id, &["admin"]));
    let response = session(&server, Some(&token), "/ui/api/session").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        support::json(response).await,
        json!({
            "scopes": ["admin"],
            "principalName": "adam",
            "workspaceRole": null,
            "features": {
                "vectors": false,
                "attachments": false,
                "code": false,
                "webhooks": cfg!(feature = "webhooks"),
            },
        })
    );
}
