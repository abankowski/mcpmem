# Relation Observations, Observation Embeddings, and k:v Attributes — Design

Date: 2026-09-14. Status: approved for spec review (approach A chosen 2026-09-14).

## 1. Problem

Relations carry no text today. A relation is a bare triple
(`relation(from_id, to_id, type_id, created_us)`) whose only embedded form
is one auto-generated chunk — the triple text
(`fromName \n relationType \n toName`,
`crates/mcpmem-indexer/src/lib.rs:611-634`). Entities own observations, and
each observation body is already its own embedded chunk
(`crates/mcpmem-indexer/src/provider.rs:22-34`). Relations have no way to
record facts ("since 2023", "contract #12"), and neither entities nor
relations can carry structured metadata that must stay out of search.

## 2. Goals

- Relations have observations, with the same lifecycle and wire shape as
  entity observations.
- Every observation — entity or relation — has its own embedding and is
  searchable. Entity observations already are; relation observations gain
  the same treatment (chunked embedding plus full-text).
- Entities and relations have string `key: value` attributes.
- Attributes are structurally excluded from search: no FTS, no embedding,
  and attribute writes never enqueue an index job.

## 3. Non-goals

- Attribute values are strings only. No JSON values, no nested objects.
- Webhook/change-event coverage for relation observations and attributes.
  The event outbox is entity-scoped and its `RelationDelta` carries triples
  only (`crates/mcpmem-core/src/mutation.rs:145-153`), so there is no
  channel for the new data. Extending the event model is a separate feature.
- The code-index subsystem (`--enable-code`) is untouched.
- Attribute search, attribute indexing, attribute filters: none, by design.
- `read_graph`, `open_nodes`, `find_path`, `get_neighbors`,
  `extract_subgraph` keep returning bare triples for relations. The detail
  read path is `search_relations`.

## 4. Approved decisions

| Decision | Choice |
|---|---|
| Relation observation storage | New `relation_observation` table keyed on `taxonomy_relation.id`, the stable mirror owner identity |
| Embedding of relation observations | Chunk list `[(Relation, triple)] + [(Observation, body) per obs]`, one `embed_texts` call |
| Search surface | Owner-level results. FTS: `search_relations` gains optional `query`; vector: relation owners already surface, their observation chunks become the matched content |
| Attribute storage | One `attribute` table, `owner_kind` discriminator, string values |
| Attribute in index | Structurally absent: no FTS trigger, no chunk, no revision bump |
| Lifecycle on delete | Relation observations and attributes die with the relation (tombstone), like observations die with an entity |
| Merge semantics | A merged-away relation re-creates its triple; the recreated triple starts with no observations (the old triple's observations died with it) |
| Wire impact | Additive only. New tools: `add_relation_observations`, `delete_relation_observations`, `set_attributes`, `delete_attributes`. Existing tools gain optional parameters or response fields |

## 5. Data model

Migration `0011_relation_observations_and_attributes.sql`, plus a
`graph_stat` seed. Fresh databases run all migrations, so no
`schema.rs` change is needed — matching how `0009` added `chunk_vector`
via migration alone.

1. `relation_observation` — the exact parallel of `observation`, keyed on
   the mirror id:

   ```sql
   CREATE TABLE relation_observation (
       id          INTEGER PRIMARY KEY,
       relation_id INTEGER NOT NULL,
       idx         INTEGER NOT NULL,
       body        TEXT    NOT NULL,
       created_us  INTEGER NOT NULL,
       occurred_us INTEGER CHECK (occurred_us IS NULL OR occurred_us >= 0)
   ) STRICT;

   CREATE INDEX rel_obs_by_relation ON relation_observation(relation_id, idx);

   CREATE VIRTUAL TABLE rel_obs_fts
       USING fts5(body, content='relation_observation', content_rowid='id',
                  tokenize='unicode61 remove_diacritics 2');

   CREATE TRIGGER rel_obs_fts_ai AFTER INSERT ON relation_observation BEGIN
     INSERT INTO rel_obs_fts(rowid, body) VALUES (new.id, new.body);
   END;

   CREATE TRIGGER rel_obs_fts_bd BEFORE DELETE ON relation_observation BEGIN
     INSERT INTO rel_obs_fts(rel_obs_fts, rowid, body) VALUES ('delete', old.id, '');
   END;
   ```

   `relation_id` references `taxonomy_relation.id` — the same stable owner
   identity the chunk worker uses (`owner_kind='relation', owner_id=mirror
   id`). It is unique per triple and reused on recreate; observations must
   not outlive a tombstone (see §6).

2. `attribute` — one row per key, both owner kinds:

   ```sql
   CREATE TABLE attribute (
       owner_kind TEXT    NOT NULL CHECK (owner_kind IN ('entity','relation')),
       owner_id   INTEGER NOT NULL,
       key        TEXT    NOT NULL,
       value      TEXT    NOT NULL,
       created_us INTEGER NOT NULL,
       updated_us INTEGER NOT NULL,
       PRIMARY KEY (owner_kind, owner_id, key)
   ) STRICT;
   ```

   `owner_id` is `entity.id` or `taxonomy_relation.id`. No FTS table, no
   trigger, no chunk: nothing reads `attribute` in the indexer, and
   attribute mutations never bump a revision or enqueue a job. That is the
   "not indexable/searchable" property, enforced structurally.

3. `type_dict` kinds are unchanged. `graph_stat` gains keys
   `rel_obs_seq` (id sequence, parallel to `obs_seq`) and
   `relation_obs` (count, parallel to `observations`), seeded in the
   migration with `INSERT OR IGNORE`.

## 6. Mutation and lifecycle

### Relation observation writes

- `create_relations` accepts optional `observations` per relation
  (`RelationInput`). `create_relation` inserts the mirror as today, then
  inserts observation rows, then enqueues `OwnerKind::Relation` at
  revision 1 — the worker reads `relation_observation` at embed time, so
  the existing single enqueue covers them.
- New `add_relation_observations`: resolve the triple to its mirror id
  (`taxonomy_relation` where `deleted=0`), insert rows (idx = `MAX(idx)+1`,
  id from `rel_obs_seq`), then bump `taxonomy_relation.revision` and call
  `enqueue_chunk_change(conn, OwnerKind::Relation, mirror_id, revision,
  false)` — the same funnel as relation create/delete. A missing relation
  is an `InvalidParams` error, mirroring the entity observation path.
- New `delete_relation_observations`: resolve the mirror, delete rows
  matching by `body` — the same matching rule entity observation deletion
  uses (`mutation.rs:841-849`, `DELETE ... WHERE entity_id=?1 AND
  body=?2`) — bump revision, enqueue.
- `tombstone_relation_mirror` deletes the relation's observations and
  attributes inside the same transaction as the tombstone. The entity
  delete cascade funnels through it, so wiping an entity removes all
  observations and attributes of its incident relations with no extra
  hook.

### Attribute writes

- `create_relations` and `create_entities`/`upsert_entities` accept
  optional `attributes` maps.
- New `set_attributes` upserts rows (key present → update
  `value`/`updated_us`, absent → insert). New `delete_attributes` removes
  named keys.
- Attribute mutations do **not** bump any revision and do **not** enqueue
  any job. `chunk_vector` and `chunk_index_job` are untouched by a
  `set_attributes` call — asserted by test, not just documented.
- `merge_entities`: the source's attributes are upserted into the target;
  on key collision the source value wins (source data flows to the
  survivor, matching how source observations append). Entity attribute
  rows move with the owner id, so no re-pointing is needed for the target;
  the source's rows are deleted with the source.
- `rename_entity` needs no attribute work: attributes key on owner id, not
  name.

## 7. Indexer

`relation_chunk_text` becomes `relation_chunks`, returning the owner's
full chunk list:

```rust
pub fn relation_chunks(conn, mirror_id, expected_revision)
    -> Result<Option<Vec<(ChunkKind, String)>>>
```

Query adds the observation bodies to the existing fence query:

```sql
SELECT f.name, d.name, t.name, m.revision,
       COALESCE((SELECT json_group_array(o.body ORDER BY o.idx)
                 FROM relation_observation o WHERE o.relation_id = m.id), json('[]'))
FROM taxonomy_relation m
JOIN entity f ON f.id = m.from_id
JOIN entity t ON t.id = m.to_id
JOIN type_dict d ON d.id = m.type_id
WHERE m.id = ?1 AND m.deleted = 0 AND f.flags = 0 AND t.flags = 0
```

The worker's `OwnerKind::Relation` branch passes the returned list straight
through `embed_chunks_and_commit`, exactly as the entity branch does
(`lib.rs:300-305`). `commit_chunks` already writes any number of chunks
per owner and `chunk_vector` already keyed on
`(profile_id, kind, owner_kind, owner_id, chunk_index)`, so no migration
change there: the triple is `(kind='relation', chunk_index=0)`, each
observation is `(kind='observation', chunk_index=idx)`.

### Full-scan gate

`verify_vectors_current` needs no structural change: its relation clause
requires a current `kind='relation'` chunk per live mirror
(`jobs.rs:652-661`) — the triple is always chunk 0 — and its orphan
clause (`m.id IS NULL OR m.deleted != 0`, `jobs.rs:662-665`) correctly
flags a tombstoned mirror's leftover observation chunks as stale until the
pending delete job runs. The taxonomy kind-2 freshness check
(`jobs.rs:913`) counts only `kind='relation'` rows and is unaffected.

## 8. Search path

- **FTS**: `search_relations` gains an optional `query` parameter. When
  present it matches `rel_obs_fts` (bm25 rank), resolves matched
  observation rows to relations, and composes with the existing
  `from`/`to`/`relationType` filters as AND. Results stay owner-level:
  relation rows, not observation rows. When `query` is absent the handler
  behaves exactly as today.
- **Vector**: no change needed. Relation owners already appear in
  `vector_search_entities`, `hybrid_search`, `vector_mmr_search`,
  `semantic_search` and `vector_search_by_entity`. Their observation
  chunks become the per-owner best chunk and render as
  `"chunk":{"kind":"observation","text":...}` under `includeChunks`.
- `chunk_text` reassembly (`src/vector_store.rs:1129-1177`) gains a
  branch: `OwnerKind::Relation` + `ChunkKind::Observation` looks up
  `relation_observation` by `(relation_id, idx)`; the existing relation
  branch handles the triple chunk.
- `search_nodes` is unchanged: entity observations remain text-searchable
  through `obs_fts`; relation observation text is reached through
  `search_relations` and the vector tools.

## 9. Tool surface

All additive. Schema changes land in `tools.json`; dispatch arms in
`src/server.rs:1124-1195`; `ToolMeta` rows in `src/tools.rs`.

New tools:

| Tool | Input |
|---|---|
| `add_relation_observations` | `{"relations":[{"from","to","relationType","contents":[ObservationInput]}]}` |
| `delete_relation_observations` | `{"relations":[{"from","to","relationType","observations":[ObservationInput]}]}` |
| `set_attributes` | `{"targets":[{ownerKind:"entity",entityName,attributes:{}}]}` or `{ownerKind:"relation",from,to,relationType,attributes:{}}` |
| `delete_attributes` | `{"targets":[{ownerKind, entityName-or-triple, keys:[string]}]}` |

`set_attributes` returns the applied post-state per target so clients can
verify the write. `add_relation_observations` returns
`{"results":[{from,to,relationType,addedObservations:[Observation]}]}`,
mirroring `add_observations`'s shape. `delete_*` return the standard
unit-ish text response used by the other delete tools.

Changed tools (additive parameters/fields):

| Tool | Change |
|---|---|
| `create_relations` | each relation gains optional `observations` and `attributes` |
| `create_entities`, `upsert_entities` | each entity gains optional `attributes` |
| `search_relations` | gains optional `query`; rows become `{from, to, relationType, observations: [...], attributes: {...}}` (both always present, empty when none) |
| `get_entity`, `describe_entity` | output gains `attributes` |
| `export_graph` | relation rows gain `observations` and `attributes`, so backups stay lossless |

Validation caps reuse existing constants: attribute keys ≤ `MAX_NAME_BYTES`
(1024), values ≤ `MAX_OBSERVATION_BYTES` (65536), batches ≤
`MAX_RELATIONS_PER_REQUEST` / `MAX_OBSERVATIONS_PER_ENTITY`.

## 10. Requirements

Numbered, each traceable to a unit of work:

1. `REQ-OBS-STORE` — A relation has zero or more observations, stored in
   `relation_observation` keyed on the mirror id.
2. `REQ-OBS-CRUD` — Create-with-observations, add, delete, and cascade
   delete (entity delete, relation delete) work, with the same validation
   and wire shape as entity observations.
3. `REQ-OBS-INDEX` — Relation observation bodies are embedded as
   `kind='observation'` chunks owned by the relation, fenced on the mirror
   revision, in the same `embed_texts` call as the triple.
4. `REQ-OBS-FTS` — `search_relations(query=...)` matches relation
   observation bodies, composing with structural filters.
5. `REQ-OBS-SEARCH` — Relation observation text appears as matched chunk
   detail in the vector search tools.
6. `REQ-ATTR-STORE` — Entities and relations store string `key: value`
   attributes in the `attribute` table.
7. `REQ-ATTR-CRUD` — Attributes are set at create, upserted after the
   fact, deleted, merged (source wins on collision), and cascade-deleted
   with their owner.
8. `REQ-ATTR-OFFLINE` — No FTS row, no chunk, and no index job is
   produced for an attribute write; an attribute change never bumps a
   revision.
9. `REQ-ATTR-READ` — Attributes appear in `get_entity`,
   `describe_entity`, `search_relations`, and `export_graph`.
10. `REQ-LIFECYCLE` — Tombstoning a relation deletes its observations and
    attributes in the same transaction; wipe and maintenance clean the new
    tables and `rel_obs_fts`.
11. `REQ-COMPAT` — Existing tool calls parse and respond unchanged when
    the new parameters and fields are absent; `Relation` (path tools)
    stays triple-only.

## 11. Verification commands

A gate ships with the command that checks it.

`REQ-OBS-STORE` + `REQ-OBS-CRUD` + `REQ-LIFECYCLE`:

```sh
cargo test --test role_composition --features indexer,webhooks
```

Count check (SQLite, after a test write to `relation_observation`):

```sql
SELECT count(*) FROM relation_observation;
SELECT count(*) FROM rel_obs_fts;
SELECT count(*) FROM attribute WHERE owner_kind='relation';
```

`REQ-OBS-INDEX` + `REQ-OBS-SEARCH`:

```sh
cargo test --test indexer_worker --features indexer -- --test-threads=1
cargo test --test semantic_search --features indexer -- --test-threads=1
cargo test --test vector_e2e -- --test-threads=1
```

`REQ-OBS-FTS`:

```sh
cargo test --test role_composition --features indexer,webhooks
```

`REQ-ATTR-*`:

```sh
cargo test --test role_composition --features indexer,webhooks
```

`REQ-ATTR-OFFLINE` (no job, no chunk, no revision):

```sh
cargo test --test role_composition --features indexer,webhooks
```

The test names the specific assertion: after `set_attributes`, `chunk_index_job`
has no new row and `taxonomy_relation.revision` is unchanged.

`REQ-COMPAT`:

```sh
cargo test --workspace --all-targets -- --test-threads=1
```

## 12. Tests to change

- `create_relations` callers across the suite adapt to `RelationInput`
  (additive fields, so most call sites compile with the extra struct
  fields) — `handle_create_relations`, mutation tests, graph tests.
- The `chunk_text` relation branch keeps its triple test; add a
  relation-observation reassembly test (`vector_store.rs:2048-2067`
  area).
- `export_graph` golden output gains the new relation fields.
- The `handle_search_relations` response-shape test gains rows.

New tests:

- Relation observation insert/delete/revision-bump/enqueue, fenced on the
  mirror revision; re-embed sees the new observation.
- Tombstone cascade: delete relation and delete entity both remove
  observations and attributes and enqueue a relation delete job.
- `relation_chunks` layout: `[(Relation, triple)] + one (Observation, body)
  per row`, and empty-observation relation still yields the triple only.
- `verify_vectors_current` accepts a multi-chunk relation and rejects a
  stale one.
- FTS: `search_relations(query)` ranks and composes with filters; absent
  query is byte-identical to today's behavior.
- `set_attributes`/`delete_attributes` for both owner kinds, merge
  collision source-wins, wipe cleanup, `rel_obs_fts` optimize in
  maintenance.
- `REQ-ATTR-OFFLINE`: no job row, no chunk row, no revision change after
  an attribute write.

## 13. Risks

- **Mirror dependency**: relation observations depend on the
  `taxonomy_relation` mirror staying the single owner identity, including
  the no-freed-id invariant. The existing mirror tests pin that; the
  full-scan gate counts relation chunks, so a bypassed funnel fails the
  candidate scan rather than serving stale chunks.
- **Multi-chunk relation growth**: a relation with many observations grows
  the serving snapshot. Bounded by `MAX_OBSERVATIONS_PER_ENTITY` (1000),
  matching entity observations.
- **FTS content drift**: `rel_obs_fts` is external-content, so a row
  deleted outside the trigger path silently leaves a stale posting — the
  same risk `obs_fts` already carries; wipe resets it with
  `delete-all`, and tombstone cleanup goes through SQL that fires the
  `BEFORE DELETE` trigger.

## 14. Versioning

Additive contract change: new optional parameters, new tools, new
response fields. This is a minor bump: `2.0.1` → `2.1.0`.