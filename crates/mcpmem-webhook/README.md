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

The body is a version 2 JSON envelope, bounded to 64 KiB. It carries
`eventId`, `transactionId`, `entityId`, `entityRevision`, `operation`,
`occurredAtUs`, `origin`, `correlationId`, `causationId`, `hopCount`, and, for a
rename, `oldName` and `newName`. It carries no observation content: the
receiver reads the graph for the entity state.

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
