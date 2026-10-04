//! The optional browser UI module: pages, embedded assets, and the thin JSON
//! adapters under `/ui/api/*`.
//!
//! The module exists only in a build with the `ui` feature. It registers
//! nothing on its own: [`attach`] checks the resolved runtime switch and
//! returns the router it was given when the UI is off, so every `/ui/*` path
//! is then an ordinary 404.
//!
//! The MCP transport, OAuth discovery, registration, consent, and token
//! routes are outside this module and stay available in both states; OAuth
//! consent is not a browser-UI feature.

use axum::Router;

use crate::http::HttpState;

pub mod api;
pub mod assets;
pub mod pages;

/// Register the UI routes on `router` when the runtime switch is on.
///
/// The caller (`crate::http::router`) compiles this call only under the `ui`
/// feature. The switch check here is the second gate: a build with the
/// feature and `[server] ui = false` serves no `/ui/*` route, exactly like a
/// build without the feature.
pub fn attach(router: Router<HttpState>, state: &HttpState) -> Router<HttpState> {
    if !state.ui_enabled {
        return router;
    }
    let router = pages::attach(router);
    let router = assets::attach(router);
    let router = api::graph::attach(router);
    let router = api::search::attach(router);
    let router = api::admin::attach(router);
    api::attachments::attach(router)
}
