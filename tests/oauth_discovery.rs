//! The two OAuth discovery documents, and the RFC 6750 challenge that points a
//! client at them.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

mod support;

use support::PUBLIC_URL;

fn tools_list() -> Request<Body> {
    Request::post("/mcp")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        ))
        .unwrap()
}

#[tokio::test]
async fn an_unauthenticated_mcp_post_names_the_resource_metadata() {
    let app = support::oauth_router().await;
    let res = app.oneshot(tools_list()).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let header = support::header(&res, "www-authenticate");
    assert_eq!(
        header,
        format!(
            "Bearer resource_metadata=\"{PUBLIC_URL}/.well-known/oauth-protected-resource\", \
             scope=\"graph-read graph-write vectors code\""
        )
    );
}

/// RFC 6749 section 3.3 admits no empty scope list, and RFC 6750 makes the
/// parameter optional, so a server with no category enabled must leave it out
/// rather than send `scope=""`. A parser that rejects the malformed auth-param
/// discards the whole header, and with it the only discovery pointer.
#[tokio::test]
async fn the_challenge_omits_the_scope_parameter_when_no_category_is_enabled() {
    let _guard = support::category_lock().await;
    let server = support::oauth_server_without_categories().await;
    let res = server.router.clone().oneshot(tools_list()).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let header = support::header(&res, "www-authenticate");
    assert_eq!(
        header,
        format!("Bearer resource_metadata=\"{PUBLIC_URL}/.well-known/oauth-protected-resource\"")
    );
}

/// One server must not answer with two shapes of 401. The viewer's data
/// endpoints refuse with the same challenge as `/mcp`, so a scripted client
/// finds the authorization server from either.
#[tokio::test]
async fn the_ui_gate_sends_the_same_challenge_as_the_mcp_endpoint() {
    let server = support::oauth_server().await;
    let mcp = server.router.clone().oneshot(tools_list()).await.unwrap();
    let ui = server
        .router
        .clone()
        .oneshot(Request::get("/ui/graph").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(ui.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        support::header(&ui, "www-authenticate"),
        support::header(&mcp, "www-authenticate")
    );
}

#[tokio::test]
async fn the_protected_resource_document_names_this_server() {
    let app = support::oauth_router().await;
    let res = app
        .oneshot(
            Request::get("/.well-known/oauth-protected-resource")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = support::json(res).await;
    assert_eq!(
        body,
        json!({
            "resource": format!("{PUBLIC_URL}/mcp"),
            "authorization_servers": [PUBLIC_URL],
            "scopes_supported": ["graph-read", "graph-write", "vectors", "code"],
            "bearer_methods_supported": ["header"]
        })
    );
}

/// RFC 9728 section 3.1 inserts the well-known suffix between the host and the
/// path of the resource identifier, so a client holding
/// `https://mem.example.com/mcp` fetches `…/oauth-protected-resource/mcp`. That
/// URL must answer, and section 3.3 requires the `resource` value it returns to
/// be the identifier the client started from.
#[tokio::test]
async fn the_path_suffixed_document_names_the_resource_the_client_asked_about() {
    let app = support::oauth_router().await;
    let res = app
        .oneshot(
            Request::get("/.well-known/oauth-protected-resource/mcp")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = support::json(res).await;
    assert_eq!(body["resource"], format!("{PUBLIC_URL}/mcp"));
    assert_eq!(body["authorization_servers"][0], PUBLIC_URL);
}

/// This server has exactly one protected resource. A suffix naming anything
/// else must not be echoed back as though it were ours.
#[tokio::test]
async fn a_suffix_that_names_no_resource_of_ours_is_not_found() {
    let app = support::oauth_router().await;
    let res = app
        .oneshot(
            Request::get("/.well-known/oauth-protected-resource/somewhere/else")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_authorization_server_document_advertises_pkce_and_cimd() {
    let app = support::oauth_router().await;
    let res = app
        .oneshot(
            Request::get("/.well-known/oauth-authorization-server")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = support::json(res).await;
    assert_eq!(
        body,
        json!({
            "issuer": PUBLIC_URL,
            "authorization_endpoint": format!("{PUBLIC_URL}/oauth/authorize"),
            "token_endpoint": format!("{PUBLIC_URL}/oauth/token"),
            "registration_endpoint": format!("{PUBLIC_URL}/oauth/register"),
            "revocation_endpoint": format!("{PUBLIC_URL}/oauth/revoke"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "client_id_metadata_document_supported": true,
            "authorization_response_iss_parameter_supported": true,
            "scopes_supported": ["graph-read", "graph-write", "vectors", "code"]
        })
    );
}

/// With neither OAuth nor a static token, the server is open: a request with no
/// credential is dispatched, not refused.
#[tokio::test]
async fn a_server_with_no_auth_configured_stays_open() {
    let server = support::open_server().await;
    let res = server.router.clone().oneshot(tools_list()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

/// A test router must reach the tools, not just the transport. The category
/// flags the dispatcher consults are process-wide and are published when the
/// server is built, so a router whose state says `graph-read` is enabled has to
/// advertise the `graph-read` tools.
#[tokio::test]
async fn a_test_router_advertises_the_tools_of_its_enabled_categories() {
    let _guard = support::category_lock().await;
    let server = support::open_server().await;
    let res = server.router.clone().oneshot(tools_list()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = support::json(res).await;
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .expect("tools/list returns an array")
        .iter()
        .map(|t| t["name"].as_str().expect("every tool has a name"))
        .collect();
    assert!(names.contains(&"read_graph"), "tools were: {names:?}");
    assert!(names.contains(&"delete_entities"), "tools were: {names:?}");
}

/// A server with OAuth off advertises no authorization server, so neither
/// discovery document exists.
#[tokio::test]
async fn the_discovery_documents_are_absent_when_oauth_is_off() {
    let server = support::open_server().await;
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-authorization-server",
    ] {
        let res = server
            .router
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "path was: {path}");
    }
}

/// The store opened beside the graph is usable from a test, and its connection
/// carries the busy timeout `Store::new` states as its precondition. Both
/// failures are silent: a store built before the schema is migrated only breaks
/// on its first write, and a missing busy timeout only shows up as a lost
/// refresh-token replay under concurrency. Tasks 5 to 9 reach the store exactly
/// this way.
#[tokio::test]
async fn the_store_is_reachable_and_opens_on_a_migrated_schema() {
    let server = support::oauth_server().await;
    let store = server.oauth().store.lock();

    let record = mcpmem_oauth::store::ClientRecord {
        client_id: "c-1".into(),
        client_name: "probe".into(),
        redirect_uris: vec!["https://claude.ai/callback".into()],
        source: "dcr".into(),
        created_us: 1,
        last_used_us: 1,
    };
    store.put_client(&record).expect("the oauth tables exist");
    assert_eq!(
        store.get_client("c-1").expect("read back"),
        Some(record),
        "the store must round-trip through the migrated schema"
    );

    let timeout: i64 = store
        .connection()
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .unwrap();
    assert!(timeout > 0, "busy_timeout was {timeout}");
}
