# Relation-integrity repair runbook

`mcp-memory-maintenance` is an offline operator command. It never runs during
server startup and never writes the source database until it has created and
validated a SQLite online backup, acquired the writer lock, and passed its
integrity preflight.

The commands below are identical in Bash and fish.

## 1. Audit first

```sh
mcp-memory-maintenance relation-audit --database /absolute/path/to/memory.db --format json
```

The JSON contains aggregate counts only. `duplicate_groups` and
`duplicate_rows` count physical triples. `dangling_relation_rows` includes
missing endpoints, missing relation types, and types whose `kind` is not `1`.
`drift` compares physical rows with `graph_stat.relations`, all `type_dict`
counts, and entity in/out degree caches.

Do not repair while `dangling_relation_rows` is non-zero; remediate those rows
explicitly first.

## 2. Repair with a new backup

Choose a destination that does not exist and has enough capacity:

```sh
mcp-memory-maintenance relation-repair \
  --database /absolute/path/to/memory.db \
  --backup /absolute/path/to/memory-before-relation-repair.db \
  --confirm
```

The destination is atomically reserved, never overwritten, and populated with
SQLite’s online Backup API rather than a filesystem copy of a live DB/WAL. The
tool reopens it and requires `PRAGMA integrity_check = ok` before source writes.

## 3. Refusals and recovery

JSON reports use stdout; refusals and operational errors use stderr and a
non-zero status. Missing `--confirm`, missing/occupied backup path, source or
backup integrity failures, foreign-key failures, and dangling relations leave
the source unchanged. Keep the verified backup; restoring it is an explicit
operator recovery decision, not an automatic action by this tool.

## 4. Verify success

On success, retain the `before` and `after` audit JSON plus the backup. Re-run:

```sh
mcp-memory-maintenance relation-audit --database /absolute/path/to/memory.db --format json
```

Success has zero duplicate and dangling counts and zero drift. Repair keeps the
lowest `(created_us, rowid)` row per triple, rebuilds caches, and creates the
global `relation_unique_triple` index last. A second repair is idempotent but
requires another new backup destination.
