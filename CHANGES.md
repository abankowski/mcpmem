# Changes

`mcpmem` 1.0.0 is the first release of this project. It starts from
[`corporatepiyush/mcp-memory`](https://github.com/corporatepiyush/mcp-memory)
version 5.2.1, commit `d6fe34b`. The license stays Apache-2.0, and
[`NOTICE`](NOTICE) records the derivation.

This entry lists every change since that snapshot. The version line restarts at
1.0.0, because the upstream crate name belongs to another author.

## Unreleased

### Added

- **Admin UI at `/ui/admin`:** principals management with OAuth (new `admin`
  scope), runtime principals in SQLite, JSON entries immutable built-ins,
  optional approval waitlist (`[oauth] approval-waitlist`).
- **`MCSError::ConstraintViolation` in `mcpmem-core`.** A new error variant
  (JSON-RPC code `-32005`) for a write refused by a server-side rule, such as
  a runtime principal colliding with a built-in. A new public variant is
  semver-visible for the crate; the release flow decides the bump.
- **Webhook delivery ships in the binary.** The `webhooks` role no longer
  aborts with `webhook role selected without configured worker ports`; it runs
  the real worker. Configure an optional `[webhooks]` section in the TOML
  file: `allowlist` lists the HTTPS hostnames the worker may deliver to, and
  `secrets` maps each `secret_ref` to the file holding its signing key. The
  section is fail-closed by default — empty means nothing is delivered.
- **`webhook_add_subscription` and `webhook_delete_subscription`.** Two MCP
  tools, behind the `webhooks` Cargo feature, manage the
  `webhook_subscription` table. The add tool validates the endpoint with the
  worker's own `validate_endpoint` rule (https, port 443, hostname, no
  fragment), and `secretRef` is a name, never key material.
- **Webhook subscriptions in the admin UI.** `/ui/admin` lists, creates,
  edits and deletes `webhook_subscription` rows through the new
  `GET/POST /ui/api/webhooks` and `PATCH/DELETE /ui/api/webhooks/{id}`
  endpoints, behind the same `admin` scope as the principals pages. The
  admin API runs the MCP tools' own validation, so the two surfaces accept
  the same payloads, and `secretRef` stays a name. A build without the
  `webhooks` Cargo feature answers 404 and the page hides the section.

- **Scala support in the code indexer.** The tree-sitter grammar set grows
  from 11 to 12; `code_index` now parses `.scala` and `.sc` files into the
  symbol map (packages, classes, objects, traits, enums, functions, vals,
  vars, type aliases and constructor parameters). The tags query is vendored
  in `src/code/lang.rs` because the published `tree-sitter-scala` crate
  exports no `TAGS_QUERY` constant; it is a verbatim copy of the upstream
  `queries/tags.scm`, cited in-repo.

- **The server computes embeddings.** Name `provider`, `model` and
  `dimensions` in the `[indexer]` section, and the server adopts an index
  profile at startup and embeds entity text by itself. Adoption compares the
  profile fingerprint, so an unchanged file is a no-op and only a real change
  to the provider, the model, the dimension, the normalization or the metric
  starts a rebuild. A rebuild re-embeds every live entity, and it ends legacy
  compatibility: `vector_upsert_embedding` and `vector_batch_upsert` are then
  refused with `direct_vector_writes_disabled`. `normalization` defaults to
  `l2` and `metric` to `cosine`. A half-written profile is a startup error
  that names the missing key.
- **`semantic_search`.** The read-side counterpart: it takes `queryText` and
  embeds it on the server, through the provider the serving profile names.
  `textWeight` and `vecWeight` fuse the result with FTS5, as `hybrid_search`
  does; without them the tool runs a pure vector search. The tool is hidden
  from `tools/list` on a build without the `indexer` feature, and on any
  process that configured no provider, so a client never sees a tool that
  cannot answer.
- **`EmbeddingProvider::embed_texts`.** Text is now the trait primitive, and
  `embed` is a default body over it. A query has no entity and no revision,
  and `CanonicalDocument` carries both for revision fencing, so a read path
  must not fabricate one.
- **A TOML configuration file.** `--config <PATH>`, or the `MCP_MEMORY_CONFIG`
  environment variable, names a file that carries every setting that is not a
  secret. `--auth-token` has no file key, by design, and `--config` has none
  either. Precedence is a flag, then an environment variable where the
  setting reads one, then the file, then the default; a flag wins even when it
  repeats the default value, because the merge reads clap's value source rather
  than comparing against defaults. A named file that is absent, an unknown key
  and an unknown section are all startup errors. There is no implicit search
  path, so a stray file in the working directory cannot change a deployment.
  The file names secret files and never holds a secret.
  [`mcpmem.example.toml`](mcpmem.example.toml) ships every key, commented out.
- **`--durability`.** The SQLite synchronous mode was reachable only through
  `MCP_MEMORY_DURABILITY`. The flag and the `[storage] durability` key reject a
  bad value; the environment variable keeps its older warn-and-continue
  behaviour, so a typo there cannot stop a restart.
- **`ProviderRegistry::from_settings` and `ProviderSettings`** in
  `mcpmem-indexer`. The embedding worker can now be built from settings the
  caller resolved, so a configuration file reaches the provider without writing
  back into the process environment. `from_environment` delegates to it.

### Fixed

- **The `indexer` role could never start with a provider configured.** Every
  provider holds a `reqwest::blocking::Client`, whose builder creates and
  drops a temporary Tokio runtime. Dropping a runtime inside an async context
  panics, and the registry was built inside `inner_main`, which runs under
  `block_on`. Startup aborted with `Cannot drop a runtime in a context where
  blocking is not allowed`. The registry is now built inside
  `spawn_blocking`, which is not an async context.
- **The OpenAI provider silently ignored the profile's dimension.** The
  request body carried `model` and `input` but never `dimensions`, so
  `text-embedding-3-small` returned its 1536-dimension default regardless of
  what the `[indexer]` profile declared. Every embedding then failed
  `validate_vector`, the job retried forever with only a `last_error` row in
  the database, the queue never drained, and the full-scan gate blocked the
  snapshot from ever publishing — the store served no vectors. The body now
  sends the profile's `dimensions`. The live API was probed to confirm both
  ends: the field is honored by `text-embedding-3-small` and reduces the
  return to the declared length.
- **The OpenAI provider's L2-normalization contract was enforced without
  normalization.** The profile promises unit-norm vectors (`|norm-1| < 1e-4`)
  and `validate_vector` enforces it at commit, but the worker stored
  whatever the provider returned. OpenAI's embeddings are only *roughly*
  unit-norm — measured off by up to 5e-4 on the live API — so jobs
  committed or failed depending on per-vector rounding. The worker now
  normalizes to unit length before commit when the profile declares L2,
  so the provider's approximation can no longer fail the gate.
- **A permanently failing job blocked the whole store, silently.** The worker
  retried a failed job every second with no console output and no bound, and
  the full-scan gate requires *every* job to be `done`. One poisoned entity
  therefore kept the snapshot unpublished forever. Failures are now logged at
  `warn` with the entity id and the error, and after `MAX_ATTEMPTS` failures
  the job is dead-lettered (`state='dead'`) and logged at `error`. The gate
  ignores dead jobs, and dead-lettering drops the entity's stale vector row so
  a published snapshot cannot serve an outdated embedding. The next write to
  the entity re-enqueues it with a fresh attempt budget.

### Documentation

- **The README describes the build features, the runtime roles and every
  setting.** New sections: the Cargo feature matrix with install recipes, the
  `--role` table with both deployment shapes and the exact startup errors, the
  embedding-worker environment, and a full configuration reference.
- **Two limitations are written down rather than implied.** The `indexer` role
  polls and stays idle, because no shipped command creates an index profile and
  every job therefore stays `held`. The `webhooks` role fails at startup in the
  shipped binary, because it has no configured worker ports.
- **Three stale tool descriptions are corrected.** `hybrid_search` said it runs
  the two searches "simultaneously" and boosts by centrality "optionally"; it
  runs them in sequence and always boosts. `vector_store_stats` listed only
  `hnsw` and `ivf` for `indexKind`, omitting `turboquant`. `vector_reindex`
  called itself a no-op for HNSW alone, while TurboQuant is equally a no-op and
  `reindexed` is `true` in every case. The README claim that the server "does
  not call an embedding model" now says which build and which path that covers.
  The evidence is in
  [`docs/analysis/2026-09-11-query-side-embedding.md`](docs/analysis/2026-09-11-query-side-embedding.md).
- **The vector tools' three gates are written down.** `tools/list` shows a
  vector tool only when the category is enabled on the serving process, the
  caller's credential holds the `vectors` scope, and (for `semantic_search`
  alone) the store serves an index profile with a provider. The Indexer role
  exposes no tool of its own, and a token never gains a scope after issue —
  refresh keeps the original grant's set, so a connector must re-authorize.
  The deployed-connector case is a new section in
  [`docs/runbooks/oauth-deployment.md`](docs/runbooks/oauth-deployment.md#8-the-connector-sees-fewer-tools-than-the-server-enables),
  and the README no longer calls the `indexer` role "idle in 1.0.1" — since
  1.0.2 the server embeds on write and in `semantic_search`.

## 1.1.0 — 2026-09-12

### Added

- **Soft taxonomy validation.** A write that names an unknown entity type or
  relation type always succeeds; the response carries `taxonomySuggestions`
  instead of a refusal. The offline tier (string-similarity over existing
  type names, relations and examples) always runs. The semantic tier is an
  additive extension: with `--enable-vectors`, the `indexer` Cargo feature
  and a serving `[indexer]` profile, suggestions also match embedded
  taxonomy subjects in vector space, and the response `taxonomySuggestions`
  object gains `similarTypes`, `exampleEntities` and `exampleRelations`
  alongside the always-present `{ name, score }` list.
- **`suggest_taxonomy` tool.** One explicit entry point for suggestion
  lookups, reading the same offline engine and the same serving snapshot.
  It is a new tool name, so the tool surface is additive.
- **Taxonomy subjects embed through the indexer worker.** Entity types,
  relation types and relation triples are queued on write and embedded by
  the same `indexer` poll that embeds entities; the runtime reconcile
  publishes per-kind ANN snapshots that suggestions read. The serving
  profile changes are traffic that the existing vector path already
  handles.

### Migration note

- Migration `0006` (taxonomy tables) applies automatically during startup
  on every database, the same way earlier migrations do. No manual step is
  required. New databases get the tables at first open; existing databases
  are upgraded in place, and the backfill mirrors each existing relation
  triple once.

### Compatibility

- The wire shape is additive: write responses gain an optional
  `taxonomySuggestions` field, and the new `suggest_taxonomy` tool is added
  without changing any existing tool's arguments or results. A client that
  ignores unknown response fields and unknown tool names is unaffected.

## 1.0.0 — 2026-09-10

### Breaking changes

- **An observation is an object, not a string.** Every read and every mutation
  result returns `{ body, createdAtUs, occurredAtUs, originEntityName }`.
  `create_entities`, `upsert_entities`, `add_observations` and
  `delete_observations` accept `{ body, occurredAtUs? }`. The boundary denies an
  unknown field, and it denies an explicit `"occurredAtUs": null`; a caller omits
  the field instead. A 5.2.1 client that sends strings fails with
  `expected struct ObservationInput`. Two ways forward: send objects, or start
  the server with `--legacy-observations`.
- **The crate, the binary and the maintenance command are renamed.**
  `cargo install mcp-memory` becomes `cargo install mcpmem`. The binary
  `mcp-memory` becomes `mcpmem`, and `mcp-memory-maintenance` becomes
  `mcpmem-maintenance`. A Claude Desktop configuration needs
  `"command": "mcpmem"`. The `bench` binary keeps its name. No environment
  variable changed, and the default database path stays `memory.mcpmem`.
- **A managed vector store refuses a direct vector write.**
  `vector_upsert_embedding`, `vector_batch_upsert` and `vector_delete_embedding`
  fail with `direct_vector_writes_disabled` after the store adopts an index
  profile. `vector_batch_upsert` reports the refusal for each item. A store that
  stays in legacy compatibility keeps the old behaviour.

### Added

- **One workspace of five crates.** `mcpmem` holds the server and the binaries.
  `mcpmem-core` holds the transactional SQLite graph, the schema bootstrap, the
  change log and the relation repair. `mcpmem-runtime` holds the role
  enumeration, the role parser and the supervisor. `mcpmem-indexer` holds the
  embedding worker. `mcpmem-webhook` holds the delivery worker. A user installs
  `mcpmem`; the four library crates publish so the set resolves.
- **Cargo features select the subsystems.** `default = ["code"]`. `code` compiles
  the tree-sitter grammars for ten languages and supplies the `code_*` tools.
  `indexer` and `webhooks` add the two worker roles. `bedrock` implies `indexer`
  and adds the AWS embedding client. A `--no-default-features` build is a lean
  memory server; CI proves that build carries no HTTP client and no AWS client.
- **Runtime roles.** The new flag `--role` takes a comma-separated list of `mcp`,
  `indexer` and `webhooks`. The default is `mcp` alone, so an existing invocation
  keeps its behaviour. One process can run the server, a worker, or any compiled
  combination. A role whose feature is absent fails at startup with
  `runtime role '<name>' was selected but its Cargo feature is not compiled`. A
  duplicate role, an empty list and an unknown name each fail with their own
  message. The process stops when the first role returns.
- **A durable change log and a durable outbox.** Migration
  `0001_change_events.sql` creates `entity_revision`, `change_event`,
  `event_outbox`, `index_profile`, `index_profile_registry`, `index_job`,
  `profile_vector`, `ann_generation` and `idempotency_record`. A mutation writes
  its change event and its queue rows inside the same transaction as the graph
  rows, so a crash loses no work. `change_event` is immutable, and a trigger
  protects it.
- **Ordered migrations with checksums.** The server records each applied
  migration in `schema_migration` with a version, a SHA-256 checksum and a
  timestamp. A changed checksum for an applied version stops startup, and a
  database newer than the binary stops startup. Every role runs the one shared
  bootstrap, `mcpmem_core::schema::initialize_database`.
- **Semantic indexing through a durable job queue.** Every effective entity
  change queues one `index_job` row per managed profile. A second change to the
  same entity replaces the queued row and raises the lease epoch, so an in-flight
  worker cannot commit a stale revision. A profile-less store writes the job in
  state `held`, where nothing claims it.
- **An indexer worker, started with `--role indexer`.** The worker claims one due
  job, takes a 30-second lease, calls the provider outside the write
  transaction, then commits the vector under the lease fence. A commit is refused
  when the lease expired, when the entity revision moved, or when the profile is
  no longer writable. A failure returns the job to `pending` with a one-second
  delay and records `last_error`; attempts are unlimited. The role polls every
  250 milliseconds.
- **Three embedding providers behind one trait.** Ollama uses
  `MCP_MEMORY_OLLAMA_URL` and posts to `<url>/api/embed`. The OpenAI-compatible
  provider uses `MCP_MEMORY_OPENAI_URL` with `MCP_MEMORY_OPENAI_API_KEY`; both
  keys must be set together. Bedrock uses the standard AWS credential chain and
  needs the `bedrock` feature. The registry dispatches strictly on the profile
  field `provider_kind`; an unknown kind fails the job instead of reaching
  another provider. A URL that carries a user name or a password is rejected.
- **A rebuild lifecycle for the vector index.** `index_profile_registry` holds
  one state per store key: `LegacyCompat`, `Active`, `Rebuilding` or `Failed`. A
  rebuild queues every live entity against the candidate profile, and activation
  promotes the candidate only after the full scan is verified. A profile is
  immutable and carries a fingerprint.
- **Durable webhook delivery.** Migration `0002_webhook_subscriptions.sql`
  creates `webhook_subscription`. A subscription holds an HTTPS `endpoint`, the
  event kinds (`create`, `update`, `delete`, `rename`), the entity types, a
  `consumer_origin`, the ignored origins, a `secret_ref` and an `enabled` flag.
  An empty kind list or an empty type list matches everything. A subscription
  never receives its own writes, which stops a loop between two instances.
- **At-least-once delivery with a lease fence.** A mutation enqueues one
  `event_outbox` row per matching enabled subscription. One delivery per
  subscription is in flight, in one process only, under a 30-second lease with a
  token and an epoch. The worker retries a 408, a 429 and a status of 500 or
  more, and it kills any other non-2xx status at once. The delay is the
  `Retry-After` header, clamped from one second to one hour, and one second
  otherwise. After eight attempts the row moves to `dead` and keeps
  `last_error`.
- **Signed, pinned webhook requests.** The worker signs
  `<timestamp_us>.<body>` with HMAC-SHA256 and sends
  `X-Memory-Signature`, `X-Memory-Timestamp` and `Idempotency-Key`. The body is
  a version 2 JSON envelope with no observation content, bounded to 64 KiB; a
  receiver reads the graph for the entity state. The endpoint must use HTTPS on
  port 443, with a hostname from an allowlist and no credentials. The worker
  resolves the hostname, refuses a private, loopback, link-local, multicast or
  unspecified address, pins the connection to the resolved address, and disables
  redirects.
- **Typed machine ingress provenance.** A machine mutation needs the
  `memory:write` scope and the requested origin in its allowlist. The mutation
  carries the actor, the origin, a correlation id, an optional causation id, a
  hop count and an idempotency key, and a subscriber receives those fields.
- **The MCP tool `rename_entity`.** It takes `oldName` and `newName` and returns
  the renamed entity. The entity keeps its identity, its observations and every
  incident relation. An existing destination name is rejected, and an unchanged
  name is a successful no-op. A rename emits one change event with the operation
  `rename` and the name pair, and it emits no synthetic create or delete. A
  rename leaves the counters and `updated_us` unchanged.
- **Observation metadata.** Migration `0003_observation_metadata.sql` adds
  `origin_entity_id`, `origin_entity_name` and `occurred_us` with a non-negative
  check. `created_us` stays server-owned and immutable. An existing row keeps its
  `created_us`, and the three new columns are null for it. `merge_entities`
  records the source entity of a copied observation, so a reader sees
  `originEntityName`.
- **The operator binary `mcpmem-maintenance`.** `relation-audit --database <path>
  --format json` is read-only. It reports duplicate relation groups, duplicate
  rows, dangling rows and counter drift. `relation-repair --database <path>
  --backup <new-path> --confirm` keeps the oldest row of each duplicate triple,
  recomputes every derived counter from surviving physical rows, and creates the
  unique index last. The repair needs `--confirm` and a backup target that does
  not exist; it copies the database with the SQLite online backup API, reopens
  the copy, and needs `PRAGMA integrity_check = 'ok'`. A dangling relation row
  aborts the run before any write to the source. Server startup never runs a
  repair.
- **Relation uniqueness on a new database.** A database that 1.0.0 creates gets
  `UNIQUE INDEX relation_unique_triple ON relation(from_id, to_id, type_id)`. An
  existing database stays openable and gains the index only through
  `relation-repair`.
- **The deprecated flag `--legacy-observations`.** It restores the historical
  `observations: string[]` contract for an MCP client, in the manifest and in
  both directions. It changes no stored data. It needs the `mcp` role, and
  version 2.0.0 removes it.
- **A published crates.io release.** All five crates carry one version. The
  release publishes only for a published GitHub release; a push to `main`
  publishes nothing. `scripts/check-release-version.sh` refuses build metadata,
  a version mismatch between the crates, a path dependency that carries no
  version, a tag that does not match, and a version crates.io already holds.
  See [`docs/runbooks/release.md`](docs/runbooks/release.md).

### Changed

- **Every graph mutation is one transaction.** All mutations route through one
  transactional service. It issues `BEGIN IMMEDIATE`, applies the graph rows, the
  derived counters, the change events and the index jobs, and commits once; a
  guard rolls back on any later failure. A partial write can no longer happen.
  The in-memory metadata cache no longer publishes uncommitted state after a
  failed mutation.
- **`describe_entity` returns the bundle its description promised.** The result
  adds `relations` (every incident relation, ordered by `from`, `to`,
  `relationType`), `neighbors` (distinct names, sorted) and `degree` with the
  members `in` and `out`. One reader snapshot serves the whole response, and the
  degrees are counted from the returned relations, not from the denormalized
  cache. There is no total degree field.
- **The `upsert_entities` description states what the server does.** The old text
  claimed an existing entity keeps its type. The manifest now states the truth: a
  call on an existing exact name sets that entity's type to the supplied
  `entityType` and adds only the observations it does not hold, and it keeps the
  entity identity, the existing observations and every incident relation. The
  behaviour did not change.
- **A historical durable record stays readable.** A pre-1.0 `change_event`
  payload or `idempotency_record` response may hold a bare observation string.
  The reader accepts both shapes and reports the missing metadata as null. It
  fabricates no timestamp.
- **The database bootstrap creates the graph tables before it applies the
  migrations.** Before this change a worker process that reached a new database
  first created the event tables with no graph table and no seeded statistics.
  One shared initializer now commits the graph DDL, the full-text projections,
  the triggers and the statistics rows, and only then runs the migrations.
  Startup stays non-destructive and re-runs safely.

### Fixed

Each item is a defect inherited from 5.2.1. The triage is in
[`docs/analysis/2026-09-08-inherited-legacy-bugs-triage.md`](docs/analysis/2026-09-08-inherited-legacy-bugs-triage.md).

- A batch `delete_relations` built one broken SQL statement. The typed request
  now deletes each relation correctly.
- `describe_entity` advertised a graph context bundle and returned only the name,
  the type and the observations. A hub with 40 relations looked like an isolated
  node.
- A failed `merge_entities` left the graph half-merged. The copied observations
  and the redirected relations stayed committed when the source delete failed.
- `merge_entities` could create a duplicate relation. The write path now refuses
  one, and `relation-repair` removes an existing duplicate.
- A merge erased the origin of a copied observation. The reader now reports
  `originEntityName`.
- Observation time was invisible. A client could not read when a fact was
  recorded, and could not state when it happened.
- The retype contract in the tool manifest was false.
- A rename was absent from the tool set.
- Destructive tool annotations were inconsistent. One central registry now owns
  them.
- A renamed entity stayed reachable under its old name in the vector and code
  search paths, because those paths resolved a cached name map. Every operational
  path now resolves the current name from SQLite.

### Known limitations

- **The shipped binary delivers no webhook.** `mcpmem --role webhooks` fails with
  `webhook role selected without configured worker ports`. The failure is
  deliberate and observable. Delivery works for a program that embeds
  `mcpmem-webhook` and constructs the worker with a connector, a secret provider,
  a hostname allowlist and a resolver.
- **No MCP tool manages a webhook subscription.** A user writes the
  `webhook_subscription` table, or calls the Rust repository API.
- **A legacy database keeps its duplicate relations and its counter drift until
  an operator repairs it.** Startup deletes no row and adds no unique index to a
  table that existed before bootstrap.
- **There is no `retype_entity` tool.** A retype is possible only through
  `upsert_entities`.
- **Bedrock supports Amazon Titan Text Embeddings V2 only**, with 256, 512 or
  1024 dimensions.
- **`--legacy-observations` is temporary.** It stays through 1.x, and 2.0.0
  removes it.

### For a contributor

CI runs actionlint, the version gate, the crate-include gate,
`cargo package -p mcpmem-core`, `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`, the
workspace test suite, the indexer feature matrix, the runtime role matrix, and a
check that the graph-only build carries no HTTP client and no AWS client.

The gates are scripts, so the same check runs on a laptop and on a runner:
`check-release-version.sh`, `check-crate-includes.sh`, `set-version.sh`,
`next-version.sh`, `test-next-version.sh` and `publish-crates.sh`.

The design documents and the operator runbooks are under
[`docs/`](docs/).
