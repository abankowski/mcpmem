# Observation metadata contract

**Status:** approved 2026-09-08. This is the binding design input for Task 6 in
`docs/superpowers/plans/2026-09-08-legacy-graph-maintenance.md`.

## Storage

Keep immutable server-owned `observation.created_us`. Add nullable columns:

- `origin_entity_id INTEGER NULL` — internal audit identity only;
- `origin_entity_name TEXT NULL` — immutable audit label;
- `occurred_us INTEGER NULL` — caller-supplied UTC microseconds since Unix
  epoch, validated as non-negative.

New direct observations have null origin fields. A merge copies both origin
fields from the source entity when it inserts an observation. If the target
already has an identical body, merge preserves that existing row and its
provenance; it does not construct multi-origin history. Existing rows preserve
their real `created_us` and have all added metadata columns null.

## Canonical MCP contract

Default MCP responses use:

```json
{
  "body": "Client Acme Labs",
  "createdAtUs": 1780000000000000,
  "occurredAtUs": null,
  "originEntityName": null
}
```

`originEntityId` is never public. The canonical representation applies to all
MCP reads and mutation results containing observations: `get_entity`,
`describe_entity`, `read_graph`, JSON `export_graph`, and mutation results.

### Historical durable payloads

Pre-1.0 `change_event.payload` and `idempotency_record.response` records may
embed only legacy observation strings. They remain readable independently of
`--legacy-observations`: v1 adapts each such value to the canonical object,
with `createdAtUs`, `occurredAtUs`, and `originEntityName` explicitly `null`.
The original row may no longer exist, so its time must never be fabricated.
This narrowly scoped nullable `createdAtUs` exception applies only to
historical durable payloads, not to database rows read after migration.

Canonical writes retain existing tool envelopes, but each observation element
is `{ "body": string, "occurredAtUs"?: integer }`. `createdAtUs` and
`originEntityName` are output-only. FTS, vector indexing and indexer
canonicalization index `body` only.

## Legacy adapter

`--legacy-observations` is a server-wide MCP compatibility adapter, valid only
with the MCP role. It switches tool schemas and observation input/output to
the historical `observations: string[]` shape. It is disabled by default,
deprecated throughout 1.x, and removed in 2.0.0. There are never parallel
legacy and structured observation fields in one response.

## Release identity

The crate is `mcpmem`. The structured-observation contract and webhook envelope
v2 are breaking changes released as `1.0.0`.

**Superseded (2026-09-10).** The earlier decision kept the product name
`mcp-memory` and released the same contract as `6.0.0`. The upstream crate name
`mcp-memory` is taken, so a parallel `6.x` version line is not publishable. It
also collides with the upstream numbering. The fork therefore takes a separate
name and a separate version line that starts at `1.0.0`.

## Migration requirements

The metadata migration is append-only and may execute only after the shared
graph bootstrap establishes `observation` before ordered migrations. Normal
startup migrations remain one atomic batch. The relation-integrity repair is
separate, operator-gated work and does not run as a startup migration.

## Rejected alternatives

- Origin name alone: it is not a stable audit identity.
- Foreign-key source ID: source deletion would make valid merge provenance
  impossible.
- Parsing dates/provenance from observation text: non-deterministic.
- Multi-origin table for equal observation bodies: disproportionate scope.
- Separate detailed write tools: duplicates the existing MCP API.
- Embedding metadata with observation text: changes semantic search without a
  retrieval benefit.
