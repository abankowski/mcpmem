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
3. A PDF runs `pdfinfo` for the page count and `pdftoppm` for one page per
   invocation, with argument vectors — never a shell. Each rendered image goes
   to the vision OCR provider.
4. Re-arm the lease after each page, then commit every page and segment row,
   status `ready`, the incremented revision, and the chunk-index job enqueues
   in one fenced transaction.

The commit is refused when the lease expired, the attachment revision moved,
or the parent entity vanished. A refused commit publishes nothing. A transient
failure (vision provider, render, storage) keeps the attachment `extracting`
with the last `error_stage` and `last_error`, and the job retries up to eight
attempts. A terminal failure — invalid UTF-8 (`decode`) or invalid OCR
configuration (`config`) — sets status `error` immediately. Invalid OCR
configuration fails the PDF job before any image or API key leaves the
process; text extraction keeps working with the same settings.

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
