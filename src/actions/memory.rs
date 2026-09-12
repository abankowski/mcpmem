use std::collections::{HashMap, HashSet};

use mcpmem_core::mutation::{
    MutationContext, MutationRequest, MutationResult, MutationService, ObservationUpdate,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::errors::{MCSError, Result};
use crate::kg::{GraphHandle, push_json_str};
use crate::taxonomy::{SubjectKind, Suggestion, suggest_strings};
use crate::vector_store::VectorStore;

const MAX_NAME_BYTES: usize = 1024;
const MAX_OBSERVATION_BYTES: usize = 65536;
const MAX_ENTITIES_PER_REQUEST: usize = 1000;
const MAX_RELATIONS_PER_REQUEST: usize = 1000;
const MAX_OBSERVATIONS_PER_ENTITY: usize = 1000;
const MAX_NEIGHBOR_DEPTH: usize = 16;
const MAX_NAMES_PER_REQUEST: usize = 1000;
const MAX_SEARCH_LIMIT: usize = 1000;
const MAX_RELATION_SEARCH_RESULTS: usize = 1000;
const MAX_FIND_ALL_PATHS_DEPTH: usize = 10;
const MAX_FIND_ALL_PATHS_RESULTS: usize = 100;
/// Upper bound on rows returned per array by `export_graph`. A guard against an
/// unbounded in-memory JSON string, not a functional limit — realistic graphs
/// are far smaller.
const MAX_EXPORT_ROWS: i64 = 1_000_000;
/// Default page size for `search_nodes` when the caller omits `limit`.
const DEFAULT_SEARCH_LIMIT: usize = 20;
/// Upper bound on the example entities or relations attached to one
/// suggestion payload.
const MAX_TAXONOMY_EXAMPLES: usize = 3;
/// `topK` for the semantic tier of the write hook, matching the
/// `suggest_taxonomy` default.
#[cfg(feature = "indexer")]
const SEMANTIC_SUGGESTION_K: usize = 10;

/// The `taxonomySuggestions` payload for one result object.
///
/// `similarTypes` lists the similar existing type names. The semantic tier's
/// results sort first; the offline string engine fills the remaining slots.
/// An unknown entity type also gets up to [`MAX_TAXONOMY_EXAMPLES`]
/// `exampleEntities` from the top similar types, and an unknown relation
/// type gets the equivalent `exampleRelations` instead.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TypeSuggestions {
    similar_types: Vec<Suggestion>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    example_entities: Vec<ExampleEntity>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    example_relations: Vec<ExampleRelation>,
}

/// One example entity of a similar type.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExampleEntity {
    name: String,
    entity_type: String,
}

/// One example relation of a similar relation type.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExampleRelation {
    from: String,
    relation_type: String,
    to: String,
}

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

fn validate_observation(content: &str) -> Result<()> {
    if content.len() > MAX_OBSERVATION_BYTES {
        return Err(MCSError::InvalidParams(format!(
            "Observation too long (max {MAX_OBSERVATION_BYTES} bytes)"
        )));
    }
    Ok(())
}

macro_rules! text_content {
    ($text:expr) => {
        json!({
            "content": [{
                "type": "text",
                "text": $text
            }]
        })
    };
}

fn build_content_response(inner_json: &str) -> String {
    let mut out = String::with_capacity(64 + inner_json.len() + (inner_json.len() / 8));
    out.push_str(r#"{"content":[{"type":"text","text":"#);
    push_json_str(&mut out, inner_json);
    out.push_str(r#"}]}"#);
    out
}

fn apply_mutation(kg: &GraphHandle, request: MutationRequest) -> Result<MutationResult> {
    MutationService::new(kg)
        .apply_with_result(request, MutationContext::local())
        .map(|(_, result)| result)
}

/// Adds "taxonomySuggestions" to `value`, whose authored type was unknown
/// before the write, and returns it. The suggestion payload is computed once
/// per distinct authored type by [`suggestion_map`], never per result object.
fn enrich_result_object(mut value: Value, candidates: &TypeSuggestions) -> Value {
    value
        .as_object_mut()
        .expect("a result object is a JSON object")
        .insert("taxonomySuggestions".into(), json!(candidates));
    value
}

/// Runs the suggestion engine once per distinct authored type.
///
/// One counts query runs per kind. `types` is the set of authored types that
/// were unknown before the write; the write itself inserts the authored type
/// row, so existence must be captured before the mutation and the engine must
/// skip the exact self-match (it does).
///
/// The semantic tier runs first when `vs` is usable: one provider call
/// embeds every unknown type text of the kind, and each text searches the
/// kind's ANN snapshot. The offline string engine then fills the remaining
/// slots, skipping names the semantic tier already returned. Every tier
/// failure falls back to the offline tier, because the hook is advisory only
/// and must never fail the write.
fn suggestion_map<'a>(
    kg: &GraphHandle,
    types: impl Iterator<Item = &'a str>,
    kind: SubjectKind,
    vs: Option<&VectorStore>,
) -> HashMap<String, TypeSuggestions> {
    let mut types: Vec<&str> = types.collect();
    types.sort_unstable();
    types.dedup();
    if types.is_empty() {
        return HashMap::new();
    }
    let counts = match kind {
        SubjectKind::EntityType => kg.entity_type_counts(),
        SubjectKind::RelationType => kg.relation_type_counts(),
        SubjectKind::Relation => Vec::new(),
    };
    let existing: Vec<(&str, usize)> = counts.iter().map(|(n, c)| (n.as_str(), *c)).collect();
    let offline: HashMap<String, Vec<Suggestion>> = types
        .iter()
        .map(|authored_type| {
            (
                (*authored_type).to_string(),
                suggest_strings(authored_type, &existing),
            )
        })
        .collect();
    let semantic = semantic_suggestions(vs, &types, kind);
    types
        .into_iter()
        .map(|authored_type| {
            let mut similar = semantic.get(authored_type).cloned().unwrap_or_default();
            // The offline tier fills the remaining slots: every name the
            // semantic tier already returned is skipped, so a name appears
            // once with its semantic score.
            for suggestion in offline.get(authored_type).cloned().unwrap_or_default() {
                if !similar.iter().any(|s| s.name == suggestion.name) {
                    similar.push(suggestion);
                }
            }
            let (example_entities, example_relations) = match kind {
                SubjectKind::EntityType => (example_entities(kg, &similar), Vec::new()),
                SubjectKind::RelationType => (Vec::new(), example_relations(kg, &similar)),
                SubjectKind::Relation => (Vec::new(), Vec::new()),
            };
            (
                authored_type.to_string(),
                TypeSuggestions {
                    similar_types: similar,
                    example_entities,
                    example_relations,
                },
            )
        })
        .collect()
}

/// Runs the semantic tier for the unknown types, or returns an empty map.
///
/// The tier is active only when the `indexer` feature is compiled, a vector
/// store exists, the store serves an index profile, and an embedding
/// provider is configured. One provider call embeds the whole batch of
/// unknown type texts of one kind. Every failure — a missing profile, a
/// missing provider, an embed error, a wrong vector count — yields an empty
/// map, so the write falls back to the offline tier and never fails.
#[cfg(feature = "indexer")]
fn semantic_suggestions(
    vs: Option<&VectorStore>,
    types: &[&str],
    kind: SubjectKind,
) -> HashMap<String, Vec<Suggestion>> {
    let mut out: HashMap<String, Vec<Suggestion>> = HashMap::new();
    let Some(store) = vs else {
        return out;
    };
    if crate::indexer_provider::get().is_none() {
        return out;
    }
    if store.serving_profile().ok().flatten().is_none() {
        return out;
    }
    let texts: Vec<String> = types.iter().map(|s| (*s).to_string()).collect();
    let Ok(groups) = crate::taxonomy::suggest_semantic(store, &texts, kind, SEMANTIC_SUGGESTION_K)
    else {
        return out;
    };
    for (authored_type, group) in types.iter().zip(groups) {
        out.insert((*authored_type).to_string(), group);
    }
    out
}

/// Without the `indexer` feature there is no semantic tier; the hook runs
/// the offline tier only.
#[cfg(not(feature = "indexer"))]
fn semantic_suggestions(
    _vs: Option<&VectorStore>,
    _types: &[&str],
    _kind: SubjectKind,
) -> HashMap<String, Vec<Suggestion>> {
    HashMap::new()
}

/// Collects up to [`MAX_TAXONOMY_EXAMPLES`] example entities of the top
/// similar types, in similar-type order.
///
/// Each similar type is searched by name with the `search_nodes` semantics;
/// entities are deduplicated by name and type, and the first unique rows
/// win.
fn example_entities(kg: &GraphHandle, similar: &[Suggestion]) -> Vec<ExampleEntity> {
    let mut out: Vec<ExampleEntity> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for suggestion in similar {
        if out.len() >= MAX_TAXONOMY_EXAMPLES {
            break;
        }
        for entity in kg.search_nodes_filtered(&suggestion.name, None, 0, MAX_TAXONOMY_EXAMPLES) {
            if seen.insert((entity.name.clone(), entity.entity_type.clone())) {
                out.push(ExampleEntity {
                    name: entity.name,
                    entity_type: entity.entity_type,
                });
            }
            if out.len() >= MAX_TAXONOMY_EXAMPLES {
                break;
            }
        }
    }
    out
}

/// Collects up to [`MAX_TAXONOMY_EXAMPLES`] example relations of the top
/// similar relation types, in similar-type order.
fn example_relations(kg: &GraphHandle, similar: &[Suggestion]) -> Vec<ExampleRelation> {
    let mut out: Vec<ExampleRelation> = Vec::new();
    for suggestion in similar {
        if out.len() >= MAX_TAXONOMY_EXAMPLES {
            break;
        }
        for relation in kg.search_relations(
            None,
            None,
            Some(&suggestion.name),
            Some(MAX_TAXONOMY_EXAMPLES),
        ) {
            out.push(ExampleRelation {
                from: relation.from,
                relation_type: relation.relation_type,
                to: relation.to,
            });
            if out.len() >= MAX_TAXONOMY_EXAMPLES {
                break;
            }
        }
    }
    out
}

/// Maps result entity objects through the suggestion hook. An object is
/// matched by name to its authored type and pre-write existence, because the
/// result may skip entities and reorder them. The suggestion payload for
/// each distinct authored type is read from `suggestions`.
fn enrich_entities(
    values: Vec<Value>,
    authored: &HashMap<String, (String, bool)>,
    suggestions: &HashMap<String, TypeSuggestions>,
) -> Vec<Value> {
    values
        .into_iter()
        .map(|value| {
            let name = value.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let Some((authored_type, _)) = authored.get(name) else {
                return value;
            };
            match suggestions.get(authored_type) {
                Some(candidates) => enrich_result_object(value, candidates),
                None => value,
            }
        })
        .collect()
}

pub fn handle_read_graph(kg: &GraphHandle, args: Option<&Value>) -> Result<String> {
    let params = args.unwrap_or(&Value::Null);
    let filter_type = params
        .get("entityType")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let offset = opt_usize(params, "offset", 0)?;
    let limit = opt_usize(params, "limit", MAX_SEARCH_LIMIT)?.min(MAX_SEARCH_LIMIT);

    let text = kg.read_graph_filtered(filter_type, offset, limit)?;
    Ok(build_content_response(&text))
}

pub fn handle_create_entities(
    kg: &GraphHandle,
    vs: Option<&VectorStore>,
    args: Option<&Value>,
) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let entities_val = params
        .get("entities")
        .ok_or_else(|| MCSError::InvalidParams("Missing 'entities' parameter".into()))?;

    let input_entities: Vec<crate::types::EntityInput> =
        serde_json::from_value(entities_val.clone())
            .map_err(|e| MCSError::InvalidParams(format!("Invalid entity: {e}")))?;

    if input_entities.len() > MAX_ENTITIES_PER_REQUEST {
        return Err(MCSError::InvalidParams(format!(
            "Too many entities (max {MAX_ENTITIES_PER_REQUEST})"
        )));
    }
    for entity in &input_entities {
        validate_name(&entity.name)?;
        validate_name(&entity.entity_type)?;
        if entity.observations.len() > MAX_OBSERVATIONS_PER_ENTITY {
            return Err(MCSError::InvalidParams(format!(
                "Too many observations per entity (max {MAX_OBSERVATIONS_PER_ENTITY})"
            )));
        }
        for obs in &entity.observations {
            validate_observation(&obs.body)?;
        }
    }

    // The write inserts the authored type row, so existence must be captured
    // before the mutation.
    let authored: HashMap<String, (String, bool)> = input_entities
        .iter()
        .map(|entity| {
            (
                entity.name.clone(),
                (
                    entity.entity_type.clone(),
                    kg.entity_type_exists(&entity.entity_type),
                ),
            )
        })
        .collect();

    let MutationResult::Entities(result) = apply_mutation(
        kg,
        MutationRequest::CreateEntities {
            entities: input_entities,
        },
    )?
    else {
        unreachable!("entity mutation result")
    };
    let values: Vec<Value> = match serde_json::to_value(result).map_err(MCSError::JsonError)? {
        Value::Array(items) => items,
        _ => unreachable!("entity mutation result is an array"),
    };
    let suggestions = suggestion_map(
        kg,
        authored
            .values()
            .filter(|(_, known)| !*known)
            .map(|(authored_type, _)| authored_type.as_str()),
        SubjectKind::EntityType,
        vs,
    );
    let values = enrich_entities(values, &authored, &suggestions);
    let text = serde_json::to_string(&values).map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_create_relations(
    kg: &GraphHandle,
    vs: Option<&VectorStore>,
    args: Option<&Value>,
) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let relations_val = params
        .get("relations")
        .ok_or_else(|| MCSError::InvalidParams("Missing 'relations' parameter".into()))?;

    let input_relations: Vec<crate::types::Relation> =
        serde_json::from_value(relations_val.clone())
            .map_err(|e| MCSError::InvalidParams(format!("Invalid relation: {e}")))?;

    if input_relations.len() > MAX_RELATIONS_PER_REQUEST {
        return Err(MCSError::InvalidParams(format!(
            "Too many relations (max {MAX_RELATIONS_PER_REQUEST})"
        )));
    }
    for rel in &input_relations {
        validate_name(&rel.from)?;
        validate_name(&rel.to)?;
        validate_name(&rel.relation_type)?;
    }

    // The write inserts the authored type row, so existence must be captured
    // before the mutation. The result relations are the request relations
    // verbatim, so their own relationType value is the authored type.
    let known_before: HashMap<String, bool> = input_relations
        .iter()
        .map(|relation| {
            (
                relation.relation_type.clone(),
                kg.relation_type_exists(&relation.relation_type),
            )
        })
        .collect();

    let MutationResult::Relations(result) = apply_mutation(
        kg,
        MutationRequest::CreateRelations {
            relations: input_relations,
        },
    )?
    else {
        unreachable!("relation mutation result")
    };
    let values: Vec<Value> = match serde_json::to_value(result).map_err(MCSError::JsonError)? {
        Value::Array(items) => items,
        _ => unreachable!("relation mutation result is an array"),
    };
    let suggestions = suggestion_map(
        kg,
        known_before
            .iter()
            .filter(|(_, known)| !**known)
            .map(|(authored_type, _)| authored_type.as_str()),
        SubjectKind::RelationType,
        vs,
    );
    let values: Vec<Value> = values
        .into_iter()
        .map(|value| {
            let authored_type = value
                .get("relationType")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match suggestions.get(authored_type) {
                Some(candidates) => enrich_result_object(value, candidates),
                None => value,
            }
        })
        .collect();
    let text = serde_json::to_string(&values).map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_add_observations(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let observations_val = params
        .get("observations")
        .ok_or_else(|| MCSError::InvalidParams("Missing 'observations' parameter".into()))?;

    let observations: Vec<Value> = serde_json::from_value(observations_val.clone())
        .map_err(|e| MCSError::InvalidParams(format!("Invalid observations: {e}")))?;

    let mut updates = Vec::new();

    for obs in &observations {
        let entity_name = obs
            .get("entityName")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MCSError::InvalidParams("Missing 'entityName' in observation".into()))?;

        let contents: Vec<crate::types::ObservationInput> = serde_json::from_value(
            obs.get("contents")
                .cloned()
                .ok_or_else(|| MCSError::InvalidParams("Missing 'contents'".into()))?,
        )
        .map_err(|e| MCSError::InvalidParams(format!("Invalid observations: {e}")))?;

        validate_name(entity_name)?;
        if contents.len() > MAX_OBSERVATIONS_PER_ENTITY {
            return Err(MCSError::InvalidParams(format!(
                "Too many observations per entity (max {MAX_OBSERVATIONS_PER_ENTITY})"
            )));
        }
        for content in &contents {
            validate_observation(&content.body)?;
        }

        updates.push(ObservationUpdate {
            entity_name: entity_name.into(),
            contents,
        });
    }
    let MutationResult::Observations(results) = apply_mutation(
        kg,
        MutationRequest::AddObservations {
            observations: updates,
        },
    )?
    else {
        unreachable!("observation mutation result")
    };
    let text = serde_json::to_string(&json!({"results": results})).map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_delete_entities(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let mut entity_names: Vec<String> = params
        .get("entityNames")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .ok_or_else(|| {
            MCSError::InvalidParams("Missing or invalid 'entityNames' parameter".into())
        })?;
    entity_names.truncate(MAX_NAMES_PER_REQUEST);

    apply_mutation(
        kg,
        MutationRequest::DeleteEntities {
            names: entity_names,
        },
    )?;

    Ok(text_content!("Entities deleted successfully"))
}

pub fn handle_delete_observations(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let deletions = params
        .get("deletions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            MCSError::InvalidParams("Missing or invalid 'deletions' parameter".into())
        })?;

    let mut updates = Vec::new();
    if deletions.len() > MAX_NAMES_PER_REQUEST {
        return Err(MCSError::InvalidParams(format!(
            "Too many deletions (max {MAX_NAMES_PER_REQUEST})"
        )));
    }
    for deletion in deletions {
        let entity_name = deletion
            .get("entityName")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MCSError::InvalidParams("Missing 'entityName' in deletion".into()))?;
        let observations: Vec<crate::types::ObservationInput> = serde_json::from_value(
            deletion
                .get("observations")
                .cloned()
                .ok_or_else(|| MCSError::InvalidParams("Missing 'observations'".into()))?,
        )
        .map_err(|e| MCSError::InvalidParams(format!("Invalid observations: {e}")))?;
        validate_name(entity_name)?;
        if observations.len() > MAX_OBSERVATIONS_PER_ENTITY {
            return Err(MCSError::InvalidParams(format!(
                "Too many observations per entity (max {MAX_OBSERVATIONS_PER_ENTITY})"
            )));
        }
        for observation in &observations {
            validate_observation(&observation.body)?;
        }

        updates.push(ObservationUpdate {
            entity_name: entity_name.into(),
            contents: observations,
        });
    }
    apply_mutation(
        kg,
        MutationRequest::DeleteObservations {
            observations: updates,
        },
    )?;

    Ok(text_content!("Observations deleted successfully"))
}

pub fn handle_delete_relations(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let relations_val = params
        .get("relations")
        .ok_or_else(|| MCSError::InvalidParams("Missing 'relations' parameter".into()))?;

    let mut input_relations: Vec<crate::types::Relation> =
        serde_json::from_value(relations_val.clone())
            .map_err(|e| MCSError::InvalidParams(format!("Invalid relation: {e}")))?;
    input_relations.truncate(MAX_RELATIONS_PER_REQUEST);

    apply_mutation(
        kg,
        MutationRequest::DeleteRelations {
            relations: input_relations,
        },
    )?;

    Ok(text_content!("Relations deleted successfully"))
}

pub fn handle_search_nodes(kg: &GraphHandle, args: Option<&Value>) -> Result<String> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let query = params
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'query' parameter".into()))?;
    let filter_type = params
        .get("entityType")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let offset = opt_usize(params, "offset", 0)?;
    let limit = opt_usize(params, "limit", DEFAULT_SEARCH_LIMIT)?.min(MAX_SEARCH_LIMIT);

    let matching = kg.search_nodes_filtered(query, filter_type, offset, limit);
    let text = serde_json::to_string(&matching).map_err(MCSError::JsonError)?;
    Ok(build_content_response(&text))
}

pub fn handle_open_nodes(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let mut names: Vec<String> = params
        .get("names")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .ok_or_else(|| MCSError::InvalidParams("Missing or invalid 'names' parameter".into()))?;
    names.truncate(MAX_NAMES_PER_REQUEST);

    let text = kg.open_nodes(&names);
    Ok(text_content!(text))
}

pub fn handle_entity_exists(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let mut names: Vec<String> = params
        .get("names")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .ok_or_else(|| MCSError::InvalidParams("Missing or invalid 'names' parameter".into()))?;
    names.truncate(MAX_NAMES_PER_REQUEST);

    let results = kg.entities_exist(&names)?;
    let text = serde_json::to_string(&results).map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_degree(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'name' parameter".into()))?;
    validate_name(name)?;
    let direction = crate::kg::Direction::parse(params.get("direction").and_then(|v| v.as_str()));

    let degree = kg.degree(name, direction)?;
    let text = serde_json::to_string(&json!({ "name": name, "degree": degree }))
        .map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_get_entity(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'name' parameter".into()))?;

    match kg.get_entity(name)? {
        Some(entity) => {
            let text = serde_json::to_string(&entity).map_err(MCSError::JsonError)?;
            Ok(text_content!(text))
        }
        None => Err(MCSError::InvalidParams(format!(
            "Entity '{name}' not found"
        ))),
    }
}

pub fn handle_graph_stats(kg: &GraphHandle) -> Result<Value> {
    let entity_count = kg.get_entity_count().unwrap_or(0);
    let relation_count = kg.get_relation_count().unwrap_or(0);
    let text = serde_json::to_string(&json!({
        "entities": entity_count,
        "relations": relation_count
    }))
    .map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_search_relations(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.unwrap_or(&serde_json::Value::Null);

    let from = params.get("from").and_then(|v| v.as_str());
    let to = params.get("to").and_then(|v| v.as_str());
    let rtype = params.get("relationType").and_then(|v| v.as_str());

    let mut results = kg.search_relations(from, to, rtype, Some(MAX_RELATION_SEARCH_RESULTS));
    results.truncate(MAX_RELATION_SEARCH_RESULTS);
    let text = serde_json::to_string(&results).map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_find_path(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let from = params
        .get("from")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'from' parameter".into()))?;
    let to = params
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'to' parameter".into()))?;

    let path = kg.find_path(from, to)?;
    let text = serde_json::to_string(&path).map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_compact(kg: &GraphHandle) -> Result<Value> {
    apply_mutation(kg, MutationRequest::Compact)?;
    Ok(text_content!("Log compacted successfully"))
}

fn opt_usize(params: &Value, key: &str, default: usize) -> Result<usize> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_u64().map(|n| n as usize).ok_or_else(|| {
            MCSError::InvalidParams(format!("'{key}' must be a non-negative integer"))
        }),
    }
}

pub fn handle_get_neighbors(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'name' parameter".into()))?;
    validate_name(name)?;

    let direction = crate::kg::Direction::parse(params.get("direction").and_then(|v| v.as_str()));
    let rtype = params
        .get("relationType")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let depth = opt_usize(params, "depth", 1)?;
    if depth > MAX_NEIGHBOR_DEPTH {
        return Err(MCSError::InvalidParams(format!(
            "depth too large (max {MAX_NEIGHBOR_DEPTH})"
        )));
    }

    let text = kg.neighbors(name, direction, rtype, depth as u32)?;
    Ok(text_content!(text))
}

pub fn handle_describe_entity(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'name' parameter".into()))?;
    validate_name(name)?;

    let result = kg.describe_entity(name)?;
    let text = serde_json::to_string(&result).map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_list_entity_types(kg: &GraphHandle) -> Result<Value> {
    let counts = kg.entity_type_counts();
    let arr: Vec<Value> = counts
        .into_iter()
        .map(|(t, c)| json!({ "type": t, "count": c }))
        .collect();
    let text = serde_json::to_string(&arr).map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_list_relation_types(kg: &GraphHandle) -> Result<Value> {
    let counts = kg.relation_type_counts();
    let arr: Vec<Value> = counts
        .into_iter()
        .map(|(t, c)| json!({ "type": t, "count": c }))
        .collect();
    let text = serde_json::to_string(&arr).map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_upsert_entities(
    kg: &GraphHandle,
    vs: Option<&VectorStore>,
    args: Option<&Value>,
) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let entities_val = params
        .get("entities")
        .ok_or_else(|| MCSError::InvalidParams("Missing 'entities' parameter".into()))?;

    let input_entities: Vec<crate::types::EntityInput> =
        serde_json::from_value(entities_val.clone())
            .map_err(|e| MCSError::InvalidParams(format!("Invalid entity: {e}")))?;

    if input_entities.len() > MAX_ENTITIES_PER_REQUEST {
        return Err(MCSError::InvalidParams(format!(
            "Too many entities (max {MAX_ENTITIES_PER_REQUEST})"
        )));
    }
    for entity in &input_entities {
        validate_name(&entity.name)?;
        validate_name(&entity.entity_type)?;
        if entity.observations.len() > MAX_OBSERVATIONS_PER_ENTITY {
            return Err(MCSError::InvalidParams(format!(
                "Too many observations per entity (max {MAX_OBSERVATIONS_PER_ENTITY})"
            )));
        }
        for obs in &entity.observations {
            validate_observation(&obs.body)?;
        }
    }

    // The write inserts the authored type row, so existence must be captured
    // before the mutation.
    let authored: HashMap<String, (String, bool)> = input_entities
        .iter()
        .map(|entity| {
            (
                entity.name.clone(),
                (
                    entity.entity_type.clone(),
                    kg.entity_type_exists(&entity.entity_type),
                ),
            )
        })
        .collect();

    let MutationResult::Entities(results) = apply_mutation(
        kg,
        MutationRequest::UpsertEntities {
            entities: input_entities,
        },
    )?
    else {
        unreachable!("upsert mutation result")
    };
    let values: Vec<Value> = match serde_json::to_value(results).map_err(MCSError::JsonError)? {
        Value::Array(items) => items,
        _ => unreachable!("upsert mutation result is an array"),
    };
    let suggestions = suggestion_map(
        kg,
        authored
            .values()
            .filter(|(_, known)| !*known)
            .map(|(authored_type, _)| authored_type.as_str()),
        SubjectKind::EntityType,
        vs,
    );
    let values = enrich_entities(values, &authored, &suggestions);
    let text = serde_json::to_string(&json!({ "results": values })).map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_merge_entities(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let source = params
        .get("source")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'source' parameter".into()))?;
    let target = params
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'target' parameter".into()))?;
    validate_name(source)?;
    validate_name(target)?;

    let MutationResult::Entity(result) = apply_mutation(
        kg,
        MutationRequest::MergeEntities {
            source: source.into(),
            target: target.into(),
        },
    )?
    else {
        unreachable!("merge mutation result")
    };
    let text = serde_json::to_string(&result).map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_rename_entity(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let old_name = params
        .get("oldName")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'oldName' parameter".into()))?;
    let new_name = params
        .get("newName")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'newName' parameter".into()))?;
    validate_name(old_name)?;
    validate_name(new_name)?;

    let MutationResult::Entity(result) = apply_mutation(
        kg,
        MutationRequest::RenameEntity {
            old_name: old_name.into(),
            new_name: new_name.into(),
        },
    )?
    else {
        unreachable!("rename mutation result")
    };
    let text = serde_json::to_string(&result).map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_extract_subgraph(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let names: Vec<String> = params
        .get("names")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .ok_or_else(|| MCSError::InvalidParams("Missing or invalid 'names' parameter".into()))?;
    let depth = opt_usize(params, "depth", 1)? as u32;
    if depth > MAX_NEIGHBOR_DEPTH as u32 {
        return Err(MCSError::InvalidParams(format!(
            "depth too large (max {MAX_NEIGHBOR_DEPTH})"
        )));
    }

    let text = kg.extract_subgraph(&names, depth)?;
    Ok(text_content!(text))
}

pub fn handle_batch_get_entities(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let mut names: Vec<String> = params
        .get("names")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .ok_or_else(|| MCSError::InvalidParams("Missing or invalid 'names' parameter".into()))?;
    names.truncate(MAX_NAMES_PER_REQUEST);

    let results = kg.batch_get_entities(&names);
    let arr: Vec<Value> = results
        .into_iter()
        .map(|opt| match opt {
            Some(entity) => serde_json::to_value(entity).unwrap_or(Value::Null),
            None => Value::Null,
        })
        .collect();
    let text = serde_json::to_string(&arr).map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_find_all_paths(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let params = args.ok_or_else(|| MCSError::InvalidParams("Missing parameters".into()))?;
    let from = params
        .get("from")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'from' parameter".into()))?;
    let to = params
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MCSError::InvalidParams("Missing 'to' parameter".into()))?;
    let max_depth = opt_usize(params, "maxDepth", 6)?.min(MAX_FIND_ALL_PATHS_DEPTH);
    let max_paths = opt_usize(params, "maxPaths", 50)?.min(MAX_FIND_ALL_PATHS_RESULTS);

    let paths = kg.find_all_paths(from, to, max_depth, max_paths)?;
    let text = serde_json::to_string(&paths).map_err(MCSError::JsonError)?;
    Ok(text_content!(text))
}

pub fn handle_export_graph(kg: &GraphHandle, args: Option<&Value>) -> Result<Value> {
    let format = args
        .and_then(|p| p.get("format"))
        .and_then(|v| v.as_str())
        .unwrap_or("json");

    let text = kg.export(format, MAX_EXPORT_ROWS)?;
    Ok(text_content!(text))
}
