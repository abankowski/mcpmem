# Taxonomy Soft Validation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a write introduces an entity type or a relation type that does not exist yet, the server returns suggestions of similar existing types, entities, and relations — by string similarity always, and by semantic similarity when the embedding pipeline is available — while never refusing the write.

**Architecture:** Two tiers share one hook. The offline tier compares the new type name against `type_dict` names (trigram + Levenshtein). The semantic tier embeds types, relation types, and relation instances through the existing durable job-queue worker (`index_job` pattern), stores the vectors in new tables, serves them through per-kind snapshots that mirror the entity `ManagedSnapshot` (flat vectors, linear nearest-neighbour — the codebase's established profile-serving shape, `vector_store.rs:1145-1158`; a per-kind usearch index is the documented scale-up path if relation-instance volume ever demands it), and embeds the query text through the same `indexer_provider` seam that `semantic_search` uses. Freshness mirrors the entity loop: each successful `commit_vector` bumps the kind's durable generation (`jobs.rs:434` mirror), and the reconcile rebuilds when `published < durable`. The hook lives in the server layer (`src/actions/memory.rs`) and composes an additive, per-object `taxonomySuggestions` field into create/upsert results. The core mutation path never rejects an unknown type.

**Tech Stack:** Rust, rusqlite (SQLite with STRICT tables), FTS5 for text, USearch HNSW/IVF for ANN, trigram + Levenshtein for the offline tier.

**Spec:** This plan is the implementation spec. Design decisions are recorded in §Design decisions; each decision carries the check that enforces it. Executors read this document only; the conversation is not carried into tasks.

## Global Constraints

These apply to every task. Copy them verbatim into each task brief.

1. Pre-flight before any push: run the repository chain from `.omp/AGENTS.md` exactly — `cargo fmt --all --check`, `scripts/check-release-version.sh`, `scripts/check-crate-includes.sh`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-targets -- --test-threads=1`, the `indexer_worker` and `role_composition` and `webhook_tools` matrix, `cargo package -p mcpmem-core --locked --allow-dirty`. The default-feature clippy run fails on known pre-existing findings and stays unused.
2. Migrations are append-only and checksummed. `events.rs:45` registers `MIGRATIONS: [(i64, &str); 5]`; the inventory test pins count, order, and content. A new migration edits the array and its type (`; 6]`).
3. `mcpmem-core` compiles with and without features. No tier-one dependency on the `indexer` feature may enter core.
4. Wire changes are additive only. `deny_unknown_fields` stays on every DTO. An array response stays an array; a per-object optional field is the only allowed shape change.
5. Suggestions reference only types with `count > 0` (`select_all_types` behavior, `graph.rs:80`).
6. No enforcement: an unknown type must never fail a write. This is the acceptance property of the whole feature.
7. Write every brief, comment, and commit message in ASD-STE100.
8. Fixed capacity ceilings: 128 members per type document, 128 endpoint-type pairs per relation-type document, `topK` clamp 1..100, at most one provider call per write hook, at most 12 suggestion texts per call.

## Design decisions

|#|Decision|Check that enforces it|
|---|---|---|
|D1|Subject kinds: `0` = entity type, `1` = relation type, `2` = relation instance|`CHECK (subject_kind IN (0,1,2))` on `taxonomy_job`, `taxonomy_vector`|
|D2|Types fence on a new `type_dict.revision` column; instances fence on a new `revision` column in a new identity mirror table|Fence SQL in `TaxonomyJobRepository::commit` (Task 7)|
|D3|Relation rows get identity through `taxonomy_relation` (stable ids, tombstones), never through implicit rowids|`taxonomy_relation` PK `id`; delete path sets `deleted=1` and enqueues a revisioned delete (Task 8)|
|D4|New tables mirror `index_job` / `profile_vector` / `ann_generation` exactly; the entity pipeline is untouched|No task edits the entity job tables; new tables carry their own `_due` index and generation table|
|D5|The hook composes `taxonomySuggestions` per result object at the server layer; core DTOs are unchanged|`handle_create_entities` result objects gain the field only for unknown types (Task 4)|
|D6|Enqueue happens in `mutation.rs::update_counters`, in the writer transaction, one place for all mutation kinds|`update_counters` receives the before/after snapshots already; type deltas already derive there|
|D7|The semantic tier reuses `vs.serving_profile()` and `crate::indexer_provider::get()`; no new provider wiring|`handle_semantic_search` shows the exact seam (`vector_actions.rs:803-824`)|
|D8|Query embedding batches one provider call per write hook|The semantic engine takes `&[String]` and calls `provider.embed_texts` once (Task 11)|
|D9|Taxonomy snapshot freshness mirrors the entity loop: `commit_vector` bumps the kind's durable generation (mirror of `jobs.rs:434`); the reconcile rebuilds when `published < durable`|`builds one flat per-kind snapshot` in Task 10; the bump lives in Task 7's `commit_vector` success path|

## File map

|File|Change|Task|
|---|---|---|
|`crates/mcpmem-core/migrations/0006_taxonomy_index.sql`|Create|1|
|`crates/mcpmem-core/src/events.rs`|Modify: `MIGRATIONS` array + type|1|
|`crates/mcpmem-core/src/graph.rs`|Modify: public `entity_type_exists`, `relation_type_exists`|2|
|`src/taxonomy.rs`|Create: engine (fallback + semantic + assembly)|3, 11|
|`src/actions/memory.rs`|Modify: hook in three handlers|4, 12|
|`src/tools.rs`, `src/server.rs`, `tools.json`|Modify: `suggest_taxonomy` tool|5|
|`crates/mcpmem-core/src/jobs.rs`|Modify: `TaxonomyJobRepository` (claim/renew/commit/verify)|7|
|`crates/mcpmem-core/src/mutation.rs`|Modify: revision bumps + enqueue in `update_counters`|8|
|`crates/mcpmem-indexer/src/lib.rs`|Modify: `taxonomy_document`, worker dispatch|6, 9|
|`src/vector_store.rs`|Modify: `adopt_taxonomy`, per-kind ANN, resolvers, `search_kind`|10|
|`README.md`, `CHANGES.md`|Modify|5, 14|

---

## Phase A — offline tier (shippable alone)

### Task 1: Migration 0006 and registry

**Files:**
- Create: `crates/mcpmem-core/migrations/0006_taxonomy_index.sql`
- Modify: `crates/mcpmem-core/src/events.rs:45-57`
- Test: `crates/mcpmem-core/src/events.rs` (inventory test, existing anchor)

**Interfaces:**
- Produces: tables `taxonomy_job`, `taxonomy_vector`, `taxonomy_ann_generation`, `taxonomy_relation`; new columns `type_dict.revision`, `relation.revision` (unused until Task 7/8).

- [ ] **Step 1: Write the failing inventory test**

The anchor test `every_migration_version_and_checksum_is_pinned` hardcodes the expected `(version, sha256)` pairs under `super::MIGRATIONS` — it does not derive them. Add the `(6, <sha256 of the new file content>)` pair to the expected list, then run the test. It must fail RED before the pair is added (the registry holds 6 entries, the expectation 5) and pass GREEN after.

- [ ] **Step 2: Create the migration file**

```sql
ALTER TABLE type_dict ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE relation ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;

CREATE TABLE taxonomy_relation (
    id INTEGER PRIMARY KEY,
    from_id INTEGER NOT NULL,
    to_id   INTEGER NOT NULL,
    type_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,
    UNIQUE(from_id, to_id, type_id)
) STRICT;

CREATE TABLE taxonomy_job (
    subject_kind INTEGER NOT NULL CHECK (subject_kind IN (0,1,2)),
    subject_id   INTEGER NOT NULL,
    profile_id   TEXT NOT NULL,
    subject_revision INTEGER NOT NULL,
    operation TEXT NOT NULL CHECK (operation IN ('upsert','delete')),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','leased','held','done','dead')),
    lease_token TEXT,
    lease_epoch INTEGER NOT NULL DEFAULT 0,
    lease_until_us INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_us INTEGER NOT NULL DEFAULT 0,
    last_error TEXT CHECK (length(last_error) <= 2048),
    PRIMARY KEY(subject_kind, subject_id, profile_id)
) STRICT;
CREATE INDEX taxonomy_job_due ON taxonomy_job(state, next_attempt_us, lease_until_us);

CREATE TABLE taxonomy_vector (
    profile_id TEXT NOT NULL,
    subject_kind INTEGER NOT NULL CHECK (subject_kind IN (0,1,2)),
    subject_id INTEGER NOT NULL,
    subject_revision INTEGER NOT NULL,
    blob BLOB NOT NULL,
    created_at_us INTEGER NOT NULL,
    source TEXT NOT NULL,
    PRIMARY KEY(profile_id, subject_kind, subject_id)
) STRICT;

CREATE TABLE taxonomy_ann_generation (
    profile_id TEXT NOT NULL,
    subject_kind INTEGER NOT NULL CHECK (subject_kind IN (0,1,2)),
    durable_generation INTEGER NOT NULL DEFAULT 0,
    published_generation INTEGER NOT NULL DEFAULT -1,
    full_scan_generation INTEGER,
    PRIMARY KEY(profile_id, subject_kind),
    CHECK(published_generation <= durable_generation)
) STRICT;

INSERT INTO taxonomy_relation(id, from_id, to_id, type_id, revision, deleted)
SELECT rowid, from_id, to_id, type_id, 1, 0 FROM relation;
```

- [ ] **Step 3: Register the migration**

In `events.rs`, change `pub const MIGRATIONS: [(i64, &str); 5]` to `[(i64, &str); 6]` and append `(6, include_str!("../migrations/0006_taxonomy_index.sql")),`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p mcpmem-core --test every_migration_version_and_checksum_is_pinned`
Expected: RED first (expectation holds 5 pairs, registry holds 6), PASS after the expected list gains the sixth pair. The anchor's sha256 pin is the test's own value — copy it from the test's expected array, which the first RED run's assertion message names.

- [ ] **Step 5: Commit**

```bash
git add crates/mcpmem-core/migrations/0006_taxonomy_index.sql crates/mcpmem-core/src/events.rs
git commit -m "feat(core): add taxonomy index schema (migration 0006)"
```

### Task 2: Public type-existence checks

**Files:**
- Modify: `crates/mcpmem-core/src/graph.rs` (near `lookup_type_id`, `graph.rs:61`)
- Test: `crates/mcpmem-core/src/graph.rs` tests module

**Interfaces:**
- Produces: `pub fn entity_type_exists(&self, name: &str) -> bool`, `pub fn relation_type_exists(&self, name: &str) -> bool`. Both use the reader pool and never insert. `None` means `false`.

- [ ] **Step 1: Write the failing tests**

```rust
fn test_entity_type_exists() {
    let kg = new_kg_with_pool(2);
    kg.create_entities(&[Entity { name: "a".into(), entity_type: "person".into(), observations: vec![] }]).unwrap();
    assert!(kg.entity_type_exists("person"));
    assert!(!kg.entity_type_exists("persn"));
}
```
Mirror for `relation_type_exists` with a relation of type `knows`; assert a missing type stays absent and inserts no phantom row (`lookup_type_id` precedent, test at `graph.rs:3391`).

- [ ] **Step 2: Implement**

```rust
pub fn entity_type_exists(&self, name: &str) -> bool {
    let conn = self.readers.get();
    lookup_type_id(&conn, name, 0).is_some()
}
```
Add the `kind = 1` twin.

- [ ] **Step 3: Run, then commit**

Run: `cargo test --test entity_type_exists`
Expected: PASS. Commit: `feat(core): add read-only type existence checks`.

### Task 3: Offline suggestion engine

**Files:**
- Create: `src/taxonomy.rs`
- Test: `src/taxonomy.rs` tests module

**Interfaces:**
- Consumes: `Vec<(String, usize)>` from `GraphHandle::entity_type_counts` / `relation_type_counts`.
- Produces:
  - `pub enum SubjectKind { EntityType = 0, RelationType = 1, Relation = 2 }`
  - `pub struct Suggestion { pub name: String, pub score: f64 }` (serde camelCase: `name`, `score`)
  - `pub fn suggest_strings(name: &str, existing: &[(&str, usize)]) -> Vec<Suggestion>`

  Scoring: lowercase both sides. Trigram overlap `2*t / (|a|+|b|)` times `0.7` plus Levenshtein similarity `1 - dist/maxlen` times `0.3`, summed. Keep candidates with score `> 0.35`, then sort by score desc, then name asc. Skip entries with `count == 0` and `name == 0` comparisons below.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn fallback_suggests_typo_and_underscore_variants() {
    let existing = vec![("person".into(), 3usize), ("persn".into(), 0usize), ("project".into(), 2usize)];
    let got = suggest_strings("persn", &existing);
    // count==0 entries are invisible; the exact typo variant maps to "person"
    assert!(got.iter().any(|s| s.name == "person"));
    assert!(got.iter().all(|s| s.name != "persn"));
}

#[test]
fn fallback_orders_by_score_then_name() { /* "relatedTo" vs ["related_to", "relates_to"] -> related_to first */ }

#[test]
fn fallback_returns_empty_for_garbage() { /* "zzzz" -> [] */ }
```

- [ ] **Step 2: Implement `suggest_strings`**

Pure functions only. Trigrams of the lowercase name; Levenshtein over lowercase strings (bounded: stop early when `dist > maxlen`).

- [ ] **Step 3: Run**

Run: `cargo test --test fallback`
Expected: PASS.

- [ ] **Step 4: Commit**

`git commit -am "feat(server): add offline taxonomy suggestion engine"`

### Task 4: Write-hook with the offline tier

**Files:**
- Modify: `src/actions/memory.rs` (`handle_create_entities` 86-124, `handle_create_relations` 127-159, `handle_upsert_entities` ~515)
- Test: `tests/` integration (server-level, see conventions in `tests/`)

**Interfaces:**
- Consumes: Task 2 `*_exists`, Task 3 `suggest_strings`/`SubjectKind`, `GraphHandle::entity_type_counts`, `relation_type_counts`.
- Produces: result objects that carry an optional `taxonomySuggestions: [{ name, score }]` field, present only when the object's authored type was unknown before the write. The top-level response shape does not change.

Composition (in `memory.rs`) — as built by Task 4:

```rust
/// Runs the suggestion engine once per distinct authored type. One counts
/// query runs per kind.
fn suggestion_map<'a>(
    kg: &GraphHandle,
    types: impl Iterator<Item = &'a str>, // authored types unknown before the write
    kind: SubjectKind,
) -> HashMap<String, Vec<Suggestion>>

/// Appends the ready suggestion array to a result object.
fn enrich_result_object(value: Value, candidates: &[Suggestion]) -> Value
```

SUPERSEDED (2026-09-12, Task 4 execution): this section originally specified a
per-object helper that "checks the post-commit state". That is impossible: the
mutation inserts the authored type row (`type_id` in
`crates/mcpmem-core/src/mutation.rs:542`), so a post-commit existence check
always reports a self-authored type as existing and the feature could never
fire. Each handler captures existence for each authored type before the
mutation, builds the suggestion map once per distinct unknown type, and then
looks the ready array up per result object (matching entities by name; the
result may be reordered).

- [ ] **Step 1: Write the failing integration test**

Start the server in-process per `tests/` conventions. `create_entities` with `entityType: "persn"` when only `person` exists must return the entity plus `taxonomySuggestions` containing `person`; the same call with `entityType: "person"` must return no `taxonomySuggestions` key; a relation with an unknown relation type must still be created (no `isError`).

- [ ] **Step 2: Implement the helper and wire the three handlers**

Each handler maps the result `Vec<Entity>`/`Vec<Relation>` to `Vec<Value>`, then looks the ready suggestion array up per object (matching entities by name; the result may be reordered). Serialize the mapped vector.

- [ ] **Step 3: Run the suite, then commit**

Run: `cargo test --test taxonomy` (or the integration target's name).
Expected: PASS, including the no-enforcement assertion. Commit: `feat(server): add soft taxonomy suggestions to write responses`.

### Task 5: `suggest_taxonomy` tool (offline tier)

**Files:**
- Modify: `tools.json`, `src/tools.rs` (`ALL_TOOLS`), `src/server.rs` (dispatch + `SERVER_INSTRUCTIONS` at 716-720)
- Test: `src/tools.rs` and `src/server.rs` tests

**Interfaces:**
- Produces: read tool `suggest_taxonomy`:
  - Inputs: `query` (string, required, non-blank), `kind` (enum `entityType` | `relationType` | `entity` | `relation`, default `entityType`), `topK` (1..100, default 10).
  - Output: `{ suggestions: [{ name, score }] }`. For kind `entityType`/`relationType` the offline tier returns type names; for `entity`/`relation` the offline tier is empty (the semantic tier fills them, Task 12).
- `ToolMeta { name: "suggest_taxonomy", write: false }` → `category()` yields `GraphRead` automatically (`tools.rs:79-86`).

- [ ] **Step 1: Manifest + registry + dispatch tests**

Add the tool JSON to `tools.json` with the input schema above. Add the `ToolMeta` row. In the dispatch match (`server.rs:1170` area) add `"suggest_taxonomy" => taxonomy::handle_suggest_taxonomy(kg, tool_args)`.

- [ ] **Step 2: Implement the handler**

```rust
pub fn handle_suggest_taxonomy(kg: &GraphHandle, args: Option<&Value>) -> Result<Value>
```
Validates `query` non-blank; parses `kind`; for entity/relation types calls the counts + `suggest_strings`; returns the JSON object. For `entity`/`relation` kinds, calls `kg.search_nodes_filtered(query, None, 0, topK)` / `kg.search_relations` and maps to `{name, score: 1.0}` placeholders? — no: return `{ suggestions: [] }` until Task 12 fills them. State this limit in the manifest description now.

- [ ] **Step 3: Run, update README tool table, commit**

Commit: `feat(server): add suggest_taxonomy tool (offline tier)`.

---

## Phase B — semantic tier

### Task 6: Canonical documents for taxonomy subjects

**Files:**
- Modify: `crates/mcpmem-indexer/src/lib.rs` (near `canonical_document`)
- Test: `crates/mcpmem-indexer/src/lib.rs` tests

**Interfaces:**
- Produces: `pub fn taxonomy_document(conn: &Connection, kind: i64, subject_id: i64, expected_revision: i64) -> Option<CanonicalDocument>`
  - `kind 0` (entity type): fence on `type_dict` row `(kind=0, id, revision)`. Text = `"entityType: <name>\n"` + up to 128 member names (`SELECT name FROM entity WHERE type_id=?1 AND flags=0 ORDER BY id LIMIT 128`), joined by `\n`.
  - `kind 1` (relation type): fence identical on `type_dict` row `(kind=1, id, revision)`. Text = `"relationType: <name>\n"` + up to 128 distinct `"<from_type> <name> <to_type>"` triples, joined by `\n`.
  - `kind 2` (relation instance): fence on `taxonomy_relation` row `(id, deleted=0, revision)`. Resolve names/types; text = `"<from> (<from_type>) -[<relation_type>]-> <to> (<to_type>)"`. `None` when the mirror row is missing or deleted.

- [ ] **Step 1: Write the failing tests**

Existing indexer tests use a fixture DB with a fake provider. Add three tests asserting the exact document text for a seeded type/relation, and one asserting `None` on revision mismatch.

- [ ] **Step 2: Implement**

Reuse `CanonicalDocument` (fields `name`, `entity_type`, `observations`) mapped to `(name, "", vec![line1, line2, ...])` — the join in `provider.rs:15-18` already concatenates with `\n`. Follow the `canonical_document` fencing style.

- [ ] **Step 3: Run, commit**

Commit: `feat(indexer): add canonical documents for taxonomy subjects`.

### Task 7: TaxonomyJobRepository in core

**Files:**
- Modify: `crates/mcpmem-core/src/jobs.rs`
- Test: `crates/mcpmem-core/src/jobs.rs` tests

**Interfaces:**
- Consumes: migration 0006 tables; `IndexProfileRegistry` (existing).
- Produces, mirroring `IndexJobRepository`:
  - `pub struct TaxonomyJob { pub subject_kind: i64, pub subject_id: i64, pub subject_revision: i64, pub profile_id: Uuid, pub operation: IndexOperation, pub lease: Lease }`
  - `pub(crate) fn enqueue_taxonomy(conn, kind, id, revision, operation, profile)` — insert/update like `enqueue_change` (jobs.rs:32 shape, ON CONFLICT update + `lease_epoch+1`); also `UPDATE taxonomy_ann_generation SET full_scan_generation=NULL`.
  - `claim_due`, `renew`, `commit_vector` — same lease/epoch/revision gates as the entity path: commit refuses when `state != 'leased'`, `lease_token`/`lease_epoch` mismatch, or the subject's current revision differs from `job.subject_revision` for `upsert`; for `delete` the gate is `taxonomy_relation.deleted=1 AND revision == job.subject_revision` (kind 2) or unconditional (kinds 0/1 never delete).
  - `verify_full_scan`-equivalent staleness query: `taxonomy_vector` `subject_revision` vs. current source revision per kind. A source row is invalid when it has no live (`pending`/`leased`/`held`) job, or its only job is `dead` — the full-scan rebuild re-enqueues such subjects. This mirrors the entity clause `AND NOT EXISTS(... d.state='dead')`.

- [ ] **Step 1-3: Tests first (fence rejection, deletion gate, staleness), then implement**

Port the entity-path tests (`jobs.rs` tests at the file's test module): lease expiry refuses commit; revision mismatch refuses commit; delete without a matching tombstone refuses.

- [ ] **Step 4: Run, commit**

Commit: `feat(core): add taxonomy job repository with fenced commits`.

### Task 8: Enqueue in the writer transaction

**Files:**
- Modify: `crates/mcpmem-core/src/mutation.rs` (`update_counters` 850-880, `create_relation` 629-633, relation delete 719-722, `create_entity` 611-615)
- Test: `crates/mcpmem-core/src/mutation.rs` tests / integration

**Rules (mirror the count maintenance that already lives there):**
- Entity types (kind 0): recompute affected types = keys of `type_deltas` ∪ `{before.type, after.type}` of every entity change. For each, `UPDATE type_dict SET count=count+?delta, revision=revision+1 WHERE kind=0 AND name=?` then enqueue `upsert` with the new revision (read through `type_id`).
- Relation types (kind 1): same, from relation-type deltas ∪ changes whose relation rows appear/disappear.
- Relation instances (kind 2): in `create_relation`, after the `changed > 0` insert, `SELECT rowid` of the triple, insert into `taxonomy_relation` (`revision=1`), enqueue `upsert` rev 1. In the relation delete path, for each deleted triple, update the mirror row `SET revision=revision+1, deleted=1` and enqueue `delete` with the new revision.
- Every enqueue also sets `taxonomy_ann_generation.full_scan_generation=NULL` via `enqueue_taxonomy`.

- [ ] **Step 1: Write the failing tests**

Create a new entity with a new type → assert one `taxonomy_job` row (kind 0, revision 1). Create a second entity of the same type → revision 2, job upserted. Rename an entity of type `person` → revision bumps (no count change). Create a relation → kind-2 job + mirror row. Delete the relation → kind-2 delete job + mirror `deleted=1`.

- [ ] **Step 2-3: Implement, run, commit**

Commit: `feat(core): enqueue taxonomy jobs in the writer transaction`.

### Task 9: Worker dispatch for taxonomy jobs

**Files:**
- Modify: `crates/mcpmem-indexer/src/lib.rs` (`run_once`)
- Test: indexer worker tests (fake provider)

**Change:** `run_once` first tries `IndexJobRepository::claim_due`; when none, tries `TaxonomyJobRepository::claim_due`. The provider call, L2 normalization, renew-before-provider and renew-before-commit gates stay exactly as the entity path (`lib.rs:288-345`). `commit_vector` with `None` handles deletes.

- [ ] **Step 1: Write the failing test** — a pending kind-0 job three revisions old must fail the fence and stay leased/dead-lettered per policy.
- [ ] **Step 2: Implement, run, commit**

Commit: `feat(indexer): process taxonomy jobs in the worker`.

### Task 10: Serving snapshots in VectorStore

**Files:**
- Modify: `src/vector_store.rs` (near `adopt_profile` 880-943)
- Test: `src/vector_store.rs` tests

**Interfaces:**
- Produces:
  - `pub enum TaxonomyKind { EntityType = 0, RelationType = 1, Relation = 2 }`
  - `pub fn adopt_taxonomy(&mut self, registry: &IndexProfileRegistry, candidate: bool) -> Result<()>` — builds one flat per-kind snapshot from `taxonomy_vector` rows, mirroring `adopt_profile`'s `ManagedSnapshot` (flat vectors, linear search; the entity snapshot is itself flat — see D9).
  - `pub fn search_taxonomy(&self, kind: TaxonomyKind, query: &[f32], top_k: usize) -> Result<Vec<(i64, f64)>>` — nearest `(subject_id, distance)` in the kind's snapshot.
  - `pub fn resolve_taxonomy(&self, kind: TaxonomyKind, id: i64) -> Option<(String, String)>` — `(name, kind_label)` via `type_dict` (kinds 0/1) or `taxonomy_relation` + entity names (kind 2).

Managed snapshot gains per-kind ANN entries; keep the entity snapshot untouched.

- [ ] **Step 1: Write the failing tests** — seed `taxonomy_vector` rows, build, search nearest, resolve names; a stale generation refuses publish.
- [ ] **Step 2: Implement, run, commit**

Commit: `feat(server): serve per-kind taxonomy ANN snapshots`.

### Task 11: Semantic engine

**Files:**
- Modify: `src/taxonomy.rs`
- Test: `src/taxonomy.rs` tests (fake provider, per vector_actions conventions)

**Interfaces:**
- Consumes: `vs.serving_profile()`, `crate::indexer_provider::get()` — the exact seam `handle_semantic_search` uses (`vector_actions.rs:803-824`).
- Produces: `pub fn suggest_semantic(vs: &VectorStore, texts: &[String], kind: SubjectKind, top_k: usize) -> Result<Vec<Vec<Suggestion>>>`. The outer vec groups results per input text (one inner vec per text) — required so the write hook can split per-type results under one provider call. One `embed_texts` call for all texts; one `search_taxonomy` per text; maps distances to scores `1.0 - distance`; clamps to 1..100; labels via `resolve_taxonomy`.

- [ ] **Step 1: Write the failing tests** — embedding batching (one provider call), score mapping, kind routing.
- [ ] **Step 2: Implement, run, commit**

Commit: `feat(server): semantic taxonomy suggestions`.

### Task 12: Hook upgrade — semantic + examples

**Files:**
- Modify: `src/actions/memory.rs`
- Test: integration

**Change:** the suggestion assembly gains the optional semantic tier `(vs, provider)`:
- Semantic tier active → `suggest_semantic` for the unknown type name.
- Offline tier always fills any remaining slots (semantic misses types that string distance catches).
- For kind 0: also attach up to 3 `exampleEntities` (`{name, entityType}`) from the top similar types via `search_nodes_filtered(type, None, 0, 3)`.
- For kind 1: attach up to 3 `exampleRelations` (`{from, relationType, to}`) via `search_relations(None, None, similar_type, 3)`.
- The field becomes `{ similarTypes: [{name, score}], exampleEntities?: [...], exampleRelations?: [...] }`. Existing consumers of `{name, score}` (Task 4) tolerate the wider object; update the Task 4 tests.

- [ ] **Step 1: Write the failing tests** — with a fake provider: unknown type returns semantic + offline suggestions + examples; without a provider: offline only; known types: no key.
- [ ] **Step 2: Implement, run, commit**

Commit: `feat(server): semantic suggestions and examples in write responses`.

### Task 13: `suggest_taxonomy` semantic tier

**Files:**
- Modify: `src/taxonomy.rs`, `src/server.rs` (dispatch passes `vs` + provider), `tools.json` description, `README.md`

**Change:** kinds `entity` and `relation` now return real semantic results via `suggest_semantic` against taxonomy kind 2 (relation) or the entity ANN (kind `entity` via `search_entities_json`). Kinds `entityType`/`relationType` prefer `suggest_semantic`, fall back to `suggest_strings`.

- [ ] **Step 1: Update the manifest description and the README tool table.**
- [ ] **Step 2: Implement, run, commit**

Commit: `feat(server): semantic tier for suggest_taxonomy`.

---

## Phase C — consolidation

### Task 14: Full verification and release notes

**Files:**
- Modify: `README.md` (taxonomy section), `CHANGES.md`

- [ ] **Step 1: Write the README section** — "Soft taxonomy validation": behavior, tiers, gating (offline always; semantic requires `--enable-vectors` + `indexer` feature + a serving `[indexer]` profile), the `taxonomySuggestions` field, the `suggest_taxonomy` tool.
- [ ] **Step 2: CHANGES.md entry** — feature summary, migration note (`0006` auto-applies), no breaking wire change.
- [ ] **Step 3: Run the full pre-flight chain from Global Constraints**, then the marker write:

```sh
git rev-parse HEAD > "$(git rev-parse --git-dir)/omp-preflight-pass"
```

- [ ] **Step 4: Commit, then open the PR** (skill `pr-description` first; never merge).

Commit: `docs: taxonomy soft validation release notes`.

---

## Self-review

**Spec coverage:** D1-D8 each map to tasks (see table). The acceptance property (no enforcement) is asserted in Task 4 and again in Task 12. The offline tier ships at Task 5 — the feature is usable before the semantic tier lands. The semantic tier's provider seam is Task 11 against the proven `semantic_search` path.

**Placeholder scan:** Every task names concrete files, signatures, SQL, and tests. The one open reference is the exact integration-test harness name in `tests/`; it is resolved by the executor's first read of that directory in Task 4 (the repo pins harness conventions there).

**Type consistency:** `SubjectKind { EntityType = 0, RelationType = 1, Relation = 2 }` (Task 3) and `TaxonomyKind` (Task 10) are the same numeric contract; Task 11 routes between them by integer value. `taxonomySuggestions` shape starts as `[{name, score}]` (Task 4) and widens to the object form in Task 12; Task 4's tests are updated there, and the wire stays additive in both forms.