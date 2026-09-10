//! HTTP adapters for the OAuth authorization server. Every handler here is thin:
//! it parses the request, calls `mcpmem_oauth`, and shapes the response.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, http::StatusCode};
use parking_lot::Mutex;

use crate::errors::{MCSError, Result};
use crate::http::HttpState;

/// Everything the OAuth routes share. The clock is a field, not a call to the
/// wall clock: expiry tests cannot sleep for an hour.
///
/// The store is behind a mutex because it owns a `rusqlite::Connection`, which
/// is `Send` but not `Sync`. `HttpState` is cloned per request and must be
/// `Send + Sync`, so the connection needs an owner that makes it `Sync`. One
/// serialized connection is also what SQLite wants for writes, and the OAuth
/// tables see one statement per request.
///
/// Never hold the guard across an `await`. The lock is not async-aware, so a
/// task that sleeps while holding it — an upstream token exchange, say —
/// blocks every other OAuth request on that worker. Take the lock, finish the
/// statement, drop the guard.
pub struct OauthState {
    pub config: crate::config::OAuthConfig,
    pub store: Mutex<mcpmem_oauth::store::Store>,
    pub now_us: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl OauthState {
    /// Open the OAuth store on `db_path` and build the shared state.
    ///
    /// # Precondition
    ///
    /// The schema must already be migrated: build the [`crate::kg::GraphHandle`]
    /// first, which runs `initialize_database` and so creates the four
    /// `oauth_*` tables. `busy_timeout_ms` satisfies
    /// [`mcpmem_oauth::store::Store::new`]'s own precondition — without it a
    /// concurrent refresh-token replay fails with `SQLITE_BUSY` instead of
    /// revoking the family.
    pub fn open(
        config: crate::config::OAuthConfig,
        db_path: &Path,
        busy_timeout_ms: u64,
    ) -> Result<OauthState> {
        let conn = rusqlite::Connection::open(db_path).map_err(|e| open_failed(&e))?;
        conn.busy_timeout(Duration::from_millis(busy_timeout_ms))
            .map_err(|e| open_failed(&e))?;
        Ok(OauthState {
            config,
            store: Mutex::new(mcpmem_oauth::store::Store::new(conn)),
            now_us: Arc::new(mcpmem_core::events::now_us),
        })
    }
}

fn open_failed(e: &rusqlite::Error) -> MCSError {
    MCSError::MemoryError(format!("failed to open the OAuth store: {e}"))
}

/// Add the two discovery documents. Both answer 404 when OAuth is off, so a
/// server without OAuth advertises no authorization server at all.
pub fn attach(router: Router<HttpState>) -> Router<HttpState> {
    router
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
}

/// The scopes this server advertises: one slug per enabled tool category.
fn scopes(state: &HttpState) -> Vec<&'static str> {
    state.enabled_categories.iter().map(|c| c.slug()).collect()
}

async fn protected_resource(State(state): State<HttpState>) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Json(mcpmem_oauth::metadata::protected_resource(
        &oauth.config.public_url,
        &scopes(&state),
    ))
    .into_response()
}

async fn authorization_server(State(state): State<HttpState>) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Json(mcpmem_oauth::metadata::authorization_server(
        &oauth.config.public_url,
        &scopes(&state),
    ))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCategory;

    fn config() -> crate::config::OAuthConfig {
        crate::config::OAuthConfig {
            public_url: "https://mem.example.com".into(),
            oidc_issuer: "https://idp.invalid".into(),
            oidc_client_id: "mcpmem-test".into(),
            oidc_client_secret: None,
            principals: Vec::new(),
            cimd_allowed_domains: Vec::new(),
            trust_forwarded_proto: false,
        }
    }

    /// The store opened beside the graph is usable, and its connection carries
    /// the busy timeout [`mcpmem_oauth::store::Store::new`] states as its
    /// precondition. Both fail silently: a store built before the schema is
    /// migrated only breaks on the first write, and a missing busy timeout only
    /// shows up as a lost refresh-token replay under concurrency.
    #[test]
    fn the_store_opens_on_a_migrated_schema_with_a_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let state = HttpState::for_test(
            dir.path().join("t.mcpmem"),
            Some(config()),
            ToolCategory::ALL.to_vec(),
        );
        let oauth = state.oauth.as_ref().expect("oauth is on");
        let store = oauth.store.lock();

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
}
