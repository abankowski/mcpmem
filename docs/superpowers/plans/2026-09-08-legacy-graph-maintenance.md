# Legacy graph maintenance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the remaining inherited graph-maintenance work: truthful retyping, a safe rename operation, durable relation uniqueness, and an auditable observation model.

**Architecture:** Keep identity as the existing numeric `entity.id` and exact case-sensitive name. Rename is an ordinary transactional mutation that changes only the name mapping and FTS projection. Relation cleanup is an operator-invoked, backup-gated repair rather than an automatic startup migration. Observation provenance and fact time form one additive model migration after its public contract is decided.

**Tech Stack:** Rust 2024, `rusqlite`/SQLite WAL and FTS5, serde JSON, MCP tool manifests, existing transactional `MutationService` and outbox.

**Spec:** `docs/analysis/2026-09-08-inherited-legacy-bugs-triage.md` (the plan supersedes its stale #2 verdict: `describe_entity` was fixed in `f91a297`).

**Release policy:** The crate is `mcpmem`. The structured-observation contract and webhook envelope v2 are released as `1.0.0`; `--legacy-observations` remains available throughout `1.x` and is removed in `2.0.0`.

**Superseded (2026-09-10):** the earlier release policy kept the product name `mcp-memory`, released `6.0.0`, and removed `--legacy-observations` in the next major version. The upstream crate name `mcp-memory` is taken, so a parallel `6.x` version line is not publishable. It also collides with the upstream numbering.

## Global Constraints

- Preserve exact, case-sensitive entity-name identity; never implement rename as delete/recreate or merge.
- Every normal graph mutation uses the existing write transaction, emits events and index jobs atomically, and keeps reader snapshots valid.
- Relations are unique by `(from_id, to_id, type_id)` after repair; no server startup path may delete legacy rows automatically.
- A destructive repair requires a verified, newly-created backup plus a read-only preflight; it must abort on integrity failures.
- All MCP reads and mutation results containing observations use the canonical structured contract by default: `get_entity`, `describe_entity`, `read_graph`, JSON `export_graph`, and mutation results. `--legacy-observations` is the temporary server-wide compatibility adapter: it switches all of those schemas plus observation input to historical `observations: string[]`; default startup does not. It remains through `1.x` and is removed in `2.0.0`.
- `--legacy-observations` is valid only when the MCP role is selected; startup rejects it for worker-only role sets.
- `created_us` is server-owned microseconds since Unix epoch; fact time is nullable microseconds since Unix epoch.
- FTS and vector/indexer canonicalization consume observation `body` only; timestamps and provenance are retrieval/audit metadata and never trigger a semantic reindex.
- Commands below are identical in Bash and fish.
- Each implementation task is test-first, reviewed independently, formatted with `cargo fmt --all --check`, and linted with `cargo clippy --workspace --all-targets --all-features -- -D warnings`.

---

## Status ledger

- [x] #1 batch `delete_relations`: fixed.
- [x] #2 `describe_entity`: fixed in `f91a297`; relations, unique neighbours and directional degree are one reader snapshot.
- [x] #4 atomic merge: fixed.
- [x] #8 destructive tool annotations: fixed.
- [x] #6a correct `upsert_entities` retype contract (`34a1e47`).
- [x] #6b add `rename_entity` (`153bb8a`).
- [x] #3 repair historical relation duplicates and enforce uniqueness (`ca865a3`).
- [x] #5/#7 decide and implement observation provenance/timestamps (`4b728a4`, `cc26fb5`, `8cd8e9b`).

## Execution status — 2026-09-09

| Task | Status | Evidence |
| --- | --- | --- |
| 1 | complete | `34a1e47`; scoped review clean |
| 2 | complete | `153bb8a`; scoped review clean |
| 3 | complete | `d643d58`; scoped review clean |
| 4 | complete | `ca865a3`; scoped review clean |
| 5 | complete | approved contract in `docs/analysis/2026-09-08-observation-metadata-contract.md` |
| 6 | complete | `4b728a4`, `cc26fb5`, `8cd8e9b`; two scoped fix rounds clean |
| final fix-wave | complete | `d63a285`; vector rename-cache regression and scoped re-review clean |

**Final review:** the whole-branch review found and reproduced one important
vector cache alias after rename. `d63a285` resolves all operational vector and
code-search names from current SQLite state rather than diagnostic cache maps.
The scoped re-review approved the fix. Fresh final commands completed with exit
0: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
--all-features -- -D warnings`, and `cargo test --workspace --all-targets
--all-features -- --test-threads=1`.

**Checkbox bookkeeping:** the per-step boxes in the task sections below stayed
unchecked during execution. The table above, the status ledger, and the commit
ids are the authoritative record. The release-gate boxes at the end of this file
are maintained.

**Release gate — 2026-09-09:** both remaining gate items are closed. The full CI
matrix ran on `d63a285` with exit 0, and the operator rehearsal ran against a
database written by the pre-change `5.2.1` binary. The branch is ready for a
pull request; the merge decision belongs to the repository owner.

### Task 1: Reconcile the audit and make retyping contractual

**Files:**
- Modify: `docs/analysis/2026-09-08-inherited-legacy-bugs-triage.md`
- Modify: `tools.json` (`upsert_entities` entry)
- Modify: `crates/mcpmem-core/src/graph.rs` (core regression module)
- Modify: `tests/e2e.rs` (only if the existing MCP assertion does not cover the final response)

**Interfaces:**
- Consumes: `MutationRequest::UpsertEntities { entities: Vec<Entity> }`.
- Produces: documented guarantee: an existing exact-name entity is retyped when `entityType` differs and receives only novel observations.

- [ ] **Step 1: Correct the status ledger in the audit.** Mark #2 fixed, record commit `f91a297`, and retain the historical observation in a short “superseded” note rather than deleting it.
- [ ] **Step 2: Write a failing core regression.** Create entity `A` as `OldType` with observation `old`; upsert `A` as `NewType` with `old` and `new`. Assert: one entity; type is `NewType`; observations are exactly `old`, `new`; `OldType` count falls by one; `NewType` count rises by one; relation endpoints are unchanged.
- [ ] **Step 3: Run the targeted test before changing production code.**

  ```text
  cargo test -p mcpmem-core test_upsert_entities -- --exact --nocapture
  ```

  Expected: the new assertion exposes the current missing or incomplete guarantee. If it already passes, retain it as a characterization test and record that existing implementation satisfies the contract.
- [ ] **Step 4: Change only the manifest wording.** Replace “existing entities keep their type” and “entityType is only applied on creation” with: “existing entities are retyped when `entityType` differs; observations are added only when new.” Keep idempotency annotation true.
- [ ] **Step 5: Verify the manifest through MCP.** Assert `tools/list` exposes the corrected description and an MCP `upsert_entities` call returns `NewType` while retaining both observations.
- [ ] **Step 6: Run checks and commit.**

  ```text
  cargo fmt --all --check
  cargo test -p mcpmem-core test_upsert_entities -- --exact
  cargo test --test e2e e2e_upsert_merge_and_wipe -- --exact --test-threads=1
  git add docs/analysis/2026-09-08-inherited-legacy-bugs-triage.md tools.json crates/mcpmem-core/src/graph.rs tests/e2e.rs
  git commit -m "docs: make upsert retype contract explicit"
  ```

### Task 2: Add transactional `rename_entity`

**Files:**
- Modify: `crates/mcpmem-core/src/mutation.rs`
- Modify: `crates/mcpmem-core/src/events.rs`
- Modify: `crates/mcpmem-core/src/graph.rs`
- Modify: `crates/mcpmem-webhook/src/lib.rs`
- Modify: `src/actions/memory.rs`
- Modify: `src/server.rs`
- Modify: `src/tools.rs`
- Modify: `tools.json`
- Modify: `tests/mutation_service.rs`
- Modify: `tests/event_outbox.rs`
- Modify: `tests/webhook_outbox.rs`
- Modify: `tests/e2e.rs`
- Modify: `README.md`

**Interfaces:**
- Produces core API: `GraphHandle::rename_entity(&self, old_name: &str, new_name: &str) -> Result<Entity>`.
- Produces mutation: `MutationRequest::RenameEntity { old_name: String, new_name: String }`.
- Produces MCP tool: `rename_entity({ "oldName": string, "newName": string }) -> Entity`.
- Tool annotations: `readOnlyHint: false`, `destructiveHint: false`, `idempotentHint: false`; same-name is a successful no-op but does not make general rename idempotent.
- Produces `ChangeOperation::Rename` for the renamed entity. Worker deliveries use redacted webhook envelope `version: 2` for every operation; `oldName` and `newName` are non-null only for rename. The envelope never carries an entity snapshot or observation bodies. Existing v1 envelopes are a receiver-side transition/replay concern, not a second server emission mode.
- Subscriptions without an operation filter receive rename events; existing operation filters accept and exactly match the added `rename` value.

- [ ] **Step 1: Write failing mutation tests.** Cover: incoming, outgoing and self relations survive rename; numeric id and observations survive; exact old name is unreadable/search-unmatched; exact new name is readable/search-matched; `name_hash` changes; an existing distinct destination returns `InvalidParams("Entity '<new>' already exists")` without changing entity, FTS, events or jobs; same-name is a no-op.
- [ ] **Step 2: Run the focused tests and observe the missing request/API fail.**

  ```text
  cargo test --test mutation_service rename -- --nocapture
  ```

- [ ] **Step 3: Implement the mutation in the existing write transaction.** Add the enum variant, include old/new names and incident neighbours in affected-name snapshots, reject distinct collision, update `entity.name` and `entity.name_hash`, and replace the `name_fts` posting for the stable row id. Do not alter relation rows, observation rows, types, counts or `updated_us`.
- [ ] **Step 4: Wire the GraphHandle, action, dispatcher, category registry and manifest.** Validate both names with the existing boundary validator. Add `ChangeOperation::Rename` and optional `oldName`/`newName` event fields through durable event/outbox/webhook code; do not synthesize source delete/create events or neighbour updates. Every worker delivery becomes envelope `version: 2`; it remains redacted and carries no snapshot or observation bodies.
- [ ] **Step 5: Add outbox and MCP E2E regressions.** Assert one durable rename event with old/new name data and no neighbour events; assert v2 envelopes contain existing fields and `oldName`/`newName: null` for non-rename operations; assert redaction, the source’s final coalesced index job is an `upsert`, manifest category gating exposes the tool, and JSON result contains the new-name entity.
- [ ] **Step 6: Run checks and commit.**

  ```text
  cargo fmt --all --check
  cargo test --test mutation_service -- --test-threads=1
  cargo test --test event_outbox -- --test-threads=1
  cargo test --test e2e -- --test-threads=1
  git add crates/mcpmem-core/src/mutation.rs crates/mcpmem-core/src/events.rs crates/mcpmem-core/src/graph.rs crates/mcpmem-webhook/src/lib.rs src/actions/memory.rs src/server.rs src/tools.rs tools.json tests/mutation_service.rs tests/event_outbox.rs tests/webhook_outbox.rs tests/e2e.rs README.md
  git commit -m "feat: add transactional entity rename"
  ```

### Task 3: Make graph bootstrap a prerequisite of ordered migrations

**Files:**
- Modify: `crates/mcpmem-core/src/graph.rs`
- Modify: `crates/mcpmem-core/src/events.rs`
- Create: `crates/mcpmem-core/src/schema.rs`
- Modify: `crates/mcpmem-core/src/lib.rs`
- Modify: `crates/mcpmem-indexer/src/lib.rs`
- Modify: `crates/mcpmem-webhook/src/lib.rs`
- Modify: `tests/event_outbox.rs`
- Modify: `tests/webhook_outbox.rs`

**Interfaces:**
- Produces `schema::initialize_database(&Connection) -> Result<()>`, the one shared mcpmem-core initializer that creates legacy graph tables/FTS/triggers and applies ordered migrations before readers or workers operate.
- Consumes the existing `schema_migration(version, checksum, applied_at_us)` ledger without changing checksums for versions 1 and 2.

- [ ] **Step 1: Write failing compatibility tests.** Construct: a fresh database; a legacy graph database with graph tables but no migration ledger; and an existing v1/v2 database. Assert initialization leaves each readable, verifies historical checksums, and supplies graph tables before migration code can reference them. Exercise indexer and webhook worker reopen paths.
- [ ] **Step 2: Run those tests to show the current ordering gap.**

  ```text
  cargo test --test event_outbox startup_rejects_changed_migration_and_preserves_legacy_vector_rows -- --exact --nocapture
  cargo test --test webhook_outbox migration_from_a_real_0001_database_applies_only_0002 -- --exact --nocapture
  ```

- [ ] **Step 3: Extract base graph DDL and `graph_stat` seeding from `GraphHandle::new` into `schema::initialize_database`.** Call it before `events::migrate()` from GraphHandle and replace worker-only `events::migrate()` calls with the same initializer. Keep normal startup non-destructive.
- [ ] **Step 4: Preserve migration-runner semantics explicitly.** Decision (2026-09-08): keep ordered migrations append-only and checksum-verified, and apply the complete set of pending normal startup migrations in one transaction. Document and test that no partial schema version is visible after failure.
- [ ] **Step 5: Run compatibility checks and commit.**

  ```text
  cargo fmt --all --check
  cargo test --test event_outbox -- --test-threads=1
  cargo test --test webhook_outbox -- --test-threads=1
  cargo test --test indexer_worker --features indexer -- --test-threads=1
  git add crates/mcpmem-core/src/graph.rs crates/mcpmem-core/src/events.rs crates/mcpmem-core/src/schema.rs crates/mcpmem-core/src/lib.rs crates/mcpmem-indexer/src/lib.rs crates/mcpmem-webhook/src/lib.rs tests/event_outbox.rs tests/webhook_outbox.rs
  git commit -m "refactor: initialize graph schema before migrations"
  ```

### Task 4: Provide an operator-gated relation-integrity repair

**Decision resolved (2026-09-08):** cleanup is an operator command with mandatory backup, preflight and `--confirm`; it is never an automatic startup migration.

**Files:**
- Create: `crates/mcpmem-core/src/relation_integrity.rs`
- Modify: `crates/mcpmem-core/src/lib.rs`
- Modify: `crates/mcpmem-core/src/schema.rs`
- Create: `src/bin/maintenance.rs`
- Modify: `Cargo.toml` (declare binary `mcpmem-maintenance`)
- Modify: `tests/mutation_service.rs`
- Create: `tests/relation_integrity.rs`
- Create: `docs/runbooks/relation-integrity-repair.md`

**Interfaces:**
- Produces standalone, non-server command: `mcpmem-maintenance relation-audit --database <path> --format json`.
- Produces standalone repair command: `mcpmem-maintenance relation-repair --database <path> --backup <new-path> --confirm`.
- Audit JSON reports `duplicate_groups`, `duplicate_rows`, `dangling_relation_rows`, and drift for `graph_stat.relations`, `type_dict.count`, `entity.out_deg`, and `entity.in_deg`. Decision (2026-09-08): any dangling relation makes repair abort before a write; it is reported for a separate explicit remediation.
- Fresh databases receive `UNIQUE INDEX relation_unique_triple ON relation(from_id,to_id,type_id)` through the shared base schema. Repair keeps the lowest `(created_us, rowid)` relation for every duplicate triple, always recalculates derived counters from surviving physical rows even when no duplicate is found, creates that same index on repaired legacy databases, and returns the before/after audit.
- Backup uses SQLite’s online backup API to a `--backup` target that must not exist, then reopens the copy and requires `PRAGMA integrity_check = 'ok'`; raw filesystem copying of a live `.db`/WAL is forbidden.
- The shared initializer determines whether `relation` existed before bootstrap. It creates the fresh-schema unique index only when it did not; a legacy table without that index remains openable until the explicit operator repair creates it last. This is not a normal-startup migration.
- A relation with a missing endpoint, missing type dictionary row, or a type row whose `kind != 1` is an integrity failure. Audit includes it in `dangling_relation_rows`; repair aborts before any source-database write. Repair recomputes all `type_dict.count` values: kind `0` from live entities, kind `1` from surviving physical relations, and any other kind to zero.
- Reserve the backup target with atomic create-new before opening it for SQLite's online backup API; a pre-existing or raced target is refused without writing the source database.

- [ ] **Step 1: Add failing audit/repair fixture tests.** Seed physical duplicate triples with equal and unequal `created_us`, stale counters, and a dangling endpoint. Assert audit counts exactly; assert repair refuses dangling rows; assert repair without `--confirm`, backup, or a newly-created backup file refuses before any write.
- [ ] **Step 2: Run the focused test and observe the absent command/repair API fail.**

  ```text
  cargo test --test relation_integrity -- --nocapture
  ```

- [ ] **Step 3: Implement the read-only audit first.** Use one SQLite read transaction and return deterministic group ordering by `(from_id, to_id, type_id)`. Expose no secret or observation body in output.
- [ ] **Step 4: Implement backup verification and repair.** Require the backup target not to exist; create it through SQLite’s online backup API, reopen it and require `PRAGMA integrity_check = 'ok'` before `BEGIN IMMEDIATE`; reject failed `PRAGMA foreign_key_check`/integrity checks and dangling relations; remove only non-keeper duplicates; recompute counters from surviving physical rows; create the unique index last; verify the post-repair audit is clean; roll back the database transaction on any failure. Do not implement raw filesystem copying of a live database/WAL.
- [ ] **Step 5: Add CLI contract and runbook tests.** Verify JSON schema, refusal modes, idempotent second repair, deterministic keeper, recovery after reopening, and that normal server startup never invokes repair.
- [ ] **Step 6: Run checks and commit.**

  ```text
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --test relation_integrity -- --test-threads=1
  cargo test --test mutation_service -- --test-threads=1
  git add crates/mcpmem-core/src/relation_integrity.rs crates/mcpmem-core/src/lib.rs crates/mcpmem-core/src/schema.rs src/bin/maintenance.rs Cargo.toml tests/mutation_service.rs tests/relation_integrity.rs
  git commit -m "feat: add backup-gated relation integrity repair"
  ```

### Task 5: Decide the observation metadata contract

**Decision resolved (2026-09-08):** all observation storage and public-contract decisions below are approved; implementation may proceed after Task 3 establishes bootstrap ordering.

**Decision record:** add the selected answers to `docs/analysis/2026-09-08-observation-metadata-contract.md` before checking this task complete.

- [x] **Step 1: Choose provenance retention.** Decision (2026-09-08): storage retains `origin_entity_id INTEGER NULL` plus immutable `origin_entity_name TEXT NULL`; merge-copied observations receive both source values, newly-authored observations receive nulls. This survives source deletion while preserving an audit label. Public MCP objects expose only `originEntityName`; the numeric id is internal and never becomes a client identity contract.
- [x] **Step 2: Choose fact-time semantics.** Decision (2026-09-08): `occurred_us INTEGER NULL`; it is caller-supplied UTC microseconds since Unix epoch, must be non-negative, and does not replace server-set immutable `created_us`.
- [x] **Step 3: Choose public compatibility shape.** Decision (2026-09-08): canonical MCP `observations` are structured objects by default: `{ body, createdAtUs, occurredAtUs, originEntityName }`. This applies to all MCP reads and mutation results that contain observations, including JSON exports. `--legacy-observations` switches the whole MCP manifest and observation input/output JSON to the historical `observations: string[]` contract. It is a temporary server-wide adapter, documented as deprecated, available through `1.x` and removed in `2.0.0`; there are no parallel legacy/detail fields in one response.
- [x] **Step 4: Choose legacy-row policy.** Decision (2026-09-08): all new metadata columns are nullable; existing rows retain their true server `created_us` while `occurred_us`, `origin_entity_id` and `origin_entity_name` are null. No dates or provenance are inferred from free text.
- [x] **Step 5: Choose merge deduplication provenance.** Decision (2026-09-08): when target already has an identical observation body, merge creates no duplicate and leaves the existing target provenance unchanged. Provenance records the origin of the stored row, not a multi-origin history for equal text.
- [x] **Step 6: Choose canonical write shape.** Decision (2026-09-08): retain current tool names and envelopes. In canonical mode, entity `observations` and `add_observations.contents` contain `{ body: string, occurredAtUs?: integer }`; `createdAtUs` and `originEntityName` are output-only server fields. Legacy mode restores the historical string elements.
- [x] **Step 7: Record rejected alternatives.** Reject origin-name-only as unstable audit identity, `source_entity_id` with a foreign key as incompatible with source deletion, automatic extraction of dates from free text as non-deterministic, a multi-origin join table for deduplicated equal bodies as disproportionate scope, and separate detailed-write tools as duplicated API.

### Task 6: Add observation provenance and timestamps after Task 5

**Files:**
- Modify: `Cargo.toml` (release the approved breaking contract as `1.0.0`)
- Modify: `Cargo.lock` (update the root package version)
- Create: `migrations/0003_observation_metadata.sql`
- Modify: `crates/mcpmem-core/src/events.rs`
- Modify: `crates/mcpmem-core/src/types.rs`
- Modify: `crates/mcpmem-core/src/mutation.rs`
- Modify: `crates/mcpmem-core/src/graph.rs`
- Modify: `src/actions/memory.rs`
- Modify: `src/actions/code.rs`
- Modify: `src/config.rs`
- Modify: `src/lib.rs`
- Modify: `src/server.rs`
- Modify: `src/bin/bench.rs`
- Modify: `src/ui/graph.js`
- Modify: `tools.json`
- Modify: `tests/mutation_service.rs`
- Modify: `tests/e2e.rs`
- Modify: `tests/event_outbox.rs`
- Modify: affected UI, role, indexer, webhook, vector and core fixture tests

**Implementation findings (2026-09-09):** the earlier file list was incomplete.
The CLI flag is owned by `src/lib.rs` and validated through `src/config.rs`; the
UI directly renders observation values; code ingestion reads/writes observation
bodies; benchmark and cross-crate fixtures construct typed entities.  The six
raw SQLite JSON projections in `graph.rs` must share the structured projection,
or some read paths would silently retain strings.  `src/types.rs` is an unused
duplicate and is deliberately outside this task.

**Interfaces:**
- Produces storage fields: `observation.origin_entity_id`, `observation.origin_entity_name`, `observation.occurred_us`; existing `created_us` stays immutable.
- Produces canonical structured observation input/output by default and a server-wide `--legacy-observations` adapter that supplies the historical string-array schema.
- Produces typed observation input `{ body, occurredAtUs? }`; `createdAtUs` and `originEntityName` are output-only. Legacy string input is accepted only while `--legacy-observations` is active.

- [ ] **Step 1: Write failing migration and contract tests from the selected Task 5 decisions.** Cover fresh/legacy database migration, null legacy metadata, timestamp validation, default structured JSON for `get_entity`/`describe_entity`, legacy string JSON and manifest under `--legacy-observations`, merge provenance, and rollback at the final write.
- [ ] **Step 2: Run the focused tests and confirm the new columns/types are absent.**

  ```text
  cargo test --test mutation_service observation_metadata -- --nocapture
  cargo test --test event_outbox migration -- --nocapture
  ```

- [ ] **Step 3: Add the append-only migration and registry entry.** The migration may run only after Task 3’s shared bootstrap establishes `observation`. Do not edit migrations 0001 or 0002.
- [ ] **Step 4: Implement typed boundary parsing and persistence.** Validate `occurred_us >= 0`; server fills `created_us`; mutation inserts origin metadata only for merge-copied observations; direct writes preserve null origin. If merge finds an equal target body, do not insert a row or alter that row’s provenance.
- [ ] **Step 5: Preserve durable-payload readability independently of the MCP adapter.** `change_event.payload` and `idempotency_record.response` currently contain serialized string observations. Their pre-1.0 representation needs an explicit storage compatibility rule and regression fixtures; the MCP flag must not control whether existing durable records decode. Do not fabricate timestamps absent from a historical payload.
- [ ] **Step 6: Implement the selected structured contract and adapter.** Default mode serializes metadata with explicit nulls for legacy rows. `--legacy-observations` switches manifest schemas and observation input/output to strings. Update exports and UI paths to consume the canonical internal model; preserve FTS and indexer canonicalization as `body`-only text, independent of transport JSON. Reject malformed or mixed observation arrays rather than silently dropping invalid elements.
- [ ] **Step 7: Run checks and commit.**

  ```text
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace --all-targets --all-features -- --test-threads=1
  git add Cargo.toml Cargo.lock migrations/0003_observation_metadata.sql crates/mcpmem-core/src/events.rs crates/mcpmem-core/src/types.rs crates/mcpmem-core/src/mutation.rs crates/mcpmem-core/src/graph.rs src/actions/memory.rs src/actions/code.rs src/config.rs src/lib.rs src/server.rs src/bin/bench.rs src/ui/graph.js tools.json tests/mutation_service.rs tests/e2e.rs tests/event_outbox.rs
  git commit -m "feat: preserve observation provenance and fact time"
  ```

## Final verification and release gate

- [x] Run the exact CI matrix. Executed on `d63a285`, 2026-09-09; every command
  returned exit 0. The workspace run reported 338 passed, 0 failed, 1 ignored
  over 22 test binaries. The ignored case is `subprocess_writer_child`, a helper
  that `concurrent_process_writers_keep_all_events` starts as a separate
  process.

  ```text
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace --all-targets -- --test-threads=1
  cargo test --test indexer_worker --features indexer -- --test-threads=1
  cargo test --test indexer_worker --no-default-features --features indexer -- --test-threads=1
  cargo test --test role_composition --no-default-features
  cargo test --test role_composition --features indexer
  cargo test --test role_composition --features webhooks
  cargo test --test role_composition --features indexer,webhooks
  ```

- [x] Verify an operator backup/restore rehearsal on a copy of a non-production
  database before authorizing the real duplicate repair. The rehearsal used a
  database written by the pre-change `5.2.1` binary at `a140755`, not a fixture.
  Evidence: `docs/runbooks/2026-09-09-relation-repair-rehearsal.md`. Audit,
  three refusal modes, repair, keeper determinism, backup integrity, idempotent
  second repair, the dangling-row abort, and the 1.0.0 server, then called
  6.0.0, on the same legacy database all behaved as the plan specifies.
- [x] Update the status ledger and triage verdicts with commit IDs and executed command output.
