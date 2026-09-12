//! Provider-neutral index jobs and vector-space registry.
use crate::errors::{MCSError, Result};
use crate::events::{Lease, lease_until, parse_uuid, sha256, sql_error};
use crate::graph::TxGuard;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Profile ids that must receive an index-job update. When no managed
/// profile serves the store, the list holds the nil id so the job is held.
pub(crate) fn serving_profile_ids(conn: &Connection) -> Result<Vec<Uuid>> {
    let mut stmt = conn.prepare("SELECT serving_profile FROM index_profile_registry WHERE serving_profile IS NOT NULL UNION SELECT candidate_profile FROM index_profile_registry WHERE state='Rebuilding' AND candidate_profile IS NOT NULL").map_err(sql_error)?;
    let mut profiles = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(sql_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_error)?;
    if profiles.is_empty() {
        profiles.push(uuid::Uuid::nil().to_string());
    }
    profiles
        .iter()
        .map(|profile| parse_uuid(profile))
        .collect()
}

pub(crate) fn enqueue_change(
    conn: &Connection,
    entity_id: i64,
    revision: i64,
    deleted: bool,
) -> Result<()> {
    // A nil profile is an explicitly held LegacyCompat job, never a claimable
    // provider profile. Managed serving and candidate profiles receive updates.
    let profiles = serving_profile_ids(conn)?;
    for profile in profiles {
        let state = if profile.is_nil() {
            "held"
        } else {
            "pending"
        };
        conn.execute("INSERT INTO index_job(entity_id,profile_id,entity_revision,operation,state) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(entity_id,profile_id) DO UPDATE SET entity_revision=excluded.entity_revision,operation=excluded.operation,state=excluded.state,lease_token=NULL,lease_epoch=lease_epoch+1,lease_until_us=0,attempts=0,next_attempt_us=0,last_error=NULL", params![entity_id,profile.to_string(),revision,if deleted {"delete"} else {"upsert"},state]).map_err(sql_error)?;
        conn.execute(
            "UPDATE ann_generation SET full_scan_generation=NULL WHERE profile_id=?1",
            [profile.to_string()],
        )
        .map_err(sql_error)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Normalization {
    None,
    L2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DistanceMetric {
    Cosine,
    InnerProduct,
    L2Squared,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexProfile {
    pub id: Uuid,
    pub store_key: String,
    pub provider_kind: String,
    pub model: String,
    pub dimensions: u32,
    pub representation_version: String,
    pub normalization: Normalization,
    pub distance_metric: DistanceMetric,
    pub vector_encoding_version: String,
}

impl IndexProfile {
    pub fn validate(&self) -> Result<()> {
        if self.id.is_nil()
            || self.store_key != "default"
            || self.dimensions == 0
            || self.dimensions > 65_536
            || self.vector_encoding_version != "f32le-v1"
            || [
                &self.provider_kind,
                &self.model,
                &self.representation_version,
            ]
            .iter()
            .any(|s| s.trim().is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err(MCSError::InvalidParams("invalid index profile".into()));
        }
        Ok(())
    }

    /// serde_json's default sorted object map is the canonical key order.
    pub fn fingerprint(&self) -> Result<String> {
        self.validate()?;
        let mut value = serde_json::to_value(self)?;
        value
            .as_object_mut()
            .ok_or_else(|| MCSError::MemoryError("profile must be an object".into()))?
            .remove("id");
        Ok(sha256(&serde_json::to_vec(&value)?))
    }

    pub fn validate_vector(&self, vector: &[f32]) -> Result<()> {
        if vector.len() != self.dimensions as usize || vector.iter().any(|x| !x.is_finite()) {
            return Err(MCSError::InvalidParams(
                "vector dimensions or finite-value validation failed".into(),
            ));
        }
        if self.normalization == Normalization::L2 {
            let norm: f64 = vector.iter().map(|x| f64::from(*x).powi(2)).sum();
            if (norm - 1.0).abs() > 1e-4 {
                return Err(MCSError::InvalidParams(
                    "vector is not L2 normalized".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StoreState {
    LegacyCompat,
    Active(Uuid),
    Rebuilding {
        serving: Option<Uuid>,
        candidate: Uuid,
    },
    Failed {
        serving: Option<Uuid>,
        candidate: Uuid,
        reason: String,
    },
}

pub struct IndexProfileRegistry<'a> {
    conn: &'a Connection,
}

impl<'a> IndexProfileRegistry<'a> {
    pub const fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// The connection this registry reads. The server crate serves taxonomy
    /// snapshots through it: the generation read, the vector rows and the
    /// publish mark then share one transaction view with the registry state.
    pub fn connection(&self) -> &Connection {
        self.conn
    }

    pub fn get(&self, id: Uuid) -> Result<IndexProfile> {
        let text: String = self
            .conn
            .query_row(
                "SELECT definition FROM index_profile WHERE id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn state(&self, store_key: &str) -> Result<StoreState> {
        let (state,serving,candidate,reason): (String,Option<String>,Option<String>,Option<String>) = self.conn.query_row("SELECT state,serving_profile,candidate_profile,failure_reason FROM index_profile_registry WHERE store_key=?1", [store_key], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(sql_error)?;
        let serving = serving.as_deref().map(parse_uuid).transpose()?;
        let candidate = candidate.as_deref().map(parse_uuid).transpose()?;
        match (state.as_str(), serving, candidate, reason) {
            ("LegacyCompat", None, None, None) => Ok(StoreState::LegacyCompat),
            ("Active", Some(profile), None, None) => Ok(StoreState::Active(profile)),
            ("Rebuilding", serving, Some(candidate), None) => {
                Ok(StoreState::Rebuilding { serving, candidate })
            }
            ("Failed", serving, Some(candidate), Some(reason)) => Ok(StoreState::Failed {
                serving,
                candidate,
                reason,
            }),
            _ => Err(MCSError::MemoryError(
                "invalid persisted profile registry state".into(),
            )),
        }
    }

    pub fn serving_profile(&self, store_key: &str) -> Result<Option<Uuid>> {
        Ok(match self.state(store_key)? {
            StoreState::LegacyCompat => None,
            StoreState::Active(profile) => Some(profile),
            StoreState::Rebuilding { serving, .. } | StoreState::Failed { serving, .. } => serving,
        })
    }

    /// Must be checked within the same write transaction as a legacy mutation.
    pub fn ensure_legacy_writes(&self, store_key: &str) -> Result<()> {
        if self.state(store_key)? != StoreState::LegacyCompat {
            return Err(MCSError::InvalidParams(
                "direct_vector_writes_disabled".into(),
            ));
        }
        Ok(())
    }

    pub fn begin_rebuild(&self, profile: &IndexProfile) -> Result<()> {
        let fingerprint = profile.fingerprint()?;
        let tx = TxGuard::begin(self.conn)?;
        if matches!(
            self.state(&profile.store_key)?,
            StoreState::Rebuilding { .. }
        ) {
            return Err(MCSError::InvalidParams(
                "profile rebuild already in progress".into(),
            ));
        }
        if self.serving_profile(&profile.store_key)? == Some(profile.id) {
            return Err(MCSError::InvalidParams(
                "cannot rebuild into serving profile".into(),
            ));
        }
        // Profiles are immutable. Reusing a retired/candidate ID would also
        // reuse its generation and vectors, defeating the rebuild boundary.
        self.conn
            .execute(
                "INSERT INTO index_profile VALUES(?1,?2,?3,?4,'Rebuilding')",
                params![
                    profile.id.to_string(),
                    profile.store_key,
                    fingerprint,
                    serde_json::to_string(profile)?
                ],
            )
            .map_err(sql_error)?;
        self.conn.execute("UPDATE index_profile SET state='Retired' WHERE id=(SELECT candidate_profile FROM index_profile_registry WHERE store_key=?1)", [&profile.store_key]).map_err(sql_error)?;
        self.conn.execute("UPDATE index_profile_registry SET state='Rebuilding',candidate_profile=?2,failure_reason=NULL WHERE store_key=?1", params![profile.store_key,profile.id.to_string()]).map_err(sql_error)?;
        self.conn
            .execute(
                "INSERT INTO ann_generation(profile_id) VALUES(?1)",
                [profile.id.to_string()],
            )
            .map_err(sql_error)?;
        self.conn.execute("INSERT INTO entity_revision SELECT id,1,0 FROM entity WHERE flags=0 ON CONFLICT(entity_id) DO NOTHING", []).map_err(sql_error)?;
        self.conn.execute("INSERT INTO index_job(entity_id,profile_id,entity_revision,operation) SELECT e.id,?1,r.revision,'upsert' FROM entity e JOIN entity_revision r ON r.entity_id=e.id WHERE e.flags=0", [profile.id.to_string()]).map_err(sql_error)?;
        tx.commit()
    }

    pub fn fail_rebuild(&self, candidate: Uuid, reason: &str) -> Result<()> {
        let tx = TxGuard::begin(self.conn)?;
        let changed = self.conn.execute("UPDATE index_profile_registry SET state='Failed',failure_reason=?2 WHERE state='Rebuilding' AND candidate_profile=?1", params![candidate.to_string(),reason.chars().take(2048).collect::<String>()]).map_err(sql_error)?;
        if changed != 1 {
            return Err(MCSError::InvalidParams(
                "candidate is not rebuilding".into(),
            ));
        }
        self.conn.execute("UPDATE index_job SET state='held',lease_token=NULL,lease_epoch=lease_epoch+1 WHERE profile_id=?1 AND state!='done'", [candidate.to_string()]).map_err(sql_error)?;
        tx.commit()
    }

    pub fn activate(&self, candidate: Uuid) -> Result<()> {
        let tx = TxGuard::begin(self.conn)?;
        let profile = self.get(candidate)?;
        if !matches!(self.state(&profile.store_key)?, StoreState::Rebuilding {candidate: c,..} if c==candidate)
        {
            return Err(MCSError::InvalidParams(
                "candidate is not rebuilding".into(),
            ));
        }
        verify_vectors_current(self.conn, candidate)?;
        let generation = AnnGenerationRepository::new(self.conn).get(candidate)?;
        if generation.full_scan_generation != Some(generation.durable_generation)
            || generation.published_generation != generation.durable_generation
        {
            return Err(MCSError::InvalidParams(
                "candidate requires verified Full scan and matching published ANN generation"
                    .into(),
            ));
        }
        self.conn
            .execute(
                "UPDATE index_profile SET state='Retired' WHERE store_key=?1 AND state='Active'",
                [&profile.store_key],
            )
            .map_err(sql_error)?;
        self.conn
            .execute(
                "UPDATE index_profile SET state='Active' WHERE id=?1",
                [candidate.to_string()],
            )
            .map_err(sql_error)?;
        self.conn.execute("UPDATE index_profile_registry SET state='Active',serving_profile=?2,candidate_profile=NULL,failure_reason=NULL WHERE store_key=?1", params![profile.store_key,candidate.to_string()]).map_err(sql_error)?;
        self.conn.execute("UPDATE index_job SET state='held',lease_token=NULL,lease_epoch=lease_epoch+1 WHERE profile_id!=?1 AND state!='done'", [candidate.to_string()]).map_err(sql_error)?;
        tx.commit()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum IndexOperation {
    Upsert,
    Delete,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexJob {
    pub entity_id: i64,
    pub entity_revision: i64,
    pub profile_id: Uuid,
    pub operation: IndexOperation,
    pub lease: Lease,
    pub attempts: i64,
}

pub struct IndexJobRepository<'a> {
    conn: &'a Connection,
}

impl<'a> IndexJobRepository<'a> {
    pub const fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn claim_due(&self, now: i64, duration_us: i64) -> Result<Option<IndexJob>> {
        let until = lease_until(now, duration_us)?;
        let tx = TxGuard::begin(self.conn)?;
        let row: Option<(i64,String,i64,String,i64,i64)> = self.conn.query_row("SELECT entity_id,profile_id,entity_revision,operation,lease_epoch,attempts FROM index_job j WHERE ((state='pending' AND next_attempt_us<=?1) OR (state='leased' AND lease_until_us<=?1)) AND EXISTS(SELECT 1 FROM index_profile_registry r WHERE r.serving_profile=j.profile_id OR (r.state='Rebuilding' AND r.candidate_profile=j.profile_id)) ORDER BY next_attempt_us,entity_id,profile_id LIMIT 1", [now], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional().map_err(sql_error)?;
        let job = row.map(|(entity_id,profile,revision,operation,epoch,attempts)| -> Result<IndexJob> {
            let token = Uuid::new_v4();
            self.conn.execute("UPDATE index_job SET state='leased',lease_token=?3,lease_epoch=lease_epoch+1,lease_until_us=?4,attempts=attempts+1 WHERE entity_id=?1 AND profile_id=?2", params![entity_id,profile,token.to_string(),until]).map_err(sql_error)?;
            Ok(IndexJob { entity_id,entity_revision:revision,profile_id:parse_uuid(&profile)?,operation:match operation.as_str() { "upsert"=>IndexOperation::Upsert,"delete"=>IndexOperation::Delete,_=>return Err(MCSError::MemoryError("invalid index operation".into())) },lease:Lease {token,epoch:epoch+1,until_us:until},attempts:attempts+1 })
        }).transpose()?;
        tx.commit()?;
        Ok(job)
    }

    pub fn renew(&self, job: &IndexJob, now: i64, duration_us: i64) -> Result<bool> {
        let until = lease_until(now, duration_us)?;
        let tx = TxGuard::begin(self.conn)?;
        let changed = self.conn.execute("UPDATE index_job SET lease_until_us=?6 WHERE entity_id=?1 AND profile_id=?2 AND lease_token=?3 AND lease_epoch=?4 AND state='leased' AND lease_until_us>?5", params![job.entity_id,job.profile_id.to_string(),job.lease.token.to_string(),job.lease.epoch,now,until]).map_err(sql_error)?;
        tx.commit()?;
        Ok(changed == 1)
    }

    pub fn retry(
        &self,
        job: &IndexJob,
        now: i64,
        next_attempt_us: i64,
        error: &str,
        dead: bool,
    ) -> Result<bool> {
        let tx = TxGuard::begin(self.conn)?;
        let changed = self.conn.execute("UPDATE index_job SET state=?6,next_attempt_us=?7,last_error=?8 WHERE entity_id=?1 AND profile_id=?2 AND lease_token=?3 AND lease_epoch=?4 AND state='leased' AND lease_until_us>?5", params![job.entity_id,job.profile_id.to_string(),job.lease.token.to_string(),job.lease.epoch,now,if dead {"dead"} else {"pending"},next_attempt_us,error.chars().take(2048).collect::<String>()]).map_err(sql_error)?;
        // A dead-lettered entity must not keep a stale vector in the
        // candidate: the verified full-scan gate would otherwise publish a
        // snapshot serving an outdated embedding. Its next write re-enqueues
        // the entity from scratch.
        if changed == 1 && dead {
            self.conn
                .execute(
                    "DELETE FROM profile_vector WHERE profile_id=?1 AND entity_id=?2",
                    params![job.profile_id.to_string(), job.entity_id],
                )
                .map_err(sql_error)?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Fenced durable effect and completion are indivisible. The caller builds
    /// or swaps its ANN reader only after this returns true; dirty generation
    /// persists if it crashes before doing so. A repeated completion is a no-op.
    pub fn commit_vector(
        &self,
        job: &IndexJob,
        now: i64,
        vector: Option<&[f32]>,
        source: &str,
    ) -> Result<bool> {
        let tx = TxGuard::begin(self.conn)?;
        let state: Option<(String,i64)> = self.conn.query_row("SELECT state,lease_until_us FROM index_job WHERE entity_id=?1 AND profile_id=?2 AND entity_revision=?3 AND lease_token=?4 AND lease_epoch=?5", params![job.entity_id,job.profile_id.to_string(),job.entity_revision,job.lease.token.to_string(),job.lease.epoch], |r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(sql_error)?;
        if matches!(&state,Some((state,_)) if state=="done") {
            tx.commit()?;
            return Ok(true);
        }
        if !matches!(state,Some((state,until)) if state=="leased" && until>now) {
            return Ok(false);
        }
        if !profile_writable(self.conn, job.profile_id)? {
            return Ok(false);
        }
        let revision: Option<(i64, bool)> = self
            .conn
            .query_row(
                "SELECT revision,deleted FROM entity_revision WHERE entity_id=?1",
                [job.entity_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_error)?;
        if revision != Some((job.entity_revision, job.operation == IndexOperation::Delete)) {
            return Ok(false);
        }
        let profile = IndexProfileRegistry::new(self.conn).get(job.profile_id)?;
        match (job.operation, vector) {
            (IndexOperation::Upsert, Some(vector)) => {
                profile.validate_vector(vector)?;
                let bytes: Vec<u8> = vector.iter().flat_map(|x| x.to_le_bytes()).collect();
                self.conn.execute("INSERT INTO profile_vector VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(profile_id,entity_id) DO UPDATE SET entity_revision=excluded.entity_revision,blob=excluded.blob,created_at_us=excluded.created_at_us,source=excluded.source", params![job.profile_id.to_string(),job.entity_id,job.entity_revision,bytes,now,source]).map_err(sql_error)?;
            }
            (IndexOperation::Delete, None) => {
                self.conn
                    .execute(
                        "DELETE FROM profile_vector WHERE profile_id=?1 AND entity_id=?2",
                        params![job.profile_id.to_string(), job.entity_id],
                    )
                    .map_err(sql_error)?;
            }
            _ => {
                return Err(MCSError::InvalidParams(
                    "vector payload does not match job operation".into(),
                ));
            }
        }
        self.conn
            .execute(
                "UPDATE index_job SET state='done' WHERE entity_id=?1 AND profile_id=?2",
                params![job.entity_id, job.profile_id.to_string()],
            )
            .map_err(sql_error)?;
        self.conn.execute("UPDATE ann_generation SET durable_generation=durable_generation+1,full_scan_generation=NULL WHERE profile_id=?1", [job.profile_id.to_string()]).map_err(sql_error)?;
        tx.commit()?;
        Ok(true)
    }
}

fn profile_writable(conn: &Connection, profile: Uuid) -> Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM index_profile_registry WHERE serving_profile=?1 OR (state='Rebuilding' AND candidate_profile=?1))", [profile.to_string()], |r| r.get(0)).map_err(sql_error)
}

fn verify_vectors_current(conn: &Connection, profile: Uuid) -> Result<()> {
    // A dead-lettered job declares its entity unindexable: the gate must not
    // block the whole store on it. The worker deletes the entity's vector row
    // when it dead-letters, so no stale vector sneaks into the snapshot.
    let invalid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM entity e LEFT JOIN entity_revision r ON r.entity_id=e.id LEFT JOIN profile_vector v ON v.entity_id=e.id AND v.profile_id=?1 WHERE e.flags=0 AND NOT EXISTS(SELECT 1 FROM index_job d WHERE d.entity_id=e.id AND d.profile_id=?1 AND d.state='dead') AND (v.entity_id IS NULL OR r.revision IS NULL OR v.entity_revision!=r.revision)) OR EXISTS(SELECT 1 FROM profile_vector v LEFT JOIN entity e ON e.id=v.entity_id WHERE v.profile_id=?1 AND (e.id IS NULL OR e.flags!=0)) OR EXISTS(SELECT 1 FROM index_job WHERE profile_id=?1 AND state NOT IN ('done','dead'))", [profile.to_string()], |r| r.get(0)).map_err(sql_error)?;
    if invalid {
        return Err(MCSError::InvalidParams(
            "candidate Full scan has missing or stale vectors/jobs".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AnnGeneration {
    pub profile_id: Uuid,
    pub durable_generation: i64,
    pub published_generation: i64,
    pub full_scan_generation: Option<i64>,
}

pub struct AnnGenerationRepository<'a> {
    conn: &'a Connection,
}

impl<'a> AnnGenerationRepository<'a> {
    pub const fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn get(&self, profile: Uuid) -> Result<AnnGeneration> {
        self.conn.query_row("SELECT durable_generation,published_generation,full_scan_generation FROM ann_generation WHERE profile_id=?1", [profile.to_string()], |r| Ok(AnnGeneration {profile_id:profile,durable_generation:r.get(0)?,published_generation:r.get(1)?,full_scan_generation:r.get(2)?})).map_err(sql_error)
    }

    pub fn verify_full_scan(&self, profile: Uuid) -> Result<()> {
        let tx = TxGuard::begin(self.conn)?;
        verify_vectors_current(self.conn, profile)?;
        let changed = self.conn.execute("UPDATE ann_generation SET full_scan_generation=durable_generation WHERE profile_id=?1", [profile.to_string()]).map_err(sql_error)?;
        if changed != 1 {
            return Err(MCSError::InvalidParams("unknown ANN profile".into()));
        }
        tx.commit()
    }

    /// Call only after building the replacement reader from a consistent read
    /// snapshot at this generation. A concurrent durable update rejects publish.
    pub fn mark_published(&self, profile: Uuid, generation: i64) -> Result<bool> {
        let tx = TxGuard::begin(self.conn)?;
        let changed = self.conn.execute("UPDATE ann_generation SET published_generation=?2 WHERE profile_id=?1 AND durable_generation=?2", params![profile.to_string(),generation]).map_err(sql_error)?;
        tx.commit()?;
        Ok(changed == 1)
    }
}

/// Queue one taxonomy subject for one profile. A nil profile means an
/// explicitly held job, in the same way the entity path holds its LegacyCompat
/// fallback. The generation marker resets so a later full-scan verification
/// re-checks this kind from scratch.
pub(crate) fn enqueue_taxonomy(
    conn: &Connection,
    kind: i64,
    id: i64,
    revision: i64,
    operation: IndexOperation,
    profile_id: Uuid,
) -> Result<()> {
    // A nil profile is an explicitly held job, mirroring the entity path's
    // LegacyCompat fallback: the store has no managed profile to serve it.
    let state = if profile_id.is_nil() { "held" } else { "pending" };
    conn.execute("INSERT INTO taxonomy_job(subject_kind,subject_id,profile_id,subject_revision,operation,state) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(subject_kind,subject_id,profile_id) DO UPDATE SET subject_revision=excluded.subject_revision,operation=excluded.operation,state=excluded.state,lease_token=NULL,lease_epoch=lease_epoch+1,lease_until_us=0,attempts=0,next_attempt_us=0,last_error=NULL", params![kind,id,profile_id.to_string(),revision,match operation { IndexOperation::Upsert => "upsert", IndexOperation::Delete => "delete" },state]).map_err(sql_error)?;
    conn.execute(
        "UPDATE taxonomy_ann_generation SET full_scan_generation=NULL WHERE profile_id=?1 AND subject_kind=?2",
        params![profile_id.to_string(), kind],
    )
    .map_err(sql_error)?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaxonomyJob {
    pub subject_kind: i64,
    pub subject_id: i64,
    pub subject_revision: i64,
    pub profile_id: Uuid,
    pub operation: IndexOperation,
    pub lease: Lease,
    pub attempts: i64,
}

pub struct TaxonomyJobRepository<'a> {
    conn: &'a Connection,
}

impl<'a> TaxonomyJobRepository<'a> {
    pub const fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn claim_due(&self, now: i64, duration_us: i64) -> Result<Option<TaxonomyJob>> {
        let until = lease_until(now, duration_us)?;
        let tx = TxGuard::begin(self.conn)?;
        let row: Option<(i64,i64,String,i64,String,i64,i64)> = self.conn.query_row("SELECT subject_kind,subject_id,profile_id,subject_revision,operation,lease_epoch,attempts FROM taxonomy_job j WHERE ((state='pending' AND next_attempt_us<=?1) OR (state='leased' AND lease_until_us<=?1)) AND EXISTS(SELECT 1 FROM index_profile_registry r WHERE r.serving_profile=j.profile_id OR (r.state='Rebuilding' AND r.candidate_profile=j.profile_id)) ORDER BY next_attempt_us,subject_kind,subject_id,profile_id LIMIT 1", [now], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional().map_err(sql_error)?;
        let job = row.map(|(kind,id,profile,revision,operation,epoch,attempts)| -> Result<TaxonomyJob> {
            let token = Uuid::new_v4();
            self.conn.execute("UPDATE taxonomy_job SET state='leased',lease_token=?4,lease_epoch=lease_epoch+1,lease_until_us=?5,attempts=attempts+1 WHERE subject_kind=?1 AND subject_id=?2 AND profile_id=?3", params![kind,id,profile,token.to_string(),until]).map_err(sql_error)?;
            Ok(TaxonomyJob { subject_kind:kind,subject_id:id,subject_revision:revision,profile_id:parse_uuid(&profile)?,operation:match operation.as_str() { "upsert"=>IndexOperation::Upsert,"delete"=>IndexOperation::Delete,_=>return Err(MCSError::MemoryError("invalid taxonomy operation".into())) },lease:Lease {token,epoch:epoch+1,until_us:until},attempts:attempts+1 })
        }).transpose()?;
        tx.commit()?;
        Ok(job)
    }

    pub fn renew(&self, job: &TaxonomyJob, now: i64, duration_us: i64) -> Result<bool> {
        let until = lease_until(now, duration_us)?;
        let tx = TxGuard::begin(self.conn)?;
        let changed = self.conn.execute("UPDATE taxonomy_job SET lease_until_us=?7 WHERE subject_kind=?1 AND subject_id=?2 AND profile_id=?3 AND lease_token=?4 AND lease_epoch=?5 AND state='leased' AND lease_until_us>?6", params![job.subject_kind,job.subject_id,job.profile_id.to_string(),job.lease.token.to_string(),job.lease.epoch,now,until]).map_err(sql_error)?;
        tx.commit()?;
        Ok(changed == 1)
    }

    pub fn retry(
        &self,
        job: &TaxonomyJob,
        now: i64,
        next_attempt_us: i64,
        error: &str,
        dead: bool,
    ) -> Result<bool> {
        let tx = TxGuard::begin(self.conn)?;
        let changed = self.conn.execute("UPDATE taxonomy_job SET state=?7,next_attempt_us=?8,last_error=?9 WHERE subject_kind=?1 AND subject_id=?2 AND profile_id=?3 AND lease_token=?4 AND lease_epoch=?5 AND state='leased' AND lease_until_us>?6", params![job.subject_kind,job.subject_id,job.profile_id.to_string(),job.lease.token.to_string(),job.lease.epoch,now,if dead {"dead"} else {"pending"},next_attempt_us,error.chars().take(2048).collect::<String>()]).map_err(sql_error)?;
        // A dead-lettered subject must not keep a stale vector row: the
        // full-scan gate would otherwise publish an outdated embedding. Its
        // next write re-enqueues the subject from scratch, mirroring the
        // entity dead-letter cleanup.
        if changed == 1 && dead {
            self.conn
                .execute(
                    "DELETE FROM taxonomy_vector WHERE profile_id=?1 AND subject_kind=?2 AND subject_id=?3",
                    params![job.profile_id.to_string(), job.subject_kind, job.subject_id],
                )
                .map_err(sql_error)?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Fenced durable effect and completion are indivisible, mirroring the
    /// entity commit. A repeated completion is a no-op.
    pub fn commit_vector(
        &self,
        job: &TaxonomyJob,
        now: i64,
        vector: Option<&[f32]>,
        source: &str,
    ) -> Result<bool> {
        let tx = TxGuard::begin(self.conn)?;
        let state: Option<(String,i64)> = self.conn.query_row("SELECT state,lease_until_us FROM taxonomy_job WHERE subject_kind=?1 AND subject_id=?2 AND profile_id=?3 AND subject_revision=?4 AND lease_token=?5 AND lease_epoch=?6", params![job.subject_kind,job.subject_id,job.profile_id.to_string(),job.subject_revision,job.lease.token.to_string(),job.lease.epoch], |r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(sql_error)?;
        if matches!(&state,Some((state,_)) if state=="done") {
            tx.commit()?;
            return Ok(true);
        }
        if !matches!(state,Some((state,until)) if state=="leased" && until>now) {
            return Ok(false);
        }
        let (source_revision, source_deleted): (i64,bool) = match job.subject_kind {
            0 | 1 => {
                match self.conn.query_row("SELECT revision FROM type_dict WHERE id=?1 AND kind=?2", params![job.subject_id,job.subject_kind], |r| r.get(0)).optional().map_err(sql_error)? {
                    Some(revision) => (revision, false),
                    None => return Ok(false),
                }
            }
            2 => {
                match self.conn.query_row("SELECT revision,deleted FROM taxonomy_relation WHERE id=?1", [job.subject_id], |r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(sql_error)? {
                    Some(row) => row,
                    None => return Ok(false),
                }
            }
            _ => return Err(MCSError::MemoryError("invalid taxonomy subject kind".into())),
        };
        if source_revision != job.subject_revision {
            return Ok(false);
        }
        // Kinds 0/1 carry no tombstone and never enqueue deletes. Kind 2
        // requires the mirror row's deleted flag to match the operation.
        if job.subject_kind == 2 && source_deleted != (job.operation == IndexOperation::Delete) {
            return Ok(false);
        }
        match (job.operation, vector) {
            (IndexOperation::Upsert, Some(vector)) => {
                let bytes: Vec<u8> = vector.iter().flat_map(|x| x.to_le_bytes()).collect();
                self.conn.execute("INSERT INTO taxonomy_vector VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(profile_id,subject_kind,subject_id) DO UPDATE SET subject_revision=excluded.subject_revision,blob=excluded.blob,created_at_us=excluded.created_at_us,source=excluded.source", params![job.profile_id.to_string(),job.subject_kind,job.subject_id,job.subject_revision,bytes,now,source]).map_err(sql_error)?;
            }
            (IndexOperation::Delete, None) => {
                if job.subject_kind == 2 {
                    self.conn.execute("DELETE FROM taxonomy_vector WHERE profile_id=?1 AND subject_kind=2 AND subject_id=?2 AND subject_revision=?3", params![job.profile_id.to_string(),job.subject_id,job.subject_revision]).map_err(sql_error)?;
                }
            }
            _ => {
                return Err(MCSError::InvalidParams(
                    "vector payload does not match job operation".into(),
                ));
            }
        }
        self.conn
            .execute(
                "UPDATE taxonomy_job SET state='done' WHERE subject_kind=?1 AND subject_id=?2 AND profile_id=?3 AND lease_token=?4 AND lease_epoch=?5",
                params![job.subject_kind, job.subject_id, job.profile_id.to_string(), job.lease.token.to_string(), job.lease.epoch],
            )
            .map_err(sql_error)?;
        tx.commit()?;
        Ok(true)
    }
}

/// Soft full-scan completeness check for one taxonomy kind. It reports whether
/// the kind is missing queued work or carries a stale vector, without failing
/// the caller. Public because the server crate's VectorStore runs it before
/// serving a candidate taxonomy snapshot.
pub fn taxonomy_scan_invalid(conn: &Connection, profile_id: Uuid, kind: i64) -> Result<bool> {
    let profile = profile_id.to_string();
    let invalid: bool = match kind {
        // Kinds 0 and 1 read type_dict members as their source.
        0 | 1 => conn.query_row("SELECT EXISTS(SELECT 1 FROM type_dict s WHERE s.kind=?2 AND s.count>0 AND NOT EXISTS(SELECT 1 FROM taxonomy_job j WHERE j.subject_kind=?2 AND j.subject_id=s.id AND j.profile_id=?1 AND j.state!='dead')) OR EXISTS(SELECT 1 FROM taxonomy_vector v JOIN type_dict s ON s.id=v.subject_id WHERE v.profile_id=?1 AND v.subject_kind=?2 AND s.kind=?2 AND s.count>0 AND v.subject_revision!=s.revision)", params![profile,kind], |r| r.get(0)).map_err(sql_error)?,
        // Kind 2 reads active relation mirrors as its source.
        2 => conn.query_row("SELECT EXISTS(SELECT 1 FROM taxonomy_relation s WHERE s.deleted=0 AND NOT EXISTS(SELECT 1 FROM taxonomy_job j WHERE j.subject_kind=2 AND j.subject_id=s.id AND j.profile_id=?1 AND j.state!='dead')) OR EXISTS(SELECT 1 FROM taxonomy_vector v JOIN taxonomy_relation s ON s.id=v.subject_id WHERE v.profile_id=?1 AND v.subject_kind=2 AND s.deleted=0 AND v.subject_revision!=s.revision)", [profile], |r| r.get(0)).map_err(sql_error)?,
        _ => return Err(MCSError::MemoryError("invalid taxonomy subject kind".into())),
    };
    Ok(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::initialize_database;

    /// In-memory store with every migration applied and one serving profile.
    fn fixture() -> (Connection, Uuid) {
        let conn = Connection::open_in_memory().unwrap();
        initialize_database(&conn).unwrap();
        let profile = Uuid::new_v4();
        conn.execute(
            "UPDATE index_profile_registry SET state='Active',serving_profile=?1 WHERE store_key='default'",
            [profile.to_string()],
        )
        .unwrap();
        (conn, profile)
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn seed_type(conn: &Connection, id: i64, kind: i64, revision: i64) {
        conn.execute(
            "INSERT INTO type_dict(id,kind,name,count,revision) VALUES(?1,?2,?3,?4,?5)",
            params![id, kind, format!("type{kind}-{id}"), 1, revision],
        )
        .unwrap();
    }

    fn seed_relation(conn: &Connection, id: i64, revision: i64, deleted: i64) {
        conn.execute(
            "INSERT INTO taxonomy_relation(id,from_id,to_id,type_id,revision,deleted) VALUES(?1,?2,?3,?4,?5,?6)",
            params![id, id * 100, id * 100 + 1, 2, revision, deleted],
        )
        .unwrap();
    }

    #[test]
    fn enqueue_after_enqueue_upserts_and_bumps_lease_epoch() {
        let (conn, profile) = fixture();
        conn.execute(
            "INSERT INTO taxonomy_ann_generation(profile_id,subject_kind,durable_generation,full_scan_generation) VALUES(?1,0,5,5)",
            [profile.to_string()],
        )
        .unwrap();
        enqueue_taxonomy(&conn, 0, 7, 3, IndexOperation::Upsert, profile).unwrap();
        let repo = TaxonomyJobRepository::new(&conn);
        let first = repo.claim_due(100, 100).unwrap().unwrap();
        assert_eq!(
            (first.subject_kind, first.subject_id, first.subject_revision),
            (0, 7, 3)
        );
        enqueue_taxonomy(&conn, 0, 7, 4, IndexOperation::Upsert, profile).unwrap();
        let (revision, epoch, token, state): (i64, i64, Option<String>, String) = conn
            .query_row(
                "SELECT subject_revision,lease_epoch,lease_token,state FROM taxonomy_job WHERE subject_kind=0 AND subject_id=7 AND profile_id=?1",
                [profile.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(revision, 4);
        assert_eq!(epoch, first.lease.epoch + 1);
        assert!(token.is_none());
        assert_eq!(state, "pending");
        // The superseded lease cannot commit the old revision.
        assert!(!repo.commit_vector(&first, 101, Some(&[1.0]), "worker").unwrap());
        assert_eq!(count(&conn, "taxonomy_vector"), 0);
        let generation: Option<i64> = conn
            .query_row(
                "SELECT full_scan_generation FROM taxonomy_ann_generation WHERE profile_id=?1 AND subject_kind=0",
                [profile.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(generation, None);
    }

    #[test]
    fn claim_picks_the_oldest_due_job() {
        let (conn, profile) = fixture();
        enqueue_taxonomy(&conn, 1, 3, 1, IndexOperation::Upsert, Uuid::nil()).unwrap();
        seed_type(&conn, 1, 0, 1);
        enqueue_taxonomy(&conn, 0, 1, 1, IndexOperation::Upsert, profile).unwrap();
        enqueue_taxonomy(&conn, 2, 10, 9, IndexOperation::Upsert, profile).unwrap();
        conn.execute(
            "UPDATE taxonomy_job SET next_attempt_us=500 WHERE subject_kind=2 AND subject_id=10",
            [],
        )
        .unwrap();
        let repo = TaxonomyJobRepository::new(&conn);
        let first = repo.claim_due(100, 10).unwrap().unwrap();
        assert_eq!((first.subject_kind, first.subject_id), (0, 1));
        assert_eq!(first.operation, IndexOperation::Upsert);
        assert_eq!((first.lease.epoch, first.lease.until_us), (1, 110));
        let (state, attempts): (String, i64) = conn
            .query_row(
                "SELECT state,attempts FROM taxonomy_job WHERE subject_kind=0 AND subject_id=1 AND profile_id=?1",
                [profile.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((state.as_str(), attempts), ("leased", 1));
        // The held job is never claimable and the second job is not due yet.
        assert!(repo.claim_due(100, 10).unwrap().is_none());
        // Complete the first job so its expired lease cannot be re-claimed.
        assert!(repo.commit_vector(&first, 101, Some(&[1.0, 0.0]), "worker").unwrap());
        let second = repo.claim_due(500, 10).unwrap().unwrap();
        assert_eq!((second.subject_kind, second.subject_id, second.subject_revision), (2, 10, 9));
        assert_eq!(second.lease.until_us, 510);
        assert_eq!(count(&conn, "taxonomy_job"), 3);
    }

    #[test]
    fn commit_refuses_after_lease_expiry() {
        let (conn, profile) = fixture();
        seed_type(&conn, 1, 0, 7);
        enqueue_taxonomy(&conn, 0, 1, 7, IndexOperation::Upsert, profile).unwrap();
        let repo = TaxonomyJobRepository::new(&conn);
        let job = repo.claim_due(100, 10).unwrap().unwrap();
        assert!(!repo.commit_vector(&job, 111, Some(&[1.0, 0.0]), "worker").unwrap());
        assert_eq!(count(&conn, "taxonomy_vector"), 0);
        let state: String = conn
            .query_row(
                "SELECT state FROM taxonomy_job WHERE subject_kind=0 AND subject_id=1 AND profile_id=?1",
                [profile.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "leased");
    }

    #[test]
    fn commit_refuses_on_revision_mismatch_for_every_kind() {
        let (conn, profile) = fixture();
        seed_type(&conn, 1, 0, 7);
        seed_type(&conn, 2, 1, 4);
        seed_relation(&conn, 10, 3, 0);
        let repo = TaxonomyJobRepository::new(&conn);
        for (kind, id, revision) in [(0, 1, 6), (1, 2, 3), (2, 10, 2)] {
            enqueue_taxonomy(&conn, kind, id, revision, IndexOperation::Upsert, profile).unwrap();
            let job = repo.claim_due(100 + id, 10).unwrap().unwrap();
            assert_eq!((job.subject_kind, job.subject_id), (kind, id));
            assert!(!repo.commit_vector(&job, 101 + id, Some(&[1.0, 0.0]), "worker").unwrap());
        }
        assert_eq!(count(&conn, "taxonomy_vector"), 0);
        assert_eq!(count(&conn, "taxonomy_job"), 3);
    }

    #[test]
    fn commit_refuses_a_delete_without_the_matching_tombstone() {
        let (conn, profile) = fixture();
        seed_relation(&conn, 10, 3, 0);
        seed_relation(&conn, 11, 3, 1);
        let repo = TaxonomyJobRepository::new(&conn);
        // An active relation has no tombstone: refuse.
        enqueue_taxonomy(&conn, 2, 10, 3, IndexOperation::Delete, profile).unwrap();
        let job = repo.claim_due(100, 10).unwrap().unwrap();
        assert!(!repo.commit_vector(&job, 101, None, "worker").unwrap());
        // A tombstoned relation at a stale revision: refuse.
        enqueue_taxonomy(&conn, 2, 11, 2, IndexOperation::Delete, profile).unwrap();
        let job = repo.claim_due(102, 10).unwrap().unwrap();
        assert!(!repo.commit_vector(&job, 103, None, "worker").unwrap());
        // An upsert against a tombstoned relation: refuse.
        seed_relation(&conn, 13, 5, 1);
        enqueue_taxonomy(&conn, 2, 13, 5, IndexOperation::Upsert, profile).unwrap();
        let job = repo.claim_due(104, 10).unwrap().unwrap();
        assert!(!repo.commit_vector(&job, 105, Some(&[1.0]), "worker").unwrap());
        assert_eq!(count(&conn, "taxonomy_vector"), 0);
        assert_eq!(count(&conn, "taxonomy_job"), 3);
    }

    #[test]
    fn the_valid_path_succeeds_and_writes_the_vector_row() {
        let (conn, profile) = fixture();
        seed_type(&conn, 1, 0, 7);
        enqueue_taxonomy(&conn, 0, 1, 7, IndexOperation::Upsert, profile).unwrap();
        let repo = TaxonomyJobRepository::new(&conn);
        let job = repo.claim_due(100, 10).unwrap().unwrap();
        assert!(repo.commit_vector(&job, 105, Some(&[1.0, 0.0]), "worker").unwrap());
        let (kind, revision, blob, created_at, source): (i64, i64, Vec<u8>, i64, String) = conn
            .query_row(
                "SELECT subject_kind,subject_revision,blob,created_at_us,source FROM taxonomy_vector WHERE profile_id=?1 AND subject_kind=0 AND subject_id=1",
                [profile.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(kind, 0);
        assert_eq!(revision, 7);
        assert_eq!(blob, [0, 0, 128, 63, 0, 0, 0, 0]);
        assert_eq!(created_at, 105);
        assert_eq!(source, "worker");
        let state: String = conn
            .query_row(
                "SELECT state FROM taxonomy_job WHERE subject_kind=0 AND subject_id=1 AND profile_id=?1",
                [profile.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "done");
        // A repeated completion is a no-op.
        assert!(repo.commit_vector(&job, 106, None, "worker").unwrap());
        assert_eq!(count(&conn, "taxonomy_vector"), 1);
        // The delete path removes the matching vector row.
        seed_relation(&conn, 10, 5, 1);
        conn.execute(
            "INSERT INTO taxonomy_vector VALUES(?1,2,10,5,X'000000000000803F',1,'old')",
            [profile.to_string()],
        )
        .unwrap();
        enqueue_taxonomy(&conn, 2, 10, 5, IndexOperation::Delete, profile).unwrap();
        let job = repo.claim_due(200, 10).unwrap().unwrap();
        assert!(repo.commit_vector(&job, 201, None, "worker").unwrap());
        assert_eq!(count(&conn, "taxonomy_vector"), 1);
        let state: String = conn
            .query_row(
                "SELECT state FROM taxonomy_job WHERE subject_kind=2 AND subject_id=10 AND profile_id=?1",
                [profile.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "done");
    }

    #[test]
    fn taxonomy_scan_invalid_reports_stale_or_missing_work() {
        let (conn, profile) = fixture();
        seed_type(&conn, 1, 0, 7);
        seed_relation(&conn, 10, 5, 0);
        let repo = TaxonomyJobRepository::new(&conn);
        // A fully indexed kind is valid.
        enqueue_taxonomy(&conn, 0, 1, 7, IndexOperation::Upsert, profile).unwrap();
        let job = repo.claim_due(100, 10).unwrap().unwrap();
        assert!(repo.commit_vector(&job, 101, Some(&[1.0, 0.0]), "worker").unwrap());
        assert!(!taxonomy_scan_invalid(&conn, profile, 0).unwrap());
        enqueue_taxonomy(&conn, 2, 10, 5, IndexOperation::Upsert, profile).unwrap();
        let job = repo.claim_due(200, 10).unwrap().unwrap();
        assert!(repo.commit_vector(&job, 201, Some(&[1.0, 0.0]), "worker").unwrap());
        assert!(!taxonomy_scan_invalid(&conn, profile, 2).unwrap());
        // A stale vector is invalid.
        conn.execute("UPDATE type_dict SET revision=8 WHERE id=1", []).unwrap();
        assert!(taxonomy_scan_invalid(&conn, profile, 0).unwrap());
        conn.execute("UPDATE type_dict SET revision=7 WHERE id=1", []).unwrap();
        // A missing job is invalid.
        conn.execute("DELETE FROM taxonomy_job WHERE subject_kind=0 AND subject_id=1", [])
            .unwrap();
        assert!(taxonomy_scan_invalid(&conn, profile, 0).unwrap());
        assert!(!taxonomy_scan_invalid(&conn, profile, 2).unwrap());
        // A pending job counts as queued work even before the vector exists.
        seed_relation(&conn, 12, 2, 0);
        conn.execute(
            "INSERT INTO taxonomy_job(subject_kind,subject_id,profile_id,subject_revision,operation,state) VALUES(2,12,?1,2,'upsert','pending')",
            [profile.to_string()],
        )
        .unwrap();
        assert!(!taxonomy_scan_invalid(&conn, profile, 2).unwrap());
        // A dead job does not count as queued work.
        seed_relation(&conn, 11, 3, 0);
        conn.execute(
            "INSERT INTO taxonomy_job(subject_kind,subject_id,profile_id,subject_revision,operation,state) VALUES(2,11,?1,3,'upsert','dead')",
            [profile.to_string()],
        )
        .unwrap();
        assert!(taxonomy_scan_invalid(&conn, profile, 2).unwrap());
        // A type without members is not a source.
        conn.execute("UPDATE type_dict SET count=0 WHERE id=1", []).unwrap();
        assert!(!taxonomy_scan_invalid(&conn, profile, 0).unwrap());
    }
}
