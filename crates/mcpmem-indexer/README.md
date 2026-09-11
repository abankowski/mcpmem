# mcpmem-indexer

The durable, provider-neutral embedding indexer worker of
[mcpmem](https://github.com/abankowski/mcpmem), an MCP server that gives LLM
agents persistent memory.

This crate is a library. The worker runs as a role of the server binary.
Install the server crate to use it:

```sh
cargo install mcpmem --features indexer
mcpmem --role mcp,indexer          # one process
mcpmem --role indexer              # a separate worker process
```

## How the work arrives

Every effective graph change queues one `index_job` row per managed index
profile, inside the same transaction as the graph write. A crash therefore
loses no work. A second change to the same entity replaces the queued row and
raises the lease epoch, so an in-flight worker cannot commit a stale revision.

> ### The worker needs an index profile
>
> The worker claims a job only for a serving or a candidate index profile.
> Name a provider, a model and a dimension in the `[indexer]` section of the
> server configuration file. The server then adopts a profile at startup, and
> the queue drains.
>
> Without those three keys every index job stays in the `held` state. The
> worker polls and stays idle, and the caller keeps supplying vectors.

## How the worker runs

1. Claim one due job and take a 30-second lease.
2. Renew the lease, then call the embedding provider outside the write
   transaction.
3. Renew the lease again, then commit the vector into `profile_vector`.

The commit is refused when the lease expired, when the entity revision moved,
or when the profile is no longer writable. A repeated completion is a no-op.

On any failure the worker records the error in `index_job.last_error`, returns
the job to `pending`, and sets the next attempt one second later. Attempts are
unlimited: the worker never dead-letters a job, so a provider outage delays the
index instead of losing it.

## Providers

`ProviderRegistry` dispatches strictly on the profile field `provider_kind`. An
unknown kind fails the job with `unsupported embedding provider '<kind>'`
instead of reaching another provider. A URL that carries a user name or a
password is rejected at construction.

| `provider_kind` | Configuration | Notes |
|---|---|---|
| `ollama` | `MCP_MEMORY_OLLAMA_URL` | Posts to `<url>/api/embed` |
| `openai`, `openai-compatible` | `MCP_MEMORY_OPENAI_URL` and `MCP_MEMORY_OPENAI_API_KEY` | Both keys must be set together |
| `bedrock` | the standard AWS region and credential chain | Needs the `bedrock` Cargo feature. Amazon Titan Text Embeddings V2 only, with 256, 512 or 1024 dimensions |

The server configuration file carries the same three settings.
`[indexer] ollama-url`, `[indexer] openai-url` and `[indexer] openai-api-key-file`
configure the Ollama and OpenAI-compatible providers. An environment variable
wins over the matching file key.

Add a provider by implementing the `EmbeddingProvider` trait.

## Profiles and rebuilds

`index_profile_registry` holds one state per store key: `LegacyCompat`,
`Active`, `Rebuilding` or `Failed`. A profile is immutable and carries a
fingerprint. A rebuild queues every live entity against the candidate profile,
and activation promotes the candidate only after the full scan is verified.

Once a store adopts a profile, a direct vector write is refused with
`direct_vector_writes_disabled`. The worker owns the vectors from that point.

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
