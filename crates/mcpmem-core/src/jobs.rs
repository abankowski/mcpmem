//! Provider-neutral index jobs and vector-space registry.
use crate::errors::{MCSError, Result};
use crate::events::{Lease, lease_until, parse_uuid, sha256, sql_error};
use crate::graph::TxGuard;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) fn enqueue_change(
    conn: &Connection,
    entity_id: i64,
    revision: i64,
    deleted: bool,
) -> Result<()> {
    // A nil profile is an explicitly held LegacyCompat job, never a claimable
    // provider profile. Managed serving and candidate profiles receive updates.
    let mut stmt = conn.prepare("SELECT serving_profile FROM index_profile_registry WHERE serving_profile IS NOT NULL UNION SELECT candidate_profile FROM index_profile_registry WHERE state='Rebuilding' AND candidate_profile IS NOT NULL").map_err(sql_error)?;
    let mut profiles = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(sql_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_error)?;
    if profiles.is_empty() {
        profiles.push(uuid::Uuid::nil().to_string());
    }
    for profile in profiles {
        let state = if profile == uuid::Uuid::nil().to_string() {
            "held"
        } else {
            "pending"
        };
        conn.execute("INSERT INTO index_job(entity_id,profile_id,entity_revision,operation,state) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(entity_id,profile_id) DO UPDATE SET entity_revision=excluded.entity_revision,operation=excluded.operation,state=excluded.state,lease_token=NULL,lease_epoch=lease_epoch+1,lease_until_us=0,attempts=0,next_attempt_us=0,last_error=NULL", params![entity_id,profile,revision,if deleted {"delete"} else {"upsert"},state]).map_err(sql_error)?;
        conn.execute(
            "UPDATE ann_generation SET full_scan_generation=NULL WHERE profile_id=?1",
            [&profile],
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
    let invalid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM entity e LEFT JOIN entity_revision r ON r.entity_id=e.id LEFT JOIN profile_vector v ON v.entity_id=e.id AND v.profile_id=?1 WHERE e.flags=0 AND (v.entity_id IS NULL OR r.revision IS NULL OR v.entity_revision!=r.revision)) OR EXISTS(SELECT 1 FROM profile_vector v LEFT JOIN entity e ON e.id=v.entity_id WHERE v.profile_id=?1 AND (e.id IS NULL OR e.flags!=0)) OR EXISTS(SELECT 1 FROM index_job WHERE profile_id=?1 AND state!='done')", [profile.to_string()], |r| r.get(0)).map_err(sql_error)?;
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
