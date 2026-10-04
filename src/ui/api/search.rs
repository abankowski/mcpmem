//! The search adapter under `/ui/api/search`: a page of FTS5 matches for the
//! viewer's search box, in the same `{entities, relations, entityTypes,
//! stats, page}` shape as `/ui/api/graph` (the matched nodes; the user
//! double-clicks to expand their relationships). Same auth + `graph-read`
//! gate as the graph routes.

use axum::Router;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;

use crate::errors::Result;
use crate::http::HttpState;
use crate::kg::GraphHandle;
use crate::ui::api::graph::{
    MAX_UI_NODES, PageMeta, UiSelection, parse_usize, splice_meta, ui_data_gate, ui_json,
};

/// Register the search route.
pub fn attach(router: Router<HttpState>) -> Router<HttpState> {
    router.route("/ui/api/search", get(ui_search_handler))
}

/// `GET /ui/api/search` — a page of FTS5 matches for the viewer's search box.
/// Query params: `q` (the query; prefix-matched), `entityType` (filter),
/// `offset`, `limit` (capped at [`MAX_UI_NODES`]), `workspaceId` and `token`.
async fn ui_search_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let principal = match ui_data_gate(&state, &headers, &params) {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let query = params
        .get("q")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let entity_type = params.get("entityType").filter(|s| !s.is_empty()).cloned();
    let offset = parse_usize(&params, "offset", 0);
    let limit = parse_usize(&params, "limit", 100).clamp(1, MAX_UI_NODES);
    let selection = UiSelection::new(state, principal, &params);
    ui_json(selection, "/ui/api/search", move |kg| {
        build_search_payload(kg, &query, entity_type.as_deref(), offset, limit)
    })
    .await
}

/// Turn a free-text search box query into a safe FTS5 MATCH expression: keep
/// alphanumeric/underscore tokens (dropping punctuation that would otherwise
/// be FTS operators and silently fail the query), AND them together, and make
/// the final token a prefix (`term*`) for a natural search-as-you-type feel.
fn fts_query(raw: &str) -> String {
    let tokens: Vec<String> = raw
        .split_whitespace()
        .map(|t| {
            t.chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
        })
        .filter(|t| !t.is_empty())
        .collect();
    let n = tokens.len();
    tokens
        .into_iter()
        .enumerate()
        .map(|(i, t)| if i + 1 == n { format!("{t}*") } else { t })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Assemble the `/ui/api/search` JSON: an observation-free page of FTS5
/// matches from [`GraphHandle::search_nodes_lite_json`] as `{entities,
/// relations: []}` (the matched nodes; the user double-clicks to expand
/// their relationships) plus the shared viewer metadata. `hasMore` is
/// detected server-side by fetching one extra match past the page.
fn build_search_payload(
    kg: &GraphHandle,
    query: &str,
    entity_type: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<String> {
    let fts = fts_query(query);
    let (entities_json, returned, has_more) =
        kg.search_nodes_lite_json(&fts, entity_type, offset, limit);

    let mut graph = String::with_capacity(entities_json.len() + 48);
    graph.push_str("{\"entities\":");
    graph.push_str(&entities_json);
    graph.push_str(",\"relations\":[]}");

    let (type_counts, entities_total, relations_total) = kg.ui_meta();
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
