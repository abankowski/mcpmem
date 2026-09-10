# mcpmem-core

The transactional SQLite knowledge-graph core of
[mcpmem](https://github.com/abankowski/mcpmem), an MCP server that gives LLM
agents persistent memory.

This crate is a library. It holds no transport and no MCP layer. Install the
server crate instead if you want to run the server:

```sh
cargo install mcpmem
```

## What this crate owns

- **The graph.** Entities, relations and observations, with the FTS5
  projections over names and observation bodies.
- **The schema bootstrap.** `schema::initialize_database` creates the base
  tables, the full-text tables, the triggers and the seeded counters in one
  transaction, then applies the ordered migrations. Every runtime role calls it,
  so a worker process and the server agree on the database layout.
- **The migrations.** `events::MIGRATIONS` embeds the SQL files under
  `migrations/`. The runner records each version in `schema_migration` with a
  SHA-256 checksum. A changed checksum for an applied version stops startup, and
  a database newer than the binary stops startup.
- **The mutation service.** Every mutation runs in one `BEGIN IMMEDIATE`
  transaction that covers the graph rows, the derived counters, the change
  events and the index jobs. A guard rolls back on any later failure, so a
  partial write cannot happen.
- **The change log and the outbox.** `change_event` is immutable and a trigger
  protects it. `event_outbox` and `index_job` carry the durable work for the two
  worker crates.
- **The relation-integrity repair.** The audit and the backup-gated repair that
  the `mcpmem-maintenance` binary runs.

## Data model

```
Entity(name, entityType, observations[])   ──relationType──▶   Entity(...)
```

- **Entity** — a named node with a type, for example `person`, `company` or
  `project`. Names are unique and case-sensitive.
- **Relation** — a directed edge `(from, to, relationType)`. Traversal follows
  both directions.
- **Observation** — a fact attached to an entity. An observation carries a
  `body`, the server-owned `createdAtUs`, an optional caller-supplied
  `occurredAtUs`, and `originEntityName` when a merge copied it.

Search uses FTS5 with `unicode61 remove_diacritics 2` tokenization. Names and
observation bodies live in separate external-content FTS5 tables, `name_fts` and
`obs_fts`.

## Tables

| Table | Purpose |
|---|---|
| `entity` | Primary storage; materialized `obs_count`, `out_deg`, `in_deg`; `name_hash` for O(1) routing |
| `observation` | Observations of one entity, with `created_us`, `occurred_us` and the merge origin |
| `relation` | Directed edges with covering indexes. A new database also gets `UNIQUE INDEX relation_unique_triple` |
| `name_fts`, `obs_fts` | External-content FTS5 over names and observation bodies |
| `type_dict` | Interned entity and relation types with live counts |
| `graph_stat` | `WITHOUT ROWID` counters: entities, relations, observations, sequences |
| `change_event` | The immutable change log |
| `event_outbox` | Pending webhook deliveries, with leases |
| `index_job` | Pending embedding work, one row per entity and profile |
| `webhook_subscription` | The subscriptions that receive change events |

## Concurrency

`GraphHandle` holds one writer connection behind a mutex, plus a read-only
connection pool for concurrent reads under WAL. A reader that needs a
consistent bundle, for example `describe_entity`, takes one snapshot.

## Documentation

The full server documentation is in the
[workspace README](https://github.com/abankowski/mcpmem#readme). The release
notes are in
[CHANGES.md](https://github.com/abankowski/mcpmem/blob/main/CHANGES.md).

## License

Apache-2.0. See
[LICENSE](https://github.com/abankowski/mcpmem/blob/main/LICENSE) and
[NOTICE](https://github.com/abankowski/mcpmem/blob/main/NOTICE), which records
the derivation from `corporatepiyush/mcp-memory` 5.2.1.
