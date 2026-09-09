# Triaging legacy mcp-memory bugs in this fork

Audited against fork head `35a4ee3` on 2026-09-08. This report separates an
observed production symptom in the original instance from current fork
behaviour. It is an analysis only; no production code was changed.

## Executive result

| Original item | Current verdict | Priority |
|---|---|---|
| 1. Batch `delete_relations` SQL | Fixed | None |
| 2. `describe_entity` omits graph context | Fixed (`f91a297`) | None |
| 3. Merge creates duplicate relations | Fixed for new writes; legacy-data gap remains | P1 data repair |
| 4. Merge is non-atomic | Fixed | None |
| 5. Merge loses observation provenance | Inherited | P2, schema/API decision |
| 6. Rename/retype absent | Rename inherited; retype works but contract is wrong | P1/P2 |
| 7. Observation time absent from API | Inherited public-model gap | P2, schema/API decision |
| 8. Destructive annotations inconsistent | Fixed | None |

The first remaining safe implementation slice is **#6 contract correction and
rename**. It is useful to graph maintenance, does not require a data migration,
and unblocks use of the current graph. #5 and #7 now have their approved public
model in `2026-09-08-observation-metadata-contract.md`; implementation still
depends on graph-bootstrap ordering.

## Verified verdicts

### 1. Batch relation deletion — fixed

`GraphHandle::delete_relations` now routes through the typed
`MutationRequest::DeleteRelations` boundary
([graph.rs](../../crates/memory-core/src/graph.rs:667)). The historical SQL
builder is gone. The mutation regression suite, including multiple relation
deletion, passed locally: `cargo test --test mutation_service --
--test-threads=1` (8 passed).

Keep a public MCP regression for a two-element `delete_relations` request. The
current core test is good transactional coverage but is not a substitute for
the JSON/action adapter boundary.

### 2. `describe_entity` graph context — fixed

**Superseded observation (audit head `35a4ee3`):** `describe_entity` then
returned only name/type/observations although its tool description promised the
incident graph bundle.

Commit `f91a297` introduced a typed `EntityDescription` assembled in one reader
snapshot: entity observations, all incident relations, distinct neighbour names
and directional `{in, out}` degree. The MCP E2E coverage now exercises inbound,
outbound and self-loop relations. No follow-up work remains for this item.

### 3. Merge relation duplicates — partly fixed, P1 data repair

New relation writes use `INSERT ... SELECT ... WHERE NOT EXISTS` against
`(from_id,to_id,type_id)` ([mutation.rs](../../crates/memory-core/src/mutation.rs:571)).
The merge collision regression passes, so current runtime merge does not add a
new duplicate. Task 2 also preserves physical legacy duplicates correctly when
calculating counters.

The table still lacks `UNIQUE(from_id,to_id,type_id)`. A database already
containing duplicates can retain them, and an unforeseen write path can re-add
them. Before adding a unique index, create an ordered migration that:

1. detects and reports duplicate triples;
2. keeps one deterministic row per triple;
3. reconciles affected graph counters; and
4. creates the unique index.

This must be tested against a fixture with physical duplicates. It is a data
change, so do not execute it without an operator backup/precondition gate.

### 4. Merge atomicity — fixed

Merge now executes through `MutationService`, whose graph changes, derived
counters, change event and job rows share one write transaction. The rollback
regressions in `tests/mutation_service.rs` passed locally (8 passed), including
late failures that roll back preceding graph work. The old three-WAL-commit
failure mode is not present on the current mutation path.

### 5. Observation provenance on merge — inherited, P2 implementation

`observation` has only `id`, `entity_id`, `idx`, `body`, and `created_us`
([graph.rs](../../crates/memory-core/src/graph.rs:455)); inserts persist no
source entity ([mutation.rs](../../crates/memory-core/src/mutation.rs:531)). A
merge cannot tell a later reader where an observation came from.

The approved contract stores nullable `origin_entity_id` plus immutable
`origin_entity_name`, and keeps both null for direct writes. Public MCP objects
expose only `originEntityName`; equal-body merge deduplication retains the target
row and its existing provenance. See
[`2026-09-08-observation-metadata-contract.md`](2026-09-08-observation-metadata-contract.md).

Important migration constraint: the ordered event migrations currently run
before legacy graph bootstrap creates `observation`. An `ALTER observation`
cannot simply be appended to the current migration list; first split/bootstrap
schema initialization so every migration's prerequisites exist.

### 6. Retype and rename — split verdict

Retyping is already implemented: `upsert_entities` updates an existing
entity's type. The tool documentation says its `entityType` applies only at
creation, so the current contract is false and callers cannot safely rely on
the capability.

Rename remains absent. Recommend:

- Correct `upsert_entities` documentation and add a regression proving type
  changes preserve observations and relations.
- Add `rename_entity(oldName,newName)` as an explicit write operation. It must
  reject an existing destination name, update `name` and `name_hash` in one
  mutation transaction, and rebuild/update name FTS without changing identity,
  observations or relation endpoints.

Do not implement rename as merge/delete/recreate; that changes identity and
reintroduces provenance loss.

### 7. Observation timestamps — inherited public-model gap, P2 implementation

The database already writes server `created_us`, but entity reads and MCP
responses expose only observation strings. There is no caller-supplied
`occurred_at`, so neither storage time nor fact time is a usable API field.

The approved model preserves server-owned `created_us`, adds nullable
caller-supplied non-negative `occurred_us`, and exposes canonical structured
observations by default. A deprecated `--legacy-observations` MCP adapter
retains the historical string arrays through 6.x and is removed in 7.0.0.
See [`2026-09-08-observation-metadata-contract.md`](2026-09-08-observation-metadata-contract.md).

It shares the bootstrap-migration prerequisite with #5 and should be one
planned observation-model migration, not two independent ALTERs.

### 8. Tool destructive metadata — fixed

The tool registry categorizes writes centrally; `delete_*`, `merge_entities`
and vector deletion receive destructive metadata while reads remain read-only.
The emitted `tools/list` JSON is deterministic for a fixed configuration.
Client approval behaviour can still differ by MCP client policy, but the server
metadata is no longer the inconsistent source described in the original report.

## Proposed execution order

1. **P1 maintenance contract:** fix `describe_entity`; correct retype
   documentation; add `rename_entity` with FTS and collision regressions.
2. **P1 data integrity:** ship an operator-gated duplicate-relation audit and
   cleanup/unique-index migration after a backup/precondition design review.
3. **P2 observation model implementation:** refactor graph bootstrap/migrations,
   then implement the approved structured observation contract and temporary
   6.x legacy adapter in one additive migration.

## Evidence run by the controller

```text
cargo test --test mutation_service -- --test-threads=1
# 8 passed
cargo test --test e2e e2e_relations_and_describe -- --test-threads=1
# 1 passed
```

Supporting focused audits:

- [Mutation audit](2026-09-08-legacy-mutation-audit.md)
- [Read-model audit](2026-09-08-legacy-read-model-audit.md)
- [Tool metadata audit](2026-09-08-legacy-tool-metadata-audit.md)
