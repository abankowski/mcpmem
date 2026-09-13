use serde_json::{Value, json};

use crate::errors::{MCSError, Result};
use crate::kg::{GraphHandle, push_json_str};
use crate::vector_store::{VectorStore, with_scratch};
use mcpmem_core::jobs::OwnerKind;
use rustc_hash::FxHashMap;

/// One fused result row: the display triple, the three scores, and the best
/// vector chunk when the caller asked for chunk detail.
struct FusedRow {
    name: String,
    entity_type: String,
    kind: String,
    score: f64,
    text_score: f64,
    vec_score: f64,
    chunk: Option<ChunkDetail>,
}

/// The matched chunk of one result row for `includeChunks`: its kind, its
/// reassembled text, and its distance.
struct ChunkDetail {
    kind: String,
    text: String,
    score: f64,
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

/// Owner-level filter shared by the four search tools. `kind` limits the owner
/// kind ("entity" or "relation"); `type` matches the chunk type name in
/// `type_dict` exactly. `type` without `kind` matches either dict kind.
#[derive(Clone, Debug, Default)]
pub struct SearchFilter {
    pub kind: Option<String>,
    pub r#type: Option<String>,
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

/// Parse the `filter` argument. `None` when the argument is absent or null.
/// The `kind` whitelist is exactly "entity" and "relation"; anything else is
/// refused before any search runs.
fn parse_filter(params: &Value) -> Result<Option<SearchFilter>> {
    let Some(f) = params.get("filter").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let object = f
        .as_object()
        .ok_or_else(|| MCSError::InvalidParams("'filter' must be an object".into()))?;
    let kind = filter_member(object, "kind")?;
    if kind.is_some_and(|k| k != "entity" && k != "relation") {
        return Err(MCSError::InvalidParams(
            "'filter.kind' must be \"entity\" or \"relation\"".into(),
        ));
    }
    let ftype = filter_member(object, "type")?;
    Ok(Some(SearchFilter {
        kind: kind.map(str::to_string),
        r#type: ftype.map(str::to_string),
    }))
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

/// One owner-level result row of the shared search: display triple, owner
/// score (the best chunk's distance), and the matched chunk for
/// `includeChunks`.
type OwnerRow = (String, String, String, f64, Option<ChunkDetail>);

/// Search owners by their best chunk. The owner set, the filter, the
/// kind-marked rows, and the chunk detail are the shared contract of the four
/// search tools; `exclude` drops one owner from the results (the query entity
/// itself in `vector_search_by_entity`) while still returning `top_k` rows.
fn search_owners(
    vs: &VectorStore,
    query: &[f32],
    top_k: usize,
    exclude: Option<(OwnerKind, i64)>,
    filter: Option<&SearchFilter>,
    include_chunks: bool,
) -> Result<String> {
    Ok(build_owner_results(&search_owner_rows(
        vs,
        query,
        top_k,
        exclude,
        filter,
        include_chunks,
    )?))
}

fn search_owner_rows(
    vs: &VectorStore,
    query: &[f32],
    top_k: usize,
    exclude: Option<(OwnerKind, i64)>,
    filter: Option<&SearchFilter>,
    include_chunks: bool,
) -> Result<Vec<OwnerRow>> {
    // Ask for one owner more than returned so dropping the excluded owner
    // (which ranks first, its own identity chunk is the query) still leaves a
    // full result set. The final truncate covers the case where the excluded
    // owner is not in the ranked set at all.
    let target = top_k.saturating_add(usize::from(exclude.is_some()));
    let kind = filter.and_then(|f| f.kind.as_deref());
    let ftype = filter.and_then(|f| f.r#type.as_deref());
    let mut rows: Vec<OwnerRow> = Vec::with_capacity(target);

    // Chunk-serving store: overfetch chunks so owners whose best chunk sits
    // past `top_k` still rank, then reduce to one best chunk per owner.
    // Truncating at the chunk level first would let one owner's many near
    // chunks crowd out the other owners and under-fill the result set.
    let fetch = (target * 8).clamp(target, 1000);
    let hits = vs.search_chunks(query, fetch, kind, ftype)?;
    let owners = vs.aggregate_owners(&hits, target);
    for (owner_kind, owner_id, dist, _best_idx) in owners {
        if exclude == Some((owner_kind, owner_id)) {
            continue;
        }
        let Some((name, etype, kind_label)) = vs.resolve_owner(owner_kind, owner_id)? else {
            continue;
        };
        let chunk = if include_chunks {
            hits.iter()
                .find(|h| h.owner_kind == owner_kind && h.owner_id == owner_id)
                .and_then(|h| {
                    vs.chunk_text(h).map(|text| ChunkDetail {
                        kind: h.chunk_kind.as_str().to_string(),
                        text,
                        score: f64::from(h.dist),
                    })
                })
        } else {
            None
        };
        rows.push((name, etype, kind_label, f64::from(dist), chunk));
    }
    rows.truncate(top_k);
    Ok(rows)
}

/// Append one `"chunk":{...}` member to a result row.
fn write_chunk_detail(out: &mut String, chunk: &ChunkDetail) {
    use std::fmt::Write;
    out.push_str(r#","chunk":{"kind":"#);
    push_json_str(out, &chunk.kind);
    out.push_str(r#","text":"#);
    push_json_str(out, &chunk.text);
    write!(out, r#","score":{:.6}}}"#, chunk.score).unwrap();
}

/// Render owner rows `(name, entityType, kind, score, chunk?)` as the standard
/// results JSON: `{"results":[{name, entityType, kind, score, chunk?}],
/// "count":N}`.
fn build_owner_results(rows: &[OwnerRow]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(128 + rows.len() * 96);
    out.push_str(r#"{"results":["#);
    for (i, (name, etype, kind, score, chunk)) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(r#"{"name":"#);
        push_json_str(&mut out, name);
        out.push_str(r#","entityType":"#);
        push_json_str(&mut out, etype);
        out.push_str(r#","kind":"#);
        push_json_str(&mut out, kind);
        write!(out, r#","score":{score:.6}"#).unwrap();
        if let Some(chunk) = chunk {
            write_chunk_detail(&mut out, chunk);
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
    }
}

pub fn handle_vector_search_entities(
    vs: &VectorStore,
    _kg: &GraphHandle,
    args: Option<&Value>,
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
        search_owners(vs, buf, top_k, None, filter.as_ref(), include_chunks)
    })?;

    Ok(build_content_response(&json))
}

pub fn handle_hybrid_search(
    vs: &VectorStore,
    kg: &GraphHandle,
    args: Option<&Value>,
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
        };
        perform_hybrid_search(vs, kg, query_text, buf, &params)
    })?;

    Ok(build_content_response(&build_fused_results(&results)))
}

/// Render fused rows as the standard results JSON. `hybrid_search` and
/// `semantic_search` fuse the same two rankings, so a client parses one shape
/// for both. Rows are kind-marked, and `includeChunks` attaches the best
/// vector chunk of the owner.
fn build_fused_results(results: &[FusedRow]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(128 + results.len() * 96);
    out.push_str(r#"{"results":["#);
    for (i, row) in results.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(r#"{"name":"#);
        push_json_str(&mut out, &row.name);
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
        if let Some(chunk) = &row.chunk {
            write_chunk_detail(&mut out, chunk);
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

    // The vector half runs at owner level: chunk hits aggregate to one best
    // chunk per owner, so one owner's many chunks cannot crowd out the others.
    let hits = vs.search_chunks(query_emb, fetch_k, kind, ftype)?;
    let vec_owners = vs.aggregate_owners(&hits, fetch_k);

    // The FTS half matches entities only. A relation-kind filter excludes it
    // (relation rows enter the fusion with their vector rank only), and a type
    // filter applies to the entity type, mirroring the chunk-level predicate.
    let mut text_matches: Vec<(u8, i64)> = Vec::new();
    if kind != Some("relation") {
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
    for entry in scored.iter().take(top_k) {
        let Some((name, etype, kind_label)) = vs.resolve_owner(entry.kind, entry.owner_id)? else {
            continue;
        };
        let chunk = if params.include_chunks {
            hits.iter()
                .find(|h| h.owner_kind == entry.kind && h.owner_id == entry.owner_id)
                .and_then(|h| {
                    vs.chunk_text(h).map(|text| ChunkDetail {
                        kind: h.chunk_kind.as_str().to_string(),
                        text,
                        score: f64::from(h.dist),
                    })
                })
        } else {
            None
        };
        results.push(FusedRow {
            name,
            entity_type: etype,
            kind: kind_label,
            score: entry.total,
            text_score: entry.text_score,
            vec_score: entry.vec_score,
            chunk,
        });
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

/// Render resolved `(name, entityType, score)` rows as the standard results JSON.
fn build_named_results(rows: &[(String, String, f64)]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(64 + rows.len() * 64);
    out.push_str(r#"{"results":["#);
    for (i, (name, etype, score)) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(r#"{"name":"#);
        push_json_str(&mut out, name);
        out.push_str(r#","entityType":"#);
        push_json_str(&mut out, etype);
        write!(out, r#","score":{score:.6}}}"#).unwrap();
    }
    out.push_str(r#"],"count":"#);
    out.push_str(&rows.len().to_string());
    out.push('}');
    out
}

/// "More like this": find owners nearest to a given entity's identity chunk.
/// `{ entityName, topK?, filter?, includeChunks?, excludeSelf? }`.
pub fn handle_vector_search_by_entity(
    vs: &VectorStore,
    _kg: &GraphHandle,
    args: Option<&Value>,
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
    let json = search_owners(vs, &query, top_k, exclude, filter.as_ref(), include_chunks)?;
    Ok(build_content_response(&json))
}

/// Maximal Marginal Relevance search: diversified semantic retrieval.
/// `{ embedding, topK?, fetchK?, lambda?, entityType? }`. `lambda` in `[0,1]`
/// trades relevance (1.0) against diversity (0.0). Reduces near-duplicate hits —
/// a common RAG context-selection step. The reported `score` is the MMR score.
pub fn handle_vector_mmr_search(
    vs: &VectorStore,
    _kg: &GraphHandle,
    args: Option<&Value>,
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
    let entity_type = params
        .get("entityType")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    let query = to_f32(&embedding);

    // The candidate pool is owner-level: chunk hits aggregate to one best
    // chunk per owner, so one owner's many chunks cannot crowd out the others.
    let mut pool: Vec<(OwnerKind, i64, f32)> = Vec::new();
    let hits = vs.search_chunks(&query, fetch_k, Some("entity"), entity_type)?;
    for (kind, id, dist, _) in vs.aggregate_owners(&hits, fetch_k) {
        pool.push((kind, id, dist));
    }

    let mut cands: Vec<MmrCand> = Vec::with_capacity(pool.len());
    for (kind, id, dist) in pool {
        let Some((name, etype, _)) = vs.resolve_owner(kind, id)? else {
            continue;
        };
        if let Some(ft) = entity_type
            && etype != ft
        {
            continue;
        }
        // Diversity compares the owner's identity chunk against the already
        // selected ones. Without a vector the owner cannot be diversified, so
        // it drops out of the pool.
        let emb = match vs.owner_identity_vector(kind, id)? {
            Some(v) => v,
            None => continue,
        };
        let rel = -f64::from(dist);
        cands.push(MmrCand {
            name,
            etype,
            emb,
            rel,
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

    let named: Vec<(String, String, f64)> = selected
        .into_iter()
        .zip(scores)
        .map(|(c, s)| (c.name, c.etype, s))
        .collect();
    Ok(build_content_response(&build_named_results(&named)))
}

struct MmrCand {
    name: String,
    etype: String,
    emb: Vec<f32>,
    rel: f64,
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
        };
        let results = perform_hybrid_search(vs, kg, query_text, &query, &params)?;
        return Ok(build_content_response(&build_fused_results(&results)));
    }

    let json = search_owners(vs, &query, top_k, None, filter.as_ref(), include_chunks)?;
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
        let rows = vec![(
            "ada".to_string(),
            "Person".to_string(),
            "entity".to_string(),
            0.25,
            Some(ChunkDetail {
                kind: "identity".to_string(),
                text: "ada\nPerson".to_string(),
                score: 0.25,
            }),
        )];
        assert_eq!(
            build_owner_results(&rows),
            r#"{"results":[{"name":"ada","entityType":"Person","kind":"entity","score":0.250000,"chunk":{"kind":"identity","text":"ada\nPerson","score":0.250000}}],"count":1}"#
        );
    }

    /// Without detail the row must not carry a `chunk` member at all.
    #[test]
    fn build_owner_results_omits_chunk_member_without_detail() {
        let rows = vec![(
            "acme".to_string(),
            "Company".to_string(),
            "entity".to_string(),
            0.5,
            None,
        )];
        let json = build_owner_results(&rows);
        assert_eq!(
            json,
            r#"{"results":[{"name":"acme","entityType":"Company","kind":"entity","score":0.500000}],"count":1}"#
        );
        assert!(!json.contains("\"chunk\""), "no chunk member: {json}");
    }

    /// Chunk text escapes like any JSON string value: quotes and backslashes
    /// must not break the row shape, and the text must round-trip through a
    /// JSON parser back to the source bytes.
    #[test]
    fn chunk_member_escapes_text() {
        let rows = vec![(
            "e".to_string(),
            "t".to_string(),
            "relation".to_string(),
            0.0,
            Some(ChunkDetail {
                kind: "relation".to_string(),
                text: "a \"quoted\" \\ path\nline".to_string(),
                score: 0.0,
            }),
        )];
        let json = build_owner_results(&rows);
        assert!(
            json.contains(
                r#""chunk":{"kind":"relation","text":"a \"quoted\" \\ path\nline","score":0.000000}"#
            ),
            "escaped text inside the chunk member: {json}"
        );
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
            name: "ada".to_string(),
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
        }];
        assert_eq!(
            build_fused_results(&rows),
            r#"{"results":[{"name":"ada","entityType":"Person","kind":"entity","score":1.000000,"textScore":0.500000,"vecScore":0.500000,"chunk":{"kind":"observation","text":"writes rust","score":0.100000}}],"count":1}"#
        );
    }
}
