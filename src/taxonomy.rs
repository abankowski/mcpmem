//! Suggests taxonomy names for a new name.
//!
//! The engine compares a new name with the existing names. Each candidate gets
//! a score from trigram overlap and Levenshtein distance. A candidate passes
//! only when its score is above a threshold. This module is pure: it takes
//! names and counts, and it returns scored suggestions. It does no I/O.

use serde::Serialize;
use serde_json::{Value, json};

use crate::errors::{MCSError, Result};
use crate::kg::GraphHandle;
use crate::vector_store::VectorStore;

/// The kind of subject a taxonomy entry describes.
///
/// The variants match the `subject_kind` column in the taxonomy tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectKind {
    /// An entity type.
    EntityType = 0,
    /// A relation type.
    RelationType = 1,
    /// A relation instance.
    Relation = 2,
}

/// A candidate name with its score.
///
/// The serializer uses camelCase names, so the JSON form is `name` and
/// `score`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    /// The candidate name.
    pub name: String,
    /// The similarity score of the candidate.
    pub score: f64,
}

/// Suggests existing names that are similar to `name`.
///
/// Each entry in `existing` is a name and its usage count. The engine skips an
/// entry with count 0, and skips the entry whose name equals `name`. A
/// candidate must score above 0.35 to be kept. The result sorts by score
/// descending, then by name ascending.
pub fn suggest_strings(name: &str, existing: &[(&str, usize)]) -> Vec<Suggestion> {
    let query: String = name.to_lowercase();
    let mut out: Vec<Suggestion> = Vec::new();

    for (candidate, count) in existing {
        if *count == 0 {
            continue;
        }
        let lowered: String = candidate.to_lowercase();
        if lowered == query {
            continue;
        }
        let score = similarity(&query, &lowered);
        if score > 0.35 {
            out.push(Suggestion {
                name: (*candidate).to_string(),
                score,
            });
        }
    }

    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// Computes the similarity score of `a` and `b`.
///
/// The score is the trigram overlap term times 0.7, plus the Levenshtein term
/// times 0.3.
fn similarity(a: &str, b: &str) -> f64 {
    let ta = trigrams(a);
    let tb = trigrams(b);
    let denom = ta.len() + tb.len();
    let overlap = |xs: &[String], ys: &[String]| xs.iter().filter(|x| ys.contains(x)).count();
    let trigram = if denom == 0 {
        0.0
    } else {
        2.0 * overlap(&ta, &tb) as f64 / denom as f64
    } * 0.7;

    let maxlen = a.chars().count().max(b.chars().count());
    let dist = levenshtein(a, b);
    let levenshtein = if maxlen == 0 {
        0.0
    } else {
        1.0 - dist as f64 / maxlen as f64
    } * 0.3;

    trigram + levenshtein
}

/// Returns the overlapping trigrams of `s` as a vector.
fn trigrams(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    chars
        .windows(3)
        .map(|w| w.iter().collect())
        .collect()
}

/// Returns the Levenshtein distance of `a` and `b`.
///
/// The computation stops early when the distance already exceeds the longer
/// length, because the resulting similarity can never clear the threshold.
fn levenshtein(a: &str, b: &str) -> usize {
    let ca: Vec<char> = a.chars().collect();
    let cb: Vec<char> = b.chars().collect();
    let maxlen = ca.len().max(cb.len());
    if maxlen == 0 {
        return 0;
    }

    let mut prev: Vec<usize> = (0..=cb.len()).collect();
    for (i, x) in ca.iter().enumerate() {
        let mut cur = vec![0usize; cb.len() + 1];
        cur[0] = i + 1;
        let mut row_min = cur[0];
        for (j, y) in cb.iter().enumerate() {
            cur[j + 1] = if x == y {
                prev[j]
            } else {
                1 + prev[j].min(cur[j]).min(prev[j + 1])
            };
            row_min = row_min.min(cur[j + 1]);
        }
        if row_min > maxlen {
            return maxlen + 1;
        }
        prev = cur;
    }
    prev[cb.len()]
}

/// Default `topK` when the caller omits the argument.
const DEFAULT_SUGGESTION_K: usize = 10;
/// Lowest allowed `topK`.
const MIN_SUGGESTION_K: usize = 1;
/// Highest allowed `topK`.
const MAX_SUGGESTION_K: usize = 100;

/// Handles the `suggest_taxonomy` read tool.
///
/// The tool suggests existing taxonomy names that are similar to `query`.
/// For kind `entityType` and `relationType` the semantic tier returns the
/// nearest type names from the ANN snapshot, and the offline string engine
/// fills the remaining slots. For kind `entity` the entity ANN returns the
/// nearest entity names. For kind `relation` the kind-2 taxonomy snapshot
/// returns the nearest relation names. Every semantic failure falls back to
/// the offline tier for the type kinds, and to an empty list for the other
/// kinds, so the tool never errors on a missing semantic tier. The result
/// is a JSON object of the shape `{ "suggestions": [...] }`.
pub fn handle_suggest_taxonomy(
    vs: Option<&VectorStore>,
    kg: &GraphHandle,
    args: Option<&Value>,
) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let query = params
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'query' parameter".into()))?;
    // Blank text matches nothing and asks the engine to compare an empty
    // string against every name. Refuse it before the counts query.
    if query.trim().is_empty() {
        return Err(MCSError::InvalidParams(
            "'query' must not be empty or whitespace".into(),
        ));
    }

    let kind: &str = match params.get("kind") {
        None | Some(Value::Null) => "entityType",
        Some(v) => v.as_str().ok_or_else(|| {
            MCSError::InvalidParams("'kind' must be a string".into())
        })?,
    };
    match kind {
        "entityType" | "relationType" | "entity" | "relation" => (),
        _ => return Err(MCSError::InvalidParams(format!(
            "'kind' must be one of entityType, relationType, entity, relation"
        ))),
    }
    let top_k = opt_usize(params, "topK", DEFAULT_SUGGESTION_K)?
        .clamp(MIN_SUGGESTION_K, MAX_SUGGESTION_K);

    let offline = |counts: Vec<(String, usize)>| -> Vec<Suggestion> {
        let existing: Vec<(&str, usize)> = counts
            .iter()
            .map(|(n, c)| (n.as_str(), *c))
            .collect();
        suggest_strings(query, &existing)
    };
    let suggestions: Vec<Suggestion> = match kind {
        "entityType" => semantic_first(
            vs,
            query,
            SubjectKind::EntityType,
            top_k,
            offline(kg.entity_type_counts()),
        ),
        "relationType" => semantic_first(
            vs,
            query,
            SubjectKind::RelationType,
            top_k,
            offline(kg.relation_type_counts()),
        ),
        "entity" => entity_suggestions(vs, query, top_k),
        "relation" => semantic_first(vs, query, SubjectKind::Relation, top_k, Vec::new()),
        _ => unreachable!("the kind was validated above"),
    };

    Ok(json!({ "suggestions": suggestions }))
}

/// Reads a non-negative integer argument, or `default` when it is absent.
fn opt_usize(params: &Value, key: &str, default: usize) -> Result<usize> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_u64().map(|n| n as usize).ok_or_else(|| {
            MCSError::InvalidParams(format!("'{key}' must be a non-negative integer"))
        }),
    }
}

/// Scales a vector to unit length in place.
///
/// A zero vector keeps its value. Dividing by a zero norm would fill the
/// vector with NaN, and every later comparison against NaN is false, so the
/// search would silently return nothing. This is a copy of the private
/// `vector_actions::l2_normalize`; keep the two in step.
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

/// Suggests taxonomy subjects through the semantic tier.
///
/// The server embeds every text with the model the serving index profile
/// names, then searches the profile's per-kind ANN snapshots for the nearest
/// subjects. One provider call embeds all texts; each text searches the
/// kind's snapshot separately. A suggestion's score is `1.0 - distance`, the
/// convention `semantic_search` results use. An absent snapshot yields an
/// empty list, never an error, so the caller can fall back to the offline
/// string engine.
///
/// The result groups by input text: `out[i]` holds the suggestions for
/// `texts[i]`. A caller that batches one write's unknown types keeps the
/// one-provider-call bound and still splits the results back per type.
#[cfg(feature = "indexer")]
pub fn suggest_semantic(
    vs: &crate::vector_store::VectorStore,
    texts: &[String],
    kind: SubjectKind,
    top_k: usize,
) -> Result<Vec<Vec<Suggestion>>> {
    use mcpmem_core::jobs::Normalization;
    use mcpmem_indexer::EmbeddingProvider;

    let top_k = top_k.clamp(1, 100);

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

    // One call for the whole batch: each call bills for the request. A wrong
    // count is a provider fault; the per-text loop below would otherwise pair
    // results with the wrong texts.
    let vectors = provider
        .embed_texts(&profile, &texts)
        .map_err(|e| MCSError::MemoryError(format!("Embedding the taxonomy texts failed: {e}")))?;
    if vectors.len() != texts.len() {
        return Err(MCSError::MemoryError(format!(
            "The embedding provider returned {} vectors for {} texts; it must return exactly \
             one vector per text",
            vectors.len(),
            texts.len()
        )));
    }

    // `SubjectKind` and `TaxonomyKind` are the same numeric contract: the
    // variants line up by discriminant, matching the `subject_kind` column.
    let taxonomy_kind = match kind {
        SubjectKind::EntityType => crate::vector_store::TaxonomyKind::EntityType,
        SubjectKind::RelationType => crate::vector_store::TaxonomyKind::RelationType,
        SubjectKind::Relation => crate::vector_store::TaxonomyKind::Relation,
    };
    let mut groups: Vec<Vec<Suggestion>> = Vec::with_capacity(texts.len());
    for mut query in vectors {
        if query.len() != profile.dimensions as usize {
            return Err(MCSError::MemoryError(format!(
                "The embedding provider returned {} dimensions and the serving profile names \
                 model '{}' at {} dimensions. The provider and the profile disagree.",
                query.len(),
                profile.model,
                profile.dimensions
            )));
        }
        // Nothing validates a query vector, and cosine distance against an
        // unnormalized query ranks the results wrongly, so normalize it here,
        // exactly as `handle_semantic_search` does.
        if profile.normalization == Normalization::L2 {
            l2_normalize(&mut query);
        }
        let mut hits = vs.search_taxonomy(taxonomy_kind, &query, top_k)?;
        // The snapshot search already returns at most top_k hits; the clamp
        // keeps the per-text bound local to this function.
        hits.truncate(top_k);
        let mut group: Vec<Suggestion> = Vec::with_capacity(hits.len());
        for (id, distance) in hits {
            // A vector may point at a deleted subject: the snapshot never
            // sees the deletion, so the row resolves to nothing. Skip it.
            let Some((name, _)) = vs.resolve_taxonomy(taxonomy_kind, id) else {
                continue;
            };
            group.push(Suggestion { name, score: 1.0 - distance });
        }
        groups.push(group);
    }
    Ok(groups)
}

/// Merges the semantic results with the offline results.
///
/// The semantic results sort first. The offline tier then fills the
/// remaining slots, skipping a name the semantic tier already returned.
/// Any missing piece of the semantic tier, and any semantic failure, falls
/// back to the offline tier alone.
#[cfg(feature = "indexer")]
fn semantic_first(
    vs: Option<&VectorStore>,
    query: &str,
    kind: SubjectKind,
    top_k: usize,
    offline: Vec<Suggestion>,
) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = Vec::new();
    // The tier is advisory: every failure must fall back, never error.
    if let Some(store) = vs
        && crate::indexer_provider::get().is_some()
        && store.serving_profile().ok().flatten().is_some()
        && let Ok(groups) = suggest_semantic(store, &[query.to_owned()], kind, top_k)
        && let Some(first) = groups.into_iter().next()
    {
        out = first;
    }
    for suggestion in offline {
        if !out.iter().any(|s| s.name == suggestion.name) {
            out.push(suggestion);
        }
    }
    out.truncate(top_k);
    out
}

/// Without the `indexer` feature the offline tier stands alone.
#[cfg(not(feature = "indexer"))]
fn semantic_first(
    _vs: Option<&VectorStore>,
    _query: &str,
    _kind: SubjectKind,
    top_k: usize,
    mut offline: Vec<Suggestion>,
) -> Vec<Suggestion> {
    offline.truncate(top_k);
    offline
}

/// Suggests entity names through the entity ANN.
///
/// The tier is advisory: any missing piece or any failure returns an empty
/// list, never an error, because entity names are not taxonomy types and
/// there is no offline fallback for them.
#[cfg(feature = "indexer")]
fn entity_suggestions(vs: Option<&VectorStore>, query: &str, top_k: usize) -> Vec<Suggestion> {
    if let Some(store) = vs
        && crate::indexer_provider::get().is_some()
        && store.serving_profile().ok().flatten().is_some()
        && let Ok(picks) = suggest_entities(store, query, top_k)
    {
        return picks;
    }
    Vec::new()
}

/// Without the `indexer` feature there is no entity ANN to query.
#[cfg(not(feature = "indexer"))]
fn entity_suggestions(_vs: Option<&VectorStore>, _query: &str, _top_k: usize) -> Vec<Suggestion> {
    Vec::new()
}

/// Embeds the query text and searches the entity ANN for the nearest
/// entities.
///
/// The score is `1.0 - distance`, the convention `semantic_search` results
/// use. The search caps the result at `top_k`.
#[cfg(feature = "indexer")]
fn suggest_entities(vs: &VectorStore, query: &str, top_k: usize) -> Result<Vec<Suggestion>> {
    use mcpmem_core::jobs::Normalization;
    use mcpmem_indexer::EmbeddingProvider;

    let top_k = top_k.clamp(1, 100);

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

    let texts = [query.to_owned()];
    let vectors = provider
        .embed_texts(&profile, &texts)
        .map_err(|e| MCSError::MemoryError(format!("Embedding the query text failed: {e}")))?;

    // One text goes in, so one vector must come back. Any other count is a
    // provider fault.
    let returned = vectors.len();
    let mut query_vec = vectors
        .into_iter()
        .next()
        .filter(|_| returned == 1)
        .ok_or_else(|| {
            MCSError::MemoryError(format!(
                "The embedding provider returned {returned} vectors for one query text; it must \
                 return exactly one"
            ))
        })?;

    if query_vec.len() != profile.dimensions as usize {
        return Err(MCSError::MemoryError(format!(
            "The embedding provider returned {} dimensions and the serving profile names model \
             '{}' at {} dimensions. The provider and the profile disagree.",
            query_vec.len(),
            profile.model,
            profile.dimensions
        )));
    }
    // Nothing validates a query vector, and cosine distance against an
    // unnormalized query ranks the results wrongly, so normalize it here,
    // exactly as `handle_semantic_search` does.
    if profile.normalization == Normalization::L2 {
        l2_normalize(&mut query_vec);
    }
    let json = vs.search_entities_json(&query_vec, top_k, None)?;
    let parsed: Value = serde_json::from_str(&json).map_err(MCSError::JsonError)?;
    let mut out: Vec<Suggestion> = Vec::new();
    if let Some(rows) = parsed.get("results").and_then(Value::as_array) {
        for row in rows {
            // The store emits the distance under the `score` key; the
            // suggestion score is `1.0 - distance`, so the nearest entity
            // sorts first with the highest score.
            let Some(name) = row.get("name").and_then(Value::as_str) else {
                continue;
            };
            let distance = row.get("score").and_then(Value::as_f64).unwrap_or(1.0);
            out.push(Suggestion {
                name: name.to_owned(),
                score: 1.0 - distance,
            });
        }
    }
    out.truncate(top_k);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use super::*;

    #[test]
    fn fallback_suggests_typo_and_underscore_variants() {
        // "persn" is a typo for "person". The entry with count 0 is invisible,
        // and the exact typo variant must not match itself.
        let existing = vec![
            ("person".into(), 3usize),
            ("persn".into(), 0usize),
            ("project".into(), 2usize),
        ];
        let got = suggest_strings("persn", &existing);
        assert!(got.iter().any(|s| s.name == "person"));
        assert!(got.iter().all(|s| s.name != "persn"));
    }

    #[test]
    fn fallback_orders_by_score_then_name() {
        // "related_to" is closer to "relatedTo" than "relates_to" is, so it
        // must sort first.
        let existing = vec![
            ("relates_to".into(), 1usize),
            ("related_to".into(), 2usize),
        ];
        let got = suggest_strings("relatedTo", &existing);
        assert_eq!(got[0].name, "related_to");
        assert!(
            got.iter()
                .position(|s| s.name == "related_to")
                .unwrap()
                < got.iter().position(|s| s.name == "relates_to").unwrap()
        );

        // Two candidates with an equal score must sort by name ascending.
        let tied = vec![
            ("abcdx".into(), 1usize),
            ("abcde".into(), 1usize),
        ];
        let got = suggest_strings("abc", &tied);
        assert_eq!(got[0].name, "abcde");
    }

    #[test]
    fn fallback_skips_exact_self_match_with_count() {
        // The exact match must be skipped even when its count is non-zero.
        // The lowercase comparison also handles a case-different query.
        let existing = vec![("person".into(), 3usize), ("project".into(), 2usize)];
        let got = suggest_strings("person", &existing);
        assert!(got.iter().all(|s| s.name != "person"));

        let got = suggest_strings("Person", &existing);
        assert!(got.iter().all(|s| s.name != "person"));
    }

    #[test]
    fn fallback_returns_empty_for_garbage() {
        let existing = vec![
            ("person".into(), 3usize),
            ("project".into(), 2usize),
        ];
        let got = suggest_strings("zzzz", &existing);
        assert!(got.is_empty());
    }

    /// A graph over a temporary database, seeded with two entity types and two
    /// relation types. `dir` stays alive for the whole test.
    fn seeded_graph() -> (tempfile::TempDir, crate::kg::GraphHandle) {
        let dir = tempfile::tempdir().unwrap();
        let kg = crate::kg::GraphHandle::new(
            &dir.path().join("memory.db"),
            crate::config::Durability::Sync,
            crate::config::SqliteTuning::default(),
            std::num::NonZeroUsize::new(32).unwrap(),
            2,
        )
        .unwrap();
        crate::actions::memory::handle_create_entities(
            &kg,
            None,
            Some(&json!({"entities":[
                {"name":"a","entityType":"person","observations":[]},
                {"name":"b","entityType":"project","observations":[]}
            ]})),
        )
        .unwrap();
        crate::actions::memory::handle_create_relations(
            &kg,
            None,
            Some(&json!({"relations":[
                {"from":"a","to":"b","relationType":"knows"},
                {"from":"b","to":"a","relationType":"relates_to"}
            ]})),
        )
        .unwrap();
        (dir, kg)
    }

    #[test]
    fn suggest_taxonomy_suggests_existing_type_names() {
        let (_dir, kg) = seeded_graph();

        // A misspelled query gets the established entity type. The kind
        // defaults to entityType when the argument is omitted.
        let value = handle_suggest_taxonomy(
            None,
            &kg,
            Some(&json!({ "query": "persn" })),
        )
        .unwrap();
        let suggestions = &value["suggestions"];
        assert!(suggestions.is_array());
        assert_eq!(suggestions[0]["name"], "person");
        assert!(suggestions[0]["score"].as_f64().unwrap() > 0.0);

        // The relationType kind reads the relation-type names.
        let value = handle_suggest_taxonomy(
            None,
            &kg,
            Some(&json!({ "query": "know_", "kind": "relationType" })),
        )
        .unwrap();
        let suggestions = &value["suggestions"];
        assert!(suggestions.as_array().unwrap().iter().any(|s| s["name"] == "knows"));
    }

    #[test]
    fn suggest_taxonomy_rejects_blank_query_and_unknown_kind() {
        let (_dir, kg) = seeded_graph();

        let Err(err) = handle_suggest_taxonomy(None, &kg, Some(&json!({ "query": "   " })))
            else {
                panic!("expected a blank-query error");
            };
        let err = err.to_string();
        assert!(err.contains("must not be empty or whitespace"), "{err}");

        let Err(err) = handle_suggest_taxonomy(None, &kg, Some(&json!({})))
            else {
                panic!("expected a missing-query error");
            };
        let err = err.to_string();
        assert!(err.contains("Missing 'query'"), "{err}");

        let Err(err) = handle_suggest_taxonomy(
            None,
            &kg,
            Some(&json!({ "query": "persn", "kind": "taxonomy" })),
        )
        else {
            panic!("expected an unknown-kind error");
        };
        let err = err.to_string();
        assert!(err.contains("'kind'"), "{err}");

        // A non-string kind must be rejected, not silently defaulted.
        let Err(err) = handle_suggest_taxonomy(
            None,
            &kg,
            Some(&json!({ "query": "persn", "kind": 7 })),
        )
        else {
            panic!("expected a non-string kind error");
        };
        let err = err.to_string();
        assert!(err.contains("'kind' must be a string"), "{err}");
    }

    #[test]
    fn suggest_taxonomy_entity_and_relation_kinds_need_the_semantic_tier() {
        // Without a vector store the semantic tier cannot run, and entity
        // names and relation names have no offline equivalent: both kinds
        // return an empty list, without an error.
        let (_dir, kg) = seeded_graph();

        let value = handle_suggest_taxonomy(
            None,
            &kg,
            Some(&json!({ "query": "persn", "kind": "entity" })),
        )
        .unwrap();
        assert_eq!(
            value["suggestions"].as_array().map(|a| a.len()),
            Some(0)
        );

        let value = handle_suggest_taxonomy(
            None,
            &kg,
            Some(&json!({ "query": "know_", "kind": "relation" })),
        )
        .unwrap();
        assert_eq!(
            value["suggestions"].as_array().map(|a| a.len()),
            Some(0)
        );
    }

    #[test]
    fn suggest_taxonomy_clamps_top_k_to_the_allowed_range() {
        let (_dir, kg) = seeded_graph();

        // Two candidates qualify for "pers"; a topK of 1 keeps the best one
        // and a topK of 0 clamps to 1 instead of returning an empty list.
        let value = handle_suggest_taxonomy(
            None,
            &kg,
            Some(&json!({ "query": "pers", "kind": "entityType", "topK": 1 })),
        )
        .unwrap();
        assert_eq!(
            value["suggestions"].as_array().map(|a| a.len()),
            Some(1)
        );

        let value = handle_suggest_taxonomy(
            None,
            &kg,
            Some(&json!({ "query": "pers", "kind": "entityType", "topK": 0 })),
        )
        .unwrap();
        assert_eq!(
            value["suggestions"].as_array().map(|a| a.len()),
            Some(1)
        );
    }

    #[cfg(feature = "indexer")]
    mod semantic {
        use super::*;
        use crate::config::{Durability, SqliteTuning};
        use crate::kg::GraphHandle;
        use crate::types::EntityInput as Entity;
        use crate::vector_store::VectorStore;
        use mcpmem_core::jobs::{DistanceMetric, IndexProfile, IndexProfileRegistry, Normalization};
        use mcpmem_indexer::OpenAiCompatibleProvider;
        use parking_lot::Mutex;
        use rusqlite::params;
        use serde_json::json;
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::num::NonZeroUsize;
        use std::sync::Arc;
        use std::time::Duration;

        /// One `/v1/embeddings` request the fake server recorded.
        ///
        /// The recorded batch is the input array exactly as the provider sent
        /// it, so an assertion on it proves how many texts one call carried.
        #[derive(Debug, Clone, PartialEq, Eq)]
        struct RecordedCall {
            texts: Vec<String>,
        }

        /// The shared state of the fake embeddings server.
        struct FakeState {
            calls: Mutex<Vec<RecordedCall>>,
        }

        /// A fake OpenAI-compatible embeddings endpoint on a loopback port.
        ///
        /// [`crate::indexer_provider`] holds one [`ProviderRegistry`], and a
        /// registry can hold only the concrete Ollama and OpenAI providers,
        /// both plain HTTP clients. A trait-level stub can never reach
        /// [`suggest_semantic`], so the stub must speak HTTP itself. The
        /// server returns all-ones vectors of the requested dimension and
        /// records each request. One server lives for the whole test binary;
        /// every semantic test shares it through the process-wide registry.
        struct FakeEmbeddings {
            url: String,
            state: Arc<FakeState>,
        }

        /// One loopback embeddings server for the whole test binary.
        static FAKE: std::sync::LazyLock<Arc<FakeEmbeddings>> = std::sync::LazyLock::new(|| {
            Arc::new(FakeEmbeddings::start())
        });

        fn fake_embeddings() -> &'static FakeEmbeddings {
            FAKE.as_ref()
        }

        impl FakeEmbeddings {
            fn start() -> Self {
                let listener = TcpListener::bind("127.0.0.1:0")
                    .expect("bind the fake embeddings listener");
                let addr = listener
                    .local_addr()
                    .expect("read back the fake embeddings port");
                let state = Arc::new(FakeState { calls: Mutex::new(Vec::new()) });
                let thread_state = Arc::clone(&state);
                let _ = std::thread::Builder::new()
                    .name("taxonomy-semantic-fake".into())
                    .spawn(move || {
                        loop {
                            // A dying client is an accept or read failure,
                            // never a server fault; keep serving.
                            let Some((conn, _peer)) = listener.accept().ok() else {
                                continue;
                            };
                            handle_connection(conn, Arc::clone(&thread_state));
                        }
                    });
                Self {
                    url: format!("http://{addr}/v1/embeddings"),
                    state,
                }
            }

            /// Every request the server has seen so far, in arrival order.
            fn recorded(&self) -> Vec<RecordedCall> {
                (self.state.calls.lock()).clone()
            }
        }

        /// Installs the fake server into the process-wide registry cell.
        ///
        /// The cell accepts one value for the lifetime of the process, and
        /// every test registers the same fake server, so whichever call wins
        /// the race the registry is equivalent.
        fn install_fake_provider() {
            let fake = fake_embeddings();
            let provider = OpenAiCompatibleProvider::new(
                fake.url.clone(),
                "test-key".into(),
                Duration::from_secs(5),
            )
            .expect("make an OpenAI provider without a request");
            crate::indexer_provider::init(Arc::new(mcpmem_indexer::ProviderRegistry::new(
                None,
                Some(Arc::new(provider)),
            )));
        }

        /// Serves one connection: read the whole request, record it, answer
        /// with fixed embeddings, then close.
        fn handle_connection(mut conn: TcpStream, state: Arc<FakeState>) {
            let mut buf = Vec::new();
            let mut chunk = vec![0u8; 8192];
            loop {
                let read = conn.read(&mut chunk);
                if read.is_err() {
                    return;
                }
                let n = read.unwrap();
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some((headers_end, body_len)) = request_frame(&buf) {
                    // The body follows the blank-line separator.
                    let body_start = headers_end + 4;
                    if buf.len() >= body_start + body_len {
                        respond(&mut conn, &buf[body_start..body_start + body_len], Arc::clone(&state));
                        return;
                    }
                }
            }
        }

        /// The header-block end and the `Content-Length` of a request.
        ///
        /// Returns None until the full header block is buffered.
        fn request_frame(buf: &[u8]) -> Option<(usize, usize)> {
            let text = String::from_utf8_lossy(buf).to_string();
            let Some(headers_end) = text.find("\r\n\r\n") else {
                return None;
            };
            let mut length: usize = 0;
            for line in text[..headers_end].lines() {
                let Some(colon) = line.find(':') else {
                    continue;
                };
                if line[..colon].trim().eq_ignore_ascii_case("content-length") {
                    length = line[colon + 1..].trim().parse::<usize>().ok().unwrap_or(0);
                }
            }
            Some((headers_end, length))
        }

        /// Records the request and answers with all-ones embeddings.
        ///
        /// Two sentinel texts drive the fault paths: `WRONG_DIMENSIONS` makes
        /// the server return half-width vectors and `WRONG_COUNT` makes it
        /// return a single vector. Both faults are provider behavior the
        /// engine must detect. Each request produces the same fault.
        fn respond(conn: &mut TcpStream, body: &[u8], state: Arc<FakeState>) {
            let text = String::from_utf8_lossy(body).to_string();
            let Some(value) = serde_json::from_str::<serde_json::Value>(text.as_str()).ok()
            else {
                return;
            };
            let input: Vec<String> = value["input"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .map(|row| row.as_str().expect("an input text").to_owned())
                        .collect()
                })
                .unwrap_or(Vec::new());
            let dimensions: usize = value["dimensions"].as_u64().map(|n| n as usize).unwrap_or(0);
            state.calls.lock().extend_from_slice(&[RecordedCall { texts: input.clone() }]);

            let wrong_dimensions = input.first().is_some_and(|text| *text == "WRONG_DIMENSIONS");
            let wrong_count = input.first().is_some_and(|text| *text == "WRONG_COUNT");
            let count = if wrong_count {
                input.len() + 1
            } else {
                input.len()
            };
            let dims = if wrong_dimensions {
                (dimensions / 2).max(1)
            } else {
                dimensions
            };
            let data: Vec<_> = (0..count)
                .map(|_| json!({"embedding": vec![1.0; dims]}))
                .collect();
            let payload = json!({"data": data}).to_string();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = conn.write_all(head.as_bytes());
            let _ = conn.write_all(payload.as_bytes());
        }

        /// A temporary store over a fresh database, with the fake provider
        /// installed. All vectors the server returns are all-ones, so tests
        /// seed snapshot vectors and derive exact distances from them.
        struct Env {
            // Held alive so the store's database file stays on disk.
            _dir: tempfile::TempDir,
            kg: GraphHandle,
            vs: VectorStore,
        }

        fn env(dims: u32) -> Env {
            install_fake_provider();
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("memory.db");
            let kg = GraphHandle::new(
                &db_path,
                Durability::Async,
                SqliteTuning::default(),
                NonZeroUsize::new(10000).unwrap(),
                4,
            )
            .unwrap();
            let vs = VectorStore::new(&db_path, dims).unwrap();
            Env { _dir: dir, kg, vs }
        }

        fn create_test_entity(kg: &GraphHandle, name: &str, etype: &str) {
            kg.create_entities(&[Entity {
                name: name.into(),
                entity_type: etype.into(),
                observations: vec!["test observation".into()],
            }])
            .unwrap();
        }

        /// Registers a serving profile for the store and returns its id.
        ///
        /// The registry rows are inserted directly, the way a completed
        /// rebuild would leave them; the entity index is not rebuilt, because
        /// only the taxonomy snapshots matter here.
        fn seed_profile(env: &Env, dims: u32, normalization: Normalization) -> uuid::Uuid {
            let profile = IndexProfile {
                id: uuid::Uuid::new_v4(),
                store_key: "default".into(),
                provider_kind: "openai".into(),
                model: "test-model".into(),
                dimensions: dims,
                representation_version: "v1".into(),
                normalization,
                distance_metric: DistanceMetric::L2Squared,
                vector_encoding_version: "f32le-v1".into(),
            };
            let conn = env.vs.db.lock();
            conn.execute(
                "INSERT INTO index_profile VALUES(?1,'default',?2,?3,'Active')",
                params![
                    profile.id.to_string(),
                    "taxonomy-semantic-fixture",
                    serde_json::to_string(&profile).unwrap()
                ],
            )
            .unwrap();
            conn.execute(
                "UPDATE index_profile_registry SET state='Active',serving_profile=?1 WHERE store_key='default'",
                [profile.id.to_string()],
            )
            .unwrap();
            profile.id
        }

        fn seed_generation(env: &Env, profile: uuid::Uuid, kind: i64, durable: i64) {
            let conn = env.vs.db.lock();
            conn.execute(
                "INSERT INTO taxonomy_ann_generation(profile_id,subject_kind,durable_generation) VALUES(?1,?2,?3)",
                params![profile.to_string(), kind, durable],
            )
            .unwrap();
        }

        fn seed_vector(env: &Env, profile: uuid::Uuid, kind: i64, id: i64, embedding: &[f32]) {
            let bytes: Vec<u8> = embedding
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect();
            let conn = env.vs.db.lock();
            conn.execute(
                "INSERT INTO taxonomy_vector VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(profile_id,subject_kind,subject_id) DO UPDATE SET subject_revision=excluded.subject_revision,blob=excluded.blob,created_at_us=excluded.created_at_us,source=excluded.source",
                params![profile.to_string(), kind, id, 1i64, bytes, 1i64, "test"],
            )
            .unwrap();
        }

        fn seed_type(env: &Env, id: i64, kind: i64, name: &str) {
            let conn = env.vs.db.lock();
            conn.execute(
                "INSERT INTO type_dict(id,kind,name,count,revision) VALUES(?1,?2,?3,1,1)",
                params![id, kind, name],
            )
            .unwrap();
        }

        /// The type-dict id of a live type row, so snapshot seeding points a
        /// vector at a subject the graph really owns.
        fn type_id(env: &Env, kind: i64, name: &str) -> i64 {
            let conn = env.vs.db.lock();
            conn.query_row(
                "SELECT id FROM type_dict WHERE kind=?1 AND name=?2",
                params![kind, name],
                |row| row.get(0),
            )
            .unwrap()
        }

        /// Builds the per-kind snapshots for the seeded profile, the way the
        /// indexer worker does after a committed batch.
        fn adopt(env: &mut Env) -> Result<()> {
            let conn = rusqlite::Connection::open(&env.vs.db_path).unwrap();
            let registry = IndexProfileRegistry::new(&conn);
            env.vs.adopt_taxonomy(&registry, false)
        }

        #[test]
        fn batches_all_texts_into_one_provider_call() {
            let mut env = env(4);
            let profile = seed_profile(&env, 4, Normalization::None);
            seed_type(&env, 7, 0, "person");
            seed_generation(&env, profile, 0, 1);
            seed_vector(&env, profile, 0, 7, &[1.0; 4]);
            adopt(&mut env).unwrap();

            let texts: Vec<String> = vec!["alpha".into(), "beta".into(), "gamma".into()];
            let got = suggest_semantic(&env.vs, &texts, SubjectKind::EntityType, 2).unwrap();

            // One search per text: the single seeded subject comes back for
            // each of the three texts, grouped by input text.
            assert_eq!(got.len(), 3);
            assert!(got.iter().all(|group| group.len() == 1 && group[0].name == "person"));

            // And the three texts rode one embed_texts call.
            let calls = fake_embeddings().recorded();
            let matches = calls.iter().filter(|call| call.texts == texts).count();
            assert!(
                matches == 1,
                "one embed_texts call must carry the whole batch; calls: {calls:?}"
            );
        }

        #[test]
        fn maps_distances_to_scores() {
            let mut env = env(4);
            let profile = seed_profile(&env, 4, Normalization::None);
            seed_type(&env, 7, 0, "person");
            seed_type(&env, 8, 0, "organization");
            seed_type(&env, 9, 0, "acme");
            seed_generation(&env, profile, 0, 1);
            seed_vector(&env, profile, 0, 7, &[1.0; 4]);
            seed_vector(&env, profile, 0, 8, &[2.0, 1.0, 1.0, 1.0]);
            seed_vector(&env, profile, 0, 9, &[1.0, 1.0, 1.0, 2.0]);
            adopt(&mut env).unwrap();

            let query = "query".to_owned();
            let got = suggest_semantic(&env.vs, &[query], SubjectKind::EntityType, 10).unwrap();
            let group = &got[0];

            // The provider returns all-ones, so the L2Squared distances are
            // exactly 0.0, 1.0 and 1.0, and the scores are 1 - distance.
            assert_eq!(group[0].name, "person");
            assert_eq!(group[0].score, 1.0);
            assert_eq!(group[1].name, "organization");
            assert_eq!(group[1].score, 0.0);
            assert_eq!(group[2].name, "acme");
            assert_eq!(group[2].score, 0.0);
        }

        #[test]
        fn routes_the_subject_kind_through_the_numeric_contract() {
            let mut env = env(4);
            let profile = seed_profile(&env, 4, Normalization::None);
            seed_type(&env, 7, 0, "person");
            seed_type(&env, 8, 1, "works_at");
            create_test_entity(&env.kg, "alice", "person");
            create_test_entity(&env.kg, "acme", "organization");
            let alice = env.vs.entity_id_of("alice").unwrap().unwrap();
            let acme = env.vs.entity_id_of("acme").unwrap().unwrap();
            {
                let conn = env.vs.db.lock();
                conn.execute(
                    "INSERT INTO taxonomy_relation(id,from_id,to_id,type_id,revision,deleted) VALUES(42,?1,?2,?3,1,0)",
                    params![alice, acme, 8],
                )
                .unwrap();
            }
            for (kind, id) in [(0, 7), (1, 8), (2, 42)] {
                seed_generation(&env, profile, kind, 1);
                seed_vector(&env, profile, kind, id, &[1.0; 4]);
            }
            adopt(&mut env).unwrap();

            let entity_types = suggest_semantic(
                &env.vs,
                &["query".into()],
                SubjectKind::EntityType,
                10,
            )
            .unwrap();
            assert_eq!(entity_types.len(), 1);
            assert_eq!(entity_types[0][0].name, "person");

            let relation_types = suggest_semantic(
                &env.vs,
                &["query".into()],
                SubjectKind::RelationType,
                10,
            )
            .unwrap();
            assert_eq!(relation_types.len(), 1);
            assert_eq!(relation_types[0][0].name, "works_at");

            let relations = suggest_semantic(
                &env.vs,
                &["query".into()],
                SubjectKind::Relation,
                10,
            )
            .unwrap();
            assert_eq!(relations.len(), 1);
            assert_eq!(relations[0][0].name, "alice -[works_at]-> acme");
        }

        #[test]
        fn skips_rows_whose_subject_was_deleted() {
            let mut env = env(4);
            let profile = seed_profile(&env, 4, Normalization::None);
            seed_type(&env, 7, 0, "person");
            seed_generation(&env, profile, 0, 1);
            seed_vector(&env, profile, 0, 7, &[1.0; 4]);
            // Subject 999 has a vector but no relation row: the snapshot
            // keeps serving it until the next rebuild, and the engine must
            // drop it rather than surface a name that no longer resolves.
            seed_generation(&env, profile, 2, 1);
            seed_vector(&env, profile, 2, 999, &[1.0; 4]);
            adopt(&mut env).unwrap();

            let relations = suggest_semantic(
                &env.vs,
                &["query".into()],
                SubjectKind::Relation,
                10,
            )
            .unwrap();
            assert!(relations[0].is_empty(), "a dangling vector must be skipped: {relations:?}");

            let entity_types = suggest_semantic(
                &env.vs,
                &["query".into()],
                SubjectKind::EntityType,
                10,
            )
            .unwrap();
            assert_eq!(entity_types[0][0].name, "person");
        }

        #[test]
        fn returns_empty_when_the_snapshot_is_absent() {
            let env = env(4);
            let _ = seed_profile(&env, 4, Normalization::None);
            // A serving profile with no adopted snapshot for the kind: the
            // engine must fall back to the offline tier, not fail.
            let got = suggest_semantic(&env.vs, &["query".into()], SubjectKind::EntityType, 10)
                .unwrap();
            assert!(got[0].is_empty());
        }

        #[test]
        fn rejects_wrong_dimensions_from_the_provider() {
            let env = env(4);
            let _ = seed_profile(&env, 4, Normalization::None);
            let Err(err) = suggest_semantic(
                &env.vs,
                &["WRONG_DIMENSIONS".into()],
                SubjectKind::EntityType,
                10,
            )
            else {
                panic!("expected a dimension-mismatch error");
            };
            let text = err.to_string();
            assert!(text.contains("dimensions") && text.contains("disagree"), "{text}");
        }

        #[test]
        fn rejects_a_wrong_vector_count_from_the_provider() {
            let env = env(4);
            let _ = seed_profile(&env, 4, Normalization::None);
            let Err(err) = suggest_semantic(
                &env.vs,
                &["WRONG_COUNT".into()],
                SubjectKind::EntityType,
                10,
            )
            else {
                panic!("expected a count-mismatch error");
            };
            let text = err.to_string();
            assert!(text.contains("one vector per text"), "{text}");
        }

        #[test]
        fn normalizes_the_query_when_the_profile_asks_for_l2() {
            let mut env = env(4);
            let profile = seed_profile(&env, 4, Normalization::L2);
            seed_type(&env, 7, 0, "person");
            seed_generation(&env, profile, 0, 1);
            seed_vector(&env, profile, 0, 7, &[0.5; 4]);
            adopt(&mut env).unwrap();

            let got = suggest_semantic(&env.vs, &["query".into()], SubjectKind::EntityType, 10)
                .unwrap();
            // The provider returns all-ones. L2 normalization scales it to
            // 0.5 per component, which exactly matches the seeded unit
            // vector; without normalization the distance would be 1.0 and the
            // score would be 0.0.
            assert_eq!(got[0][0].name, "person");
            assert_eq!(got[0][0].score, 1.0);
        }

        #[test]
        fn without_a_serving_profile_names_the_indexer_section() {
            let env = env(4);
            let Err(err) = suggest_semantic(&env.vs, &["query".into()], SubjectKind::EntityType, 5)
            else {
                panic!("expected a missing-profile error");
            };
            let text = err.to_string();
            assert!(text.contains("[indexer]") && text.contains("no index profile"), "{text}");
        }

        #[test]
        fn suggest_taxonomy_entity_type_prefers_semantic_hits_then_offline_fill() {
            let mut env = env(4);
            // "person" carries a snapshot vector (semantic hit) while
            // "persons" has none, so only the offline tier can find it. The
            // offline scores alone would rank "persons" first for "persn";
            // the semantic tier must override that order.
            create_test_entity(&env.kg, "person one", "person");
            create_test_entity(&env.kg, "persons alpha", "persons");
            let profile = seed_profile(&env, 4, Normalization::None);
            seed_generation(&env, profile, 0, 1);
            seed_vector(&env, profile, 0, type_id(&env, 0, "person"), &[1.0; 4]);
            adopt(&mut env).unwrap();

            let value = handle_suggest_taxonomy(
                Some(&env.vs),
                &env.kg,
                Some(&json!({ "query": "persn", "kind": "entityType" })),
            )
            .unwrap();
            let suggestions = value["suggestions"].as_array().unwrap();
            assert_eq!(
                suggestions[0]["name"], "person",
                "the semantic hit must sort first: {suggestions:?}"
            );
            assert_eq!(suggestions[0]["score"], 1.0);
            assert_eq!(
                suggestions.iter().filter(|s| s["name"] == "person").count(),
                1,
                "the offline copy of a semantic hit must be skipped: {suggestions:?}"
            );
            assert!(
                suggestions.iter().any(|s| s["name"] == "persons"),
                "the offline tier must fill the remaining slots: {suggestions:?}"
            );
        }

        #[test]
        fn suggest_taxonomy_falls_back_to_offline_without_a_profile() {
            let env = env(4);
            create_test_entity(&env.kg, "person one", "person");
            create_test_entity(&env.kg, "persons alpha", "persons");
            // No profile and no snapshot: the tool must answer from the
            // offline tier and never call the provider.
            let calls_before = fake_embeddings().recorded().len();
            let value = handle_suggest_taxonomy(
                Some(&env.vs),
                &env.kg,
                Some(&json!({ "query": "persn", "kind": "entityType" })),
            )
            .unwrap();
            let suggestions = value["suggestions"].as_array().unwrap();
            assert!(
                suggestions.iter().any(|s| s["name"] == "person"),
                "the offline tier must answer: {suggestions:?}"
            );
            assert_eq!(
                fake_embeddings().recorded().len(),
                calls_before,
                "no provider call may happen without a serving profile"
            );
        }

        #[test]
        fn suggest_taxonomy_entity_kind_queries_the_entity_ann() {
            let env = env(4);
            // The entity ANN is populated before the profile exists, because
            // a serving profile turns off direct vector writes.
            create_test_entity(&env.kg, "alice", "person");
            create_test_entity(&env.kg, "acme", "organization");
            env.vs.upsert_embedding("alice", &[1.0; 4], "test").unwrap();
            env.vs
                .upsert_embedding("acme", &[1.0, 0.0, 0.0, 0.0], "test")
                .unwrap();
            let _ = seed_profile(&env, 4, Normalization::None);

            // The provider returns all-ones, so alice sits at distance 0
            // (score 1.0) and acme, which points elsewhere than the query,
            // ranks second with a lower score.
            let value = handle_suggest_taxonomy(
                Some(&env.vs),
                &env.kg,
                Some(&json!({ "query": "who works here", "kind": "entity" })),
            )
            .unwrap();
            let suggestions = value["suggestions"].as_array().unwrap();
            assert_eq!(suggestions[0]["name"], "alice");
            assert_eq!(suggestions[0]["score"], 1.0);
            assert_eq!(
                suggestions.iter().map(|s| s["name"].as_str().unwrap()).collect::<Vec<_>>(),
                vec!["alice", "acme"]
            );
            assert!(
                suggestions[1]["score"].as_f64().unwrap() < 1.0,
                "a non-identical entity must score below the exact match: {suggestions:?}"
            );
        }

        #[test]
        fn suggest_taxonomy_relation_kind_queries_the_kind_two_snapshot() {
            let mut env = env(4);
            let profile = seed_profile(&env, 4, Normalization::None);
            seed_type(&env, 7, 0, "person");
            seed_type(&env, 8, 1, "works_at");
            create_test_entity(&env.kg, "alice", "person");
            create_test_entity(&env.kg, "acme", "organization");
            let alice = env.vs.entity_id_of("alice").unwrap().unwrap();
            let acme = env.vs.entity_id_of("acme").unwrap().unwrap();
            {
                let conn = env.vs.db.lock();
                conn.execute(
                    "INSERT INTO taxonomy_relation(id,from_id,to_id,type_id,revision,deleted) VALUES(42,?1,?2,?3,1,0)",
                    params![alice, acme, 8],
                )
                .unwrap();
            }
            // Only the kind-2 snapshot holds a vector, so the suggestion can
            // come from nowhere else.
            seed_generation(&env, profile, 2, 1);
            seed_vector(&env, profile, 2, 42, &[1.0; 4]);
            adopt(&mut env).unwrap();

            let value = handle_suggest_taxonomy(
                Some(&env.vs),
                &env.kg,
                Some(&json!({ "query": "knows-ish", "kind": "relation" })),
            )
            .unwrap();
            let suggestions = value["suggestions"].as_array().unwrap();
            assert_eq!(suggestions[0]["name"], "alice -[works_at]-> acme");
            assert_eq!(suggestions[0]["score"], 1.0);
        }

        #[test]
        fn suggest_taxonomy_never_errors_when_the_semantic_path_is_unavailable() {
            // A store with no serving profile: entity and relation have no
            // offline tier, so they must return empty lists, never errors.
            let env = env(4);
            for kind in ["entity", "relation"] {
                let value = handle_suggest_taxonomy(
                    Some(&env.vs),
                    &env.kg,
                    Some(&json!({ "query": "query", "kind": kind })),
                )
                .unwrap();
                assert_eq!(
                    value["suggestions"].as_array().map(|a| a.len()),
                    Some(0),
                    "kind {kind} without a profile must be empty"
                );
            }
        }
    }
}