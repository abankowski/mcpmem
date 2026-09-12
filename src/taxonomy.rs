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
/// For kind `entityType` and `relationType` the offline tier returns the type
/// names kept by the string engine. For kind `entity` and `relation` it
/// returns an empty list until the semantic tier lands in a later change.
/// The result is a JSON object of the shape `{ "suggestions": [...] }`.
pub fn handle_suggest_taxonomy(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
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

    let mut suggestions: Vec<Suggestion> = match kind {
        "entityType" => {
            let counts = kg.entity_type_counts();
            let existing: Vec<(&str, usize)> = counts
                .iter()
                .map(|(n, c)| (n.as_str(), *c))
                .collect();
            suggest_strings(query, &existing)
        }
        "relationType" => {
            let counts = kg.relation_type_counts();
            let existing: Vec<(&str, usize)> = counts
                .iter()
                .map(|(n, c)| (n.as_str(), *c))
                .collect();
            suggest_strings(query, &existing)
        }
        // The semantic tier fills these kinds in a later change.
        _ => Vec::new(),
    };
    suggestions.truncate(top_k);

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
            Some(&json!({"entities":[
                {"name":"a","entityType":"person","observations":[]},
                {"name":"b","entityType":"project","observations":[]}
            ]})),
        )
        .unwrap();
        crate::actions::memory::handle_create_relations(
            &kg,
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

        let Err(err) = handle_suggest_taxonomy(&kg, Some(&json!({ "query": "   " })))
            else {
                panic!("expected a blank-query error");
            };
        let err = err.to_string();
        assert!(err.contains("must not be empty or whitespace"), "{err}");

        let Err(err) = handle_suggest_taxonomy(&kg, Some(&json!({})))
            else {
                panic!("expected a missing-query error");
            };
        let err = err.to_string();
        assert!(err.contains("Missing 'query'"), "{err}");

        let Err(err) = handle_suggest_taxonomy(
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
    fn suggest_taxonomy_entity_and_relation_kinds_are_empty_until_semantic_tier() {
        let (_dir, kg) = seeded_graph();

        let value = handle_suggest_taxonomy(
            &kg,
            Some(&json!({ "query": "persn", "kind": "entity" })),
        )
        .unwrap();
        assert_eq!(
            value["suggestions"].as_array().map(|a| a.len()),
            Some(0)
        );

        let value = handle_suggest_taxonomy(
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
            &kg,
            Some(&json!({ "query": "pers", "kind": "entityType", "topK": 1 })),
        )
        .unwrap();
        assert_eq!(
            value["suggestions"].as_array().map(|a| a.len()),
            Some(1)
        );

        let value = handle_suggest_taxonomy(
            &kg,
            Some(&json!({ "query": "pers", "kind": "entityType", "topK": 0 })),
        )
        .unwrap();
        assert_eq!(
            value["suggestions"].as_array().map(|a| a.len()),
            Some(1)
        );
    }
}