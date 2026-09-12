use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use petgraph::Directed;
use petgraph::graph::NodeIndex;
use petgraph::stable_graph::StableGraph;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::errors::{MCSError, Result};
use crate::ivf::{IvfFlatIndex, Metric as IvfMetric};
use crate::kg::push_json_str;
use crate::turboquant::TurboQuantIndex;
use mcpmem_core::jobs::{
    AnnGenerationRepository, DistanceMetric, IndexProfileRegistry, StoreState,
    taxonomy_scan_invalid,
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
    /// A rebuild started. Every live entity is queued for embedding, and a
    /// direct `vector_*` write is refused from now on.
    RebuildStarted,
    /// The last rebuild into this profile failed and stays failed. The store
    /// keeps serving whatever it served before.
    PreviousRebuildFailed(String),
}
pub type EntityId = i64;

/// Number of concurrent searcher threads reserved inside the usearch HNSW
/// index. usearch hard-fails a search ("Reserve capacity ahead of searches!")
/// when more threads query than were reserved, so [`SearchGate`] caps
/// concurrent searches at exactly this number — correctness never depends on
/// how many transport threads (stdio pipeline, HTTP handlers) pile in.
fn search_thread_cap() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .saturating_mul(2)
            .clamp(8, 64)
    })
}

/// Counting gate bounding concurrent HNSW searches to the reserved thread
/// capacity. Cheap: uncontended acquire is one mutex lock.
struct SearchGate {
    permits: Mutex<usize>,
    cv: parking_lot::Condvar,
}

impl SearchGate {
    const fn new(n: usize) -> Self {
        Self {
            permits: Mutex::new(n),
            cv: parking_lot::Condvar::new(),
        }
    }

    fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        let mut p = self.permits.lock();
        while *p == 0 {
            self.cv.wait(&mut p);
        }
        *p -= 1;
        drop(p);
        // Release on all exits, including a panicking `f`.
        struct Release<'a>(&'a SearchGate);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                *self.0.permits.lock() += 1;
                self.0.cv.notify_one();
            }
        }
        let _release = Release(self);
        f()
    }
}

/// Unifies the ANN backends behind one small surface so [`VectorStore`] does
/// not branch on the index kind at every call site. Distances follow usearch's
/// convention (smaller = closer) for all backends.
enum AnnIndex {
    Hnsw { index: Arc<Index>, gate: SearchGate },
    Ivf(Box<IvfFlatIndex>),
    Turbo(Box<TurboQuantIndex>),
}

impl AnnIndex {
    /// Current allocated capacity (HNSW) or live count (IVF/TurboQuant; both
    /// grow on demand).
    fn capacity(&self) -> usize {
        match self {
            AnnIndex::Hnsw { index, .. } => index.capacity(),
            AnnIndex::Ivf(i) => i.len(),
            AnnIndex::Turbo(i) => i.len(),
        }
    }

    /// Ensure room for `target` vectors. No-op for IVF/TurboQuant (growable
    /// `Vec`s). Always reserves [`search_thread_cap`] searcher threads —
    /// reserving 1 (as this once did) makes usearch reject any concurrent
    /// search with "Reserve capacity ahead of searches!".
    fn reserve(&self, target: usize) -> Result<()> {
        if let AnnIndex::Hnsw { index, .. } = self {
            index
                .reserve_capacity_and_threads(target, search_thread_cap())
                .map_err(|e| MCSError::MemoryError(format!("usearch reserve: {e}")))?;
        }
        Ok(())
    }

    /// Add `id`/`vector` to the index (caller has already removed any prior entry).
    fn add(&self, id: u64, vector: &[f32]) -> Result<()> {
        match self {
            AnnIndex::Hnsw { index, .. } => index
                .add(id, vector)
                .map_err(|e| MCSError::MemoryError(format!("usearch add: {e}"))),
            AnnIndex::Ivf(i) => i
                .upsert(id, vector)
                .map(|_| ())
                .map_err(MCSError::MemoryError),
            AnnIndex::Turbo(i) => i
                .upsert(id, vector)
                .map(|_| ())
                .map_err(MCSError::MemoryError),
        }
    }

    /// Remove `id`; returns whether it existed.
    fn remove(&self, id: u64) -> Result<bool> {
        match self {
            AnnIndex::Hnsw { index, .. } => index
                .remove(id)
                .map(|n| n > 0)
                .map_err(|e| MCSError::MemoryError(format!("usearch remove: {e}"))),
            AnnIndex::Ivf(i) => Ok(i.remove(id)),
            AnnIndex::Turbo(i) => Ok(i.remove(id)),
        }
    }

    /// Nearest `top_k` ids with distances (ascending). `nprobe` applies to IVF only.
    fn search(
        &self,
        query: &[f32],
        top_k: usize,
        nprobe: Option<usize>,
    ) -> Result<Vec<(u64, f32)>> {
        match self {
            AnnIndex::Hnsw { index, gate } => gate.run(|| {
                let m = index
                    .search(query, top_k)
                    .map_err(|e| MCSError::MemoryError(format!("usearch search: {e}")))?;
                let cap = m.keys.len().min(m.distances.len());
                Ok((0..cap).map(|j| (m.keys[j], m.distances[j])).collect())
            }),
            AnnIndex::Ivf(i) => i
                .search(query, top_k, nprobe)
                .map_err(MCSError::MemoryError),
            AnnIndex::Turbo(i) => i.search(query, top_k).map_err(MCSError::MemoryError),
        }
    }

    /// (Re)train the IVF centroids. No-op for HNSW and TurboQuant (the latter
    /// is data-oblivious — there is nothing to train).
    fn train(&self) -> Result<()> {
        if let AnnIndex::Ivf(i) = self {
            i.train().map_err(MCSError::MemoryError)?;
        }
        Ok(())
    }

    const fn kind(&self) -> IndexKind {
        match self {
            AnnIndex::Hnsw { .. } => IndexKind::Hnsw,
            AnnIndex::Ivf(_) => IndexKind::Ivf,
            AnnIndex::Turbo(_) => IndexKind::TurboQuant,
        }
    }

    fn memory_bytes(&self) -> usize {
        match self {
            AnnIndex::Hnsw { index, .. } => index.memory_usage(),
            AnnIndex::Ivf(i) => i.memory_bytes(),
            AnnIndex::Turbo(i) => i.memory_bytes(),
        }
    }

    /// (graph_bytes, vectors_bytes). IVF and TurboQuant have no graph, so their
    /// graph component is 0.
    fn memory_breakdown(&self) -> (usize, usize) {
        match self {
            AnnIndex::Hnsw { index, .. } => {
                let s = index.memory_stats();
                (
                    s.graph_allocated + s.graph_reserved,
                    s.vectors_allocated + s.vectors_reserved,
                )
            }
            AnnIndex::Ivf(i) => (0, i.memory_bytes()),
            AnnIndex::Turbo(i) => (0, i.memory_bytes()),
        }
    }
}

#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct BlobHeader {
    dims: u32,
}

/// The ANN index backend a [`VectorStore`] uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum IndexKind {
    /// usearch HNSW: best recall/latency, higher memory and build cost.
    #[default]
    Hnsw,
    /// IVF-Flat: k-means partitioned exact-within-cell search. Cheaper to build
    /// and lighter in memory; suits large, batch-ingested, periodically-rebuilt
    /// corpora. Exact (brute-force) until trained.
    Ivf,
    /// TurboQuant (arXiv:2504.19874): data-oblivious per-coordinate quantization
    /// with unbiased inner-product estimates. Stores only `tq_bits`-per-coordinate
    /// codes (plus two norms), needs zero training, and scans codes exhaustively
    /// with asymmetric distance estimation.
    TurboQuant,
}

/// Tunable parameters for the vector index. Built from CLI flags;
/// [`VectorConfig::new`] supplies the defaults used by tests and any caller that
/// only cares about the embedding dimension.
#[derive(Clone, Copy, Debug)]
pub struct VectorConfig {
    /// Embedding dimension. All upserted/queried vectors must match this.
    pub dims: u32,
    /// Which ANN backend to use.
    pub index_kind: IndexKind,
    /// Distance metric used by the index.
    pub metric: MetricKind,
    /// On-disk/in-index scalar representation (enables quantization). HNSW only.
    pub quantization: ScalarKind,
    /// HNSW graph degree (`M`). Higher = better recall, more memory.
    pub connectivity: usize,
    /// HNSW `efConstruction`. Higher = better index quality, slower inserts.
    pub expansion_add: usize,
    /// HNSW `efSearch`. Higher = better recall, slower queries.
    pub expansion_search: usize,
    /// IVF number of Voronoi cells (centroids). IVF only.
    pub ivf_nlist: usize,
    /// IVF default cells probed per query. IVF only.
    pub ivf_nprobe: usize,
    /// TurboQuant bits per coordinate (1-8). TurboQuant only.
    pub tq_bits: u32,
}

impl VectorConfig {
    /// Default HNSW configuration for the given embedding dimension.
    pub const fn new(dims: u32) -> Self {
        Self {
            dims,
            index_kind: IndexKind::Hnsw,
            metric: MetricKind::Cos,
            quantization: ScalarKind::F32,
            connectivity: 16,
            expansion_add: 200,
            expansion_search: 50,
            ivf_nlist: 256,
            ivf_nprobe: 8,
            tq_bits: 4,
        }
    }
}

pub struct VectorStore {
    /// Last observed names, maintained for callers inspecting the cache. Entity
    /// identity must be resolved against SQLite: other writers can rename rows.
    pub name_to_id: Arc<DashMap<String, EntityId>>,
    pub id_to_name: Arc<DashMap<EntityId, String>>,

    pub(crate) graph: Arc<RwLock<StableGraph<EntityId, (), Directed, u32>>>,
    pub(crate) node_map: Arc<DashMap<EntityId, NodeIndex<u32>>>,

    index: AnnIndex,
    pub(crate) db: Mutex<Connection>,

    pub dims: u32,
    pub count: AtomicUsize,
    /// Default cells probed per IVF query (ignored by HNSW).
    ivf_nprobe: usize,

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

struct ManagedSnapshot {
    profile: uuid::Uuid,
    durable_generation: i64,
    metric: DistanceMetric,
    vectors: Vec<(EntityId, Vec<f32>)>,
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

/// `INSERT OR REPLACE … VALUES (?,?,?,?,?),(…),…` with `rows` tuples. Full
/// batch chunks share one SQL string, so `prepare_cached` reuses the plan.
fn multi_row_insert_sql(rows: usize) -> String {
    debug_assert!(rows > 0);
    let mut sql = String::from(
        "INSERT OR REPLACE INTO vector_embedding (entity_id, dims, blob, model, created_us) VALUES ",
    );
    for i in 0..rows {
        if i > 0 {
            sql.push(',');
        }
        sql.push_str("(?,?,?,?,?)");
    }
    sql
}

fn serialize_embedding(emb: &[f32]) -> Vec<u8> {
    let header = BlobHeader {
        dims: emb.len() as u32,
    };
    let f32_bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(emb.as_ptr() as *const u8, emb.len() * 4) };
    let mut bytes = Vec::with_capacity(4 + f32_bytes.len());
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(f32_bytes);
    bytes
}

fn parse_embedding_blob(blob: &[u8]) -> Result<&[f32]> {
    let (header, rest) = BlobHeader::ref_from_prefix(blob)
        .map_err(|_| MCSError::MemoryError("Invalid blob header".into()))?;
    let count = header.dims as usize;
    let bytes = rest
        .get(..count * 4)
        .ok_or_else(|| MCSError::MemoryError("Blob data too short".into()))?;
    let emb = unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const f32, count) };
    Ok(emb)
}

impl VectorStore {
    /// Open a store with the default HNSW configuration for `dims`.
    pub fn new(db_path: &Path, dims: u32) -> Result<Self> {
        Self::with_config(db_path, &VectorConfig::new(dims))
    }

    /// Open a store with an explicit HNSW configuration.
    pub fn with_config(db_path: &Path, cfg: &VectorConfig) -> Result<Self> {
        let dims = cfg.dims;
        let conn = Connection::open(db_path).map_err(sqlite_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(sqlite_err)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;
             CREATE TABLE IF NOT EXISTS vector_embedding (
                 entity_id INTEGER PRIMARY KEY,
                 dims      INTEGER NOT NULL,
                 blob      BLOB    NOT NULL,
                 model     TEXT    NOT NULL DEFAULT '',
                 created_us INTEGER NOT NULL
             );",
        )
        .map_err(sqlite_err)?;

        let index = match cfg.index_kind {
            IndexKind::Hnsw => {
                let index_opts = IndexOptions {
                    dimensions: dims as usize,
                    metric: cfg.metric,
                    quantization: cfg.quantization,
                    connectivity: cfg.connectivity,
                    expansion_add: cfg.expansion_add,
                    expansion_search: cfg.expansion_search,
                    multi: false,
                };
                let index = Index::new(&index_opts)
                    .map_err(|e| MCSError::MemoryError(format!("usearch init: {e}")))?;
                // Reserve searcher threads up front so concurrent searches on
                // a store that never inserted (or hasn't grown yet) also work.
                index
                    .reserve_capacity_and_threads(1024, search_thread_cap())
                    .map_err(|e| MCSError::MemoryError(format!("usearch reserve: {e}")))?;
                AnnIndex::Hnsw {
                    index: Arc::new(index),
                    gate: SearchGate::new(search_thread_cap()),
                }
            }
            IndexKind::Ivf => AnnIndex::Ivf(Box::new(IvfFlatIndex::new(
                dims as usize,
                IvfMetric::from_usearch(cfg.metric),
                cfg.ivf_nlist,
                cfg.ivf_nprobe,
            ))),
            IndexKind::TurboQuant => {
                // The estimator's distortion guarantees scale as 1/d (weak at
                // low dimension) and the D×D projection grows quadratically
                // (dims pad to the next power of two: 1536 → 2048 → 16 MiB),
                // so the backend is gated to the embedding sizes it is
                // designed for.
                if !(384..=1536).contains(&dims) {
                    return Err(MCSError::MemoryError(format!(
                        "TurboQuant requires --embedding-dims between 384 and 1536, got {dims}"
                    )));
                }
                // Fixed seed: codes are rebuilt from the SQLite blobs on every
                // open, but a stable seed keeps searches reproducible across
                // restarts.
                AnnIndex::Turbo(Box::new(TurboQuantIndex::new(
                    dims as usize,
                    IvfMetric::from_usearch(cfg.metric),
                    cfg.tq_bits,
                    0x7042_9042_5045_5254, // "TURBOQUANT"-flavored constant
                )))
            }
        };

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
            index,
            db,
            dims,
            count: AtomicUsize::new(0),
            ivf_nprobe: cfg.ivf_nprobe,
            db_path: db_path.to_path_buf(),
            managed_snapshot: RwLock::new(None),
            taxonomy_snapshots: RwLock::new([None, None, None]),
        };
        store.load_existing()?;

        Ok(store)
    }

    fn load_existing(&self) -> Result<()> {
        let conn = self.db.lock();
        let count: usize = conn
            .query_row("SELECT COUNT(*) FROM vector_embedding", [], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(sqlite_err)? as usize;

        if count == 0 {
            return Ok(());
        }

        self.index.reserve(count)?;

        let mut stmt = conn
            .prepare("SELECT entity_id, dims, blob, model FROM vector_embedding")
            .map_err(sqlite_err)?;

        let rows = stmt
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let dims: i64 = row.get(1)?;
                let blob: Vec<u8> = row.get(2)?;
                let model: String = row.get(3)?;
                Ok((id, dims, blob, model))
            })
            .map_err(sqlite_err)?;

        for row in rows {
            let (id, _row_dims, blob, _model) = row.map_err(sqlite_err)?;
            let emb = parse_embedding_blob(&blob)?;
            self.index.add(id as u64, emb)?;
            self.count.fetch_add(1, Ordering::Relaxed);
        }

        // Train the IVF backend over the freshly-loaded set so a reopened,
        // populated database gets sub-linear search immediately (HNSW: no-op).
        self.index.train()?;

        self.load_names_from_entity_table(&conn)?;
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

    pub fn upsert_embedding(
        &self,
        entity_name: &str,
        embedding: &[f32],
        model: &str,
    ) -> Result<()> {
        let mut conn = self.db.lock();
        // Keep name resolution and the write in one snapshot while excluding a
        // concurrent rename between them, including writers in other processes.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_err)?;
        IndexProfileRegistry::new(&tx).ensure_legacy_writes("default")?;
        self.upsert_one(&tx, entity_name, embedding, model)?;
        tx.commit().map_err(sqlite_err)
    }

    /// Rows per multi-row `INSERT`: 5 bind variables each, so 500 rows use
    /// 2500 variables — comfortably under SQLite's 32766 cap — and a fixed
    /// size lets full chunks share one cached prepared statement.
    const BATCH_ROWS_PER_STMT: usize = 500;

    /// Insert or replace many embeddings under a single lock and a single
    /// SQLite transaction, written as multi-row `INSERT OR REPLACE … VALUES
    /// (…),(…),…` statements — one WAL commit and ~`n/500` statements instead
    /// of `n` of each. Per-item failures (unknown entity, wrong dims) land in
    /// that item's result slot and do not abort the rest of the batch. If a
    /// row-write or the final commit fails, every item that had succeeded is
    /// reported failed; the in-memory index may then be ahead of the DB until
    /// the next reopen (when it is rebuilt from the DB), matching the crash
    /// semantics of the single-item path under async durability.
    pub fn upsert_embeddings_batch(&self, items: &[(&str, Vec<f32>, &str)]) -> Vec<Result<()>> {
        use rusqlite::types::Value as SqlValue;

        let mut conn = self.db.lock();
        if let Err(error) = IndexProfileRegistry::new(&conn).ensure_legacy_writes("default") {
            return items
                .iter()
                .map(|_| Err(MCSError::InvalidParams(error.to_string())))
                .collect();
        }
        let tx = match conn.transaction() {
            Ok(tx) => tx,
            Err(e) => {
                let msg = format!("begin batch transaction: {e}");
                return items
                    .iter()
                    .map(|_| Err(MCSError::MemoryError(msg.clone())))
                    .collect();
            }
        };

        let now = now_micros();
        let mut results: Vec<Result<()>> = Vec::with_capacity(items.len());
        // (item index, bound row values) for items that passed validation +
        // index insertion; only these become INSERT rows.
        let mut ok_indices: Vec<usize> = Vec::with_capacity(items.len());
        let mut rows: Vec<[SqlValue; 5]> = Vec::with_capacity(items.len());
        for (i, (name, emb, model)) in items.iter().enumerate() {
            match self.resolve_and_index(&tx, name, emb) {
                Ok(entity_id) => {
                    ok_indices.push(i);
                    rows.push([
                        SqlValue::Integer(entity_id),
                        SqlValue::Integer(i64::from(self.dims)),
                        SqlValue::Blob(serialize_embedding(emb)),
                        SqlValue::Text((*model).to_string()),
                        SqlValue::Integer(now),
                    ]);
                    results.push(Ok(()));
                }
                Err(e) => results.push(Err(e)),
            }
        }

        let mut write_error: Option<String> = None;
        for chunk in rows.chunks(Self::BATCH_ROWS_PER_STMT) {
            let sql = multi_row_insert_sql(chunk.len());
            let outcome = tx.prepare_cached(&sql).and_then(|mut stmt| {
                stmt.execute(rusqlite::params_from_iter(
                    chunk.iter().flat_map(|row| row.iter()),
                ))
            });
            if let Err(e) = outcome {
                write_error = Some(format!("batch insert: {e}"));
                break;
            }
        }
        if write_error.is_none()
            && let Err(e) = tx.commit()
        {
            write_error = Some(format!("commit batch transaction: {e}"));
        }
        if let Some(msg) = write_error {
            for &i in &ok_indices {
                results[i] = Err(MCSError::MemoryError(msg.clone()));
            }
        }
        results
    }

    /// Validate one embedding, resolve its entity, and insert it into the ANN
    /// index (growing capacity in chunks) — everything an upsert does except
    /// the SQLite row write. Returns the entity id to write.
    fn resolve_and_index(
        &self,
        conn: &Connection,
        entity_name: &str,
        embedding: &[f32],
    ) -> Result<EntityId> {
        if embedding.len() != self.dims as usize {
            return Err(MCSError::InvalidParams(format!(
                "Embedding dimension mismatch: got {}, expected {}",
                embedding.len(),
                self.dims
            )));
        }

        let entity = self
            .get_entity_id_and_name(conn, entity_name)?
            .ok_or_else(|| {
                MCSError::InvalidParams(format!("Entity '{entity_name}' not found in KG"))
            })?;
        let entity_id = entity.0;

        // Grow the index capacity in chunks rather than one slot per upsert, so
        // a bulk load doesn't trigger a reallocation on every insert.
        let needed = self.count.load(Ordering::Relaxed).saturating_add(1);
        if needed > self.index.capacity() {
            const CHUNK: usize = 1024;
            let target = needed.div_ceil(CHUNK).saturating_mul(CHUNK);
            self.index.reserve(target)?;
        }
        let existed = self.index.remove(entity_id as u64).unwrap_or(false);
        self.index.add(entity_id as u64, embedding)?;

        if !existed {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        Ok(entity_id)
    }

    /// The single-embedding upsert core; `conn` belongs to the caller's write
    /// transaction so the resolved exact name cannot change before persistence.
    fn upsert_one(
        &self,
        conn: &Connection,
        entity_name: &str,
        embedding: &[f32],
        model: &str,
    ) -> Result<()> {
        let entity_id = self.resolve_and_index(conn, entity_name, embedding)?;
        let blob = serialize_embedding(embedding);
        let mut stmt = conn
            .prepare_cached(
                "INSERT OR REPLACE INTO vector_embedding (entity_id, dims, blob, model, created_us) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(sqlite_err)?;
        stmt.execute(params![entity_id, self.dims, blob, model, now_micros()])
            .map_err(sqlite_err)?;
        Ok(())
    }

    pub fn delete_embedding(&self, entity_name: &str) -> Result<bool> {
        let mut conn = self.db.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_err)?;
        IndexProfileRegistry::new(&tx).ensure_legacy_writes("default")?;
        let Some((entity_id, _)) = self.get_entity_id_and_name(&tx, entity_name)? else {
            return Ok(false);
        };
        let deleted = tx
            .execute(
                "DELETE FROM vector_embedding WHERE entity_id = ?1",
                params![entity_id],
            )
            .map_err(sqlite_err)?;
        if deleted == 0 {
            return Ok(false);
        }
        let indexed = self.index.remove(entity_id as u64)?;
        tx.commit().map_err(sqlite_err)?;
        self.forget_entity_name(entity_name);

        {
            let mut g = self.graph.write();
            if let Some(nx) = self.node_map.get(&entity_id) {
                g.remove_node(*nx);
                self.node_map.remove(&entity_id);
            }
        }

        if indexed {
            self.count.fetch_sub(1, Ordering::Relaxed);
        }
        Ok(true)
    }

    pub fn search_embeddings(&self, query: &[f32], top_k: usize) -> Result<Vec<(EntityId, f32)>> {
        if let Some(snapshot) = self.managed_snapshot.read().clone() {
            if query.len()
                != snapshot
                    .vectors
                    .first()
                    .map_or(query.len(), |(_, vector)| vector.len())
            {
                return Err(MCSError::InvalidParams(
                    "query dimensions do not match active index profile".into(),
                ));
            }
            let mut matches: Vec<_> = snapshot
                .vectors
                .iter()
                .map(|(id, vector)| (*id, managed_distance(snapshot.metric, query, vector)))
                .collect();
            matches.sort_by(|left, right| left.1.total_cmp(&right.1));
            matches.truncate(top_k.clamp(1, 100));
            return Ok(matches);
        }
        if self.count.load(Ordering::Relaxed) == 0 {
            return Ok(Vec::new());
        }
        let top_k = top_k.clamp(1, 100);
        let matches = self.index.search(query, top_k, Some(self.ivf_nprobe))?;
        Ok(matches
            .into_iter()
            .map(|(id, dist)| (id as EntityId, dist))
            .collect())
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
                "SELECT entity_id,blob FROM profile_vector WHERE profile_id=?1 ORDER BY entity_id",
            )
            .map_err(sqlite_err)?;
        let vectors = statement
            .query_map([profile_id.to_string()], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(sqlite_err)?
            .map(|row| {
                let (id, blob) = row.map_err(sqlite_err)?;
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
                Ok((id, vector))
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
            let mut statement = conn
                .prepare(
                    "SELECT subject_id,blob FROM taxonomy_vector WHERE profile_id=?1 AND subject_kind=?2 ORDER BY subject_id",
                )
                .map_err(sqlite_err)?;
            let vectors = statement
                .query_map(params![profile.to_string(), kind.as_i64()], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .map_err(sqlite_err)?
                .map(|row| {
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
                })
                .collect::<Result<Vec<_>>>()?;
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

    pub fn search_entities_json(
        &self,
        query: &[f32],
        top_k: usize,
        entity_type_filter: Option<&str>,
    ) -> Result<String> {
        let results = self.search_embeddings(query, top_k)?;
        if results.is_empty() {
            return Ok(r#"{"results":[],"count":0}"#.to_string());
        }

        let conn = self.db.lock();
        let mut out = String::with_capacity(128 + results.len() * 64);
        out.push_str(r#"{"results":["#);
        let mut first = true;
        let mut actual_count = 0usize;

        for &(id, dist) in &results {
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

    pub fn build_search_response_json(&self, results: &[(EntityId, f32)]) -> String {
        let mut out = String::with_capacity(128 + results.len() * 64);
        out.push_str(r#"{"results":["#);
        for (i, &(id, dist)) in results.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(r#"{"entityId":"#);
            out.push_str(&id.to_string());
            out.push_str(r#","distance":"#);
            write_f32(&mut out, dist);
            out.push('}');
        }
        out.push_str(r#"],"count":"#);
        out.push_str(&results.len().to_string());
        out.push('}');
        out
    }

    pub fn rebuild_graph_cache(&self) -> Result<()> {
        let conn = self.db.lock();

        let mut ent_stmt = conn
            .prepare("SELECT entity_id FROM vector_embedding")
            .map_err(sqlite_err)?;
        let ids: Vec<EntityId> = ent_stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .map_err(sqlite_err)?
            .filter_map(|r| r.ok())
            .collect();

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

    pub fn count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    pub const fn dims(&self) -> u32 {
        self.dims
    }

    /// The profile this store serves, or `None` while it stays in legacy
    /// compatibility. A caller that must know the model or the dimension of the
    /// managed index reads it here; the registry itself is private.
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
    /// A rebuild re-enqueues every live entity, and it takes the store out of
    /// legacy compatibility, after which a direct `vector_*` write is refused.
    /// The caller decides whether that is wanted; this method only reports what
    /// it did.
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

    /// Approximate resident RAM used by the ANN index, in bytes.
    pub fn index_memory_bytes(&self) -> usize {
        self.index.memory_bytes()
    }

    /// Breakdown of index RAM into (graph_bytes, vectors_bytes). IVF has no graph
    /// component, so its `graph_bytes` is 0.
    pub fn index_memory_breakdown(&self) -> (usize, usize) {
        self.index.memory_breakdown()
    }

    /// Current allocated capacity of the index (number of vectors it can hold
    /// before the next reservation).
    pub fn index_capacity(&self) -> usize {
        self.index.capacity()
    }

    /// The active ANN backend.
    pub const fn index_kind(&self) -> IndexKind {
        self.index.kind()
    }

    /// Rebuild the ANN index structure: retrains the IVF centroids over the
    /// current vectors (no-op for HNSW). Call after large batch ingestion to keep
    /// IVF recall high.
    pub fn reindex(&self) -> Result<()> {
        self.index.train()
    }

    /// Resolve a live entity id by exact current name in the KG table.
    pub fn entity_id_of(&self, name: &str) -> Result<Option<EntityId>> {
        let conn = self.db.lock();
        Ok(self.get_entity_id_and_name(&conn, name)?.map(|(id, _)| id))
    }

    /// Fetch the stored embedding for an entity id, if any.
    pub fn get_embedding_by_id(&self, id: EntityId) -> Result<Option<Vec<f32>>> {
        let conn = self.db.lock();
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT blob FROM vector_embedding WHERE entity_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .ok();
        match blob {
            Some(b) => Ok(Some(parse_embedding_blob(&b)?.to_vec())),
            None => Ok(None),
        }
    }

    /// Fetch `(entity_id, embedding, model)` for an entity by name.
    pub fn get_embedding_by_name(
        &self,
        name: &str,
    ) -> Result<Option<(EntityId, Vec<f32>, String)>> {
        let conn = self.db.lock();
        // Resolve the name and read its vector in a single SQLite snapshot.
        let row: Option<(EntityId, Vec<u8>, String)> = conn
            .prepare_cached(
                "SELECT e.id, v.blob, v.model FROM entity e
                 JOIN vector_embedding v ON v.entity_id = e.id
                 WHERE e.name_hash = ?1 AND e.name = ?2 AND e.flags = 0",
            )
            .map_err(sqlite_err)?
            .query_row(params![crate::kg::name_hash(name), name], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()
            .map_err(sqlite_err)?;
        match row {
            Some((id, blob, model)) => {
                self.cache_entity_name(id, name);
                Ok(Some((id, parse_embedding_blob(&blob)?.to_vec(), model)))
            }
            None => {
                self.forget_entity_name(name);
                Ok(None)
            }
        }
    }

    /// Resolve an entity id to its current `(name, entityType)` in the KG.
    pub fn resolve_name_type(&self, id: EntityId) -> (String, String) {
        let conn = self.db.lock();
        self.get_entity_name_type(&conn, id)
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// k-NN that returns resolved `(id, name, entityType, distance)`, optionally
    /// filtered by `entity_type` and excluding `exclude` ids. Over-fetches to
    /// compensate for filtered-out rows.
    pub fn search_resolved(
        &self,
        query: &[f32],
        top_k: usize,
        entity_type: Option<&str>,
        exclude: &std::collections::HashSet<EntityId>,
    ) -> Result<Vec<(EntityId, String, String, f32)>> {
        let fetch = (top_k.saturating_mul(3) + exclude.len()).clamp(top_k, 100);
        let raw = self.search_embeddings(query, fetch)?;
        let mut out = Vec::with_capacity(top_k);
        for (id, dist) in raw {
            if exclude.contains(&id) {
                continue;
            }
            let (name, etype) = self.resolve_name_type(id);
            if name.is_empty() {
                continue;
            }
            if let Some(ft) = entity_type
                && etype != ft
            {
                continue;
            }
            out.push((id, name, etype, dist));
            if out.len() >= top_k {
                break;
            }
        }
        Ok(out)
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

    fn setup_ivf(dims: u32) -> TestEnv {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let lru = NonZeroUsize::new(10000).unwrap();
        let kg =
            GraphHandle::new(&db_path, Durability::Async, SqliteTuning::default(), lru, 4).unwrap();
        let mut cfg = VectorConfig::new(dims);
        cfg.index_kind = IndexKind::Ivf;
        cfg.ivf_nlist = 4;
        cfg.ivf_nprobe = 4;
        let vs = VectorStore::with_config(&db_path, &cfg).unwrap();
        TestEnv { kg, vs, _dir: dir }
    }

    fn setup_turbo(dims: u32, bits: u32) -> TestEnv {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let lru = NonZeroUsize::new(10000).unwrap();
        let kg =
            GraphHandle::new(&db_path, Durability::Async, SqliteTuning::default(), lru, 4).unwrap();
        let mut cfg = VectorConfig::new(dims);
        cfg.index_kind = IndexKind::TurboQuant;
        cfg.tq_bits = bits;
        let vs = VectorStore::with_config(&db_path, &cfg).unwrap();
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
        seed_taxonomy_vector(&env, profile, 2, 3, 1, &make_embedding(4, 5.0));
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

    fn renamed_vector_fixture() -> (TestEnv, EntityId) {
        use mcpmem_core::mutation::{MutationContext, MutationRequest, MutationService};

        let env = setup(4);
        create_test_entity(&env.kg, "before", "person");
        env.vs
            .upsert_embedding("before", &[1.0, 0.0, 0.0, 0.0], "original")
            .unwrap();
        let id = env.vs.entity_id_of("before").unwrap().unwrap();
        assert_eq!(*env.vs.name_to_id.get("before").unwrap(), id);
        assert_eq!(env.vs.id_to_name.get(&id).unwrap().as_str(), "before");

        // An independently opened writer uses the same service as inbound HTTP,
        // without reaching the MCP dispatcher or this VectorStore's caches.
        let writer = GraphHandle::new(
            &env.vs.db_path,
            Durability::Sync,
            SqliteTuning::default(),
            NonZeroUsize::new(32).unwrap(),
            2,
        )
        .unwrap();
        MutationService::new(&writer)
            .apply(
                MutationRequest::RenameEntity {
                    old_name: "before".into(),
                    new_name: "after".into(),
                },
                MutationContext::local(),
            )
            .unwrap();
        assert!(writer.get_entity("before").unwrap().is_none());
        assert_eq!(writer.get_entity("after").unwrap().unwrap().name, "after");
        (env, id)
    }

    #[test]
    fn vector_names_reject_stale_alias_after_external_rename() {
        // Exercise each operation with independently warmed stale caches: a
        // successful read must not be necessary to make subsequent writes safe.
        let mut outcomes = Vec::new();
        for operation in ["get", "upsert", "batch", "delete"] {
            let (env, id) = renamed_vector_fixture();
            let rejected = match operation {
                "get" => env.vs.get_embedding_by_name("before").unwrap().is_none(),
                "upsert" => env
                    .vs
                    .upsert_embedding("before", &[0.0, 1.0, 0.0, 0.0], "wrong")
                    .is_err(),
                "batch" => {
                    env.vs
                        .upsert_embeddings_batch(&[("before", vec![0.0, 1.0, 0.0, 0.0], "wrong")])
                        [0]
                    .is_err()
                }
                "delete" => !env.vs.delete_embedding("before").unwrap(),
                _ => unreachable!(),
            };
            let absent = env.vs.entity_id_of("before").unwrap().is_none();
            let preserved = env.vs.get_embedding_by_name("after").unwrap()
                == Some((id, vec![1.0, 0.0, 0.0, 0.0], "original".into()));
            outcomes.push((operation, rejected, absent, preserved, env.vs.count()));
        }
        assert_eq!(
            outcomes,
            [
                ("get", true, true, true, 1),
                ("upsert", true, true, true, 1),
                ("batch", true, true, true, 1),
                ("delete", true, true, true, 1),
            ]
        );
    }

    #[test]
    fn vector_recreated_name_is_isolated_after_external_rename() {
        let (env, renamed_id) = renamed_vector_fixture();
        create_test_entity(&env.kg, "before", "project");
        let new_id = env.vs.entity_id_of("before").unwrap().unwrap();
        assert_ne!(new_id, renamed_id);
        assert!(env.vs.get_embedding_by_name("before").unwrap().is_none());
        assert!(!env.vs.delete_embedding("before").unwrap());
        assert_eq!(env.vs.count(), 1);
        env.vs
            .upsert_embedding("before", &[0.0, 1.0, 0.0, 0.0], "new")
            .unwrap();
        assert_eq!(
            env.vs.get_embedding_by_name("before").unwrap(),
            Some((new_id, vec![0.0, 1.0, 0.0, 0.0], "new".into()))
        );
        assert!(env.vs.delete_embedding("before").unwrap());
        assert!(!env.vs.delete_embedding("before").unwrap());
        assert_eq!(
            env.vs.get_embedding_by_name("after").unwrap(),
            Some((renamed_id, vec![1.0, 0.0, 0.0, 0.0], "original".into()))
        );
        assert_eq!(env.vs.count(), 1);
    }

    #[test]
    fn vector_resolved_search_uses_current_name_after_external_rename() {
        let mut names = Vec::new();
        for operation in ["resolve", "search", "json", "hybrid"] {
            let (env, id) = renamed_vector_fixture();
            let query = [1.0, 0.0, 0.0, 0.0];
            let name = match operation {
                "resolve" => env.vs.resolve_name_type(id).0,
                "search" => {
                    let hits = env
                        .vs
                        .search_resolved(&query, 1, None, &std::collections::HashSet::new())
                        .unwrap();
                    assert_eq!(hits.len(), 1);
                    assert_eq!(hits[0].0, id);
                    hits[0].1.clone()
                }
                "json" => {
                    let response = env.vs.search_entities_json(&query, 1, None).unwrap();
                    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
                    assert_eq!(parsed["count"], 1);
                    parsed["results"][0]["name"].as_str().unwrap().to_owned()
                }
                "hybrid" => {
                    let response = crate::vector_actions::handle_hybrid_search(
                        &env.vs,
                        &env.kg,
                        Some(&serde_json::json!({
                            "queryText": "nonexistenttext", "queryEmbedding": query, "topK": 1
                        })),
                    )
                    .unwrap();
                    let outer: serde_json::Value = serde_json::from_str(&response).unwrap();
                    let parsed: serde_json::Value =
                        serde_json::from_str(outer["content"][0]["text"].as_str().unwrap())
                            .unwrap();
                    assert_eq!(parsed["count"], 1);
                    parsed["results"][0]["name"].as_str().unwrap().to_owned()
                }
                _ => unreachable!(),
            };
            names.push((operation, name));
        }
        assert_eq!(
            names,
            [
                ("resolve", "after".into()),
                ("search", "after".into()),
                ("json", "after".into()),
                ("hybrid", "after".into()),
            ]
        );
    }

    #[test]
    fn test_vector_upsert_and_search() {
        let env = setup(4);
        create_test_entity(&env.kg, "alice", "person");
        create_test_entity(&env.kg, "bob", "person");

        let emb_a = vec![1.0, 0.0, 0.0, 0.0];
        let emb_b = vec![0.0, 1.0, 0.0, 0.0];
        env.vs
            .upsert_embedding("alice", &emb_a, "test-model")
            .unwrap();
        env.vs
            .upsert_embedding("bob", &emb_b, "test-model")
            .unwrap();

        let query = vec![1.0, 0.0, 0.0, 0.0];
        let results = env.vs.search_embeddings(&query, 10).unwrap();
        assert_eq!(results.len(), 2);
        let top_name = env
            .vs
            .id_to_name
            .get(&results[0].0)
            .map(|r| r.value().clone());
        assert_eq!(top_name.as_deref(), Some("alice"));
        assert!(results[0].1 < results[1].1);
    }

    #[test]
    fn test_vector_delete_embedding() {
        let env = setup(4);
        create_test_entity(&env.kg, "alice", "person");
        env.vs
            .upsert_embedding("alice", &make_embedding(4, 1.0), "")
            .unwrap();
        assert_eq!(env.vs.count(), 1);

        let deleted = env.vs.delete_embedding("alice").unwrap();
        assert!(deleted);
        assert_eq!(env.vs.count(), 0);

        let results = env
            .vs
            .search_embeddings(&make_embedding(4, 1.0), 10)
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn direct_writes_are_rejected_while_a_profile_rebuilds() {
        use mcpmem_core::jobs::{
            DistanceMetric, IndexProfile, IndexProfileRegistry, Normalization,
        };
        use uuid::Uuid;

        let env = setup(4);
        create_test_entity(&env.kg, "alice", "person");
        let profile = IndexProfile {
            id: Uuid::new_v4(),
            store_key: "default".into(),
            provider_kind: "test".into(),
            model: "test".into(),
            dimensions: 4,
            representation_version: "v1".into(),
            normalization: Normalization::None,
            distance_metric: DistanceMetric::Cosine,
            vector_encoding_version: "f32le-v1".into(),
        };
        let conn = rusqlite::Connection::open(&env.vs.db_path).unwrap();
        IndexProfileRegistry::new(&conn)
            .begin_rebuild(&profile)
            .unwrap();
        let error = env
            .vs
            .upsert_embedding("alice", &make_embedding(4, 1.0), "test")
            .unwrap_err();
        assert!(error.to_string().contains("direct_vector_writes_disabled"));
    }

    #[test]
    fn test_vector_upsert_nonexistent_entity() {
        let env = setup(4);
        let err = env
            .vs
            .upsert_embedding("nonexistent", &make_embedding(4, 1.0), "");
        assert!(err.is_err());
    }

    #[test]
    fn test_vector_dimension_mismatch() {
        let env = setup(4);
        create_test_entity(&env.kg, "alice", "person");
        let err = env
            .vs
            .upsert_embedding("alice", &make_embedding(8, 1.0), "");
        assert!(err.is_err());
    }

    #[test]
    fn test_vector_search_top_k() {
        let env = setup(4);
        for i in 0..5 {
            create_test_entity(&env.kg, &format!("e{i}"), "test");
            env.vs
                .upsert_embedding(&format!("e{i}"), &make_embedding(4, i as f32 * 0.2), "")
                .unwrap();
        }
        let results = env
            .vs
            .search_embeddings(&make_embedding(4, 0.0), 3)
            .unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_vector_search_type_filter() {
        let env = setup(4);
        create_test_entity(&env.kg, "alice", "person");
        create_test_entity(&env.kg, "acme", "organization");
        env.vs
            .upsert_embedding("alice", &make_embedding(4, 1.0), "")
            .unwrap();
        env.vs
            .upsert_embedding("acme", &make_embedding(4, 0.95), "")
            .unwrap();

        let json = env
            .vs
            .search_entities_json(&make_embedding(4, 1.0), 10, Some("person"))
            .unwrap();
        assert!(json.contains("alice"));
        assert!(!json.contains("acme"));
    }

    #[test]
    fn test_vector_blob_roundtrip() {
        let emb: Vec<f32> = vec![1.0, 2.5, -3.0, 0.0];
        let blob = serialize_embedding(&emb);
        let parsed = parse_embedding_blob(&blob).unwrap();
        assert_eq!(parsed.len(), emb.len());
        for (a, b) in parsed.iter().zip(emb.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
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

        env.vs
            .upsert_embedding("alice", &make_embedding(4, 1.0), "")
            .unwrap();
        env.vs
            .upsert_embedding("bob", &make_embedding(4, 0.5), "")
            .unwrap();
        env.vs
            .upsert_embedding("charlie", &make_embedding(4, 0.0), "")
            .unwrap();

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

    #[test]
    fn test_vector_upsert_replace() {
        let env = setup(4);
        create_test_entity(&env.kg, "alice", "person");
        env.vs
            .upsert_embedding("alice", &make_embedding(4, 1.0), "")
            .unwrap();
        env.vs
            .upsert_embedding("alice", &make_embedding(4, 0.5), "")
            .unwrap();
        assert_eq!(env.vs.count(), 1);

        let results = env
            .vs
            .search_embeddings(&make_embedding(4, 0.5), 10)
            .unwrap();
        assert_eq!(results.len(), 1);
        let name = env
            .vs
            .id_to_name
            .get(&results[0].0)
            .map(|r| r.value().clone());
        assert_eq!(name.as_deref(), Some("alice"));
    }

    #[test]
    fn test_vector_index_capacity_grows_in_chunks() {
        let env = setup(4);
        // Empty store reserves nothing.
        assert_eq!(env.vs.count(), 0);

        // First insert reserves at least a full 1024 chunk up front (usearch may
        // over-allocate beyond that, but it must not reserve just one slot).
        create_test_entity(&env.kg, "e0", "t");
        env.vs
            .upsert_embedding("e0", &make_embedding(4, 0.0), "")
            .unwrap();
        let cap_after_first = env.vs.index_capacity();
        assert!(cap_after_first >= 1024, "capacity {cap_after_first} < 1024");

        // Inserts within the same chunk do not reallocate.
        for i in 1..50 {
            let name = format!("e{i}");
            create_test_entity(&env.kg, &name, "t");
            env.vs
                .upsert_embedding(&name, &make_embedding(4, i as f32 * 0.01), "")
                .unwrap();
        }
        assert_eq!(env.vs.count(), 50);
        assert_eq!(
            env.vs.index_capacity(),
            cap_after_first,
            "capacity changed mid-chunk"
        );

        // Overwriting an existing entity never grows capacity.
        env.vs
            .upsert_embedding("e0", &make_embedding(4, 0.5), "")
            .unwrap();
        assert_eq!(env.vs.count(), 50);
        assert_eq!(env.vs.index_capacity(), cap_after_first);

        // Memory accounting is exposed and non-zero once vectors are present.
        assert!(env.vs.index_memory_bytes() > 0);
        let (graph_bytes, vec_bytes) = env.vs.index_memory_breakdown();
        assert!(graph_bytes + vec_bytes > 0);
    }

    #[test]
    fn test_vector_empty_store_search() {
        let env = setup(4);
        let json = env
            .vs
            .search_entities_json(&make_embedding(4, 1.0), 10, None)
            .unwrap();
        assert_eq!(json, r#"{"results":[],"count":0}"#);
    }

    #[test]
    fn test_vector_persistence_across_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("persist.db");
        let lru = NonZeroUsize::new(10000).unwrap();

        let kg =
            GraphHandle::new(&db_path, Durability::Async, SqliteTuning::default(), lru, 4).unwrap();
        kg.create_entities(&[Entity {
            name: "alice".into(),
            entity_type: "person".into(),
            observations: vec![],
        }])
        .unwrap();

        let vs1 = VectorStore::new(&db_path, 4).unwrap();
        vs1.upsert_embedding("alice", &make_embedding(4, 1.0), "")
            .unwrap();
        assert_eq!(vs1.count(), 1);
        drop(vs1);
        drop(kg);

        let kg2 =
            GraphHandle::new(&db_path, Durability::Async, SqliteTuning::default(), lru, 4).unwrap();
        let vs2 = VectorStore::new(&db_path, 4).unwrap();
        assert_eq!(vs2.count(), 1);

        let results = vs2.search_embeddings(&make_embedding(4, 1.0), 10).unwrap();
        assert_eq!(results.len(), 1);
        drop(vs2);
        drop(kg2);
    }

    #[test]
    fn test_vector_search_json_format() {
        let env = setup(4);
        create_test_entity(&env.kg, "alice", "person");
        env.vs
            .upsert_embedding("alice", &make_embedding(4, 1.0), "")
            .unwrap();

        let json = env
            .vs
            .search_entities_json(&make_embedding(4, 1.0), 10, None)
            .unwrap();
        assert!(json.contains("alice"));
        assert!(json.contains("person"));
        assert!(json.contains("score"));
        assert!(json.contains("count"));
    }

    #[test]
    fn test_vector_concurrent_upsert() {
        let env = setup(8);
        let vs = Arc::new(env.vs);

        let mut threads = Vec::new();
        for i in 0..4 {
            let vs = Arc::clone(&vs);
            threads.push(std::thread::spawn(move || {
                let name = format!("thread_{i}");
                // entity creation happens through GraphHandle - shared
                vs.upsert_embedding(&name, &make_embedding(8, i as f32 * 0.25), "")
                    .ok();
            }));
        }

        create_test_entity(&env.kg, "thread_0", "t");
        create_test_entity(&env.kg, "thread_1", "t");
        create_test_entity(&env.kg, "thread_2", "t");
        create_test_entity(&env.kg, "thread_3", "t");

        for t in threads {
            t.join().unwrap();
        }
    }

    // ── IVF backend (via VectorStore) ─────────────────────────────────────

    #[test]
    fn test_ivf_store_upsert_search_delete() {
        let env = setup_ivf(4);
        assert_eq!(env.vs.index_kind(), IndexKind::Ivf);
        create_test_entity(&env.kg, "alice", "person");
        create_test_entity(&env.kg, "bob", "person");
        env.vs
            .upsert_embedding("alice", &make_embedding(4, 1.0), "m")
            .unwrap();
        env.vs
            .upsert_embedding("bob", &make_embedding(4, 0.1), "m")
            .unwrap();
        assert_eq!(env.vs.count(), 2);

        let results = env
            .vs
            .search_embeddings(&make_embedding(4, 1.0), 10)
            .unwrap();
        assert_eq!(results.len(), 2);
        // alice (all 1.0) is the closest match to the all-ones query.
        let top_name = env
            .vs
            .id_to_name
            .get(&results[0].0)
            .map(|r| r.value().clone());
        assert_eq!(top_name.as_deref(), Some("alice"));

        assert!(env.vs.delete_embedding("alice").unwrap());
        assert_eq!(env.vs.count(), 1);
        let results = env
            .vs
            .search_embeddings(&make_embedding(4, 1.0), 10)
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_ivf_persistence_and_reindex() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("ivf.db");
        let lru = NonZeroUsize::new(10000).unwrap();
        let kg =
            GraphHandle::new(&db_path, Durability::Async, SqliteTuning::default(), lru, 4).unwrap();
        let mut cfg = VectorConfig::new(4);
        cfg.index_kind = IndexKind::Ivf;
        cfg.ivf_nlist = 3;
        cfg.ivf_nprobe = 3;

        {
            let vs = VectorStore::with_config(&db_path, &cfg).unwrap();
            for i in 0..12 {
                let name = format!("e{i}");
                create_test_entity(&kg, &name, "t");
                vs.upsert_embedding(&name, &make_embedding(4, i as f32 * 0.1), "")
                    .unwrap();
            }
            vs.reindex().unwrap();
            assert_eq!(vs.count(), 12);
        }

        // Reopen: embeddings reload and the IVF index retrains on load.
        let vs2 = VectorStore::with_config(&db_path, &cfg).unwrap();
        assert_eq!(vs2.count(), 12);
        let results = vs2.search_embeddings(&make_embedding(4, 0.0), 3).unwrap();
        assert!(!results.is_empty());
        // The exact match (e0 == all-zeros) should be the nearest.
        let top = vs2.id_to_name.get(&results[0].0).map(|r| r.value().clone());
        assert_eq!(top.as_deref(), Some("e0"));
    }

    // ── TurboQuant backend (via VectorStore) ──────────────────────────────

    #[test]
    fn test_turbo_store_upsert_search_delete() {
        let env = setup_turbo(384, 4);
        assert_eq!(env.vs.index_kind(), IndexKind::TurboQuant);
        create_test_entity(&env.kg, "alice", "person");
        create_test_entity(&env.kg, "bob", "person");
        // Distinct directions (the cosine metric ignores pure magnitude, so
        // the constant `make_embedding` vectors would tie).
        let mut emb_a = make_embedding(384, 0.1);
        emb_a[0] = 1.0;
        let mut emb_b = make_embedding(384, 0.1);
        emb_b[383] = 1.0;
        env.vs.upsert_embedding("alice", &emb_a, "m").unwrap();
        env.vs.upsert_embedding("bob", &emb_b, "m").unwrap();
        assert_eq!(env.vs.count(), 2);

        let results = env.vs.search_embeddings(&emb_a, 10).unwrap();
        assert_eq!(results.len(), 2);
        let top_name = env
            .vs
            .id_to_name
            .get(&results[0].0)
            .map(|r| r.value().clone());
        assert_eq!(top_name.as_deref(), Some("alice"));
        assert!(results[0].1 < results[1].1);

        assert!(env.vs.delete_embedding("alice").unwrap());
        assert_eq!(env.vs.count(), 1);
        let results = env.vs.search_embeddings(&emb_a, 10).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_turbo_store_replace_and_memory_accounting() {
        let env = setup_turbo(384, 2);
        create_test_entity(&env.kg, "alice", "person");
        env.vs
            .upsert_embedding("alice", &make_embedding(384, 1.0), "")
            .unwrap();
        env.vs
            .upsert_embedding("alice", &make_embedding(384, 0.5), "")
            .unwrap();
        assert_eq!(env.vs.count(), 1);
        // Codes-only backend: memory is reported, no graph component.
        assert!(env.vs.index_memory_bytes() > 0);
        let (graph_bytes, vec_bytes) = env.vs.index_memory_breakdown();
        assert_eq!(graph_bytes, 0);
        assert!(vec_bytes > 0);
        // reindex is a no-op for the data-oblivious backend.
        env.vs.reindex().unwrap();
        assert_eq!(env.vs.count(), 1);
    }

    #[test]
    fn test_hnsw_concurrent_searches_all_succeed() {
        // Regression: reserve used threads=1, so any two concurrent searches
        // made usearch fail with "Reserve capacity ahead of searches!". All
        // concurrent searches must now succeed with correct top-1 results.
        use std::sync::Arc as StdArc;
        let env = setup(8);
        let n = 64;
        for i in 0..n {
            create_test_entity(&env.kg, &format!("c{i}"), "t");
            let mut e = make_embedding(8, 0.05);
            e[i % 8] = 1.0 + (i / 8) as f32 * 0.1;
            env.vs.upsert_embedding(&format!("c{i}"), &e, "m").unwrap();
        }
        let vs = StdArc::new(env.vs);
        let mut handles = Vec::new();
        for t in 0..32u64 {
            let vs = StdArc::clone(&vs);
            handles.push(std::thread::spawn(move || {
                for i in 0..20usize {
                    let idx = ((t as usize) * 20 + i) % 64;
                    let mut q = make_embedding(8, 0.05);
                    q[idx % 8] = 1.0 + (idx / 8) as f32 * 0.1;
                    let r = vs
                        .search_embeddings(&q, 1)
                        .expect("concurrent search must not fail");
                    assert!(!r.is_empty(), "search returned nothing");
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_batch_upsert_single_transaction() {
        let env = setup(8);
        for i in 0..5 {
            create_test_entity(&env.kg, &format!("b{i}"), "t");
        }
        // Mixed batch: 5 valid, one unknown entity, one wrong dims.
        let mut items: Vec<(&str, Vec<f32>, &str)> = Vec::new();
        let names: Vec<String> = (0..5).map(|i| format!("b{i}")).collect();
        for name in &names {
            items.push((name.as_str(), make_embedding(8, 0.5), "m"));
        }
        items.push(("missing-entity", make_embedding(8, 0.5), "m"));
        items.push(("b0", make_embedding(4, 0.5), "m"));

        let results = env.vs.upsert_embeddings_batch(&items);
        assert_eq!(results.len(), 7);
        assert!(
            results[..5].iter().all(Result::is_ok),
            "valid items must succeed"
        );
        assert!(results[5].is_err(), "unknown entity must fail its slot");
        assert!(results[6].is_err(), "dim mismatch must fail its slot");
        assert_eq!(env.vs.count(), 5, "count reflects only successful items");

        // Batch replaces existing rows without inflating the count.
        let again: Vec<(&str, Vec<f32>, &str)> = names
            .iter()
            .map(|n| (n.as_str(), make_embedding(8, 0.9), "m2"))
            .collect();
        assert!(
            env.vs
                .upsert_embeddings_batch(&again)
                .iter()
                .all(Result::is_ok)
        );
        assert_eq!(env.vs.count(), 5);

        let results = env
            .vs
            .search_embeddings(&make_embedding(8, 0.9), 10)
            .unwrap();
        assert_eq!(results.len(), 5, "all batch rows must be searchable");
    }

    #[test]
    fn test_batch_upsert_crosses_multi_row_statement_chunks() {
        // > BATCH_ROWS_PER_STMT rows: two full multi-row INSERTs plus a
        // remainder, all in one transaction; every row must land and reload.
        let n = VectorStore::BATCH_ROWS_PER_STMT * 2 + 7;
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("chunks.db");
        let lru = NonZeroUsize::new(10000).unwrap();
        let kg =
            GraphHandle::new(&db_path, Durability::Async, SqliteTuning::default(), lru, 4).unwrap();
        let cfg = VectorConfig::new(8);
        let names: Vec<String> = (0..n).map(|i| format!("m{i}")).collect();
        {
            let vs = VectorStore::with_config(&db_path, &cfg).unwrap();
            let entities: Vec<Entity> = names
                .iter()
                .map(|name| Entity {
                    name: name.clone(),
                    entity_type: "t".into(),
                    observations: vec![],
                })
                .collect();
            kg.create_entities(&entities).unwrap();
            let items: Vec<(&str, Vec<f32>, &str)> = names
                .iter()
                .map(|nm| (nm.as_str(), make_embedding(8, 0.5), "m"))
                .collect();
            let results = vs.upsert_embeddings_batch(&items);
            assert_eq!(results.len(), n);
            assert!(results.iter().all(Result::is_ok), "all rows must succeed");
            assert_eq!(vs.count(), n);
        }
        let vs2 = VectorStore::with_config(&db_path, &cfg).unwrap();
        assert_eq!(vs2.count(), n, "all chunked rows must be durably committed");
    }

    #[test]
    fn test_batch_upsert_persists_across_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("batch.db");
        let lru = NonZeroUsize::new(10000).unwrap();
        let kg =
            GraphHandle::new(&db_path, Durability::Async, SqliteTuning::default(), lru, 4).unwrap();
        let cfg = VectorConfig::new(8);
        {
            let vs = VectorStore::with_config(&db_path, &cfg).unwrap();
            for i in 0..20 {
                create_test_entity(&kg, &format!("p{i}"), "t");
            }
            let names: Vec<String> = (0..20).map(|i| format!("p{i}")).collect();
            let items: Vec<(&str, Vec<f32>, &str)> = names
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    let mut e = make_embedding(8, 0.1);
                    e[i % 8] = 1.0;
                    (n.as_str(), e, "m")
                })
                .collect();
            assert!(vs.upsert_embeddings_batch(&items).iter().all(Result::is_ok));
            assert_eq!(vs.count(), 20);
        }
        // The transaction must be durably committed: everything reloads.
        let vs2 = VectorStore::with_config(&db_path, &cfg).unwrap();
        assert_eq!(vs2.count(), 20, "batch rows must survive reopen");
    }

    #[test]
    fn test_batch_upsert_empty_is_noop() {
        let env = setup(4);
        assert!(env.vs.upsert_embeddings_batch(&[]).is_empty());
        assert_eq!(env.vs.count(), 0);
    }

    #[test]
    fn test_turbo_rejects_out_of_range_dims() {
        let dir = tempfile::TempDir::new().unwrap();
        for dims in [8u32, 383, 1537, 4096] {
            let mut cfg = VectorConfig::new(dims);
            cfg.index_kind = IndexKind::TurboQuant;
            let err = match VectorStore::with_config(&dir.path().join("t.db"), &cfg) {
                Err(e) => e,
                Ok(_) => panic!("dims {dims} outside 384..=1536 must be rejected"),
            };
            assert!(
                err.to_string().contains("384"),
                "error should state the valid range: {err}"
            );
        }
        // Boundary values are accepted.
        for dims in [384u32, 1536] {
            let mut cfg = VectorConfig::new(dims);
            cfg.index_kind = IndexKind::TurboQuant;
            let db = dir.path().join(format!("ok{dims}.db"));
            VectorStore::with_config(&db, &cfg).unwrap();
        }
    }

    #[test]
    fn test_turbo_persistence_across_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("turbo.db");
        let lru = NonZeroUsize::new(10000).unwrap();
        let kg =
            GraphHandle::new(&db_path, Durability::Async, SqliteTuning::default(), lru, 4).unwrap();
        let mut cfg = VectorConfig::new(384);
        cfg.index_kind = IndexKind::TurboQuant;
        cfg.tq_bits = 4;

        {
            let vs = VectorStore::with_config(&db_path, &cfg).unwrap();
            for i in 0..10 {
                let name = format!("e{i}");
                create_test_entity(&kg, &name, "t");
                let mut emb = make_embedding(384, 0.1);
                emb[i % 384] = 1.0 + i as f32 * 0.1;
                vs.upsert_embedding(&name, &emb, "").unwrap();
            }
            assert_eq!(vs.count(), 10);
        }

        // Reopen: embeddings reload from SQLite and are re-encoded with the
        // fixed seed, so search behaves identically.
        let vs2 = VectorStore::with_config(&db_path, &cfg).unwrap();
        assert_eq!(vs2.count(), 10);
        let mut q = make_embedding(384, 0.1);
        q[3] = 1.3;
        let results = vs2.search_embeddings(&q, 3).unwrap();
        assert!(!results.is_empty());
        let top = vs2.id_to_name.get(&results[0].0).map(|r| r.value().clone());
        assert_eq!(top.as_deref(), Some("e3"));
    }

    // ── New retrieval helpers (backend-agnostic) ──────────────────────────

    #[test]
    fn test_get_embedding_helpers() {
        let env = setup(4);
        create_test_entity(&env.kg, "alice", "person");
        let emb = vec![0.1, 0.2, 0.3, 0.4];
        env.vs.upsert_embedding("alice", &emb, "model-x").unwrap();

        let id = env.vs.entity_id_of("alice").unwrap().unwrap();
        let by_id = env.vs.get_embedding_by_id(id).unwrap().unwrap();
        assert_eq!(by_id, emb);

        let (got_id, got_emb, model) = env.vs.get_embedding_by_name("alice").unwrap().unwrap();
        assert_eq!(got_id, id);
        assert_eq!(got_emb, emb);
        assert_eq!(model, "model-x");

        assert!(env.vs.get_embedding_by_name("nobody").unwrap().is_none());
    }

    #[test]
    fn test_search_resolved_excludes_and_filters() {
        let env = setup(4);
        create_test_entity(&env.kg, "a", "doc");
        create_test_entity(&env.kg, "b", "doc");
        create_test_entity(&env.kg, "c", "note");
        env.vs
            .upsert_embedding("a", &make_embedding(4, 1.0), "")
            .unwrap();
        env.vs
            .upsert_embedding("b", &make_embedding(4, 0.9), "")
            .unwrap();
        env.vs
            .upsert_embedding("c", &make_embedding(4, 0.95), "")
            .unwrap();

        let id_a = env.vs.entity_id_of("a").unwrap().unwrap();
        let mut exclude = std::collections::HashSet::new();
        exclude.insert(id_a);

        // Exclude "a"; without a type filter we expect b and c.
        let rows = env
            .vs
            .search_resolved(&make_embedding(4, 1.0), 10, None, &exclude)
            .unwrap();
        let names: Vec<&str> = rows.iter().map(|(_, n, _, _)| n.as_str()).collect();
        assert!(!names.contains(&"a"));
        assert!(names.contains(&"b") && names.contains(&"c"));

        // Now filter to type "doc": only "b" remains (a excluded, c is a note).
        let rows = env
            .vs
            .search_resolved(&make_embedding(4, 1.0), 10, Some("doc"), &exclude)
            .unwrap();
        let names: Vec<&str> = rows.iter().map(|(_, n, _, _)| n.as_str()).collect();
        assert_eq!(names, vec!["b"]);
    }
}
