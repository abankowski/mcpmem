use std::path::Path;
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use petgraph::Directed;
use petgraph::graph::NodeIndex;
use petgraph::stable_graph::StableGraph;
use rusqlite::{Connection, OptionalExtension, params};

use crate::errors::{MCSError, Result};
use crate::kg::push_json_str;
use mcpmem_core::jobs::{
    AnnGenerationRepository, ChunkKind, DistanceMetric, IndexProfileRegistry, OwnerKind,
    StoreState, taxonomy_scan_invalid,
};

/// The taxonomy subject kinds a snapshot can serve. The discriminant matches
/// the `subject_kind` column of `taxonomy_vector`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaxonomyKind {
    EntityType = 0,
    RelationType = 1,
    Relation = 2,
}

impl TaxonomyKind {
    pub const ALL: [TaxonomyKind; 3] = [
        TaxonomyKind::EntityType,
        TaxonomyKind::RelationType,
        TaxonomyKind::Relation,
    ];

    const fn as_i64(self) -> i64 {
        self as i64
    }
}

/// What [`VectorStore::adopt_profile`] did. Reported rather than logged inside,
/// so the caller decides how loud each outcome is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdoptOutcome {
    /// The store already serves an equivalent profile.
    Unchanged,
    /// A rebuild into this profile was already running.
    RebuildInProgress,
    /// A rebuild started. Every live owner is queued for embedding. The
    /// 2.0.0 surface has no client ingestion tools left to refuse.
    RebuildStarted,
    /// The last rebuild into this profile failed and stays failed. The store
    /// keeps serving whatever it served before.
    PreviousRebuildFailed(String),
}
pub type EntityId = i64;

/// The dimension the store validates chunk blobs against. The profile owns
/// the real contract; this is the legacy CLI surface left for test servers.
#[derive(Clone, Copy, Debug)]
pub struct VectorConfig {
    /// Embedding dimension. All chunk/query vectors must match this.
    pub dims: u32,
}

impl VectorConfig {
    /// Default configuration for the given embedding dimension.
    pub const fn new(dims: u32) -> Self {
        Self { dims }
    }
}

pub struct VectorStore {
    /// Last observed names, maintained for callers inspecting the cache. Entity
    /// identity must be resolved against SQLite: other writers can rename rows.
    pub name_to_id: Arc<DashMap<String, EntityId>>,
    pub id_to_name: Arc<DashMap<EntityId, String>>,

    pub(crate) graph: Arc<RwLock<StableGraph<EntityId, (), Directed, u32>>>,
    pub(crate) node_map: Arc<DashMap<EntityId, NodeIndex<u32>>>,

    pub(crate) db: Mutex<Connection>,

    pub dims: u32,

    pub db_path: std::path::PathBuf,
    /// Profile-owned vectors are immutable snapshots. Readers clone this Arc
    /// before searching, so a completed rebuild never exposes a half-built
    /// candidate generation.
    managed_snapshot: RwLock<Option<Arc<ManagedSnapshot>>>,
    /// Per-kind taxonomy snapshots, one entry per [`TaxonomyKind`]
    /// discriminant. Readers clone the Arc before searching, so a completed
    /// adopt never exposes a half-built kind.
    taxonomy_snapshots: RwLock<[Option<Arc<TaxonomySnapshot>>; 3]>,
}

/// One chunk row of the managed snapshot: the owner, its kind and type, and
/// the raw vector the profile validated when the snapshot was built.
struct SnapshotVector {
    owner_kind: OwnerKind,
    owner_id: i64,
    chunk_kind: ChunkKind,
    chunk_index: i64,
    type_id: i64,
    vector: Vec<f32>,
}

#[cfg(test)]
pub struct SeedChunk<'a> {
    owner_kind: OwnerKind,
    owner_id: i64,
    chunk_kind: ChunkKind,
    chunk_index: i64,
    type_id: i64,
    vector: &'a [f32],
}

struct ManagedSnapshot {
    profile: uuid::Uuid,
    durable_generation: i64,
    metric: DistanceMetric,
    vectors: Vec<SnapshotVector>,
}

/// One match of [`VectorStore::search_chunks`], with the frame that makes the
/// row actionable: whose chunk it is, which chunk, and how far from `query`.
#[derive(Clone, Copy, Debug)]
pub struct ChunkHit {
    pub owner_kind: OwnerKind,
    pub owner_id: i64,
    pub chunk_kind: ChunkKind,
    pub chunk_index: i64,
    pub type_id: i64,
    pub dist: f32,
}

/// One kind's adopted taxonomy snapshot: immutable vectors at one durable
/// generation. Additive to the entity `ManagedSnapshot`.
struct TaxonomySnapshot {
    profile: uuid::Uuid,
    durable_generation: i64,
    metric: DistanceMetric,
    vectors: Vec<(i64, Vec<f32>)>,
}

fn sqlite_err(e: rusqlite::Error) -> MCSError {
    MCSError::IoError(std::io::Error::other(e))
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Vec<f32>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

pub fn with_scratch<R>(f: impl FnOnce(&mut Vec<f32>) -> R) -> R {
    SCRATCH.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();
        f(&mut buf)
    })
}

#[cfg(test)]
fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}

fn managed_distance(metric: DistanceMetric, left: &[f32], right: &[f32]) -> f32 {
    match metric {
        DistanceMetric::L2Squared => left.iter().zip(right).map(|(a, b)| (a - b).powi(2)).sum(),
        DistanceMetric::InnerProduct => -left.iter().zip(right).map(|(a, b)| a * b).sum::<f32>(),
        DistanceMetric::Cosine => {
            let dot: f32 = left.iter().zip(right).map(|(a, b)| a * b).sum();
            let left_norm = left.iter().map(|value| value.powi(2)).sum::<f32>().sqrt();
            let right_norm = right.iter().map(|value| value.powi(2)).sum::<f32>().sqrt();
            if left_norm == 0.0 || right_norm == 0.0 {
                1.0
            } else {
                1.0 - dot / (left_norm * right_norm)
            }
        }
    }
}

/// Publish gate for one taxonomy kind, mirroring
/// `AnnGenerationRepository::mark_published` against the
/// `taxonomy_ann_generation` table. Refuses (false) when a concurrent durable
/// advance moved the generation past the one this snapshot was built from.
fn mark_taxonomy_published(
    conn: &Connection,
    profile: uuid::Uuid,
    kind: i64,
    generation: i64,
) -> Result<bool> {
    let changed = conn
        .execute(
            "UPDATE taxonomy_ann_generation SET published_generation=?3 WHERE profile_id=?1 AND subject_kind=?2 AND durable_generation=?3",
            params![profile.to_string(), kind, generation],
        )
        .map_err(sqlite_err)?;
    Ok(changed == 1)
}

impl VectorStore {
    /// Open a store with the default configuration for `dims`.
    pub fn new(db_path: &Path, dims: u32) -> Result<Self> {
        Self::with_config(db_path, &VectorConfig::new(dims))
    }

    /// Open a store at a database path. The legacy `vector_embedding` table
    /// and the in-memory ANN index no longer exist: the store serves the
    /// durable chunk snapshot only, and profiles own the dimensions.
    pub fn with_config(db_path: &Path, cfg: &VectorConfig) -> Result<Self> {
        let dims = cfg.dims;
        let conn = Connection::open(db_path).map_err(sqlite_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(sqlite_err)?;
        conn.execute_batch("PRAGMA journal_mode = WAL;\n             PRAGMA synchronous = NORMAL;\n             PRAGMA temp_store = MEMORY;")
            .map_err(sqlite_err)?;

        let name_to_id = Arc::new(DashMap::new());
        let id_to_name = Arc::new(DashMap::new());
        let graph = Arc::new(RwLock::new(
            StableGraph::<EntityId, (), Directed, u32>::new(),
        ));
        let node_map = Arc::new(DashMap::new());
        let db = Mutex::new(conn);

        let store = Self {
            name_to_id,
            id_to_name,
            graph,
            node_map,
            db,
            dims,
            db_path: db_path.to_path_buf(),
            managed_snapshot: RwLock::new(None),
            taxonomy_snapshots: RwLock::new([None, None, None]),
        };
        store.load_existing()?;

        Ok(store)
    }

    fn load_existing(&self) -> Result<()> {
        self.load_names_from_entity_table(&self.db.lock())?;
        Ok(())
    }

    fn load_names_from_entity_table(&self, conn: &Connection) -> Result<()> {
        let mut stmt = conn
            .prepare("SELECT id, name FROM entity WHERE flags = 0")
            .map_err(sqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let name: String = row.get(1)?;
                Ok((id, name))
            })
            .map_err(sqlite_err)?;

        self.name_to_id.clear();
        self.id_to_name.clear();

        for row in rows {
            let (id, name) = row.map_err(sqlite_err)?;
            self.name_to_id.insert(name.clone(), id);
            self.id_to_name.insert(id, name);
        }
        Ok(())
    }

    fn get_entity_id_and_name(
        &self,
        conn: &Connection,
        entity_name: &str,
    ) -> Result<Option<(EntityId, String)>> {
        // Invalidation from a transport cannot cover independently opened
        // writers. Never treat a previously observed name as a live alias.
        let h = crate::kg::name_hash(entity_name);
        let mut stmt = conn
            .prepare_cached(
                "SELECT id, name FROM entity WHERE name_hash = ?1 AND name = ?2 AND flags = 0",
            )
            .map_err(sqlite_err)?;
        match stmt.query_row(params![h, entity_name], |row| {
            let id: i64 = row.get(0)?;
            let name: String = row.get(1)?;
            Ok((id, name))
        }) {
            Ok(tup) => {
                self.cache_entity_name(tup.0, &tup.1);
                Ok(Some(tup))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                self.forget_entity_name(entity_name);
                Ok(None)
            }
            Err(e) => Err(sqlite_err(e)),
        }
    }

    fn cache_entity_name(&self, id: EntityId, name: &str) {
        if let Some(previous) = self.id_to_name.insert(id, name.to_owned())
            && previous != name
        {
            self.name_to_id
                .remove_if(&previous, |_, value| *value == id);
        }
        if let Some(previous) = self.name_to_id.insert(name.to_owned(), id)
            && previous != id
        {
            self.id_to_name
                .remove_if(&previous, |_, value| value == name);
        }
    }

    fn forget_entity_name(&self, name: &str) {
        if let Some((_, id)) = self.name_to_id.remove(name) {
            self.id_to_name.remove_if(&id, |_, value| value == name);
        }
    }

    fn get_entity_name_type(
        &self,
        conn: &Connection,
        id: EntityId,
    ) -> Result<Option<(String, String)>> {
        let entity = conn
            .prepare_cached(
                "SELECT e.name, COALESCE(t.name, '') FROM entity e
                 LEFT JOIN type_dict t ON t.id = e.type_id
                 WHERE e.id = ?1 AND e.flags = 0",
            )
            .map_err(sqlite_err)?
            .query_row([id], |row| Ok((row.get::<_, String>(0)?, row.get(1)?)))
            .optional()
            .map_err(sqlite_err)?;
        if let Some((name, _)) = &entity {
            self.cache_entity_name(id, name);
        } else if let Some((_, name)) = self.id_to_name.remove(&id) {
            self.name_to_id.remove_if(&name, |_, value| *value == id);
        }
        Ok(entity)
    }

    pub fn search_chunks(
        &self,
        query: &[f32],
        fetch_k: usize,
        filter_kind: Option<&str>,
        filter_type: Option<&str>,
    ) -> Result<Vec<ChunkHit>> {
        let Some(snapshot) = self.managed_snapshot.read().clone() else {
            return Ok(Vec::new());
        };
        if query.len()
            != snapshot
                .vectors
                .first()
                .map_or(query.len(), |sv| sv.vector.len())
        {
            return Err(MCSError::InvalidParams(
                "query dimensions do not match active index profile".into(),
            ));
        }
        let mut matches: Vec<ChunkHit> = Vec::new();
        for sv in &snapshot.vectors {
            if let Some(kind) = filter_kind
                && sv.owner_kind.as_str() != kind
            {
                continue;
            }
            if let Some(ftype) = filter_type
                && !self.chunk_type_matches(sv.type_id, ftype)?
            {
                continue;
            }
            matches.push(ChunkHit {
                owner_kind: sv.owner_kind,
                owner_id: sv.owner_id,
                chunk_kind: sv.chunk_kind,
                chunk_index: sv.chunk_index,
                type_id: sv.type_id,
                dist: managed_distance(snapshot.metric, query, &sv.vector),
            });
        }
        matches.sort_by(|a, b| a.dist.total_cmp(&b.dist));
        matches.truncate(fetch_k.clamp(1, 1000));
        Ok(matches)
    }

    /// Whether the type name of `type_id` equals `ftype` in either dict kind.
    /// A lookup error propagates: silently dropping chunks from a
    /// type-filtered search would serve a wrong result as if it were correct.
    fn chunk_type_matches(&self, type_id: i64, ftype: &str) -> Result<bool> {
        let conn = self.db.lock();
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM type_dict WHERE id=?1 AND name=?2)",
            params![type_id, ftype],
            |r| r.get::<_, bool>(0),
        )
        .map_err(sqlite_err)
    }

    /// Reduce chunk hits to one `(owner_kind, owner_id, best_dist,
    /// best_chunk_index)` per owner. Hits arrive distance-ascending, so the
    /// first hit per owner is its best chunk; `best_chunk_index` is `Some`
    /// only when that chunk is an observation (identity and relation chunks
    /// have no index worth returning). The output is sorted ascending by
    /// distance and truncated to `top_k` owners.
    pub fn aggregate_owners(
        &self,
        hits: &[ChunkHit],
        top_k: usize,
    ) -> Vec<(OwnerKind, i64, f32, Option<usize>)> {
        // A hash map cannot key on the owner pair (`OwnerKind` implements no
        // `Hash`), and hits arrive distance-ascending, so a single pass that
        // keeps the first row per owner is the same computation.
        let mut out: Vec<(OwnerKind, i64, f32, Option<usize>)> = Vec::new();
        for hit in hits {
            if out
                .iter()
                .any(|(kind, id, _, _)| *kind == hit.owner_kind && *id == hit.owner_id)
            {
                continue;
            }
            let best_chunk: Option<usize> = match hit.chunk_kind {
                ChunkKind::Observation => Some(hit.chunk_index as usize),
                _ => None,
            };
            out.push((hit.owner_kind, hit.owner_id, hit.dist, best_chunk));
        }
        out.sort_by(|a, b| a.2.total_cmp(&b.2));
        out.truncate(top_k);
        out
    }

    pub fn identity_vector(&self, entity_name: &str) -> Result<Option<Vec<f32>>> {
        let conn = self.db.lock();
        let id: Option<i64> = conn
            .query_row(
                "SELECT id FROM entity WHERE name_hash=?1 AND name=?2 AND flags=0",
                params![crate::kg::name_hash(entity_name), entity_name],
                |r| r.get(0),
            )
            .optional()
            .map_err(sqlite_err)?;
        drop(conn);
        let Some(id) = id else {
            return Ok(None);
        };
        self.owner_identity_vector(OwnerKind::Entity, id)
    }

    /// The identity chunk of one owner in the profile serving chunk reads.
    pub fn owner_identity_vector(
        &self,
        owner_kind: OwnerKind,
        owner_id: i64,
    ) -> Result<Option<Vec<f32>>> {
        let conn = self.db.lock();
        let Some(profile) = self.serving_profile_id(&conn)? else {
            return Ok(None);
        };
        // The byte-length gate must follow the profile's own dimension. The
        // CLI `--embedding-dims` default only describes stores that never
        // adopted a profile; gating on it drops every identity chunk of a
        // serving profile that differs (e.g. 768 while the flag stays 384).
        let dims = IndexProfileRegistry::new(&conn).get(profile)?.dimensions as usize;
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT blob FROM chunk_vector
                 WHERE profile_id=?1 AND kind='identity' AND owner_kind=?2 AND owner_id=?3",
                params![profile.to_string(), owner_kind.as_str(), owner_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sqlite_err)?;
        let Some(bytes) = blob else {
            return Ok(None);
        };
        if bytes.len() != dims * std::mem::size_of::<f32>() {
            return Ok(None);
        }
        let (chunks, _) = bytes.as_chunks::<4>();
        Ok(Some(
            chunks
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect::<Vec<_>>(),
        ))
    }

    /// The profile id `search_chunks` and the identity helpers read, mirroring
    /// `reconcile_managed_snapshot`: an active profile serves itself, a
    /// rebuilding profile serves its candidate, and a failed profile keeps
    /// serving whatever the snapshot already published. Legacy compatibility
    /// serves nothing.
    fn serving_profile_id(&self, conn: &Connection) -> Result<Option<uuid::Uuid>> {
        let registry = IndexProfileRegistry::new(conn);
        Ok(match registry.state("default")? {
            StoreState::LegacyCompat => None,
            StoreState::Active(profile) => Some(profile),
            StoreState::Rebuilding { candidate, .. } => Some(candidate),
            StoreState::Failed { .. } => self
                .managed_snapshot
                .read()
                .as_ref()
                .map(|snapshot| snapshot.profile),
        })
    }

    /// Rebuild and atomically publish a managed reader from a durable profile
    /// generation. This is worker-only; MCP searches never invoke it.
    ///
    /// Also drives taxonomy adoption for every kind, with the entity path's
    /// candidate decision. A refused adoption is logged, never propagated:
    /// the entity path must still publish and activate when a taxonomy job
    /// keeps a kind scan-invalid forever.
    pub fn reconcile_managed_snapshot(&self) -> Result<()> {
        let conn = self.db.lock();
        let registry = IndexProfileRegistry::new(&conn);
        let state = registry.state("default")?;
        let candidate = matches!(&state, StoreState::Rebuilding { .. });
        // A taxonomy refusal must not break the entity path: a dead job can
        // keep a kind scan-invalid forever, so deferring the entity
        // activation on that refusal would block entity search for good.
        // Log and continue; the next poll retries the adoption.
        if let Err(error) = self.adopt_taxonomy(&registry, candidate) {
            tracing::debug!(%error, "taxonomy adoption deferred; the entity path continues");
        }
        let active = match state {
            StoreState::Active(profile) => Some((
                profile,
                AnnGenerationRepository::new(&conn)
                    .get(profile)?
                    .durable_generation,
            )),
            StoreState::Rebuilding { candidate, .. } => Some((
                candidate,
                AnnGenerationRepository::new(&conn)
                    .get(candidate)?
                    .durable_generation,
            )),
            StoreState::Failed { .. } => self
                .managed_snapshot
                .read()
                .as_ref()
                .map(|snapshot| (snapshot.profile, snapshot.durable_generation)),
            StoreState::LegacyCompat => None,
        };
        if self
            .managed_snapshot
            .read()
            .as_ref()
            .map(|snapshot| (snapshot.profile, snapshot.durable_generation))
            == active
        {
            return Ok(());
        }
        let Some((profile_id, durable_generation)) = active else {
            *self.managed_snapshot.write() = None;
            return Ok(());
        };
        let profile = registry.get(profile_id)?;
        let mut statement = conn
            .prepare(
                "SELECT kind, owner_kind, owner_id, chunk_index, type_id, blob
                 FROM chunk_vector WHERE profile_id=?1 ORDER BY owner_kind, owner_id, chunk_index",
            )
            .map_err(sqlite_err)?;
        let vectors = statement
            .query_map([profile_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                ))
            })
            .map_err(sqlite_err)?
            .map(|row| {
                let (kind, owner_kind, owner_id, chunk_index, type_id, blob) =
                    row.map_err(sqlite_err)?;
                if blob.len() != profile.dimensions as usize * std::mem::size_of::<f32>() {
                    return Err(MCSError::MemoryError(
                        "managed profile vector has invalid byte length".into(),
                    ));
                }
                let (chunks, _) = blob.as_chunks::<4>();
                let vector = chunks
                    .iter()
                    .map(|bytes| f32::from_le_bytes(*bytes))
                    .collect::<Vec<_>>();
                profile.validate_vector(&vector)?;
                Ok(SnapshotVector {
                    owner_kind: match owner_kind.as_str() {
                        "entity" => OwnerKind::Entity,
                        "relation" => OwnerKind::Relation,
                        _ => return Err(MCSError::MemoryError("invalid chunk owner kind".into())),
                    },
                    owner_id,
                    chunk_kind: match kind.as_str() {
                        "identity" => ChunkKind::Identity,
                        "observation" => ChunkKind::Observation,
                        "relation" => ChunkKind::Relation,
                        _ => return Err(MCSError::MemoryError("invalid chunk kind".into())),
                    },
                    chunk_index,
                    type_id,
                    vector,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if !AnnGenerationRepository::new(&conn).mark_published(profile_id, durable_generation)? {
            return Ok(());
        }
        if candidate {
            AnnGenerationRepository::new(&conn).verify_full_scan(profile_id)?;
            registry.activate(profile_id)?;
        }
        *self.managed_snapshot.write() = Some(Arc::new(ManagedSnapshot {
            profile: profile_id,
            durable_generation,
            metric: profile.distance_metric,
            vectors,
        }));
        Ok(())
    }

    /// Builds one ANN snapshot per taxonomy kind from the durable
    /// `taxonomy_vector` rows, mirroring [`Self::reconcile_managed_snapshot`].
    /// The registry owns the connection this call reads and writes: callers
    /// hold the store's db lock and pass a registry built on that connection,
    /// so the generation read, the vector load and the publish mark share one
    /// transaction view.
    ///
    /// A kind whose generation row is absent, or whose durable generation did
    /// not move since the last build, is left untouched. A non-candidate build
    /// publishes through `taxonomy_ann_generation` and is refused when a
    /// concurrent durable advance queued newer work. A candidate build
    /// re-verifies every kind with `taxonomy_scan_invalid` and refuses (an
    /// Err, so nothing is swapped) when any kind has missing or stale work;
    /// activating the registry stays with the caller.
    pub fn adopt_taxonomy(&self, registry: &IndexProfileRegistry, candidate: bool) -> Result<()> {
        let conn = registry.connection();
        let state = registry.state("default")?;
        let profile = match state {
            StoreState::Active(profile)
            | StoreState::Rebuilding {
                candidate: profile, ..
            } => Some(profile),
            // Failed keeps serving whatever was already adopted, exactly like
            // the entity snapshot.
            StoreState::Failed { .. } => return Ok(()),
            StoreState::LegacyCompat => None,
        };
        let Some(profile) = profile else {
            *self.taxonomy_snapshots.write() = [None, None, None];
            return Ok(());
        };
        let profile_def = registry.get(profile)?;
        let existing = self.taxonomy_snapshots.read().clone();
        let mut snapshots = [None, None, None];
        for kind in TaxonomyKind::ALL {
            let idx = kind as usize;
            let generation: Option<i64> = conn
                .query_row(
                    "SELECT durable_generation FROM taxonomy_ann_generation WHERE profile_id=?1 AND subject_kind=?2",
                    params![profile.to_string(), kind.as_i64()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sqlite_err)?;
            // No durable work for this kind: nothing to serve. A previous
            // snapshot is dropped so a retired profile cannot linger.
            let Some(generation) = generation else {
                continue;
            };
            // A candidate build re-verifies every kind even when its
            // generation did not move: activation must not ride on a stale
            // scan. The refusal leaves every snapshot in place.
            if candidate && taxonomy_scan_invalid(conn, profile, kind.as_i64())? {
                return Err(MCSError::InvalidParams(
                    "candidate taxonomy snapshot has missing or stale vectors or jobs".into(),
                ));
            }
            let unchanged = existing[idx]
                .as_ref()
                .map(|snapshot| (snapshot.profile, snapshot.durable_generation))
                == Some((profile, generation));
            if unchanged {
                snapshots[idx] = existing[idx].clone();
                continue;
            }
            // Kind 2 (relations) derives its snapshot from the single
            // relation chunk owned by each mirror; kinds 0 and 1 keep their
            // `taxonomy_vector` rows. Both read the same `(id, blob)` shape.
            let decode_row = |row: rusqlite::Result<(i64, Vec<u8>)>| -> Result<(i64, Vec<f32>)> {
                let (id, blob) = row.map_err(sqlite_err)?;
                if blob.len() != profile_def.dimensions as usize * std::mem::size_of::<f32>() {
                    return Err(MCSError::MemoryError(
                        "taxonomy vector has invalid byte length".into(),
                    ));
                }
                let (chunks, _) = blob.as_chunks::<4>();
                let vector = chunks
                    .iter()
                    .map(|bytes| f32::from_le_bytes(*bytes))
                    .collect::<Vec<_>>();
                profile_def.validate_vector(&vector)?;
                Ok((id, vector))
            };
            let vectors = if kind == TaxonomyKind::Relation {
                let mut statement = conn
                    .prepare(
                        "SELECT owner_id, blob FROM chunk_vector WHERE profile_id=?1 AND kind='relation' ORDER BY owner_id",
                    )
                    .map_err(sqlite_err)?;
                statement
                    .query_map(params![profile.to_string()], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
                    })
                    .map_err(sqlite_err)?
                    .map(decode_row)
                    .collect::<Result<Vec<_>>>()?
            } else {
                let mut statement = conn
                    .prepare(
                        "SELECT subject_id,blob FROM taxonomy_vector WHERE profile_id=?1 AND subject_kind=?2 ORDER BY subject_id",
                    )
                    .map_err(sqlite_err)?;
                statement
                    .query_map(params![profile.to_string(), kind.as_i64()], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
                    })
                    .map_err(sqlite_err)?
                    .map(decode_row)
                    .collect::<Result<Vec<_>>>()?
            };
            // Serve only durable generations. A concurrent durable advance
            // past the generation this build read refuses the publish; the
            // caller re-adopts after the next committed batch.
            if !candidate && !mark_taxonomy_published(conn, profile, kind.as_i64(), generation)? {
                return Ok(());
            }
            snapshots[idx] = Some(Arc::new(TaxonomySnapshot {
                profile,
                durable_generation: generation,
                metric: profile_def.distance_metric,
                vectors,
            }));
        }
        *self.taxonomy_snapshots.write() = snapshots;
        Ok(())
    }

    /// Nearest taxonomy subjects for a query in the kind's adopted snapshot,
    /// as `(subject_id, distance)` ascending. An absent snapshot returns an
    /// empty list, not an error, so the suggestion engine falls back to the
    /// offline tier. Distances use the snapshot's profile metric.
    pub fn search_taxonomy(
        &self,
        kind: TaxonomyKind,
        query: &[f32],
        top_k: usize,
    ) -> Result<Vec<(i64, f64)>> {
        let Some(snapshot) = self.taxonomy_snapshots.read()[kind as usize].clone() else {
            return Ok(Vec::new());
        };
        if query.len()
            != snapshot
                .vectors
                .first()
                .map_or(query.len(), |(_, vector)| vector.len())
        {
            return Err(MCSError::InvalidParams(
                "query dimensions do not match active taxonomy profile".into(),
            ));
        }
        let mut matches: Vec<_> = snapshot
            .vectors
            .iter()
            .map(|(id, vector)| {
                (
                    *id,
                    f64::from(managed_distance(snapshot.metric, query, vector)),
                )
            })
            .collect();
        matches.sort_by(|left, right| left.1.total_cmp(&right.1));
        matches.truncate(top_k.clamp(1, 100));
        Ok(matches)
    }

    /// Resolves a taxonomy subject id to its current `(name, kind_label)`.
    /// Kinds 0/1 read `type_dict` names; kind 2 renders the live entity names
    /// around the relation mirror row. Returns None for an unknown subject.
    pub fn resolve_taxonomy(&self, kind: TaxonomyKind, id: i64) -> Option<(String, String)> {
        let conn = self.db.lock();
        match kind {
            TaxonomyKind::EntityType | TaxonomyKind::RelationType => {
                let label = if kind == TaxonomyKind::EntityType {
                    "entityType"
                } else {
                    "relationType"
                };
                let name: String = conn
                    .query_row(
                        "SELECT name FROM type_dict WHERE id=?1 AND kind=?2",
                        params![id, kind.as_i64()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sqlite_err)
                    .ok()
                    .flatten()?;
                Some((name, label.to_string()))
            }
            TaxonomyKind::Relation => {
                let (from_id, to_id, type_id): (i64, i64, i64) = conn
                    .query_row(
                        "SELECT from_id,to_id,type_id FROM taxonomy_relation WHERE id=?1",
                        [id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()
                    .map_err(sqlite_err)
                    .ok()
                    .flatten()?;
                // An endpoint that is gone (deleted or merged away) must not
                // render a placeholder name: a suggestion engine would serve
                // it as a real name. The relation path skips such rows.
                let (from_name, _) = self.get_entity_name_type(&conn, from_id).ok().flatten()?;
                let (to_name, _) = self.get_entity_name_type(&conn, to_id).ok().flatten()?;
                let relation_type: String = conn
                    .query_row(
                        "SELECT name FROM type_dict WHERE id=?1 AND kind=1",
                        [type_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sqlite_err)
                    .ok()
                    .flatten()?;
                Some((
                    format!("{from_name} -[{relation_type}]-> {to_name}"),
                    "relation".to_string(),
                ))
            }
        }
    }

    /// Entity rows from the serving chunk snapshot, as `{results, count}`
    /// JSON with `name`, `entityType` and `score` (the best chunk distance).
    /// Entity-only, like the suggestion path that consumes it: relation rows
    /// never surface here.
    pub fn search_entities_json(
        &self,
        query: &[f32],
        top_k: usize,
        entity_type_filter: Option<&str>,
    ) -> Result<String> {
        let top_k = top_k.clamp(1, 100);
        // Over-fetch chunks before aggregating, so one owner's many near
        // chunks cannot crowd the other owners out of top_k.
        let fetch = top_k.saturating_mul(3).clamp(top_k, 100);
        let hits = self.search_chunks(query, fetch, Some("entity"), None)?;
        let owners = self.aggregate_owners(&hits, top_k);
        if owners.is_empty() {
            return Ok(r#"{"results":[],"count":0}"#.to_string());
        }

        let conn = self.db.lock();
        let mut out = String::with_capacity(128 + owners.len() * 64);
        out.push_str(r#"{"results":["#);
        let mut first = true;
        let mut actual_count = 0usize;

        for &(_, id, dist, _) in &owners {
            let Some((name, etype)) = self.get_entity_name_type(&conn, id)? else {
                continue;
            };

            if let Some(filter_type) = entity_type_filter
                && etype != filter_type
            {
                continue;
            }

            if !first {
                out.push(',');
            }
            first = false;

            out.push_str(r#"{"name":"#);
            push_json_str(&mut out, &name);
            out.push_str(r#","entityType":"#);
            push_json_str(&mut out, &etype);
            write_f32(&mut out, dist);
            out.push('}');
            actual_count += 1;
        }

        out.push_str(r#"],"count":"#);
        out.push_str(&actual_count.to_string());
        out.push('}');
        Ok(out)
    }

    pub fn rebuild_graph_cache(&self) -> Result<()> {
        let conn = self.db.lock();

        let mut ent_stmt = conn
            .prepare("SELECT id FROM entity WHERE flags=0")
            .map_err(sqlite_err)?;
        let ids: Vec<EntityId> = ent_stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .map_err(sqlite_err)?
            .map(|r| r.ok().unwrap_or_default())
            .collect::<Vec<_>>();

        let mut g = StableGraph::<EntityId, (), Directed, u32>::with_capacity(ids.len(), 0);
        let nm = DashMap::new();

        for &id in &ids {
            let nx = g.add_node(id);
            nm.insert(id, nx);
        }

        if !ids.is_empty() {
            const BATCH_SIZE: usize = 5000;
            for chunk in ids.chunks(BATCH_SIZE) {
                let placeholders: Vec<String> = chunk.iter().map(|_| "?".to_string()).collect();
                let sql = format!(
                    "SELECT from_id, to_id FROM relation WHERE from_id IN ({}) AND to_id IN ({})",
                    placeholders.join(","),
                    placeholders.join(",")
                );
                let mut rel_stmt = conn.prepare(&sql).map_err(sqlite_err)?;

                let mut param_values: Vec<&dyn rusqlite::types::ToSql> =
                    Vec::with_capacity(chunk.len() * 2);
                for id in chunk {
                    param_values.push(id as &dyn rusqlite::types::ToSql);
                }
                for id in chunk {
                    param_values.push(id as &dyn rusqlite::types::ToSql);
                }

                let rel_rows = rel_stmt
                    .query_map(param_values.as_slice(), |row| {
                        let from: i64 = row.get(0)?;
                        let to: i64 = row.get(1)?;
                        Ok((from, to))
                    })
                    .map_err(sqlite_err)?;

                for rel in rel_rows {
                    let (from, to) = rel.map_err(sqlite_err)?;
                    if let (Some(f_nx), Some(t_nx)) = (nm.get(&from), nm.get(&to))
                        && g.find_edge(*f_nx, *t_nx).is_none()
                    {
                        g.add_edge(*f_nx, *t_nx, ());
                    }
                }
            }
        }

        *self.graph.write() = g;
        self.node_map.clear();
        for entry in nm.iter() {
            self.node_map.insert(*entry.key(), *entry.value());
        }

        Ok(())
    }

    pub fn graph_node_count(&self) -> usize {
        self.node_map.len()
    }

    pub fn graph_edge_count(&self) -> usize {
        self.graph.read().edge_count()
    }

    pub fn get_entity_type(&self, entity_id: EntityId) -> Result<Option<String>> {
        let conn = self.db.lock();
        let etype = conn
            .query_row(
                "SELECT t.name FROM entity e JOIN type_dict t ON t.id = e.type_id WHERE e.id = ?1 AND e.flags = 0",
                params![entity_id],
                |row| row.get(0),
            )
            .ok();
        Ok(etype)
    }

    /// Chunk rows the serving snapshot holds. Counts what the serving path
    /// searches: zero for an absent snapshot, one per `chunk_vector` row
    /// loaded into the published generation.
    pub fn count(&self) -> usize {
        self.managed_snapshot
            .read()
            .as_ref()
            .map(|snapshot| snapshot.vectors.len())
            .unwrap_or(0)
    }

    pub const fn dims(&self) -> u32 {
        self.dims
    }

    /// The profile this store serves, or `None` while the registry holds the
    /// store in legacy compatibility. A caller that must know the model or the
    /// dimension of the managed index reads it here; the registry itself is
    /// private.
    pub fn serving_profile(&self) -> Result<Option<mcpmem_core::jobs::IndexProfile>> {
        let conn = self.db.lock();
        let registry = IndexProfileRegistry::new(&conn);
        registry
            .serving_profile("default")?
            .map(|id| registry.get(id))
            .transpose()
    }

    /// Moves the store onto `desired`, and does nothing when it already serves
    /// an equivalent profile.
    ///
    /// Equivalence is the profile fingerprint, which covers every field except
    /// the identifier. That is what makes this safe to call on every startup:
    /// an unchanged configuration file is a no-op, and only a real change to
    /// the provider, the model, the dimension, the normalization or the metric
    /// starts a rebuild.
    ///
    /// A rebuild re-enqueues every live owner, and it takes the store out of
    /// legacy compatibility. The caller decides whether that is wanted; this
    /// method only reports what it did.
    pub fn adopt_profile(&self, desired: &mcpmem_core::jobs::IndexProfile) -> Result<AdoptOutcome> {
        let wanted = desired.fingerprint()?;
        let conn = self.db.lock();
        let registry = IndexProfileRegistry::new(&conn);
        let same = |id| -> Result<bool> { Ok(registry.get(id)?.fingerprint()? == wanted) };
        match registry.state("default")? {
            StoreState::Active(active) if same(active)? => Ok(AdoptOutcome::Unchanged),
            StoreState::Rebuilding { candidate, .. } => {
                if same(candidate)? {
                    Ok(AdoptOutcome::RebuildInProgress)
                } else {
                    Err(MCSError::InvalidParams(
                        "a rebuild into a different profile is already in progress; let it finish or clear it before changing the configuration".into(),
                    ))
                }
            }
            StoreState::Failed {
                candidate, reason, ..
            } if same(candidate)? => Ok(AdoptOutcome::PreviousRebuildFailed(reason)),
            StoreState::Active(_) | StoreState::Failed { .. } | StoreState::LegacyCompat => {
                registry.begin_rebuild(desired)?;
                Ok(AdoptOutcome::RebuildStarted)
            }
        }
    }

    /// Resolve a live entity id by exact current name in the KG table.
    pub fn entity_id_of(&self, name: &str) -> Result<Option<EntityId>> {
        let conn = self.db.lock();
        Ok(self.get_entity_id_and_name(&conn, name)?.map(|(id, _)| id))
    }

    pub fn resolve_name_type(&self, id: EntityId) -> (String, String) {
        let conn = self.db.lock();
        self.get_entity_name_type(&conn, id)
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// Resolve one owner to its display row: `(name, entityType, kind)`.
    ///
    /// An entity row shows its current name and type, marked `"entity"`. A
    /// relation row shows the live triple `from -> TYPE -> to` and the relation
    /// type, marked `"relation"`. A row whose subject is gone (deleted entity,
    /// deleted or endpoint-less relation) is `None`, not a placeholder: the
    /// caller drops it instead of serving a name that no longer exists.
    pub fn resolve_owner(
        &self,
        owner_kind: OwnerKind,
        owner_id: i64,
    ) -> Result<Option<(String, String, String)>> {
        match owner_kind {
            OwnerKind::Entity => {
                let conn = self.db.lock();
                let Some((name, etype)) = self.get_entity_name_type(&conn, owner_id)? else {
                    return Ok(None);
                };
                Ok(Some((name, etype, "entity".to_string())))
            }
            OwnerKind::Relation => {
                let conn = self.db.lock();
                let row: Option<(String, String, String)> = conn
                    .query_row(
                        "SELECT f.name, d.name, t.name FROM taxonomy_relation m
                         JOIN entity f ON f.id=m.from_id JOIN entity t ON t.id=m.to_id
                         JOIN type_dict d ON d.id=m.type_id
                         WHERE m.id=?1 AND m.deleted=0 AND f.flags=0 AND t.flags=0",
                        [owner_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(sqlite_err)?;
                Ok(row
                    .map(|(f, ty, t)| (format!("{f} -> {ty} -> {t}"), ty, "relation".to_string())))
            }
        }
    }

    /// The stored text of one chunk, reassembled from SQL for `includeChunks`.
    ///
    /// Identity text is `name \n type`, observation text is the body, and
    /// relation text is the triple `from \n type \n to` — the exact texts the
    /// worker embedded. `None` when the underlying row is gone (or, for
    /// entity chunks, when the store never wrote that chunk kind).
    pub fn chunk_text(&self, hit: &ChunkHit) -> Option<String> {
        match hit.owner_kind {
            OwnerKind::Entity => match hit.chunk_kind {
                ChunkKind::Identity => {
                    let conn = self.db.lock();
                    conn.query_row(
                        "SELECT e.name || char(10) || COALESCE(t.name,'')
                         FROM entity e LEFT JOIN type_dict t ON t.id=e.type_id WHERE e.id=?1",
                        [hit.owner_id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(sqlite_err)
                    .ok()
                    .flatten()
                }
                ChunkKind::Observation => {
                    let conn = self.db.lock();
                    // The chunk_index is the observation idx.
                    conn.query_row(
                        "SELECT body FROM observation WHERE entity_id=?1 AND idx=?2",
                        params![hit.owner_id, hit.chunk_index],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(sqlite_err)
                    .ok()
                    .flatten()
                }
                _ => None,
            },
            OwnerKind::Relation => {
                // Rebuild "from \n type \n to" from the mirror.
                let conn = self.db.lock();
                conn.query_row(
                    "SELECT f.name || char(10) || d.name || char(10) || t.name
                     FROM taxonomy_relation m JOIN entity f ON f.id=m.from_id
                     JOIN entity t ON t.id=m.to_id JOIN type_dict d ON d.id=m.type_id
                     WHERE m.id=?1",
                    [hit.owner_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sqlite_err)
                .ok()
                .flatten()
            }
        }
    }

    pub fn invalidate_entity_cache(&self, names: &[String]) {
        for name in names {
            self.forget_entity_name(name);
        }
    }

    pub fn name_to_id(&self) -> &DashMap<String, EntityId> {
        &self.name_to_id
    }

    pub fn id_to_name(&self) -> &DashMap<EntityId, String> {
        &self.id_to_name
    }

    /// Insert `chunk_vector` rows for the profile serving chunk reads, the
    /// worker-shaped payload a store test cannot easily produce. Test-only:
    /// the worker's own path is covered in `tests/indexer_worker.rs`.
    #[cfg(test)]
    pub fn seed_test_chunks(&self, chunks: &[SeedChunk]) -> Result<()> {
        let conn = self.db.lock();
        let Some(profile) = self.serving_profile_id(&conn)? else {
            return Err(MCSError::InvalidParams(
                "no serving profile to seed chunk rows into".into(),
            ));
        };
        for chunk in chunks {
            let bytes: Vec<u8> = chunk
                .vector
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect();
            conn.execute(
                "INSERT INTO chunk_vector(profile_id,kind,owner_kind,owner_id,chunk_index,type_id,owner_revision,blob,created_at_us,source)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    profile.to_string(),
                    chunk.chunk_kind.as_str(),
                    chunk.owner_kind.as_str(),
                    chunk.owner_id,
                    chunk.chunk_index,
                    chunk.type_id,
                    1i64,
                    bytes,
                    now_micros(),
                    "test",
                ],
            )
            .map_err(sqlite_err)?;
        }
        Ok(())
    }
}

fn write_f32(buf: &mut String, val: f32) {
    use std::fmt::Write;
    write!(buf, r#","score":{:.6}"#, val).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Durability, SqliteTuning};
    use crate::kg::GraphHandle;
    use crate::types::EntityInput as Entity;
    use std::num::NonZeroUsize;

    struct TestEnv {
        kg: GraphHandle,
        vs: VectorStore,
        _dir: tempfile::TempDir,
    }

    fn setup(dims: u32) -> TestEnv {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let lru = NonZeroUsize::new(10000).unwrap();
        let kg =
            GraphHandle::new(&db_path, Durability::Async, SqliteTuning::default(), lru, 4).unwrap();
        let vs = VectorStore::new(&db_path, dims).unwrap();
        TestEnv { kg, vs, _dir: dir }
    }

    fn create_test_entity(kg: &GraphHandle, name: &str, etype: &str) {
        kg.create_entities(&[Entity {
            name: name.into(),
            entity_type: etype.into(),
            observations: vec!["test observation".into()],
        }])
        .unwrap();
    }

    fn make_embedding(dims: u32, value: f32) -> Vec<f32> {
        vec![value; dims as usize]
    }

    /// Resolve one entity's id by name in the KG. GraphHandle exposes no ids,
    /// so the test reads them through the store's own resolver.
    fn entity_id_of(env: &TestEnv, name: &str) -> i64 {
        env.vs.entity_id_of(name).unwrap().unwrap()
    }

    /// Resolve the `type_dict` id for an entity type name (kind 0), the id
    /// the worker stamps on that entity's chunk rows.
    fn type_id_of(env: &TestEnv, type_name: &str) -> i64 {
        let conn = env.vs.db.lock();
        conn.query_row(
            "SELECT id FROM type_dict WHERE kind=0 AND name=?1",
            [type_name],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
    }

    /// Register one serving profile in the store. The taxonomy tables carry
    /// per-kind generations for this profile, which adopt() serves.
    fn seed_taxonomy_profile(env: &TestEnv, dims: u32) -> uuid::Uuid {
        use mcpmem_core::jobs::{DistanceMetric, IndexProfile, Normalization};
        let profile = IndexProfile {
            id: uuid::Uuid::new_v4(),
            store_key: "default".into(),
            provider_kind: "test".into(),
            model: "test".into(),
            dimensions: dims,
            representation_version: "v1".into(),
            normalization: Normalization::None,
            distance_metric: DistanceMetric::L2Squared,
            vector_encoding_version: "f32le-v1".into(),
        };
        let conn = env.vs.db.lock();
        conn.execute(
            "INSERT INTO index_profile VALUES(?1,'default',?2,?3,'Active')",
            params![
                profile.id.to_string(),
                "taxonomy-test-fixture",
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

    fn seed_taxonomy_generation(env: &TestEnv, profile: uuid::Uuid, kind: i64, durable: i64) {
        let conn = env.vs.db.lock();
        conn.execute(
            "INSERT INTO taxonomy_ann_generation(profile_id,subject_kind,durable_generation) VALUES(?1,?2,?3)",
            params![profile.to_string(), kind, durable],
        )
        .unwrap();
    }

    fn seed_taxonomy_vector(
        env: &TestEnv,
        profile: uuid::Uuid,
        kind: i64,
        id: i64,
        revision: i64,
        embedding: &[f32],
    ) {
        let bytes: Vec<u8> = embedding
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        let conn = env.vs.db.lock();
        conn.execute(
            "INSERT INTO taxonomy_vector VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(profile_id,subject_kind,subject_id) DO UPDATE SET subject_revision=excluded.subject_revision,blob=excluded.blob,created_at_us=excluded.created_at_us,source=excluded.source",
            params![profile.to_string(), kind, id, revision, bytes, 1i64, "test"],
        )
        .unwrap();
    }

    fn seed_type_dict(env: &TestEnv, id: i64, kind: i64, name: &str) {
        let conn = env.vs.db.lock();
        conn.execute(
            "INSERT INTO type_dict(id,kind,name,count,revision) VALUES(?1,?2,?3,1,1)",
            params![id, kind, name],
        )
        .unwrap();
    }

    fn adopt(env: &mut TestEnv, candidate: bool) -> Result<()> {
        // A worker-style connection, like mcpmem-indexer's run_once: the
        // registry owns it, and adopt_taxonomy shares its transaction view.
        let conn = rusqlite::Connection::open(&env.vs.db_path).unwrap();
        let registry = IndexProfileRegistry::new(&conn);
        env.vs.adopt_taxonomy(&registry, candidate)
    }

    #[test]
    fn taxonomy_snapshots_build_and_serve_one_kind_at_a_time() {
        let mut env = setup(4);
        let profile = seed_taxonomy_profile(&env, 4);
        seed_taxonomy_generation(&env, profile, 0, 1);
        seed_taxonomy_vector(&env, profile, 0, 7, 1, &make_embedding(4, 1.0));
        adopt(&mut env, false).unwrap();
        assert_eq!(
            env.vs
                .search_taxonomy(TaxonomyKind::EntityType, &[1.0; 4], 10)
                .unwrap(),
            vec![(7, 0.0)]
        );
        // A kind without a generation row stays absent: empty, not an error.
        assert!(
            env.vs
                .search_taxonomy(TaxonomyKind::RelationType, &[1.0; 4], 10)
                .unwrap()
                .is_empty()
        );
        // The remaining kinds adopt on their own generation rows.
        seed_taxonomy_generation(&env, profile, 1, 1);
        seed_taxonomy_vector(&env, profile, 1, 9, 1, &make_embedding(4, -3.0));
        seed_taxonomy_generation(&env, profile, 2, 1);
        // Kind 2 derives its snapshot from the relation chunk (Task 5).
        env.vs
            .seed_test_chunks(&[SeedChunk {
                owner_kind: OwnerKind::Relation,
                owner_id: 3,
                chunk_kind: ChunkKind::Relation,
                chunk_index: 0,
                type_id: 1,
                vector: &make_embedding(4, 5.0),
            }])
            .unwrap();
        adopt(&mut env, false).unwrap();
        assert_eq!(
            env.vs
                .search_taxonomy(TaxonomyKind::RelationType, &[1.0; 4], 10)
                .unwrap(),
            vec![(9, 64.0)]
        );
        assert_eq!(
            env.vs
                .search_taxonomy(TaxonomyKind::Relation, &[5.0; 4], 10)
                .unwrap(),
            vec![(3, 0.0)]
        );
    }

    #[test]
    fn taxonomy_search_returns_the_nearest_subject() {
        let mut env = setup(4);
        let profile = seed_taxonomy_profile(&env, 4);
        seed_taxonomy_generation(&env, profile, 0, 1);
        for (id, value) in [(7, -2.0), (8, 4.0), (9, 1.0)] {
            seed_taxonomy_vector(&env, profile, 0, id, 1, &make_embedding(4, value));
        }
        adopt(&mut env, false).unwrap();
        let hits = env
            .vs
            .search_taxonomy(TaxonomyKind::EntityType, &[3.9; 4], 10)
            .unwrap();
        assert_eq!(hits[0].0, 8);
        assert!(hits[0].1 < hits[1].1);
        assert_eq!(
            env.vs
                .search_taxonomy(TaxonomyKind::EntityType, &[3.9; 4], 2)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn taxonomy_resolve_maps_ids_to_names_for_all_kinds() {
        let env = setup(4);
        create_test_entity(&env.kg, "alice", "person");
        create_test_entity(&env.kg, "acme", "organization");
        let alice = env.vs.entity_id_of("alice").unwrap().unwrap();
        let acme = env.vs.entity_id_of("acme").unwrap().unwrap();
        seed_type_dict(&env, 7, 0, "person");
        seed_type_dict(&env, 8, 1, "works_at");
        {
            let conn = env.vs.db.lock();
            conn.execute(
                "INSERT INTO taxonomy_relation(id,from_id,to_id,type_id,revision,deleted) VALUES(42,?1,?2,?3,1,0)",
                params![alice, acme, 8],
            )
            .unwrap();
        }
        assert_eq!(
            env.vs.resolve_taxonomy(TaxonomyKind::EntityType, 7),
            Some(("person".to_string(), "entityType".to_string()))
        );
        assert_eq!(
            env.vs.resolve_taxonomy(TaxonomyKind::RelationType, 8),
            Some(("works_at".to_string(), "relationType".to_string()))
        );
        assert_eq!(
            env.vs.resolve_taxonomy(TaxonomyKind::Relation, 42),
            Some((
                "alice -[works_at]-> acme".to_string(),
                "relation".to_string()
            ))
        );
        assert_eq!(env.vs.resolve_taxonomy(TaxonomyKind::Relation, 999), None);
        assert_eq!(env.vs.resolve_taxonomy(TaxonomyKind::EntityType, 999), None);
    }

    #[test]
    fn taxonomy_resolve_relation_with_a_deleted_endpoint_returns_none() {
        let env = setup(4);
        create_test_entity(&env.kg, "alice", "person");
        create_test_entity(&env.kg, "acme", "organization");
        let alice = env.vs.entity_id_of("alice").unwrap().unwrap();
        let acme = env.vs.entity_id_of("acme").unwrap().unwrap();
        seed_type_dict(&env, 8, 1, "works_at");
        {
            let conn = env.vs.db.lock();
            conn.execute(
                "INSERT INTO taxonomy_relation(id,from_id,to_id,type_id,revision,deleted) VALUES(42,?1,?2,?3,1,0)",
                params![alice, acme, 8],
            )
            .unwrap();
        }
        assert!(
            env.vs
                .resolve_taxonomy(TaxonomyKind::Relation, 42)
                .is_some()
        );
        // The endpoint vanishes (delete or merge); the mirror row stays, and
        // the resolution must refuse instead of rendering a placeholder.
        env.kg.delete_entities(&["alice".into()]).unwrap();
        assert_eq!(env.vs.resolve_taxonomy(TaxonomyKind::Relation, 42), None);
    }

    #[test]
    fn taxonomy_absent_snapshot_returns_an_empty_search() {
        let mut env = setup(4);
        // Nothing adopted yet: empty result, not an error.
        assert!(
            env.vs
                .search_taxonomy(TaxonomyKind::Relation, &[1.0; 4], 10)
                .unwrap()
                .is_empty()
        );
        // A generation with no vector rows builds an empty snapshot.
        let profile = seed_taxonomy_profile(&env, 4);
        seed_taxonomy_generation(&env, profile, 0, 7);
        adopt(&mut env, false).unwrap();
        assert!(
            env.vs
                .search_taxonomy(TaxonomyKind::EntityType, &[1.0; 4], 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn taxonomy_stale_generation_refuses_to_publish() {
        let mut env = setup(4);
        let profile = seed_taxonomy_profile(&env, 4);
        seed_taxonomy_generation(&env, profile, 0, 5);
        seed_taxonomy_vector(&env, profile, 0, 7, 1, &make_embedding(4, 1.0));
        adopt(&mut env, false).unwrap();
        assert_eq!(
            env.vs
                .search_taxonomy(TaxonomyKind::EntityType, &[1.0; 4], 10)
                .unwrap(),
            vec![(7, 0.0)]
        );
        // The worker overwrites the vector but durable stays at 5: the
        // unchanged-generation early return keeps serving the old snapshot.
        seed_taxonomy_vector(&env, profile, 0, 7, 1, &make_embedding(4, 2.0));
        adopt(&mut env, false).unwrap();
        assert_eq!(
            env.vs
                .search_taxonomy(TaxonomyKind::EntityType, &[1.0; 4], 10)
                .unwrap(),
            vec![(7, 0.0)]
        );
        // And it must not have re-published the same generation.
        let conn = env.vs.db.lock();
        let published: i64 = conn
            .query_row(
                "SELECT published_generation FROM taxonomy_ann_generation WHERE profile_id=?1 AND subject_kind=0",
                [profile.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(published, 5);
        // A build prepared against generation 5 is refused once durable
        // advanced to 6; the gate accepts only the current generation.
        conn.execute(
            "UPDATE taxonomy_ann_generation SET durable_generation=6 WHERE profile_id=?1 AND subject_kind=0",
            [profile.to_string()],
        )
        .unwrap();
        assert!(!mark_taxonomy_published(&conn, profile, 0, 5).unwrap());
        assert!(mark_taxonomy_published(&conn, profile, 0, 6).unwrap());
        drop(conn);
        // The normal flow: durable advanced, the next adopt rebuilds, serves
        // the new vector and publishes generation 6.
        seed_taxonomy_vector(&env, profile, 0, 7, 2, &make_embedding(4, 2.0));
        adopt(&mut env, false).unwrap();
        assert_eq!(
            env.vs
                .search_taxonomy(TaxonomyKind::EntityType, &[1.0; 4], 10)
                .unwrap(),
            vec![(7, 4.0)]
        );
        let conn = env.vs.db.lock();
        let published: i64 = conn
            .query_row(
                "SELECT published_generation FROM taxonomy_ann_generation WHERE profile_id=?1 AND subject_kind=0",
                [profile.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(published, 6);
    }

    #[test]
    fn taxonomy_candidate_refuses_when_the_scan_is_invalid() {
        let mut env = setup(4);
        let profile = seed_taxonomy_profile(&env, 4);
        seed_taxonomy_generation(&env, profile, 0, 1);
        seed_type_dict(&env, 7, 0, "person");
        seed_taxonomy_vector(&env, profile, 0, 7, 1, &make_embedding(4, 1.0));
        {
            // A live job makes the kind scan-clean.
            let conn = env.vs.db.lock();
            conn.execute(
                "INSERT INTO taxonomy_job(subject_kind,subject_id,profile_id,subject_revision,operation,state) VALUES(0,7,?1,1,'upsert','pending')",
                [profile.to_string()],
            )
            .unwrap();
        }
        adopt(&mut env, true).unwrap();
        assert_eq!(
            env.vs
                .search_taxonomy(TaxonomyKind::EntityType, &[1.0; 4], 10)
                .unwrap(),
            vec![(7, 0.0)]
        );
        // A stale source revision makes the candidate scan invalid: the build
        // is refused and the served snapshot is not replaced.
        {
            let conn = env.vs.db.lock();
            conn.execute("UPDATE type_dict SET revision=2 WHERE id=7", [])
                .unwrap();
        }
        let error = adopt(&mut env, true).unwrap_err();
        assert!(error.to_string().contains("taxonomy"));
        assert_eq!(
            env.vs
                .search_taxonomy(TaxonomyKind::EntityType, &[1.0; 4], 10)
                .unwrap(),
            vec![(7, 0.0)]
        );
    }

    #[test]
    fn reconcile_activates_the_entity_when_taxonomy_scan_is_invalid() {
        // D10 wiring: adoption must not break the entity path. A dead
        // taxonomy job keeps a kind scan-invalid forever, so a refused
        // adoption must not defer the entity activation; it logs and the
        // entity side proceeds.
        let env = setup(4);
        let profile = seed_taxonomy_profile(&env, 4);
        {
            let conn = env.vs.db.lock();
            conn.execute(
                "UPDATE index_profile_registry SET state='Rebuilding',serving_profile=NULL,candidate_profile=?1 WHERE store_key='default'",
                [profile.to_string()],
            )
            .unwrap();
            // begin_rebuild also seeds the entity generation row; the seed
            // helper leaves it out because nothing else calls reconcile.
            conn.execute(
                "INSERT INTO ann_generation(profile_id) VALUES(?1)",
                [profile.to_string()],
            )
            .unwrap();
        }
        seed_taxonomy_generation(&env, profile, 0, 1);
        seed_type_dict(&env, 7, 0, "person");
        // The subject's only job is dead, so the kind scan is invalid on
        // every poll: the adoption refuses forever.
        {
            let conn = env.vs.db.lock();
            conn.execute(
                "INSERT INTO taxonomy_job(subject_kind,subject_id,profile_id,subject_revision,operation,state) VALUES(0,7,?1,1,'upsert','dead')",
                [profile.to_string()],
            )
            .unwrap();
        }
        env.vs.reconcile_managed_snapshot().unwrap();
        assert!(matches!(
            IndexProfileRegistry::new(&env.vs.db.lock()).state("default").unwrap(),
            StoreState::Active(active) if active == profile
        ));
        // The taxonomy side stays deferred: the dead subject is not served.
        assert!(
            env.vs
                .search_taxonomy(TaxonomyKind::EntityType, &[1.0; 4], 10)
                .unwrap()
                .is_empty()
        );
        // A repeated reconcile stays green and keeps the taxonomy deferred.
        env.vs.reconcile_managed_snapshot().unwrap();
        assert!(
            env.vs
                .search_taxonomy(TaxonomyKind::EntityType, &[1.0; 4], 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn snapshot_serves_filtered_chunk_hits() {
        let env = setup(4);
        // Reconcile reads the durable generation of the serving profile, the
        // same shape the taxonomy tests use to drive a managed snapshot.
        let profile = seed_taxonomy_profile(&env, 4);
        {
            let conn = env.vs.db.lock();
            conn.execute(
                "INSERT INTO ann_generation(profile_id) VALUES(?1)",
                [profile.to_string()],
            )
            .unwrap();
        }
        create_test_entity(&env.kg, "ada", "Person");
        env.kg
            .add_observations("ada", &["writes rust".into()])
            .unwrap();
        create_test_entity(&env.kg, "acme", "Company");
        // Seed chunk rows directly (worker path is covered in indexer_worker):
        env.vs
            .seed_test_chunks(&[
                SeedChunk {
                    owner_kind: OwnerKind::Entity,
                    owner_id: entity_id_of(&env, "ada"),
                    chunk_kind: ChunkKind::Identity,
                    chunk_index: 0,
                    type_id: type_id_of(&env, "Person"),
                    vector: &[1.0, 0.0, 0.0, 0.0],
                },
                SeedChunk {
                    owner_kind: OwnerKind::Entity,
                    owner_id: entity_id_of(&env, "ada"),
                    chunk_kind: ChunkKind::Observation,
                    chunk_index: 1,
                    type_id: type_id_of(&env, "Person"),
                    vector: &[0.9, 0.1, 0.0, 0.0],
                },
                SeedChunk {
                    owner_kind: OwnerKind::Entity,
                    owner_id: entity_id_of(&env, "acme"),
                    chunk_kind: ChunkKind::Identity,
                    chunk_index: 0,
                    type_id: type_id_of(&env, "Company"),
                    vector: &[0.0, 0.0, 0.0, 1.0],
                },
            ])
            .expect("seed chunk rows into the serving profile");
        env.vs.reconcile_managed_snapshot().unwrap();
        let hits = env
            .vs
            .search_chunks(&[1.0, 0.0, 0.0, 0.0], 10, Some("entity"), Some("Person"))
            .unwrap();
        assert_eq!(hits.len(), 2, "Person chunks only");
        for hit in &hits {
            assert_eq!(hit.owner_kind, OwnerKind::Entity);
        }
        let owners = env.vs.aggregate_owners(&hits, 10);
        assert_eq!(owners.len(), 1, "one owner, best chunk wins");
        let ada = entity_id_of(&env, "ada");
        assert_eq!(owners[0].1, ada);
        // Best-chunk discrimination: ada's identity chunk sits at distance 0
        // and wins, so the aggregated distance is the identity distance and no
        // observation index is reported.
        assert_eq!(owners[0].2, 0.0, "the winning chunk is the identity chunk");
        assert_eq!(owners[0].3, None, "an identity win carries no chunk index");
        // An owner whose observation is its best chunk must win with that
        // chunk: bob's observation (distance ~0.0002) beats his identity
        // (0.08), so the aggregate reports the observation index.
        create_test_entity(&env.kg, "bob", "Person");
        let bob = entity_id_of(&env, "bob");
        env.vs
            .seed_test_chunks(&[
                SeedChunk {
                    owner_kind: OwnerKind::Entity,
                    owner_id: bob,
                    chunk_kind: ChunkKind::Identity,
                    chunk_index: 0,
                    type_id: type_id_of(&env, "Person"),
                    vector: &[0.8, 0.2, 0.0, 0.0],
                },
                SeedChunk {
                    owner_kind: OwnerKind::Entity,
                    owner_id: bob,
                    chunk_kind: ChunkKind::Observation,
                    chunk_index: 1,
                    type_id: type_id_of(&env, "Person"),
                    vector: &[0.99, 0.01, 0.0, 0.0],
                },
            ])
            .expect("seed bob's chunks");
        {
            // The snapshot is immutable per generation: a durable advance is
            // what the worker's commit produces before the next reconcile.
            let conn = env.vs.db.lock();
            conn.execute(
                "UPDATE ann_generation SET durable_generation=durable_generation+1 WHERE profile_id=?1",
                [profile.to_string()],
            )
            .unwrap();
        }
        env.vs.reconcile_managed_snapshot().unwrap();
        let hits = env
            .vs
            .search_chunks(&[1.0, 0.0, 0.0, 0.0], 10, Some("entity"), Some("Person"))
            .unwrap();
        let owners = env.vs.aggregate_owners(&hits, 10);
        assert_eq!(owners.len(), 2, "ada and bob now both match Person");
        assert_eq!(
            owners[0].1, ada,
            "ada's identity chunk is the nearest of all"
        );
        assert_eq!(owners[0].2, 0.0);
        assert_eq!(owners[0].3, None);
        // Bob's best chunk is his observation (0.0002), not his identity
        // (0.08): the aggregate must report the observation's index.
        assert_eq!(owners[1].1, bob);
        assert!(
            owners[1].2 > 0.0 && owners[1].2 < 0.01,
            "observation distance, not identity"
        );
        assert_eq!(
            owners[1].3,
            Some(1),
            "the winning chunk is bob's observation"
        );
    }

    #[test]
    fn identity_vector_serves_the_identity_chunk_of_an_entity() {
        let env = setup(4);
        let profile = seed_taxonomy_profile(&env, 4);
        create_test_entity(&env.kg, "ada", "Person");
        let ada = entity_id_of(&env, "ada");
        env.vs
            .seed_test_chunks(&[
                SeedChunk {
                    owner_kind: OwnerKind::Entity,
                    owner_id: ada,
                    chunk_kind: ChunkKind::Identity,
                    chunk_index: 0,
                    type_id: type_id_of(&env, "Person"),
                    vector: &[1.0, 0.0, 0.0, 0.0],
                },
                SeedChunk {
                    owner_kind: OwnerKind::Entity,
                    owner_id: ada,
                    chunk_kind: ChunkKind::Observation,
                    chunk_index: 1,
                    type_id: type_id_of(&env, "Person"),
                    vector: &[0.9, 0.1, 0.0, 0.0],
                },
            ])
            .expect("seed identity and observation chunks");
        // Publish a managed snapshot: the failed-state arm mirrors reconcile,
        // which keeps serving whatever snapshot was already published.
        {
            let conn = env.vs.db.lock();
            conn.execute(
                "INSERT INTO ann_generation(profile_id) VALUES(?1)",
                [profile.to_string()],
            )
            .unwrap();
        }
        env.vs.reconcile_managed_snapshot().unwrap();
        assert_eq!(
            env.vs.identity_vector("ada").unwrap(),
            Some(vec![1.0, 0.0, 0.0, 0.0])
        );
        assert_eq!(
            env.vs
                .owner_identity_vector(OwnerKind::Entity, ada)
                .unwrap(),
            Some(vec![1.0, 0.0, 0.0, 0.0])
        );
        // Missing entities and owners have no identity chunk.
        assert!(env.vs.identity_vector("nobody").unwrap().is_none());
        assert!(
            env.vs
                .owner_identity_vector(OwnerKind::Entity, ada + 1000)
                .unwrap()
                .is_none()
        );
        // While a rebuild runs, the candidate profile serves chunk reads.
        {
            let conn = env.vs.db.lock();
            conn.execute(
                "UPDATE index_profile_registry SET state='Rebuilding',serving_profile=NULL,candidate_profile=?1 WHERE store_key='default'",
                [profile.to_string()],
            )
            .unwrap();
        }
        assert_eq!(
            env.vs
                .owner_identity_vector(OwnerKind::Entity, ada)
                .unwrap(),
            Some(vec![1.0, 0.0, 0.0, 0.0])
        );
        // A failed rebuild keeps serving what the snapshot already published,
        // exactly like reconcile: identity reads still work even though the
        // registry no longer points at a serving profile.
        {
            let conn = env.vs.db.lock();
            conn.execute(
                "UPDATE index_profile_registry SET state='Failed',serving_profile=NULL,candidate_profile=?1,failure_reason='boom' WHERE store_key='default'",
                [profile.to_string()],
            )
            .unwrap();
        }
        assert_eq!(
            env.vs
                .owner_identity_vector(OwnerKind::Entity, ada)
                .unwrap(),
            Some(vec![1.0, 0.0, 0.0, 0.0])
        );
    }

    #[test]
    fn identity_vector_follows_the_serving_profile_dimension() {
        // The store is configured at 4 dims (`--embedding-dims 4`), but the
        // serving profile embeds at 8. The chunk-length gate must follow the
        // profile: gating on the CLI default would drop the identity chunk
        // and break every search-by-entity read on such a store.
        let env = setup(4);
        let profile = seed_taxonomy_profile(&env, 8);
        create_test_entity(&env.kg, "ada", "Person");
        let ada = entity_id_of(&env, "ada");
        env.vs
            .seed_test_chunks(&[SeedChunk {
                owner_kind: OwnerKind::Entity,
                owner_id: ada,
                chunk_kind: ChunkKind::Identity,
                chunk_index: 0,
                type_id: type_id_of(&env, "Person"),
                vector: &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            }])
            .expect("seed the identity chunk at profile dims");
        {
            let conn = env.vs.db.lock();
            conn.execute(
                "INSERT INTO ann_generation(profile_id) VALUES(?1)",
                [profile.to_string()],
            )
            .unwrap();
        }
        env.vs.reconcile_managed_snapshot().unwrap();
        assert_eq!(
            env.vs.identity_vector("ada").unwrap(),
            Some(vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
        );
        assert_eq!(
            env.vs
                .owner_identity_vector(OwnerKind::Entity, ada)
                .unwrap(),
            Some(vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
        );
    }

    #[test]
    fn test_vector_scratch_buffer() {
        with_scratch(|buf| {
            buf.push(1.0);
            buf.push(2.0);
            assert_eq!(buf.len(), 2);
        });
        with_scratch(|buf| {
            assert!(buf.is_empty());
            buf.extend_from_slice(&[3.0, 4.0, 5.0]);
            assert_eq!(buf.len(), 3);
        });
    }

    #[test]
    fn test_vector_rebuild_graph_cache() {
        let env = setup(4);
        create_test_entity(&env.kg, "alice", "person");
        create_test_entity(&env.kg, "bob", "person");
        create_test_entity(&env.kg, "charlie", "person");

        env.kg
            .create_relations(&[crate::types::Relation {
                from: "alice".into(),
                to: "bob".into(),
                relation_type: "knows".into(),
            }])
            .unwrap();

        env.vs.rebuild_graph_cache().unwrap();
        assert_eq!(env.vs.graph_node_count(), 3);
        assert_eq!(env.vs.graph_edge_count(), 1);
    }

    /// The mirror row `(from_id, to_id, type_id)` of one relation owner.
    fn relation_row(env: &TestEnv, id: i64) -> (i64, i64, i64) {
        let conn = env.vs.db.lock();
        conn.query_row(
            "SELECT from_id, to_id, type_id FROM taxonomy_relation WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
    }

    #[test]
    fn resolve_owner_renders_entity_and_relation_rows() {
        let env = setup(4);
        create_test_entity(&env.kg, "ada", "Person");
        create_test_entity(&env.kg, "acme", "Company");
        let ada = entity_id_of(&env, "ada");
        let acme = entity_id_of(&env, "acme");

        let (name, etype, kind) = env
            .vs
            .resolve_owner(OwnerKind::Entity, ada)
            .unwrap()
            .expect("ada is a live entity");
        assert_eq!(
            (name.as_str(), etype.as_str(), kind.as_str()),
            ("ada", "Person", "entity")
        );

        // Unknown entity: None, not a placeholder.
        assert!(
            env.vs
                .resolve_owner(OwnerKind::Entity, 999_999)
                .unwrap()
                .is_none()
        );

        // A deleted relation resolves to None even when its endpoints live on.
        {
            let conn = env.vs.db.lock();
            conn.execute("INSERT INTO type_dict(kind,name) VALUES(1,'works_at')", [])
                .unwrap();
            let type_id: i64 = conn
                .query_row(
                    "SELECT id FROM type_dict WHERE kind=1 AND name='works_at'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            conn.execute(
                "INSERT INTO taxonomy_relation(id,from_id,to_id,type_id,revision,deleted) VALUES(42,?1,?2,?3,1,0)",
                params![ada, acme, type_id],
            )
            .unwrap();
            // The mirror allows one id per triple, so the deleted twin uses a
            // different triple (acme -> ada) and the same relation type.
            conn.execute(
                "INSERT INTO taxonomy_relation(id,from_id,to_id,type_id,revision,deleted) VALUES(43,?1,?2,?3,1,1)",
                params![acme, ada, type_id],
            )
            .unwrap();
        }

        let (name, etype, kind) = env
            .vs
            .resolve_owner(OwnerKind::Relation, 42)
            .unwrap()
            .expect("the live relation row resolves");
        assert_eq!(
            (name.as_str(), etype.as_str(), kind.as_str()),
            ("ada -> works_at -> acme", "works_at", "relation")
        );

        let (from_id, to_id, _) = relation_row(&env, 42);
        assert_eq!((from_id, to_id), (ada, acme));
        assert!(
            env.vs
                .resolve_owner(OwnerKind::Relation, 43)
                .unwrap()
                .is_none(),
            "a deleted relation must not render a row"
        );
        assert!(
            env.vs
                .resolve_owner(OwnerKind::Relation, 999_999)
                .unwrap()
                .is_none(),
            "an unknown relation must not render a row"
        );
    }

    #[test]
    fn chunk_text_reassembles_identity_observation_and_relation() {
        let env = setup(4);
        // create_test_entity writes one observation at idx 0.
        create_test_entity(&env.kg, "ada", "Person");
        create_test_entity(&env.kg, "acme", "Company");
        let ada = entity_id_of(&env, "ada");
        let acme = entity_id_of(&env, "acme");
        {
            let conn = env.vs.db.lock();
            let type_id: i64 = conn
                .query_row(
                    "SELECT id FROM type_dict WHERE kind=1 AND name='works_at'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or_else(|_| {
                    conn.execute("INSERT INTO type_dict(kind,name) VALUES(1,'works_at')", [])
                        .unwrap();
                    conn.query_row(
                        "SELECT id FROM type_dict WHERE kind=1 AND name='works_at'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap()
                });
            conn.execute(
                "INSERT INTO taxonomy_relation(id,from_id,to_id,type_id,revision,deleted) VALUES(42,?1,?2,?3,1,0)",
                params![ada, acme, type_id],
            )
            .unwrap();
        }

        let identity = ChunkHit {
            owner_kind: OwnerKind::Entity,
            owner_id: ada,
            chunk_kind: ChunkKind::Identity,
            chunk_index: 0,
            type_id: 0,
            dist: 0.0,
        };
        assert_eq!(
            env.vs.chunk_text(&identity).unwrap(),
            "ada\nPerson",
            "identity text is name \\n type"
        );

        let observation = ChunkHit {
            owner_kind: OwnerKind::Entity,
            owner_id: ada,
            chunk_kind: ChunkKind::Observation,
            chunk_index: 0,
            type_id: 0,
            dist: 0.0,
        };
        assert_eq!(
            env.vs.chunk_text(&observation).unwrap(),
            "test observation",
            "observation text is the body"
        );

        let relation = ChunkHit {
            owner_kind: OwnerKind::Relation,
            owner_id: 42,
            chunk_kind: ChunkKind::Relation,
            chunk_index: 0,
            type_id: 0,
            dist: 0.0,
        };
        assert_eq!(
            env.vs.chunk_text(&relation).unwrap(),
            "ada\nworks_at\nacme",
            "relation text is the triple"
        );

        // A missing observation index is None, not an empty string.
        let missing = ChunkHit {
            owner_kind: OwnerKind::Entity,
            owner_id: ada,
            chunk_kind: ChunkKind::Observation,
            chunk_index: 7,
            type_id: 0,
            dist: 0.0,
        };
        assert!(env.vs.chunk_text(&missing).is_none());
    }
}
