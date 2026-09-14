use rustc_hash::FxHashMap;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::{Mutex, MutexGuard};
use rusqlite::{Connection, OpenFlags, params};

use crate::errors::{MCSError, Result};
use crate::mutation::{
    MutationContext, MutationRequest, MutationResult, MutationService, ObservationUpdate,
};
use crate::storage::{Durability, SqliteTuning};
use crate::types::{
    AttributeDelete, AttributeSet, Degree, Entity, EntityDescription, EntityInput, Observation,
    ObservationInput, Relation, RelationDetail, RelationInput, RelationObservationUpdate,
};

/// Single SQL projection for every full graph JSON read. Alias `o` is an observation row.
const OBSERVATION_JSON: &str = "json_object('body',o.body,'createdAtUs',o.created_us,'occurredAtUs',o.occurred_us,'originEntityName',o.origin_entity_name)";

/// Cap on entities/relations collected in a single traversal (DoS guard).
/// Prevents a dense graph at high depth from allocating unbounded memory.
const MAX_TRAVERSAL_ENTITIES: usize = 500_000;
const MAX_TRAVERSAL_RELS: usize = 2_000_000;

fn sqlite_err(e: rusqlite::Error) -> MCSError {
    MCSError::IoError(std::io::Error::other(e))
}

const fn is_not_found(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::QueryReturnedNoRows)
}

#[inline(always)]
pub fn name_hash(name: &str) -> i64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h as i64
}

fn entity_name_lookup(conn: &Connection, name: &str) -> Result<Option<i64>> {
    let h = name_hash(name);
    let mut stmt = conn
        .prepare_cached("SELECT id FROM entity WHERE name_hash = ?1 AND name = ?2 AND flags = 0")
        .map_err(sqlite_err)?;
    match stmt.query_row(params![h, name], |row| row.get::<_, i64>(0)) {
        Ok(id) => Ok(Some(id)),
        Err(e) if is_not_found(&e) => Ok(None),
        Err(e) => Err(sqlite_err(e)),
    }
}

/// Read-only type lookup. This never inserts, so it is
/// safe to call on a `query_only` reader connection. Returns `None` when the
/// type does not exist.
fn lookup_type_id(conn: &Connection, type_name: &str, kind: i64) -> Option<i64> {
    conn.prepare_cached("SELECT id FROM type_dict WHERE kind = ?1 AND name = ?2")
        .ok()?
        .query_row(params![kind, type_name], |row| row.get::<_, i64>(0))
        .ok()
}

/// Load the k:v attribute map of one owner from the `attribute` table.
/// REQ-ATTR-READ: the read models populate attributes here, on the result
/// path — the write snapshot (`EntitySnapshot`) stays free of them.
fn attributes_for(
    conn: &Connection,
    owner_kind: &str,
    owner_id: i64,
) -> Result<BTreeMap<String, String>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT key, value FROM attribute
             WHERE owner_kind = ?1 AND owner_id = ?2
             ORDER BY key",
        )
        .map_err(sqlite_err)?;
    let rows = stmt
        .query_map(params![owner_kind, owner_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sqlite_err)?;
    rows.collect::<rusqlite::Result<BTreeMap<String, String>>>()
        .map_err(sqlite_err)
}

/// Canonicalize an attribute map for the read models: `None` when empty, so
/// attribute-less entities and descriptions keep the previous wire shape.
fn some_nonempty(map: BTreeMap<String, String>) -> Option<BTreeMap<String, String>> {
    if map.is_empty() { None } else { Some(map) }
}

/// Build the detail rows (always-present observations and attributes) of a
/// list of relation triples, preserving order. A live triple always has a
/// live mirror; a row whose mirror is absent (legacy corruption) is skipped
/// rather than surfaced with fabricated metadata.
fn relation_details(
    conn: &Connection,
    triples: impl Iterator<Item = Relation>,
) -> Result<Vec<RelationDetail>> {
    use rusqlite::OptionalExtension;
    let mut out = Vec::new();
    for relation in triples {
        let Some(mirror_id) = conn
            .query_row(
                "SELECT m.id FROM taxonomy_relation m
                 JOIN entity f ON f.id = m.from_id AND f.name = ?1 AND f.flags = 0
                 JOIN entity t ON t.id = m.to_id AND t.name = ?2 AND t.flags = 0
                 JOIN type_dict d ON d.id = m.type_id AND d.kind = 1 AND d.name = ?3
                 WHERE m.deleted = 0",
                params![relation.from, relation.to, relation.relation_type],
                |row| row.get(0),
            )
            .optional()
            .map_err(sqlite_err)?
        else {
            continue;
        };
        let observations: Vec<Observation> = {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT body, created_us, occurred_us
                     FROM relation_observation WHERE relation_id = ?1
                     ORDER BY idx, id",
                )
                .map_err(sqlite_err)?;
            stmt.query_map([mirror_id], |row| {
                Ok(Observation {
                    body: row.get(0)?,
                    created_at_us: Some(row.get(1)?),
                    occurred_at_us: row.get(2)?,
                    origin_entity_name: None,
                })
            })
            .map_err(sqlite_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sqlite_err)?
        };
        out.push(RelationDetail {
            from: relation.from,
            to: relation.to,
            relation_type: relation.relation_type,
            observations,
            attributes: attributes_for(conn, "relation", mirror_id)?,
        });
    }
    Ok(out)
}

fn read_graph_stat(conn: &Connection, key: &str) -> Result<i64> {
    conn.query_row(
        "SELECT value FROM graph_stat WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .map_err(sqlite_err)
}

fn select_all_types(conn: &Connection, kind: i64) -> Result<Vec<(String, usize)>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT name, count FROM type_dict WHERE kind = ?1 AND count > 0 ORDER BY count DESC",
        )
        .map_err(sqlite_err)?;
    let rows = stmt
        .query_map(params![kind], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
        })
        .map_err(sqlite_err)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// `(name, count, desc)` for every registered type of `kind`: one that has
/// members (`count > 0`), or one that a description marks as intentional even
/// before its first member exists. The list tools expose this registry view;
/// suggestions keep the stricter count-only source above.
fn select_type_catalog(
    conn: &Connection,
    kind: i64,
) -> Result<Vec<(String, usize, Option<String>)>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT name, count, desc FROM type_dict
             WHERE kind = ?1 AND (count > 0 OR desc IS NOT NULL)
             ORDER BY count DESC",
        )
        .map_err(sqlite_err)?;
    let rows = stmt
        .query_map(params![kind], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as usize,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(sqlite_err)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Comma-separated decimal list of ids for an inline `IN (...)` / `VALUES`
/// clause. The ids are `i64` row ids read straight from the database — never
/// user text — so inlining them as SQL literals is injection-safe and, unlike
/// bound `?` parameters, is *not* subject to SQLite's `SQLITE_MAX_VARIABLE_NUMBER`
/// (~32k) ceiling. Traversals and pages over large id/relation sets can thus
/// build one statement instead of overflowing the parameter limit (which used to
/// make the query error out and be silently swallowed into an empty result).
fn int_csv(ids: &[i64]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(ids.len() * 8);
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{id}");
    }
    s
}

/// Build a `(from,to,type), …` literal list for a relation-triple `VALUES` CTE.
/// Same rationale as [`int_csv`]: the triples are DB row ids, so inlining them is
/// injection-safe and sidesteps the bound-parameter ceiling that a large
/// neighbourhood (3 params per edge) would otherwise breach — which previously
/// errored the relations query and was silently swallowed into `[]`.
fn rel_values_literal(rels: &HashSet<(i64, i64, i64)>) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(rels.len() * 16);
    for (i, (f, t, tp)) in rels.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "({f},{t},{tp})");
    }
    s
}

/// Load full entities (name, type, observations) for a set of ids in a single
/// query, returning an id→[`Entity`] map. Replaces the old per-id fetches (an
/// N+1 pattern) on the search path.
fn batch_entities_by_ids(conn: &Connection, ids: &[i64]) -> FxHashMap<i64, Entity> {
    let mut map = FxHashMap::default();
    if ids.is_empty() {
        return map;
    }
    let sql = format!(
        "SELECT e.id, e.name, t.name,
                COALESCE((SELECT json_group_array({OBSERVATION_JSON} ORDER BY o.idx, o.id)
                          FROM observation o WHERE o.entity_id = e.id), '[]')
         FROM entity e JOIN type_dict t ON t.id = e.type_id
         WHERE e.id IN ({}) AND e.flags = 0",
        int_csv(ids)
    );
    if let Ok(mut stmt) = conn.prepare(&sql)
        && let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
    {
        for (id, name, etype, obs_json) in rows.flatten() {
            let observations: Vec<Observation> =
                serde_json::from_str(&obs_json).unwrap_or_default();
            map.insert(
                id,
                Entity {
                    name,
                    entity_type: etype,
                    observations,
                    attributes: None,
                },
            );
        }
    }
    map
}

/// Like [`batch_entities_by_ids`], but for the viewer's list payloads: skips the
/// observation *bodies* (the canvas never renders them in bulk — they are
/// lazy-loaded for the single inspected node) and returns only name, type, and
/// the denormalised `obs_count`. Keeps the search payload — and the reader-lock
/// hold — small.
fn batch_entity_lite_by_ids(
    conn: &Connection,
    ids: &[i64],
) -> FxHashMap<i64, (String, String, i64)> {
    let mut map = FxHashMap::default();
    if ids.is_empty() {
        return map;
    }
    let sql = format!(
        "SELECT e.id, e.name, t.name, e.obs_count
         FROM entity e JOIN type_dict t ON t.id = e.type_id
         WHERE e.id IN ({}) AND e.flags = 0",
        int_csv(ids)
    );
    if let Ok(mut stmt) = conn.prepare(&sql)
        && let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
    {
        for (id, name, etype, oc) in rows.flatten() {
            map.insert(id, (name, etype, oc));
        }
    }
    map
}

/// Collect entity ids matching an FTS query — name matches first (by rank), then
/// observation matches — de-duplicated, each source capped at `cap` rows. Shared
/// by the MCP and viewer search paths so both agree on ordering.
fn fts_candidate_ids(conn: &Connection, query: &str, cap: usize) -> Vec<i64> {
    let mut ids: Vec<i64> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    let cap_i64 = cap as i64;

    if let Ok(mut stmt) =
        conn.prepare("SELECT rowid FROM name_fts WHERE name_fts MATCH ?1 ORDER BY rank LIMIT ?2")
        && let Ok(rows) = stmt.query_map(params![query, cap_i64], |row| row.get::<_, i64>(0))
    {
        for id in rows.flatten() {
            if seen.insert(id) {
                ids.push(id);
            }
        }
    }

    if let Ok(mut stmt) = conn.prepare(
        "SELECT entity_id FROM obs_fts JOIN observation ON obs_fts.rowid = observation.id
         WHERE obs_fts MATCH ?1
         GROUP BY entity_id
         LIMIT ?2",
    ) && let Ok(rows) = stmt.query_map(params![query, cap_i64], |row| row.get::<_, i64>(0))
    {
        for id in rows.flatten() {
            if seen.insert(id) {
                ids.push(id);
            }
        }
    }

    ids
}

/// Direction of relation traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}

impl Direction {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("OUTGOING") => Direction::Outgoing,
            Some("INCOMING") => Direction::Incoming,
            _ => Direction::Both,
        }
    }
}

/// Escape a string for embedding in JSON, writing directly into the given buffer.
/// Avoids allocating a temporary `serde_json::Value` for the JSON-RPC wrapper.
pub fn push_json_str(buf: &mut String, raw: &str) {
    buf.push('"');
    let mut start = 0;
    let bytes = raw.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        let esc: u8 = match b {
            b'"' => b'"',
            b'\\' => b'\\',
            b'\n' => b'n',
            b'\r' => b'r',
            b'\t' => b't',
            0x08 => b'b',
            0x0C => b'f',
            0x00..=0x07 | 0x0B | 0x0E..=0x1F => continue, // escaped below
            _ => continue,
        };
        buf.push_str(&raw[start..i]);
        buf.push('\\');
        buf.push(esc as char);
        start = i + 1;
    }
    // Control chars 0x00-0x1F not handled above: escape as \u00XX
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if b <= 0x07 || b == 0x0B || (0x0E..=0x1F).contains(&b) {
            buf.push_str(&raw[start..i]);
            write_escape_unicode(buf, b);
            start = i + 1;
        }
    }
    buf.push_str(&raw[start..]);
    buf.push('"');
}

#[inline(never)]
fn write_escape_unicode(buf: &mut String, b: u8) {
    use std::fmt::Write;
    write!(buf, "\\u{:04x}", b).unwrap();
}

// ── Transaction guard (RAII rollback on error) ─────────────────────────

pub(crate) struct TxGuard<'a> {
    conn: &'a Connection,
    done: bool,
}

impl<'a> TxGuard<'a> {
    pub(crate) fn begin(conn: &'a Connection) -> Result<Self> {
        // BEGIN IMMEDIATE acquires the WAL write lock up front rather than
        // lazily on the first write. This makes the busy-timeout apply to lock
        // acquisition deterministically and avoids `SQLITE_BUSY_SNAPSHOT`
        // surprises when readers are concurrently active.
        conn.execute_batch("BEGIN IMMEDIATE").map_err(sqlite_err)?;
        Ok(Self { conn, done: false })
    }

    pub(crate) fn commit(mut self) -> Result<()> {
        self.conn.execute_batch("COMMIT").map_err(sqlite_err)?;
        self.done = true;
        Ok(())
    }
}

impl Drop for TxGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

// ── Reader pool ───────────────────────────────────────────────────────────

/// A small fixed pool of `query_only` SQLite connections used for read
/// operations. WAL mode permits any number of concurrent readers alongside the
/// single writer, so spreading reads across several connections lets them run
/// in parallel instead of serializing on the writer's mutex.
struct ReaderPool {
    conns: Vec<Mutex<Connection>>,
    next: AtomicUsize,
}

impl ReaderPool {
    /// Acquire a reader connection. Fast path: grab the first idle one. If every
    /// connection is busy, block on a round-robin pick so callers still make
    /// progress (and never spin).
    fn get(&self) -> MutexGuard<'_, Connection> {
        for c in &self.conns {
            if let Some(g) = c.try_lock() {
                return g;
            }
        }
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.conns.len();
        self.conns[i].lock()
    }
}

// ── GraphHandle ──────────────────────────────────────────────────────────

pub struct GraphHandle {
    /// The single read-write connection. SQLite allows only one writer, so all
    /// mutations serialize here.
    pub(crate) writer: Mutex<Connection>,
    /// Pool of `query_only` connections for concurrent reads (WAL).
    readers: ReaderPool,
    seq_entity: AtomicI64,
    seq_obs: AtomicI64,
    seq_rel_obs: AtomicI64,
}

/// Open one `query_only` reader connection against an existing WAL database.
///
/// The connection is opened read-write at the OS level (so it can attach to the
/// `-shm` wal-index — SQLite cannot read a WAL database through a pure
/// `SQLITE_OPEN_READ_ONLY` handle) and then locked to reads with
/// `PRAGMA query_only = ON`, which makes any accidental write error out.
fn open_reader(path: &Path, tuning: &SqliteTuning) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(sqlite_err)?;
    conn.busy_timeout(Duration::from_millis(tuning.busy_timeout_ms))
        .map_err(sqlite_err)?;
    conn.execute_batch(&format!(
        "PRAGMA query_only   = ON;
         PRAGMA cache_size   = -{};
         PRAGMA temp_store   = MEMORY;
         PRAGMA mmap_size    = {};",
        tuning.cache_size_kb, tuning.mmap_size
    ))
    .map_err(sqlite_err)?;
    Ok(conn)
}

impl GraphHandle {
    pub fn new(
        path: &Path,
        durability: Durability,
        tuning: SqliteTuning,
        _lru_cache_size: NonZeroUsize,
        read_pool_size: usize,
    ) -> Result<Self> {
        let conn = Connection::open(path).map_err(sqlite_err)?;
        // Apply the busy handler through the API so it is in force for every
        // subsequent statement (including schema creation and BEGIN IMMEDIATE).
        conn.busy_timeout(Duration::from_millis(tuning.busy_timeout_ms))
            .map_err(sqlite_err)?;

        // `page_size` and `auto_vacuum` are fixed when the database first gets
        // content, and `page_size` additionally must precede `journal_mode=WAL`.
        // Set both up front on this connection, before any table is created, so
        // they take effect on a fresh database. On an existing database they are
        // silently ignored (would require VACUUM to change).
        conn.execute_batch(&format!(
            "PRAGMA page_size    = {};
             PRAGMA auto_vacuum  = INCREMENTAL;",
            tuning.page_size
        ))
        .map_err(sqlite_err)?;

        conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = OFF;
             PRAGMA cache_size    = -{};
             PRAGMA temp_store    = MEMORY;
             PRAGMA busy_timeout  = {};
             PRAGMA synchronous   = NORMAL;
             PRAGMA journal_size_limit = {};",
            tuning.cache_size_kb, tuning.busy_timeout_ms, tuning.journal_size_limit
        ))
        .map_err(sqlite_err)?;

        crate::schema::initialize_database(&conn)?;

        conn.execute_batch(&format!("PRAGMA mmap_size = {};", tuning.mmap_size))
            .map_err(sqlite_err)?;

        let sync_pragma = match durability {
            Durability::Sync => "PRAGMA synchronous = FULL",
            Durability::Async => "PRAGMA synchronous = NORMAL",
        };
        conn.execute_batch(sync_pragma).map_err(sqlite_err)?;

        // Bound the cost of `PRAGMA optimize` (here and in maintenance) so a
        // large database cannot stall startup/maintenance analyzing every index.
        conn.execute_batch("PRAGMA analysis_limit = 400;")
            .map_err(sqlite_err)?;

        conn.execute_batch("PRAGMA optimize;").map_err(sqlite_err)?;

        let seq_entity = read_graph_stat(&conn, "entity_seq").unwrap_or(0);
        let seq_obs = read_graph_stat(&conn, "obs_seq").unwrap_or(0);
        let seq_rel_obs = read_graph_stat(&conn, "rel_obs_seq").unwrap_or(0);

        // Open the reader pool against the now-initialized database. At least one
        // reader is always created.
        let pool_size = read_pool_size.max(1);
        let mut conns = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            conns.push(Mutex::new(open_reader(path, &tuning)?));
        }
        let readers = ReaderPool {
            conns,
            next: AtomicUsize::new(0),
        };

        Ok(Self {
            writer: Mutex::new(conn),
            readers,
            seq_entity: AtomicI64::new(seq_entity),
            seq_obs: AtomicI64::new(seq_obs),
            seq_rel_obs: AtomicI64::new(seq_rel_obs),
        })
    }

    pub(crate) fn next_entity_id(&self) -> i64 {
        self.seq_entity.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Refresh while holding BEGIN IMMEDIATE; another process may have advanced
    /// the durable counters since this handle opened its connection.
    pub(crate) fn refresh_seqs(&self, conn: &Connection) -> Result<()> {
        self.seq_entity
            .fetch_max(read_graph_stat(conn, "entity_seq")?, Ordering::Relaxed);
        self.seq_obs
            .fetch_max(read_graph_stat(conn, "obs_seq")?, Ordering::Relaxed);
        self.seq_rel_obs
            .fetch_max(read_graph_stat(conn, "rel_obs_seq")?, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn next_obs_id(&self) -> i64 {
        self.seq_obs.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Next relation observation id, from the dedicated `rel_obs_seq` cell.
    /// The id space is disjoint from entity observation ids.
    pub(crate) fn next_rel_obs_id(&self) -> i64 {
        self.seq_rel_obs.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn get_entity_id(&self, conn: &Connection, name: &str) -> Result<Option<(i64, i64, i64, i64)>> {
        use rusqlite::OptionalExtension;
        conn.query_row(
            "SELECT id, type_id, out_deg, in_deg FROM entity WHERE name_hash = ?1 AND name = ?2 AND flags = 0",
            params![name_hash(name), name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional().map_err(sqlite_err)
    }

    pub(crate) fn sync_seqs(&self, conn: &Connection) -> Result<()> {
        let seq_e = self.seq_entity.load(Ordering::Relaxed);
        let seq_o = self.seq_obs.load(Ordering::Relaxed);
        let seq_r = self.seq_rel_obs.load(Ordering::Relaxed);
        conn.execute(
            "UPDATE graph_stat SET value = CASE key
                 WHEN 'entity_seq' THEN ?1
                 WHEN 'obs_seq' THEN ?2
                 WHEN 'rel_obs_seq' THEN ?3
                 ELSE value END
             WHERE key IN ('entity_seq', 'obs_seq', 'rel_obs_seq')",
            params![seq_e, seq_o, seq_r],
        )
        .map_err(sqlite_err)?;
        Ok(())
    }

    // ── Public API ──────────────────────────────────────────────────────

    pub fn get_entity(&self, name: &str) -> Result<Option<Entity>> {
        let conn = self.readers.get();
        // One read transaction prevents mixing metadata and observations from
        // opposite sides of a concurrent commit.
        let tx = conn.unchecked_transaction().map_err(sqlite_err)?;
        let entity = match crate::mutation::read_entity(&tx, name)? {
            Some(snapshot) => {
                let mut entity = snapshot.entity();
                entity.attributes =
                    some_nonempty(attributes_for(&tx, "entity", snapshot.entity_id)?);
                Some(entity)
            }
            None => None,
        };
        tx.commit().map_err(sqlite_err)?;
        Ok(entity)
    }

    fn mutate(&self, request: MutationRequest) -> Result<MutationResult> {
        MutationService::new(self)
            .apply_with_result(request, MutationContext::local())
            .map(|(_, result)| result)
    }

    pub fn create_entities(&self, entities: &[EntityInput]) -> Result<Vec<Entity>> {
        match self.mutate(MutationRequest::CreateEntities {
            entities: entities.to_vec(),
        })? {
            MutationResult::Entities(result) => Ok(result),
            _ => unreachable!("create_entities always returns entities"),
        }
    }

    pub fn upsert_entities(&self, entities: &[EntityInput]) -> Result<Vec<Entity>> {
        match self.mutate(MutationRequest::UpsertEntities {
            entities: entities.to_vec(),
        })? {
            MutationResult::Entities(result) => Ok(result),
            _ => unreachable!("upsert_entities always returns entities"),
        }
    }

    pub fn delete_entities(&self, names: &[String]) -> Result<()> {
        self.mutate(MutationRequest::DeleteEntities {
            names: names.to_vec(),
        })
        .map(|_| ())
    }

    pub fn create_relations(&self, relations: &[RelationInput]) -> Result<Vec<Relation>> {
        match self.mutate(MutationRequest::CreateRelations {
            relations: relations.to_vec(),
        })? {
            MutationResult::Relations(result) => Ok(result),
            _ => unreachable!("create_relations always returns relations"),
        }
    }

    pub fn delete_relations(&self, relations: &[Relation]) -> Result<()> {
        self.mutate(MutationRequest::DeleteRelations {
            relations: relations.to_vec(),
        })
        .map(|_| ())
    }

    pub fn add_relation_observations(
        &self,
        from: &str,
        to: &str,
        relation_type: &str,
        contents: &[ObservationInput],
    ) -> Result<Vec<Observation>> {
        match self.mutate(MutationRequest::AddRelationObservations {
            relations: vec![RelationObservationUpdate {
                relation: Relation {
                    from: from.into(),
                    to: to.into(),
                    relation_type: relation_type.into(),
                },
                contents: contents.to_vec(),
            }],
        })? {
            MutationResult::RelationObservations(mut result) => {
                Ok(result.remove(0).added_observations)
            }
            _ => unreachable!("add_relation_observations always returns observations"),
        }
    }

    pub fn delete_relation_observations(
        &self,
        from: &str,
        to: &str,
        relation_type: &str,
        observations: &[ObservationInput],
    ) -> Result<()> {
        self.mutate(MutationRequest::DeleteRelationObservations {
            relations: vec![RelationObservationUpdate {
                relation: Relation {
                    from: from.into(),
                    to: to.into(),
                    relation_type: relation_type.into(),
                },
                contents: observations.to_vec(),
            }],
        })
        .map(|_| ())
    }

    pub fn set_attributes(&self, targets: &[AttributeSet]) -> Result<()> {
        self.mutate(MutationRequest::SetAttributes {
            targets: targets.to_vec(),
        })
        .map(|_| ())
    }

    pub fn delete_attributes(&self, targets: &[AttributeDelete]) -> Result<()> {
        self.mutate(MutationRequest::DeleteAttributes {
            targets: targets.to_vec(),
        })
        .map(|_| ())
    }

    pub fn add_observations(
        &self,
        entity_name: &str,
        contents: &[ObservationInput],
    ) -> Result<Vec<Observation>> {
        match self.mutate(MutationRequest::AddObservations {
            observations: vec![ObservationUpdate {
                entity_name: entity_name.into(),
                contents: contents.to_vec(),
            }],
        })? {
            MutationResult::Observations(mut result) => Ok(result.remove(0).added_observations),
            _ => unreachable!("add_observations always returns observations"),
        }
    }

    pub fn delete_observations(
        &self,
        entity_name: &str,
        observations: &[ObservationInput],
    ) -> Result<()> {
        self.mutate(MutationRequest::DeleteObservations {
            observations: vec![ObservationUpdate {
                entity_name: entity_name.into(),
                contents: observations.to_vec(),
            }],
        })
        .map(|_| ())
    }

    pub fn merge_entities(&self, source: &str, target: &str) -> Result<Entity> {
        match self.mutate(MutationRequest::MergeEntities {
            source: source.into(),
            target: target.into(),
        })? {
            MutationResult::Entity(result) => Ok(result),
            _ => unreachable!("merge_entities always returns an entity"),
        }
    }

    pub fn rename_entity(&self, old_name: &str, new_name: &str) -> Result<Entity> {
        match self.mutate(MutationRequest::RenameEntity {
            old_name: old_name.into(),
            new_name: new_name.into(),
        })? {
            MutationResult::Entity(result) => Ok(result),
            _ => unreachable!("rename_entity always returns an entity"),
        }
    }

    /// Delete a code file and all its defined symbols in the same transaction.
    pub fn code_purge_file(&self, rel_path: &str) -> Result<usize> {
        match self.mutate(MutationRequest::PurgeDefinedEntities {
            name: rel_path.into(),
        })? {
            MutationResult::Count(count) => Ok(count),
            _ => unreachable!("purge always returns a count"),
        }
    }

    pub fn search_nodes_filtered(
        &self,
        query: &str,
        filter_type: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Vec<Entity> {
        if query.is_empty() {
            return Vec::new();
        }
        let conn = self.readers.get();

        // Collect ordered candidate ids, then load them all in ONE query instead
        // of an `entity_by_id` per candidate (the old N+1). Ordering (name matches
        // first, then observation matches) and the post-filter offset semantics
        // are preserved exactly.
        let cap = offset.saturating_add(limit);
        let candidates = fts_candidate_ids(&conn, query, cap);
        let mut by_id = batch_entities_by_ids(&conn, &candidates);

        let mut results = Vec::new();
        let mut count: usize = 0;
        for eid in candidates {
            let Some(entity) = by_id.remove(&eid) else {
                continue;
            };
            if let Some(ft) = filter_type
                && !ft.is_empty()
                && entity.entity_type != ft
            {
                continue;
            }
            if count < offset {
                count += 1;
                continue;
            }
            if results.len() >= limit {
                break;
            }
            results.push(entity);
            count += 1;
        }

        results
    }

    /// The viewer's search payload: a JSON array of `{name, entityType, obsCount}`
    /// (no observation bodies — see [`batch_entity_lite_by_ids`]), in the same
    /// order and with the same post-filter offset semantics as
    /// [`Self::search_nodes_filtered`]. Returns `(entities_json, returned,
    /// has_more)`; `has_more` is detected by fetching one extra match past the
    /// page. Builds the JSON directly to avoid a `Vec<Entity>` round-trip.
    pub fn search_nodes_lite_json(
        &self,
        query: &str,
        filter_type: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> (String, usize, bool) {
        use std::fmt::Write as _;
        if query.is_empty() {
            return ("[]".to_string(), 0, false);
        }
        let conn = self.readers.get();
        // One extra row past the page lets us report `hasMore` after filtering.
        let cap = offset.saturating_add(limit).saturating_add(1);
        let candidates = fts_candidate_ids(&conn, query, cap);
        let by_id = batch_entity_lite_by_ids(&conn, &candidates);
        let ft = filter_type.filter(|s| !s.is_empty());

        let mut arr = String::from("[");
        let mut count: usize = 0; // entities passing the type filter, seen so far
        let mut returned: usize = 0;
        let mut has_more = false;
        for eid in candidates {
            let Some((name, etype, oc)) = by_id.get(&eid) else {
                continue;
            };
            if let Some(f) = ft
                && etype != f
            {
                continue;
            }
            if count < offset {
                count += 1;
                continue;
            }
            if returned >= limit {
                has_more = true;
                break;
            }
            if returned > 0 {
                arr.push(',');
            }
            arr.push_str("{\"name\":");
            push_json_str(&mut arr, name);
            arr.push_str(",\"entityType\":");
            push_json_str(&mut arr, etype);
            let _ = write!(arr, ",\"obsCount\":{oc}}}");
            returned += 1;
            count += 1;
        }
        arr.push(']');
        (arr, returned, has_more)
    }

    pub fn read_graph_filtered(
        &self,
        filter_type: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<String> {
        self.read_graph_page(filter_type, offset, limit, true)
            .map(|(json, _)| json)
    }

    /// Observation-free page for the browser viewer: entities carry `obsCount`
    /// (the denormalised count) instead of the observation bodies, which the
    /// canvas never renders in bulk — the inspector lazy-loads them for the one
    /// selected node via `GET /ui/node`. Returns `(json, returned)` so the caller
    /// can build the pagination cursor without re-parsing the payload.
    pub fn read_graph_filtered_lite(
        &self,
        filter_type: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<(String, usize)> {
        self.read_graph_page(filter_type, offset, limit, false)
    }

    fn read_graph_page(
        &self,
        filter_type: Option<&str>,
        offset: usize,
        limit: usize,
        include_obs: bool,
    ) -> Result<(String, usize)> {
        let conn = self.readers.get();

        let limit_sql: i64 = if limit == usize::MAX {
            -1
        } else {
            limit.min(i64::MAX as usize) as i64
        };
        let offset_sql: i64 = offset as i64;

        // Resolve the requested page of entity ids first. Relations are then
        // scoped to edges whose *both* endpoints fall inside this page, which
        // keeps the response self-consistent (no dangling references to
        // entities that were paged out) and bounds the relation payload by the
        // page size instead of dumping every relation in the graph.
        let filter = filter_type.filter(|ft| !ft.is_empty());
        let ids: Vec<i64> = if let Some(ft) = filter {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT e.id FROM entity e
                     WHERE e.type_id = (SELECT id FROM type_dict WHERE kind = 0 AND name = ?1)
                       AND e.flags = 0
                     ORDER BY e.id LIMIT ?2 OFFSET ?3",
                )
                .map_err(sqlite_err)?;
            stmt.query_map(params![ft, limit_sql, offset_sql], |r| r.get::<_, i64>(0))
                .map_err(sqlite_err)?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT e.id FROM entity e WHERE e.flags = 0
                     ORDER BY e.id LIMIT ?1 OFFSET ?2",
                )
                .map_err(sqlite_err)?;
            stmt.query_map(params![limit_sql, offset_sql], |r| r.get::<_, i64>(0))
                .map_err(sqlite_err)?
                .filter_map(|r| r.ok())
                .collect()
        };

        if ids.is_empty() {
            return Ok((r#"{"entities":[],"relations":[]}"#.to_string(), 0));
        }

        // Inline the page's ids as SQL integer literals rather than bound `?`
        // parameters: a full-graph read (limit = usize::MAX) can exceed SQLite's
        // ~32k variable cap, which would error the query. The ids are DB row ids,
        // so this is injection-safe. See [`int_csv`].
        let idlist = int_csv(&ids);
        let returned = ids.len();

        // The viewer omits observation bodies (they inflate the payload, the
        // reader-lock hold, and browser memory for a graph the canvas only lays
        // out); it ships `obsCount` instead. The MCP `read_graph` keeps the full
        // observations shape.
        let obs_field = if include_obs {
            format!("'observations', COALESCE((SELECT json_group_array({OBSERVATION_JSON} ORDER BY o.idx, o.id)
                        FROM observation o WHERE o.entity_id = e.id), json('[]'))")
        } else {
            "'obsCount', e.obs_count".to_owned()
        };

        let entities_json: String = {
            let sql = format!(
                "SELECT COALESCE(json_group_array(json_object(
                    'name', e.name,
                    'entityType', t.name,
                    {obs_field}
                ) ORDER BY e.id), json('[]'))
                FROM entity e
                JOIN type_dict t ON t.id = e.type_id
                WHERE e.id IN ({idlist}) AND e.flags = 0"
            );
            conn.query_row(&sql, [], |row| row.get::<_, String>(0))
                .map_err(sqlite_err)?
        };

        let relations_json: String = {
            let sql = format!(
                "SELECT COALESCE(json_group_array(json_object(
                    'from', e1.name,
                    'to', e2.name,
                    'relationType', t.name
                )), json('[]'))
                FROM relation r
                JOIN entity e1 ON e1.id = r.from_id
                JOIN entity e2 ON e2.id = r.to_id
                JOIN type_dict t ON t.id = r.type_id
                WHERE r.from_id IN ({idlist}) AND r.to_id IN ({idlist})
                  AND e1.flags = 0 AND e2.flags = 0"
            );
            conn.query_row(&sql, [], |row| row.get::<_, String>(0))
                .map_err(sqlite_err)?
        };

        let mut out = String::with_capacity(32 + entities_json.len() + relations_json.len());
        out.push_str("{\"entities\":");
        out.push_str(&entities_json);
        out.push_str(",\"relations\":");
        out.push_str(&relations_json);
        out.push('}');
        Ok((out, returned))
    }

    pub fn open_nodes(&self, names: &[String]) -> String {
        let conn = self.readers.get();
        let mut entity_ids: Vec<i64> = Vec::new();

        for name in names {
            let h = name_hash(name);
            if let Ok(Some(id)) = conn
                .query_row(
                    "SELECT id FROM entity WHERE name_hash = ?1 AND name = ?2 AND flags = 0",
                    params![h, name],
                    |row| row.get::<_, i64>(0),
                )
                .map(Some)
                .or_else(|e| {
                    if is_not_found(&e) {
                        Ok(None)
                    } else {
                        Err(sqlite_err(e))
                    }
                })
            {
                entity_ids.push(id);
            }
        }

        if entity_ids.is_empty() {
            return r#"{"entities":[],"relations":[]}"#.to_string();
        }

        let placeholders: Vec<String> = entity_ids.iter().map(|_| "?".to_string()).collect();
        let ids_str = placeholders.join(",");

        let entities_json: String = {
            let sql = format!(
                "SELECT COALESCE(json_group_array(json_object(
                    'name', e.name,
                    'entityType', t.name,
                    'observations', COALESCE((
                        SELECT json_group_array({OBSERVATION_JSON} ORDER BY o.idx, o.id)
                        FROM observation o WHERE o.entity_id = e.id
                    ), json('[]'))
                ) ORDER BY e.id), json('[]'))
                FROM entity e
                JOIN type_dict t ON t.id = e.type_id
                WHERE e.id IN ({ids_str}) AND e.flags = 0"
            );
            conn.query_row(&sql, rusqlite::params_from_iter(&entity_ids), |row| {
                row.get::<_, String>(0)
            })
            .unwrap_or_else(|_| "[]".to_string())
        };

        let relations_json: String = {
            let sql = format!(
                "SELECT COALESCE(json_group_array(json_object(
                    'from', e1.name,
                    'to', e2.name,
                    'relationType', t.name
                )), json('[]'))
                FROM relation r
                JOIN entity e1 ON e1.id = r.from_id
                JOIN entity e2 ON e2.id = r.to_id
                JOIN type_dict t ON t.id = r.type_id
                WHERE (r.from_id IN ({ids_str}) OR r.to_id IN ({ids_str}))
                  AND e1.flags = 0 AND e2.flags = 0"
            );
            let all_params: Vec<&dyn rusqlite::types::ToSql> = entity_ids
                .iter()
                .map(|id| id as &dyn rusqlite::types::ToSql)
                .chain(
                    entity_ids
                        .iter()
                        .map(|id| id as &dyn rusqlite::types::ToSql),
                )
                .collect();
            let mut stmt = conn.prepare(&sql).unwrap();
            stmt.query_row(all_params.as_slice(), |row| row.get::<_, String>(0))
                .unwrap_or_else(|_| "[]".to_string())
        };

        let mut out = String::with_capacity(32 + entities_json.len() + relations_json.len());
        out.push_str("{\"entities\":");
        out.push_str(&entities_json);
        out.push_str(",\"relations\":");
        out.push_str(&relations_json);
        out.push('}');
        out
    }

    pub fn entities_exist(&self, names: &[String]) -> Result<Vec<bool>> {
        let conn = self.readers.get();
        let mut results = Vec::with_capacity(names.len());
        for name in names {
            let h = name_hash(name);
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM entity WHERE name_hash = ?1 AND name = ?2 AND flags = 0",
                    params![h, name],
                    |_| Ok(()),
                )
                .is_ok();
            results.push(exists);
        }
        Ok(results)
    }

    pub fn degree(&self, name: &str, direction: Direction) -> Result<usize> {
        let conn = self.readers.get();
        let (_, _, out_d, in_d) = match self.get_entity_id(&conn, name)? {
            Some(v) => v,
            None => {
                return Err(MCSError::InvalidParams(format!(
                    "Entity '{name}' not found"
                )));
            }
        };
        Ok(match direction {
            Direction::Outgoing => out_d as usize,
            Direction::Incoming => in_d as usize,
            Direction::Both => (out_d + in_d) as usize,
        })
    }

    pub fn get_entity_count(&self) -> Result<usize> {
        let conn = self.readers.get();
        read_graph_stat(&conn, "entities")
            .map(|v| v as usize)
            .map_err(|_| MCSError::MemoryError("Failed to read entity count".into()))
    }

    pub fn get_relation_count(&self) -> Result<usize> {
        let conn = self.readers.get();
        read_graph_stat(&conn, "relations")
            .map(|v| v as usize)
            .map_err(|_| MCSError::MemoryError("Failed to read relation count".into()))
    }

    pub fn search_relations(
        &self,
        from: Option<&str>,
        to: Option<&str>,
        rtype: Option<&str>,
        query: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<RelationDetail>> {
        let conn = self.readers.get();

        // A filter that is supplied but resolves to nothing uses the sentinel
        // id -1 (which matches no row), so the query returns empty rather than
        // silently dropping the filter and matching every relation. The lookups
        // are read-only — `get_type_id` would *insert* a phantom type, which is
        // both wrong and impossible on a `query_only` reader connection.
        let from_id = from
            .filter(|f| !f.is_empty())
            .map(|f| entity_name_lookup(&conn, f).ok().flatten().unwrap_or(-1));
        let to_id = to
            .filter(|t| !t.is_empty())
            .map(|t| entity_name_lookup(&conn, t).ok().flatten().unwrap_or(-1));
        let type_id = rtype
            .filter(|rt| !rt.is_empty())
            .map(|rt| lookup_type_id(&conn, rt, 1).unwrap_or(-1));

        // REQ-OBS-FTS: query mode matches `rel_obs_fts` (bm25 rank) and
        // resolves matched observation rows to their owner relation, composed
        // with the structural filters as AND. Results stay owner-level: one
        // detail row per distinct triple, in best-rank order.
        if let Some(query) = query.filter(|q| !q.trim().is_empty()) {
            let mut sql = String::from(
                "SELECT f.name, t.name, d.name
                 FROM rel_obs_fts ft
                 JOIN relation_observation ro ON ro.id = ft.rowid
                 JOIN taxonomy_relation m ON m.id = ro.relation_id
                 JOIN entity f ON f.id = m.from_id
                 JOIN entity t ON t.id = m.to_id
                 JOIN type_dict d ON d.id = m.type_id
                 WHERE rel_obs_fts MATCH ?1 AND m.deleted = 0
                   AND f.flags = 0 AND t.flags = 0",
            );
            // Resolved ids (or the -1 sentinel for supplied-but-missing) are
            // DB row ids, never user text, so inlining them is injection-safe
            // and avoids binding-temporary lifetimes — the same convention as
            // `int_csv` / `rel_values_literal`.
            if let Some(fid) = from_id {
                sql.push_str(&format!(" AND m.from_id = {fid}"));
            }
            if let Some(tid) = to_id {
                sql.push_str(&format!(" AND m.to_id = {tid}"));
            }
            if let Some(tpid) = type_id {
                sql.push_str(&format!(" AND m.type_id = {tpid}"));
            }
            sql.push_str(" ORDER BY rank");
            if let Some(lim) = limit
                && lim > 0
            {
                sql.push_str(&format!(" LIMIT {lim}"));
            }
            let mut triples: Vec<Relation> = Vec::new();
            let mut seen: HashSet<(String, String, String)> = HashSet::new();
            let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
            let rows = stmt
                .query_map(params![query], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(sqlite_err)?;
            for row in rows {
                let (from, to, relation_type) = row.map_err(sqlite_err)?;
                if seen.insert((from.clone(), to.clone(), relation_type.clone())) {
                    triples.push(Relation {
                        from,
                        to,
                        relation_type,
                    });
                }
            }
            return relation_details(&conn, triples.into_iter());
        }

        // No-query path: keep the existing exact-match arms and their ordering
        // semantics (ORDER BY from_id, to_id) byte-identical, then attach the
        // always-present per-row observations and attributes.
        let mut triples: Vec<Relation> = Vec::new();
        match (from_id, to_id, type_id) {
            (Some(fid), Some(tid), Some(tpid)) => {
                if let Ok(mut stmt) = conn.prepare_cached(
                    "SELECT e1.name, e2.name, t.name
                     FROM relation r
                     JOIN entity e1 ON e1.id = r.from_id
                     JOIN entity e2 ON e2.id = r.to_id
                     JOIN type_dict t ON t.id = r.type_id
                     WHERE r.from_id = ?1 AND r.to_id = ?2 AND r.type_id = ?3
                       AND e1.flags = 0 AND e2.flags = 0
                     ORDER BY r.from_id, r.to_id",
                ) && let Ok(rows) = stmt.query_map(params![fid, tid, tpid], |row| {
                    Ok(Relation {
                        from: row.get(0)?,
                        to: row.get(1)?,
                        relation_type: row.get(2)?,
                    })
                }) {
                    for row in rows.flatten() {
                        triples.push(row);
                    }
                }
            }
            (Some(fid), Some(tid), None) => {
                if let Ok(mut stmt) = conn.prepare_cached(
                    "SELECT e1.name, e2.name, t.name
                     FROM relation r
                     JOIN entity e1 ON e1.id = r.from_id
                     JOIN entity e2 ON e2.id = r.to_id
                     JOIN type_dict t ON t.id = r.type_id
                     WHERE r.from_id = ?1 AND r.to_id = ?2
                       AND e1.flags = 0 AND e2.flags = 0
                     ORDER BY r.from_id, r.to_id",
                ) && let Ok(rows) = stmt.query_map(params![fid, tid], |row| {
                    Ok(Relation {
                        from: row.get(0)?,
                        to: row.get(1)?,
                        relation_type: row.get(2)?,
                    })
                }) {
                    for row in rows.flatten() {
                        triples.push(row);
                    }
                }
            }
            (Some(fid), None, Some(tpid)) => {
                if let Ok(mut stmt) = conn.prepare_cached(
                    "SELECT e1.name, e2.name, t.name
                     FROM relation r
                     JOIN entity e1 ON e1.id = r.from_id
                     JOIN entity e2 ON e2.id = r.to_id
                     JOIN type_dict t ON t.id = r.type_id
                     WHERE r.from_id = ?1 AND r.type_id = ?2
                       AND e1.flags = 0 AND e2.flags = 0
                     ORDER BY r.from_id, r.to_id",
                ) && let Ok(rows) = stmt.query_map(params![fid, tpid], |row| {
                    Ok(Relation {
                        from: row.get(0)?,
                        to: row.get(1)?,
                        relation_type: row.get(2)?,
                    })
                }) {
                    for row in rows.flatten() {
                        triples.push(row);
                    }
                }
            }
            (None, Some(tid), Some(tpid)) => {
                if let Ok(mut stmt) = conn.prepare_cached(
                    "SELECT e1.name, e2.name, t.name
                     FROM relation r
                     JOIN entity e1 ON e1.id = r.from_id
                     JOIN entity e2 ON e2.id = r.to_id
                     JOIN type_dict t ON t.id = r.type_id
                     WHERE r.to_id = ?1 AND r.type_id = ?2
                       AND e1.flags = 0 AND e2.flags = 0
                     ORDER BY r.from_id, r.to_id",
                ) && let Ok(rows) = stmt.query_map(params![tid, tpid], |row| {
                    Ok(Relation {
                        from: row.get(0)?,
                        to: row.get(1)?,
                        relation_type: row.get(2)?,
                    })
                }) {
                    for row in rows.flatten() {
                        triples.push(row);
                    }
                }
            }
            (Some(fid), None, None) => {
                if let Ok(mut stmt) = conn.prepare_cached(
                    "SELECT e1.name, e2.name, t.name
                     FROM relation r
                     JOIN entity e1 ON e1.id = r.from_id
                     JOIN entity e2 ON e2.id = r.to_id
                     JOIN type_dict t ON t.id = r.type_id
                     WHERE r.from_id = ?1
                       AND e1.flags = 0 AND e2.flags = 0
                     ORDER BY r.from_id, r.to_id",
                ) && let Ok(rows) = stmt.query_map(params![fid], |row| {
                    Ok(Relation {
                        from: row.get(0)?,
                        to: row.get(1)?,
                        relation_type: row.get(2)?,
                    })
                }) {
                    for row in rows.flatten() {
                        triples.push(row);
                    }
                }
            }
            (None, Some(tid), None) => {
                if let Ok(mut stmt) = conn.prepare_cached(
                    "SELECT e1.name, e2.name, t.name
                     FROM relation r
                     JOIN entity e1 ON e1.id = r.from_id
                     JOIN entity e2 ON e2.id = r.to_id
                     JOIN type_dict t ON t.id = r.type_id
                     WHERE r.to_id = ?1
                       AND e1.flags = 0 AND e2.flags = 0
                     ORDER BY r.from_id, r.to_id",
                ) && let Ok(rows) = stmt.query_map(params![tid], |row| {
                    Ok(Relation {
                        from: row.get(0)?,
                        to: row.get(1)?,
                        relation_type: row.get(2)?,
                    })
                }) {
                    for row in rows.flatten() {
                        triples.push(row);
                    }
                }
            }
            (None, None, Some(tpid)) => {
                if let Ok(mut stmt) = conn.prepare_cached(
                    "SELECT e1.name, e2.name, t.name
                     FROM relation r
                     JOIN entity e1 ON e1.id = r.from_id
                     JOIN entity e2 ON e2.id = r.to_id
                     JOIN type_dict t ON t.id = r.type_id
                     WHERE r.type_id = ?1
                       AND e1.flags = 0 AND e2.flags = 0
                     ORDER BY r.from_id, r.to_id",
                ) && let Ok(rows) = stmt.query_map(params![tpid], |row| {
                    Ok(Relation {
                        from: row.get(0)?,
                        to: row.get(1)?,
                        relation_type: row.get(2)?,
                    })
                }) {
                    for row in rows.flatten() {
                        triples.push(row);
                    }
                }
            }
            (None, None, None) => {
                if let Ok(mut stmt) = conn.prepare_cached(
                    "SELECT e1.name, e2.name, t.name
                     FROM relation r
                     JOIN entity e1 ON e1.id = r.from_id
                     JOIN entity e2 ON e2.id = r.to_id
                     JOIN type_dict t ON t.id = r.type_id
                     WHERE e1.flags = 0 AND e2.flags = 0
                     ORDER BY r.from_id, r.to_id",
                ) && let Ok(rows) = stmt.query_map([], |row| {
                    Ok(Relation {
                        from: row.get(0)?,
                        to: row.get(1)?,
                        relation_type: row.get(2)?,
                    })
                }) {
                    for row in rows.flatten() {
                        triples.push(row);
                    }
                }
            }
        }
        if let Some(lim) = limit {
            triples.truncate(lim);
        }
        relation_details(&conn, triples.into_iter())
    }

    pub fn find_path(&self, from: &str, to: &str) -> Result<Option<Vec<String>>> {
        let conn = self.readers.get();
        let (from_id, _, _, _) = match self.get_entity_id(&conn, from)? {
            Some(v) => v,
            None => {
                return Err(MCSError::InvalidParams(format!(
                    "Source entity '{from}' not found"
                )));
            }
        };
        let (to_id, _, _, _) = match self.get_entity_id(&conn, to)? {
            Some(v) => v,
            None => {
                return Err(MCSError::InvalidParams(format!(
                    "Target entity '{to}' not found"
                )));
            }
        };

        if from_id == to_id {
            return Ok(Some(vec![from.to_string()]));
        }

        // BFS with adjacency from relation table.
        let mut visited = HashSet::new();
        let mut parent: FxHashMap<i64, i64> = FxHashMap::default();
        let mut queue = VecDeque::new();
        visited.insert(from_id);
        queue.push_back(from_id);

        while let Some(cur) = queue.pop_front() {
            if cur == to_id {
                break;
            }
            // Fetch out-neighbors.
            if let Ok(mut stmt) =
                conn.prepare_cached("SELECT to_id FROM relation WHERE from_id = ?1")
                && let Ok(rows) = stmt.query_map(params![cur], |row| row.get::<_, i64>(0))
            {
                for row in rows.flatten() {
                    if visited.insert(row) {
                        parent.insert(row, cur);
                        queue.push_back(row);
                    }
                }
            }
            // Also check in-neighbors (undirected traversal).
            if let Ok(mut stmt) =
                conn.prepare_cached("SELECT from_id FROM relation WHERE to_id = ?1")
                && let Ok(rows) = stmt.query_map(params![cur], |row| row.get::<_, i64>(0))
            {
                for row in rows.flatten() {
                    if visited.insert(row) {
                        parent.insert(row, cur);
                        queue.push_back(row);
                    }
                }
            }
        }

        if !parent.contains_key(&to_id) && to_id != from_id {
            return Ok(None);
        }

        let mut path = Vec::new();
        let mut cur = to_id;
        path.push(cur);
        while let Some(&p) = parent.get(&cur) {
            path.push(p);
            cur = p;
            if cur == from_id {
                break;
            }
        }
        path.reverse();

        let placeholders: Vec<String> = path.iter().map(|_| "?".to_string()).collect();
        let sql = format!(
            "SELECT id, name FROM entity WHERE id IN ({})",
            placeholders.join(",")
        );
        let name_map: FxHashMap<i64, String> = if let Ok(mut stmt) = conn.prepare(&sql)
            && let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(&path), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            }) {
            rows.flatten().collect()
        } else {
            FxHashMap::default()
        };

        let name_path: Vec<String> = path
            .iter()
            .filter_map(|id| name_map.get(id).cloned())
            .collect();

        Ok(Some(name_path))
    }

    pub fn compact(&self) -> Result<()> {
        self.mutate(MutationRequest::Compact).map(|_| ())
    }

    pub fn neighbors(
        &self,
        name: &str,
        direction: Direction,
        rtype: Option<&str>,
        depth: u32,
    ) -> Result<String> {
        self._traverse(name, direction, rtype, depth, true)
    }

    pub fn extract_subgraph(&self, names: &[String], depth: u32) -> Result<String> {
        if names.is_empty() {
            return Ok(r#"{"entities":[],"relations":[]}"#.to_string());
        }

        let conn = self.readers.get();
        let mut all_entity_ids: HashSet<i64> = HashSet::new();
        let mut frontier: HashSet<i64> = HashSet::new();
        let mut all_rel_pairs: HashSet<(i64, i64, i64)> = HashSet::new();

        // Resolve seed entities.
        for name in names {
            let h = name_hash(name);
            if let Ok(Some(id)) = conn
                .query_row(
                    "SELECT id FROM entity WHERE name_hash = ?1 AND name = ?2 AND flags = 0",
                    params![h, name],
                    |row| row.get::<_, i64>(0),
                )
                .map(Some)
                .or_else(|e| {
                    if is_not_found(&e) {
                        Ok(None)
                    } else {
                        Err(sqlite_err(e))
                    }
                })
            {
                all_entity_ids.insert(id);
                frontier.insert(id);
            }
        }

        let mut current_depth = 0u32;
        while current_depth < depth && !frontier.is_empty() {
            let mut next_frontier: HashSet<i64> = HashSet::new();

            // Collect relations for current frontier — batched IN queries
            // instead of one query per frontier entity.
            const CHUNK: usize = 500;
            let frontier_ids: Vec<i64> = frontier.iter().copied().collect();
            for chunk in frontier_ids.chunks(CHUNK) {
                let placeholders: Vec<String> = chunk.iter().map(|_| "?".to_string()).collect();
                let in_clause = placeholders.join(",");

                // Forward: from_id IN chunk.
                if let Ok(mut stmt) = conn.prepare(&format!(
                    "SELECT from_id, to_id, type_id FROM relation WHERE from_id IN ({in_clause})",
                )) && let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(chunk), |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                }) {
                    for row in rows.flatten() {
                        let (from_id, to_id, type_id) = row;
                        all_rel_pairs.insert((from_id, to_id, type_id));
                        if all_entity_ids.insert(to_id) {
                            next_frontier.insert(to_id);
                        }
                    }
                }

                // Backward: to_id IN chunk.
                if let Ok(mut stmt) = conn.prepare(&format!(
                    "SELECT from_id, to_id, type_id FROM relation WHERE to_id IN ({in_clause})",
                )) && let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(chunk), |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                }) {
                    for row in rows.flatten() {
                        let (from_id, to_id, type_id) = row;
                        all_rel_pairs.insert((from_id, to_id, type_id));
                        if all_entity_ids.insert(from_id) {
                            next_frontier.insert(from_id);
                        }
                    }
                }
            }
            if all_entity_ids.len() > MAX_TRAVERSAL_ENTITIES
                || all_rel_pairs.len() > MAX_TRAVERSAL_RELS
            {
                break;
            }
            frontier = next_frontier;
            current_depth += 1;
        }

        let entities_json: String = if all_entity_ids.is_empty() {
            "[]".to_string()
        } else {
            let ids: Vec<i64> = all_entity_ids.iter().copied().collect();
            let sql = format!(
                "SELECT COALESCE(json_group_array(json_object(
                    'name', e.name,
                    'entityType', t.name,
                    'observations', COALESCE((
                        SELECT json_group_array({OBSERVATION_JSON} ORDER BY o.idx, o.id)
                        FROM observation o WHERE o.entity_id = e.id
                    ), json('[]'))
                ) ORDER BY e.id), json('[]'))
                FROM entity e
                JOIN type_dict t ON t.id = e.type_id
                WHERE e.id IN ({}) AND e.flags = 0",
                int_csv(&ids)
            );
            conn.query_row(&sql, [], |row| row.get::<_, String>(0))
                .map_err(sqlite_err)?
        };

        let relations_json: String = if all_rel_pairs.is_empty() {
            "[]".to_string()
        } else {
            let sql = format!(
                "WITH r(from_id, to_id, type_id) AS (VALUES {})
                SELECT COALESCE(json_group_array(json_object(
                    'from', e1.name,
                    'to', e2.name,
                    'relationType', t.name
                )), json('[]'))
                FROM r
                JOIN entity e1 ON e1.id = r.from_id
                JOIN entity e2 ON e2.id = r.to_id
                JOIN type_dict t ON t.id = r.type_id
                WHERE e1.flags = 0 AND e2.flags = 0",
                rel_values_literal(&all_rel_pairs)
            );
            conn.query_row(&sql, [], |row| row.get::<_, String>(0))
                .map_err(sqlite_err)?
        };

        let mut out = String::with_capacity(32 + entities_json.len() + relations_json.len());
        out.push_str("{\"entities\":");
        out.push_str(&entities_json);
        out.push_str(",\"relations\":");
        out.push_str(&relations_json);
        out.push('}');
        Ok(out)
    }

    pub fn describe_entity(&self, name: &str) -> Result<EntityDescription> {
        let conn = self.readers.get();
        // Keep the entity, its observations, incident relations, and degree in
        // one WAL snapshot so a concurrent graph mutation cannot split this
        // public read model across commits.
        let tx = conn.unchecked_transaction().map_err(sqlite_err)?;
        let entity = crate::mutation::read_entity(&tx, name)?
            .ok_or_else(|| MCSError::InvalidParams(format!("Entity '{name}' not found")))?;
        let relations = crate::mutation::relations_for(&tx, name)?;
        let mut neighbors: Vec<String> = relations
            .iter()
            .map(|relation| {
                if relation.from == name {
                    relation.to.clone()
                } else {
                    relation.from.clone()
                }
            })
            .collect();
        neighbors.sort();
        neighbors.dedup();
        // The counters on `entity` are a denormalized cache. Derive the public
        // degree from this response's incident relations so legacy counter
        // drift cannot make one snapshot internally inconsistent.
        let incoming = relations
            .iter()
            .filter(|relation| relation.to == name)
            .count() as i64;
        let outgoing = relations
            .iter()
            .filter(|relation| relation.from == name)
            .count() as i64;
        let attributes = some_nonempty(attributes_for(&tx, "entity", entity.entity_id)?);
        tx.commit().map_err(sqlite_err)?;

        Ok(EntityDescription {
            name: entity.name,
            entity_type: entity.entity_type,
            observations: entity.observations,
            relations,
            neighbors,
            degree: Degree { incoming, outgoing },
            attributes,
        })
    }

    pub fn entity_type_counts(&self) -> Vec<(String, usize)> {
        let conn = self.readers.get();
        select_all_types(&conn, 0).unwrap_or_default()
    }

    /// `(name, count, desc)` for every registered entity type: one with
    /// members, or one whose description registers it before first use.
    pub fn entity_type_catalog(&self) -> Vec<(String, usize, Option<String>)> {
        let conn = self.readers.get();
        select_type_catalog(&conn, 0).unwrap_or_default()
    }

    /// The viewer's shared page metadata — entity-type legend and the graph-wide
    /// entity/relation totals — gathered on a *single* reader connection. The
    /// `/ui/graph` and `/ui/search` handlers used to take three or four separate
    /// reader-pool acquisitions per request (`entity_type_counts` +
    /// `get_entity_count` + `get_relation_count`); folding them into one lock
    /// acquisition cuts pool churn and shortens the reader hold, which is what
    /// bounds concurrent read throughput.
    pub fn ui_meta(&self) -> (Vec<(String, usize)>, usize, usize) {
        let conn = self.readers.get();
        let types = select_all_types(&conn, 0).unwrap_or_default();
        let entities = read_graph_stat(&conn, "entities").unwrap_or(0).max(0) as usize;
        let relations = read_graph_stat(&conn, "relations").unwrap_or(0).max(0) as usize;
        (types, entities, relations)
    }

    pub fn relation_type_counts(&self) -> Vec<(String, usize)> {
        let conn = self.readers.get();
        select_all_types(&conn, 1).unwrap_or_default()
    }

    /// `(name, count, desc)` for every registered relation type: one with
    /// members, or one whose description registers it before first use.
    pub fn relation_type_catalog(&self) -> Vec<(String, usize, Option<String>)> {
        let conn = self.readers.get();
        select_type_catalog(&conn, 1).unwrap_or_default()
    }

    /// Set or clear the description of one entity type (`kind` 0) or relation
    /// type (`kind` 1) by exact name. The type row is created when it does
    /// not exist yet — a description can thus register a type before its
    /// first member (`count` stays 0). `None` clears the stored description.
    /// This is registry metadata, not a graph mutation: no change event is
    /// emitted and the taxonomy index revision is untouched.
    pub fn set_type_description(
        &self,
        kind: i64,
        name: &str,
        description: Option<&str>,
    ) -> Result<()> {
        let conn = self.writer.lock();
        // The writer lock serializes, so select-then-insert is race-free —
        // the same pattern `mutation::type_id` uses for the insert path.
        let existing = lookup_type_id(&conn, name, kind);
        let description: Option<String> = description.map(|d| d.into());
        match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE type_dict SET desc = ?1 WHERE id = ?2",
                    params![description, id],
                )
                .map_err(sqlite_err)?;
            }
            None => {
                conn.execute(
                    "INSERT INTO type_dict(kind, name, count, desc) VALUES(?1, ?2, 0, ?3)",
                    params![kind, name, description],
                )
                .map_err(sqlite_err)?;
            }
        }
        Ok(())
    }

    /// Whether an entity type with the given name exists. Read-only: a missing
    /// type stays absent and no row is inserted.
    pub fn entity_type_exists(&self, name: &str) -> bool {
        let conn = self.readers.get();
        lookup_type_id(&conn, name, 0).is_some()
    }

    /// Whether a relation type with the given name exists. Read-only: a missing
    /// type stays absent and no row is inserted.
    pub fn relation_type_exists(&self, name: &str) -> bool {
        let conn = self.readers.get();
        lookup_type_id(&conn, name, 1).is_some()
    }

    pub fn batch_get_entities(&self, names: &[String]) -> Vec<Option<Entity>> {
        let conn = self.readers.get();
        // Resolve ids, load the full entities in one query, then attach every
        // resolved id's attributes from a single `attribute` lookup — no N+1.
        let ids: Vec<Option<i64>> = names
            .iter()
            .map(|n| entity_name_lookup(&conn, n).ok().flatten())
            .collect();
        let resolved: Vec<i64> = ids.iter().flatten().copied().collect();
        let mut by_id = batch_entities_by_ids(&conn, &resolved);
        let mut attrs: FxHashMap<i64, BTreeMap<String, String>> = FxHashMap::default();
        if !resolved.is_empty() {
            let sql = format!(
                "SELECT owner_id, key, value FROM attribute
                 WHERE owner_kind = 'entity' AND owner_id IN ({})
                 ORDER BY owner_id, key",
                int_csv(&resolved)
            );
            if let Ok(mut stmt) = conn.prepare(&sql)
                && let Ok(rows) = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
            {
                for row in rows.flatten() {
                    attrs.entry(row.0).or_default().insert(row.1, row.2);
                }
            }
        }
        names
            .iter()
            .enumerate()
            .map(|(i, _)| -> Option<Entity> {
                let Some(id) = ids[i] else {
                    return None;
                };
                let Some(mut entity) = by_id.remove(&id) else {
                    return None;
                };
                if let Some(map) = attrs.remove(&id) {
                    entity.attributes = some_nonempty(map);
                }
                Some(entity)
            })
            .collect()
    }

    pub fn find_all_paths(
        &self,
        from: &str,
        to: &str,
        max_depth: usize,
        max_paths: usize,
    ) -> Result<Vec<Vec<String>>> {
        let conn = self.readers.get();
        let (from_id, _, _, _) = match self.get_entity_id(&conn, from)? {
            Some(v) => v,
            None => {
                return Err(MCSError::InvalidParams(format!(
                    "Source entity '{from}' not found"
                )));
            }
        };
        let (to_id, _, _, _) = match self.get_entity_id(&conn, to)? {
            Some(v) => v,
            None => {
                return Err(MCSError::InvalidParams(format!(
                    "Target entity '{to}' not found"
                )));
            }
        };

        if from_id == to_id {
            return Ok(vec![vec![from.to_string()]]);
        }

        // BFS enumerating all paths up to max_depth.
        let mut all_paths: Vec<Vec<i64>> = Vec::new();
        let mut queue: VecDeque<(i64, Vec<i64>)> = VecDeque::new();
        queue.push_back((from_id, vec![from_id]));

        const MAX_QUEUE_SIZE: usize = 10_000_000;

        while let Some((cur, path)) = queue.pop_front() {
            if all_paths.len() >= max_paths {
                break;
            }
            if path.len() > max_depth {
                continue;
            }

            // Out-neighbors.
            if let Ok(mut stmt) =
                conn.prepare_cached("SELECT to_id FROM relation WHERE from_id = ?1")
                && let Ok(rows) = stmt.query_map(params![cur], |row| row.get::<_, i64>(0))
            {
                for next_id in rows.flatten() {
                    if next_id == to_id {
                        let mut full_path = path.clone();
                        full_path.push(next_id);
                        all_paths.push(full_path);
                        if all_paths.len() >= max_paths {
                            break;
                        }
                    } else if !path.contains(&next_id) && path.len() < max_depth {
                        if queue.len() >= MAX_QUEUE_SIZE {
                            return Err(MCSError::InvalidParams(
                                    "Path exploration queue exceeded limit (too many paths on highly connected graph)".to_string()
                                ));
                        }
                        let mut new_path = path.clone();
                        new_path.push(next_id);
                        queue.push_back((next_id, new_path));
                    }
                }
            }

            // In-neighbors (undirected).
            if let Ok(mut stmt) =
                conn.prepare_cached("SELECT from_id FROM relation WHERE to_id = ?1")
                && let Ok(rows) = stmt.query_map(params![cur], |row| row.get::<_, i64>(0))
            {
                for next_id in rows.flatten() {
                    if next_id == to_id {
                        let mut full_path = path.clone();
                        full_path.push(next_id);
                        all_paths.push(full_path);
                        if all_paths.len() >= max_paths {
                            break;
                        }
                    } else if !path.contains(&next_id) && path.len() < max_depth {
                        if queue.len() >= MAX_QUEUE_SIZE {
                            return Err(MCSError::InvalidParams(
                                    "Path exploration queue exceeded limit (too many paths on highly connected graph)".to_string()
                                ));
                        }
                        let mut new_path = path.clone();
                        new_path.push(next_id);
                        queue.push_back((next_id, new_path));
                    }
                }
            }
        }

        // Convert ids to names — one batch query instead of N lookups per path.
        let all_ids: HashSet<i64> = all_paths.iter().flat_map(|p| p.iter()).copied().collect();
        let id_list: Vec<i64> = all_ids.into_iter().collect();
        let name_map: FxHashMap<i64, String> = if id_list.is_empty() {
            FxHashMap::default()
        } else {
            let placeholders: Vec<String> = id_list.iter().map(|_| "?".to_string()).collect();
            let sql = format!(
                "SELECT id, name FROM entity WHERE id IN ({})",
                placeholders.join(",")
            );
            if let Ok(mut stmt) = conn.prepare(&sql)
                && let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(&id_list), |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
            {
                rows.flatten().collect()
            } else {
                FxHashMap::default()
            }
        };

        let mut named_paths: Vec<Vec<String>> = Vec::with_capacity(all_paths.len());
        for path_ids in all_paths {
            let named: Vec<String> = path_ids
                .iter()
                .filter_map(|id| name_map.get(id).cloned())
                .collect();
            named_paths.push(named);
        }

        Ok(named_paths)
    }

    /// Export the whole graph as a JSON string. `max_rows` caps both the entity
    /// and relation arrays so a pathologically large graph cannot be coerced
    /// into an unbounded in-memory string (DoS guard); callers pass a generous
    /// constant. A negative value means "no limit".
    pub fn export(&self, _format: &str, max_rows: i64) -> Result<String> {
        let conn = self.readers.get();
        // Only JSON is supported; the format argument is accepted for forward
        // compatibility.
        conn.query_row(
            &format!(
                "SELECT json_object(
                'entities', COALESCE((
                    SELECT json_group_array(json_object(
                        'name', e.name,
                        'entityType', t.name,
                        'observations', COALESCE((
                            SELECT json_group_array({OBSERVATION_JSON} ORDER BY o.idx, o.id)
                            FROM observation o WHERE o.entity_id = e.id
                        ), json('[]')),
                        'attributes', COALESCE((
                            SELECT json_group_object(key, value) FROM attribute
                            WHERE owner_kind = 'entity' AND owner_id = e.id
                        ), json('{{}}'))
                    ) ORDER BY e.id)
                    FROM (
                        SELECT id, name, type_id FROM entity
                        WHERE flags = 0 ORDER BY id LIMIT ?1
                    ) e
                    JOIN type_dict t ON t.id = e.type_id
                ), json('[]')),
                'relations', COALESCE((
                    SELECT json_group_array(json_object(
                        'from', e1.name,
                        'to', e2.name,
                        'relationType', t.name,
                        'observations', COALESCE((
                            SELECT json_group_array(json_object(
                                'body', ro.body,
                                'createdAtUs', ro.created_us,
                                'occurredAtUs', ro.occurred_us,
                                'originEntityName', NULL))
                            FROM relation_observation ro
                            JOIN taxonomy_relation m ON m.id = ro.relation_id AND m.deleted = 0
                            WHERE m.from_id = r.from_id AND m.to_id = r.to_id AND m.type_id = r.type_id
                            ORDER BY ro.idx
                        ), json('[]')),
                        'attributes', COALESCE((
                            SELECT json_group_object(key, value) FROM attribute
                            WHERE owner_kind = 'relation' AND owner_id = (
                                SELECT id FROM taxonomy_relation
                                WHERE from_id = r.from_id AND to_id = r.to_id
                                  AND type_id = r.type_id AND deleted = 0)
                        ), json('{{}}'))
                    ))
                    FROM (
                        SELECT from_id, to_id, type_id FROM relation LIMIT ?1
                    ) r
                    JOIN entity e1 ON e1.id = r.from_id
                    JOIN entity e2 ON e2.id = r.to_id
                    JOIN type_dict t ON t.id = r.type_id
                    WHERE e1.flags = 0 AND e2.flags = 0
                ), json('[]'))
            )"
            ),
            params![max_rows],
            |row| row.get::<_, String>(0),
        )
        .map_err(sqlite_err)
    }

    pub fn wipe(&self) -> Result<()> {
        self.mutate(MutationRequest::Wipe).map(|_| ())
    }

    /// Periodic database maintenance: WAL checkpoint, query planner analysis,
    /// and FTS index optimization. Call from a background timer.
    pub fn run_maintenance(&self) -> Result<()> {
        let conn = self.writer.lock();

        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(sqlite_err)?;

        conn.execute_batch("PRAGMA optimize(0x10000);")
            .map_err(sqlite_err)?;

        let tx = TxGuard::begin(&conn)?;
        conn.execute_batch(
            "INSERT INTO name_fts(name_fts) VALUES('optimize');
             INSERT INTO obs_fts(obs_fts) VALUES('optimize');
             INSERT INTO rel_obs_fts(rel_obs_fts) VALUES('optimize');",
        )
        .map_err(sqlite_err)?;
        tx.commit()?;

        Ok(())
    }

    /// Run a non-blocking `wal_checkpoint(PASSIVE)` to fsync committed WAL frames
    /// without stalling readers or writers. Call from a short-interval timer to
    /// bound the durability window in `async` mode.
    pub fn checkpoint_passive(&self) -> Result<()> {
        let conn = self.writer.lock();
        conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
            .map_err(sqlite_err)?;
        Ok(())
    }

    fn _traverse(
        &self,
        name: &str,
        direction: Direction,
        rtype: Option<&str>,
        depth: u32,
        // unused — we always include relations; the caller controls via depth
        _include_relations: bool,
    ) -> Result<String> {
        let conn = self.readers.get();
        let (start_id, _, _, _) = match self.get_entity_id(&conn, name)? {
            Some(v) => v,
            None => {
                return Err(MCSError::InvalidParams(format!(
                    "Entity '{name}' not found"
                )));
            }
        };

        let mut all_ids: HashSet<i64> = HashSet::new();
        let mut all_rels: HashSet<(i64, i64, i64)> = HashSet::new();
        let mut frontier: HashSet<i64> = HashSet::new();
        all_ids.insert(start_id);
        frontier.insert(start_id);

        // Read-only type resolution. A requested-but-missing type uses the
        // sentinel id -1 (matches no edge), so traversal yields just the start
        // entity instead of falling back to "no type filter" and walking every
        // edge. `get_type_id` is avoided here: it inserts and cannot run on the
        // `query_only` reader connection.
        let type_filter: Option<i64> = rtype
            .filter(|rt| !rt.is_empty())
            .map(|rt| lookup_type_id(&conn, rt, 1).unwrap_or(-1));

        // Pre-compile all four possible queries outside the loop.
        let mut q_out_t = conn.prepare_cached(
            "SELECT to_id, type_id FROM relation WHERE from_id = ?1 AND type_id = ?2",
        );
        let mut q_out =
            conn.prepare_cached("SELECT to_id, type_id FROM relation WHERE from_id = ?1");
        let mut q_in_t = conn.prepare_cached(
            "SELECT from_id, type_id FROM relation WHERE to_id = ?1 AND type_id = ?2",
        );
        let mut q_in =
            conn.prepare_cached("SELECT from_id, type_id FROM relation WHERE to_id = ?1");

        let mut cur_depth = 0u32;
        while cur_depth < depth && !frontier.is_empty() {
            let mut next_frontier: HashSet<i64> = HashSet::new();

            for &fid in &frontier {
                if direction == Direction::Outgoing || direction == Direction::Both {
                    if let Some(tid) = type_filter {
                        if let Ok(ref mut stmt) = q_out_t
                            && let Ok(rows) = stmt.query_map(params![fid, tid], |row| {
                                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                            })
                        {
                            for row in rows.flatten() {
                                let (to_id, t_id) = row;
                                all_rels.insert((fid, to_id, t_id));
                                if all_ids.insert(to_id) {
                                    next_frontier.insert(to_id);
                                }
                            }
                        }
                    } else if let Ok(ref mut stmt) = q_out
                        && let Ok(rows) = stmt.query_map(params![fid], |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                        })
                    {
                        for row in rows.flatten() {
                            let (to_id, t_id) = row;
                            all_rels.insert((fid, to_id, t_id));
                            if all_ids.insert(to_id) {
                                next_frontier.insert(to_id);
                            }
                        }
                    }
                }

                if direction == Direction::Incoming || direction == Direction::Both {
                    if let Some(tid) = type_filter {
                        if let Ok(ref mut stmt) = q_in_t
                            && let Ok(rows) = stmt.query_map(params![fid, tid], |row| {
                                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                            })
                        {
                            for row in rows.flatten() {
                                let (from_id, t_id) = row;
                                all_rels.insert((from_id, fid, t_id));
                                if all_ids.insert(from_id) {
                                    next_frontier.insert(from_id);
                                }
                            }
                        }
                    } else if let Ok(ref mut stmt) = q_in
                        && let Ok(rows) = stmt.query_map(params![fid], |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                        })
                    {
                        for row in rows.flatten() {
                            let (from_id, t_id) = row;
                            all_rels.insert((from_id, fid, t_id));
                            if all_ids.insert(from_id) {
                                next_frontier.insert(from_id);
                            }
                        }
                    }
                }
            }

            // DoS guard: stop traversal if we've collected too many entities
            // or relations. The response will be partial, which is preferable
            // to an OOM crash on densely connected graphs.
            if all_ids.len() > MAX_TRAVERSAL_ENTITIES || all_rels.len() > MAX_TRAVERSAL_RELS {
                break;
            }

            frontier = next_frontier;
            cur_depth += 1;
        }

        let entities_json: String = if all_ids.is_empty() {
            "[]".to_string()
        } else {
            let ids: Vec<i64> = all_ids.iter().copied().collect();
            let sql = format!(
                "SELECT COALESCE(json_group_array(json_object(
                    'name', e.name,
                    'entityType', t.name,
                    'observations', COALESCE((
                        SELECT json_group_array({OBSERVATION_JSON} ORDER BY o.idx, o.id)
                        FROM observation o WHERE o.entity_id = e.id
                    ), json('[]'))
                ) ORDER BY e.id), json('[]'))
                FROM entity e
                JOIN type_dict t ON t.id = e.type_id
                WHERE e.id IN ({}) AND e.flags = 0",
                int_csv(&ids)
            );
            conn.query_row(&sql, [], |row| row.get::<_, String>(0))
                .map_err(sqlite_err)?
        };

        let relations_json: String = if all_rels.is_empty() {
            "[]".to_string()
        } else {
            let sql = format!(
                "WITH r(from_id, to_id, type_id) AS (VALUES {})
                SELECT COALESCE(json_group_array(json_object(
                    'from', e1.name,
                    'to', e2.name,
                    'relationType', t.name
                )), json('[]'))
                FROM r
                JOIN entity e1 ON e1.id = r.from_id
                JOIN entity e2 ON e2.id = r.to_id
                JOIN type_dict t ON t.id = r.type_id
                WHERE e1.flags = 0 AND e2.flags = 0",
                rel_values_literal(&all_rels)
            );
            conn.query_row(&sql, [], |row| row.get::<_, String>(0))
                .map_err(sqlite_err)?
        };

        let mut out = String::with_capacity(32 + entities_json.len() + relations_json.len());
        out.push_str("{\"entities\":");
        out.push_str(&entities_json);
        out.push_str(",\"relations\":");
        out.push_str(&relations_json);
        out.push('}');
        Ok(out)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EntityInput as Entity;
    use serde_json::Value;
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
            cleanup_db(&self.1);
        }
    }

    fn cleanup_db(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    fn new_kg() -> TestKg {
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("kg_test_{}_{}.db", std::process::id(), n));
        cleanup_db(&path);
        let kg = GraphHandle::new(
            &path,
            Durability::Async,
            SqliteTuning::default(),
            NonZeroUsize::new(10000).unwrap(),
            4,
        )
        .expect("create KG");
        TestKg(kg, path)
    }

    #[test]
    fn test_create_and_get_entity() {
        let kg = new_kg();
        let entities = vec![Entity {
            name: "test".into(),
            entity_type: "person".into(),
            observations: vec!["obs1".into(), "obs2".into()],
            attributes: None,
        }];
        let created = kg.create_entities(&entities).unwrap();
        assert_eq!(created.len(), 1);

        let got = kg.get_entity("test").unwrap().unwrap();
        assert_eq!(got.name, "test");
        assert_eq!(got.entity_type, "person");
        assert_eq!(
            got.observations
                .iter()
                .map(|o| o.body.as_str())
                .collect::<Vec<_>>(),
            vec!["obs1", "obs2"]
        );
    }

    #[test]
    fn test_get_nonexistent() {
        let kg = new_kg();
        assert!(kg.get_entity("nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_delete_entity() {
        let kg = new_kg();
        kg.create_entities(&[Entity {
            name: "del".into(),
            entity_type: "t".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
        assert!(kg.get_entity("del").unwrap().is_some());
        kg.delete_entities(&["del".to_string()]).unwrap();
        assert!(kg.get_entity("del").unwrap().is_none());
    }

    #[test]
    fn test_add_and_delete_observations() {
        let kg = new_kg();
        kg.create_entities(&[Entity {
            name: "obs_test".into(),
            entity_type: "t".into(),
            observations: vec!["a".into()],
            attributes: None,
        }])
        .unwrap();

        let added = kg
            .add_observations("obs_test", &["b".into(), "c".into()])
            .unwrap();
        assert_eq!(added.len(), 2);

        let ent = kg.get_entity("obs_test").unwrap().unwrap();
        assert!(ent.observations.iter().any(|o| o.body == "b"));
        assert!(ent.observations.iter().any(|o| o.body == "c"));

        kg.delete_observations("obs_test", &["b".into()]).unwrap();
        let ent = kg.get_entity("obs_test").unwrap().unwrap();
        assert!(!ent.observations.iter().any(|o| o.body == "b"));
        assert!(ent.observations.iter().any(|o| o.body == "c"));
        assert!(ent.observations.iter().any(|o| o.body == "a"));
    }

    #[test]
    fn test_create_relations() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "A".into(),
                entity_type: "node".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "B".into(),
                entity_type: "node".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();

        let rels = kg.create_relations(&[rel_input("A", "B", "edge")]).unwrap();
        assert_eq!(rels.len(), 1);

        assert_eq!(kg.get_entity_count().unwrap(), 2);
        assert_eq!(kg.get_relation_count().unwrap(), 1);
    }

    #[test]
    fn test_search_nodes() {
        let kg = new_kg();
        kg.create_entities(&[Entity {
            name: "Einstein".into(),
            entity_type: "scientist".into(),
            observations: vec!["physics".into(), "relativity".into()],
            attributes: None,
        }])
        .unwrap();

        let results = kg.search_nodes_filtered("physics", None, 0, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Einstein");

        let results = kg.search_nodes_filtered("physics", Some("scientist"), 0, 10);
        assert_eq!(results.len(), 1);

        let results = kg.search_nodes_filtered("physics", Some("nonexistent"), 0, 10);
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_find_path() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "A".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "B".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "C".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();

        kg.create_relations(&[rel_input("A", "B", "e"), rel_input("B", "C", "e")])
            .unwrap();

        let path = kg.find_path("A", "C").unwrap().unwrap();
        assert_eq!(path, vec!["A", "B", "C"]);
    }

    #[test]
    fn test_degree() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "A".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "B".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "C".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();

        kg.create_relations(&[rel_input("A", "B", "e"), rel_input("A", "C", "e")])
            .unwrap();

        assert_eq!(kg.degree("A", Direction::Outgoing).unwrap(), 2);
        assert_eq!(kg.degree("A", Direction::Incoming).unwrap(), 0);
        assert_eq!(kg.degree("B", Direction::Incoming).unwrap(), 1);
    }

    #[test]
    fn test_neighbors() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "A".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "B".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();

        kg.create_relations(&[rel_input("A", "B", "e")]).unwrap();

        let result = kg.neighbors("A", Direction::Outgoing, None, 1).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["entities"].as_array().unwrap().len(), 2);
        assert_eq!(v["relations"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_open_nodes() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "X".into(),
                entity_type: "n".into(),
                observations: vec!["obs_x".into()],
                attributes: None,
            },
            Entity {
                name: "Y".into(),
                entity_type: "n".into(),
                observations: vec!["obs_y".into()],
                attributes: None,
            },
        ])
        .unwrap();

        kg.create_relations(&[rel_input("X", "Y", "e")]).unwrap();

        let result = kg.open_nodes(&["X".into()]);
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["entities"].as_array().unwrap().len(), 1);
        assert_eq!(v["relations"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_entities_exist() {
        let kg = new_kg();
        kg.create_entities(&[Entity {
            name: "exists".into(),
            entity_type: "t".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();

        let res = kg
            .entities_exist(&["exists".into(), "missing".into()])
            .unwrap();
        assert_eq!(res, vec![true, false]);
    }

    #[test]
    fn test_describe_entity() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "A".into(),
                entity_type: "t".into(),
                observations: vec!["o".into()],
                attributes: None,
            },
            Entity {
                name: "B".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "C".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();

        kg.create_relations(&[
            rel_input("B", "A", "inbound"),
            rel_input("A", "B", "outbound"),
            rel_input("A", "C", "other"),
            rel_input("A", "A", "self"),
        ])
        .unwrap();

        let entity = kg.describe_entity("A").unwrap();
        assert_eq!(entity.name, "A");
        assert_eq!(entity.entity_type, "t");
        assert_eq!(
            entity
                .observations
                .iter()
                .map(|o| o.body.as_str())
                .collect::<Vec<_>>(),
            ["o"]
        );
        assert_eq!(entity.relations.len(), 4);
        assert_eq!(
            entity.relations,
            vec![
                Relation {
                    from: "A".into(),
                    to: "A".into(),
                    relation_type: "self".into(),
                },
                Relation {
                    from: "A".into(),
                    to: "B".into(),
                    relation_type: "outbound".into(),
                },
                Relation {
                    from: "A".into(),
                    to: "C".into(),
                    relation_type: "other".into(),
                },
                Relation {
                    from: "B".into(),
                    to: "A".into(),
                    relation_type: "inbound".into(),
                },
            ]
        );
        assert_eq!(entity.neighbors, ["A", "B", "C"]);
        assert_eq!(entity.degree.incoming, 2);
        assert_eq!(entity.degree.outgoing, 3);

        // `out_deg` and `in_deg` are a denormalized legacy cache. The public
        // describe response must describe the returned relation set even when
        // a pre-existing database carries stale cache values.
        kg.writer
            .lock()
            .execute(
                "UPDATE entity SET out_deg = 99, in_deg = 88 WHERE name = 'A'",
                [],
            )
            .unwrap();
        let entity = kg.describe_entity("A").unwrap();
        assert_eq!(entity.degree.incoming, 2);
        assert_eq!(entity.degree.outgoing, 3);
        assert!(kg.describe_entity("missing").is_err());
    }

    #[test]
    fn test_entity_type_counts() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "a".into(),
                entity_type: "person".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "b".into(),
                entity_type: "person".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "c".into(),
                entity_type: "place".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();

        let counts = kg.entity_type_counts();
        let map: FxHashMap<_, _> = counts.into_iter().collect();
        assert_eq!(map.get("person"), Some(&2));
        assert_eq!(map.get("place"), Some(&1));
    }

    #[test]
    fn test_relation_type_counts() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "a".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "b".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "c".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();

        kg.create_relations(&[rel_input("a", "b", "knows"), rel_input("a", "c", "knows")])
            .unwrap();

        let counts = kg.relation_type_counts();
        let map: FxHashMap<_, _> = counts.into_iter().collect();
        assert_eq!(map.get("knows"), Some(&2));
    }

    #[test]
    fn test_upsert_entities() {
        let kg = new_kg();
        kg.create_entities(&[Entity {
            name: "A".into(),
            entity_type: "OldType".into(),
            observations: vec!["old".into()],
            attributes: None,
        }])
        .unwrap();
        kg.create_relations(&[rel_input("A", "A", "self")]).unwrap();

        // Upsert retypes an exact-name entity and only adds novel observations.
        kg.upsert_entities(&[Entity {
            name: "A".into(),
            entity_type: "NewType".into(),
            observations: vec!["old".into(), "new".into()],
            attributes: None,
        }])
        .unwrap();

        assert_eq!(kg.get_entity_count().unwrap(), 1);
        let ent = kg.get_entity("A").unwrap().unwrap();
        assert_eq!(ent.entity_type, "NewType");
        assert_eq!(
            ent.observations
                .iter()
                .map(|o| o.body.as_str())
                .collect::<Vec<_>>(),
            ["old", "new"]
        );

        let type_counts: FxHashMap<_, _> = kg.entity_type_counts().into_iter().collect();
        assert_eq!(type_counts.get("OldType"), None);
        assert_eq!(type_counts.get("NewType"), Some(&1));

        assert_eq!(
            kg.search_relations(Some("A"), Some("A"), Some("self"), None, None)
                .unwrap(),
            [RelationDetail {
                from: "A".into(),
                to: "A".into(),
                relation_type: "self".into(),
                observations: vec![],
                attributes: BTreeMap::new(),
            }]
        );
    }

    #[test]
    fn test_merge_entities() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "source".into(),
                entity_type: "t".into(),
                observations: vec!["src_obs".into()],
                attributes: None,
            },
            Entity {
                name: "target".into(),
                entity_type: "t".into(),
                observations: vec!["tgt_obs".into()],
                attributes: None,
            },
        ])
        .unwrap();

        kg.create_relations(&[rel_input("source", "target", "e")])
            .unwrap();

        let merged = kg.merge_entities("source", "target").unwrap();
        assert_eq!(merged.name, "target");
        assert!(kg.get_entity("source").unwrap().is_none());
    }

    #[test]
    fn test_find_all_paths() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "A".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "B".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "C".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();

        kg.create_relations(&[
            rel_input("A", "B", "e"),
            rel_input("B", "C", "e"),
            rel_input("A", "C", "e"),
        ])
        .unwrap();

        let paths = kg.find_all_paths("A", "C", 5, 10).unwrap();
        assert!(paths.len() >= 2);
    }

    #[test]
    fn test_batch_get_entities() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "a".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "b".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();

        let results = kg.batch_get_entities(&["a".into(), "missing".into(), "b".into()]);
        assert_eq!(results.len(), 3);
        assert!(results[0].is_some());
        assert!(results[1].is_none());
        assert!(results[2].is_some());
    }

    #[test]
    fn test_export_graph() {
        let kg = new_kg();
        kg.create_entities(&[Entity {
            name: "exp".into(),
            entity_type: "t".into(),
            observations: vec!["o".into()],
            attributes: None,
        }])
        .unwrap();

        let exported = kg.export("json", i64::MAX).unwrap();
        assert!(exported.contains("exp"));
        assert!(exported.contains("o"));
    }

    #[test]
    fn test_graph_stats() {
        let kg = new_kg();
        assert_eq!(kg.get_entity_count().unwrap(), 0);
        assert_eq!(kg.get_relation_count().unwrap(), 0);

        kg.create_entities(&[Entity {
            name: "s".into(),
            entity_type: "t".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();

        assert_eq!(kg.get_entity_count().unwrap(), 1);
    }

    #[test]
    fn test_read_graph_filtered() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "p1".into(),
                entity_type: "person".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "p2".into(),
                entity_type: "place".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();

        let out = kg.read_graph_filtered(Some("person"), 0, 10).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["entities"].as_array().unwrap().len(), 1);
        assert_eq!(v["entities"][0]["name"], "p1");
    }

    #[test]
    fn test_wipe() {
        let kg = new_kg();
        kg.create_entities(&[Entity {
            name: "w".into(),
            entity_type: "t".into(),
            observations: vec!["o".into()],
            attributes: None,
        }])
        .unwrap();
        assert_eq!(kg.get_entity_count().unwrap(), 1);

        kg.wipe().unwrap();
        assert_eq!(kg.get_entity_count().unwrap(), 0);
    }

    #[test]
    fn test_push_json_str() {
        let mut buf = String::new();
        push_json_str(&mut buf, "hello");
        assert_eq!(buf, "\"hello\"");
        let mut buf = String::new();
        push_json_str(&mut buf, "he\"llo");
        assert_eq!(buf, "\"he\\\"llo\"");
    }

    // ── create_entities edge cases ────────────────────────────────────

    #[test]
    fn test_create_entities_empty_input() {
        let kg = new_kg();
        let created = kg.create_entities(&[]).unwrap();
        assert!(created.is_empty());
    }

    #[test]
    fn test_create_entities_skip_empty_name() {
        let kg = new_kg();
        let created = kg
            .create_entities(&[Entity {
                name: "".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            }])
            .unwrap();
        assert!(created.is_empty());
        assert_eq!(kg.get_entity_count().unwrap(), 0);
    }

    #[test]
    fn test_create_entities_duplicate_names() {
        let kg = new_kg();
        let e = Entity {
            name: "dup".into(),
            entity_type: "t".into(),
            observations: vec!["obs".into()],
            attributes: None,
        };
        let first = kg.create_entities(std::slice::from_ref(&e)).unwrap();
        assert_eq!(first.len(), 1);
        let second = kg.create_entities(&[e]).unwrap();
        assert!(second.is_empty());
        assert_eq!(kg.get_entity_count().unwrap(), 1);
    }

    #[test]
    fn test_create_entities_partial_duplicates() {
        let kg = new_kg();
        let created = kg
            .create_entities(&[
                Entity {
                    name: "a".into(),
                    entity_type: "t".into(),
                    observations: vec![],
                    attributes: None,
                },
                Entity {
                    name: "b".into(),
                    entity_type: "t".into(),
                    observations: vec![],
                    attributes: None,
                },
            ])
            .unwrap();
        assert_eq!(created.len(), 2);

        let second = kg
            .create_entities(&[
                Entity {
                    name: "b".into(),
                    entity_type: "t".into(),
                    observations: vec![],
                    attributes: None,
                },
                Entity {
                    name: "c".into(),
                    entity_type: "t".into(),
                    observations: vec![],
                    attributes: None,
                },
            ])
            .unwrap();
        assert_eq!(second.len(), 1); // only c created
        assert_eq!(second[0].name, "c");
        assert_eq!(kg.get_entity_count().unwrap(), 3);
    }

    #[test]
    fn test_create_entities_mixed_empty_and_valid() {
        let kg = new_kg();
        let created = kg
            .create_entities(&[
                Entity {
                    name: "".into(),
                    entity_type: "t".into(),
                    observations: vec![],
                    attributes: None,
                },
                Entity {
                    name: "valid".into(),
                    entity_type: "t".into(),
                    observations: vec![],
                    attributes: None,
                },
                Entity {
                    name: "".into(),
                    entity_type: "t".into(),
                    observations: vec![],
                    attributes: None,
                },
            ])
            .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].name, "valid");
        assert_eq!(kg.get_entity_count().unwrap(), 1);
    }

    #[test]
    fn test_create_entities_same_name_in_batch() {
        let kg = new_kg();
        let created = kg
            .create_entities(&[
                Entity {
                    name: "dup_in_batch".into(),
                    entity_type: "t".into(),
                    observations: vec![],
                    attributes: None,
                },
                Entity {
                    name: "dup_in_batch".into(),
                    entity_type: "t".into(),
                    observations: vec![],
                    attributes: None,
                },
            ])
            .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(kg.get_entity_count().unwrap(), 1);
    }

    // ── create_relations edge cases ───────────────────────────────────

    #[test]
    fn test_create_relations_empty_input() {
        let kg = new_kg();
        let rels = kg.create_relations(&[]).unwrap();
        assert!(rels.is_empty());
    }

    #[test]
    fn test_create_relations_nonexistent_from() {
        let kg = new_kg();
        kg.create_entities(&[Entity {
            name: "B".into(),
            entity_type: "t".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();

        let rels = kg.create_relations(&[rel_input("A", "B", "e")]).unwrap();
        assert!(rels.is_empty());
        assert_eq!(kg.get_relation_count().unwrap(), 0);
    }

    #[test]
    fn test_create_relations_nonexistent_to() {
        let kg = new_kg();
        kg.create_entities(&[Entity {
            name: "A".into(),
            entity_type: "t".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();

        let rels = kg.create_relations(&[rel_input("A", "B", "e")]).unwrap();
        assert!(rels.is_empty());
        assert_eq!(kg.get_relation_count().unwrap(), 0);
    }

    #[test]
    fn test_create_relations_both_nonexistent() {
        let kg = new_kg();
        let rels = kg.create_relations(&[rel_input("A", "B", "e")]).unwrap();
        assert!(rels.is_empty());
    }

    #[test]
    fn test_create_relations_self_loop() {
        let kg = new_kg();
        kg.create_entities(&[Entity {
            name: "self".into(),
            entity_type: "t".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();

        let rels = kg
            .create_relations(&[rel_input("self", "self", "loop")])
            .unwrap();
        assert_eq!(rels.len(), 1);
        assert_eq!(kg.get_relation_count().unwrap(), 1);
        assert_eq!(kg.degree("self", Direction::Outgoing).unwrap(), 1);
        assert_eq!(kg.degree("self", Direction::Incoming).unwrap(), 1);
    }

    #[test]
    fn test_create_relations_duplicate() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "A".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "B".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();

        let r = RelationInput {
            from: "A".into(),
            to: "B".into(),
            relation_type: "e".into(),
            observations: vec![],
            attributes: None,
        };
        let first = kg.create_relations(std::slice::from_ref(&r)).unwrap();
        assert_eq!(first.len(), 1);

        let second = kg.create_relations(&[r]).unwrap();
        assert!(second.is_empty());
        assert_eq!(kg.get_relation_count().unwrap(), 1);
    }

    #[test]
    fn test_create_relations_new_type_auto_created() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "A".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "B".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();

        let rels = kg
            .create_relations(&[rel_input("A", "B", "brand_new_type")])
            .unwrap();
        assert_eq!(rels.len(), 1);

        let counts = kg.relation_type_counts();
        let map: FxHashMap<_, _> = counts.into_iter().collect();
        assert_eq!(map.get("brand_new_type"), Some(&1));
    }

    #[test]
    fn test_create_relations_degree_updates() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "A".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "B".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "C".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();

        kg.create_relations(&[rel_input("A", "B", "e"), rel_input("A", "C", "e")])
            .unwrap();

        assert_eq!(kg.degree("A", Direction::Outgoing).unwrap(), 2);
        assert_eq!(kg.degree("A", Direction::Incoming).unwrap(), 0);
        assert_eq!(kg.degree("B", Direction::Incoming).unwrap(), 1);
        assert_eq!(kg.degree("C", Direction::Incoming).unwrap(), 1);
        assert_eq!(kg.degree("A", Direction::Both).unwrap(), 2);
    }

    #[test]
    fn test_create_relations_delete_and_recreate() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "A".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "B".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();

        let r = Relation {
            from: "A".into(),
            to: "B".into(),
            relation_type: "e".into(),
        };
        let input = RelationInput {
            from: r.from.clone(),
            to: r.to.clone(),
            relation_type: r.relation_type.clone(),
            observations: vec![],
            attributes: None,
        };
        kg.create_relations(std::slice::from_ref(&input)).unwrap();
        assert_eq!(kg.get_relation_count().unwrap(), 1);

        kg.delete_relations(std::slice::from_ref(&r)).unwrap();
        assert_eq!(kg.get_relation_count().unwrap(), 0);

        // Recreate after delete
        let re = kg.create_relations(&[input]).unwrap();
        assert_eq!(re.len(), 1);
        assert_eq!(kg.get_relation_count().unwrap(), 1);
    }

    // ── Integration edge cases ────────────────────────────────────────

    #[test]
    fn test_create_entities_then_relations_then_delete_entity_with_relations() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "A".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "B".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();
        kg.create_relations(&[rel_input("A", "B", "e")]).unwrap();

        assert_eq!(kg.get_relation_count().unwrap(), 1);

        // Deleting entity A should also delete the relation
        kg.delete_entities(&["A".into()]).unwrap();
        assert!(kg.get_entity("A").unwrap().is_none());
        assert_eq!(kg.get_relation_count().unwrap(), 0);
    }

    #[test]
    fn test_graph_stats_after_entity_with_observations() {
        let kg = new_kg();
        kg.create_entities(&[Entity {
            name: "stat".into(),
            entity_type: "t".into(),
            observations: vec!["o1".into(), "o2".into(), "o3".into()],
            attributes: None,
        }])
        .unwrap();

        let ecount = kg.get_entity_count().unwrap();
        // graph_stat for observations is tracked but there's no public getter for it
        assert_eq!(ecount, 1);

        // delete reverts stats
        kg.delete_entities(&["stat".into()]).unwrap();
        assert_eq!(kg.get_entity_count().unwrap(), 0);
    }

    // ── Helpers for the fix-specific suites ────────────────────────────────

    fn new_kg_with_pool(read_pool_size: usize) -> TestKg {
        use std::sync::atomic::AtomicU64;
        static COUNTER: AtomicU64 = AtomicU64::new(1_000_000);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("kg_pool_{}_{}.db", std::process::id(), n));
        cleanup_db(&path);
        let kg = GraphHandle::new(
            &path,
            Durability::Async,
            SqliteTuning::default(),
            NonZeroUsize::new(10_000).unwrap(),
            read_pool_size,
        )
        .expect("create KG");
        TestKg(kg, path)
    }

    fn seed_line(kg: &GraphHandle, n: usize) {
        let entities: Vec<Entity> = (0..n)
            .map(|i| Entity {
                name: format!("n{i}"),
                entity_type: "node".into(),
                observations: vec![format!("obs of n{i}").into()],
                attributes: None,
            })
            .collect();
        kg.create_entities(&entities).unwrap();
        let rels: Vec<RelationInput> = (0..n.saturating_sub(1))
            .map(|i| RelationInput {
                from: format!("n{i}"),
                to: format!("n{}", i + 1),
                relation_type: "edge".into(),
                observations: vec![],
                attributes: None,
            })
            .collect();
        if !rels.is_empty() {
            kg.create_relations(&rels).unwrap();
        }
    }

    fn count_relations(graph_json: &str) -> usize {
        let v: Value = serde_json::from_str(graph_json).unwrap();
        v["relations"].as_array().unwrap().len()
    }

    fn count_entities(graph_json: &str) -> usize {
        let v: Value = serde_json::from_str(graph_json).unwrap();
        v["entities"].as_array().unwrap().len()
    }

    // ── Fix #1: reader pool / concurrency ──────────────────────────────────

    #[test]
    fn test_pool_size_one_still_works() {
        let kg = new_kg_with_pool(1);
        seed_line(&kg, 5);
        assert_eq!(kg.get_entity_count().unwrap(), 5);
        assert!(kg.get_entity("n2").unwrap().is_some());
        let g = kg.read_graph_filtered(None, 0, usize::MAX).unwrap();
        assert_eq!(count_entities(&g), 5);
    }

    #[test]
    fn test_reads_see_committed_writes() {
        // A read on a pool connection must observe a just-committed write made on
        // the writer connection (WAL visibility).
        let kg = new_kg_with_pool(4);
        kg.create_entities(&[Entity {
            name: "fresh".into(),
            entity_type: "t".into(),
            observations: vec!["v".into()],
            attributes: None,
        }])
        .unwrap();
        // get_entity goes through the reader pool.
        let got = kg.get_entity("fresh").unwrap().unwrap();
        assert_eq!(
            got.observations
                .iter()
                .map(|o| o.body.as_str())
                .collect::<Vec<_>>(),
            vec!["v"]
        );
    }

    #[test]
    fn test_concurrent_readers_consistent() {
        // Many readers hammering the pool while the writer mutates must never
        // panic, deadlock, or observe a torn graph. The final counts must match.
        let kg = new_kg_with_pool(4);
        seed_line(&kg, 50);

        std::thread::scope(|s| {
            // 8 reader threads.
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..200 {
                        let _ = kg.get_entity("n10");
                        let _ = kg.search_nodes_filtered("obs", None, 0, 10);
                        let _ = kg.read_graph_filtered(None, 0, 100);
                        let _ = kg.get_entity_count();
                        let _ = kg.neighbors("n10", Direction::Both, None, 2);
                    }
                });
            }
            // 1 writer thread adding more entities concurrently.
            s.spawn(|| {
                for i in 100..160 {
                    kg.create_entities(&[Entity {
                        name: format!("w{i}"),
                        entity_type: "node".into(),
                        observations: vec![format!("w obs {i}").into()],
                        attributes: None,
                    }])
                    .unwrap();
                }
            });
        });

        // 50 seeded + 60 written.
        assert_eq!(kg.get_entity_count().unwrap(), 110);
        assert!(kg.get_entity("w159").unwrap().is_some());
    }

    #[test]
    fn test_reader_pool_rejects_writes_internally() {
        // Sanity: query_only readers cannot mutate. We can't call a write through
        // the pool directly, but we can confirm a read method that *would* have
        // inserted (search_relations resolving a missing type) does not create a
        // phantom type — see the dedicated test below — and that reads under a
        // size-1 pool serialize correctly without deadlock.
        let kg = new_kg_with_pool(1);
        seed_line(&kg, 3);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..100 {
                        let _ = kg.read_graph_filtered(None, 0, 10);
                    }
                });
            }
        });
        assert_eq!(kg.get_entity_count().unwrap(), 3);
    }

    // ── Fix #6: read_graph relation scoping + export bound ─────────────────

    #[test]
    fn test_read_graph_relations_scoped_to_page() {
        let kg = new_kg_with_pool(2);
        // n0 -> n1 -> n2 -> n3 (4 entities, 3 edges).
        seed_line(&kg, 4);

        // Full page: all 3 edges present.
        let full = kg.read_graph_filtered(None, 0, usize::MAX).unwrap();
        assert_eq!(count_entities(&full), 4);
        assert_eq!(count_relations(&full), 3);

        // Page of only the first entity (n0): its only edge n0->n1 has an
        // endpoint (n1) outside the page, so no relations are returned.
        let page1 = kg.read_graph_filtered(None, 0, 1).unwrap();
        assert_eq!(count_entities(&page1), 1);
        assert_eq!(count_relations(&page1), 0);

        // Page of first two entities (n0, n1): edge n0->n1 fully inside, n1->n2
        // straddles the boundary and is excluded.
        let page2 = kg.read_graph_filtered(None, 0, 2).unwrap();
        assert_eq!(count_entities(&page2), 2);
        assert_eq!(count_relations(&page2), 1);
    }

    #[test]
    fn test_read_graph_pagination_offset() {
        let kg = new_kg_with_pool(2);
        seed_line(&kg, 5);
        let g = kg.read_graph_filtered(None, 2, 2).unwrap();
        assert_eq!(count_entities(&g), 2);
        // Entities are ordered by id; offset 2 skips n0, n1.
        assert!(!g.contains("\"n0\""));
        assert!(!g.contains("\"n1\""));
        assert!(g.contains("\"n2\""));
        assert!(g.contains("\"n3\""));
    }

    #[test]
    fn test_read_graph_empty() {
        let kg = new_kg_with_pool(2);
        let g = kg.read_graph_filtered(None, 0, usize::MAX).unwrap();
        assert_eq!(g, r#"{"entities":[],"relations":[]}"#);
    }

    #[test]
    fn test_read_graph_filtered_by_type() {
        let kg = new_kg_with_pool(2);
        kg.create_entities(&[
            Entity {
                name: "p1".into(),
                entity_type: "person".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "q1".into(),
                entity_type: "place".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "p2".into(),
                entity_type: "person".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();
        let g = kg
            .read_graph_filtered(Some("person"), 0, usize::MAX)
            .unwrap();
        assert_eq!(count_entities(&g), 2);
        assert!(g.contains("\"p1\""));
        assert!(g.contains("\"p2\""));
        assert!(!g.contains("\"q1\""));
    }

    #[test]
    fn test_export_respects_max_rows() {
        let kg = new_kg_with_pool(2);
        seed_line(&kg, 5);

        // Unbounded export returns everything.
        let full = kg.export("json", i64::MAX).unwrap();
        assert_eq!(count_entities(&full), 5);
        assert_eq!(count_relations(&full), 4);

        // Capped export truncates both arrays to the cap.
        let capped = kg.export("json", 2).unwrap();
        assert_eq!(count_entities(&capped), 2);
        assert_eq!(count_relations(&capped), 2);
    }

    #[test]
    fn test_export_negative_max_rows_is_unbounded() {
        let kg = new_kg_with_pool(2);
        seed_line(&kg, 3);
        // SQLite treats a negative LIMIT as "no limit".
        let out = kg.export("json", -1).unwrap();
        assert_eq!(count_entities(&out), 3);
    }

    // ── Fix #8: writes remain correct without the per-write PRAGMA optimize ─

    #[test]
    fn test_many_small_write_batches_stay_consistent() {
        let kg = new_kg_with_pool(2);
        for i in 0..100 {
            kg.create_entities(&[Entity {
                name: format!("e{i}"),
                entity_type: "t".into(),
                observations: vec![format!("o{i}").into()],
                attributes: None,
            }])
            .unwrap();
        }
        assert_eq!(kg.get_entity_count().unwrap(), 100);
        // Search must still find a needle inserted across many tiny batches,
        // proving FTS stayed consistent without per-write optimization.
        let hits = kg.search_nodes_filtered("e57", None, 0, 10);
        assert!(hits.iter().any(|e| e.name == "e57"));
    }

    // ── Fix #9: wipe fully resets the FTS indexes ──────────────────────────

    #[test]
    fn test_wipe_clears_name_and_obs_fts() {
        let kg = new_kg_with_pool(2);
        kg.create_entities(&[Entity {
            name: "Einstein".into(),
            entity_type: "scientist".into(),
            observations: vec!["physics".into()],
            attributes: None,
        }])
        .unwrap();

        // Both FTS indexes resolve before the wipe.
        assert_eq!(kg.search_nodes_filtered("Einstein", None, 0, 10).len(), 1);
        assert_eq!(kg.search_nodes_filtered("physics", None, 0, 10).len(), 1);

        kg.wipe().unwrap();

        // After wipe both indexes must be empty (a bare DELETE on an
        // external-content FTS5 table would have left stale rowids behind).
        assert_eq!(kg.get_entity_count().unwrap(), 0);
        assert!(kg.search_nodes_filtered("Einstein", None, 0, 10).is_empty());
        assert!(kg.search_nodes_filtered("physics", None, 0, 10).is_empty());
    }

    #[test]
    fn test_wipe_then_recreate_search_works() {
        // Recreating the same names after a wipe must produce a clean, searchable
        // index — not a corrupted one or duplicate FTS rows.
        let kg = new_kg_with_pool(2);
        kg.create_entities(&[Entity {
            name: "Einstein".into(),
            entity_type: "scientist".into(),
            observations: vec!["physics".into()],
            attributes: None,
        }])
        .unwrap();
        kg.wipe().unwrap();

        kg.create_entities(&[Entity {
            name: "Einstein".into(),
            entity_type: "scientist".into(),
            observations: vec!["physics".into(), "relativity".into()],
            attributes: None,
        }])
        .unwrap();

        let by_name = kg.search_nodes_filtered("Einstein", None, 0, 10);
        assert_eq!(by_name.len(), 1, "exactly one Einstein after recreate");
        let by_obs = kg.search_nodes_filtered("relativity", None, 0, 10);
        assert_eq!(by_obs.len(), 1);
        assert_eq!(kg.get_entity_count().unwrap(), 1);
    }

    // ── Read-only type/entity resolution (introduced by the reader pool) ───

    #[test]
    fn test_search_relations_missing_type_returns_empty() {
        let kg = new_kg_with_pool(2);
        seed_line(&kg, 3); // edges of type "edge"
        // A filter for a relation type that does not exist must return nothing,
        // not every relation — and must not create a phantom type row.
        let r = kg
            .search_relations(None, None, Some("does_not_exist"), None, None)
            .unwrap();
        assert!(r.is_empty());
        // The phantom type must not have been inserted by the read.
        let types = kg.relation_type_counts();
        assert!(types.iter().all(|(t, _)| t != "does_not_exist"));
    }

    #[test]
    fn test_entity_type_exists() {
        let kg = new_kg_with_pool(2);
        kg.create_entities(&[Entity {
            name: "a".into(),
            entity_type: "person".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
        assert!(kg.entity_type_exists("person"));
        assert!(!kg.entity_type_exists("persn"));
        // The negative read must not have inserted a phantom type row.
        let types = kg.entity_type_counts();
        assert!(types.iter().all(|(t, _)| t != "persn"));
    }

    #[test]
    fn test_type_descriptions_registry_and_catalog() {
        let kg = new_kg_with_pool(2);
        // A description registers a type before any member exists.
        kg.set_type_description(0, "person", Some("A human being or persona"))
            .unwrap();
        let catalog = kg.entity_type_catalog();
        assert_eq!(
            catalog,
            vec![(
                "person".into(),
                0usize,
                Some("A human being or persona".into())
            )]
        );
        // The count-only view and existence follow the same row.
        assert!(kg.entity_type_exists("person"));
        assert!(
            kg.entity_type_counts().is_empty(),
            "count 0 types stay out of counts"
        );

        // Setting again updates in place, keeps count 0 while unused.
        kg.set_type_description(0, "person", Some("A living human"))
            .unwrap();
        let catalog = kg.entity_type_catalog();
        assert_eq!(
            catalog,
            vec![("person".into(), 0usize, Some("A living human".into()))]
        );

        // Clearing removes the description. The described row loses its desc,
        // and with count still 0 it leaves the catalog (no member, no desc).
        kg.set_type_description(0, "person", None).unwrap();
        assert!(kg.entity_type_catalog().is_empty());
        assert!(
            kg.entity_type_exists("person"),
            "the row stays; only the desc clears"
        );

        // A member makes the type count 1; the desc rides along and the type
        // stays in the catalog even with the desc cleared.
        kg.create_entities(&[Entity {
            name: "alice".into(),
            entity_type: "person".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
        assert_eq!(
            kg.entity_type_catalog(),
            vec![("person".into(), 1usize, None)]
        );
        assert_eq!(kg.entity_type_counts(), vec![("person".into(), 1usize)]);

        // Relation kinds take the same path.
        kg.set_type_description(1, "works_at", Some("Employment link"))
            .unwrap();
        assert_eq!(
            kg.relation_type_catalog(),
            vec![("works_at".into(), 0usize, Some("Employment link".into()))]
        );
        assert!(kg.relation_type_exists("works_at"));
    }

    #[test]
    fn test_type_description_length_not_capped_at_core() {
        // The server layer enforces the byte cap; the core accepts any text.
        let kg = new_kg_with_pool(2);
        let long = "x".repeat(20_000);
        kg.set_type_description(0, "person", Some(long.as_str()))
            .unwrap();
        assert_eq!(kg.entity_type_catalog().len(), 1);
    }

    #[test]
    fn test_relation_type_exists() {
        let kg = new_kg_with_pool(2);
        kg.create_entities(&[
            Entity {
                name: "a".into(),
                entity_type: "person".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "b".into(),
                entity_type: "person".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();
        kg.create_relations(&[rel_input("a", "b", "knows")])
            .unwrap();
        assert!(kg.relation_type_exists("knows"));
        assert!(!kg.relation_type_exists("unknown_kind"));
        // The negative read must not have inserted a phantom type row.
        let types = kg.relation_type_counts();
        assert!(types.iter().all(|(t, _)| t != "unknown_kind"));
    }

    #[test]
    fn test_search_relations_missing_from_returns_empty() {
        let kg = new_kg_with_pool(2);
        seed_line(&kg, 3);
        let r = kg
            .search_relations(Some("ghost"), None, None, None, None)
            .unwrap();
        assert!(r.is_empty(), "missing 'from' must not match every relation");
    }

    #[test]
    fn test_search_relations_existing_filters_still_work() {
        let kg = new_kg_with_pool(2);
        seed_line(&kg, 3);
        let r = kg
            .search_relations(Some("n0"), None, Some("edge"), None, None)
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].from, "n0");
        assert_eq!(r[0].to, "n1");
    }

    #[test]
    fn test_neighbors_missing_type_returns_only_start() {
        let kg = new_kg_with_pool(2);
        seed_line(&kg, 3);
        let json = kg
            .neighbors("n0", Direction::Both, Some("nonexistent"), 2)
            .unwrap();
        // No edge matches the bogus type, so only the start node comes back.
        assert_eq!(count_entities(&json), 1);
        assert_eq!(count_relations(&json), 0);
    }

    #[test]
    fn test_neighbors_existing_type_filters() {
        let kg = new_kg_with_pool(2);
        kg.create_entities(&[
            Entity {
                name: "a".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "b".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "c".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();
        kg.create_relations(&[rel_input("a", "b", "knows"), rel_input("a", "c", "likes")])
            .unwrap();
        let json = kg
            .neighbors("a", Direction::Outgoing, Some("knows"), 1)
            .unwrap();
        assert!(json.contains("\"b\""));
        assert!(!json.contains("\"c\""));
        assert_eq!(count_relations(&json), 1);
    }

    #[test]
    fn test_sqlite_tuning_applied_to_fresh_db() {
        use std::sync::atomic::AtomicU64;
        static COUNTER: AtomicU64 = AtomicU64::new(2_000_000);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("kg_tuning_{}_{}.db", std::process::id(), n));
        cleanup_db(&path);

        let tuning = SqliteTuning {
            page_size: 8192,
            ..SqliteTuning::default()
        };
        let kg = TestKg(
            GraphHandle::new(
                &path,
                Durability::Async,
                tuning,
                NonZeroUsize::new(64).unwrap(),
                2,
            )
            .expect("create KG"),
            path.clone(),
        );
        kg.create_entities(&[Entity {
            name: "a".into(),
            entity_type: "n".into(),
            observations: vec!["o".into()],
            attributes: None,
        }])
        .unwrap();

        // page_size (fresh-DB only) and auto_vacuum=INCREMENTAL must have taken
        // effect, and journal_mode must be WAL.
        let probe = Connection::open(&path).unwrap();
        let page_size: i64 = probe
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .unwrap();
        assert_eq!(page_size, 8192);
        let auto_vacuum: i64 = probe
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .unwrap();
        assert_eq!(auto_vacuum, 2, "expected INCREMENTAL auto_vacuum");
        let journal: String = probe
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal.to_lowercase(), "wal");
    }

    #[test]
    fn test_checkpoint_passive_is_noop_safe() {
        let kg = new_kg();
        // On an empty / freshly-written DB a passive checkpoint must succeed.
        kg.checkpoint_passive().unwrap();
        kg.create_entities(&[Entity {
            name: "a".into(),
            entity_type: "n".into(),
            observations: vec!["o".into()],
            attributes: None,
        }])
        .unwrap();
        // And after a write, repeatedly, without error or deadlock.
        kg.checkpoint_passive().unwrap();
        kg.checkpoint_passive().unwrap();
        // Data is still readable afterwards.
        assert!(kg.get_entity("a").unwrap().is_some());
    }

    // ── Relation observations and attributes (wave 1 core) ────────────────

    fn rel_input(from: &str, to: &str, relation_type: &str) -> RelationInput {
        RelationInput {
            from: from.into(),
            to: to.into(),
            relation_type: relation_type.into(),
            observations: vec![],
            attributes: None,
        }
    }

    fn seed_relation_obs_attrs(kg: &GraphHandle) -> i64 {
        kg.create_entities(&[
            Entity {
                name: "a".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "b".into(),
                entity_type: "n".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();
        kg.create_relations(&[RelationInput {
            from: "a".into(),
            to: "b".into(),
            relation_type: "uses".into(),
            observations: vec!["contract #12".into(), "legacy".into()],
            attributes: Some(std::collections::BTreeMap::from([("k".into(), "v".into())])),
        }])
        .unwrap();
        let conn = kg.writer.lock();
        conn.query_row(
            "SELECT m.id FROM taxonomy_relation m
             JOIN entity f ON f.id = m.from_id AND f.name = 'a'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn get_entity_and_describe_include_attributes() {
        let kg = new_kg();
        kg.create_entities(&[Entity {
            name: "a".into(),
            entity_type: "t".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
        let conn = kg.writer.lock();
        let id: i64 = conn
            .query_row("SELECT id FROM entity WHERE name='a'", [], |row| row.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO attribute(owner_kind,owner_id,key,value,created_us,updated_us)
             VALUES('entity',?1,'k','v',1,1)",
            [id],
        )
        .unwrap();
        drop(conn);

        let got = kg.get_entity("a").unwrap().unwrap();
        assert_eq!(
            got.attributes,
            Some(std::collections::BTreeMap::from([("k".into(), "v".into())]))
        );
        let described = kg.describe_entity("a").unwrap();
        assert_eq!(
            described.attributes,
            Some(std::collections::BTreeMap::from([("k".into(), "v".into())]))
        );

        // Attribute-less entities canonicalize to None on the wire.
        kg.create_entities(&[Entity {
            name: "plain".into(),
            entity_type: "t".into(),
            observations: vec![],
            attributes: None,
        }])
        .unwrap();
        assert_eq!(kg.get_entity("plain").unwrap().unwrap().attributes, None);
        assert_eq!(kg.describe_entity("plain").unwrap().attributes, None);
    }

    #[test]
    fn batch_get_entities_include_attributes() {
        let kg = new_kg();
        kg.create_entities(&[
            Entity {
                name: "a".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
            Entity {
                name: "b".into(),
                entity_type: "t".into(),
                observations: vec![],
                attributes: None,
            },
        ])
        .unwrap();
        let conn = kg.writer.lock();
        conn.execute(
            "INSERT INTO attribute(owner_kind,owner_id,key,value,created_us,updated_us)
             SELECT 'entity', e.id, 'k', 'v', 1, 1 FROM entity e WHERE e.name='a'",
            [],
        )
        .unwrap();
        drop(conn);

        let results = kg.batch_get_entities(&["a".into(), "missing".into(), "b".into()]);
        assert_eq!(results.len(), 3);
        assert_eq!(
            results[0].as_ref().unwrap().attributes,
            Some(std::collections::BTreeMap::from([("k".into(), "v".into())]))
        );
        assert!(results[1].is_none());
        assert_eq!(results[2].as_ref().unwrap().attributes, None);
    }

    #[test]
    fn search_relations_query_matches_relation_observation_bodies() {
        let kg = new_kg();
        seed_relation_obs_attrs(&kg);

        // Query mode returns owner-level detail rows in rank order.
        let detail = kg
            .search_relations(None, None, None, Some("contract"), Some(10usize))
            .unwrap();
        assert_eq!(detail.len(), 1);
        assert_eq!(detail[0].from, "a");
        assert_eq!(detail[0].to, "b");
        assert_eq!(detail[0].relation_type, "uses");
        assert_eq!(
            detail[0]
                .observations
                .iter()
                .map(|o| o.body.as_str())
                .collect::<Vec<_>>(),
            ["contract #12", "legacy"],
            "owner-level detail carries the full observation list, matched body first"
        );
        assert_eq!(
            detail[0].attributes,
            std::collections::BTreeMap::from([("k".into(), "v".into())])
        );

        // Query mode composes with structural filters.
        let filtered = kg
            .search_relations(
                Some("a"),
                Some("b"),
                Some("uses"),
                Some("legacy"),
                Some(10usize),
            )
            .unwrap();
        assert_eq!(filtered.len(), 1);
        let no_match = kg
            .search_relations(Some("ghost"), None, None, Some("contract"), Some(10usize))
            .unwrap();
        assert!(no_match.is_empty(), "missing entity filter yields nothing");

        // Without a query every relation row carries always-present detail.
        let all = kg
            .search_relations(None, None, None, None, Some(10usize))
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(
            all[0]
                .observations
                .iter()
                .map(|o| o.body.as_str())
                .collect::<Vec<_>>(),
            ["contract #12", "legacy"],
            "observations always present, ordered by idx"
        );
        assert_eq!(
            all[0].attributes,
            std::collections::BTreeMap::from([("k".into(), "v".into())]),
            "attributes always present"
        );
    }

    #[test]
    fn export_contains_relation_observations_and_attributes() {
        let kg = new_kg();
        seed_relation_obs_attrs(&kg);
        // Entity attribute rows ride along in export too.
        kg.set_attributes(&[AttributeSet {
            owner_kind: "entity".into(),
            entity_name: Some("a".into()),
            from: None,
            to: None,
            relation_type: None,
            attributes: std::collections::BTreeMap::from([("ea".into(), "ev".into())]),
        }])
        .unwrap();

        let exported = kg.export("json", 100).unwrap();
        assert!(exported.contains("\"contract #12\""), "observation body");
        assert!(exported.contains("\"legacy\""), "second observation body");
        assert!(exported.contains("\"k\":\"v\""), "relation attribute map");
        assert!(exported.contains("\"ea\":\"ev\""), "entity attribute map");
    }

    #[test]
    fn wipe_clears_relation_observations_attributes_and_fts() {
        let kg = new_kg();
        seed_relation_obs_attrs(&kg);
        kg.set_attributes(&[AttributeSet {
            owner_kind: "entity".into(),
            entity_name: Some("a".into()),
            from: None,
            to: None,
            relation_type: None,
            attributes: std::collections::BTreeMap::from([("ea".into(), "ev".into())]),
        }])
        .unwrap();
        let conn = kg.writer.lock();
        let obs_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rel_obs_fts WHERE rel_obs_fts MATCH 'contract'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(obs_hits, 1, "non-empty rel_obs_fts fixture");
        drop(conn);

        kg.wipe().unwrap();

        let conn = kg.writer.lock();
        for (table, field) in [
            ("relation_observation", "relation_id"),
            ("attribute", "owner_id"),
        ] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} must be empty after wipe");
            let _ = field;
        }
        let fts_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rel_obs_fts WHERE rel_obs_fts MATCH 'contract'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(fts_hits, 0, "rel_obs_fts must not retain postings");
    }
}
