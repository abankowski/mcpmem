//! The session adapter gives the browser the caller's scopes and server features.
//! It reads credentials only from the Authorization header. A workspace role
//! exists only for an explicit workspace or the caller's saved default.

use std::collections::BTreeSet;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::authz::PrincipalKind;
use crate::http::{
    HttpState, principal_of, store_failure, ui_error, ui_unauthorized, workspace_failure,
};
use crate::principals::{human_key, resolve_human};
use crate::tools::ToolCategory;
use crate::workspace::{WorkspaceAccess, WorkspaceError};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionQuery {
    workspace_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionResponse {
    scopes: BTreeSet<String>,
    principal_name: Option<String>,
    workspace_role: Option<String>,
    features: SessionFeatures,
}

#[derive(Serialize)]
struct SessionFeatures {
    vectors: bool,
    attachments: bool,
    code: bool,
    webhooks: bool,
}

pub fn attach(router: Router<HttpState>) -> Router<HttpState> {
    router.route("/ui/api/session", get(session_handler))
}

/// Return the principal's scopes, its registered name, and the selected role.
/// Server features do not follow the scopes of this credential.
async fn session_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(query): Query<SessionQuery>,
) -> Response {
    let Some(principal) = principal_of(&state, &headers) else {
        return ui_unauthorized(&state);
    };

    let principal_name = if principal.kind == PrincipalKind::Human {
        let Some((iss, sub)) = human_key(&principal.id) else {
            return ui_unauthorized(&state);
        };
        let Some(oauth) = state.oauth.as_ref() else {
            return ui_unauthorized(&state);
        };
        match resolve_human(
            &iss,
            &sub,
            |iss, sub| {
                oauth
                    .config
                    .principals
                    .iter()
                    .find(|entry| entry.key() == (iss, sub))
                    .map(|entry| entry.name.clone())
            },
            |iss, sub| {
                oauth.with_principals(|store| store.get(iss, sub).map(|row| row.map(|r| r.name)))
            },
        ) {
            Ok(name) => name,
            Err(error) => return store_failure(error),
        }
    } else {
        None
    };

    let enabled = &state.enabled_categories;
    let features = SessionFeatures {
        vectors: enabled.contains(&ToolCategory::Vectors),
        attachments: enabled.contains(&ToolCategory::Attachments),
        code: cfg!(feature = "code") && enabled.contains(&ToolCategory::Code),
        webhooks: cfg!(feature = "webhooks"),
    };
    let registry = state.registry;
    let requested = query.workspace_id;
    let principal_id = principal.id;
    let workspace_role = match tokio::task::spawn_blocking(move || {
        let role_of = |workspace_id: &str| {
            registry
                .view(&principal_id, workspace_id)
                .map(|(_, view)| Some(view.role))
        };
        match requested.as_deref() {
            Some(id) => role_of(id),
            None => match registry.resolve(&principal_id, None, WorkspaceAccess::Read) {
                Ok(record) => role_of(&record.workspace_id),
                Err(WorkspaceError::SelectionRequired) => Ok(None),
                Err(error) => Err(error),
            },
        }
    })
    .await
    {
        Ok(Ok(role)) => role,
        Ok(Err(error)) => return workspace_failure(&error),
        Err(error) => {
            error!("/ui/api/session task panicked: {error}");
            return ui_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal error",
            );
        }
    };

    Json(SessionResponse {
        scopes: principal.scopes,
        principal_name,
        workspace_role,
        features,
    })
    .into_response()
}
