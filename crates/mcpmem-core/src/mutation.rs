//! The single graph write boundary. Snapshots, graph writes and derived
//! counters share the writer transaction; no change is published before commit.
use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::errors::{MCSError, Result};
use crate::graph::{GraphHandle, TxGuard, name_hash};
use crate::types::{Entity, EntityInput, Observation, ObservationInput, Relation};

pub type MutationError = MCSError;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MutationContext {
    pub actor: String,
    pub origin: String,
    pub correlation_id: Uuid,
    pub causation_id: Option<Uuid>,
    pub hop_count: u8,
    pub idempotency_key: Option<String>,
}

impl MutationContext {
    /// Trusted in-process legacy ingress. Network callers must supply a
    /// context derived from their authenticated principal instead.
    pub fn local() -> Self {
        Self {
            actor: "local".into(),
            origin: "mcp".into(),
            correlation_id: Uuid::new_v4(),
            causation_id: None,
            hop_count: 0,
            idempotency_key: None,
        }
    }

    pub fn validate(self) -> Result<Self> {
        if self.actor.trim().is_empty()
            || self.actor.len() > 256
            || self.origin.trim().is_empty()
            || self.origin.len() > 256
            || self.actor.chars().any(char::is_control)
            || self.origin.chars().any(char::is_control)
            || self.correlation_id.is_nil()
            || self.hop_count > 15
            || self.causation_id.is_some_and(|id| id.is_nil())
            || (self.hop_count > 0) != self.causation_id.is_some()
            || self.idempotency_key.as_ref().is_some_and(|key| {
                key.is_empty() || key.len() > 128 || key.chars().any(char::is_control)
            })
        {
            return Err(MCSError::InvalidParams(
                "Invalid mutation provenance".into(),
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObservationUpdate {
    pub entity_name: String,
    pub contents: Vec<ObservationInput>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum MutationRequest {
    CreateEntities {
        entities: Vec<EntityInput>,
    },
    UpsertEntities {
        entities: Vec<EntityInput>,
    },
    DeleteEntities {
        names: Vec<String>,
    },
    CreateRelations {
        relations: Vec<Relation>,
    },
    DeleteRelations {
        relations: Vec<Relation>,
    },
    AddObservations {
        observations: Vec<ObservationUpdate>,
    },
    DeleteObservations {
        observations: Vec<ObservationUpdate>,
    },
    MergeEntities {
        source: String,
        target: String,
    },
    RenameEntity {
        old_name: String,
        new_name: String,
    },
    PurgeDefinedEntities {
        name: String,
    },
    Compact,
    Wipe,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntitySnapshot {
    pub entity_id: i64,
    pub name: String,
    pub entity_type: String,
    pub observations: Vec<Observation>,
}

impl EntitySnapshot {
    pub fn entity(&self) -> Entity {
        Entity {
            name: self.name.clone(),
            entity_type: self.entity_type.clone(),
            observations: self.observations.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeOperation {
    Create,
    Update,
    Delete,
    Rename,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RelationDelta {
    pub added: Vec<Relation>,
    pub removed: Vec<Relation>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityChange {
    pub operation: ChangeOperation,
    pub before: Option<EntitySnapshot>,
    pub after: Option<EntitySnapshot>,
    pub relation_delta: Option<RelationDelta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommittedChangeSet {
    pub transaction_id: Uuid,
    pub changes: Vec<EntityChange>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservationResult {
    pub entity_name: String,
    pub added_observations: Vec<Observation>,
}

/// Legacy response data is captured inside the same transaction, preventing
/// an adapter from returning a concurrent writer's later state.
#[derive(Debug, Serialize, Deserialize)]
pub enum MutationResult {
    Entities(Vec<Entity>),
    Relations(Vec<Relation>),
    Observations(Vec<ObservationResult>),
    Entity(Entity),
    Count(usize),
    Unit,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MutationOutcome {
    pub changes: CommittedChangeSet,
    pub result: MutationResult,
    pub replayed: bool,
}

pub struct MutationService<'a> {
    graph: &'a GraphHandle,
}

impl<'a> MutationService<'a> {
    pub const fn new(graph: &'a GraphHandle) -> Self {
        Self { graph }
    }

    pub fn apply(
        &self,
        request: MutationRequest,
        context: MutationContext,
    ) -> Result<CommittedChangeSet> {
        self.apply_with_result(request, context)
            .map(|(changes, _)| changes)
    }

    pub fn apply_with_result(
        &self,
        request: MutationRequest,
        context: MutationContext,
    ) -> Result<(CommittedChangeSet, MutationResult)> {
        if context.idempotency_key.is_some() {
            return Err(MCSError::InvalidParams(
                "idempotent ingress requires a raw request fingerprint".into(),
            ));
        }
        self.apply_inner(request, context, None)
            .map(|outcome| (outcome.changes, outcome.result))
    }

    pub fn apply_idempotent(
        &self,
        request: MutationRequest,
        context: MutationContext,
        fingerprint: &str,
    ) -> Result<MutationOutcome> {
        if context.idempotency_key.is_none()
            || fingerprint.len() != 64
            || !fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(MCSError::InvalidParams(
                "idempotent ingress requires a key and SHA-256 request fingerprint".into(),
            ));
        }
        self.apply_inner(request, context, Some(fingerprint))
    }

    fn apply_inner(
        &self,
        request: MutationRequest,
        context: MutationContext,
        fingerprint: Option<&str>,
    ) -> Result<MutationOutcome> {
        let context = context.validate()?;
        let conn = self.graph.writer.lock();
        let tx = TxGuard::begin(&conn)?;
        if let (Some(key), Some(fingerprint)) = (&context.idempotency_key, fingerprint) {
            let prior: Option<(String,String)> = conn.query_row("SELECT request_fingerprint,response FROM idempotency_record WHERE principal_id=?1 AND idempotency_key=?2", params![context.actor,key], |r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(sql_error)?;
            if let Some((saved_fingerprint, response)) = prior {
                if saved_fingerprint != fingerprint {
                    return Err(MCSError::InvalidParams("idempotency_conflict".into()));
                }
                let mut outcome: MutationOutcome = serde_json::from_str(&response)?;
                outcome.replayed = true;
                tx.commit()?;
                return Ok(outcome);
            }
        }
        if let Some(parent_id) = context.causation_id {
            let parent = crate::events::EventRepository::new(&conn)
                .get(parent_id)?
                .ok_or_else(|| MCSError::InvalidParams("unknown causation event".into()))?;
            if parent.provenance.correlation_id != context.correlation_id
                || parent.provenance.hop_count.checked_add(1) != Some(context.hop_count)
            {
                return Err(MCSError::InvalidParams("invalid causation chain".into()));
            }
        }
        self.graph.refresh_seqs(&conn)?;
        let rename = match &request {
            MutationRequest::RenameEntity { old_name, new_name } => {
                Some((old_name.clone(), new_name.clone()))
            }
            _ => None,
        };
        let names = affected_names(&conn, &request)?;
        let before = capture(&conn, &names)?;
        let result = execute(self.graph, &conn, request)?;
        let after = capture(&conn, &names)?;
        let changes = match rename {
            Some((old_name, new_name)) if old_name != new_name => {
                rename_changes(&before, &after, &old_name, &new_name)
            }
            _ => effective_changes(&before, &after),
        };
        update_counters(&conn, &before, &after, &changes)?;
        self.graph.sync_seqs(&conn)?;
        let committed = CommittedChangeSet {
            transaction_id: Uuid::new_v4(),
            changes,
        };
        crate::events::persist_changes(&conn, &committed, &context)?;
        let outcome = MutationOutcome {
            changes: committed,
            result,
            replayed: false,
        };
        if let (Some(key), Some(fingerprint)) = (&context.idempotency_key, fingerprint) {
            conn.execute(
                "INSERT INTO idempotency_record VALUES(?1,?2,?3,?4,?5)",
                params![
                    context.actor,
                    key,
                    fingerprint,
                    serde_json::to_string(&outcome)?,
                    now_us()
                ],
            )
            .map_err(sql_error)?;
        }
        tx.commit()?;
        Ok(outcome)
    }
}

fn sql_error(error: rusqlite::Error) -> MCSError {
    MCSError::IoError(std::io::Error::other(error))
}

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}

pub(crate) fn read_entity(conn: &Connection, name: &str) -> Result<Option<EntitySnapshot>> {
    let row = conn.query_row(
        "SELECT e.id, e.name, t.name FROM entity e JOIN type_dict t ON t.id=e.type_id WHERE e.name_hash=?1 AND e.name=?2 AND e.flags=0",
        params![name_hash(name), name],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
    ).optional().map_err(sql_error)?;
    row.map(|(entity_id, name, entity_type)| {
        let mut stmt = conn
            .prepare_cached("SELECT body,created_us,occurred_us,origin_entity_name FROM observation WHERE entity_id=?1 ORDER BY idx, id")
            .map_err(sql_error)?;
        let observations = stmt
            .query_map([entity_id], |row| Ok(Observation { body: row.get(0)?, created_at_us: Some(row.get(1)?), occurred_at_us: row.get(2)?, origin_entity_name: row.get(3)? }))
            .map_err(sql_error)?
            .collect::<rusqlite::Result<Vec<Observation>>>()
            .map_err(sql_error)?;
        Ok(EntitySnapshot {
            entity_id,
            name,
            entity_type,
            observations,
        })
    })
    .transpose()
}

fn require_entity(conn: &Connection, name: &str) -> Result<EntitySnapshot> {
    read_entity(conn, name)?
        .ok_or_else(|| MCSError::InvalidParams(format!("Entity '{name}' not found")))
}

pub(crate) fn relations_for(conn: &Connection, name: &str) -> Result<Vec<Relation>> {
    let mut stmt = conn.prepare_cached(
        "SELECT f.name, t.name, d.name FROM relation r JOIN entity f ON f.id=r.from_id JOIN entity t ON t.id=r.to_id JOIN type_dict d ON d.id=r.type_id WHERE f.flags=0 AND t.flags=0 AND (r.from_id IN (SELECT id FROM entity WHERE name_hash=?1 AND name=?2 AND flags=0) OR r.to_id IN (SELECT id FROM entity WHERE name_hash=?1 AND name=?2 AND flags=0)) ORDER BY f.name, t.name, d.name"
    ).map_err(sql_error)?;
    stmt.query_map(params![name_hash(name), name], |row| {
        Ok(Relation {
            from: row.get(0)?,
            to: row.get(1)?,
            relation_type: row.get(2)?,
        })
    })
    .map_err(sql_error)?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(sql_error)
}

fn defined_names(conn: &Connection, name: &str) -> Result<Vec<String>> {
    let mut names: Vec<String> = relations_for(conn, name)?
        .into_iter()
        .filter(|r| r.from == name && r.relation_type == "defines")
        .map(|r| r.to)
        .collect();
    names.push(name.into());
    names.sort();
    names.dedup();
    Ok(names)
}

fn affected_names(conn: &Connection, request: &MutationRequest) -> Result<BTreeSet<String>> {
    let mut names: BTreeSet<String> = match request {
        MutationRequest::CreateEntities { entities }
        | MutationRequest::UpsertEntities { entities } => {
            entities.iter().map(|e| e.name.clone()).collect()
        }
        MutationRequest::DeleteEntities { names } => names.iter().cloned().collect(),
        MutationRequest::CreateRelations { relations }
        | MutationRequest::DeleteRelations { relations } => relations
            .iter()
            .flat_map(|r| [r.from.clone(), r.to.clone()])
            .collect(),
        MutationRequest::AddObservations { observations }
        | MutationRequest::DeleteObservations { observations } => {
            observations.iter().map(|o| o.entity_name.clone()).collect()
        }
        MutationRequest::MergeEntities { source, target } => {
            [source.clone(), target.clone()].into()
        }
        MutationRequest::RenameEntity { old_name, new_name } => {
            [old_name.clone(), new_name.clone()].into()
        }
        MutationRequest::PurgeDefinedEntities { name } => {
            defined_names(conn, name)?.into_iter().collect()
        }
        MutationRequest::Compact => BTreeSet::new(),
        MutationRequest::Wipe => {
            let mut stmt = conn
                .prepare("SELECT name FROM entity WHERE flags=0")
                .map_err(sql_error)?;
            stmt.query_map([], |row| row.get(0))
                .map_err(sql_error)?
                .collect::<rusqlite::Result<_>>()
                .map_err(sql_error)?
        }
    };
    // Deletes/merges change surviving neighbours too. Resolve these before
    // deleting any rows so their before snapshots and relation deltas survive.
    if matches!(
        request,
        MutationRequest::DeleteEntities { .. }
            | MutationRequest::MergeEntities { .. }
            | MutationRequest::RenameEntity { .. }
            | MutationRequest::PurgeDefinedEntities { .. }
    ) {
        let neighbours = names
            .iter()
            .map(|name| relations_for(conn, name))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .flat_map(|r| [r.from, r.to])
            .collect::<Vec<_>>();
        names.extend(neighbours);
    }
    Ok(names)
}

#[derive(Default)]
struct Snapshot {
    entities: BTreeMap<String, EntitySnapshot>,
    relations: BTreeSet<Relation>,
    relation_rows: BTreeMap<Relation, i64>,
}

fn capture(conn: &Connection, names: &BTreeSet<String>) -> Result<Snapshot> {
    let mut snapshot = Snapshot::default();
    for name in names {
        if let Some(entity) = read_entity(conn, name)? {
            snapshot.entities.insert(name.clone(), entity);
        }
        let mut relation_rows = BTreeMap::new();
        for relation in relations_for(conn, name)? {
            *relation_rows.entry(relation).or_default() += 1;
        }
        // Both endpoint queries return every physical row of the same relation.
        // Replace the count rather than adding it twice; keep set semantics for
        // committed deltas independently of legacy duplicate storage rows.
        snapshot.relations.extend(relation_rows.keys().cloned());
        snapshot.relation_rows.extend(relation_rows);
    }
    Ok(snapshot)
}

fn effective_changes(before: &Snapshot, after: &Snapshot) -> Vec<EntityChange> {
    let mut deltas: BTreeMap<&str, RelationDelta> = BTreeMap::new();
    for (added, relations) in [
        (true, after.relations.difference(&before.relations)),
        (false, before.relations.difference(&after.relations)),
    ] {
        for relation in relations {
            for name in [&relation.from, &relation.to]
                .into_iter()
                .collect::<BTreeSet<_>>()
            {
                let delta = deltas.entry(name).or_default();
                if added {
                    delta.added.push(relation.clone());
                } else {
                    delta.removed.push(relation.clone());
                }
            }
        }
    }
    before
        .entities
        .keys()
        .chain(after.entities.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|name| {
            let old = before.entities.get(name);
            let new = after.entities.get(name);
            let delta = deltas.remove(name.as_str()).unwrap_or_default();
            let has_delta = !delta.added.is_empty() || !delta.removed.is_empty();
            if old == new && !has_delta {
                return None;
            }
            let operation = match (old, new) {
                (None, Some(_)) => ChangeOperation::Create,
                (Some(_), None) => ChangeOperation::Delete,
                _ => ChangeOperation::Update,
            };
            Some(EntityChange {
                operation,
                before: old.cloned(),
                after: new.cloned(),
                relation_delta: has_delta.then_some(delta),
                old_name: None,
                new_name: None,
            })
        })
        .collect()
}

fn rename_changes(
    before: &Snapshot,
    after: &Snapshot,
    old_name: &str,
    new_name: &str,
) -> Vec<EntityChange> {
    let (Some(before), Some(after)) = (before.entities.get(old_name), after.entities.get(new_name))
    else {
        return Vec::new();
    };
    vec![EntityChange {
        operation: ChangeOperation::Rename,
        before: Some(before.clone()),
        after: Some(after.clone()),
        relation_delta: None,
        old_name: Some(old_name.into()),
        new_name: Some(new_name.into()),
    }]
}

fn type_id(conn: &Connection, name: &str, kind: i64) -> Result<i64> {
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM type_dict WHERE kind=?1 AND name=?2",
            params![kind, name],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_error)?
    {
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO type_dict(kind,name,count) VALUES(?1,?2,0)",
        params![kind, name],
    )
    .map_err(sql_error)?;
    Ok(conn.last_insert_rowid())
}

/// Queue one taxonomy subject for every serving profile, using the same
/// lookup as the entity job path. Without a serving profile the job is
/// explicitly held by the queue function itself.
fn enqueue_taxonomy_jobs(
    conn: &Connection,
    kind: i64,
    id: i64,
    revision: i64,
    operation: crate::jobs::IndexOperation,
) -> Result<()> {
    for profile_id in crate::jobs::serving_profile_ids(conn)? {
        crate::jobs::enqueue_taxonomy(conn, kind, id, revision, operation, profile_id)?;
    }
    Ok(())
}

/// Tombstone the taxonomy mirror of one deleted relation triple and enqueue
/// the delete. A missing mirror is a legacy row: insert it tombstoned.
fn tombstone_relation_mirror(conn: &Connection, from_id: i64, to_id: i64, type_id: i64) -> Result<()> {
    let (id, revision): (i64, i64) = conn
        .query_row(
            "INSERT INTO taxonomy_relation(from_id,to_id,type_id,revision,deleted) VALUES(?1,?2,?3,1,1) \
             ON CONFLICT(from_id,to_id,type_id) DO UPDATE SET revision=revision+1,deleted=1 \
             RETURNING id,revision",
            params![from_id, to_id, type_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(sql_error)?;
    enqueue_taxonomy_jobs(conn, 2, id, revision, crate::jobs::IndexOperation::Delete)
}

fn insert_observations(
    graph: &GraphHandle,
    conn: &Connection,
    id: i64,
    contents: &[ObservationInput],
) -> Result<Vec<Observation>> {
    let idx: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(idx),-1) FROM observation WHERE entity_id=?1",
            [id],
            |r| r.get(0),
        )
        .map_err(sql_error)?;
    let mut stmt = conn
        .prepare_cached(
            "INSERT INTO observation(id,entity_id,idx,body,created_us,occurred_us) VALUES(?1,?2,?3,?4,?5,?6)",
        )
        .map_err(sql_error)?;
    let mut inserted = Vec::with_capacity(contents.len());
    for (offset, observation) in contents.iter().enumerate() {
        if observation.occurred_at_us.is_some_and(|time| time < 0) {
            return Err(MCSError::InvalidParams(
                "occurredAtUs must be non-negative".into(),
            ));
        }
        let created_at_us = now_us();
        stmt.execute(params![
            graph.next_obs_id(),
            id,
            idx + offset as i64 + 1,
            observation.body,
            created_at_us,
            observation.occurred_at_us
        ])
        .map_err(sql_error)?;
        inserted.push(Observation {
            body: observation.body.clone(),
            created_at_us: Some(created_at_us),
            occurred_at_us: observation.occurred_at_us,
            origin_entity_name: None,
        });
    }
    Ok(inserted)
}

fn create_entity(graph: &GraphHandle, conn: &Connection, entity: &EntityInput) -> Result<bool> {
    if entity.name.is_empty() || read_entity(conn, &entity.name)?.is_some() {
        return Ok(false);
    }
    let id = graph.next_entity_id();
    let kind = type_id(conn, &entity.entity_type, 0)?;
    conn.execute("INSERT INTO entity(id,name_hash,name,type_id,obs_count,out_deg,in_deg,created_us,updated_us,flags) VALUES(?1,?2,?3,?4,0,0,0,?5,?5,0)", params![id,name_hash(&entity.name),entity.name,kind,now_us()]).map_err(sql_error)?;
    insert_observations(graph, conn, id, &entity.observations)?;
    conn.execute(
        "INSERT INTO name_fts(rowid,name) VALUES(?1,?2)",
        params![id, entity.name],
    )
    .map_err(sql_error)?;
    Ok(true)
}

fn create_relation(conn: &Connection, relation: &Relation) -> Result<bool> {
    let (Some(from), Some(to)) = (
        read_entity(conn, &relation.from)?,
        read_entity(conn, &relation.to)?,
    ) else {
        return Ok(false);
    };
    let kind = type_id(conn, &relation.relation_type, 1)?;
    let changed = conn.execute("INSERT INTO relation(from_id,to_id,type_id,created_us) SELECT ?1,?2,?3,?4 WHERE NOT EXISTS(SELECT 1 FROM relation WHERE from_id=?1 AND to_id=?2 AND type_id=?3)", params![from.entity_id,to.entity_id,kind,now_us()]).map_err(sql_error)?;
    if changed > 0 {
        // Mirror the triple for the taxonomy worker. The mirror id tracks the
        // physical rowid; a delete + recreate resets the row and its revision.
        let rowid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO taxonomy_relation(id,from_id,to_id,type_id,revision,deleted) VALUES(?1,?2,?3,?4,1,0) \
             ON CONFLICT(from_id,to_id,type_id) DO UPDATE SET revision=1,deleted=0,id=excluded.id",
            params![rowid, from.entity_id, to.entity_id, kind],
        )
        .map_err(sql_error)?;
        enqueue_taxonomy_jobs(conn, 2, rowid, 1, crate::jobs::IndexOperation::Upsert)?;
    }
    Ok(changed > 0)
}

fn delete_entities(conn: &Connection, names: &[String]) -> Result<()> {
    for name in names.iter().collect::<BTreeSet<_>>() {
        if let Some(entity) = read_entity(conn, name)? {
            let triples = conn
                .prepare_cached(
                    "SELECT from_id, to_id, type_id FROM relation WHERE from_id=?1 OR to_id=?1",
                )
                .map_err(sql_error)?
                .query_map([entity.entity_id], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
                })
                .map_err(sql_error)?
                .collect::<rusqlite::Result<Vec<(i64, i64, i64)>>>()
                .map_err(sql_error)?;
            conn.execute(
                "DELETE FROM observation WHERE entity_id=?1",
                [entity.entity_id],
            )
            .map_err(sql_error)?;
            conn.execute(
                "DELETE FROM relation WHERE from_id=?1 OR to_id=?1",
                [entity.entity_id],
            )
            .map_err(sql_error)?;
            conn.execute(
                "INSERT INTO name_fts(name_fts,rowid,name) VALUES('delete',?1,?2)",
                params![entity.entity_id, entity.name],
            )
            .map_err(sql_error)?;
            conn.execute("DELETE FROM entity WHERE id=?1", [entity.entity_id])
                .map_err(sql_error)?;
            for (from_id, to_id, type_id) in triples {
                tombstone_relation_mirror(conn, from_id, to_id, type_id)?;
            }
        }
    }
    Ok(())
}

fn execute(
    graph: &GraphHandle,
    conn: &Connection,
    request: MutationRequest,
) -> Result<MutationResult> {
    match request {
        MutationRequest::CreateEntities { entities } => {
            let mut created = Vec::new();
            for entity in entities {
                if create_entity(graph, conn, &entity)? {
                    created.push(require_entity(conn, &entity.name)?.entity());
                }
            }
            Ok(MutationResult::Entities(created))
        }
        MutationRequest::UpsertEntities { entities } => {
            let mut result = Vec::new();
            for entity in entities {
                if let Some(existing) = read_entity(conn, &entity.name)? {
                    if existing.entity_type != entity.entity_type {
                        conn.execute(
                            "UPDATE entity SET type_id=?1 WHERE id=?2",
                            params![type_id(conn, &entity.entity_type, 0)?, existing.entity_id],
                        )
                        .map_err(sql_error)?;
                    }
                    let mut seen: BTreeSet<&str> = existing
                        .observations
                        .iter()
                        .map(|o| o.body.as_str())
                        .collect();
                    let added: Vec<ObservationInput> = entity
                        .observations
                        .iter()
                        .filter(|o| seen.insert(o.body.as_str()))
                        .cloned()
                        .collect();
                    insert_observations(graph, conn, existing.entity_id, &added)?;
                    result.push(require_entity(conn, &entity.name)?.entity());
                } else if create_entity(graph, conn, &entity)? {
                    result.push(require_entity(conn, &entity.name)?.entity());
                }
            }
            Ok(MutationResult::Entities(result))
        }
        MutationRequest::DeleteEntities { names } => {
            delete_entities(conn, &names)?;
            Ok(MutationResult::Unit)
        }
        MutationRequest::CreateRelations { relations } => {
            let mut created = Vec::new();
            for relation in relations {
                if create_relation(conn, &relation)? {
                    created.push(relation);
                }
            }
            Ok(MutationResult::Relations(created))
        }
        MutationRequest::DeleteRelations { relations } => {
            for relation in relations {
                let triples = conn.prepare_cached(
                    "SELECT rowid, from_id, to_id, type_id FROM relation \
                     WHERE from_id IN (SELECT id FROM entity WHERE name_hash=?1 AND name=?2 AND flags=0) \
                     AND to_id IN (SELECT id FROM entity WHERE name_hash=?3 AND name=?4 AND flags=0) \
                     AND type_id IN (SELECT id FROM type_dict WHERE kind=1 AND name=?5)",
                )
                .map_err(sql_error)?
                .query_map(
                    params![name_hash(&relation.from), relation.from, name_hash(&relation.to), relation.to, relation.relation_type],
                    |row| {
                        Ok((
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .map_err(sql_error)?
                .collect::<rusqlite::Result<Vec<(i64, i64, i64)>>>()
                .map_err(sql_error)?;
                conn.execute("DELETE FROM relation WHERE from_id IN (SELECT id FROM entity WHERE name_hash=?1 AND name=?2 AND flags=0) AND to_id IN (SELECT id FROM entity WHERE name_hash=?3 AND name=?4 AND flags=0) AND type_id IN (SELECT id FROM type_dict WHERE kind=1 AND name=?5)", params![name_hash(&relation.from),relation.from,name_hash(&relation.to),relation.to,relation.relation_type]).map_err(sql_error)?;
                for (from_id, to_id, type_id) in triples {
                    tombstone_relation_mirror(conn, from_id, to_id, type_id)?;
                }
            }
            Ok(MutationResult::Unit)
        }
        MutationRequest::AddObservations { observations } => {
            let mut result = Vec::new();
            for update in observations {
                let entity = require_entity(conn, &update.entity_name)?;
                let inserted =
                    insert_observations(graph, conn, entity.entity_id, &update.contents)?;
                result.push(ObservationResult {
                    entity_name: update.entity_name,
                    added_observations: inserted,
                });
            }
            Ok(MutationResult::Observations(result))
        }
        MutationRequest::DeleteObservations { observations } => {
            for update in observations {
                if update.contents.is_empty() {
                    continue;
                }
                let entity = require_entity(conn, &update.entity_name)?;
                for body in &update.contents {
                    conn.execute(
                        "DELETE FROM observation WHERE entity_id=?1 AND body=?2",
                        params![entity.entity_id, body.body],
                    )
                    .map_err(sql_error)?;
                }
            }
            Ok(MutationResult::Unit)
        }
        MutationRequest::MergeEntities { source, target } => {
            let old = require_entity(conn, &source)?;
            let into = require_entity(conn, &target)?;
            if source != target {
                // Body remains the observation identity. Equal target bodies keep
                // their metadata; newly copied rows retain the original fact/write
                // times and record this merge's immediate source as audit origin.
                let mut seen: BTreeSet<&str> =
                    into.observations.iter().map(|o| o.body.as_str()).collect();
                let mut idx: i64 = conn
                    .query_row(
                        "SELECT COALESCE(MAX(idx),-1) FROM observation WHERE entity_id=?1",
                        [into.entity_id],
                        |r| r.get(0),
                    )
                    .map_err(sql_error)?;
                for observation in &old.observations {
                    if seen.insert(&observation.body) {
                        idx += 1;
                        conn.execute("INSERT INTO observation(id,entity_id,idx,body,created_us,occurred_us,origin_entity_id,origin_entity_name) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", params![graph.next_obs_id(),into.entity_id,idx,observation.body,observation.created_at_us,observation.occurred_at_us,old.entity_id,old.name]).map_err(sql_error)?;
                    }
                }
                let relations = relations_for(conn, &source)?;
                for mut relation in relations {
                    if relation.from == source {
                        relation.from = target.clone();
                    }
                    if relation.to == source {
                        relation.to = target.clone();
                    }
                    create_relation(conn, &relation)?;
                }
                delete_entities(conn, std::slice::from_ref(&source))?;
            }
            Ok(MutationResult::Entity(
                require_entity(conn, &target)?.entity(),
            ))
        }
        MutationRequest::RenameEntity { old_name, new_name } => {
            let entity = require_entity(conn, &old_name)?;
            if old_name == new_name {
                return Ok(MutationResult::Entity(entity.entity()));
            }
            if read_entity(conn, &new_name)?.is_some() {
                return Err(MCSError::InvalidParams(format!(
                    "Entity '{new_name}' already exists"
                )));
            }
            conn.execute(
                "UPDATE entity SET name_hash=?1,name=?2 WHERE id=?3",
                params![name_hash(&new_name), new_name, entity.entity_id],
            )
            .map_err(sql_error)?;
            conn.execute(
                "INSERT INTO name_fts(name_fts,rowid,name) VALUES('delete',?1,?2)",
                params![entity.entity_id, old_name],
            )
            .map_err(sql_error)?;
            conn.execute(
                "INSERT INTO name_fts(rowid,name) VALUES(?1,?2)",
                params![entity.entity_id, new_name],
            )
            .map_err(sql_error)?;
            Ok(MutationResult::Entity(
                require_entity(conn, &new_name)?.entity(),
            ))
        }
        MutationRequest::PurgeDefinedEntities { name } => {
            let names = defined_names(conn, &name)?;
            delete_entities(conn, &names)?;
            Ok(MutationResult::Count(names.len()))
        }
        MutationRequest::Compact => {
            conn.execute_batch("PRAGMA incremental_vacuum;")
                .map_err(sql_error)?;
            Ok(MutationResult::Unit)
        }
        MutationRequest::Wipe => {
            let mut stmt = conn
                .prepare("SELECT name FROM entity WHERE flags=0")
                .map_err(sql_error)?;
            let names = stmt
                .query_map([], |row| row.get(0))
                .map_err(sql_error)?
                .collect::<rusqlite::Result<Vec<String>>>()
                .map_err(sql_error)?;
            delete_entities(conn, &names)?;
            // External-content indexes may contain orphan postings left by
            // legacy deletions. Reset the indexes inside this transaction too.
            conn.execute_batch(
                "INSERT INTO name_fts(name_fts) VALUES('delete-all');
                 INSERT INTO obs_fts(obs_fts) VALUES('delete-all');",
            )
            .map_err(sql_error)?;
            Ok(MutationResult::Unit)
        }
    }
}

fn update_counters(
    conn: &Connection,
    before: &Snapshot,
    after: &Snapshot,
    changes: &[EntityChange],
) -> Result<()> {
    let mut type_deltas: BTreeMap<(i64, &str), i64> = BTreeMap::new();
    for old in before.entities.values() {
        *type_deltas.entry((0, &old.entity_type)).or_default() -= 1;
    }
    for new in after.entities.values() {
        *type_deltas.entry((0, &new.entity_type)).or_default() += 1;
    }
    for (old, count) in &before.relation_rows {
        *type_deltas.entry((1, &old.relation_type)).or_default() -= count;
    }
    for (new, count) in &after.relation_rows {
        *type_deltas.entry((1, &new.relation_type)).or_default() += count;
    }
    // Affected types are the union of the net-delta keys and the types of
    // every entity change. Each affected type gets ONE count/revision update
    // and ONE enqueue, never one per change.
    let mut affected_types: BTreeSet<(i64, &str)> = type_deltas.keys().copied().collect();
    for change in changes {
        if let Some(before) = &change.before {
            affected_types.insert((0, before.entity_type.as_str()));
        }
        if let Some(after) = &change.after {
            affected_types.insert((0, after.entity_type.as_str()));
        }
        if let Some(delta) = &change.relation_delta {
            for relation in delta.added.iter().chain(delta.removed.iter()) {
                affected_types.insert((1, relation.relation_type.as_str()));
            }
        }
    }
    for (kind, name) in affected_types {
        let delta = type_deltas.get(&(kind, name)).copied().unwrap_or(0);
        let revision: i64 = conn
            .query_row(
                "UPDATE type_dict SET count=count+?1, revision=revision+1 WHERE kind=?2 AND name=?3 RETURNING revision",
                params![delta, kind, name],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        enqueue_taxonomy_jobs(
            conn,
            kind,
            type_id(conn, name, kind)?,
            revision,
            crate::jobs::IndexOperation::Upsert,
        )?;
    }
    let observations = |snapshot: &Snapshot| {
        snapshot
            .entities
            .values()
            .map(|e| e.observations.len() as i64)
            .sum::<i64>()
    };
    for (key, delta) in [
        (
            "entities",
            after.entities.len() as i64 - before.entities.len() as i64,
        ),
        (
            "relations",
            after.relation_rows.values().sum::<i64>() - before.relation_rows.values().sum::<i64>(),
        ),
        ("observations", observations(after) - observations(before)),
    ] {
        if delta != 0 {
            conn.execute(
                "UPDATE graph_stat SET value=value+?1 WHERE key=?2",
                params![delta, key],
            )
            .map_err(sql_error)?;
        }
    }
    let mut degrees: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
    for (relation, count) in &after.relation_rows {
        degrees.entry(&relation.from).or_default().0 += count;
        degrees.entry(&relation.to).or_default().1 += count;
    }
    for entity in changes
        .iter()
        .filter(|change| change.operation != ChangeOperation::Rename)
        .filter_map(|change| change.after.as_ref())
    {
        let (outgoing, incoming) = degrees
            .get(entity.name.as_str())
            .copied()
            .unwrap_or_default();
        conn.execute(
            "UPDATE entity SET obs_count=?1,out_deg=?2,in_deg=?3,updated_us=?4 WHERE id=?5",
            params![
                entity.observations.len() as i64,
                outgoing,
                incoming,
                now_us(),
                entity.entity_id
            ],
        )
        .map_err(sql_error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphHandle;
    use crate::storage::{Durability, SqliteTuning};
    use crate::types::EntityInput as Entity;
    use std::num::NonZeroUsize;
    use std::ops::Deref;
    use std::path::PathBuf;

    struct TestKg(GraphHandle, PathBuf);

    impl Deref for TestKg {
        type Target = GraphHandle;
        fn deref(&self) -> &GraphHandle {
            &self.0
        }
    }

    impl Drop for TestKg {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.1);
            let _ = std::fs::remove_file(self.1.with_extension("db-wal"));
            let _ = std::fs::remove_file(self.1.with_extension("db-shm"));
        }
    }

    fn new_kg() -> TestKg {
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;
        static COUNTER: AtomicU64 = AtomicU64::new(200_000);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("kg_mutation_{}_{}.db", std::process::id(), n));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
        let kg = GraphHandle::new(
            &path,
            Durability::Async,
            SqliteTuning::default(),
            NonZeroUsize::new(10000).unwrap(),
            4,
        )
        .expect("create test kg");
        TestKg(kg, path)
    }

    /// One managed serving profile, so taxonomy jobs land pending instead of
    /// held.
    fn serving_profile(kg: &GraphHandle) -> Uuid {
        let profile = Uuid::new_v4();
        let conn = kg.writer.lock();
        conn.execute(
            "UPDATE index_profile_registry SET state='Active', serving_profile=?1 WHERE store_key='default'",
            [profile.to_string()],
        )
        .expect("activate serving profile");
        profile
    }

    fn entity(name: &str, entity_type: &str) -> Entity {
        Entity {
            name: name.into(),
            entity_type: entity_type.into(),
            observations: vec![],
        }
    }

    fn relation(from: &str, to: &str, relation_type: &str) -> Relation {
        Relation {
            from: from.into(),
            to: to.into(),
            relation_type: relation_type.into(),
        }
    }

    /// (subject_kind, subject_id, subject_revision, operation, state)
    fn taxonomy_jobs(kg: &GraphHandle) -> Vec<(i64, i64, i64, String, String)> {
        let conn = kg.writer.lock();
        let mut stmt = conn
            .prepare("SELECT subject_kind, subject_id, subject_revision, operation, state FROM taxonomy_job ORDER BY subject_kind, subject_id")
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    /// (count, revision) of one type_dict row.
    fn type_row(kg: &GraphHandle, kind: i64, name: &str) -> (i64, i64) {
        let conn = kg.writer.lock();
        conn.query_row(
            "SELECT count, revision FROM type_dict WHERE kind=?1 AND name=?2",
            params![kind, name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    }

    /// (id, revision, deleted) of the mirror row for one relation triple.
    fn mirror_row(kg: &GraphHandle, from: &str, to: &str, relation_type: &str) -> (i64, i64, i64) {
        let conn = kg.writer.lock();
        conn.query_row(
            "SELECT m.id, m.revision, m.deleted
             FROM taxonomy_relation m
             JOIN entity f ON f.id = m.from_id
             JOIN entity t ON t.id = m.to_id
             JOIN type_dict d ON d.id = m.type_id
             WHERE f.name=?1 AND t.name=?2 AND d.name=?3 AND d.kind=1",
            params![from, to, relation_type],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
    }

    #[test]
    fn create_entity_enqueues_one_type_job_at_revision_one() {
        let kg = new_kg();
        serving_profile(&kg);
        kg.create_entities(&[entity("ada", "person")]).unwrap();

        let jobs = taxonomy_jobs(&kg);
        assert_eq!(jobs.len(), 1, "exactly one taxonomy job expected");
        assert_eq!(jobs[0].0, 0, "entity type job expected");
        assert_eq!(jobs[0].2, 1, "first revision expected");
        assert_eq!(jobs[0].3, "upsert");
        assert_eq!(jobs[0].4, "pending");
        let (count, revision) = type_row(&kg, 0, "person");
        assert_eq!((count, revision), (1, 1));
    }

    #[test]
    fn second_entity_of_same_type_upserts_job_to_revision_two() {
        let kg = new_kg();
        serving_profile(&kg);
        kg.create_entities(&[entity("ada", "person")]).unwrap();
        kg.create_entities(&[entity("bob", "person")]).unwrap();

        let jobs = taxonomy_jobs(&kg);
        assert_eq!(jobs.len(), 1, "one job row per affected type expected");
        assert_eq!((jobs[0].0, jobs[0].2, jobs[0].3.as_str()), (0, 2, "upsert"));
        let (count, revision) = type_row(&kg, 0, "person");
        assert_eq!((count, revision), (2, 2));
    }

    #[test]
    fn rename_bumps_type_revision_without_count_change() {
        let kg = new_kg();
        serving_profile(&kg);
        kg.create_entities(&[entity("ada", "person")]).unwrap();
        kg.rename_entity("ada", "ada lovelace").unwrap();

        let (count, revision) = type_row(&kg, 0, "person");
        assert_eq!((count, revision), (1, 2), "count stable, revision bumped");
        let jobs = taxonomy_jobs(&kg);
        assert_eq!(jobs.len(), 1);
        assert_eq!((jobs[0].0, jobs[0].2, jobs[0].3.as_str()), (0, 2, "upsert"));
    }

    #[test]
    fn create_relation_writes_mirror(){
        let kg = new_kg();
        serving_profile(&kg);
        kg.create_entities(&[entity("ada", "person"), entity("bob", "person")]).unwrap();
        kg.create_relations(&[relation("ada", "bob", "knows")]).unwrap();

        let (mirror_id, mirror_revision, deleted) = mirror_row(&kg, "ada", "bob", "knows");
        assert_eq!((mirror_revision, deleted), (1, 0), "fresh mirror expected");

        let jobs = taxonomy_jobs(&kg);
        let kind2: Vec<_> = jobs.iter().filter(|job| job.0 == 2).collect();
        assert_eq!(kind2.len(), 1, "one kind-2 job expected");
        assert_eq!((kind2[0].1, kind2[0].2, kind2[0].3.as_str()), (mirror_id, 1, "upsert"));
        let (count, revision) = type_row(&kg, 1, "knows");
        assert_eq!((count, revision), (1, 1));
    }

    #[test]
    fn delete_relation_tombstones_mirror_and_enqueues_delete(){
        let kg = new_kg();
        serving_profile(&kg);
        kg.create_entities(&[entity("ada", "person"), entity("bob", "person")]).unwrap();
        kg.create_relations(&[relation("ada", "bob", "knows")]).unwrap();
        let (mirror_id, _, _) = mirror_row(&kg, "ada", "bob", "knows");

        kg.delete_relations(&[relation("ada", "bob", "knows")]).unwrap();

        let (mirror_id_after, revision, deleted) = mirror_row(&kg, "ada", "bob", "knows");
        assert_eq!((mirror_id_after, revision, deleted), (mirror_id, 2, 1), "tombstone expected");
        let conn = kg.writer.lock();
        let remaining: i64 = conn.query_row("SELECT COUNT(*) FROM relation", [], |r| r.get(0)).unwrap();
        drop(conn);
        assert_eq!(remaining, 0, "physical triple deleted");
        let jobs = taxonomy_jobs(&kg);
        let kind2: Vec<_> = jobs.iter().filter(|job| job.0 == 2).collect();
        assert_eq!(kind2.len(), 1);
        assert_eq!((kind2[0].1, kind2[0].2, kind2[0].3.as_str()), (mirror_id, 2, "delete"));
    }

    #[test]
    fn one_mutation_with_many_changes_bumps_each_type_once(){
        let kg = new_kg();
        serving_profile(&kg);
        // Two creations of one new type in a single call. The union ruling
        // demands one revision bump and one job row for the affected type.
        kg.create_entities(&[entity("ada", "person"), entity("bob", "person")]).unwrap();

        let (count, revision) = type_row(&kg, 0, "person");
        assert_eq!((count, revision), (2, 1), "one bump, not one per change");
        let jobs = taxonomy_jobs(&kg);
        assert_eq!(jobs.len(), 1);
        assert_eq!((jobs[0].0, jobs[0].2), (0, 1));
    }

    #[test]
    fn delete_entity_tombstones_its_relation_mirrors(){
        let kg = new_kg();
        serving_profile(&kg);
        kg.create_entities(&[entity("ada", "person"), entity("bob", "person")]).unwrap();
        kg.create_relations(&[relation("ada", "bob", "knows")]).unwrap();
        let (mirror_id, _, _) = mirror_row(&kg, "ada", "bob", "knows");

        kg.delete_entities(&["ada".into()]).unwrap();

        let conn = kg.writer.lock();
        let (revision, deleted): (i64, i64) = conn
            .query_row(
                "SELECT revision, deleted FROM taxonomy_relation WHERE id=?1",
                [mirror_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        drop(conn);
        assert_eq!((revision, deleted), (2, 1), "cascade tombstone expected");
        let jobs = taxonomy_jobs(&kg);
        let kind2: Vec<_> = jobs.iter().filter(|job| job.0 == 2).collect();
        assert_eq!(kind2.len(), 1);
        assert_eq!((kind2[0].2, kind2[0].3.as_str()), (2, "delete"));
        let (count, revision) = type_row(&kg, 0, "person");
        // Bump once at entity creation, once at relation creation (the
        // relation delta makes an entity change for each endpoint), once at
        // the delete. Count drops to the one survivor.
        assert_eq!((count, revision), (1, 3), "survivor count and bumps");
    }
}
