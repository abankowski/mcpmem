# Managed code repositories — design

Date: 2026-09-12. Status: pending approval.

## Goal

Let an operator register remote git repositories for code-symbol indexing,
trigger a fresh clone+index and later reindexes, and remove a repository
(registration, clone, and indexed database) with one action. The same
operations must be reachable from the admin web UI and from MCP tools.

The index pipeline itself already exists: `code_index` parses a local
directory tree and stores symbols in a per-project SQLite database
(`<memory_file>.code/<project>.code.db`), incrementally via content hash.
This design adds the managed-repository layer on top: registration,
credentials, git acquisition, and lifecycle.

## Decisions (user-confirmed)

|Decision|Choice|
|---|---|
|Repository kind|Remote git repo + credentials (`url` + optional token/SSH key)|
|Remove semantics|Full wipe: registration + clone + indexed DB + any watcher|
|MCP surface|Full parity: `code_repo_add`, `code_repo_list`, `code_repo_reindex`, `code_repo_remove`|
|UI reindex|Async with polling: POST returns 202, background task updates state, UI polls the list|
|Git acquisition|Shell out to the `git` binary. No new Rust dependency (`git2` would pull libgit2/curl into the tree and trip the repo's dependency guards). Host assumed to have git >= 2.30 (verified at server startup; if absent, repo management is disabled with a clear error)|

## Architecture

```
HTTP /ui/api/repos (admin scope)      MCP tools (code_repo_*)
        │                                      │
        └──────────────┬───────────────────────┘
                       ▼
              repo service (src/repos.rs)
        register · list · reindex · remove
                       │
        ┌──────────────┼──────────────┐
        ▼              ▼              ▼
   code_repo       git binary     code_registry::drop_project
   table (0006)    clone/fetch     + watcher stop
```

## Components

### 1. Persistence — migration `0006_code_repos.sql`

Applied by the existing checksummed migration runner in `mcpmem-core`
(`crates/mcpmem-core/src/events.rs` schema_migration table).

```sql
CREATE TABLE code_repo (
  key              TEXT PRIMARY KEY,  -- project id; validated by code_registry::validate_project
  url              TEXT NOT NULL,
  auth_kind        TEXT NOT NULL CHECK (auth_kind IN ('none','token','ssh')),
  auth_secret      TEXT,              -- raw token or SSH private key (see Security)
  snippets         INTEGER NOT NULL DEFAULT 0,
  state            TEXT NOT NULL DEFAULT 'pending',
                   -- pending|cloning|indexing|indexed|error|removing
  last_error       TEXT,
  last_indexed_us  INTEGER
) STRICT;
CREATE INDEX code_repo_state ON code_repo(state);
```

`key` must pass `code_registry::validate_project` (charset `[A-Za-z0-9_-]`,
max 64) because it doubles as the per-project database name.

### 2. Repo service — new module `src/repos.rs` (feature `code`)

All lifecycle logic lives here; the HTTP handlers and MCP handlers are thin
adapters over it.

- `register(key, url, auth, snippets)` — validate key/url; insert row
  (state `pending`); return.
- `list() -> Vec<RepoRow>` — rows with `auth_secret` masked (`•••`),
  last index state, error text.
- `reindex(key)` — update or fetch the clone, then run the existing
  `actions::code::handle_code_index` on the worktree with `project=key`.
- `remove(key)` — stop watcher (if any), evict the registry handle,
  delete `<key>.code.db` + `-wal`/`-shm`, delete the clone directory,
  delete the row. Full wipe.
- `concurrency`: exactly one background operation per key at a time —
  a `Mutex<HashSet<String>>` of keys with an in-flight job. Second trigger
  returns 409 (`already_in_progress`).

### 3. Git acquisition — `src/repos.rs` / `src/git.rs`

Worktree root: `<memory_file>.code/repos/<key>/` (sibling of the project
DBs; lives under the same `.code` dir).

- Initial: `git clone --quiet <url> <worktree>` (default branch HEAD).
- Reindex: `git -C <worktree> fetch --quiet origin` then
  `git -C <worktree> reset --hard --quiet @{u}`.
- Credential hygiene (no secret in argv, URL, or on-disk config):
  - token: `GIT_CONFIG_KEY_0=http.extraHeader`,
    `GIT_CONFIG_VALUE_0=Authorization: Bearer <token>`,
    `GIT_TERMINAL_PROMPT=0` — via env, not `-c` (avoids `/proc/*/cmdline`
    exposure and leaves no config in the clone).
  - ssh key: write to a `0600` temp file in the `.code` dir;
    `GIT_SSH_COMMAND="ssh -i <file> -o IdentitiesOnly=yes
    -o StrictHostKeyChecking=accept-new"`; unlink after the command.
  - `auth_kind = none`: bare clone; public repos work.
- Every git command runs with a hardcoded 10-minute timeout and a bounded
  log capture;
  on failure the repo state becomes `error` with `last_error` set.
- Stale worktree repair: if the clone dir exists but is not a git repo, or
  `git status` fails, remove it and re-clone.

### 4. HTTP admin API — `src/http.rs` (existing pattern, `admin` scope gate)

- `GET /ui/api/repos` — list (secrets masked), 200.
  ~~Also returns `codeSectionEnabled` so the UI can hide the section when
  code indexing is compiled out / disabled.~~ **SUPERSEDED (2026-09-12,
  during implementation):** the routes exist only when built with the `code`
  feature; a build without it answers 404 and the admin SPA hides the
  section on 404 — the same convention the webhooks section already uses.
  No flag is returned.
- `POST /ui/api/repos` — `{key, url, auth_kind, auth_secret?, snippets?}`.
  202 Accepted, job started (clone+index). 409 on duplicate key or an
  in-flight job for that key. 400 on invalid key/url/state.
- `POST /ui/api/repos/{key}/reindex` — 202, job started; 409 if in flight;
  404 unknown key.
- `DELETE /ui/api/repos/{key}` — 202 (async wipe: row first marked
  `removing` so the list excludes it), 404 unknown key.

All bodies use camelCase keys, matching `src/ui/admin.js` conventions.

### 5. Admin UI — `src/ui/admin.html` + `admin.js` + `admin.css`

New section **Repositories** below Principals:

- Table: key, url, auth kind, state badge (pending/cloning/indexing/
  indexed/error), last indexed, error tooltip, actions: Reindex, Remove.
- Add form (existing `<dialog id="form">` pattern): key, url,
  auth kind select (none/token/ssh), secret textarea (SSH keys are
  multi-line), snippets checkbox.
- Polling: after any mutation that returns 202, poll `GET /ui/api/repos`
  every 2 s until no row is in a transitional state (or a 10 s error/EOF
  backoff); the seed flow for transitions mirrors `admin.js`'s `load()`.
- `admin.js` keeps its current section — add a second `api()` consumer,
  no shared-state changes. The OAuth/scopes flow (`beginAuth`,
  `completeAuth`) is untouched.

### 6. MCP tools — `code_tools.json` + `src/actions/code.rs`

Synchronous (MCP convention; `code_index` already blocks):

- `code_repo_add` — `{key, url, auth?: {kind, secret}, snippets?}`;
  registers, then clone+index synchronously; returns final state.
- `code_repo_list` — registered repos with masked secrets + state.
- `code_repo_reindex` — `{key}`; fetch + reindex synchronously; reports
  indexed symbol counts from the underlying `handle_code_index` result.
- `code_repo_remove` — `{key}`; full wipe, synchronous.

Tool descriptors added to `code_tools.json`; dispatch wired in
`src/server.rs` (`CODE` static map + match arm), same as the existing
`code_index`/`code_outline`/... arms.

### 7. Watcher stop — `src/watcher.rs`

`spawn_watcher` currently detaches a thread that pins the project's
`Arc<GraphHandle>` forever; no stop exists. Managed-repo removal must not
race a watcher on the same DB file.

- Add a process-wide `Mutex<HashMap<String, WatchHandle>>` registry:
  `spawn_watcher` registers, the watcher thread removes itself on exit.
- `stop_watcher(project)`: signal a shared `AtomicBool` (or channel) the
  watcher loop checks each debounce; join the thread (bounded wait,
  e.g. 5 s) and delete the handle from the map.
- `code_watch` (ad-hoc local dirs) keeps working unchanged; its watcher
  now also lands in the registry so a later `remove` on the same project
  can stop it. `code_registry::drop_project(key)` evicts the weak/strong
  handle after the watcher is stopped, then the DB files are deleted.

## Data flow

Add (HTTP, async path):
`POST /ui/api/repos` → insert row (pending) → spawn task → state=cloning →
`git clone` → state=indexing → `code_index` on worktree → state=indexed |
error(+last_error). UI polls list.

Reindex (MCP, sync path):
`code_repo_reindex {key}` → fetch + reset worktree → `handle_code_index`
(project=key) → result JSON returned to caller. State updated the same way.

Remove:
`DELETE /ui/api/repos/{key}` → stop watcher → `drop_project` → delete DB
files → delete worktree → delete row. A removal mid-clone/mid-index is
rejected with 409 (job in flight) — the operator retries after the job
finishes or errors.

## Security

- Credentials at rest: `auth_secret` stored raw in the memory DB — the
  same trust boundary as the knowledge graph itself (memory file +
  `.code/` dir permissions). **Stated plainly: git needs the secret
  itself; digest-hashing is impossible.** If encryption-at-rest is wanted
  later, it is a separate subsystem (key management does not exist today).
  The design keeps this to one column and one documented boundary; no
  half-measures.
- Secrets never appear in argv, URLs, git config files, or API responses
  (masked as fixed-length `•••` in `list()`).
- SSH key files chmod `0600`, unlinked after the git command.
- `GIT_TERMINAL_PROMPT=0` → an auth failure is a prompt-free error with
  `last_error` set, never a hang.
- The admin API stays behind the existing `admin`-scope OAuth gate
  (`http.rs`, `admin_*` handlers). The registry lets the server fetch
  arbitrary URLs — same trust as principals approval today; noted as an
  accepted boundary, not silently.

## Error handling

- Invalid key (charset/length) → 400, no row written.
- Invalid/unreachable URL → clone fails → state=error, `last_error`
  contains the git stderr tail (truncated, redacted of the auth header).
- Duplicate key → 409.
- Job in flight for key → 409 `already_in_progress`.
- DB file deletion failure → remove returns error, row transitions to
  `error` with `last_error` set, so the operator sees it in the list with
  error text and can retry reindex (worktree repair re-clones) or remove.
  ~~row stays `removing`, operator sees it in the list with error text~~
  **SUPERSEDED (2026-09-12, final review):** a row left in `removing` is
  filtered from the list (the design's own list rule), so a stuck wipe was
  invisible and the key unrecoverable from the UI. Transitioning to
  `error` keeps the row visible and actionable. The wipe phase also
  evicts the project's code-vector store (its HNSW index and live SQLite
  connection on the same file) before deleting the database.
- Server restart leaves rows mid-flight (`cloning`/`indexing`): on
  startup, mark those `pending`; a reindex or remove retries from a known
  point. Worktree repair: if the clone dir exists but is not a valid git
  worktree, remove it and re-clone (a partial clone dir that fails
  `git status` is simply deleted; `git clone` cannot resume into an
  existing directory).

## Testing

- `repos.rs` unit tests with a local file:// git repo fixture (init +
  commit in a temp dir; `git` binary present in CI, or skipped when
  absent): register→clone→index, reindex after a new commit (symbol count
  grows), remove wipes DB + worktree + row, duplicate key, bad URL error
  state, concurrency 409.
- `tests/ui_http.rs`-style router tests for the four endpoints with the
  existing in-process axum harness and a `file://` repo: auth gating
  (401/403), 202 + list transition, 409s, 404s.
- MCP: extend `tests/` (there is no `code` integration test today —
  create `tests/repos_tools.rs` behind `#[cfg(feature = "code")]`) driving
  `handle_code_repo_*` directly against a fixture repo.
- Watcher stop test: spawn `code_watch` on a fixture dir, call
  `stop_watcher`, assert the thread exits and `drop_project` returns, DB
  file removable.
- Pre-flight, unchanged: repel none of the existing CI steps; the new
  migration gets the standard double-apply/checksum tests.

## Non-goals (v1)

- No auto-watch of managed repos — reindex is explicit (UI button, MCP
  tool).
- No branch/pin selection — default branch HEAD only.
- No encrypted-at-rest credentials, no keychain integration.
- No git LFS policy tuning (clone fetches what the repo declares).
- No job persistence/queue — one in-flight job per key, in process;
  restarts reset transient states to `pending` and the operator retries.