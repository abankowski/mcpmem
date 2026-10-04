//! The browser pages and the embedded React shell.
//!
//! Every page route serves the one built shell (`ui/dist/index.html`):
//! the React app routes `/ui`, `/ui/search`, and `/ui/admin/*` client-side.
//! The shell and its assets hold no data, so they are served without auth;
//! the `/ui/api/*` adapters are the gate.

use axum::Router;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::http::HttpState;

/// The built React shell, embedded at compile time. The manifest lists its
/// assets; the shell itself is the entry the pages answer with.
const UI_SHELL: &str = include_str!("../../ui/dist/index.html");

/// Register the page routes. The callback URL serves the same shell: after
/// the provider redirects back with `?code=`, the app in the page completes
/// the PKCE exchange. Every other `/ui/admin/*` subpath also serves the
/// shell, so a reload on an admin subpage loads its assets and the app
/// routes the path client-side; the exact callback route wins over the
/// catch-all during matching.
pub fn attach(router: Router<HttpState>) -> Router<HttpState> {
    router
        .route("/ui", get(page_handler))
        .route("/ui/search", get(page_handler))
        .route("/ui/admin", get(page_handler))
        .route("/ui/admin/callback", get(page_handler))
        .route("/ui/admin/{*rest}", get(page_handler))
}

/// `GET /ui` (and `/ui/search`, `/ui/admin`, `/ui/admin/callback`, and any
/// `/ui/admin/*` subpath) — the React shell. The pages hold no graph or
/// admin data, so no route needs auth; every data read goes through the
/// `/ui/api/*` gate.
async fn page_handler(State(_state): State<HttpState>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        UI_SHELL,
    )
        .into_response()
}
