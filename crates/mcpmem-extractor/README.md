# mcpmem-extractor

The durable attachment extraction worker of
[mcpmem](https://github.com/abankowski/mcpmem), an MCP server that gives LLM
agents persistent memory. The worker turns uploaded text and PDF attachments
into stored page text; the existing embedding worker turns that text into
searchable vectors.

This crate is a library. The worker runs as a role of the server binary.
Install the server crate to use it:

```sh
cargo install mcpmem --features extractor
mcpmem --role mcp,extractor      # one process
mcpmem --role extractor          # a separate worker process
```

The `extractor` Cargo feature implies `indexer`, because extraction enqueues
the same chunk-index jobs the indexer embeds.

## How the work arrives

Upload finalization writes the attachment bytes, its page table, and one
`attachment_job` row in one graph transaction. The `uploaded` job stays
durable when no extractor process runs: upload tools never depend on a local
extractor role. At startup a process that enables the attachment tools but
runs no `extractor` role logs a warning that a separate extractor process must
run. The warning proves nothing about whether one is actually running.

## External requirement: Poppler

PDF rendering is not bundled. The extractor process needs the external Poppler
commands `pdfinfo` and `pdftoppm` on its `PATH`. The release notes list this
requirement because no crates.io package can carry external binaries.

Check the requirement with this command (identical in bash and fish):

```sh
command -v pdfinfo && command -v pdftoppm && pdfinfo -v && pdftoppm -v
```

A missing executable, a nonzero renderer exit, or an empty rendered image is
an observable `render` error. The worker never reports success without page
images.

## How the worker runs

1. Sweep expired upload sessions, then claim one due attachment job with a
   30-second lease.
2. Read the stored blob without holding any transaction. A text file decodes
   as UTF-8, drops a BOM, and normalizes CRLF into LF; its page is page 1.
3. A PDF runs `pdfinfo` for the page count and page size, and `pdftoppm` for
   one page per invocation, with argument vectors — never a shell. Each
   rendered image goes to the vision OCR provider. `pdfinfo` and `pdftoppm`
   each get a hard 8-second deadline: a stalled renderer is killed, never
   left to hold the worker loop. A PDF over 64 pages, a page box that
   renders more than 16,777,216 pixels at 150 dpi, or a rendered PNG over
   32 MiB is refused at stage `render` before any OCR call — the page-count
   and pixel refusals happen before the first render.

   The vision request has a 15-second deadline, shorter than the 30-second
   claim lease. The lease renewal runs only after a request returns, so any
   response that outlived the lease would fail the renewal fence and the
   transcription would be discarded; the deadline keeps every in-flight
   request inside the lease. A response whose `finish_reason` is `length`
   is a provider failure: the model stopped at the token limit, and a
   truncated transcription is never published as a complete page.

4. One claim transcribes at most eight pages. A longer PDF keeps its lease
   re-armed per page, defers at the bound, and resumes on a later claim
   from its durable checkpoints instead of holding the role loop for a
   whole document: the loop rotates to the next workspace, and the deferred
   job becomes claimable again once its lease expires.
5. Commit every page and segment row, status `ready`, the incremented
   revision, and the chunk-index job enqueues in one fenced transaction.

Checkpointed page rows in `attachment_text` are provisional: a resumed
claim skips the pages it already transcribed, and only the fenced commit
publishes them. If the lease is lost before the commit, the rows are
harmless leftovers of a superseded attempt — the attachment stays
`extracting` and a later claim resumes from them.

The commit is refused when the lease expired, the attachment revision moved,
or the parent entity vanished. A refused commit publishes nothing. A transient
failure (vision provider, render, storage) keeps the attachment `extracting`
with the last `error_stage` and `last_error`, and the job retries up to eight
attempts. A terminal failure — invalid UTF-8 (`decode`) or invalid OCR
configuration (`config`) — sets status `error` immediately. Invalid OCR
configuration fails the PDF job before any image or API key leaves the
process; text extraction keeps working with the same settings.

The page caps are sized so a maximum-size PDF (64 pages) completes within
the eight-claim dead-letter bound at eight pages per claim.

OCR calls never run inside a graph write transaction.

## OCR configuration

The `[ocr]` section of the server configuration file names the vision
endpoint and model:

```toml
[ocr]
model = "gpt-4o-mini"
provider = "inherit"
# vision-url = "https://api.openai.com/v1/chat/completions" # optional for first-party OpenAI
# api-key-file = "/path/to/vision-key" # required for an explicit openai provider
```

`inherit` reuses the primary embedding provider's effective API key (same
environment-over-file precedence). Only a first-party `openai` primary may use
the default OpenAI vision host when `vision-url` is absent. An inherited
`openai-compatible` primary needs an explicit `vision-url` outside the default
OpenAI host, so the primary key never reaches OpenAI. An explicit `openai`
provider needs a readable, non-empty `api-key-file`. Any other provider name
fails PDF jobs with `unsupported ocr provider '<kind>'` at stage `config`.

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
