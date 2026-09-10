# Legacy read-model and repair-tool audit

Audited 2026-09-08 against fork commit `35a4ee3` (Task 6). This is a read-only comparison of legacy findings 2, 6 and 7. A stored column that callers cannot read does not resolve a missing public feature.

## Executive result

| Legacy finding | Verdict | What changed in the fork |
| --- | --- | --- |
| 2. `describe_entity` omits relations, neighbors and degree | **Inherited** | The advertised contract is still not implemented. |
| 6. No rename or retype | **Partly fixed / contract stale** | `upsert_entities` changes the type of an existing entity, but no rename operation exists and the MCP manifest says the opposite. |
| 7. Observation timestamps unavailable | **Inherited at the public API** | `observation.created_us` has existed since the legacy schema, but every public entity/read shape discards it; no `occurred_at` exists. |

## 2. `describe_entity` is an incomplete one-shot bundle — inherited

**Evidence.** The manifest promises the entity, every incident relation, distinct neighbor names, and degree (`tools.json:247-256`). The dispatched handler only calls `GraphHandle::describe_entity` and serializes its result (`src/actions/memory.rs:465-475`). That graph method delegates to `get_entity` (`crates/mcpmem-core/src/graph.rs:1559-1562`), whose public shape is `Entity { name, entityType, observations }` (`crates/mcpmem-core/src/types.rs:3-10`). Neither relations, neighbors nor degree can appear in its JSON.

The existing E2E creates A--B and A--C, then asserts only that `describe_entity("A")` contains A; degree is tested in a separate call (`tests/e2e.rs:329-413`, especially 383-398). The focused test passed:

```
cargo test --test e2e e2e_relations_and_describe -- --exact --test-threads=1
# 1 passed
```

**Impact.** A successful response is indistinguishable from an entity with no relations. Agents will reasonably trust the explicitly advertised one-shot contract.

**Suggested implementation.** Introduce an additive `DescribeEntity` response such as `{ entity, relations, neighborNames, degree }`, build it in one read transaction, and reuse existing relation/degree queries. Pin whether degree counts incident directed edges or distinct neighbors (the existing `degree` tool is the compatibility reference). Add an MCP E2E with incoming and outgoing edges that asserts all fields plus a zero-edge case.

**API/migration risk.** No database migration is needed. Extra JSON fields are compatible for permissive clients, but a Rust return-type change and strict consumers require an intentional response type rather than silently changing `Entity` globally.

## 6. Repairing names and types — partly fixed, manifest stale

**Evidence.** No `rename_entity` or `retype_entity` occurs in the registered tool list (`src/tools.rs:76-171`), dispatcher (`src/server.rs:761-812`), or `tools.json`; there is no dedicated MCP repair operation.

However, the mutation path changes an existing entity's `type_id` when an upsert supplies a different `entityType` (`crates/mcpmem-core/src/mutation.rs:615-635`). The core regression test proves `old` becomes `new` and observations persist (`crates/mcpmem-core/src/graph.rs:2389-2409`):

```
cargo test -p mcpmem-core test_upsert_entities -- --test-threads=1
# 1 passed
```

The MCP schema nevertheless says existing entities “keep their type” and `entityType` applies only on creation (`tools.json:277-297`). That statement is false on this fork.

**Impact.** Retyping works today through `upsert_entities({name, entityType: newType, observations: []})`, preserving relations and observations, but is undiscoverable and coupled to an idempotent fact-write tool. Rename remains impossible without destructive reconstruction/merge.

**Suggested implementation.** Correct the `upsert_entities` description and field description, then add explicit `retype_entity(name, newType)` and `rename_entity(oldName, newName)` mutation variants for auditable intent. Rename must atomically update `name` and `name_hash`, reject active target-name collision, and preserve the numeric entity id so relations, revisions, index jobs and vectors stay attached. Retype should also update `updated_us`; the current type UPDATE does not (`crates/mcpmem-core/src/mutation.rs:619-624`).

**API/migration risk.** The explicit tools are additive; fixing the manifest corrects a contract without behavior change. Rename needs no schema migration, but must use `MutationService` so durable event/index/webhook outboxes see it. Decide whether a rename is a normal update event (recommended) and whether aliases exist; none exists now.

## 7. Observation timestamps — inherited at the MCP boundary

**Evidence.** Storage has a non-null creation timestamp: `observation.created_us` is declared in the primary schema (`crates/mcpmem-core/src/graph.rs:455-464`) and insertion supplies `now_us()` (`crates/mcpmem-core/src/mutation.rs:516-538`). This was already present in legacy commit `d6fe34b`.

The read model selects only `body` (`crates/mcpmem-core/src/mutation.rs:309-329`) and serializes observations as `Vec<String>` (`crates/mcpmem-core/src/types.rs:3-10`). `describe_entity` returns that same model. The new event log's `occurred_at_us` timestamps mutations, not individual observations (`crates/mcpmem-core/src/events.rs:100-142`; `migrations/0001_change_events.sql:6-14`).

**Impact.** Callers cannot export, filter or sort observations by recorded time, or distinguish server-recorded time from fact time. Existing `created_us` is operational metadata only.

**Suggested implementation.** Add an `Observation` read/write type with `body`, server-owned `createdAtUs`, optional caller-provided `occurredAtUs`; add nullable `occurred_us` to `observation`; retain immutable `created_us`. Backfill `occurred_us` as NULL, not `created_us`. Preserve string arrays for legacy endpoints and expose metadata by opt-in response/flag or a new read tool until clients migrate. Validate supplied fact times; never let callers set creation time.

**Migration/API risk.** This is a schema and public-contract change. Use a new numbered startup migration (`ALTER TABLE observation ADD COLUMN occurred_us INTEGER`), not an edit to migration 1: migration ordering and checksum validation are enforced at `crates/mcpmem-core/src/events.rs:38-82`. Changing `Entity.observations` directly from strings to objects would break MCP responses, event payloads and indexer content.

## Recommended order

1. Fix finding 2 and its E2E: already-promised read contract, no storage migration.
2. Correct the false upsert manifest immediately, then add explicit rename/retype with event/outbox coverage.
3. Design observation metadata as a versioned backward-compatible migration/API task; it has the widest downstream blast radius.

## Reproducibility checks

```
cargo test --test e2e e2e_relations_and_describe -- --exact --test-threads=1
cargo test -p mcpmem-core test_upsert_entities -- --test-threads=1
```

Both passed. Their weakness is evidence: neither asserts the advertised `describe_entity` bundle, and only the core (not MCP) test proves retype.
