# Webhook envelope v3 — full snapshots and exact change data

Status: proposal, 2026-09-16. Not yet implemented.

## The problem

The envelope v2 carries `eventId`, `transactionId`, `entityId`,
`entityRevision`, `operation`, `occurredAtUs`, `origin`, `correlationId`,
`causationId`, `hopCount`, and `oldName`/`newName` for a rename
(`crates/mcpmem-webhook/src/lib.rs:558`). It carries no entity content.
The README states: "It carries no observation content: the receiver reads the
graph for the entity state."

Three consequences make the notification useless for a mirroring consumer:

1. A delete event names only `entityId`. The receiver cannot learn what
   disappeared, because the entity is gone from the graph.
2. An update event names only the identity. The receiver cannot learn which
   observations changed or were removed without re-reading the graph.
3. A kv (`attributes`) change emits no event at all. `SetAttributes` and
   `DeleteAttributes` return an empty `affected_names` set
   (`crates/mcpmem-core/src/mutation.rs:437`), so `persist_changes` never
   creates a `change_event`. REQ-ATTR-OFFLINE excluded them deliberately.
   The same holds for `AddRelationObservations` and
   `DeleteRelationObservations`.

## Verified facts

- The durable `EntityChange` already stores `before`, `after`,
  `relation_delta`, and `old_name`/`new_name`
  (`crates/mcpmem-core/src/mutation.rs:159`). The envelope v2 never
  serializes it.
- `EntitySnapshot` carries only `entity_id`, `name`, `entity_type`,
  `observations` (`crates/mcpmem-core/src/mutation.rs:125`). It has no
  attributes. REQ-ATTR-READ (`crates/mcpmem-core/src/graph.rs:70`) states
  the write snapshot stays free of them.
- `RelationDelta` carries bare triples (`from`, `to`, `relationType`). The
  full relation object (observations, attributes) exists only in the
  read-model `RelationDetail` (`crates/mcpmem-core/src/types.rs:312`) and
  in the relation mirror rows (`taxonomy_relation`).
- The 2026-09-14 decision
  (`docs/analysis/2026-09-14-relation-observations-and-attributes.md`)
  pins both rules. This proposal supersedes that decision, in the same way
  that document supersedes its predecessor: mark the old rules superseded
  inline with the reason, do not delete them.
- The envelope is bounded to 64 KiB (`MAX_BODY`,
  `crates/mcpmem-webhook/src/lib.rs:16`). A policy error kills the delivery
  at once. Full before/after snapshots exceed this cap for any entity with
  many observations, so the cap must rise with the payload.
- `change_event` rows are stored as JSON. New optional fields on
  `EntitySnapshot` deserialize old rows when declared with
  `#[serde(default)]`. The wire version constant makes every delivery v3,
  including deliveries of pre-v3 stored events; historical rows deliver
  with null attributes.

## Recommendation: send both, always full

Send `before` and `after` in one event. Each event becomes the durable
change record, already captured at mutation time. The pair answers every
case:

- Delete: `after` is null and `before` is the complete last-known object.
  This is the only possible answer to "what is gone". It does not
  contradict delete semantics; it defines them.
- Create: `before` is null and `after` is the complete object.
- Update: apply `after` to converge, or compare `before` with `after` for
  the exact added and removed observations and kv keys.

Why not one side only:

- `after` only cannot express a delete. It is the current shape and the
  reported pain.
- `before` only forces each receiver to diff against its own mirror. A
  receiver that joined late or missed a delivery holds a stale mirror and
  computes a wrong diff. The server holds the ground truth at event time;
  only the server-side pair is exact.

Do not add a separate diff field. The pair is the diff. A redundant
computed diff doubles the payload and can drift from the snapshots. One
source of truth per fact: `before` and `after`.

## The envelope v3 shape

```json
{
  "version": 3,
  "eventId": "…",
  "transactionId": "…",
  "entityId": 5,
  "entityRevision": 7,
  "operation": "delete",
  "occurredAtUs": 1720000000000000,
  "origin": "producer",
  "correlationId": "…",
  "causationId": null,
  "hopCount": 0,
  "kind": "entity",
  "entity": {
    "before": {
      "entityId": 5,
      "name": "Ada",
      "entityType": "Osoba",
      "observations": [
        { "body": "…", "createdAtUs": 1, "occurredAtUs": 1, "originEntityName": null }
      ],
      "attributes": { "email": "ada@…" }
    },
    "after": null,
    "relationDelta": {
      "added": [],
      "removed": [
        { "from": "Ada", "to": "Bob", "relationType": "pracuje",
          "observations": [], "attributes": {} }
      ]
    }
  }
}
```

Rules:

- `kind` is `"entity"` or `"relation"`. Exactly one kind block is present.
- `entity.before` and `entity.after` are `EntitySnapshot` objects or null.
  Null means "did not exist in that state": delete has `after: null`,
  create has `before: null`.
- `entity.relationDelta` entries become full `RelationDetail` objects
  (triple, observations, attributes), not bare triples.
- `oldName` and `newName` stay on the envelope top level for rename events
  (v2 compatibility, test-pinned).
- `kind: "relation"` events cover relation observation and attribute
  changes only. Relation create and delete continue to ride the endpoint
  entity events through `relationDelta`, as they do today.

Relation event example:

```json
{
  "version": 3,
  "eventId": "…",
  "transactionId": "…",
  "entityId": 3,
  "entityRevision": 4,
  "operation": "update",
  "occurredAtUs": 1720000000000000,
  "origin": "producer",
  "correlationId": "…",
  "causationId": null,
  "hopCount": 0,
  "kind": "relation",
  "relation": {
    "from": "Ada",
    "to": "Bob",
    "relationType": "pracuje",
    "before": { "observations": [ "…" ], "attributes": { "since": "2020" } },
    "after":  { "observations": [ "…", "…" ], "attributes": {} },
    "relationRevision": 3
  }
}
```

## What must change in the core

### 1. Attributes enter the event snapshot

Add `attributes: Option<BTreeMap<String, String>>` to `EntitySnapshot`
with `#[serde(default, skip_serializing_if = "Option::is_none")]`, the same
pattern as `EntityChange.old_name`. Capture attributes in `capture()` with
`attributes_for` at mutation time, inside the same transaction. This
supersedes the REQ-ATTR-READ clause "the write snapshot stays free of
them". The read path keeps its behavior; the snapshot capture gains one
indexed query per affected entity. Old stored events deserialize with
`attributes: null`.

### 2. Attribute writes emit events

`SetAttributes` and `DeleteAttributes` resolve their affected entity names
into `affected_names`. The entity diff now differs in attributes, so
`effective_changes` produces `EntityChange::Update`. `DeleteAttributes` on
an entity deletes keys: the keys are in `before`, absent in `after`, and
the pair shows exactly what was deleted.

Two harms motivated REQ-ATTR-OFFLINE: the entity_revision bump and the
index job re-enqueue on every attribute write. The webhook event is
wanted; the two harms are not. Attribute-only changes therefore persist an
event on a path that skips the revision bump and skips `enqueue_change`.
The event carries the current structural revision. Document that
`entityRevision` counts structural changes; consecutive attribute events
may share one revision.

The same applies to `AddRelationObservations` and
`DeleteRelationObservations`: they emit one relation-kind event attached to
the `from` endpoint, with the mirror's own revision in `relationRevision`.
The mirror revision and index enqueue already exist
(`bump_relation_revision_enqueue`); only the webhook event is missing.

### 3. Relation details in the delta

At mutation time, `capture()` reads the observations and attributes of each
changed relation from the mirror, so `relation_delta` entries are full
objects. On relation delete, capture the mirror state before
`tombstone_relation_mirror` runs.

### 4. Payload cap

Make `MAX_BODY` configurable (`WebhookConfigFile.max_body_bytes`), default
1 MiB. Keep the hard policy error at the configured cap: a partially
delivered envelope would break the signature contract. Document that an
entity or relation larger than the cap dead-letters with the reason, and
the operator raises the cap in config.

## Consumer contract

- Apply `entity.after` (or `relation.after`) to converge. The event is
  self-contained: a consumer that joins late can apply the pair without a
  graph read.
- Diff `before` against `after` for exact observation and attribute
  changes, including deletions.
- Delete: `after` null. The full `before` is the last-known state.
- Historical events (stored before v3) deliver with attributes null and
  without `relationDelta` detail. Treat null attributes as "unknown".
- Deduplicate by `eventId`, as before. Signing and headers do not change.

## Superseded decisions

- 2026-09-14 REQ-ATTR-READ: snapshots stay free of attributes. Superseded
  for the event path so receivers learn kv changes; read models unchanged.
- 2026-09-14 plan bullet: the four new `MutationRequest` variants emit no
  events. Superseded: they now emit events, still without the revision bump
  and index churn the rule existed to prevent. REQ-ATTR-OFFLINE as
  specified in `docs/superpowers/specs/2026-09-14-relation-observations-and-attributes-design.md`
  (no index job, no chunk, no revision bump) is retained and satisfied by
  the quiet path.
- 2026-09-07 contracts addendum: "Observation bodies ... are excluded"
  and the envelope "never carries an entity snapshot or observation
  bodies". Superseded by the v3 body. Secret references stay excluded;
  the endpoint allowlist and the signature already govern who receives
  observation content.

## Tests and docs to update

- `tests/webhook_outbox.rs:399-404` asserts no `before`, `after`, or
  `observations` field. Flip to assert the full snapshots, attributes, and
  the delete case. Keep the assertion that the body carries no secret
  reference.
- Envelope version assertions become 3.
- `crates/mcpmem-webhook/README.md` "The request" section.
- `docs/analysis/2026-09-14-relation-observations-and-attributes.md`,
  `2026-09-07-unified-memory-runtime-contracts.md`,
  `2026-09-08-legacy-graph-maintenance.md` mark the superseded clauses
  inline.
- The workspace README webhook section and `CHANGES.md` entry.

## Sequence

1. Core: extend `EntitySnapshot`, capture attributes, revise
   `affected_names`, add the quiet event path for attribute/relation-obs
   writes, capture relation details.
2. Webhook crate: envelope v3, `kind` union, configurable cap.
3. Update tests in the same changes, watch each new guard fail once.
4. Doc updates in the same changes, including the superseded markers.
5. Run the repository pre-flight, open the PR.