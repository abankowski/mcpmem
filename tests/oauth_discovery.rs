//! The two OAuth discovery documents, and the RFC 6750 challenge that points a
//! client at them.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

mod support;

#[tokio::test]
async fn an_unauthenticated_mcp_post_names_the_resource_metadata() {
    let app = support::oauth_router().await;
    let res = app
        .oneshot(
            Request::post("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let header = support::header(&res, "www-authenticate");
    assert!(header.starts_with("Bearer "), "header was: {header}");
    assert!(
        header.contains(
            r#"resource_metadata="https://mem.example.com/.well-known/oauth-protected-resource""#
        ),
        "header was: {header}"
    );
    assert!(header.contains(r#"scope=""#), "header was: {header}");
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
    assert_eq!(body["resource"], "https://mem.example.com/mcp");
    assert_eq!(body["authorization_servers"][0], "https://mem.example.com");
    assert!(
        body["scopes_supported"]
            .as_array()
            .unwrap()
            .contains(&"graph-read".into())
    );
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
    let body: serde_json::Value = support::json(res).await;
    assert_eq!(body["issuer"], "https://mem.example.com");
    assert_eq!(body["code_challenge_methods_supported"][0], "S256");
    assert_eq!(body["client_id_metadata_document_supported"], true);
    assert_eq!(body["authorization_response_iss_parameter_supported"], true);
    assert_eq!(body["token_endpoint_auth_methods_supported"][0], "none");
    assert_eq!(
        body["registration_endpoint"],
        "https://mem.example.com/oauth/register"
    );
}

/// With neither OAuth nor a static token, the server is open: a request with no
/// credential is dispatched, not refused.
#[tokio::test]
async fn a_server_with_no_auth_configured_stays_open() {
    let (_dir, app) = support::open_router().await;
    let res = app
        .oneshot(
            Request::post("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

/// A server with OAuth off advertises no authorization server, so neither
/// discovery document exists.
#[tokio::test]
async fn the_discovery_documents_are_absent_when_oauth_is_off() {
    let (_dir, app) = support::open_router().await;
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-authorization-server",
    ] {
        let res = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "path was: {path}");
    }
}
