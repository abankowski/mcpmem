# Isolated Knowledge Workspaces — Design

**Status:** Design approved in sections on 2026-10-01. Written specification awaits review.

## Goal and scope

A workspace owns one knowledge graph. Each graph has its own entities, relations, observations, indexes, events, and subscriptions. A user can own workspaces, grant access, list accessible workspaces, and choose a personal default. Every workspace control has an MCP tool. The `/ui` graph viewer has a workspace dropdown.

Code-project databases do not become workspaces. This change does not add workspace deletion, graph import, ownership transfer, or cross-workspace search.

## Evidence and root cause

- `crates/mcpmem-core/src/schema.rs:25-99` defines graph tables and FTS indexes without a workspace key.
- `src/server.rs:398-413` constructs one graph handle and one vector store from one path.
- `src/authz.rs:49-63` checks tool categories, not graph access.
- `src/http.rs:386-420` maps anonymous HTTP to a static principal when no credentials exist.
- `crates/mcpmem-core/migrations/0004_oauth.sql:40-46` stores a mutable principal name on OAuth tokens.
- `src/ui/graph.js:197-212,715-724` has asynchronous graph requests without a workspace generation guard.

**Root cause:** The graph, its derived data, and the request principal have no shared workspace boundary. Existing scopes cannot supply that boundary because they say which kind of tool may run, not which graph it may read. A single database with a workspace column is possible, but it needs a filter in every query, index, event, and worker path. One omitted filter can expose private data. Separate graph files avoid that failure class.

The Second Brain memory records an earlier requirement to put sensitive personal data in a separate zone. That observation supports isolation; it does not decide this feature's access policy.

## Architecture and single owners of facts

| Fact | Owner | Consumers |
|---|---|---|
| Workspace ID, name, visibility, owner, grants, defaults, machine IDs and token digests | Workspace registry, a separate SQLite file beside the configured memory file | MCP, viewer, runtime roles |
| Entity, relation, FTS, event, subscription, job and vector data | One SQLite file per workspace | Graph handle, vector store, indexer, webhook worker |
| Verified human identity | OIDC `(issuer, subject)` through the existing principal resolver | Workspace registry and OAuth grants |
| Effective workspace access | One registry authorization function per request | MCP graph tools, vector tools, viewer and subscription tools |
| Tool-category scope | Existing category registry and OAuth scope checks | MCP and viewer |
| Selected viewer workspace | Viewer state, not the user's stored default | All viewer data requests |

Keep the existing memory file as the legacy workspace file. Place the registry at `<memory-file>.workspaces.sqlite` and new graph files under `<memory-file>.workspaces/<workspace-id>.sqlite`. Generate immutable UUID workspace IDs. Store graph paths in the registry; never derive a path from a supplied workspace name or ID. The registry never stores entity content. The legacy graph file still contains existing OAuth and principal tables; those tables do not join graph data across files.

The registry uses SQLite `STRICT` tables. It enables foreign keys on every registry connection. It owns these fields:

```sql
CREATE TABLE workspace (
  workspace_id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  visibility TEXT NOT NULL CHECK (visibility IN ('private', 'public')),
  owner_id TEXT NOT NULL,
  graph_path TEXT NOT NULL UNIQUE,
  created_us INTEGER NOT NULL
) STRICT;
CREATE TABLE workspace_grant (
  workspace_id TEXT NOT NULL REFERENCES workspace(workspace_id),
  principal_id TEXT NOT NULL,
  role TEXT NOT NULL CHECK (role IN ('reader', 'writer')),
  PRIMARY KEY (workspace_id, principal_id)
) STRICT;
CREATE TABLE workspace_default (
  principal_id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspace(workspace_id)
) STRICT;
CREATE TABLE machine_account (
  principal_id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  scopes TEXT NOT NULL,
  token_digest BLOB NOT NULL UNIQUE,
  revoked INTEGER NOT NULL DEFAULT 0 CHECK (revoked IN (0, 1))
) STRICT;
CREATE TABLE workspace_registry_version (
  version INTEGER PRIMARY KEY
) STRICT;
```

The existing OAuth principal store remains in the legacy graph file. New graph files contain empty base OAuth tables because they share the core schema. Only the legacy file serves authentication records. The registry validates human IDs through the existing resolver for built-in and runtime principals. It recognizes `machine:local` and the configured `machine:static` as built-in accounts. It never treats an unconfigured static account as registered.

The server selects one workspace before it creates a graph handle for a request. It checks the caller's category scope and workspace access before it opens the selected file. A bounded handle cache uses workspace IDs as keys. Workers enumerate registered files with bounded work per cycle. The indexer and vector publisher operate on one selected graph at a time. Each graph retains its own durable jobs and profiles. The webhook worker reads subscriptions and events from the same graph file.

A workspace registry entry becomes active only after its graph file passes schema setup. An unregistered file after a crash is not reachable through any graph request. Startup reports such a file for operator review; it does not silently delete it. Registry changes use transactions. A request checks grants at dispatch time, so revocation does not wait for token renewal or a cache expiry.

## Identity and access

Human IDs use the verified issuer and subject, not the person's name. Encode them with `mcpmem_oauth::principal_id(iss, sub)` and a `human:` prefix. Use `machine:local` for stdio and `machine:static` for the configured static bearer. MCP-created machine accounts get `machine:<uuid>` IDs and distinct random bearer credentials. Store only a digest of each credential; show the secret once. Revocation stops the credential on the next request. A human with the existing `admin` scope, or trusted local stdio, can create and revoke machine credentials. Machine credentials hold only tool-category scopes. They cannot get `admin` from a workspace grant.

Existing OAuth tokens contain a mutable name, not the stable issuer and subject. The rollout revokes old OAuth grants and requires a new login. New grants carry the stable human ID. Do not infer an ID from an old name. Operators must record this session break in the deployment notice.

| Caller | Private read | Private write | Change visibility or grants | Public read |
|---|---|---|---|---|
| Owner | Yes | Yes | Yes | Yes |
| Reader | Yes | No | No | Yes |
| Writer | Yes | Yes | No | Yes |
| Other authenticated caller | No | No | No | Yes |
| Anonymous HTTP caller | No | No | No | No |

A public graph does not grant writes. A writer can read. An owner can grant `reader` or `writer` access to an existing registered human or machine ID. Only the owner changes visibility and grants. The owner cannot revoke their own ownership through these tools. The registry rejects a grant to an unknown or disabled identity. Graph access is necessary in addition to the existing tool category scope. Refuse the removal of a principal that owns a workspace. If an operator removes a built-in owner from a file, refuse startup until the operator restores that identity.

Workspace listing exposes public graph IDs, names, and visibility to authenticated callers. It also shows the caller's owned and granted private graphs. It never exposes private graph metadata to other callers. List results include the caller's role and `isDefault`; only the owner sees the owner's identity and the grant list. The registry does not grant access from a display name.

## MCP contract

Use the existing camel-case JSON argument convention. Use `workspaceId` for graph selection. Each graph and vector tool gains an optional string `workspaceId`; omission selects the caller's saved default. Selection occurs once before a batch starts. A request never joins records from two workspaces. Code-project tools remain unchanged. Webhook subscription tools gain the same selector and need workspace ownership in addition to their current scope.

| Tool | Input fields | Result fields | Scope and access |
|---|---|---|---|
| `create_workspace` | `name: string`, `visibility: "private" \| "public"` | `workspace: WorkspaceView` | `graph-write`; caller becomes owner |
| `list_workspaces` | `cursor?: string`, `limit?: integer` | `workspaces: WorkspaceView[]`, `nextCursor: string \| null` | `graph-read`; only accessible workspaces |
| `get_workspace` | `workspaceId: string` | `workspace: WorkspaceView` | `graph-read`; accessible workspace |
| `set_workspace_visibility` | `workspaceId: string`, `visibility: "private" \| "public"` | `workspace: WorkspaceView` | `graph-write`; owner |
| `list_workspace_grants` | `workspaceId: string` | `grants: { principalId: string, role: "reader" \| "writer" }[]` | `graph-read`; owner |
| `grant_workspace_access` | `workspaceId: string`, `principalId: string`, `role: "reader" \| "writer"` | `grant: { principalId: string, role: "reader" \| "writer" }` | `graph-write`; owner |
| `revoke_workspace_access` | `workspaceId: string`, `principalId: string` | `revoked: boolean` | `graph-write`; owner |
| `set_default_workspace` | `workspaceId: string` | `workspace: WorkspaceView` | `graph-read`; accessible workspace |
| `create_machine_account` | `name: string`, `scopes: string[]` | `principalId: string`, `token: string` | `graph-write` enabled; `admin` human or trusted local stdio; token appears once |
| `list_machine_accounts` | No fields | `accounts: { principalId: string, name: string, scopes: string[], revoked: boolean }[]` | `graph-write` enabled; `admin` human or trusted local stdio |
| `revoke_machine_account` | `principalId: string` | `revoked: boolean` | `graph-write` enabled; `admin` human or trusted local stdio |

`WorkspaceView` is `{ workspaceId: string, name: string, visibility: "private" | "public", role: "owner" | "writer" | "reader" | "public", isDefault: boolean }`. `get_workspace` can add `ownerId: string` only for the owner. `list_workspaces` sorts by immutable workspace ID and caps `limit` at 100; the default is 100. `nextCursor` is opaque and is `null` after the last result. A malformed cursor is an input error. The default is a stored setting for each stable identity. A per-call `workspaceId` never changes it.

The first workspace a caller creates becomes their default if they do not have one. A grant never changes a default. If a grant or machine account is revoked, remove its affected default in the same registry transaction. If a public graph becomes private, clear defaults for callers without a grant in the same transaction. Do not select another workspace automatically. A newly authenticated user without a default must select one explicitly or set a default through MCP.

Graph management tools appear in `tools/list` only when the caller has the tool-category scope. Machine account tools also need the `graph-write` category and an admin human or trusted local stdio. A tool call still checks ownership and grants. Neither a tool list nor a graph list promises future access after revocation.

## HTTP and viewer contract

For workspace-enabled HTTP, refuse startup when both OAuth and a bearer credential are absent. The error must name the two accepted authentication options. Local stdio keeps its local machine identity. This breaks the open request path at `src/http.rs:416-418`, which now admits requests as `static`.

The viewer has a labeled workspace dropdown in its toolbar. It reads the workspace list with the current bearer credential, follows list cursors, and shows every accessible workspace. It starts with the saved default. If there is no default, it shows no graph until the caller selects one. Selecting an item changes only this viewer session; it never calls `set_default_workspace`.

Every `/ui/graph`, `/ui/search`, `/ui/node`, and `/ui/expand` request includes the selected `workspaceId`. The server checks the viewer's `graph-read` scope and workspace read access for each request. A switch clears the graph, inspector, search, type filter, pagination, and prior error state. It also aborts old requests. A request generation check discards a late response even when cancellation cannot stop its delivery. A revoked selection returns an error and clears the viewer; the viewer refreshes its list.

Existing admin webhook routes must resolve a workspace and require both their current admin gate and ownership of that workspace. The MCP subscription tools use ownership without the admin UI gate. The viewer and MCP selectors use the same registry authorization function. A graph page does not combine nodes across files, even if they have the same name.

## Errors and transactional rules

- An unknown workspace ID and an inaccessible private workspace ID return the same not-found result. Do not return its name or owner.
- A call without `workspaceId` and without a valid default returns a distinct `workspace selection required` error. It does not pick a public workspace.
- A known public workspace with no write grant returns an access error on writes. A missing tool scope keeps its current insufficient-scope error.
- A malformed UUID, unknown grant target, invalid role, and invalid list cursor return input errors.
- Workspace create records an owner and a usable graph together from the caller's view. Registry failure must not publish a graph file.
- A revoked credential or graph grant blocks the next request. Worker delivery can continue only for subscriptions managed by the current owner within that workspace.

Do not add a cross-workspace relation API. `export_graph`, relation traversal, text search, semantic search, hybrid search, graph statistics, FTS, and vector caches use only the selected graph file. A code-project repository is not a graph workspace and remains outside this selector.

## Migration and rollout

A new registry binds the existing memory file to one legacy workspace ID. The operator must provide `legacy-owner-id` under `[workspaces]` for the first workspace migration. The value must be an existing, stable human or machine ID. Startup refuses the change if the value is absent or invalid. The owner gets the initial default; no other legacy principal receives a private grant. The operator can change grants through MCP after startup. Subsequent starts read the saved registry mapping; they do not infer or replace the owner.

No entity, relation, index, event, or webhook row moves from the existing file. Keep all existing SQLite migration files and checksums unchanged. Add migration 14 as a workspace version marker in every graph file. Apply the marker to the legacy graph before the new registry becomes active. The old migration runner rejects a newer schema (`crates/mcpmem-core/src/events.rs:167-170`). A failed registry setup may leave a marked file, but the old binary cannot serve it. The new binary can retry registry setup. Apply the marker to a new graph file before its registry entry becomes active. Back up the graph files and registry as one set. Rollback needs a workspace-aware binary; do not start a pre-workspace binary on marked files. The deployment notice must name OAuth re-login and the new HTTP credential prerequisite.
Stop every pre-workspace server process before the first marker migration. A process that already has an open graph handle cannot know about the new registry. Start only the workspace-aware binary after the marker and registry pass their checks.

Before deployment, run a read-only count on the actual existing database and record the result. The command is the same in bash and fish once `DB` is set:

```bash
# bash
DB=/path/from/server/config
sqlite3 -readonly "$DB" 'SELECT COUNT(*) AS live_entities FROM entity WHERE flags = 0;'
```

```fish
# fish
set DB /path/from/server/config
sqlite3 -readonly "$DB" 'SELECT COUNT(*) AS live_entities FROM entity WHERE flags = 0;'
```

A zero count means this check cannot prove preservation of live legacy data. Confirm the configured owner against the registered principal IDs before the first start. Do not infer ownership from the count or from a display name.

## Acceptance map

| ID | Requirement | Decisive check |
|---|---|---|
| W-01 | A caller creates a private or public workspace and owns it. | Create both types; inspect owner and separate graph files. |
| W-02 | Graph data and derived state do not cross workspace files. | Use the same node names in two graphs; compare graph, FTS, vector, events, and exports. |
| W-03 | List shows owned, granted, and public graphs without private metadata leaks. | Compare pages for the owner, reader, writer, unrelated user, and anonymous caller. |
| W-04 | A private grant has reader or writer access. | Test reads, writes, grant replacement, and revocation on the next call. |
| W-05 | Only the owner changes grants and visibility. | Test both owner success and writer refusal; test public to private transition. |
| W-06 | Each stable identity stores its own default. | Set two defaults; check omitted and explicit `workspaceId` calls. |
| W-07 | A missing or revoked default never falls back. | Revoke access and verify selection error before a new default is set. |
| W-08 | Machine credentials have separate identities and rights. | Create two machine accounts; grant one; revoke its credential. |
| W-09 | OAuth identity stays stable across a name change. | Change the principal name and verify ownership and grants by issuer and subject. |
| W-10 | Every MCP graph, vector, and subscription path checks the selected workspace. | Exercise every registered tool through the same resolver. |
| W-11 | The viewer switches among accessible graphs without changing the default. | Switch while graph, node, search, and expand requests are in flight. |
| W-12 | The viewer clears inaccessible graphs after revocation. | Revoke during a session; fetch again and inspect list and canvas. |
| W-13 | Background index and webhook workers retain graph isolation. | Publish jobs and subscriptions in both files; verify each output source. |
| W-14 | Legacy data stays in place with an explicit owner. | Start from a non-empty historical fixture; compare counts and content. |
| W-15 | Open HTTP cannot serve graph data after workspaces start. | Start without OAuth and bearer; assert startup refusal and no data response. |
| W-16 | Old OAuth grants cannot become another user's workspace identity. | Present a legacy name-based grant; require new authentication. |
| W-17 | A pre-workspace binary cannot serve a workspace graph. | Apply migration 14 to a populated file; start the old binary and assert schema refusal. |

Run unit tests, integration tests, the repository's full CI pre-flight, and the changed MCP and HTTP paths with real credentials. Before a release, run the non-empty precondition against the actual deployment database. The README describes creation, grants, public reads, defaults, override, UI selection, migration, and the open-HTTP break in the same feature change.

## Alternatives rejected

- **One SQLite database with a workspace column:** A missed predicate in FTS, vector, traversal, export, or worker code can cross the privacy boundary.
- **Use existing OAuth names for owners:** The stored grant name can change and is not a stable issuer and subject key.
- **Treat public as anonymous:** The owner selected authenticated public reads. Open HTTP must not get a machine identity without a credential.
- **Change default when the viewer switches:** The owner selected a session-only viewer switch. A persistent default has a separate MCP tool.
