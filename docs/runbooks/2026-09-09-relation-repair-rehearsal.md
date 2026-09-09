# Relation-repair rehearsal — 2026-09-09

This record answers the final release-gate item of
`docs/superpowers/plans/2026-09-08-legacy-graph-maintenance.md`: an operator
backup and repair rehearsal on a non-production database.

The rehearsal did not use a hand-written fixture. The source database comes from
the pre-change binary, `mcp-memory 5.2.1` at commit `a140755`, through a
throwaway example that calls `create_entities`, `create_relations` and
`merge_entities`. The result is a real legacy database: `relation` has no unique
index, and `schema_migration` holds versions 1 and 2 only.

Seeded state: 199 entities, 199 relation rows, 600 observation rows.

The commands are identical in Bash and fish.

## 1. Damage that a historical write race leaves behind

Four extra physical relation rows were inserted for existing triples: three with
a later `created_us`, one with an equal `created_us`, and one with an earlier
`created_us`. Denormalized counters were skewed: `graph_stat.relations`, one
`type_dict.count`, one `entity.out_deg`, and one `entity.in_deg`.

## 2. Audit before repair

```sh
mcp-memory-maintenance relation-audit --database /tmp/legacy-rehearsal/damaged.db --format json
```

```json
{"duplicate_groups":4,"duplicate_rows":4,"dangling_relation_rows":0,
 "drift":{"graph_stat_relations":{"stored":202,"actual":203},
 "type_dict_count":2,"entity_out_deg":4,"entity_in_deg":5}}
```

## 3. Refusals, with the source database unchanged

| Case | Result | Exit |
| --- | --- | --- |
| No `--confirm` | `Invalid params: relation repair requires --confirm` | 1 |
| Backup path already exists | `IO error: File exists (os error 17)` | 1 |
| No `--backup` | `Invalid params: relation repair requires a --backup path` | 1 |

The source kept 203 relation rows after all three refusals.

## 4. Repair

```sh
mcp-memory-maintenance relation-repair \
  --database /tmp/legacy-rehearsal/damaged.db \
  --backup /tmp/legacy-rehearsal/backup-1.db \
  --confirm
```

The command returned `before` and `after` audits. The `after` audit reports zero
duplicate rows, zero dangling rows and zero drift, with
`graph_stat.relations` stored and actual both 199.

Verified after the repair:

- `relation_unique_triple` exists.
- The backup holds the pre-repair state: 203 relation rows, and
  `PRAGMA integrity_check` returns `ok`.
- Every surviving row is the group minimum of `created_us`. Four duplicate
  groups matched their expected keeper exactly.
- A second repair with a new backup path is a clean no-op.
- A manual duplicate insert now fails with
  `UNIQUE constraint failed: relation.from_id, relation.to_id, relation.type_id`.

## 5. Dangling rows abort the repair

One entity row was deleted directly through SQLite to orphan two relation rows.

```json
{"duplicate_groups":0,"duplicate_rows":0,"dangling_relation_rows":2, …}
```

Repair refused with `relation repair refused: dangling relation rows require
explicit remediation` and exit code 1. The source kept 199 relation rows. The
verified backup stays on disk, as section 3 of the repair runbook states; a
later attempt needs a new backup path.

## 6. The 6.0.0 server on the same legacy database

The release binary opened an unrepaired copy of the legacy database.

- `schema_migration` holds versions 1, 2 and 3.
- No unique index was created at startup. Normal startup performs no repair.
- All 199 relation rows survived.
- `observation` gained `origin_entity_id`, `origin_entity_name` and
  `occurred_us`.

MCP results on that database:

- Default mode returns structured observations. Legacy rows keep their true
  `createdAtUs`, with `occurredAtUs` and `originEntityName` null.
- `--legacy-observations` returns the historical `observations: string[]`.
- `rename_entity` returned the renamed entity with its observations, and
  `search_relations` reported the edge under the new name.
- `add_observations` accepted `occurredAtUs` and stored it beside a server-set
  `createdAtUs`.
- `merge_entities` copied source observations with
  `originEntityName: "entity_10"`, and left target rows with a null origin.

## 7. One rehearsal artifact that is not a defect

A first attempt piped all requests at once into stdio with the default
concurrency of 8. The server answered out of order, so a read that was written
after the rename ran before it and reported the old state. The rehearsal in
section 6 uses `--stdio-concurrency 1`. Request order across concurrent stdio
requests is a client responsibility.
