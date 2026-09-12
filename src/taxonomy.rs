//! Suggests taxonomy names for a new name.
//!
//! The engine compares a new name with the existing names. Each candidate gets
//! a score from trigram overlap and Levenshtein distance. A candidate passes
//! only when its score is above a threshold. This module is pure: it takes
//! names and counts, and it returns scored suggestions. It does no I/O.

use serde::Serialize;

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

#[cfg(test)]
mod tests {
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
    fn fallback_returns_empty_for_garbage() {
        let existing = vec![
            ("person".into(), 3usize),
            ("project".into(), 2usize),
        ];
        let got = suggest_strings("zzzz", &existing);
        assert!(got.is_empty());
    }
}