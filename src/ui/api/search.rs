//! The search adapter under `/ui/api/search`: a mode dispatcher for the
//! viewer's search box.
//!
//! `direct` answers from the FTS5 index of the selected scope: the entity
//! name index for `scope=nodes`, the relation-observation index for
//! `scope=relations`. `semantic` embeds the query with the serving profile's
//! model and ranks vector rows. `hybrid` embeds the same way and fuses the
//! FTS5 and vector rankings. The three modes share the `{results, count,
//! elapsedMs}` envelope and the `SearchHit` tagged union, and every relation
//! hit carries the structured triple resolved from the live graph.
//!
//! The adapter never calls its own `/mcp` endpoint; it drives the graph
//! handle and the shared vector path directly, exactly as MCP dispatch does.

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use serde::Serialize;
use serde_json::{Value, json};
use tracing::error;

use crate::authz::Principal;
use crate::errors::{MCSError, Result};
use crate::http::{HttpState, workspace_failure};
use crate::kg::GraphHandle;
use crate::ui::api::graph::{UiSelection, ui_data_gate, ui_json};
use crate::vector_store::VectorStore;
use crate::workspace::{WorkspaceAccess, WorkspaceError};

/// Register the search route.
pub fn attach(router: Router<HttpState>) -> Router<HttpState> {
    router.route("/ui/api/search", get(ui_search_handler))
}

/// The parsed search query shared by every mode. The strings are owned, so a
/// blocking payload task can capture the struct and outlive this request.
struct SearchParams {
    scope: String,
    entity_type: Option<String>,
    from: Option<String>,
    to: Option<String>,
    relation_type: Option<String>,
    k: usize,
}

/// One row of the `SearchHit` tagged union. The `kind` member discriminates
/// the row: `entity`, `relation`, or `attachment`. A member is present only
/// for the kind that carries it, so the browser's type switch sees one shape
/// per kind and never a star of nulls.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HitRow {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entity_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    relation_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vec_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attachment_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entity_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    excerpt: Option<String>,
}

/// The search envelope every mode returns.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchEnvelope {
    results: Vec<Value>,
    count: usize,
    elapsed_ms: u64,
}

/// A payload-task failure, kept distinct so the handler maps each kind to the
/// status the design pins: a workspace error to 404, a bad request to 400,
/// an unusable profile or provider to 503, and a contract break to 500.
enum UiSearchError {
    Workspace(WorkspaceError),
    Unavailable(String),
    #[cfg(feature = "indexer")]
    BadRequest(String),
    Malformed(String),
}

/// `GET /ui/api/search` — the viewer's search box.
///
/// Query params: `q` (must have a value), `mode` (`direct` default, `semantic`,
/// `hybrid`), `scope` (`nodes` default, `relations`), `type` (the entity
/// type in nodes scope, the relation type in relations scope),
/// `from`/`to`/`relationType` (relation filters), `k` (10, 20, or 50;
/// default 10), `workspaceId` and `token` (auth fallback).
async fn ui_search_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let principal = match ui_data_gate(&state, &headers, &params) {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let Some(query) = params
        .get("q")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return search_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "missing 'q' parameter",
        );
    };
    let scope = match params
        .get("scope")
        .map(|s| s.to_lowercase())
        .unwrap_or("nodes".to_string())
    {
        s if s == "nodes" || s == "relations" => s,
        other => {
            return search_error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("'scope' must be nodes or relations, not {other:?}"),
            );
        }
    };
    let mode = match params
        .get("mode")
        .map(|s| s.to_lowercase())
        .unwrap_or("direct".to_string())
    {
        s if s == "direct" || s == "semantic" || s == "hybrid" => s,
        other => {
            return search_error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("'mode' must be direct, semantic or hybrid, not {other:?}"),
            );
        }
    };
    let k = params
        .get("k")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10);
    if !matches!(k, 10 | 20 | 50) {
        return search_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "'k' must be 10, 20 or 50",
        );
    }
    let entity_type = params.get("type").filter(|s| !s.is_empty()).cloned();
    let from = params.get("from").filter(|s| !s.is_empty()).cloned();
    let to = params.get("to").filter(|s| !s.is_empty()).cloned();
    // The `type` parameter doubles as the relation-type filter in relations
    // scope, and `relationType` (the spec's own member) wins when both are
    // present. A relation scope reads only `relation_type`; a nodes scope
    // reads only `entity_type`.
    let relation_type = match params
        .get("relationType")
        .filter(|s| !s.is_empty())
        .cloned()
    {
        Some(relation_type) => Some(relation_type),
        None => entity_type.clone(),
    };

    match mode.as_str() {
        "direct" => {
            let selection = UiSelection::new(state, principal, &params);
            let p = SearchParams {
                scope,
                entity_type,
                from,
                to,
                relation_type,
                k,
            };
            ui_json(selection, "/ui/api/search", move |kg| {
                build_direct_payload(kg, &query, &p)
            })
            .await
        }
        _ => {
            // Semantic and hybrid need the `vectors` scope. Refuse before any
            // store or profile check, so the challenge names the scope and
            // never the server's configuration.
            if let Some(missing) = crate::authz::missing_scope(&principal, "semantic_search") {
                return insufficient_scope_response(missing);
            }
            let allow_attachments =
                crate::server::attachments_enabled() && principal.scopes.contains("attachments");
            let p = SearchParams {
                scope,
                entity_type,
                from,
                to,
                relation_type,
                k,
            };
            ui_vector_response(
                state,
                principal,
                params,
                query,
                p,
                mode == "hybrid",
                allow_attachments,
            )
            .await
        }
    }
}

/// One error body in the `{code,message}` contract the design doc pins.
fn search_error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "code": code, "message": message.into() })),
    )
        .into_response()
}

/// The 403 for a missing scope, with the UI's `{code,message}` body. The
/// challenge names the scope the caller must ask for, mirroring the mutation
/// adapter's response.
fn insufficient_scope_response(scope: &'static str) -> Response {
    let challenge = format!("Bearer error=\"insufficient_scope\", scope=\"{scope}\"");
    (
        StatusCode::FORBIDDEN,
        [(header::WWW_AUTHENTICATE, challenge)],
        Json(json!({
            "code": "insufficient_scope",
            "message": "insufficient scope",
        })),
    )
        .into_response()
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

/// Assemble the search envelope around `rows`.
fn build_envelope(rows: Vec<Value>, started: std::time::Instant) -> Result<String> {
    let count = rows.len();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    serde_json::to_string(&SearchEnvelope {
        results: rows,
        count,
        elapsed_ms,
    })
    .map_err(MCSError::JsonError)
}

/// One `SearchHit` row: `{kind:"entity",name,entityType}` for a node match,
/// `{kind:"relation",from,to,relationType}` for a relation match. FTS has no
/// comparable score, so a direct row carries none (`snippet` is a view
/// concern and stays off the wire until a real snippet exists).
fn fts_hit(
    kind: &str,
    name: Option<String>,
    entity_type: Option<String>,
    from: Option<String>,
    to: Option<String>,
    relation_type: Option<String>,
) -> Value {
    serde_json::to_value(&HitRow {
        kind: kind.to_string(),
        name,
        entity_type,
        from,
        to,
        relation_type,
        score: None,
        text_score: None,
        vec_score: None,
        attachment_id: None,
        entity_name: None,
        filename: None,
        page: None,
        excerpt: None,
    })
    .expect("an FTS hit always serializes")
}

/// The direct mode: one FTS pass over the selected scope, capped at `k`.
/// From, to and relationType narrow relation rows before the limit; `type`
/// narrows node rows. `elapsedMs` covers the payload work.
fn build_direct_payload(kg: &GraphHandle, query: &str, p: &SearchParams) -> Result<String> {
    let started = std::time::Instant::now();
    let fts = fts_query(query);
    let rows: Vec<Value> = match p.scope.as_str() {
        "relations" => {
            let mut rows: Vec<Value> = Vec::new();
            for detail in kg.search_relations(
                p.from.as_deref(),
                p.to.as_deref(),
                p.relation_type.as_deref(),
                Some(&fts),
                Some(p.k),
            )? {
                rows.push(fts_hit(
                    "relation",
                    None,
                    None,
                    Some(detail.from),
                    Some(detail.to),
                    Some(detail.relation_type),
                ));
            }
            rows
        }
        _ => {
            let mut rows: Vec<Value> = Vec::new();
            for entity in kg.search_nodes_filtered(&fts, p.entity_type.as_deref(), 0, p.k) {
                rows.push(fts_hit(
                    "entity",
                    Some(entity.name),
                    Some(entity.entity_type),
                    None,
                    None,
                    None,
                ));
            }
            rows
        }
    };
    build_envelope(rows, started)
}

/// Convert one vector row into a `SearchHit`. Rows whose kind does not fit
/// the requested scope are dropped (a relations search keeps only relation
/// rows; a nodes search keeps entity and attachment rows). The conversion
/// copies the structured members only — it never parses a formatted name.
#[cfg(feature = "indexer")]
fn convert_vector_row(row: &Value, scope: &str, fuse: bool) -> Option<HitRow> {
    let kind = row.get("kind").and_then(Value::as_str)?;
    let score = row.get("score").and_then(Value::as_f64);
    // Hybrid rows carry the two fused halves; semantic rows carry neither.
    let fused_score = |key: &str| -> Option<f64> {
        if fuse {
            row.get(key).and_then(Value::as_f64)
        } else {
            None
        }
    };
    match kind {
        "entity" if scope == "nodes" => {
            let name = row.get("name").and_then(Value::as_str)?;
            let entity_type = row.get("entityType").and_then(Value::as_str)?;
            Some(HitRow {
                kind: "entity".to_string(),
                name: Some(name.to_owned()),
                entity_type: Some(entity_type.to_owned()),
                from: None,
                to: None,
                relation_type: None,
                score,
                text_score: fused_score("textScore"),
                vec_score: fused_score("vecScore"),
                attachment_id: None,
                entity_name: None,
                filename: None,
                page: None,
                excerpt: None,
            })
        }
        "relation" if scope == "relations" => {
            let from = row.get("from").and_then(Value::as_str)?;
            let to = row.get("to").and_then(Value::as_str)?;
            let relation_type = row.get("relationType").and_then(Value::as_str)?;
            Some(HitRow {
                kind: "relation".to_string(),
                name: None,
                entity_type: None,
                from: Some(from.to_owned()),
                to: Some(to.to_owned()),
                relation_type: Some(relation_type.to_owned()),
                score,
                text_score: fused_score("textScore"),
                vec_score: fused_score("vecScore"),
                attachment_id: None,
                entity_name: None,
                filename: None,
                page: None,
                excerpt: None,
            })
        }
        "attachment" if scope == "nodes" => {
            let filename = row.get("filename").and_then(Value::as_str)?;
            let entity_name = row.get("entityName").and_then(Value::as_str)?;
            let excerpt = row.get("excerpt").and_then(Value::as_str)?;
            Some(HitRow {
                kind: "attachment".to_string(),
                name: None,
                entity_type: None,
                from: None,
                to: None,
                relation_type: None,
                score,
                text_score: fused_score("textScore"),
                vec_score: fused_score("vecScore"),
                attachment_id: row
                    .get("attachmentId")
                    .and_then(Value::as_u64)
                    .map(|v| v as i64),
                entity_name: Some(entity_name.to_owned()),
                filename: Some(filename.to_owned()),
                page: row.get("page").and_then(Value::as_u64).map(|v| v as i64),
                excerpt: Some(excerpt.to_owned()),
            })
        }
        _ => None,
    }
}

/// Run the semantic and hybrid modes in one blocking task: resolve the
/// selected workspace, then map every failure to its status. The vectors
/// scope gate has already passed in the handler.
async fn ui_vector_response(
    state: HttpState,
    principal: Principal,
    params: std::collections::HashMap<String, String>,
    query: String,
    p: SearchParams,
    fuse: bool,
    allow_attachments: bool,
) -> Response {
    match tokio::task::spawn_blocking(move || {
        build_vector_payload(
            &state,
            &principal,
            &params,
            &query,
            &p,
            fuse,
            allow_attachments,
        )
    })
    .await
    {
        Ok(Ok(json)) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Ok(Err(UiSearchError::Workspace(error))) => workspace_failure(&error),
        Ok(Err(UiSearchError::Unavailable(message))) => {
            search_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
        }
        #[cfg(feature = "indexer")]
        Ok(Err(UiSearchError::BadRequest(message))) => {
            search_error(StatusCode::BAD_REQUEST, "bad_request", message)
        }
        Ok(Err(UiSearchError::Malformed(message))) => {
            error!("/ui/api/search vector payload: {message}");
            search_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal error",
            )
        }
        Err(join_err) => {
            error!("/ui/api/search task panicked: {join_err}");
            search_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal error",
            )
        }
    }
}

/// Resolve the workspace entry, check the profile, then embed and rank. The
/// profile and provider checks are the 503 seam: a mode that cannot embed
/// must never answer an empty result that pretends it worked.
fn build_vector_payload(
    state: &HttpState,
    principal: &Principal,
    params: &std::collections::HashMap<String, String>,
    query: &str,
    p: &SearchParams,
    fuse: bool,
    allow_attachments: bool,
) -> std::result::Result<String, UiSearchError> {
    let started = std::time::Instant::now();
    let record = state
        .registry
        .resolve(
            &principal.id,
            params.get("workspaceId").map(String::as_str),
            WorkspaceAccess::Read,
        )
        .map_err(UiSearchError::Workspace)?;
    let entry = state
        .handles
        .get(&record)
        .map_err(UiSearchError::Workspace)?;
    let Some(vs) = entry.vs.as_deref() else {
        return Err(UiSearchError::Unavailable(
            "vector search is unavailable because the vector subsystem is off; \
             start the server with --enable-vectors"
                .into(),
        ));
    };
    match vs.serving_profile() {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(UiSearchError::Unavailable(
                "the store serves no index profile".into(),
            ));
        }
        Err(error) => return Err(UiSearchError::Unavailable(error.to_string())),
    }
    let rows = embed_and_search(vs, &entry.kg, query, p, fuse, allow_attachments)?;
    let body =
        build_envelope(rows, started).map_err(|e| UiSearchError::Malformed(e.to_string()))?;
    Ok(body)
}

/// Embed the query with the serving profile's model, run the shared vector
/// path, and convert its rows. The conversion copies the vector rows'
/// structured members only — never a formatted name.
#[cfg(feature = "indexer")]
fn embed_and_search(
    vs: &VectorStore,
    kg: &GraphHandle,
    query: &str,
    p: &SearchParams,
    fuse: bool,
    allow_attachments: bool,
) -> std::result::Result<Vec<Value>, UiSearchError> {
    if crate::indexer_provider::get().is_none() {
        return Err(UiSearchError::Unavailable(
            "no embedding provider is configured".into(),
        ));
    }
    let mut filter = serde_json::json!({});
    if p.scope == "relations" {
        // `kind` enters the candidate search itself. A nodes search leaves it
        // absent, so consented file hits can rank beside entity hits; the row
        // conversion then drops any relation row from a nodes response.
        filter["kind"] = Value::String("relation".into());
    }
    if let Some(t) = p.entity_type.as_deref() {
        filter["type"] = Value::String(t.into());
    }
    if let Some(f) = p.from.as_deref() {
        filter["from"] = Value::String(f.into());
    }
    if let Some(t) = p.to.as_deref() {
        filter["to"] = Value::String(t.into());
    }
    if let Some(rt) = p.relation_type.as_deref() {
        filter["relationType"] = Value::String(rt.into());
    }
    // Either weight turns on the FTS5 fusion, exactly as the MCP semantic
    // path reads the same two members.
    let args = if fuse {
        serde_json::json!({
            "queryText": query.to_owned(),
            "topK": p.k,
            "filter": filter,
            "textWeight": 0.5,
            "vecWeight": 0.5,
        })
    } else {
        serde_json::json!({
            "queryText": query.to_owned(),
            "topK": p.k,
            "filter": filter,
        })
    };

    let content =
        match crate::vector_actions::handle_semantic_search(vs, kg, Some(&args), allow_attachments)
        {
            Ok(inner) => inner,
            Err(MCSError::InvalidParams(message)) => {
                return Err(UiSearchError::BadRequest(message));
            }
            Err(error) => return Err(UiSearchError::Unavailable(error.to_string())),
        };
    let outer: Value = serde_json::from_str(&content).map_err(|e| {
        UiSearchError::Malformed(format!("the vector response does not parse: {e}"))
    })?;
    let Some(text) = outer["content"][0]["text"].as_str() else {
        return Err(UiSearchError::Malformed(
            "the vector response has no text member".into(),
        ));
    };
    let rows_value: Value = serde_json::from_str::<Value>(text)
        .map_err(|e| UiSearchError::Malformed(format!("the vector rows do not parse: {e}")))?;
    let rows = rows_value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| UiSearchError::Malformed("the vector rows have no results array".into()))?;
    let mut hits: Vec<Value> = Vec::new();
    for row in rows {
        if let Some(hit) = convert_vector_row(row, p.scope.as_str(), fuse) {
            hits.push(
                serde_json::to_value(&hit).map_err(|e| {
                    UiSearchError::Malformed(format!("a hit does not serialize: {e}"))
                })?,
            );
        }
    }
    Ok(hits)
}

/// A build without the indexer feature cannot embed a query, so both modes
/// answer the unavailable status. The direct mode never reaches this
/// function.
#[cfg(not(feature = "indexer"))]
fn embed_and_search(
    _vs: &VectorStore,
    _kg: &GraphHandle,
    _query: &str,
    _p: &SearchParams,
    _fuse: bool,
    _allow_attachments: bool,
) -> std::result::Result<Vec<Value>, UiSearchError> {
    Err(UiSearchError::Unavailable(
        "semantic search is unavailable because this build cannot embed a query; \
         rebuild with --features indexer"
            .into(),
    ))
}
