# Unified Memory Runtime — Contract Addendum

**Status:** normative for `2026-09-07-unified-memory-runtime.md`. It resolves
the public and durable seams left open by the viability analysis. The analysis
remains the architecture authority where this addendum is silent.

## 1. Compatibility and migration ownership

`memory-core` owns one ordered migration runner for the existing memory SQLite
file. It runs before any graph, vector, indexer, or webhook role opens a
connection. It records `version`, SHA-256 `checksum`, and `applied_at_us` in
`schema_migration`; a version may run once only, and a checksum mismatch aborts
startup. Each migration is one `BEGIN IMMEDIATE` transaction. There are no
automatic down-migrations; the operator restores a tested database backup.

`mcp-memory` must retain the existing graph schema and all graph/MCP tool
responses. The existing one-row-per-entity `vector_embedding` table remains
the legacy compatibility store. Managed profiles use a new `profile_vector`
table, keyed by `(profile_id, entity_id)`, so a rebuilding candidate can coexist
with the serving legacy or active profile. Existing rows are never silently
labelled as a managed provider profile; they are exposed as `LegacyCompat`.
`profile_vector` is `(profile_id UUID, entity_id INTEGER, entity_revision
INTEGER, blob BLOB, created_at_us INTEGER, source TEXT, PRIMARY KEY
(profile_id, entity_id))`; new blobs use `f32le-v1` and managed rows may not
use the legacy native-memory encoding.

```text
source = "legacy_vector_embedding"
dimensions = vector_embedding.dims
model = row.model (or "")
encoding = "native-f32-legacy"
```

Before migration, `mcp-memory migrate --check` is read-only and reports row
counts grouped by dimensions and model, plus the configured dimensions. It
fails if a row's stored dimensions differ from its blob header. `migrate
--apply` requires a backup path and refuses to proceed unless the preflight is
clean. The compatibility fixture is a copied pre-fork `memory.mcpmem` with
graph and vector rows; the check is `cargo test --test migration_compatibility`.

## 2. Vector profile contract

There is exactly one `Active` profile per `store_key`; a profile is the complete
vector-space contract, not a provider label:

```rust
struct IndexProfile {
    id: Uuid,
    store_key: String,                 // V1: "default"
    provider_kind: String,             // "openai-compatible", "ollama", "bedrock", "external"
    model: String,
    dimensions: u32,
    representation_version: String,
    normalization: Normalization,      // None | L2
    distance_metric: DistanceMetric,   // Cosine | InnerProduct | L2Squared
    vector_encoding_version: String,  // V1: "f32le-v1"
}
```

`profile_fingerprint` is lowercase SHA-256 over the canonical JSON form of all
fields except `id`. `index_profile` stores that fingerprint uniquely with state
`Rebuilding | Active | Retired`; the store registry is exactly `LegacyCompat`,
`Active(profile)`, `Rebuilding(serving, candidate)`, or `Failed(candidate,
reason)`. A profile becomes `Active` only after a verified complete Full scan,
no stale entity revisions, durable candidate rows, and a replacement ANN reader
snapshot all carry its ID and generation. A failed rebuild retains the serving
reader. Vectors never mix profiles in one ANN index.

The legacy direct `vector_*` tools stay unchanged in `LegacyCompat`. Entering
managed rebuilding explicitly disables direct mutation tools with the stable
`direct_vector_writes_disabled` error; read/search tools remain available
against the serving generation. A protected machine vector ingress may write
only with `vector:write:default`, a declared profile ID and entity revision; it
rechecks the active profile, revision, dimensions, finite values and declared
normalization in the commit transaction. This prevents a caller from silently
mixing externally supplied vectors with worker-owned vectors. An explicit
profile rebuild is the only transition from legacy to managed storage.

## 3. Authentication and machine write ingress

MCP retains its current optional bearer-token behaviour. Automation writes and
admin operations use `/api/v1` HTTP endpoints and an authentication port:

```rust
trait CredentialAuthenticator {
    fn authenticate_bearer(&self, credential: SecretString)
        -> Result<Principal, AuthenticationError>;
}
struct Principal {
    id: PrincipalId,
    kind: PrincipalKind, // Machine | Admin
    scopes: BTreeSet<Scope>,
    allowed_origins: BTreeSet<Origin>,
    secret_scopes: BTreeSet<SecretScope>,
}
trait SecretProvider {
    fn signing_key(&self, reference: &SecretRef, purpose: SecretPurpose)
        -> Result<SigningKey, SecretError>;
}
```

V1 ships only a token-file adapter: one opaque token per line in a root-owned
file, mapped to a principal ID and scopes. Tokens are never stored in SQLite,
logs, change events, or subscription responses. The port permits later mTLS or
OIDC adapters without changing HTTP payloads. The legacy `--auth-token` maps
only to `legacy_mcp`; it grants no `/api/v1` scope. Scopes are `memory:write`,
`webhooks:read`, `webhooks:write`, `deliveries:read`, `deliveries:retry`,
`jobs:read`, and `secrets:use:<scope>`.

`POST /api/v1/mutations` requires `memory:write`, a machine principal,
`Authorization: Bearer <token>`, `Idempotency-Key`, `X-Memory-Origin`,
`X-Memory-Correlation-Id`, `X-Memory-Hop`, and this JSON body (maximum 1 MiB):

```json
{
  "schemaVersion": 1,
  "mutation": { "operation": "...", "payload": {} }
}
```

The server derives `actor` solely from the authenticated principal, never from
the body. `X-Memory-Origin` must exactly match the principal's configured
origin. Without `X-Memory-Causation-Id`, hop must be zero; with it, causation
must name a locally known event, correlation must match its parent, and hop must
equal parent hop plus one, to a maximum of 15. A subscription's mandatory
`consumerOrigin` is always ignored by that subscription. Idempotency stores
SHA-256 over method, normalised path and raw body under
`(principal_id, Idempotency-Key)`; matching retries return the original `200`
response with `Idempotency-Replayed: true`, mismatches return `409`.

## 4. Subscription and webhook wire contract

Subscription registration is REST-only and native-TLS or loopback-only behind
an operator TLS terminator. These routes never accept query-token credentials
or trusted forwarded headers unless a trusted proxy is configured:

```text
POST|GET       /api/v1/admin/webhook-subscriptions
GET|PUT|DELETE /api/v1/admin/webhook-subscriptions/{id}
GET            /api/v1/admin/webhook-deliveries?state=&subscriptionId=&cursor=
POST           /api/v1/admin/webhook-deliveries/{deliveryId}/retry
GET            /api/v1/admin/jobs
GET            /api/v1/admin/runtime/status
```

Write routes require `webhooks:write`; read routes require their corresponding
`*:read` scope. A subscription holds `endpoint`, event/entity
filters, ignored origins, `secret_ref`, and enabled state. It returns a secret
reference but never secret material. `secret_ref` resolves through a `SecretProvider`
port at delivery time; V1 supplies an operator-managed file implementation.

The request body is UTF-8 JSON, maximum 64 KiB, with stable envelope version:

```json
{
  "schemaVersion": 1,
  "eventId": "UUID",
  "transactionId": "UUID",
  "occurredAt": "RFC 3339 UTC",
  "operation": "create|update|delete",
  "entity": { "name": "...", "entityType": "...", "revision": 42 },
  "change": { "changedFields": ["..."], "relationDelta": { "added": [], "removed": [] } },
  "provenance": { "actorId": "...", "origin": "...", "correlationId": "UUID", "causationId": "UUID or null", "hopCount": 0 }
}
```

Observation bodies, secret references, credentials, and arbitrary before/after
snapshots are excluded. Delivery sends `Content-Type: application/json`,
`Idempotency-Key: <eventId>`, `X-Memory-Webhook-Version: 1`,
`X-Memory-Event-Id`, `X-Memory-Delivery-Id`, `X-Memory-Subscription-Id`,
`X-Memory-Timestamp` (Unix seconds), `X-Memory-Key-Id`, and
`X-Memory-Signature: v1=<lowercase HMAC-SHA-256 hex>`. The signature is
HMAC-SHA-256 over UTF-8 with literal LF separators:

```text
mcp-memory-webhook-v1\n<timestamp>\n<event_id>\n<delivery_id>\n<subscription_id>\n<key_id>\n<lowercase hex SHA-256(raw_body)>
```

Consumers accept timestamps within five minutes and deduplicate `eventId` for
at least 30 days. A 2xx status acknowledges delivery. Network errors, 408, 429,
425, 429, and 500/502/503/504 retry; every other response dead-letters
immediately. The default is twelve attempts with full-jitter exponential backoff
capped at one hour, a ten-second total attempt deadline, no redirects, and one
in-flight delivery per subscription. `Retry-After` for 429/503 is honoured,
clamped to one second through one hour.

Endpoints must be HTTPS, must use port 443, and must not contain userinfo or a
fragment. Production configuration requires an exact hostname allowlist.
Registration and every delivery resolve every address and reject loopback,
link-local, private, multicast, and unspecified ranges. The connector dials the
validated address while preserving the original Host and TLS SNI; it never
resolves the hostname again. A test-only injected connector covers local fake
receivers without weakening production policy.

## 5. Performance gate

`tests/resource_isolation.rs` owns the reproducible gate. Its fixed fixture is
10,000 entities, 20,000 relations, and 384-dimensional vectors; its workload is
80% graph/FTS reads, 20% vector search, 32 concurrent MCP requests, for 30
seconds after a 10-second warm-up. It records a no-worker baseline `B` and then
repeats while (separately) an embedding call blocks, a webhook receiver blocks,
and an ANN replacement index builds.

For each stalled run, zero requests may fail and the CI non-regression gate is:

```text
p95 <= (2 * B.p95) + 5 ms
p99 <= (2 * B.p99) + 10 ms
```

The CI job stores the baseline and stalled-run JSON as artifacts. The command
that enforces the gate is `cargo test --test resource_isolation -- --ignored
--nocapture`; it is enabled only after that test exists. This relative contract
is portable across supported hardware. A production absolute SLO is deliberately
not invented here: before enabling worker roles on a target VM, its owner records
the measured p50/p95/p99 from this fixture in deployment configuration and uses
that value as the release limit.

## Rejected choices

- A shared bearer token for automation and administration: it cannot attribute
  mutations or apply least privilege.
- Webhook payloads shaped for n8n or Node-RED: they would make those products a
  compatibility boundary instead of ordinary HTTP consumers.
- Signing re-serialized JSON: different serializers make the signature
  ambiguous; the raw-body hash is deterministic.
- Multiple active vector profiles in V1: old and candidate rows coexist only
  during a rebuild; queries always use exactly one serving profile.
- An absolute latency threshold before measuring target hardware: it would be a
  guess, not a gate.
