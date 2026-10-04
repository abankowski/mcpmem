use serde_json::{Value, json};

use crate::errors::{MCSError, Result};
use crate::kg::{GraphHandle, push_json_str};
use crate::vector_store::{RelationFilter, RelationOwner, VectorStore, with_scratch};
use mcpmem_core::jobs::OwnerKind;
use rustc_hash::FxHashMap;

/// One fused result row: the row subject (`name` for an entity or
/// attachment, the resolved triple for a relation), the three scores, and
/// the best vector chunk when the caller asked for chunk detail.
struct FusedRow {
    name: Option<String>,
    relation: Option<RelationOwner>,
    entity_type: String,
    kind: String,
    score: f64,
    text_score: f64,
    vec_score: f64,
    chunk: Option<ChunkDetail>,
    attachment: Option<AttachmentDetail>,
}

/// The matched chunk of one result row for `includeChunks`: its kind, its
/// reassembled text, and its distance.
struct ChunkDetail {
    kind: String,
    text: String,
    score: f64,
}

/// An attachment hit uses the persisted page and the exact matched segment,
/// plus the file id and parent entity name that identify the match: `name`
/// is only the filename, which is unique within one entity alone, and
/// `get_attachment` addresses the file by id.
struct AttachmentDetail {
    attachment_id: i64,
    parent_name: String,
    page: i64,
    excerpt: String,
    chunk_score: f64,
}

const MAX_EMBEDDING_DIMS: usize = 4096;
const MAX_TOP_K: usize = 100;
const DEFAULT_TOP_K: usize = 10;
const MAX_NAME_BYTES: usize = 1024;

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(MCSError::InvalidParams("Name must not be empty".into()));
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(MCSError::InvalidParams(format!(
            "Name too long (max {MAX_NAME_BYTES} bytes)"
        )));
    }
    Ok(())
}

fn parse_embedding(val: &Value) -> Result<Vec<f64>> {
    let arr = val
        .as_array()
        .ok_or_else(|| MCSError::InvalidParams("'embedding' must be an array of numbers".into()))?;
    if arr.is_empty() {
        return Err(MCSError::InvalidParams(
            "Embedding must not be empty".into(),
        ));
    }
    if arr.len() > MAX_EMBEDDING_DIMS {
        return Err(MCSError::InvalidParams(format!(
            "Embedding too large (max {MAX_EMBEDDING_DIMS} dimensions)"
        )));
    }
    let emb: Vec<f64> = arr
        .iter()
        .map(|v| {
            v.as_f64()
                .ok_or_else(|| MCSError::InvalidParams("Embedding values must be numbers".into()))
        })
        .collect::<Result<_>>()?;
    Ok(emb)
}

fn opt_usize(params: &Value, key: &str, default: usize) -> Result<usize> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_u64().map(|n| n as usize).ok_or_else(|| {
            MCSError::InvalidParams(format!("'{key}' must be a non-negative integer"))
        }),
    }
}

fn opt_f64(params: &Value, key: &str, default: f64) -> Result<f64> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_f64()
            .ok_or_else(|| MCSError::InvalidParams(format!("'{key}' must be a number"))),
    }
}

/// Owner-level filter shared by the five search tools. `kind` limits the
/// owner kind ("entity", "relation", or "attachment"); `type` matches the
/// stored parent entity type for an attachment. `from`, `to` and
/// `relation_type` match the live triple of a relation row, and apply
/// before ranking and before the candidate pool truncation. Entity and
/// attachment rows ignore the triple members.
#[derive(Clone, Debug, Default)]
pub struct SearchFilter {
    pub kind: Option<String>,
    pub r#type: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub relation_type: Option<String>,
}

/// One filter member: `None` when absent or null or an empty string, an
/// `InvalidParams` error when the value is present but not a string — a
/// number or object silently read as "no filter" would return a wrong result
/// as if it were correct.
fn filter_member<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s)),
        Some(Value::String(_)) => Ok(None),
        Some(_) => Err(MCSError::InvalidParams(format!(
            "'filter.{key}' must be a string"
        ))),
    }
}

/// Parse the `filter` argument. Only the three stored owner kinds are valid.
fn parse_filter(params: &Value) -> Result<Option<SearchFilter>> {
    let Some(f) = params.get("filter").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let object = f
        .as_object()
        .ok_or_else(|| MCSError::InvalidParams("'filter' must be an object".into()))?;
    let kind = filter_member(object, "kind")?;
    if kind.is_some_and(|k| k != "entity" && k != "relation" && k != "attachment") {
        return Err(MCSError::InvalidParams(
            "'filter.kind' must be \"entity\", \"relation\", or \"attachment\"".into(),
        ));
    }
    let ftype = filter_member(object, "type")?;
    let from = filter_member(object, "from")?;
    let to = filter_member(object, "to")?;
    let relation_type = filter_member(object, "relationType")?;
    Ok(Some(SearchFilter {
        kind: kind.map(str::to_string),
        r#type: ftype.map(str::to_string),
        from: from.map(str::to_string),
        to: to.map(str::to_string),
        relation_type: relation_type.map(str::to_string),
    }))
}

fn include_attachments(params: &Value, allow_attachments: bool) -> bool {
    allow_attachments
        && params
            .get("includeAttachments")
            .and_then(Value::as_bool)
            .unwrap_or(true)
}

fn text_content(text: &str) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": text
        }]
    })
}

fn build_content_response(inner_json: &str) -> String {
    let mut out = String::with_capacity(64 + inner_json.len() + (inner_json.len() / 8));
    out.push_str(r#"{"content":[{"type":"text","text":"#);
    push_json_str(&mut out, inner_json);
    out.push_str(r#"}]}"#);
    out
}

/// One owner-level result row of the shared search. A relation row carries
/// the resolved triple in `relation`, and `name` stays `None` for it.
struct OwnerRow {
    name: Option<String>,
    relation: Option<RelationOwner>,
    entity_type: String,
    kind: String,
    score: f64,
    chunk: Option<ChunkDetail>,
    attachment: Option<AttachmentDetail>,
}

/// Search owners by their best chunk. Exclude a reference entity without
/// letting it consume a result slot.
// The callers pass the parsed filter and both inclusion flags from one
// argument object; a struct would only move the same arity to the callers.
#[allow(clippy::too_many_arguments)]
fn search_owners(
    vs: &VectorStore,
    query: &[f32],
    top_k: usize,
    exclude: Option<(OwnerKind, i64)>,
    filter: Option<&SearchFilter>,
    include_chunks: bool,
    allow_attachments: bool,
) -> Result<String> {
    Ok(build_owner_results(
        &search_owner_rows(
            vs,
            query,
            top_k,
            exclude,
            filter,
            include_chunks,
            allow_attachments,
        )?,
        include_chunks,
    ))
}

// Same arity as [`search_owners`]; a struct would move it to the callers.
#[allow(clippy::too_many_arguments)]
fn search_owner_rows(
    vs: &VectorStore,
    query: &[f32],
    top_k: usize,
    exclude: Option<(OwnerKind, i64)>,
    filter: Option<&SearchFilter>,
    include_chunks: bool,
    allow_attachments: bool,
) -> Result<Vec<OwnerRow>> {
    let kind = filter.and_then(|f| f.kind.as_deref());
    let ftype = filter.and_then(|f| f.r#type.as_deref());
    let relation_filter = filter.map(|f| RelationFilter {
        from: f.from.as_deref(),
        to: f.to.as_deref(),
        relation_type: f.relation_type.as_deref(),
    });
    let mut rows = Vec::with_capacity(top_k);

    // Rank distinct owners first. A file with many pages cannot crowd out
    // graph owners. Resolve live rows before the final topK limit.
    let hits = vs.search_best_chunks(
        query,
        1000,
        kind,
        ftype,
        relation_filter.as_ref(),
        allow_attachments,
    )?;
    for hit in &hits {
        if exclude == Some((hit.owner_kind, hit.owner_id)) {
            continue;
        }
        let Some(resolved) = vs.resolve_owner(hit.owner_kind, hit.owner_id)?
        else {
            continue;
        };
        if ftype.is_some_and(|want| resolved.entity_type != want) {
            continue;
        }
        let attachment = if hit.owner_kind == OwnerKind::Attachment {
            let Some((id, _filename, parent, _etype)) = vs.attachment_owner(hit)? else {
                continue;
            };
            let Some((page, excerpt)) = vs.attachment_segment(hit) else {
                continue;
            };
            Some(AttachmentDetail {
                attachment_id: id,
                parent_name: parent,
                page,
                excerpt,
                chunk_score: f64::from(hit.dist),
            })
        } else {
            None
        };
        let chunk = if include_chunks && attachment.is_none() {
            vs.chunk_text(hit).map(|text| ChunkDetail {
                kind: hit.chunk_kind.as_str().to_string(),
                text,
                score: f64::from(hit.dist),
            })
        } else {
            None
        };
        rows.push(OwnerRow {
            name: resolved.name,
            relation: resolved.relation,
            entity_type: resolved.entity_type,
            kind: resolved.kind,
            score: f64::from(hit.dist),
            chunk,
            attachment,
        });
        if rows.len() == top_k {
            break;
        }
    }
    Ok(rows)
}

/// Append one `"chunk":{...}` member to a result row.
fn write_chunk_detail(out: &mut String, kind: &str, text: &str, score: f64) {
    use std::fmt::Write;
    out.push_str(r#","chunk":{"kind":"#);
    push_json_str(out, kind);
    out.push_str(r#","text":"#);
    push_json_str(out, text);
    write!(out, r#","score":{score:.6}}}"#).unwrap();
}

fn write_attachment_detail(out: &mut String, filename: &str, detail: &AttachmentDetail) {
    use std::fmt::Write;
    out.push_str(r#","filename":"#);
    push_json_str(out, filename);
    write!(
        out,
        r#","attachmentId":{},"entityName":"#,
        detail.attachment_id
    )
    .unwrap();
    push_json_str(out, &detail.parent_name);
    write!(out, r#","page":{},"excerpt":"#, detail.page).unwrap();
    push_json_str(out, &detail.excerpt);
}

/// Write the subject of one result row: `name` for an entity or attachment
/// row, the structured triple for a relation row.
fn write_row_subject(out: &mut String, name: Option<&str>, relation: Option<&RelationOwner>) {
    if let Some(rel) = relation {
        out.push_str(r#""from":"#);
        push_json_str(out, &rel.from);
        out.push_str(r#","to":"#);
        push_json_str(out, &rel.to);
        out.push_str(r#","relationType":"#);
        push_json_str(out, &rel.relation_type);
    } else {
        out.push_str(r#""name":"#);
        push_json_str(out, name.expect("a named row must carry a name"));
    }
}

/// Render owner rows with file metadata, and optional matched chunk detail.
fn build_owner_results(rows: &[OwnerRow], include_chunks: bool) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(128 + rows.len() * 96);
    out.push_str(r#"{"results":["#);
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        write_row_subject(&mut out, row.name.as_deref(), row.relation.as_ref());
        out.push_str(r#","entityType":"#);
        push_json_str(&mut out, &row.entity_type);
        out.push_str(r#","kind":"#);
        push_json_str(&mut out, &row.kind);
        write!(out, r#","score":{:.6}"#, row.score).unwrap();
        if let Some(detail) = &row.attachment {
            write_attachment_detail(
                &mut out,
                row.name.as_deref().expect("an attachment row has a filename"),
                detail,
            );
            if include_chunks {
                write_chunk_detail(&mut out, "attachment", &detail.excerpt, row.score);
            }
        } else if let Some(chunk) = &row.chunk {
            write_chunk_detail(&mut out, &chunk.kind, &chunk.text, chunk.score);
        }
        out.push('}');
    }
    out.push_str(r#"],"count":"#);
    out.push_str(&rows.len().to_string());
    out.push('}');
    out
}

/// The `(kind, id)` fusion key. `OwnerKind` implements no `Hash`, so the key
/// is the owner kind's discriminant.
const fn owner_key(kind: OwnerKind) -> u8 {
    match kind {
        OwnerKind::Entity => 0,
        OwnerKind::Relation => 1,
        OwnerKind::Attachment => 2,
    }
}

pub fn handle_vector_search_entities(
    vs: &VectorStore,
    _kg: &GraphHandle,
    args: Option<&Value>,
    allow_attachments: bool,
) -> Result<String> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;

    let embedding = parse_embedding(
        params
            .get("embedding")
            .ok_or_else(|| MCSError::InvalidParams("Missing 'embedding' parameter".into()))?,
    )?;

    let top_k = opt_usize(params, "topK", DEFAULT_TOP_K)?.clamp(1, MAX_TOP_K);
    let filter = parse_filter(params)?;
    let include_chunks = params
        .get("includeChunks")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let json = with_scratch(|buf| {
        buf.reserve(embedding.len());
        buf.extend(embedding.iter().map(|&v| v as f32));
        search_owners(
            vs,
            buf,
            top_k,
            None,
            filter.as_ref(),
            include_chunks,
            include_attachments(params, allow_attachments),
        )
    })?;

    Ok(build_content_response(&json))
}

pub fn handle_hybrid_search(
    vs: &VectorStore,
    kg: &GraphHandle,
    args: Option<&Value>,
    allow_attachments: bool,
) -> Result<String> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;

    let query_text = params
        .get("queryText")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'queryText' parameter".into()))?;

    let query_embedding =
        parse_embedding(params.get("queryEmbedding").ok_or_else(|| {
            MCSError::InvalidParams("Missing 'queryEmbedding' parameter".into())
        })?)?;

    let text_weight = opt_f64(params, "textWeight", 0.5)?;
    let vec_weight = opt_f64(params, "vecWeight", 0.5)?;
    let top_k = opt_usize(params, "topK", DEFAULT_TOP_K)?.clamp(1, MAX_TOP_K);
    let filter = parse_filter(params)?;
    let include_chunks = params
        .get("includeChunks")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let results = with_scratch(|buf| {
        buf.reserve(query_embedding.len());
        buf.extend(query_embedding.iter().map(|&v| v as f32));
        let params = HybridParams {
            text_weight,
            vec_weight,
            top_k,
            filter: filter.as_ref(),
            include_chunks,
            allow_attachments: include_attachments(params, allow_attachments),
        };
        perform_hybrid_search(vs, kg, query_text, buf, &params)
    })?;

    Ok(build_content_response(&build_fused_results(
        &results,
        include_chunks,
    )))
}

/// Render fused rows as the standard results JSON. `hybrid_search` and
/// `semantic_search` fuse the same two rankings, so a client parses one shape
/// for both. Rows are kind-marked, and `includeChunks` attaches the best
/// vector chunk of the owner.
fn build_fused_results(results: &[FusedRow], include_chunks: bool) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(128 + results.len() * 96);
    out.push_str(r#"{"results":["#);
    for (i, row) in results.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        write_row_subject(&mut out, row.name.as_deref(), row.relation.as_ref());
        out.push_str(r#","entityType":"#);
        push_json_str(&mut out, &row.entity_type);
        out.push_str(r#","kind":"#);
        push_json_str(&mut out, &row.kind);
        write!(
            out,
            r#","score":{:.6},"textScore":{:.6},"vecScore":{:.6}"#,
            row.score, row.text_score, row.vec_score
        )
        .unwrap();
        if let Some(detail) = &row.attachment {
            write_attachment_detail(
                &mut out,
                row.name.as_deref().expect("an attachment row has a filename"),
                detail,
            );
            if include_chunks {
                write_chunk_detail(&mut out, "attachment", &detail.excerpt, detail.chunk_score);
            }
        } else if let Some(chunk) = &row.chunk {
            write_chunk_detail(&mut out, &chunk.kind, &chunk.text, chunk.score);
        }
        out.push('}');
    }
    out.push_str(r#"],"count":"#);
    out.push_str(&results.len().to_string());
    out.push('}');
    out
}

fn perform_hybrid_search(
    vs: &VectorStore,
    kg: &GraphHandle,
    query_text: &str,
    query_emb: &[f32],
    params: &HybridParams<'_>,
) -> Result<Vec<FusedRow>> {
    let top_k = params.top_k;
    let fetch_k = top_k.saturating_mul(3).clamp(1, 100);
    let rrf_constant = 60.0;
    let kind = params.filter.and_then(|f| f.kind.as_deref());
    let ftype = params.filter.and_then(|f| f.r#type.as_deref());
    let relation_filter = params.filter.map(|f| RelationFilter {
        from: f.from.as_deref(),
        to: f.to.as_deref(),
        relation_type: f.relation_type.as_deref(),
    });

    // Rank one best segment per owner before the candidate limit.
    let hits = vs.search_best_chunks(
        query_emb,
        fetch_k,
        kind,
        ftype,
        relation_filter.as_ref(),
        params.allow_attachments,
    )?;
    let vec_owners = vs.aggregate_owners(&hits, fetch_k);

    // FTS matches entities only. Other owner kinds enter by vector rank.
    // A type filter applies to the entity type in both candidate feeds.
    let mut text_matches: Vec<(u8, i64)> = Vec::new();
    if kind.is_none_or(|value| value == "entity") {
        let kg_results = kg.search_nodes_filtered(query_text, None, 0, fetch_k);
        for entity in &kg_results {
            let Some(id) = vs.entity_id_of(&entity.name)? else {
                continue;
            };
            if let Some(want) = ftype
                && vs.get_entity_type(id)?.as_deref() != Some(want)
            {
                continue;
            }
            text_matches.push((owner_key(OwnerKind::Entity), id));
        }
    }

    let mut score_map: FxHashMap<(u8, i64), AggScore> = FxHashMap::with_capacity_and_hasher(
        vec_owners.len() + text_matches.len(),
        rustc_hash::FxBuildHasher,
    );
    for (rank, (owner_kind, owner_id, _dist, _best)) in vec_owners.iter().enumerate() {
        let entry = score_map
            .entry((owner_key(*owner_kind), *owner_id))
            .or_insert_with(|| AggScore {
                kind: *owner_kind,
                owner_id: *owner_id,
                total: 0.0,
                vec_score: 0.0,
                text_score: 0.0,
            });
        let rrf = params.vec_weight * (1.0 / (rrf_constant + rank as f64));
        entry.total += rrf;
        entry.vec_score += rrf;
    }

    for (rank, &(key_kind, id)) in text_matches.iter().enumerate() {
        let entry = score_map.entry((key_kind, id)).or_insert_with(|| AggScore {
            kind: OwnerKind::Entity,
            owner_id: id,
            total: 0.0,
            vec_score: 0.0,
            text_score: 0.0,
        });
        let rrf = params.text_weight * (1.0 / (rrf_constant + rank as f64));
        entry.total += rrf;
        entry.text_score += rrf;
    }

    let mut scored: Vec<AggScore> = score_map.into_values().collect();
    scored.sort_unstable_by(|a, b| {
        b.total
            .partial_cmp(&a.total)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // The centrality boost stays entity-keyed: relation rows get no boost.
    if vs.graph_node_count() > 0 {
        let g = vs.graph.read();
        for entry in &mut scored {
            if entry.kind == OwnerKind::Entity
                && let Some(nx) = vs.node_map.get(&entry.owner_id)
            {
                let deg = g.neighbors(*nx).count() as f64;
                if deg > 0.0 {
                    let boost = 0.1 * (deg / (deg + 5.0));
                    entry.total += boost;
                }
            }
        }
        scored.sort_unstable_by(|a, b| {
            b.total
                .partial_cmp(&a.total)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    let mut results = Vec::with_capacity(top_k.min(scored.len()));
    for entry in &scored {
        let Some(resolved) = vs.resolve_owner(entry.kind, entry.owner_id)? else {
            continue;
        };
        if ftype.is_some_and(|want| resolved.entity_type != want) {
            continue;
        }
        let best = hits
            .iter()
            .find(|h| h.owner_kind == entry.kind && h.owner_id == entry.owner_id);
        let attachment = if entry.kind == OwnerKind::Attachment {
            let Some(hit) = best.as_ref() else {
                continue;
            };
            let Some((id, _filename, parent, _etype)) = vs.attachment_owner(hit)? else {
                continue;
            };
            let Some((page, excerpt)) = vs.attachment_segment(hit) else {
                continue;
            };
            Some(AttachmentDetail {
                attachment_id: id,
                parent_name: parent,
                page,
                excerpt,
                chunk_score: f64::from(hit.dist),
            })
        } else {
            None
        };
        let chunk = if params.include_chunks && attachment.is_none() {
            best.and_then(|hit| {
                vs.chunk_text(hit).map(|text| ChunkDetail {
                    kind: hit.chunk_kind.as_str().to_string(),
                    text,
                    score: f64::from(hit.dist),
                })
            })
        } else {
            None
        };
        results.push(FusedRow {
            name: resolved.name,
            relation: resolved.relation,
            entity_type: resolved.entity_type,
            kind: resolved.kind,
            score: entry.total,
            text_score: entry.text_score,
            vec_score: entry.vec_score,
            chunk,
            attachment,
        });
        if results.len() == top_k {
            break;
        }
    }

    Ok(results)
}

struct AggScore {
    kind: OwnerKind,
    owner_id: i64,
    total: f64,
    vec_score: f64,
    text_score: f64,
}

/// The tunable half of a fused search, packed so the fusion core stays under
/// the argument-cap lint. `filter` borrows the caller's parsed filter; the
/// fusion core only reads it.
struct HybridParams<'a> {
    text_weight: f64,
    vec_weight: f64,
    top_k: usize,
    filter: Option<&'a SearchFilter>,
    include_chunks: bool,
    allow_attachments: bool,
}

pub fn handle_refresh_graph_cache(
    vs: &VectorStore,
    _kg: &GraphHandle,
    _args: Option<&Value>,
) -> Result<Value> {
    vs.rebuild_graph_cache()?;
    let text = serde_json::to_string(&json!({
        "nodes": vs.graph_node_count(),
        "edges": vs.graph_edge_count(),
    }))
    .map_err(MCSError::JsonError)?;
    Ok(text_content(&text))
}

pub fn handle_vector_store_stats(
    vs: &VectorStore,
    _kg: &GraphHandle,
    _args: Option<&Value>,
) -> Result<Value> {
    // The dimension the store actually serves is the serving profile's, not
    // the CLI `--embedding-dims` fallback (they differ whenever the config
    // file mints a profile at another dimension).
    let dims = vs
        .serving_profile()?
        .map(|profile| profile.dimensions)
        .unwrap_or_else(|| vs.dims());
    let text = serde_json::to_string(&json!({
        "embeddingCount": vs.count(),
        "dims": dims,
        "petgraphNodes": vs.graph_node_count(),
        "petgraphEdges": vs.graph_edge_count(),
    }))
    .map_err(MCSError::JsonError)?;
    Ok(text_content(&text))
}

/// Convert parsed `f64` numbers into the `f32` scratch buffer.
fn to_f32(emb: &[f64]) -> Vec<f32> {
    emb.iter().map(|&v| v as f32).collect()
}

#[inline]
fn cosine_sim(a: &[f32], b: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (&x, &y) in a.iter().zip(b) {
        dot += f64::from(x) * f64::from(y);
        na += f64::from(x) * f64::from(x);
        nb += f64::from(y) * f64::from(y);
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 { 0.0 } else { dot / denom }
}

/// "More like this": find owners nearest to a given entity's identity chunk.
/// `{ entityName, topK?, filter?, includeChunks?, excludeSelf? }`.
pub fn handle_vector_search_by_entity(
    vs: &VectorStore,
    _kg: &GraphHandle,
    args: Option<&Value>,
    allow_attachments: bool,
) -> Result<String> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let name = params
        .get("entityName")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'entityName' parameter".into()))?;
    validate_name(name)?;
    let top_k = opt_usize(params, "topK", DEFAULT_TOP_K)?.clamp(1, MAX_TOP_K);
    let filter = parse_filter(params)?;
    let include_chunks = params
        .get("includeChunks")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let exclude_self = params
        .get("excludeSelf")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let Some(entity_id) = vs.entity_id_of(name)? else {
        return Err(MCSError::InvalidParams(format!(
            "Entity '{name}' does not exist"
        )));
    };
    // The identity chunk is the query. Without one the entity has no vector
    // to search by: the managed snapshot is the only vector source now.
    let query = vs
        .identity_vector(name)?
        .ok_or_else(|| MCSError::InvalidParams(format!("Entity '{name}' has no identity chunk")))?;
    let exclude = if exclude_self {
        Some((OwnerKind::Entity, entity_id))
    } else {
        None
    };
    let json = search_owners(
        vs,
        &query,
        top_k,
        exclude,
        filter.as_ref(),
        include_chunks,
        include_attachments(params, allow_attachments),
    )?;
    Ok(build_content_response(&json))
}

/// Maximal Marginal Relevance search: diversified semantic retrieval.
/// `{ embedding, topK?, fetchK?, lambda?, filter?, includeAttachments? }`.
/// `lambda` in `[0,1]` trades relevance (1.0) against diversity (0.0).
/// Reduces near-duplicate hits — a common RAG context-selection step.
/// The reported `score` is the MMR score.
pub fn handle_vector_mmr_search(
    vs: &VectorStore,
    _kg: &GraphHandle,
    args: Option<&Value>,
    allow_attachments: bool,
) -> Result<String> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let embedding = parse_embedding(
        params
            .get("embedding")
            .ok_or_else(|| MCSError::InvalidParams("Missing 'embedding' parameter".into()))?,
    )?;
    let top_k = opt_usize(params, "topK", DEFAULT_TOP_K)?.clamp(1, MAX_TOP_K);
    let fetch_k = opt_usize(params, "fetchK", (top_k * 4).max(20))?.clamp(top_k, MAX_TOP_K);
    let lambda = opt_f64(params, "lambda", 0.5)?.clamp(0.0, 1.0);
    let parsed_filter = parse_filter(params)?;
    let kind = parsed_filter.as_ref().and_then(|f| f.kind.as_deref());
    let ftype = parsed_filter.as_ref().and_then(|f| f.r#type.as_deref());
    let relation_filter = parsed_filter.as_ref().map(|f| RelationFilter {
        from: f.from.as_deref(),
        to: f.to.as_deref(),
        relation_type: f.relation_type.as_deref(),
    });

    let query = to_f32(&embedding);
    let include_files = include_attachments(params, allow_attachments);

    // The kind filter enters the candidate search itself, so fetchK cannot
    // exhaust the pool on nearer owners of another kind. A long file cannot
    // crowd out the other owners.
    let hits = vs.search_best_chunks(
        &query,
        fetch_k,
        kind,
        ftype,
        relation_filter.as_ref(),
        include_files,
    )?;
    let mut pool: Vec<(OwnerKind, i64, f32)> = Vec::new();
    let mut visited: std::collections::HashSet<(u8, i64)> = std::collections::HashSet::new();
    for hit in &hits {
        let key = (owner_key(hit.owner_kind), hit.owner_id);
        if !visited.insert(key) {
            continue;
        }
        if kind.is_some_and(|want| hit.owner_kind.as_str() != want) {
            continue;
        }
        pool.push((hit.owner_kind, hit.owner_id, hit.dist));
    }

    let mut cands: Vec<MmrCand> = Vec::with_capacity(pool.len());
    for (owner_kind, owner_id, dist) in pool {
        let Some(resolved) = vs.resolve_owner(owner_kind, owner_id)? else {
            continue;
        };
        if ftype.is_some_and(|want| resolved.entity_type != want) {
            continue;
        }
        // Diversity compares the owner's identity chunk against the already
        // selected ones. Relations and attachments store no identity chunk
        // (their chunk kinds are 'relation' and 'attachment'), so the
        // matched segment vector plays that role for them.
        let emb: Option<Vec<f32>> = if owner_kind == OwnerKind::Entity {
            vs.owner_identity_vector(owner_kind, owner_id)
                .ok()
                .unwrap_or(None)
        } else {
            let hit = hits
                .iter()
                .find(|h| h.owner_kind == owner_kind && h.owner_id == owner_id);
            hit.as_ref().and_then(|h| vs.matched_chunk_vector(h).ok()?)
        };
        let Some(matched) = emb else {
            continue;
        };
        let rel = -f64::from(dist);
        let segment = if owner_kind == OwnerKind::Attachment {
            let hit = hits
                .iter()
                .find(|h| h.owner_kind == owner_kind && h.owner_id == owner_id);
            hit.as_ref().and_then(|h| vs.attachment_segment(h))
        } else {
            None
        };
        // The other search paths drop a file whose segment vanished between
        // the hit and the resolve. MMR must do the same, or a just-deleted
        // file surfaces with a cached name and score.
        if owner_kind == OwnerKind::Attachment && segment.is_none() {
            continue;
        }
        let (attachment_id, parent) = if owner_kind == OwnerKind::Attachment {
            let hit = hits
                .iter()
                .find(|h| h.owner_kind == owner_kind && h.owner_id == owner_id);
            let Some(hit) = hit else {
                continue;
            };
            let Some((id, _filename, parent, _etype)) = vs.attachment_owner(hit)? else {
                continue;
            };
            (Some(id), Some(parent))
        } else {
            (None, None)
        };
        cands.push(MmrCand {
            name: resolved.name,
            relation: resolved.relation,
            etype: resolved.entity_type,
            kind: resolved.kind,
            emb: matched,
            rel,
            segment,
            attachment_id,
            parent,
        });
    }

    let mut selected: Vec<MmrCand> = Vec::with_capacity(top_k.min(cands.len()));
    let mut scores: Vec<f64> = Vec::with_capacity(top_k.min(cands.len()));
    while selected.len() < top_k && !cands.is_empty() {
        let mut best_idx = 0usize;
        let mut best_mmr = f64::NEG_INFINITY;
        for (i, c) in cands.iter().enumerate() {
            let max_sim = selected
                .iter()
                .map(|s| cosine_sim(&c.emb, &s.emb))
                .fold(0.0f64, f64::max);
            let mmr = lambda * c.rel - (1.0 - lambda) * max_sim;
            if mmr > best_mmr {
                best_mmr = mmr;
                best_idx = i;
            }
        }
        let chosen = cands.swap_remove(best_idx);
        selected.push(chosen);
        scores.push(best_mmr);
    }

    let mut rows: Vec<OwnerRow> = Vec::with_capacity(selected.len());
    for (c, s) in selected.iter().zip(scores) {
        let attachment = match (&c.segment, c.attachment_id, &c.parent) {
            (Some((page, excerpt)), Some(id), Some(parent)) => Some(AttachmentDetail {
                attachment_id: id,
                parent_name: parent.clone(),
                page: *page,
                excerpt: excerpt.clone(),
                chunk_score: s,
            }),
            _ => None,
        };
        rows.push(OwnerRow {
            name: c.name.clone(),
            relation: c.relation.clone(),
            entity_type: c.etype.clone(),
            kind: c.kind.clone(),
            score: s,
            chunk: None,
            attachment,
        });
    }
    Ok(build_content_response(&build_owner_results(&rows, false)))
}

struct MmrCand {
    name: Option<String>,
    relation: Option<RelationOwner>,
    etype: String,
    kind: String,
    emb: Vec<f32>,
    rel: f64,
    segment: Option<(i64, String)>,
    /// Attachment rows only: the live file id and parent entity name the
    /// snapshot-consistent resolve returned.
    attachment_id: Option<i64>,
    parent: Option<String>,
}

/// Scale a vector to unit length in place.
///
/// A zero vector keeps its value. Dividing by a zero norm would fill the
/// vector with NaN, and every later comparison against NaN is false, so the
/// search would silently return nothing.
#[cfg(feature = "indexer")]
fn l2_normalize(vector: &mut [f32]) {
    let norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 && norm.is_finite() {
        let scale = 1.0 / norm;
        for value in vector.iter_mut() {
            *value = (f64::from(*value) * scale) as f32;
        }
    }
}

/// Search by text: `{ queryText, topK?, filter?, includeChunks?, textWeight?,
/// vecWeight? }`.
///
/// The server embeds the query with the model the serving index profile names,
/// so the query vector and the stored vectors come from one model. A chat
/// client needs no embedding service of its own. Result rows are kind-marked,
/// and `filter` narrows the owner set before ranking.
///
/// `textWeight` or `vecWeight` turns on fusion with the FTS5 ranking, through
/// the same [`perform_hybrid_search`] that `hybrid_search` uses. Without
/// either, the search is pure vector.
#[cfg(feature = "indexer")]
pub fn handle_semantic_search(
    vs: &VectorStore,
    kg: &GraphHandle,
    args: Option<&Value>,
    allow_attachments: bool,
) -> Result<String> {
    use mcpmem_core::jobs::Normalization;
    use mcpmem_indexer::EmbeddingProvider;

    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;

    let query_text = params
        .get("queryText")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'queryText' parameter".into()))?;
    // Blank text embeds to a vector that means nothing, and the provider bills
    // for the call. Refuse it before any network request.
    if query_text.trim().is_empty() {
        return Err(MCSError::InvalidParams(
            "'queryText' must not be empty or whitespace".into(),
        ));
    }

    let top_k = opt_usize(params, "topK", DEFAULT_TOP_K)?.clamp(1, MAX_TOP_K);
    let filter = parse_filter(params)?;
    let include_chunks = params
        .get("includeChunks")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Either weight turns on fusion. The other one then takes its default, the
    // rule `hybrid_search` already follows. A null counts as absent, because
    // `opt_f64` reads it that way.
    let given = |key: &str| matches!(params.get(key), Some(value) if !value.is_null());
    let fuse = given("textWeight") || given("vecWeight");

    let profile = vs.serving_profile()?.ok_or_else(|| {
        MCSError::InvalidParams(
            "This store serves no index profile, so the server does not know which model to \
             embed with. Name a provider, a model and a dimension in the [indexer] section of \
             the configuration file, then restart the server."
                .into(),
        )
    })?;

    let provider = crate::indexer_provider::get().ok_or_else(|| {
        MCSError::InvalidParams(format!(
            "No embedding provider is configured, so the server cannot embed the query text. \
             The serving profile names the provider kind '{}'. Configure that provider in the \
             [indexer] section of the configuration file, then restart the server.",
            profile.provider_kind
        ))
    })?;

    let texts = [query_text.to_owned()];
    let vectors = provider
        .embed_texts(&profile, &texts)
        .map_err(|e| MCSError::MemoryError(format!("Embedding the query text failed: {e}")))?;

    // One text goes in, so one vector must come back. Any other count is a
    // provider fault. `into_iter().next()` takes the vector without an index,
    // so that fault cannot panic the server.
    let returned = vectors.len();
    let mut query = vectors
        .into_iter()
        .next()
        .filter(|_| returned == 1)
        .ok_or_else(|| {
            MCSError::MemoryError(format!(
                "The embedding provider returned {returned} vectors for one query text; it must \
                 return exactly one"
            ))
        })?;

    if query.len() != profile.dimensions as usize {
        return Err(MCSError::MemoryError(format!(
            "The embedding provider returned {} dimensions and the serving profile names model \
             '{}' at {} dimensions. The provider and the profile disagree.",
            query.len(),
            profile.model,
            profile.dimensions
        )));
    }

    // The worker validates a stored vector as L2 normalized before it commits
    // the job. Nothing validates a query vector, and cosine distance against an
    // unnormalized query ranks the results wrongly, so normalize it here.
    if profile.normalization == Normalization::L2 {
        l2_normalize(&mut query);
    }

    if fuse {
        let text_weight = opt_f64(params, "textWeight", 0.5)?;
        let vec_weight = opt_f64(params, "vecWeight", 0.5)?;
        // The chunk-level filter applies inside the fusion, so a filtered call
        // needs no widened pool and no post-filtering; the FTS half applies
        // the same type predicate to its entity matches.
        let params = HybridParams {
            text_weight,
            vec_weight,
            top_k,
            filter: filter.as_ref(),
            include_chunks,
            allow_attachments: include_attachments(params, allow_attachments),
        };
        let results = perform_hybrid_search(vs, kg, query_text, &query, &params)?;
        return Ok(build_content_response(&build_fused_results(
            &results,
            include_chunks,
        )));
    }

    let json = search_owners(
        vs,
        &query,
        top_k,
        None,
        filter.as_ref(),
        include_chunks,
        include_attachments(params, allow_attachments),
    )?;
    Ok(build_content_response(&json))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared owner-row renderer: kind-marked rows, and the `chunk`
    /// member exactly when detail is present, with the wire key names a client
    /// parses for all four search tools.
    #[test]
    fn build_owner_results_renders_kind_rows_and_chunk_member() {
        let rows = vec![OwnerRow {
            name: Some("ada".to_string()),
            relation: None,
            entity_type: "Person".to_string(),
            kind: "entity".to_string(),
            score: 0.25,
            chunk: Some(ChunkDetail {
                kind: "identity".to_string(),
                text: "ada\nPerson".to_string(),
                score: 0.25,
            }),
            attachment: None,
        }];
        assert_eq!(
            build_owner_results(&rows, true),
            r#"{"results":[{"name":"ada","entityType":"Person","kind":"entity","score":0.250000,"chunk":{"kind":"identity","text":"ada\nPerson","score":0.250000}}],"count":1}"#
        );
    }

    /// Without detail the row must not carry a `chunk` member at all.
    #[test]
    fn build_owner_results_omits_chunk_member_without_detail() {
        let rows = vec![OwnerRow {
            name: Some("acme".to_string()),
            relation: None,
            entity_type: "Company".to_string(),
            kind: "entity".to_string(),
            score: 0.5,
            chunk: None,
            attachment: None,
        }];
        let json = build_owner_results(&rows, false);
        assert_eq!(
            json,
            r#"{"results":[{"name":"acme","entityType":"Company","kind":"entity","score":0.500000}],"count":1}"#
        );
        assert!(!json.contains("\"chunk\""), "no chunk member: {json}");
    }

    /// Chunk text escapes like any JSON string value: quotes and backslashes
    /// must not break the row shape, and the text must round-trip through a
    /// JSON parser back to the source bytes. The row is a relation, so it
    /// renders the structured triple in place of a `name` member.
    #[test]
    fn chunk_member_escapes_text() {
        let rows = vec![OwnerRow {
            name: None,
            relation: Some(RelationOwner {
                from: "a".to_string(),
                to: "b".to_string(),
                relation_type: "t".to_string(),
            }),
            entity_type: "t".to_string(),
            kind: "relation".to_string(),
            score: 0.0,
            chunk: Some(ChunkDetail {
                kind: "relation".to_string(),
                text: "a \"quoted\" \\ path\nline".to_string(),
                score: 0.0,
            }),
            attachment: None,
        }];
        let json = build_owner_results(&rows, true);
        assert!(
            json.contains(
                r#""chunk":{"kind":"relation","text":"a \"quoted\" \\ path\nline","score":0.000000}"#
            ),
            "escaped text inside the chunk member: {json}"
        );
        assert!(
            json.contains(r#""from":"a","to":"b","relationType":"t""#),
            "a relation row carries the triple, not a name: {json}"
        );
        assert!(!json.contains("\"name\""), "no name member on a relation row: {json}");
        let parsed: Value = serde_json::from_str(&json).expect("the row JSON parses");
        assert_eq!(
            parsed["results"][0]["chunk"]["text"].as_str(),
            Some("a \"quoted\" \\ path\nline")
        );
    }

    /// The fused renderer is the other `write_chunk_detail` consumer; the
    /// chunk member must sit after the scores, kind-marked rows included.
    #[test]
    fn fused_rows_render_kind_and_chunk_member() {
        let rows = vec![FusedRow {
            name: Some("ada".to_string()),
            relation: None,
            entity_type: "Person".to_string(),
            kind: "entity".to_string(),
            score: 1.0,
            text_score: 0.5,
            vec_score: 0.5,
            chunk: Some(ChunkDetail {
                kind: "observation".to_string(),
                text: "writes rust".to_string(),
                score: 0.1,
            }),
            attachment: None,
        }];
        assert_eq!(
            build_fused_results(&rows, true),
            r#"{"results":[{"name":"ada","entityType":"Person","kind":"entity","score":1.000000,"textScore":0.500000,"vecScore":0.500000,"chunk":{"kind":"observation","text":"writes rust","score":0.100000}}],"count":1}"#
        );
    }

    /// An attachment row carries filename, attachmentId, parent entityName,
    /// page and excerpt in place of the chunk member, because the segment is
    /// the row itself. The excerpt repeats inside the chunk detail when the
    /// caller asked for chunks.
    #[test]
    fn owner_rows_render_attachment_metadata() {
        let rows = vec![OwnerRow {
            name: Some("notes.txt".to_string()),
            relation: None,
            entity_type: "Person".to_string(),
            kind: "attachment".to_string(),
            score: 0.3,
            chunk: None,
            attachment: Some(AttachmentDetail {
                attachment_id: 7,
                parent_name: "ada".to_string(),
                page: 2,
                excerpt: "PAGE TWO exact excerpt".to_string(),
                chunk_score: 0.3,
            }),
        }];
        let json = build_owner_results(&rows, true);
        assert_eq!(
            json,
            r#"{"results":[{"name":"notes.txt","entityType":"Person","kind":"attachment","score":0.300000,"filename":"notes.txt","attachmentId":7,"entityName":"ada","page":2,"excerpt":"PAGE TWO exact excerpt","chunk":{"kind":"attachment","text":"PAGE TWO exact excerpt","score":0.300000}}],"count":1}"#,
            "{json}"
        );
        let json = build_owner_results(&rows, false);
        assert!(
            !json.contains("\"chunk\""),
            "the excerpt stays, the chunk detail goes: {json}"
        );
        assert!(
            json.contains(
                r#","filename":"notes.txt","attachmentId":7,"entityName":"ada","page":2,"excerpt":"PAGE TWO exact excerpt"}"#
            ),
            "file metadata stays without chunk detail: {json}"
        );
    }
}
