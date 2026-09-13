# Chunked Embeddings for Entities and Relations — Design

Date: 2026-09-13. Status: approved for spec review.

## 1. Problem

One vector represents one entity today. The vector text combines the name,
the type, and every observation body in one string
(`crates/mcpmem-indexer/src/provider.rs:14-20`). Long observation sets
dilute the meaning of the vector. Relations have embeddings only in the
taxonomy corner: a kind-2 mirror and job stream feed `suggest_taxonomy`,
so relation matches never reach the main search surface, and the entity
vector cannot find a relation.

## 2. Goals

- One identity vector per entity. The identity text is `name \n type`.
- One vector per observation. The text is the observation body.
- One vector per relation. The text is the full triple
  `fromName \n relationType \n toName`.
- Search results stay owner-level. The best chunk decides the score of an
  owner.
- Searches filter by owner kind and by type name.
- The server owns every embedding. Client-supplied embedding tools
  disappear.
- Relations join semantic search as kind-marked result rows. `chunk_vector`
  is the single relation embedding store; the taxonomy kind-2 snapshot
  derives from it.
- `vector_search_by_entity` stays, re-targeted at the identity chunk.

## 3. Non-goals

- The code-index subsystem (`--enable-code`, `code_embed`,
  `code_semantic_search`) keeps its own client-supplied embeddings and its
  own HNSW index. This change does not touch that subsystem.
- We store no chunk text beside the vector. Chunk text reassembles from
  SQL at query time.
- We add no per-chunk permissions.

## 4. Approved decisions

| Decision | Choice |
|---|---|
| Chunk layout, entity | Identity chunk (name + type), one chunk per observation |
| Chunk layout, relation | One chunk per relation, the full triple |
| Result shape | Owner-level by default, opt-in chunk detail (`includeChunks`) |
| Relation surface | Merged into semantic/hybrid search, kind-marked rows |
| Filtering | `filter: {kind, type}` on the search tools |
| Embedding ownership | Server-only. Client upsert tools are removed |
| Legacy rows | Dropped in the migration, full reindex from zero |
| Read tools | `vector_get_embedding`, `vector_recommend` removed; `vector_search_by_entity` stays on the identity chunk |
| Relation store | `chunk_vector` is the single store; the taxonomy kind-2 snapshot derives from it |
| ANN for the knowledge graph | Entity ANN layers removed (no path feeds them) |

## 5. Data model

Migration `0002_chunked_embeddings.sql`:

1. `profile_vector` becomes `chunk_vector`:
   - Primary key `(profile_id, kind, owner_kind, owner_id, chunk_index)`.
   - `kind` is `identity`, `observation`, or `relation`.
   - `owner_kind` is `entity` or `relation`.
   - `owner_id` is `entity.id` or `taxonomy_relation.id`. The mirror id is
     unique per triple and stable across deletes and recreates, so it is a
     sound owner identity. The `relation` table needs no new id column.
   - `type_id` references `type_dict` (kinds 0 and 1 share one table).
     Every chunk of an owner carries the owner type. This makes type
     filtering an exact predicate over chunk rows.
   - `owner_revision` fences the write. For an entity this is
     `entity_revision.revision`. For a relation this is
     `taxonomy_relation.revision`.
   - `blob`, `created_at_us`, `source` stay as today.
2. `index_job` becomes `chunk_index_job` with the same owner
   generalization. The lease machinery (`lease_token`, `lease_epoch`,
   `lease_until_us`, `attempts`, `state`) stays. The kind-2 taxonomy job
   stream retires: relation jobs enqueue into `chunk_index_job` through
   the same mutation funnels that keep the mirror fresh.
3. `taxonomy_vector` keeps kinds 0 and 1 (entity type, relation type).
   Kind 2 rows (relation instances) are dropped. The taxonomy kind-2
   snapshot derives from `chunk_vector` rows with `kind = 'relation'`.
4. `vector_embedding` is dropped, including its rows.
5. `profile.representation_version` becomes
   `chunks-identity+obs+relation-v2`.

`type_dict` already stores both kinds in one table. The entity type has
`kind=0`, the relation type has `kind=1`.

## 6. Worker

The worker canonicalizes each owner into chunks, then embeds them.

- Entity identity chunk text: `name \n type`.
- Observation chunk i text: `observation[i].body`. The index is
  `observation.idx`.
- Relation chunk text: `fromName \n relationType \n toName`.

The worker sends all chunk texts of one owner in ONE `embed_texts` call.
The vector count must equal the chunk count. A mismatch is an error, like
the single-vector check today (`lib.rs:310-312`).

The L2 normalization option applies to each vector as today.

The fenced commit stays one transaction:

- Re-check the lease, the epoch, the owner revision, and the deleted flag.
- Delete the old chunk rows of the owner.
- Insert the new chunk rows.
- Bump `durable_generation`. Null `full_scan_generation`.
- A relation commit also advances the taxonomy kind-2 generation, so the
  derived snapshot refreshes.

Enqueue:

- Entities enqueue as today (`persist_changes`).
- Relations enqueue from the two funnels that already maintain the
  mirror: `create_relation` (upsert) and `tombstone_relation_mirror`
  (delete, including the entity-delete cascade). Merge re-points through
  `create_relation`, so it needs no new hook. The mirror revision fences
  each relation job.

## 7. Query path

- Search scans the managed snapshot (the only serving path, as verified
  in `src/vector_store.rs:839-868`).
- An optional predicate filters chunk rows by `kind` and by `type_id`.
- The scan keeps the best K chunks by distance.
- Chunks aggregate per owner. The best chunk wins.
- Row resolution: an entity row shows `name` and `entity_type`. A relation
  row shows `from -> TYPE -> to` and the relation type, marked
  `kind: "relation"`.
- With `includeChunks: true`, each result carries its best chunk: kind,
  reassembled text, and score.
- The centrality boost applies to entity rows only. Relation rows get no
  boost in this change.
- The FTS half of `hybrid_search` matches entities only. Relation rows
  enter the fusion with their vector rank only.

## 8. Tool surface

Removed:

- `vector_upsert_embedding`
- `vector_delete_embedding`
- `vector_batch_upsert`
- `vector_get_embedding`
- `vector_recommend`
- `vector_reindex`

Changed:

- `vector_search_entities` gains `filter` and `includeChunks`. The
  `entityType` parameter is replaced by `filter.type`.
- `hybrid_search` gains `filter` and `includeChunks`.
- `semantic_search` gains `filter` and `includeChunks`.
- `vector_search_by_entity` stays. Its query vector is the identity
  chunk of the named entity. It gains `filter` and `includeChunks`.
- Result rows carry `kind` (`entity` or `relation`).
- `vector_store_stats` reports `embeddingCount` as chunk rows.

Kept unchanged:

- `vector_mmr_search`
- `vector_refresh_graph_cache`

`filter` shape:

```json
{ "kind": "entity", "type": "Person" }
```

Both fields optional. `kind` limits the owner kind. `type` matches the
chunk type name in `type_dict`. `type` without `kind` matches either dict
kind. No filter returns every kind mixed.

## 9. Requirements

Numbered, each traceable to a unit of work:

1. `REQ-CHUNKS` — Every entity has one identity chunk and one chunk per
   observation. Every relation has one chunk.
2. `REQ-FILTER` — The search tools filter by owner kind and by type name.
3. `REQ-AGGREGATE` — An owner appears once per result set. The best chunk
   decides the owner score.
4. `REQ-DETAIL` — `includeChunks` adds the matched chunk per result.
5. `REQ-REL-INDEX` — Relation create, delete, cascade, and merge changes
   the relation index in the same transaction as the graph change.
6. `REQ-OWNED` — No tool writes an embedding. The indexer owns every
   chunk vector.
7. `REQ-MIGRATE` — The migration drops legacy rows and re-enqueues every
   owner for a reindex.
8. `REQ-SURFACE` — Relation rows appear kind-marked in entity search
   results.
9. `REQ-DISPLAY` — A relation result displays `from -> TYPE -> to`.
10. `REQ-SINGLE-STORE` — `chunk_vector` is the only relation embedding
    store. The taxonomy kind-2 snapshot derives from it.
11. `REQ-SIMILAR` — `vector_search_by_entity` searches by the identity
    chunk of the named entity.

## 10. Verification commands

A gate ships with the command that checks it.

`REQ-CHUNKS`:

```sh
cargo test --test indexer_worker --features indexer -- --test-threads=1
```

`REQ-FILTER` and `REQ-SURFACE`:

```sh
cargo test --test semantic_search --features indexer -- --test-threads=1
cargo test --test vector_e2e -- --test-threads=1
```

Count check (SQLite, after a test reindex):

```sql
SELECT kind, count(*) FROM chunk_vector GROUP BY kind;
SELECT count(*) FROM chunk_vector c
  JOIN entity e ON e.id=c.owner_id
WHERE c.kind='observation' AND c.owner_kind='entity';
```

The second query must equal the total observation count.

`REQ-REL-INDEX`:

```sh
cargo test --test indexer_worker --features indexer -- --test-threads=1
```

## 11. Tests to change

The follow-up plan lists these from the current suite:

- `worker_indexes_observation_bodies_without_metadata` pins the exact
  single canonical text. Rewrite for chunk layout.
- `worker_commits_latest_canonical_revision` counts one profile row.
  Becomes chunk rows.
- `vetted_vector_revision_and_l2_gate` pins the exact blob bytes.
  Update for chunk rows.
- `startup_rejects_changed_migration_and_preserves_legacy_vector_rows`
  pins legacy survival. Replace with legacy-drop coverage.
- All `count` and `len` assertions in `vector_store.rs` tests change to
  chunk counts.
- `vector_e2e.rs` `embeddingCount` and `count` assertions change.

New tests:

- Chunk layout for identity, observation, relation.
- Filter by kind and type, exact and combined.
- Best-chunk aggregation.
- Relation enqueue, commit, cascade, and merge, fenced on the
  `taxonomy_relation` mirror revision.
- The taxonomy kind-2 snapshot derives from `chunk_vector` and refreshes
  after a relation commit.
- `vector_search_by_entity` uses the identity chunk.
- Migration drops legacy rows, drops taxonomy kind-2 rows, and
  re-enqueues every owner.

Keep the existing mirror tests in `mutation.rs` (`create_relation_writes_mirror`,
`delete_relation_tombstones_mirror_and_enqueues_delete`,
`recreated_relation_reuses_no_freed_mirror_id`,
`delete_entity_tombstones_its_relation_mirrors`). They pin the fence the
relation chunks rely on.

## 12. Risks

- The snapshot now holds every chunk. A graph with many observations
  grows the serving memory. The bench in `src/bin/bench.rs` measures the
  query cost.
- The relation chunk fence depends on the `taxonomy_relation` mirror.
  The mirror is a repository invariant with its own tests, but a future
  relation mutation path could bypass the two enqueue funnels. The
  full-scan gate must therefore count relation chunks, not only entity
  chunks.

## 13. Versioning

This is a breaking contract change. The removal of six tools, the
`entityType` parameter, and the legacy rows makes this `2.0.0`.