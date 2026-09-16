# mcpmem-webhook

The durable webhook delivery worker of
[mcpmem](https://github.com/abankowski/mcpmem), an MCP server that gives LLM
agents persistent memory.

This crate is a library. It delivers graph change events to an HTTPS endpoint,
at least once, from a SQLite outbox.

```sh
cargo install mcpmem --features webhooks
```

**Read this before you plan a deployment.** The shipped `mcpmem` binary keeps
this crate's distribution honest: it embeds the large bodies of the delivery
engine and the poll loop, and it enforces the contract of this README. It
ships `webhook_add_subscription` and `webhook_delete_subscription` as MCP
tools, and it reads the delivery policy from the `[webhooks]` section of its
TOML configuration file, not from environment variables.

## Subscriptions

One row of `webhook_subscription` is one subscription:

| Column | Meaning |
|---|---|
| `endpoint` | The HTTPS URL, 2048 characters maximum |
| `event_operations` | A JSON array of `create`, `update`, `delete`, `rename`. An empty array matches every kind |
| `entity_types` | A JSON array of entity types. An empty array matches every type |
| `consumer_origin` | The origin name of this consumer, 256 characters maximum |
| `ignored_origins` | A JSON array of origins to drop |
| `secret_ref` | The lookup name of the signing key, 512 characters maximum |
| `enabled` | 0 or 1 |

A subscription never receives its own writes: the filter drops an event whose
origin equals `consumer_origin`, or is in `ignored_origins`. This stops a
delivery loop between two instances.

## Delivery

A mutation writes one `event_outbox` row per matching enabled subscription, in
the same transaction as the graph write, so a crash loses no delivery.

One delivery per subscription is in flight, in one process only. `claim_due`
takes a 30-second lease with a token and an epoch, and a write-back succeeds
only while the token, the epoch and the deadline still match. A slow worker
whose lease expired cannot overwrite a newer attempt.

The worker retries a 408, a 429, and a status of 500 or more. Any other non-2xx
status, a policy error and a secret error kill the delivery at once, because a
repeat cannot succeed. The delay is the `Retry-After` header, clamped from one
second to one hour, and one second otherwise. After eight attempts the row
moves to `dead` and keeps `last_error`. A dead row is never claimed again, and
the server raises no alert.

## The request

| Header | Value |
|---|---|
| `X-Memory-Signature` | The lowercase hexadecimal HMAC-SHA256 of `<timestamp_us>.<body>` |
| `X-Memory-Timestamp` | The signed timestamp, in microseconds |
| `Idempotency-Key` | The event id. Duplicate suppression is the receiver's job |

A receiver must recompute the MAC over both parts to reject a replayed body.

Every delivery attempt has a 10-second deadline. A dead or stalled
connection fails the attempt and retries like any other transport failure.
The worker logs the full cause — TCP, TLS, timeout — not only reqwest's
`error sending request for url (...)` headline, so the log line names the
reason a delivery failed.

The body is a version 3 JSON envelope, bounded to a configurable cap (1 MiB by
default). It carries `eventId`, `transactionId`, `entityId`, `entityRevision`,
`operation`, `occurredAtUs`, `origin`, `correlationId`, `causationId`,
`hopCount`, `oldName`/`newName` (non-null only for a rename), and `kind`
(`entity` or `relation`). The `kind` block carries the full object:

- `entity.before` and `entity.after` are complete snapshots — name, entity
  type, observations and kv attributes. `before` is null for a create;
  `after` is null for a delete, and `before` is the last-known state of the
  deleted object. `relationDelta` entries are full relation objects, not
  bare triples.
- `relation` appears only on the events emitted for relation observation and
  attribute writes. It carries the triple, the mirror's own
  `relationRevision`, and the exact `before`/`after` pair, so a receiver can
  see which observation or attribute was added, changed or removed.

An event is self-contained: apply `entity.after` (or `relation.after`) to
converge, or diff `before` against `after` for the exact change. Events
stored before this release deliver with null attributes and no relation
detail; treat null attributes on historical events as unknown. An event
whose envelope exceeds the cap dead-letters with the policy reason — never a
partial delivery. Raise the cap with `max-body-bytes` in the `[webhooks]`
configuration section.

## The endpoint policy

The URL must use `https` on port 443, with a hostname from the allowlist, no
user name, no password and no fragment. The worker resolves the hostname,
refuses a private, loopback, link-local, multicast or unspecified address, pins
the connection to the resolved address, and disables redirects.

## Secrets

`secret_ref` is a lookup name, not key material. The embedder supplies a
`SecretProvider`; the crate ships `StaticSecretProvider`, a map from name to
key. An unknown name dead-letters the delivery with
`secret reference is not configured`.

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
