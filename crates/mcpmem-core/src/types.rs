use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Public write DTO. Server-owned metadata is deliberately not accepted here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObservationInput {
    pub body: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_occurred_at"
    )]
    pub occurred_at_us: Option<i64>,
}

fn deserialize_occurred_at<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<i64>, D::Error> {
    let value = i64::deserialize(deserializer)?;
    if value < 0 {
        return Err(serde::de::Error::custom(
            "occurredAtUs must be non-negative",
        ));
    }
    Ok(Some(value))
}

impl From<String> for ObservationInput {
    fn from(body: String) -> Self {
        Self {
            body,
            occurred_at_us: None,
        }
    }
}

impl From<&str> for ObservationInput {
    fn from(body: &str) -> Self {
        body.to_owned().into()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{AttributeSet, EntityInput, ObservationInput, RelationInput};

    #[test]
    fn observation_input_omits_absent_fact_time_on_the_wire() {
        let input = ObservationInput::from("fact");
        let encoded = serde_json::to_value(&input).expect("input serializes");
        assert_eq!(encoded, serde_json::json!({"body":"fact"}));
        assert_eq!(
            serde_json::from_value::<ObservationInput>(encoded).expect("wire value decodes"),
            input
        );

        let timed = serde_json::json!({"body":"fact","occurredAtUs":7});
        assert_eq!(
            serde_json::from_value::<ObservationInput>(timed.clone()).expect("timestamp decodes"),
            ObservationInput {
                body: "fact".into(),
                occurred_at_us: Some(7),
            }
        );
        assert_eq!(
            serde_json::to_value(ObservationInput {
                body: "fact".into(),
                occurred_at_us: Some(7),
            })
            .expect("timestamp serializes"),
            timed
        );
        assert!(
            serde_json::from_value::<ObservationInput>(
                serde_json::json!({"body":"fact","occurredAtUs":null})
            )
            .is_err(),
            "an explicitly null fact time is not a valid write input"
        );
    }

    #[test]
    fn entity_without_attributes_parses_and_serializes_without_the_key() {
        let plain = serde_json::from_str::<EntityInput>(
            r#"{"name":"x","entityType":"T","observations":[]}"#,
        )
        .expect("old payload without attributes parses");
        assert_eq!(plain.attributes, None);
        let text = serde_json::to_string(&EntityInput {
            name: "y".into(),
            entity_type: "T".into(),
            observations: vec![],
            attributes: None,
        })
        .expect("serializes");
        assert!(!text.contains("attributes"), "empty attributes are omitted");
    }

    #[test]
    fn entity_with_attributes_round_trips() {
        let attributes = BTreeMap::from([
            ("k".to_string(), "v".to_string()),
            ("n".to_string(), "2".to_string()),
        ]);
        let input = serde_json::from_str::<EntityInput>(
            r#"{"name":"x","entityType":"T","observations":[],"attributes":{"k":"v","n":"2"}}"#,
        )
        .expect("attributes parse");
        assert_eq!(input.attributes, Some(attributes));
        let text = serde_json::to_string(&input).expect("serializes");
        let again = serde_json::from_str::<EntityInput>(&text).expect("round trips");
        assert_eq!(again.attributes, input.attributes);
    }

    #[test]
    fn relation_input_defaults_observations_and_attributes() {
        let r =
            serde_json::from_str::<RelationInput>(r#"{"from":"a","to":"b","relationType":"uses"}"#)
                .expect("bare triple parses");
        assert_eq!(r.observations.len(), 0);
        assert_eq!(r.attributes, None);
        assert!(
            serde_json::from_str::<RelationInput>(
                r#"{"from":"a","to":"b","relationType":"uses","nope":1}"#,
            )
            .is_err(),
            "deny_unknown_fields stays"
        );
    }

    #[test]
    fn attribute_set_validates_owner_shape_and_rejects_unknown() {
        assert!(
            serde_json::from_str::<AttributeSet>(
                r#"{"ownerKind":"entity","entityName":"x","attributes":{"k":"v"}}"#,
            )
            .is_ok()
        );
        assert!(
            serde_json::from_str::<AttributeSet>(
                r#"{"ownerKind":"relation","from":"a","to":"b","relationType":"uses","attributes":{}}"#,
            )
            .is_ok()
        );
        assert!(
            serde_json::from_str::<AttributeSet>(
                r#"{"ownerKind":"entity","entityName":"x","attributes":{},"boom":1}"#,
            )
            .is_err()
        );
    }
}

/// Canonical read model. Unknown creation time is reserved for old durable payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Observation {
    pub body: String,
    pub created_at_us: Option<i64>,
    pub occurred_at_us: Option<i64>,
    pub origin_entity_name: Option<String>,
}

impl<'de> Deserialize<'de> for Observation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Structured {
            body: String,
            created_at_us: Option<i64>,
            occurred_at_us: Option<i64>,
            origin_entity_name: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Durable {
            Structured(Structured),
            Historical(String),
        }
        Ok(match Durable::deserialize(deserializer)? {
            Durable::Structured(value) => Self {
                body: value.body,
                created_at_us: value.created_at_us,
                occurred_at_us: value.occurred_at_us,
                origin_entity_name: value.origin_entity_name,
            },
            Durable::Historical(body) => Self {
                body,
                created_at_us: None,
                occurred_at_us: None,
                origin_entity_name: None,
            },
        })
    }
}

pub type EntityInput = Entity<ObservationInput>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entity<O = Observation> {
    pub name: String,
    #[serde(rename = "entityType")]
    pub entity_type: String,
    pub observations: Vec<O>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<BTreeMap<String, String>>,
}

/// Read model returned by `describe_entity`.
///
/// This deliberately extends, rather than replaces, [`Entity`]: entity
/// creation and graph exports retain their existing wire shape while callers
/// of the richer read operation receive the incident graph context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityDescription {
    pub name: String,
    #[serde(rename = "entityType")]
    pub entity_type: String,
    pub observations: Vec<Observation>,
    pub relations: Vec<Relation>,
    pub neighbors: Vec<String>,
    pub degree: Degree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Degree {
    #[serde(rename = "in")]
    pub incoming: i64,
    #[serde(rename = "out")]
    pub outgoing: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Relation {
    pub from: String,
    pub to: String,
    #[serde(rename = "relationType")]
    pub relation_type: String,
}

/// Public write DTO for `create_relations`. The triple is required; the
/// observations and attributes are optional on the wire and default to empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelationInput {
    pub from: String,
    pub to: String,
    pub relation_type: String,
    #[serde(default)]
    pub observations: Vec<ObservationInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<BTreeMap<String, String>>,
}

/// Public write DTO for appending observations to an existing relation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelationObservationUpdate {
    pub relation: Relation,
    pub contents: Vec<ObservationInput>,
}

/// Public write DTO for setting k:v attributes on an entity or a relation.
///
/// The wire shape is owner-kind discriminated: an entity owner carries
/// `entityName`; a relation owner carries `from`, `to` and `relationType`.
/// The absent half defaults to `None` so each owner shape parses alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttributeSet {
    pub owner_kind: String,
    #[serde(default)]
    pub entity_name: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub relation_type: Option<String>,
    pub attributes: BTreeMap<String, String>,
}

/// Public write DTO for deleting k:v attributes from an entity or a relation.
/// Identical owner shape to [`AttributeSet`]; `keys` names the removed entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttributeDelete {
    pub owner_kind: String,
    #[serde(default)]
    pub entity_name: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub relation_type: Option<String>,
    pub keys: Vec<String>,
}

/// Read model returned by `search_relations`. Serialize-only: the server
/// always fills both `observations` and `attributes`, so callers see them
/// present in the output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationDetail {
    pub from: String,
    pub to: String,
    pub relation_type: String,
    pub observations: Vec<Observation>,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeGraphOut {
    pub entities: Vec<Entity>,
    pub relations: Vec<Relation>,
}
