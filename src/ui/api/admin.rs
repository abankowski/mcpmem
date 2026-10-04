//! The admin adapters under `/ui/api/*`: principals, waitlist, webhook
//! subscriptions, and managed repositories.
//!
//! Every route runs the [`crate::http::admin_gate`] first: a human resolved
//! through an OAuth grant holding the `admin` scope. The static bearer token
//! can never hold admin, so every admin is a human. The webhook and repo
//! groups keep their feature gates too: their routes exist only when the
//! `webhooks` and `code` features compiled them in.

#[cfg(feature = "webhooks")]
use std::collections::HashMap;

#[cfg(feature = "webhooks")]
use axum::extract::Query;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
#[cfg(feature = "webhooks")]
use serde_json::json;

#[cfg(feature = "webhooks")]
use crate::http::workspace_failure;
use crate::http::{
    HttpState, admin_gate, bad_request, conflict, json_error, not_found, oauth_store_failure,
    store_failure,
};

/// The subscription tools and the admin handlers below share the store's
/// validation: both call [`webhooks_actions`]' checks, so the MCP surface
/// and the admin surface accept exactly the same payloads.
#[cfg(feature = "webhooks")]
use crate::actions::webhooks as webhooks_actions;
/// The subscription store and its admin handlers exist only in a build with
/// the `webhooks` feature, so everything they import is gated with them: a
/// default build has no webhook types in this namespace at all.
#[cfg(feature = "webhooks")]
use mcpmem_core::mutation::ChangeOperation;
#[cfg(feature = "webhooks")]
use mcpmem_core::subscriptions::{SubscriptionRepository, WebhookSubscription};
#[cfg(feature = "webhooks")]
use mcpmem_webhook::WorkerError;
#[cfg(feature = "webhooks")]
use uuid::Uuid;

/// Register the admin routes that exist in every UI build: principals and
/// waitlist. The webhook and repo groups attach under their own feature
/// gates through [`attach_webhook_admin_routes`] and
/// [`attach_repo_admin_routes`].
pub fn attach(router: Router<HttpState>) -> Router<HttpState> {
    let router = router
        .route(
            "/ui/api/principals",
            get(admin_list_principals).post(admin_create_principal),
        )
        .route(
            "/ui/api/principals/{id}",
            patch(admin_update_principal).delete(admin_delete_principal),
        )
        .route("/ui/api/waitlist", get(admin_list_waitlist))
        .route(
            "/ui/api/waitlist/{id}/approve",
            post(admin_approve_waitlist),
        )
        .route("/ui/api/waitlist/{id}", delete(admin_dismiss_waitlist));
    // The webhook-subscription admin API exists only in a build with the
    // `webhooks` feature. Elsewhere the routes are absent and the SPA
    // answers their 404 by hiding the section.
    #[cfg(feature = "webhooks")]
    let router = attach_webhook_admin_routes(router);
    // The managed-repo admin API exists only in a build with the `code`
    // feature. Elsewhere the routes are absent and the SPA hides the section.
    #[cfg(feature = "code")]
    let router = attach_repo_admin_routes(router);
    router
}

/// The id path segment → (iss, sub).
fn key_of_id(id: &str) -> Option<(String, String)> {
    mcpmem_oauth::parse_principal_id(id)
}

/// Whether a built-in principal owns this identity. The keys are owned
/// pairs; `contains` cannot borrow a `(&str, &str)` from them, so the
/// comparison is spelled out rather than cloned per row.
fn is_builtin(oauth: &crate::oauth_routes::OauthState, iss: &str, sub: &str) -> bool {
    oauth.builtin_keys.iter().any(|(i, s)| i == iss && s == sub)
}

/// One principal as the admin API answers it: built-ins from the principals
/// file, runtime rows from the store, both in the one shape.
#[derive(serde::Serialize)]
struct PrincipalView {
    id: String,
    name: String,
    iss: String,
    sub: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    scopes: Vec<String>,
    builtin: bool,
    /// The SPA reads this key; the underscore spelling is the field's, the
    /// wire spelling is the contract's.
    #[serde(rename = "maskedByBuiltin")]
    masked_by_builtin: bool,
}

#[derive(Deserialize)]
struct PrincipalInput {
    name: String,
    iss: String,
    sub: String,
    #[serde(default)]
    label: Option<String>,
    scopes: Vec<String>,
}

#[derive(Deserialize)]
struct PrincipalPatch {
    #[serde(default)]
    name: Option<String>,
    /// Some("") clears the label.
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct ApproveBody {
    #[serde(default)]
    scopes: Option<Vec<String>>,
}

/// `GET /ui/api/principals` — every principal the server knows: the
/// built-ins from the principals file, then the runtime rows, with a row
/// whose identity a built-in owns marked `masked_by_builtin`. The sidebar
/// reads the mask flag and the `defaultNewPrincipalScopes` list to render
/// the create form.
async fn admin_list_principals(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut out: Vec<PrincipalView> = oauth
        .config
        .principals
        .iter()
        .map(|p| PrincipalView {
            id: mcpmem_oauth::principal_id(&p.iss, &p.sub),
            name: p.name.clone(),
            iss: p.iss.clone(),
            sub: p.sub.clone(),
            label: p.label.clone(),
            scopes: p.scopes.clone(),
            builtin: true,
            masked_by_builtin: false,
        })
        .collect();
    let runtime = match oauth.with_principals(|s| s.list()) {
        Ok(rows) => rows,
        Err(e) => return store_failure(e),
    };
    for row in runtime {
        let masked = is_builtin(oauth, &row.iss, &row.sub);
        out.push(PrincipalView {
            id: mcpmem_oauth::principal_id(&row.iss, &row.sub),
            name: row.name,
            iss: row.iss,
            sub: row.sub,
            label: row.label,
            scopes: row.scopes,
            builtin: false,
            masked_by_builtin: masked,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "principals": out,
            "defaultNewPrincipalScopes": oauth.config.default_new_principal_scopes,
        })),
    )
        .into_response()
}

/// `POST /ui/api/principals` — create one runtime principal. A key a
/// built-in owns is refused before the store is touched: the principals file
/// is the operator's source of truth for those identities.
async fn admin_create_principal(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let input: PrincipalInput = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("the body must be a JSON principal"),
    };
    // Trim into the stored values, like the file loader does: an untrimmed
    // `sub` can never equal a provider claim, so a stray space would create
    // a principal that can never authenticate — and whitespace-only
    // variants of a built-in key would slip past the collision check.
    let name = input.name.trim();
    let iss = input.iss.trim();
    let sub = input.sub.trim();
    if name.is_empty() || iss.is_empty() || sub.is_empty() {
        return bad_request("name, iss and sub are required");
    }
    let scopes = match crate::principals::canonical_scopes(&input.scopes) {
        Ok(s) if !s.is_empty() => s,
        _ => return bad_request("at least one known scope is required"),
    };
    if is_builtin(oauth, iss, sub) {
        return conflict("a built-in principal owns this identity; it is immutable");
    }
    // A duplicate runtime key is refused before the store is touched, so the
    // answer is 409 and not the store's UNIQUE-constraint error.
    match oauth.with_principals(|s| s.get(iss, sub)) {
        Ok(Some(_)) => return conflict("a runtime principal already owns this identity"),
        Ok(None) => {}
        Err(e) => return store_failure(e),
    }
    if let Err(e) =
        oauth.with_principals(|s| s.create(iss, sub, name, input.label.as_deref(), &scopes))
    {
        // The pre-check above is not a lock: two concurrent identical POSTs
        // can both pass it, and the second then hits the UNIQUE constraint.
        // That race is classified by the store and answered as a conflict
        // too, so the caller sees the same 409 either way.
        if matches!(e, crate::errors::MCSError::ConstraintViolation(_)) {
            return conflict("a runtime principal already owns this identity");
        }
        return store_failure(e);
    }
    let view = PrincipalView {
        id: mcpmem_oauth::principal_id(iss, sub),
        name: name.to_owned(),
        iss: iss.to_owned(),
        sub: sub.to_owned(),
        label: input.label,
        scopes,
        builtin: false,
        masked_by_builtin: false,
    };
    (StatusCode::CREATED, Json(view)).into_response()
}

/// `PATCH /ui/api/principals/{id}` — change name, label or scopes of one
/// runtime principal. Every field is optional; a named field replaces the
/// stored value.
async fn admin_update_principal(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: String,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((iss, sub)) = key_of_id(&id) else {
        return not_found();
    };
    if is_builtin(oauth, &iss, &sub) {
        return conflict("a built-in principal owns this identity; it is immutable");
    }
    let row = match oauth.with_principals(|s| s.get(&iss, &sub)) {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(e) => return store_failure(e),
    };
    let patch: PrincipalPatch = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("the body must be a JSON patch"),
    };
    let name = patch.name.as_deref().unwrap_or(&row.name).trim().to_owned();
    if name.is_empty() {
        return bad_request("name must not be empty");
    }
    let label = match patch.label {
        Some(raw) if raw.is_empty() => None,
        value => value.or(row.label.clone()),
    };
    let scopes = match patch.scopes {
        Some(raw) => match crate::principals::canonical_scopes(&raw) {
            Ok(s) if !s.is_empty() => s,
            _ => return bad_request("at least one known scope is required"),
        },
        None => row.scopes,
    };
    if let Err(e) =
        oauth.with_principals(|s| s.update(&iss, &sub, &name, label.as_deref(), &scopes))
    {
        return store_failure(e);
    }
    (
        StatusCode::OK,
        Json(PrincipalView {
            id,
            name,
            iss,
            sub,
            label,
            scopes,
            builtin: false,
            masked_by_builtin: false,
        }),
    )
        .into_response()
}

/// `DELETE /ui/api/principals/{id}` — remove a runtime principal, its
/// workspace access, its pending OAuth grants and its live token families.
///
/// The admin path keeps its bare ID segment. Convert it to the stable human
/// ID before the owner check or token revocation.
async fn admin_delete_principal(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((iss, sub)) = key_of_id(&id) else {
        return not_found();
    };
    if is_builtin(oauth, &iss, &sub) {
        return conflict("a built-in principal owns this identity; it is immutable");
    }
    let row = match oauth.with_principals(|s| s.get(&iss, &sub)) {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(e) => return store_failure(e),
    };
    let stable_id = crate::principals::human_id(&iss, &sub);
    // Hold the sidecar write lock across the owner check, OAuth revocation,
    // runtime row deletion, and access cleanup. Get the OAuth and runtime
    // store locks only after the registry lock; neither store calls back into
    // the registry here. A failed store operation rolls back access cleanup.
    // A registry commit error after the runtime delete cannot restore that
    // row across the two SQLite files. Return the registry error in that case.
    let revoked = match state.registry.with_human_access_cleanup(&stable_id, || {
        let revoked = oauth
            .revoke_principal(&stable_id)
            .map_err(|error| Box::new(oauth_store_failure(error)))?;
        oauth
            .with_principals(|s| s.delete(&iss, &sub))
            .map_err(|error| Box::new(store_failure(error)))?;
        Ok::<_, Box<Response>>(revoked)
    }) {
        Ok(Ok(revoked)) => revoked,
        Ok(Err(response)) => return *response,
        Err(crate::workspace::WorkspaceError::AccessDenied) => {
            return conflict("a workspace owner cannot be deleted");
        }
        Err(error) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("workspace registry: {error}"),
            );
        }
    };
    tracing::info!(
        name = %row.name,
        revoked,
        "deleted principal; removed workspace access, pending grants and token families"
    );
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /ui/api/waitlist` — every entry awaiting approval, in the order the
/// store keeps them.
async fn admin_list_waitlist(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let entries = match oauth.with_principals(|s| s.waitlist()) {
        Ok(rows) => rows,
        Err(e) => return store_failure(e),
    };
    let out: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|e| {
            serde_json::json!({
                "id": mcpmem_oauth::principal_id(&e.iss, &e.sub),
                "name": e.name,
                "iss": e.iss,
                "sub": e.sub,
                "firstSeenUs": e.first_seen_us,
                "lastSeenUs": e.last_seen_us,
            })
        })
        .collect();
    (StatusCode::OK, Json(serde_json::json!({ "entries": out }))).into_response()
}

/// `POST /ui/api/waitlist/{id}/approve` — promote one entry to a runtime
/// principal in one transaction. A body without a `scopes` member promotes
/// with the configured default list.
async fn admin_approve_waitlist(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: String,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((iss, sub)) = key_of_id(&id) else {
        return not_found();
    };
    let scopes = match serde_json::from_str::<ApproveBody>(&body) {
        Ok(body) => body
            .scopes
            .unwrap_or_else(|| oauth.config.default_new_principal_scopes.clone()),
        Err(_) => return bad_request("the body must be JSON with an optional scopes list"),
    };
    let scopes = match crate::principals::canonical_scopes(&scopes) {
        Ok(s) if !s.is_empty() => s,
        _ => return bad_request("at least one known scope is required"),
    };
    match oauth.with_principals(|s| s.approve(&iss, &sub, &scopes)) {
        Ok(Some(row)) => (
            StatusCode::CREATED,
            Json(PrincipalView {
                id: mcpmem_oauth::principal_id(&iss, &sub),
                name: row.name,
                iss,
                sub,
                label: None,
                scopes,
                builtin: false,
                masked_by_builtin: false,
            }),
        )
            .into_response(),
        Ok(None) => not_found(),
        Err(e) => store_failure(e),
    }
}

/// `DELETE /ui/api/waitlist/{id}` — discard one entry without promoting it.
async fn admin_dismiss_waitlist(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((iss, sub)) = key_of_id(&id) else {
        return not_found();
    };
    match oauth.with_principals(|s| s.dismiss_waitlist(&iss, &sub)) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => store_failure(e),
    }
}

// ---------------------------------------------------------------------------
// Webhook subscriptions (`/ui/api/webhooks`), behind the `webhooks` feature
// ---------------------------------------------------------------------------

/// The list response. A wrapper struct rather than a hand-built JSON map,
/// so the rows serialize through the stored type's serde spelling — the
/// same `subscriptionId` / `eventOperations` / `consumerOrigin` / `secretRef`
/// keys the SPA echoes back unchanged.
#[cfg(feature = "webhooks")]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WebhookList {
    subscriptions: Vec<WebhookSubscription>,
    /// The names of the signing keys this process can deliver with, sorted.
    /// An empty list is a store-now deployment: a reference is accepted at
    /// registration time and resolved by the delivery process.
    configured_secrets: Vec<String>,
    /// Whether this process runs the delivery worker. The UI warns when a
    /// subscription is stored but nothing can deliver it.
    delivery_role: bool,
}

/// The create body: every stored field except the server-generated id.
#[cfg(feature = "webhooks")]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebhookInput {
    endpoint: String,
    consumer_origin: String,
    secret_ref: String,
    #[serde(default)]
    event_operations: Vec<ChangeOperation>,
    #[serde(default)]
    entity_types: Vec<String>,
    #[serde(default)]
    ignored_origins: Vec<String>,
    /// Matches the MCP tool: absent means enabled.
    #[serde(default)]
    enabled: Option<bool>,
}

/// The patch body: every field optional; a named field replaces the stored
/// value. An empty list in `eventOperations` / `entityTypes` /
/// `ignoredOrigins` replaces with "deliver every operation / type / origin".
#[cfg(feature = "webhooks")]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebhookPatch {
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    consumer_origin: Option<String>,
    #[serde(default)]
    secret_ref: Option<String>,
    #[serde(default)]
    event_operations: Option<Vec<ChangeOperation>>,
    #[serde(default)]
    entity_types: Option<Vec<String>>,
    #[serde(default)]
    ignored_origins: Option<Vec<String>>,
    #[serde(default)]
    enabled: Option<bool>,
}

/// A 500 whose message names the webhook store, so an operator does not
/// chase the principals store for a subscription failure.
#[cfg(feature = "webhooks")]
fn webhook_store_failure(e: impl std::fmt::Display) -> Response {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("webhook store: {e}"),
    )
}

/// Authorize one admin request before opening its workspace subscription
/// store. A non-owner gets the same response as an unknown workspace.
#[cfg(feature = "webhooks")]
fn webhook_workspace_connection(
    state: &HttpState,
    headers: &HeaderMap,
    params: &HashMap<String, String>,
) -> std::result::Result<rusqlite::Connection, Box<Response>> {
    let principal = admin_gate(state, headers)?;
    let record = state
        .registry
        .resolve(
            &principal.id,
            params.get("workspaceId").map(String::as_str),
            crate::workspace::WorkspaceAccess::Owner,
        )
        .map_err(|error| Box::new(workspace_failure(&error)))?;
    webhooks_actions::open_connection_at(&record.graph_path)
        .map_err(|error| Box::new(webhook_store_failure(error)))
}

/// Attach the webhook-subscription admin routes. Every handler is gated on
/// the `admin` scope like the principals routes; the routes themselves exist
/// only when the `webhooks` feature compiled them in.
#[cfg(feature = "webhooks")]
fn attach_webhook_admin_routes(router: Router<HttpState>) -> Router<HttpState> {
    router
        .route(
            "/ui/api/webhooks",
            get(admin_list_webhooks).post(admin_create_webhook),
        )
        .route(
            "/ui/api/webhooks/{id}",
            patch(admin_update_webhook).delete(admin_delete_webhook),
        )
        .route("/ui/api/webhooks/{id}/test", post(admin_test_webhook))
}

/// Attach the managed-repo admin routes. Every handler is gated on the
/// `admin` scope like the principals routes; the routes themselves exist
/// only when the `code` feature compiled them in.
#[cfg(feature = "code")]
fn attach_repo_admin_routes(router: Router<HttpState>) -> Router<HttpState> {
    router
        .route(
            "/ui/api/repos",
            get(admin_list_repos).post(admin_create_repo),
        )
        .route("/ui/api/repos/{key}/reindex", post(admin_reindex_repo))
        .route("/ui/api/repos/{key}", delete(admin_remove_repo))
}

/// `POST /ui/api/webhooks/{id}/test` — deliver one signed test event to the
/// subscription's endpoint and report the HTTP status. The delivery-time
/// policy runs in full: allowlist, DNS and the public-address check, then a
/// signature with the subscription's secret reference. No outbox row is
/// written and no delivery state changes, so a test never disturbs the
/// worker's queue or its dead-letter accounting.
#[cfg(feature = "webhooks")]
async fn admin_test_webhook(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let conn = match webhook_workspace_connection(&state, &headers, &params) {
        Ok(conn) => conn,
        Err(response) => return *response,
    };
    let id = match Uuid::parse_str(id.as_str()) {
        Ok(id) => id,
        // A malformed id names no row, the same answer the principals routes
        // give for an id their key format cannot parse.
        Err(_) => return not_found(),
    };
    let subscription = match SubscriptionRepository::new(&conn).get(id) {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(e) => return webhook_store_failure(e),
    };
    let Some(kit) = webhooks_actions::test_kit() else {
        return bad_request(
            "webhook test delivery is not configured: the server has no [webhooks] section",
        );
    };
    let started = std::time::Instant::now();
    // The deliverable makes a real DNS lookup and a real HTTPS request, so it
    // runs off the async runtime, the same way the worker's own blocking
    // transport does.
    let outcome = match tokio::task::spawn_blocking(move || kit.deliver(&subscription)).await {
        Ok(outcome) => outcome,
        Err(e) => return webhook_store_failure(format!("test delivery task failed: {e}")),
    };
    let latency_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    match outcome {
        Ok(response) => {
            let body = json!({
                "status": response.status,
                "ok": (200..300).contains(&response.status),
                "latencyUs": latency_us,
            });
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(WorkerError::Policy(message)) => {
            bad_request(format!("webhook test refused by policy: {message}"))
        }
        Err(WorkerError::Secret(message)) => {
            bad_request(format!("webhook test refused: {message}"))
        }
        Err(WorkerError::Delivery(message)) => json_error(
            StatusCode::BAD_GATEWAY,
            format!("webhook test delivery failed: {message}"),
        ),
        Err(WorkerError::Database(message)) => webhook_store_failure(message),
        Err(WorkerError::Core(message)) => webhook_store_failure(message),
    }
}

/// The caps the MCP add tool applies, so the admin API accepts nothing the
/// tool would refuse. `Some(message)` is the 400 body; `None` proceeds.
#[cfg(feature = "webhooks")]
fn webhook_caps_error(subscription: &WebhookSubscription) -> Option<String> {
    let max_origin = webhooks_actions::MAX_CONSUMER_ORIGIN_BYTES;
    if subscription.consumer_origin.len() > max_origin {
        return Some(format!("consumerOrigin too long (max {max_origin} bytes)"));
    }
    let max_items = webhooks_actions::MAX_LIST_ITEMS;
    for (field, count) in [
        ("eventOperations", subscription.event_operations.len()),
        ("entityTypes", subscription.entity_types.len()),
        ("ignoredOrigins", subscription.ignored_origins.len()),
    ] {
        if count > max_items {
            return Some(format!("Too many entries in '{field}' (max {max_items})"));
        }
    }
    None
}

/// `GET /ui/api/webhooks` — subscriptions in the selected workspace, oldest
/// first.
#[cfg(feature = "webhooks")]
async fn admin_list_webhooks(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let conn = match webhook_workspace_connection(&state, &headers, &params) {
        Ok(conn) => conn,
        Err(response) => return *response,
    };
    let subscriptions = match SubscriptionRepository::new(&conn).list() {
        Ok(rows) => rows,
        Err(e) => return webhook_store_failure(e),
    };
    (
        StatusCode::OK,
        Json(WebhookList {
            subscriptions,
            configured_secrets: webhooks_actions::configured_secret_refs(),
            delivery_role: webhooks_actions::delivery_role(),
        }),
    )
        .into_response()
}

/// `POST /ui/api/webhooks` — create one subscription. The rules are the MCP
/// tool's own, imported rather than copied: the endpoint URL shape, the
/// consumer-origin and list caps, and `WebhookSubscription::validate`
/// through the store's upsert. The id is server-generated.
#[cfg(feature = "webhooks")]
async fn admin_create_webhook(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    body: String,
) -> Response {
    let conn = match webhook_workspace_connection(&state, &headers, &params) {
        Ok(conn) => conn,
        Err(response) => return *response,
    };
    let input: WebhookInput = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("the body must be a JSON subscription"),
    };
    let subscription = WebhookSubscription {
        subscription_id: Uuid::new_v4(),
        endpoint: input.endpoint,
        event_operations: input.event_operations,
        entity_types: input.entity_types,
        ignored_origins: input.ignored_origins,
        consumer_origin: input.consumer_origin,
        secret_ref: input.secret_ref,
        enabled: input.enabled.unwrap_or(true),
    };
    match webhooks_actions::validate_endpoint_shape(&subscription.endpoint) {
        Ok(()) => {}
        Err(e) => {
            return bad_request(format!("invalid webhook endpoint: {e}"));
        }
    }
    // The registration-time secret check, the same one the MCP add tool
    // runs: an unknown `secretRef` is refused before the row is written.
    match webhooks_actions::validate_secret_ref(&subscription.secret_ref) {
        Ok(()) => {}
        Err(e) => {
            return bad_request(format!("invalid webhook subscription: {e}"));
        }
    }
    if let Some(message) = webhook_caps_error(&subscription) {
        return bad_request(message);
    }
    if let Err(e) = SubscriptionRepository::new(&conn).upsert(subscription.clone()) {
        // The checks above are not the only validator: upsert runs
        // `WebhookSubscription::validate` (non-empty fields, length and
        // control-character bounds), and its refusal is a bad request, not
        // a server fault.
        if matches!(e, crate::errors::MCSError::InvalidParams(_)) {
            return bad_request(format!("invalid webhook subscription: {e}"));
        }
        return webhook_store_failure(e);
    }
    (StatusCode::CREATED, Json(subscription)).into_response()
}

/// `PATCH /ui/api/webhooks/{id}` — change any subset of one subscription.
/// A named field replaces the stored value; everything else stays.
#[cfg(feature = "webhooks")]
async fn admin_update_webhook(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    body: String,
) -> Response {
    let conn = match webhook_workspace_connection(&state, &headers, &params) {
        Ok(conn) => conn,
        Err(response) => return *response,
    };
    let id = match Uuid::parse_str(id.as_str()) {
        Ok(id) => id,
        // A malformed id names no row, the same answer the principals routes
        // give for an id their key format cannot parse.
        Err(_) => return not_found(),
    };
    let patch: WebhookPatch = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("the body must be a JSON patch"),
    };
    // The patched value is validated, never the stored one: a patch that
    // names no `secretRef` leaves an existing reference alone, whatever the
    // current kit holds.
    if let Some(new_ref) = patch.secret_ref.as_deref() {
        match webhooks_actions::validate_secret_ref(new_ref) {
            Ok(()) => {}
            Err(e) => {
                return bad_request(format!("invalid webhook subscription: {e}"));
            }
        }
    }
    let repo = SubscriptionRepository::new(&conn);
    let row = match repo.get(id) {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(e) => return webhook_store_failure(e),
    };
    let merged = WebhookSubscription {
        subscription_id: id,
        endpoint: patch
            .endpoint
            .as_deref()
            .unwrap_or(&row.endpoint)
            .to_owned(),
        event_operations: patch.event_operations.unwrap_or(row.event_operations),
        entity_types: patch.entity_types.unwrap_or(row.entity_types),
        ignored_origins: patch.ignored_origins.unwrap_or(row.ignored_origins),
        consumer_origin: patch
            .consumer_origin
            .as_deref()
            .unwrap_or(&row.consumer_origin)
            .to_owned(),
        secret_ref: patch
            .secret_ref
            .as_deref()
            .unwrap_or(&row.secret_ref)
            .to_owned(),
        enabled: patch.enabled.unwrap_or(row.enabled),
    };
    match webhooks_actions::validate_endpoint_shape(&merged.endpoint) {
        Ok(()) => {}
        Err(e) => {
            return bad_request(format!("invalid webhook endpoint: {e}"));
        }
    }
    if let Some(message) = webhook_caps_error(&merged) {
        return bad_request(message);
    }
    if let Err(e) = repo.upsert(merged.clone()) {
        if matches!(e, crate::errors::MCSError::InvalidParams(_)) {
            return bad_request(format!("invalid webhook subscription: {e}"));
        }
        return webhook_store_failure(e);
    }
    (StatusCode::OK, Json(merged)).into_response()
}

/// `DELETE /ui/api/webhooks/{id}` — delete one subscription. Deleting an id
/// that names no row is a 404, the same answer the principals routes give;
/// the MCP tool's lenient delete is a separate contract.
#[cfg(feature = "webhooks")]
async fn admin_delete_webhook(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let conn = match webhook_workspace_connection(&state, &headers, &params) {
        Ok(conn) => conn,
        Err(response) => return *response,
    };
    let id = match Uuid::parse_str(id.as_str()) {
        Ok(id) => id,
        // A malformed id names no row, the same answer the principals routes
        // give for an id their key format cannot parse.
        Err(_) => return not_found(),
    };
    match SubscriptionRepository::new(&conn).delete(id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => webhook_store_failure(e),
    }
}

// ---------------------------------------------------------------------------
// Managed repositories (`/ui/api/repos`), behind the `code` feature
// ---------------------------------------------------------------------------

/// `GET /ui/api/repos` — every managed repository with its live state.
/// Mutations answer 202 and run the job on a detached thread; the next poll
/// of the list shows the transition.
#[cfg(feature = "code")]
async fn admin_list_repos(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    match crate::repos::list() {
        Ok(rows) => (StatusCode::OK, Json(serde_json::json!({ "repos": rows }))).into_response(),
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("repos store: {e}"),
        ),
    }
}

/// `POST /ui/api/repos` — register one repository and schedule its clone +
/// index job. The 202 answers before the job finishes; row state moves
/// `pending` → `cloning` → `indexing` → `indexed` (or `error`) and the list
/// shows it.
#[cfg(feature = "code")]
async fn admin_create_repo(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    let input: crate::repos::RepoInput = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            return bad_request(
                "the body must be a JSON repo: {key, url, authKind?, authSecret?, snippets?}",
            );
        }
    };
    if let Err(e) = crate::repos::register(&input) {
        return match e {
            crate::errors::MCSError::ConstraintViolation(message) => conflict(message),
            crate::errors::MCSError::InvalidParams(message) => bad_request(message),
            other => json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("repos store: {other}"),
            ),
        };
    }
    let key = input.key;
    if let Err(e) = crate::repos::add_job(&key) {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("repos store: {e}"),
        );
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "accepted", "key": key })),
    )
        .into_response()
}

/// `POST /ui/api/repos/{key}/reindex` — schedule a fetch + reindex job for
/// one repository. An unknown key is a 404; a job already in flight for the
/// key conflicts.
#[cfg(feature = "code")]
async fn admin_reindex_repo(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    match crate::repos::get_row(&key) {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("repos store: {e}"),
            );
        }
    }
    match crate::repos::reindex_job(&key) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "accepted", "key": key })),
        )
            .into_response(),
        Err(crate::errors::MCSError::InvalidParams(message)) => {
            json_error(StatusCode::CONFLICT, message)
        }
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("repos store: {e}"),
        ),
    }
}

/// `DELETE /ui/api/repos/{key}` — schedule the removal of one repository:
/// its index DB, its worktree and its row. An unknown key is a 404.
#[cfg(feature = "code")]
async fn admin_remove_repo(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Response {
    if let Err(response) = admin_gate(&state, &headers) {
        return *response;
    }
    match crate::repos::get_row(&key) {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("repos store: {e}"),
            );
        }
    }
    match crate::repos::remove_job(&key) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "accepted", "key": key })),
        )
            .into_response(),
        Err(crate::errors::MCSError::InvalidParams(message)) => {
            json_error(StatusCode::CONFLICT, message)
        }
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("repos store: {e}"),
        ),
    }
}
#[cfg(all(test, feature = "webhooks"))]
mod webhook_admin_tests {
    use super::*;
    use crate::http::router;
    use crate::principals::ADMIN_SCOPE;
    use crate::tools::ToolCategory;
    use axum::http::{Request, header};
    use http_body_util::BodyExt;
    use mcpmem_oauth::store::{Grant, TokenKind};
    use std::sync::Arc;
    use tower::ServiceExt;

    const NOW_US: i64 = 1_700_000_000_000_000;

    fn oauth_config() -> crate::config::OAuthConfig {
        crate::config::OAuthConfig {
            public_url: "https://mem.example.com".into(),
            oidc_issuer: "https://idp.invalid".into(),
            oidc_client_id: "mcpmem-test".into(),
            oidc_client_secret: None,
            principals: vec![crate::principals::PrincipalEntry {
                name: "admin".into(),
                iss: "https://idp.invalid".into(),
                sub: "admin@example.test".into(),
                label: None,
                scopes: vec![ADMIN_SCOPE.to_owned()],
            }],
            cimd_allowed_domains: Vec::new(),
            trust_forwarded_proto: false,
            approval_waitlist: false,
            approval_waitlist_ttl_seconds: 24 * 60 * 60,
            default_new_principal_scopes: vec!["graph-read".to_owned()],
        }
    }

    /// A kit holding exactly the named signing keys. The names double as
    /// the key bytes: a test key's content never matters, only its
    /// presence in the map does. The resolver and connector are never
    /// reached — these tests judge registration-time checks, not
    /// delivery.
    fn kit_with(secret_names: &[&str]) -> Arc<webhooks_actions::WebhookTestKit> {
        let secrets = secret_names
            .iter()
            .map(|name| {
                (
                    (*name).to_owned(),
                    mcpmem_webhook::SigningKey::new(name.as_bytes().to_vec())
                        .expect("a non-empty test key"),
                )
            })
            .collect();
        Arc::new(webhooks_actions::WebhookTestKit::for_test(
            std::collections::BTreeSet::new(),
            secrets,
            Arc::new(mcpmem_webhook::SystemResolver),
            Arc::new(mcpmem_webhook::HttpsConnector::production()),
            false,
            mcpmem_webhook::DEFAULT_MAX_BODY,
        ))
    }

    /// A state with OAuth on and an admin token. The admin owns a graph
    /// and has a saved default. Keep its database alive for each request.
    fn admin_state() -> (HttpState, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = HttpState::for_test(crate::http::TestSetup {
            db_path: dir.path().join("memory.db"),
            oauth: Some(oauth_config()),
            auth_token: None,
            metadata_fetch: None,
            bearer_scopes: Vec::new(),
            enabled_categories: ToolCategory::ALL.to_vec(),
            now_us: Some(Arc::new(|| NOW_US)),
            ui_enabled: true,
        });
        let principal_id = crate::principals::human_id("https://idp.invalid", "admin@example.test");
        state
            .registry
            .create(
                &principal_id,
                "admin-fixture",
                crate::workspace::Visibility::Private,
                |path| state.handles.initialize_graph(path),
            )
            .expect("the admin owns a workspace with a saved default");
        let oauth = state.oauth().expect("oauth is on");
        let token = "admin-test-token";
        oauth.with_store(|store| {
            store
                .put_token(
                    token,
                    TokenKind::Access,
                    &Grant {
                        client_id: "mcpmem-test".into(),
                        principal: crate::principals::human_id(
                            "https://idp.invalid",
                            "admin@example.test",
                        ),
                        scopes: vec![ADMIN_SCOPE.to_owned()],
                        resource: oauth.resource(),
                        family: "admin-test-family".into(),
                    },
                    NOW_US,
                    NOW_US + 3_600_000_000,
                )
                .expect("the admin token stores")
        });
        (state, token.to_owned(), dir)
    }

    /// Drive one admin-api request through the router and return the
    /// status plus the parsed JSON body.
    async fn api_request(
        state: &HttpState,
        method: &str,
        uri: &str,
        body: &str,
        token: &str,
    ) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(axum::body::Body::from(body.to_owned()))
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn the_list_names_the_configured_secrets_sorted() {
        let (state, token, _dir) = admin_state();
        let response =
            webhooks_actions::with_test_kit_async(Some(kit_with(&["stripe", "n8n"])), async {
                api_request(&state, "GET", "/ui/api/webhooks", "", &token).await
            })
            .await;
        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(
            response.1.get("configuredSecrets"),
            Some(&serde_json::json!(["n8n", "stripe"])),
            "the list must name the configured signing keys, sorted"
        );
    }

    /// A server with no kit answers an empty list: the store-now shape
    /// the SPA uses to offer a free-text reference.
    #[tokio::test]
    async fn the_list_answers_an_empty_secret_list_without_a_kit() {
        let (state, token, _dir) = admin_state();
        let response = webhooks_actions::with_test_kit_async(None, async {
            api_request(&state, "GET", "/ui/api/webhooks", "", &token).await
        })
        .await;
        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(
            response.1.get("configuredSecrets"),
            Some(&serde_json::json!([]))
        );
    }

    #[tokio::test]
    async fn create_refuses_an_unknown_secret_name() {
        let (state, token, _dir) = admin_state();
        let response = webhooks_actions::with_test_kit_async(
            Some(kit_with(&["n8n"])),
            async {
                api_request(
                    &state,
                    "POST",
                    "/ui/api/webhooks",
                    r#"{"endpoint":"https://hooks.example.test/receive","consumerOrigin":"https://example.test","secretRef":"stripe"}"#,
                    &token,
                )
                .await
            },
        )
        .await;
        assert_eq!(response.0, StatusCode::BAD_REQUEST);
        let message = response.1["error"]
            .as_str()
            .expect("a refusal carries a JSON error body");
        assert!(
            message.contains("secretRef 'stripe' is not configured"),
            "{message}"
        );
        assert!(message.contains("n8n"), "{message}");
    }

    #[tokio::test]
    async fn create_accepts_a_configured_secret_name() {
        let (state, token, _dir) = admin_state();
        let response = webhooks_actions::with_test_kit_async(
            Some(kit_with(&["n8n"])),
            async {
                api_request(
                    &state,
                    "POST",
                    "/ui/api/webhooks",
                    r#"{"endpoint":"https://hooks.example.test/receive","consumerOrigin":"https://example.test","secretRef":"n8n"}"#,
                    &token,
                )
                .await
            },
        )
        .await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(response.1["secretRef"], "n8n");
    }

    /// The patch validator reads the patched value, never the stored one:
    /// a new unknown name is refused, and a patch that names no secret is
    /// accepted whatever the current kit holds.
    #[tokio::test]
    async fn update_validates_only_a_new_secret_name() {
        let (state, token, _dir) = admin_state();
        let created = webhooks_actions::with_test_kit_async(
            Some(kit_with(&["n8n"])),
            async {
                api_request(
                    &state,
                    "POST",
                    "/ui/api/webhooks",
                    r#"{"endpoint":"https://hooks.example.test/receive","consumerOrigin":"https://example.test","secretRef":"n8n"}"#,
                    &token,
                )
                .await
            },
        )
        .await;
        assert_eq!(created.0, StatusCode::CREATED);
        let id = created.1["subscriptionId"]
            .as_str()
            .expect("the create echoes the id")
            .to_owned();

        let refused = webhooks_actions::with_test_kit_async(Some(kit_with(&["n8n"])), async {
            api_request(
                &state,
                "PATCH",
                &format!("/ui/api/webhooks/{id}"),
                r#"{"secretRef":"stripe"}"#,
                &token,
            )
            .await
        })
        .await;
        assert_eq!(
            refused.0,
            StatusCode::BAD_REQUEST,
            "a patch naming an unknown secret must be refused"
        );

        // The new kit does not hold the stored name: a patch that does
        // not name a secret must pass anyway, proving the stored value
        // is not re-checked.
        let kept = webhooks_actions::with_test_kit_async(Some(kit_with(&["github"])), async {
            api_request(
                &state,
                "PATCH",
                &format!("/ui/api/webhooks/{id}"),
                r#"{"consumerOrigin":"https://other.example.test"}"#,
                &token,
            )
            .await
        })
        .await;
        assert_eq!(kept.0, StatusCode::OK);
        assert_eq!(kept.1["secretRef"], "n8n");
    }
}
