//! The graph-viewer adapters under `/ui/api/*`: the workspace list, the
//! graph page, one node, one exact relation triple, and the expand
//! neighbourhood.
//!
//! Every route shares one gate: the caller must hold the `graph-read` scope
//! (the scope `read_graph` itself needs), and the workspace must be one the
//! caller can read. Unknown and denied workspaces map to the same 404,
//! through [`crate::http::workspace_failure`].

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use tracing::error;

use crate::authz::Principal;
use crate::errors::{MCSError, Result};
use crate::http::{
    HttpState, bad_request, insufficient_scope, json_error, not_found, principal_of_ui,
    unauthorized, workspace_failure,
};
use crate::kg::GraphHandle;
use crate::server::graph_read_enabled;
use crate::workspace::{WorkspaceAccess, WorkspaceError};

/// Upper bound on entities returned to the viewer in one `GET /ui/api/graph`
/// load, mirroring the `read_graph` search cap. Keeps the payload — and the
/// browser's force layout — bounded for very large graphs.
pub(crate) const MAX_UI_NODES: usize = 1000;

/// Upper bound on hops for a single `GET /ui/api/expand` traversal
/// (double-click to expand). One hop matches the Neo4j "expand
/// relationships" gesture; the cap bounds a single interaction's payload.
const MAX_UI_EXPAND_DEPTH: u32 = 3;

/// Register the viewer data routes.
pub fn attach(router: Router<HttpState>) -> Router<HttpState> {
    router
        .route("/ui/api/workspaces", get(ui_workspaces_handler))
        .route("/ui/api/graph", get(ui_graph_handler))
        .route("/ui/api/node", get(ui_node_handler))
        .route("/ui/api/expand", get(ui_expand_handler))
        .route("/ui/api/relation", get(ui_relation_handler))
}

/// Shared auth + scope gate for the viewer's workspace list and data routes.
/// Return the authenticated principal only after the category and the
/// `read_graph` scope checks pass.
///
/// The scope decision is [`crate::authz::allows_tool`]'s and is asked about
/// `read_graph` by name, never spelled here. `src/authz.rs` is the one place
/// that decision is made, and asking it about the tool the viewer stands for
/// is what keeps the two provably in step: a tool moved to another scope
/// moves the viewer with it.
///
/// The 401 is [`unauthorized`] and the scope refusal is
/// [`insufficient_scope`], the same two challenges `/mcp` sends: one server
/// answers with one shape, so a scripted viewer client can discover the
/// authorization server from the 401 and learn the scope to ask for from the
/// 403.
pub(crate) fn ui_data_gate(
    state: &HttpState,
    headers: &HeaderMap,
    params: &std::collections::HashMap<String, String>,
) -> std::result::Result<Principal, Box<Response>> {
    let Some(principal) = principal_of_ui(state, headers, params.get("token").map(String::as_str))
    else {
        return Err(Box::new(unauthorized(state)));
    };
    if !graph_read_enabled() {
        return Err(Box::new(
            (
                StatusCode::FORBIDDEN,
                "graph-read tools are disabled; start the server with --enable-graph-read (or --enable-all) to view the graph",
            )
                .into_response(),
        ));
    }
    if !crate::authz::allows_tool(&principal, "read_graph") {
        // The scope the challenge names is the one `authz` reports missing, so
        // the header cannot drift from the decision that produced it.
        let missing = crate::authz::missing_scope(&principal, "read_graph")
            .unwrap_or(crate::tools::ToolCategory::GraphRead.slug());
        return Err(Box::new(insufficient_scope(state, &[missing])));
    }
    Ok(principal)
}

/// A viewer data request resolves the same registry record and handle as MCP.
/// Resolve inside the payload task so registry I/O never blocks the reactor.
pub(crate) struct UiSelection {
    registry: Arc<crate::workspace::WorkspaceRegistry>,
    handles: Arc<crate::workspace::WorkspaceHandles>,
    principal_id: String,
    workspace_id: Option<String>,
}

impl UiSelection {
    pub(crate) fn new(
        state: HttpState,
        principal: Principal,
        params: &std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            registry: state.registry,
            handles: state.handles,
            principal_id: principal.id,
            workspace_id: params.get("workspaceId").cloned(),
        }
    }

    fn graph(self) -> std::result::Result<Arc<GraphHandle>, WorkspaceError> {
        let record = self.registry.resolve(
            &self.principal_id,
            self.workspace_id.as_deref(),
            WorkspaceAccess::Read,
        )?;
        Ok(self.handles.get(&record)?.kg)
    }
}

pub(crate) fn parse_usize(
    params: &std::collections::HashMap<String, String>,
    key: &str,
    default: usize,
) -> usize {
    params
        .get(key)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Run workspace selection and the graph payload in one blocking task.
pub(crate) async fn ui_json<F>(selection: UiSelection, what: &'static str, build: F) -> Response
where
    F: FnOnce(&GraphHandle) -> Result<String> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || {
        let kg = selection.graph()?;
        Ok::<_, WorkspaceError>(build(&kg))
    })
    .await
    {
        Ok(Ok(Ok(json))) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Ok(Err(error)) => workspace_failure(&error),
        Ok(Ok(Err(error))) => {
            error!("{what} error: {error}");
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
        Err(join_err) => {
            error!("{what} task panicked: {join_err}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// `GET /ui/api/workspaces` — the MCP workspace-list page for this caller.
async fn ui_workspaces_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let principal = match ui_data_gate(&state, &headers, &params) {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let limit = match params.get("limit") {
        None => 100,
        Some(raw) => match raw.parse::<usize>() {
            Ok(limit) if limit > 0 => limit,
            _ => return bad_request("'limit' must be a positive integer"),
        },
    };
    let cursor = params.get("cursor").cloned();
    let registry = state.registry;
    match tokio::task::spawn_blocking(move || {
        registry.list(&principal.id, cursor.as_deref(), limit)
    })
    .await
    {
        Ok(Ok(page)) => Json(page).into_response(),
        Ok(Err(error)) => workspace_failure(&error),
        Err(error) => {
            error!("/ui/api/workspaces task panicked: {error}");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// `GET /ui/api/graph` — a page of the whole graph for the viewer: entities,
/// the relations among them, the entity-type legend, overall stats, and a
/// pagination cursor. Requires the `graph-read` category and a readable
/// workspace.
/// Query params: `entityType` (filter), `offset`, `limit` (capped at
/// [`MAX_UI_NODES`]), `workspaceId` and `token` (auth fallback).
async fn ui_graph_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let principal = match ui_data_gate(&state, &headers, &params) {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let entity_type = params.get("entityType").filter(|s| !s.is_empty()).cloned();
    let offset = parse_usize(&params, "offset", 0);
    let limit = parse_usize(&params, "limit", 300).clamp(1, MAX_UI_NODES);
    let selection = UiSelection::new(state, principal, &params);
    ui_json(selection, "/ui/api/graph", move |kg| {
        build_graph_payload(kg, entity_type.as_deref(), offset, limit)
    })
    .await
}

/// `GET /ui/api/node` — one entity with its observation bodies, for the
/// inspector to lazy-load when a node is selected. The list endpoints
/// (`/ui/api/graph`, `/ui/api/search`) deliberately omit observation bodies
/// (they only carry `obsCount`) to keep those payloads small; this fetches
/// the bodies for the single node the user is looking at. Same auth +
/// `graph-read` gate. Query params: `name` (required), `workspaceId` and
/// `token` (auth fallback).
async fn ui_node_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let principal = match ui_data_gate(&state, &headers, &params) {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let Some(name) = params.get("name").filter(|s| !s.is_empty()).cloned() else {
        return (StatusCode::BAD_REQUEST, "missing 'name' parameter").into_response();
    };
    let selection = UiSelection::new(state, principal, &params);
    match tokio::task::spawn_blocking(move || {
        let kg = selection.graph()?;
        Ok::<_, WorkspaceError>(kg.get_entity(&name))
    })
    .await
    {
        Ok(Err(error)) => workspace_failure(&error),
        Ok(Ok(Ok(Some(entity)))) => match serde_json::to_string(&entity) {
            Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
            Err(e) => {
                error!("/ui/api/node serialize error: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        },
        Ok(Ok(Ok(None))) => (StatusCode::NOT_FOUND, "entity not found").into_response(),
        Ok(Ok(Err(e))) => {
            error!("/ui/api/node error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(join_err) => {
            error!("/ui/api/node task panicked: {join_err}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// `GET /ui/api/relation` — one exact relation triple: from, to, and
/// relationType. The triple addresses every relation read and write, so the
/// inspector fetches its observations and attributes exactly as the triple
/// names them. An unknown or deleted triple is a 404.
///
/// Query params: `from`, `to`, `relationType` (all required), `workspaceId`
/// and `token` (auth fallback).
async fn ui_relation_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let principal = match ui_data_gate(&state, &headers, &params) {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let Some(from) = params.get("from").filter(|s| !s.is_empty()).cloned() else {
        return bad_request("missing 'from' parameter");
    };
    let Some(to) = params.get("to").filter(|s| !s.is_empty()).cloned() else {
        return bad_request("missing 'to' parameter");
    };
    let Some(relation_type) = params
        .get("relationType")
        .filter(|s| !s.is_empty())
        .cloned()
    else {
        return bad_request("missing 'relationType' parameter");
    };
    let selection = UiSelection::new(state, principal, &params);
    match tokio::task::spawn_blocking(move || {
        let kg = selection.graph()?;
        Ok::<_, WorkspaceError>(kg.search_relations(
            Some(&from),
            Some(&to),
            Some(&relation_type),
            None,
            Some(1),
        ))
    })
    .await
    {
        Ok(Err(error)) => workspace_failure(&error),
        Ok(Ok(Ok(details))) => match details.first() {
            Some(detail) => match serde_json::to_string(&detail) {
                Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
                Err(e) => {
                    error!("/ui/api/relation serialize error: {e}");
                    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
                }
            },
            None => not_found(),
        },
        Ok(Ok(Err(e))) => {
            error!("/ui/api/relation error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(join_err) => {
            error!("/ui/api/relation task panicked: {join_err}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// The viewer's shared page metadata, spliced onto every list payload: the
/// entity-type legend, the graph-wide totals, and the pagination cursor that
/// drives Prev/Next without a second round-trip.
pub(crate) struct PageMeta<'a> {
    pub(crate) type_counts: &'a [(String, usize)],
    pub(crate) entities_total: usize,
    pub(crate) relations_total: usize,
    pub(crate) offset: usize,
    pub(crate) limit: usize,
    pub(crate) returned: usize,
    pub(crate) has_more: bool,
}

/// Splice [`PageMeta`] onto a `{entities,…,relations,…}` JSON object *in
/// place, without reparsing it*. `graph` is a complete object built by us, so
/// it ends in `}`; we pop that brace, append the extra keys, and re-close.
/// This replaces a full `serde_json::from_str` → mutate → `to_string`
/// round-trip over what is often the largest payload the server emits.
pub(crate) fn splice_meta(graph: &mut String, m: &PageMeta) {
    use std::fmt::Write as _;
    debug_assert!(graph.ends_with('}'));
    graph.pop();
    graph.push_str(",\"entityTypes\":[");
    for (i, (t, c)) in m.type_counts.iter().enumerate() {
        if i > 0 {
            graph.push(',');
        }
        graph.push_str("{\"type\":");
        crate::kg::push_json_str(graph, t);
        let _ = write!(graph, ",\"count\":{c}}}");
    }
    let _ = write!(
        graph,
        "],\"stats\":{{\"entities\":{e},\"relations\":{r}}},\
         \"page\":{{\"offset\":{o},\"limit\":{l},\"returned\":{ret},\"hasMore\":{hm}}}}}",
        e = m.entities_total,
        r = m.relations_total,
        o = m.offset,
        l = m.limit,
        ret = m.returned,
        hm = m.has_more,
    );
}

/// Assemble the `/ui/api/graph` JSON: an observation-free page of the graph
/// from [`GraphHandle::read_graph_filtered_lite`] plus the shared viewer
/// metadata (gathered in one reader acquisition via
/// [`GraphHandle::ui_meta`]). `hasMore` compares this page against the scope
/// total (the filtered type's count, or the whole-graph entity count) so the
/// viewer can enable Next.
fn build_graph_payload(
    kg: &GraphHandle,
    entity_type: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<String> {
    let (mut graph, returned) = kg.read_graph_filtered_lite(entity_type, offset, limit)?;
    let (type_counts, entities_total, relations_total) = kg.ui_meta();
    let scope_total = match entity_type {
        Some(t) if !t.is_empty() => type_counts
            .iter()
            .find(|(n, _)| n == t)
            .map_or(0, |(_, c)| *c),
        _ => entities_total,
    };
    let has_more = offset.saturating_add(returned) < scope_total;

    splice_meta(
        &mut graph,
        &PageMeta {
            type_counts: &type_counts,
            entities_total,
            relations_total,
            offset,
            limit,
            returned,
            has_more,
        },
    );
    Ok(graph)
}

/// `GET /ui/api/expand` — the neighbourhood of one entity, for the viewer's
/// double-click-to-expand traversal. Returns `{entities, relations}` (the
/// same shape as `/ui/api/graph`) from [`GraphHandle::neighbors`], which the
/// viewer merges into the current graph. Same auth + `graph-read` gate as
/// `/ui/api/graph`.
///
/// Query params: `name` (required, the entity to expand), `depth` (1..=
/// [`MAX_UI_EXPAND_DEPTH`], default 1), `direction` (`outgoing` /
/// `incoming` / `both`, default both), `workspaceId` and `token` (auth
/// fallback).
async fn ui_expand_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let principal = match ui_data_gate(&state, &headers, &params) {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let Some(name) = params.get("name").filter(|s| !s.is_empty()).cloned() else {
        return (StatusCode::BAD_REQUEST, "missing 'name' parameter").into_response();
    };
    let depth = params
        .get("depth")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1)
        .clamp(1, MAX_UI_EXPAND_DEPTH);
    // `Direction::parse` expects the uppercase MCP spelling; default is `Both`.
    let direction =
        crate::kg::Direction::parse(params.get("direction").map(|s| s.to_uppercase()).as_deref());

    let selection = UiSelection::new(state, principal, &params);
    let result = tokio::task::spawn_blocking(move || {
        let kg = selection.graph()?;
        Ok::<_, WorkspaceError>(kg.neighbors(&name, direction, None, depth))
    })
    .await;

    match result {
        Ok(Ok(Ok(json))) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Ok(Err(error)) => workspace_failure(&error),
        // An unknown entity is a client error (bad `name`), not a server fault.
        Ok(Ok(Err(MCSError::InvalidParams(msg)))) => (StatusCode::NOT_FOUND, msg).into_response(),
        Ok(Ok(Err(e))) => {
            error!("/ui/api/expand error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(join_err) => {
            error!("/ui/api/expand task panicked: {join_err}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}
