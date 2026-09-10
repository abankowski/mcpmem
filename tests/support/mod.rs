//! Shared fixtures for the OAuth integration tests.
//!
//! Every test binary compiles this module, and each one uses a subset of it, so
//! unused items here are expected rather than dead.
#![allow(dead_code)]

use axum::Router;
use axum::http::Response;
use http_body_util::BodyExt;
use mcpmem::config::OAuthConfig;
use mcpmem::principals::PrincipalEntry;
use mcpmem::tools::ToolCategory;

pub const PUBLIC_URL: &str = "https://mem.example.com";

/// One allowed human, holding graph-read and graph-write.
pub fn principals(iss: &str) -> Vec<PrincipalEntry> {
    vec![PrincipalEntry {
        name: "adam".into(),
        iss: iss.into(),
        sub: "sub-1".into(),
        label: Some("adam@example.com".into()),
        scopes: vec!["graph-read".into(), "graph-write".into()],
    }]
}

pub fn oauth_config(upstream_issuer: &str) -> OAuthConfig {
    OAuthConfig {
        public_url: PUBLIC_URL.into(),
        oidc_issuer: upstream_issuer.into(),
        oidc_client_id: "mcpmem-test".into(),
        oidc_client_secret: None,
        principals: principals(upstream_issuer),
        cimd_allowed_domains: vec!["claude.ai".into(), "chatgpt.com".into()],
        trust_forwarded_proto: true,
    }
}

/// A router with OAuth on, backed by a fresh temporary database. The directory
/// is returned so the caller keeps it alive for the length of the test.
pub async fn oauth_router_with(upstream_issuer: &str) -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let state = mcpmem::http::HttpState::for_test(
        dir.path().join("t.mcpmem"),
        Some(oauth_config(upstream_issuer)),
        ToolCategory::ALL.to_vec(),
    );
    (dir, mcpmem::http::router(state))
}

/// A router with OAuth on and an upstream that is never reached.
pub async fn oauth_router() -> Router {
    let (dir, router) = oauth_router_with("https://idp.invalid").await;
    // The discovery tests never touch the database after this point, and the
    // operating system removes the directory.
    std::mem::forget(dir);
    router
}

/// A router with OAuth off: no authorization server, and no static token.
pub async fn open_router() -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let state = mcpmem::http::HttpState::for_test(
        dir.path().join("t.mcpmem"),
        None,
        ToolCategory::ALL.to_vec(),
    );
    (dir, mcpmem::http::router(state))
}

pub async fn json(res: Response<axum::body::Body>) -> serde_json::Value {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

pub fn header(res: &Response<axum::body::Body>, name: &str) -> String {
    res.headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing header {name}"))
        .to_str()
        .unwrap()
        .to_owned()
}
