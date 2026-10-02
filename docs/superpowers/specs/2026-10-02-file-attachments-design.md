# File Attachments with Embeddings — Design

**Status:** Design approved in sections on 2026-10-02. Written specification awaits review.

## Goal and scope

A user can attach a file to an entity (a graph node). Text files are read
directly; PDFs are converted to text by an LLM-driven OCR. The extracted text
is chunked and embedded exactly like entity observations, so semantic, hybrid
and MMR search surface file content — each hit identifying the file, the page
and an excerpt. A per-workspace durable worker performs the extraction asynchronously;
an upload returns immediately with an `attachment_id` and a status.

Supported source types: UTF-8 text files and `application/pdf`. Code files are
out of scope: the tree-sitter `code` feature already owns code symbols, and mixing
file attachments into that path is a separate design.

This change does not add attachment versioning, per-attachment permission, or
cross-workspace file search. Extraction output is stored permanently so a
profile rebuild re-embeds without re-running OCR.

## Evidence and the pipeline this plugs into

- `crates/mcpmem-core/src/jobs.rs:39` queues one `chunk_index_job` row per owner
  per serving profile, inside the write transaction; a second change replaces
  the row and raises the lease epoch.
- `crates/mcpmem-indexer/src/lib.rs:301-308` dispatches on `job.owner_kind`
  (only `Entity` and `Relation` today) and builds the canonical document, then
  `embed_chunks_and_commit` calls the provider outside the
  transaction and `commit_chunks` fences the write on lease, revision and
  profile writability.
- `crates/mcpmem-core/src/jobs.rs:83` `ChunkKind` has three variants
  (`Identity`, `Observation`, `Relation`); `jobs.rs:100` `OwnerKind` has two.
  Both are single definitions imported by the worker and the vector store, so
  one addition sites both.
- `src/vector_store.rs:1130` `chunk_text(hit)` resolves a chunk back to text;
  `:1091` `resolve_owner` renders an owner name. `src/vector_store.rs:521`
  inserts chunk rows into `chunk_vector`, which already carries a `source`
  column.

**Root cause to reuse:** the file-attachment pipeline is OCR-and-chunking, and
the embedding half is already a fenced, durable, provider-neutral worker.
Adding attachments must not add a second embedding path. The design therefore
splits the work: a new extraction worker produces permanent text, and the
existing indexer worker embeds that text through a new `OwnerKind::Attachment`
arm. All embedding reuse (lease, revision fence, profile adopt/rebuild,
dead-letter) then works for files with no new embedding code.

## Decisions taken in brainstorming (2026-10-02)

| Question | Decision |
|---|---|
| Pipeline timing | Asynchronous durable worker; upload returns immediately with status |
| Where file bytes live | SQLite, alongside the graph — each workspace stays one portable file |
| Ship surface | MCP tools + embedded `/ui` admin (upload, download, live status) |
| OCR provider | Separate `[ocr]` config; `model` independently selected; credentials reuse the primary OpenAI embedding provider by default |
| Code files | Out of scope (tree-sitter owns code symbols) |
| Size limits | Server config, not hard-coded |

## Architecture and single owners of facts

| Fact | Owner | Consumers |
|---|---|---|
| Attachment bytes, metadata, status, revision | Attachment tables in the workspace graph file | MCP tools, admin UI, extraction worker, indexer, cascade delete |
| Extracted page text | `attachment_text` in the workspace file | Indexer chunks; rebuilds re-embed from it, never re-OCR |
| Attachment chunk vectors | The existing `chunk_vector` via the indexer worker | Vector search, `chunk_text`, entity view |
| OCR credentials and model | `[ocr]` config (the runtime config file) | Extraction worker |
| Workspace access to attachments | PR #55 workspace registry authorization (attachments inherit entity/workspace access) | MCP tools, admin UI |

Every graph fact stays in the workspace's SQLite file; no attachment data is
stored in the registry or the legacy file.

## Data model

New tables in the workspace graph schema (new migration, next number after the
workspaces migration for that schema):

```sql
CREATE TABLE attachment (
  id INTEGER PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  entity_id INTEGER NOT NULL,
  filename TEXT NOT NULL,
  mime TEXT NOT NULL,
  size_bytes INTEGER NOT NULL,
  sha256 BLOB NOT NULL,
  content BLOB NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('uploaded','extracting','ready','error')),
  revision INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  created_us INTEGER NOT NULL
) STRICT;

CREATE TABLE attachment_text (
  attachment_id INTEGER NOT NULL REFERENCES attachment(id),
  page INTEGER NOT NULL,
  text TEXT NOT NULL,
  chars INTEGER NOT NULL,
  PRIMARY KEY (attachment_id, page)
) STRICT;

CREATE TABLE attachment_job (
  attachment_id INTEGER PRIMARY KEY REFERENCES attachment(id),
  state TEXT NOT NULL CHECK (state IN ('pending','leased','done','dead')),
  lease_token TEXT,
  lease_epoch INTEGER NOT NULL DEFAULT 0,
  lease_until_us INTEGER,
  next_attempt_us INTEGER NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  last_error TEXT
) STRICT;
```

`attachment_job` mirrors the `chunk_index_job` lease/attempt machinery so the
extraction worker needs no new failure primitives: bounded attempts, dead-letter
on error, no work lost on crash (row written in the same transaction as the
blob).

Indexes: `attachment(entity_id)`, `attachment(workspace_id)`, and a unique
`(workspace_id, filename)` for a per-entity name collision policy.

## Pipeline

1. **`attach_file`** validates MIME against the allowlist and size against the
   per-attachment and per-workspace budgets, writes `content`+metadata and the
   `attachment_job` row `pending` in one transaction, returns
   `(attachment_id, status="uploaded")`.
2. **Extraction worker** (new role, gated like `indexer`/`webhooks`):
   - text files: decode UTF-8 (BOM stripped, CRLF normalised), one synthetic
     page.
   - PDFs: render per page, call the OCR provider per page, store one
     `attachment_text` row per page. On success set
     `attachment.status='ready'`, bump `attachment.revision`, enqueue
     `chunk_index_job` rows (`owner_kind='attachment'`) for every serving and
     rebuilding profile, mark the job `done`. On failure set
     `status='error'`, `last_error`, and dead-letter after the attempt bound.
3. **Existing indexer worker** gains one `OwnerKind::Attachment` arm at
   `lib.rs:301-308`: assemble from `attachment_text` one chunk per page (split
   pages over the profile's token cap), then the existing
   claim→embed→commit/fence path runs unchanged. `ChunkKind::Attachment` is a
   new variant on `jobs.rs:83`.

Chunking: PDFs use per-page chunks so a search hit carries a page number;
splitting preserves page attribution. Text files use the existing entity
chunker.

Rebuilds: `attachment_text` is permanent, so rescan/reprofile re-embeds files
without re-OCR, and profile swap supersedes old attachment chunks exactly as it
does for entities.

## OCR provider configuration

```toml
[ocr]
model = "gpt-4o-mini"    # OCR model, chosen independently of the embedding model
provider = "inherit"     # default; see below
base-url = ""            # optional override, only when provider explicitly named
api-key-file = ""        # optional override, only when provider explicitly named
```

- `provider = "inherit"` (default): reuse the primary `[indexer]` provider and
  its credentials when that provider is `openai`/`openai-compatible`.
- `provider = "openai"` with explicit `base-url`/`api-key-file`: a fully
  separate endpoint and key.
- A new `OcrProvider` registry mirrors `ProviderRegistry`: dispatch strictly on
  provider kind; an unknown kind fails the extraction job rather than silently
  reaching another provider; a URL carrying a user name or password is rejected
  at construction.
- An unset `[ocr]` section, or `provider = "inherit"` with no openai embedding
  provider, disables OCR: PDF attach returns `status='error'` with a named
  configuration gap; text-file attach still works. OCR never runs when the
  profile is unset.

## Search integration

- Attachment chunks use `ChunkKind::Attachment` and enter the existing vector
  store. `chunk_text` (vector_store.rs:1130) gains an `Attachment` branch
  resolving `(attachment_id, page, excerpt)` from `attachment_text` first, then
  the filename from `attachment`; `resolve_owner` renders the filename for an
  attachment owner.
- Existing semantic, hybrid and MMR search return attachment hits with kind
  `Attachment`, carrying `filename`, `page` and `excerpt`. A filter
  (`include_attachments`, default on) lets a caller restrict to entity hits.
  Disabling it makes the graph's own identity/observation space unaffected.
- The entity's `Identity` chunk is untouched; an attachment never replaces the
  entity's identity.

## Tools and admin UI

MCP tools: `attach_file` (base64 content, returns id+status), `list_attachments`,
`get_attachment` (bytes, status, per-page text), `delete_attachment`. The `/ui` admin
gains the same quartet via the browser: file picker upload, live status badges,
download, per-page text viewer, delete. Attachment tools register under their
own category so PR #55 scope gating covers them without new work.

`delete_attachment` deletes blob, text, its chunk rows (delete by
`owner_kind='attachment'`) and the job in one transaction; deleting an entity
cascades its attachments; re-attach bumps revision and supersedes the prior
chunk set. These are the only destructive paths, reachable only through the
fenced tools.

## Configuration and limits

`[attachments]`: `max-bytes` (per attachment, default 50 MiB), `workspace-byte-budget`
(default 256 MiB), `allow-mime` (default `text/*`, `text/markdown`,
`application/pdf`). Both budgets are enforced at `attach_file`, named in the
error. `[ocr] model` is the separate OCR model selection.

## Numbered requirements

1. `attach_file` returns immediately with `attachment_id` and `status`,
   regardless of file length; it never calls an LLM synchronously.
2. Text files become indexable without any `[ocr]` config.
3. A PDF's extracted per-page text is embedded into the serving vector profile
   through the existing indexer queue, not a second embedding path.
4. A semantic-search hit for file content returns `filename`, `page` and an
   `excerpt` from the extracted text.
5. A profile rebuild re-embeds attachment text from `attachment_text` and never
   re-runs OCR.
6. An attachment revision change supersedes an in-flight embed of the old
   revision (fence preserved).
7. `delete_attachment` and entity-delete cascade remove blob, text, chunks and
   job atomically.
8. Size and MIME violations are rejected at `attach_file` with a named reason.
9. An unknown OCR provider kind fails the extraction job with
   `unsupported ocr provider '<kind>'`; it never reaches another provider.
10. Attachment access is authorised by the PR #55 workspace registry, identical
    to entity access; no new grant type exists.
11. Every `attachment` mutation ships with its README section in the same
    change (repo rule).

## Migration

One `STRICT` migration creating the three tables, the indexes, and the
`attachment` FTS-equivalent nothing (search is vector + per-page text, not FTS
over blobs). Extraction and chunk tables are additive; no existing row or
revision changes. Rollback is drop-only (new feature, no production data).

## Testing

- Worker contract tests for the new `OwnerKind::Attachment` arm: chunks match
  page-per-chunk shape; pages over the token cap split without page drift;
  vanished/superseded attachment routes through the retry path.
- Fence chain: a revision bump between claim and commit refuses the commit.
- `chunk_text`/`resolve_owner` resolve attachment hits to `(filename, page,
  excerpt)`; unknown attachment returns `None`.
- Cascade: `delete_attachment` and entity-delete leave no `attachment`,
  `attachment_text`, `chunk_vector` or `attachment_job` row.
- Budgets: `attach_file` rejects over-`max-bytes` and over-workspace-budget
  with named reasons.
- OCR registry: a fake vision HTTP upstream proves dispatch, credential
  inheritance, and unknown-kind failure (mirrors the embedding-provider tests).
  Each guard/assertion is tested in the failing direction.
- MCP tool tests follow the repo's existing tools suite; admin UI verified
  manually against the running server.

## Non-goals and rejected alternatives

- **Separate embedding path for files** — rejected: forks the fenced,
  provider-neutral worker and splits rebuild semantics in two.
- **Filesystem for bytes** — rejected in brainstorming: breaks the single-portable-file
  workspace property and splits backup/delete/workspace-move across two artefacts.
- **Synchronous OCR** — rejected in brainstorming: multi-page PDFs would time
  out MCP calls.
- **Code files as attachments** — deferred: tree-sitter owns code symbols;
  mixing paths is a separate design.
- **Re-OCR on rebuild** — rejected: OCR is the expensive operation; permanent
  text makes rebuilds a re-embed.