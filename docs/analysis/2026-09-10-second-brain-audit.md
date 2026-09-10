# Audit of the live `second-brain` database — 2026-09-10

Source: `/home/abankowski/second-brain.mcpmem` on host `secondbrain`, captured
with the SQLite online backup API while the server ran. `PRAGMA
integrity_check` returns `ok` on the snapshot.

The snapshot predates migration `0001`: it has no `schema_migration`,
`change_event`, `event_outbox` or `index_job` table, and it still has the old
`vector_embedding` table. Graph tables match the current base schema. 287
entities, 366 relation rows, `relation` without a unique index.

## Audit result

```json
{"duplicate_groups":1,"duplicate_rows":1,"dangling_relation_rows":0,
 "drift":{"graph_stat_relations":{"stored":366,"actual":366},
 "type_dict_count":4,"entity_out_deg":1,"entity_in_deg":1}}
```

The audit is read-only. The snapshot MD5 was unchanged and no `-wal` or `-shm`
file appeared.

### The one duplicate

| from | to | type | rows | created_us |
| --- | --- | --- | --- | --- |
| John Doe | Umbrella Corp | pracuje | 2 | 1788467806192328, 1788469190035628 |

The two writes are about 23 minutes apart. `graph_stat.relations` is correct at
366, so the second insert also incremented the counter.

### The drift

| Object | Stored | Actual |
| --- | --- | --- |
| `type_dict` Osoba (kind 0) | 35 | 32 |
| `type_dict` Organizacja (kind 0) | 31 | 29 |
| `type_dict` Decyzja (kind 0) | 17 | 16 |
| `type_dict` pracuje (kind 1) | 37 | 36 |
| `Tim`.out_deg | 2 | 1 |
| `The Resource Collective`.in_deg | 3 | 2 |

No entity row carries a non-zero `flags` value, so the entity-type overcounts
are not tombstones. Six deleted entities and two removed relation endpoints left
their counters behind.

**The drift is historical.** A create and delete cycle on both the `5.2.1`
binary and the `6.0.0` binary left `type_dict` exact, so no current code path
reproduces it.

## Repair rehearsal on the copy

```json
{"after":{"duplicate_groups":0,"duplicate_rows":0,"dangling_relation_rows":0,
 "drift":{"graph_stat_relations":{"stored":365,"actual":365},
 "type_dict_count":0,"entity_out_deg":0,"entity_in_deg":0}}}
```

- The kept row is the earlier `1788467806192328`. The two other `pracuje`
  relations of the same entity, to `Globex` and to
  `Projekt Acme Inc. - augmentacja zespołów`, are untouched.
- The backup keeps all 366 rows and passes `PRAGMA integrity_check`.
- The table list is byte-identical before and after. Repair opens the file
  directly and applies no migration; it adds only `relation_unique_triple`.
- Both binaries still write to the repaired copy. `create_relations` for the
  duplicate triple returns `[]`, and create plus delete succeed.
- The `6.0.0` binary rejects a string observation with `expected struct
  ObservationInput`. That is the intended `6.0.0` contract, not a repair effect.

## Operator procedure for the live file

Stop the server first. Repair takes the writer lock, and a running server holds
it.

```sh
mcp-memory-maintenance relation-repair \
  --database /home/abankowski/second-brain.mcpmem \
  --backup /home/abankowski/second-brain-before-repair.mcpmem \
  --confirm
```

The binary must be built for the host architecture. Keep the backup until the
next server start is verified.
