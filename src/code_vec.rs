//! Self-contained ANN store for code-symbol embeddings (`code_embed` /
//! `code_semantic_search`).
//!
//! The main [`VectorStore`](crate::vector_store::VectorStore) serves the
//! durable chunk snapshot only: the client `vector_*` ingestion tools and the
//! legacy `vector_embedding` table are gone from it. The code subsystem is a
//! separate product surface: it accepts client-supplied symbol embeddings and
//! keeps its own usearch HNSW index keyed by symbol entity id, persisted to a
//! dedicated `code_vector` table in each per-project database (the same blob
//! header format as 1.x).
//!
//! 1.x code embeddings do **not** survive an upgrade: migration 0009 drops
//! the legacy `vector_embedding` table, and per-project code databases run
//! the full migration set on open. The first post-upgrade open erases every
//! persisted code embedding; clients re-ingest with `code_embed`.

#![cfg(feature = "code")]

use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::errors::{MCSError, Result};

type EntityId = i64;

/// Number of concurrent searcher threads reserved inside the usearch HNSW
/// index. usearch hard-fails a search ("Reserve capacity ahead of searches!")
/// when more threads query than were reserved, so [`SearchGate`] caps
/// concurrent searches at exactly this number — correctness never depends on
/// how many transport threads (stdio pipeline, HTTP handlers) pile in.
fn search_thread_cap() -> usize {
    static CAP: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .saturating_mul(2)
            .clamp(8, 64)
    });
    *CAP
}

/// Counting gate bounding concurrent HNSW searches to the reserved thread
/// capacity. Cheap: uncontended acquire is one mutex lock.
struct SearchGate {
    permits: parking_lot::Mutex<usize>,
    cv: parking_lot::Condvar,
}

impl SearchGate {
    const fn new(n: usize) -> Self {
        Self {
            permits: parking_lot::Mutex::new(n),
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

fn sqlite_err(e: rusqlite::Error) -> MCSError {
    MCSError::IoError(std::io::Error::other(e))
}

#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct BlobHeader {
    dims: u32,
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
    Ok(unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const f32, count) })
}

/// One per-project HNSW index over code-symbol entity ids, with the rows it
/// indexes persisted in the project database's `code_vector` table.
pub struct CodeVecIndex {
    index: Arc<Index>,
    dims: u32,
    gate: SearchGate,
    pub(crate) db: Mutex<Connection>,
}

impl CodeVecIndex {
    /// Open (and lazily create) the per-project ANN store for `dims`.
    pub fn open(db_path: &Path, dims: u32) -> Result<Arc<Self>> {
        let conn = Connection::open(db_path).map_err(sqlite_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(sqlite_err)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;
             CREATE TABLE IF NOT EXISTS code_vector (
                 entity_id INTEGER PRIMARY KEY,
                 dims      INTEGER NOT NULL,
                 blob      BLOB    NOT NULL,
                 model     TEXT    NOT NULL DEFAULT '',
                 created_us INTEGER NOT NULL
             );",
        )
        .map_err(sqlite_err)?;

        let index_opts = IndexOptions {
            dimensions: dims as usize,
            metric: MetricKind::Cos,
            quantization: ScalarKind::F32,
            connectivity: 16,
            expansion_add: 200,
            expansion_search: 50,
            multi: false,
        };
        let index = Arc::new(
            Index::new(&index_opts)
                .map_err(|e| MCSError::MemoryError(format!("usearch init: {e}")))?,
        );
        // Reserve searcher threads up front so concurrent searches on a
        // store that never inserted (or hasn't grown yet) also work. The
        // count equals `search_thread_cap`, and SearchGate admits exactly
        // that many concurrent searches: usearch hard-fails past it.
        index
            .reserve_capacity_and_threads(1024, search_thread_cap())
            .map_err(|e| MCSError::MemoryError(format!("usearch reserve: {e}")))?;

        let store = Self {
            index,
            dims,
            gate: SearchGate::new(search_thread_cap()),
            db: Mutex::new(conn),
        };
        store.load_existing()?;
        Ok(Arc::new(store))
    }

    /// Rebuild the in-memory HNSW graph from the persisted rows.
    fn load_existing(&self) -> Result<()> {
        let conn = self.db.lock();
        let mut stmt = conn
            .prepare("SELECT entity_id, dims, blob FROM code_vector")
            .map_err(sqlite_err)?;
        let mut rows: Vec<(i64, Vec<u8>)> = Vec::new();
        let iter = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(2)?))
            })
            .map_err(sqlite_err)?;
        for r in iter {
            rows.push(r.map_err(sqlite_err)?);
        }
        drop(stmt);
        if !rows.is_empty() {
            let needed = rows.len().div_ceil(1024).saturating_mul(1024).max(1024);
            self.index
                .reserve_capacity_and_threads(needed, search_thread_cap())
                .map_err(|e| MCSError::MemoryError(format!("usearch reserve: {e}")))?;
        }
        for (id, blob) in rows {
            let emb = parse_embedding_blob(&blob)?;
            self.index
                .add(id as u64, emb)
                .map_err(|e| MCSError::MemoryError(format!("usearch add: {e}")))?;
        }
        Ok(())
    }

    /// Insert or replace one symbol embedding. The code-symbol entity must
    /// exist in the project's knowledge graph.
    pub fn upsert_embedding(&self, entity_name: &str, emb: &[f32], model: &str) -> Result<()> {
        if emb.len() != self.dims as usize {
            return Err(MCSError::InvalidParams(format!(
                "Embedding dimension mismatch: got {}, expected {}",
                emb.len(),
                self.dims
            )));
        }
        let conn = self.db.lock();
        let id: i64 = conn
            .query_row(
                "SELECT id FROM entity WHERE name_hash=?1 AND name=?2 AND flags=0",
                params![crate::kg::name_hash(entity_name), entity_name],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(sqlite_err)?
            .ok_or_else(|| {
                MCSError::InvalidParams(format!("Entity '{entity_name}' not found in KG"))
            })?;
        // Grow in 1024-slot chunks rather than one slot per upsert.
        let needed = (self.index.size() + 1)
            .div_ceil(1024)
            .saturating_mul(1024)
            .max(1024);
        if needed > self.index.capacity() {
            self.index
                .reserve_capacity_and_threads(needed, search_thread_cap())
                .map_err(|e| MCSError::MemoryError(format!("usearch reserve: {e}")))?;
        }
        let _ = self.index.remove(id as u64);
        self.index
            .add(id as u64, emb)
            .map_err(|e| MCSError::MemoryError(format!("usearch add: {e}")))?;
        let blob = serialize_embedding(emb);
        let mut stmt = conn
            .prepare_cached(
                "INSERT OR REPLACE INTO code_vector (entity_id, dims, blob, model, created_us) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(sqlite_err)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;
        stmt.execute(params![id, self.dims, blob, model, now])
            .map_err(sqlite_err)?;
        Ok(())
    }

    /// Nearest symbol entity ids with distances (ascending = closer).
    pub fn search_embeddings(&self, query: &[f32], top_k: usize) -> Result<Vec<(EntityId, f32)>> {
        self.gate.run(|| {
            if self.index.size() == 0 {
                return Ok(Vec::new());
            }
            let m = self
                .index
                .search(query, top_k.clamp(1, 100))
                .map_err(|e| MCSError::MemoryError(format!("usearch search: {e}")))?;
            let cap = m.keys.len().min(m.distances.len());
            Ok((0..cap)
                .map(|j| (m.keys[j] as EntityId, m.distances[j]))
                .collect())
        })
    }

    /// Resolve a symbol entity id to its current `(name, entityType)` row.
    pub fn resolve_name_type(&self, id: EntityId) -> (String, String) {
        let conn = self.db.lock();
        conn.query_row(
            "SELECT e.name, COALESCE(t.name, '') FROM entity e
             LEFT JOIN type_dict t ON t.id = e.type_id
             WHERE e.id = ?1 AND e.flags = 0",
            [id],
            |row| Ok((row.get::<_, String>(0)?, row.get(1)?)),
        )
        .ok()
        .unwrap_or_default()
    }

    pub const fn dims(&self) -> u32 {
        self.dims
    }
}
