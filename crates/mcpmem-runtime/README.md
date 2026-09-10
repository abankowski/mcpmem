# mcpmem-runtime

The runtime role composition of
[mcpmem](https://github.com/abankowski/mcpmem), an MCP server that gives LLM
agents persistent memory.

This crate is a library. It parses the role selection, checks it against the
compiled Cargo features, and supervises one task per role. Install the server
crate instead if you want to run the server:

```sh
cargo install mcpmem
```

## Roles

One process runs the server, a worker, or any compiled combination. The user
selects roles with the `--role` flag, which takes a comma-separated list. The
default is `mcp` alone, so an existing invocation keeps its behaviour.

| Role | Cargo feature | What it does |
|---|---|---|
| `mcp` | always compiled | The stdio or HTTP MCP transport |
| `indexer` | `indexer` | Polls the durable index-job queue every 250 ms and refreshes the vector snapshot |
| `webhooks` | `webhooks` | Polls the delivery outbox every 250 ms |

```sh
mcpmem                                 # the mcp role alone
mcpmem --role mcp,indexer              # server and embedding worker in one process
mcpmem --role indexer                  # a worker-only process
```

## A wrong selection fails at startup

The parser refuses the four cases below. None of them is a silent no-op:

- a role whose Cargo feature is absent —
  `runtime role 'indexer' was selected but its Cargo feature is not compiled`;
- a repeated role — `runtime role 'mcp' was selected more than once`;
- an empty list — `at least one runtime role is required`;
- an unknown name — `unknown runtime role '<name>'`.

## Supervision

`RuntimeComposition` starts one supervised task per role, in the order the user
gave. The process stops when the first role returns or fails, so a dead worker
does not leave a half-running server.

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
