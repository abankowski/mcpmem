use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub name: String,
    #[serde(rename = "entityType")]
    pub entity_type: String,
    pub observations: Vec<String>,
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
    pub observations: Vec<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeGraphOut {
    pub entities: Vec<Entity>,
    pub relations: Vec<Relation>,
}
