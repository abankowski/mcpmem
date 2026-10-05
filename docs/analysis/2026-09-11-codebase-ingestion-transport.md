# Codebase ingestion: passing a codebase to mcpmem's indexer without manual copy

Date: 2026-09-11
Status: research, decision pending
Owner: mcpmem

## 1. The problem

The `code_index` tool parses source files on the **server's local disk** and
stores symbols in a per-project SQLite database. For a deployment where the
codebase lives on the developer machine or a git host, and mcpmem runs on a
different server, the operator must first copy the code to the server. That
copy is the friction this document removes.

The goal: a repeatable, automated way to move a codebase into the indexer,
with no manual copy, in a remote (Streamable HTTP) deployment.

## 2. Current state, verified

|Fact|Evidence|
|---|---|
|`code_index` reads bytes from the local filesystem|`std::fs::read(path)` at `src/actions/code.rs:149` (`parse_one`)|
|Entity names anchor to the server's current working directory|`canonical_base()` at `src/actions/code.rs:206`; `rel_path` strips the base prefix|
|The parser itself is buffer-based and has no disk coupling|`code::parse_source(lang, bytes)` at `src/code/mod.rs`|
|The write path already accepts an explicit file list and an explicit base|`index_paths(kg, files, base, force, snippets)` at `src/actions/code.rs`|
|Incremental skip is a BLAKE3 content hash compared against the database|`parse_one` at `src/actions/code.rs:152-160`|
|The walk honors `.gitignore` and skips build directories|`code::walk` at `src/code/mod.rs`|
|The server never fetches code today|the `indexer` role embeds vectors; it has no code-fetch path (`src/indexer_provider.rs`)|
|MCP accepts large client-supplied payloads today|`code_embed` (vector arrays), `vector_batch_upsert` at `src/vector_actions.rs:416`|
|No repository-cache directory concept exists in config|`Config` at `src/config.rs:11` has `memory_file_path`, bind address, transport only|

The parse layer already works on in-memory buffers. The only disk coupling is
the transport in `parse_one` and the walk. Moving bytes to the server is the
entire problem.

## 3. The shared seam

Both candidate options reduce to one operation:

```
write bytes into {data}/projects/{project}/...
index_paths(kg, files, base = {data}/projects/{project}, force, snippets)
```

`index_paths` already takes the file list and the base as parameters. A new
ingest tool passes its project cache directory as the base, so entity names
stay repo-relative and consistent with `code_outline`, `code_search` and
`code_get_symbol`.

Two compatibility notes:

- New projects get the cache dir as base. Projects indexed before this change
  anchored names to the server cwd; they are unaffected because an ingest
  project is a new project.
- `lookup_file_name` (`src/actions/code.rs`) assumes the cwd base. The new
  tool family must pass the same per-project base it used at ingest, or
  lookups by absolute path will mis-resolve.

## 4. Options

### Option A: server pulls with git — `code_index_git`

The tool takes `{ url, ref?, project, force?, snippets? }`. The server clones
or fetches the repository into the project cache dir, checks out the ref, and
runs the existing walk and `index_paths`. A re-index is `git fetch` + checkout
+ `index_paths`; the content-hash skip covers the unchanged files.

- Auth: public repos need none; private repos need a token. Store per-host
  tokens in the server config (`git.tokens.<host>` or an env var), never in
  the tool call.
- Git access: `git2` crate (new dependency, bundles libgit2) or shell out to
  system git. `git2` is the safer choice: no PATH dependence, no shell
  injection surface.

Pros: one tool call; canonical repository state; correct merged history, not a
snapshot; honors `.gitignore`; no client tooling; the most reasonable UX for a
git-hosted codebase.

Cons: the server needs network egress to the git host; token storage and
rotation is new surface; a full monorepo clone is slow and large; `git2`
adds compile time and a C dependency.

### Option B: client pushes files over MCP — `code_ingest`

Two tools, following the `code_embed` batching precedent:

- `code_ingest_files { project, files: [{ rel_path, content | b64, delete? }], force? }`
  — per-file content, sent as JSON. The client hashes files locally and sends
  only the changed ones; the server writes them into the cache dir and calls
  `index_paths`. `delete: true` prunes removed files (the watcher purge
  plumbing already exists).
- `code_ingest_archive { project, archive_b64, format, strip? }` — a whole
  repository as a tar/zip bundle (`git archive HEAD | base64`), for first
  index or non-git sources.

A helper script ships in the repo (`scripts/push-project.sh`) that hashes
locally with `b3sum`, diffs against a local state file, and sends only the
delta. The agent never touches the bytes; a small client-side program makes
the tool calls.

Pros: no server egress; no new server dependencies; works over stdio and HTTP
alike; uses the existing bearer/OAuth auth; fits air-gapped deployments; the
codebase already accepts client-supplied payloads on this exact path.

Cons: whole-repo transfer is base64 with +33% overhead; Streamable HTTP
proxies commonly cap request bodies (nginx `client_max_body_size` defaults to
1 MiB), so large archives need a chunked protocol or per-file calls; the
client needs the helper script (a one-time small addition).

### Option C: raw HTTP upload endpoint

A dedicated route `POST /api/v1/ingest/{project}` that streams a tarball
(`Content-Type: application/x-tar`), authenticated like the MCP endpoint with
a scope check. No base64 overhead; `Content-Length` bounded by config.

Pros: streamed bytes, no encoding cost, natural for scripted clients
(`curl --data-binary @repo.tar.gz`).

Cons: a new public route outside the MCP tool surface is a new security
review surface; stdio deployments cannot use it without a wrapper tool anyway;
it duplicates Option B's payload handling for a marginal efficiency gain.
Defer unless a non-MCP client appears.

### Option D: out-of-band sync (`rsync`/Syncthing) + existing `code_index`

`rsync -a --delete ./ server:/data/projects/foo/`, then call the existing
`code_index { path }`. Zero code changes, mature, byte-level incremental.

This is exactly the "copy code to the server" the ask rules out, automated.
It stays as a documented baseline for LAN deployments where SSH access to the
server exists and the repeated copy is acceptable. It is not a
recommendation for the remote case; and it needs shell access that a hosted
server does not expose.

### Option E: `code_ingest_url` — server downloads a codeload tarball

The tool takes a known codeload URL (`https://codeload.github.com/owner/repo/
tar.gz/ref` or the GitHub API tarball endpoint) and streams it to the cache
dir. Public repos need no token; private repos use a configured token.

Subsumed by Option A for git-hosted code: git fetch gives the same bytes with
proper incremental updates, and git is a smaller dependency surface than an
HTTP-client code path. Worth keeping only if the `git2` dependency is refused.

## 5. Comparison

|Criterion|A: git pull|B: client push|C: HTTP route|D: rsync|E: URL tarball|
|---|---|---|---|---|---|
|Manual copy|none|none|none|automated copy|none|
|Server egress|yes|no|no|n/a|yes|
|New server deps|`git2`|none|none|none|HTTP client (exists for oauth)|
|Transport|MCP tool|MCP tool|HTTP route|SSH|MCP tool|
|Incremental|fetch + hash skip|client-side hash delta|re-send bundle|byte delta|full re-download|
|Air-gap safe|no|yes|yes|yes|no|
|Auth story|config tokens|existing bearer|new route scope|SSH keys|config tokens|
|Implementation effort|med|med (tool) + small script|med|none|low|
|Works over stdio|yes|yes|no|n/a|yes|

## 6. Recommendation

Deployment context, stated by the owner: mcpmem is a **shared service, and it
is never multi-tenant**. All users share one graph and one set of code
indexes. There is no per-principal project ownership to enforce. This removes
tenant-isolation work that pointer-based transport would otherwise need, and
it changes the trade between the two options.

**Primary — `code_index_git` (Option A).** For a shared index, git pull is the
better primitive, for three reasons:

- One tool call indexes the whole canonical repository at a pinned ref. A
  refresh is a server-side `git fetch`; no client state exists.
- Deterministic convergence: every user refreshing the shared project points
  at the same ref and the same content hashes, so the index is the same for
  everyone. A stale local state cannot produce a half-updated index.
- It stores a reference (url + ref), not a one-off snapshot. The canonical
  source stays the git host.

**Fallback — `code_ingest` (Option B).** It stays worth building, for the
cases git pull cannot serve: non-git sources (generated code, ad-hoc file
sets), and servers with no egress. Under a shared index it needs one change
to be safe: **declarative full-set replacement**, not client-local delta.
The client sends the complete manifest `{ rel_path -> hash }`; the server
diffs against what it already holds, pulls only the missing or changed
content, and purges anything not in the set. A client-local state file is
wrong here — two humans pushing to one shared project with local deltas
last-writer-wins and silently orphans the other's files. Also required:
sanitize `rel_path` (no absolute paths, no `..`, no symlink writes) before
touching the cache dir.

Both paths share the cache-dir seam from section 3.

Ship order: `code_index_git` first, then `code_ingest_files`
(declarative) if a concrete non-git use case appears.

Rejected for now: Option C (defer until a non-MCP client exists), Option D
(the user's constraint), Option E (subsumed by A).

## 7. Decisions (resolved 2026-09-12)

- The server has network egress to the git host.
- The server has no git-token storage yet. Token storage is part of the
  `code_index_git` design, not a blocker.
- No non-git source exists. `code_ingest` stays designed but unbuilt.
- The primary consumers are an IDE extension and a coding agent.

## 8. IDE and coding-agent wiring

The deciding property: the client passes only `url` + `ref`. Git-host
authentication is the server's own operator-configured credential, never the
client's. Tool parameters must carry no tokens, because the stored project
reference would retain any credential placed in the URL, and server logs
would too.

Token home (design): a per-host secret loaded at startup, in the style of the
existing `--auth-token-file` / `--oidc-client-secret-file` patterns
(`src/config.rs`). Fine-grained, read-only, scoped to the repositories the
shared service is authorized to index. The OAuth human approves the `code`
scope; the git-host identity is the service's identity, which is a deliberate
trust design to state in the tool description.

Client flows:

- Coding agent: the agent reads `git remote get-url` and the current branch
  in the workspace, calls `code_index_git { url, ref }`, and calls it again
  after a branch switch or commit. One stateless MCP tool; no client state.
  The `code` scope already covers it.
- IDE extension: the same detection in the host language (VS Code or
  JetBrains); trigger on workspace open, branch switch, and commit. A
  background poll variant of `code_watch` is the deferred extension.

Project identity: derive the project key from the URL as
`{host}-{owner}-{repo}`, normalized to the `[A-Za-z0-9_-]` charset that
`validate_project` enforces (`src/code_registry.rs:96`). Two users wiring the
same repository converge on the same shared index, which is the point of a
shared brain. Keep the explicit `project` override for humans.

Ref semantics: default is the remote default branch; an agent passes the
branch it is working on.

Clone strategy: `git clone --filter=blob:none` keeps the first clone bounded
(blobs fetch on checkout), and re-index is `git fetch` + checkout — the
content-hash skip in `parse_one` handles the unchanged files. `code::walk`
already honors `.gitignore`.

## 9. Open questions

- Whether per-host git tokens live in the config file, a secret file per
  host, or the environment. Decide with the operator who provisions them.
- Whether project cache dirs should live next to the memory file
  (`{memory_file_path}.d/projects/...`) or be a separate config path.
- Whether `code_watch` should gain a git-pull variant for remote deployments
  (poll fetch on an interval instead of filesystem notify), for IDEs that
  want freshness without re-invoking the tool.