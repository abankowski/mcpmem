# Isolated Knowledge Workspaces Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use subagent-driven-development to implement this plan task by task. Each task uses checkbox syntax for status.

**Goal:** Deliver separate knowledge graphs with MCP ownership, grants, defaults, and a graph selector in `/ui`.

**Architecture:** Keep one graph file per workspace. Use one SQLite registry for metadata, access checks, and machine credentials. Resolve the workspace at every MCP and HTTP boundary. Give each worker one graph file per poll.

**Tech Stack:** Rust, rusqlite, axum, MCP JSON-RPC, vanilla JavaScript, SQLite FTS5, Cargo tests.

**Spec:** `docs/superpowers/specs/2026-10-01-workspaces-design.md`

## Global Constraints

- Use the spec as the authority. Preserve all existing numbered migrations and their checksums.
- Apply migration 14 to every graph file. Stop the old server before migration. An old binary must reject a marked file.
- Keep the existing graph data in its file. Require `[workspaces] legacy-owner-id` only for the first migration.
- Use `human:<base64url(issuer NUL subject)>`, `machine:local`, `machine:static`, and `machine:<uuid>` as stable IDs.
- Treat public graphs as readable only to authenticated identities. Writers have read and write access. Only owners change grants, visibility, and webhook subscriptions.
- Keep active legacy webhook subscriptions live after migration. Their manager is unknown. Report the endpoint list to the new owner.
- Add optional `workspaceId` to all graph, vector, and webhook MCP tool inputs. Omission uses a valid stored default; an explicit ID never changes that default.
- Use camel-case JSON fields. Do not add an implicit graph fallback. Do not widen the existing tool-category gates.
- Show all accessible graphs in the `/ui` dropdown. A switch changes only the current viewer session and discards stale responses.
- Do not change code-project database selection. Update README and CHANGES.md with the shipped feature.
- Follow test-first red-green cycles for each behavior. The controller owns all git commands. Subagents do not run git, tests, builds, lint, or formatters while other agents edit.

## File and task graph

| Task | Owns the files and interfaces | Depends on |
|---|---|---|
| 1 | `src/workspace.rs`, `src/server.rs` startup, `src/lib.rs`, `src/config.rs`, `src/config_file.rs`, `crates/mcpmem-core/src/events.rs`, migration 14, `tests/workspace_registry.rs`, `tests/event_outbox.rs`, `tests/config_file.rs` | Approved spec |
| 2 | `src/principals.rs`, `src/oauth_routes.rs`, `src/authz.rs`, OAuth crate grant code, `src/workspace.rs` identity methods, `tests/oauth_flow.rs`, `tests/principal_admin.rs`, `tests/workspace_identity.rs` | Task 1 registry |
| 3 | `src/server.rs` tool dispatch, `src/tools.rs`, `tools.json`, `vector_tools.json`, `webhooks_tools.json`, `src/workspace.rs` graph cache, `tests/workspace_mcp.rs`, `tests/scope_gating.rs` | Tasks 1 and 2 |
| 4 | `src/actions/webhooks.rs`, `src/runtime.rs`, `src/main.rs`, `src/server.rs` runtime assembly, `tests/workspace_workers.rs`, `tests/webhook_tools.rs`, `tests/role_composition.rs` | Task 3 selection |
| 5 | `src/http.rs`, `src/server.rs` HTTP dispatcher entry, `tests/workspace_http.rs`, `tests/ui_http.rs`, `tests/vector_http.rs`, `tests/webhook_admin.rs` | Tasks 3 and 4 |
| 6 | `src/ui/index.html`, `src/ui/graph.js`, `src/ui/graph.css`, `src/ui/admin.html`, `src/ui/admin.js`, viewer integration tests in `tests/workspace_http.rs` | Task 5 HTTP contract |
| 7 | `README.md`, `CHANGES.md`, affected existing fixtures and contract tests | Tasks 1–6 |

Tasks 1–5 share live Rust interfaces. Run them in order with one integration owner. Task 6 can run in a separate worktree after Task 5 fixes the HTTP shape; do not share an edit tree with another code editor. The controller integrates and reviews each task before the next dependent task starts. Task 7 owns the full pre-flight and the final end-to-end smoke check.

### Task 1: Registry, legacy owner, and downgrade guard

**Files:** Create `src/workspace.rs`, `crates/mcpmem-core/migrations/0014_workspace_marker.sql`, `tests/workspace_registry.rs`. Modify `src/server.rs` startup, `src/lib.rs`, `src/config.rs`, `src/config_file.rs`, `crates/mcpmem-core/src/events.rs`, `tests/event_outbox.rs`, `tests/config_file.rs`.

**Interfaces:** Export `WorkspaceRegistry`, `WorkspaceRecord`, `WorkspaceView`, `WorkspacePage`, `WorkspaceError`, and `WorkspaceAccess` from `src/workspace.rs`. Expose `open(memory_path: &Path, legacy_owner: Option<&str>) -> Result<Self, WorkspaceError>`, `create(principal_id: &str, name: &str, visibility: Visibility, init: impl FnOnce(&Path) -> Result<(), WorkspaceError>) -> Result<WorkspaceView, WorkspaceError>`, `resolve(principal_id: &str, requested: Option<&str>, access: WorkspaceAccess) -> Result<WorkspaceRecord, WorkspaceError>`, `list(principal_id: &str, cursor: Option<&str>, limit: usize) -> Result<WorkspacePage, WorkspaceError>`, `all_paths() -> Result<Vec<(String, PathBuf)>, WorkspaceError>`, `set_default`, `grant`, `revoke`, `set_visibility`, and `grants`. Store registered graph paths; never form a graph path from an input ID.

- [ ] **Step 1: Add failing registry and startup tests.** Build a non-empty version-13 database in `tests/event_outbox.rs`. Do not open it through the new `GraphHandle::new` before owner validation. Assert that a normal `MCPServer::new` call with no owner rejects startup without migration. Check that a missing registered graph refuses startup. Remove a saved workspace owner and check startup refusal. Test uppercase UUIDs for grants and visibility. Verify entity preservation and isolated same-name rows. A real version-13 binary must reject the marker before release.

```sql
-- The historical fixture inserts this row before the workspace migration.
INSERT INTO entity(id, name_hash, name, type_id, created_us, updated_us)
VALUES(1, 0, 'same-name', 1, 1, 1);
-- After registry bootstrap, this count stays one in the legacy file.
SELECT COUNT(*) FROM entity WHERE flags = 0;
```

- [ ] **Step 2: Run the focused tests before code.** Run `cargo test --test workspace_registry -- --test-threads=1` and the migration test in `event_outbox`. The new behavior must fail because the registry and marker do not exist. A compile error from an absent new module is an acceptable first red; a fixture setup error is not.
- [ ] **Step 3: Add the marker and registry.** Validate the configured owner before any call to `GraphHandle::new` or core migration 14. `MCPServer::new` must call `WorkspaceRegistry::open_with_principals` first. Migration 14 revokes existing OAuth tokens and codes and expires old login rows. It contains no graph table rewrite. The registry creates the five tables in the spec. Reject a missing registered graph. Validate every saved owner on restart. Canonicalize UUIDs before ACL updates. Use SQL transactions for ACL and defaults.

```sql
UPDATE oauth_token SET revoked = 1 WHERE revoked = 0;
UPDATE oauth_code SET spent = 1 WHERE spent = 0;
DELETE FROM oauth_login;
```

- [ ] **Step 4: Wire configuration and verify.** Add `[workspaces] legacy-owner-id` to strict TOML parsing. Add an optional `Config` field; do not require it after a valid registry exists. Run the focused registry, migration, and config tests. Check a real prior binary against a marked fixture before the feature ships.
- [ ] **Step 5: Review and commit the task.** Record the test and migration results in the ledger. The controller commits exact paths.

### Task 2: Stable human and machine identities

**Files:** Modify `src/principals.rs`, `src/oauth_routes.rs`, `src/authz.rs`, and `src/workspace.rs`. Modify identity and deletion paths in `src/http.rs`. Modify HTTP state wiring in `src/server.rs`. Modify `crates/mcpmem-oauth/src/store.rs`. Modify `tests/oauth_flow.rs`, `tests/principal_admin.rs`, `tests/oauth_upstream.rs`, `tests/oauth_consent.rs`, `tests/oauth_discovery.rs`, and `tests/support/flow.rs`. Create `tests/workspace_identity.rs`. Task 5 owns the HTTP viewer routes.

**Interfaces:** Export `human_id(iss: &str, sub: &str) -> String` and `registered_human(id: &str) -> Result<bool, WorkspaceError>` from a shared principal resolver. Preserve built-in precedence over runtime principals. Keep `Principal.id` as the stable ID. Registry methods `create_machine(name, scopes) -> Result<(String, String), WorkspaceError>`, `list_machines() -> Result<Vec<MachineView>, WorkspaceError>`, `revoke_machine(id) -> Result<bool, WorkspaceError>`, and `authenticate_machine(token) -> Result<Option<Principal>, WorkspaceError>` use random 256-bit credentials and a stored digest. Do not expose a digest to MCP.

- [ ] **Step 1: Write red identity tests.** A human keeps workspace ownership after a display name change. A token issued before migration no longer authenticates. Two created machine credentials resolve to different IDs and defaults. A revoked machine token fails on its next request. Admin or local stdio can manage machines; static bearer and non-admin human cannot. Removing a workspace owner fails.

```rust
let id = human_id("https://issuer.test", "subject-1");
assert!(id.starts_with("human:"));
registry.grant(&owner, &workspace_id, &id, Role::Reader)?;
rename_runtime_principal("https://issuer.test", "subject-1", "New name")?;
assert!(registry.resolve(&id, Some(&workspace_id), WorkspaceAccess::Read).is_ok());
```

- [ ] **Step 2: Run the focused identity and OAuth tests.** Confirm that the new identity tests fail for the expected absent stable-ID behavior.
- [ ] **Step 3: Extract the shared human resolver.** Decode the existing OAuth principal ID after `human:`. Consult file-backed principals before runtime rows. Record the stable ID on new login, code, and token records. Update revocation by stable ID. Bind each issued machine bearer to its own scopes; do not let machines obtain `admin`. Hold the registry write transaction through the owner check, token revocation, runtime principal deletion, and access cleanup. Recheck human registration inside create, grant, and default mutation transactions. Test both create/delete orders with `cargo test --features oauth --test principal_admin runtime_human_owner -- --test-threads=1` (the same command in bash and fish). Make the HTTP bearer resolver authenticate machine credentials.
- [ ] **Step 4: Run focused identity, OAuth, principal-admin and scope tests.** Run them serially, because the existing OAuth fixture uses shared state.
- [ ] **Step 5: Controller reviews the task diff and commits exact paths.** Record rejected name-based lookup as the alternative in the commit message.

### Task 3: MCP tools, grants, selection, and graph cache

**Files:** Modify `src/server.rs`, `src/tools.rs`, and `src/workspace.rs`. Modify the MCP dispatch path in `src/http.rs`. Modify the selected-path handlers in `src/actions/webhooks.rs`. Modify `tools.json`, `vector_tools.json`, `webhooks_tools.json`, `tests/scope_gating.rs`, and `tests/webhook_tools.rs`. Create `tests/workspace_mcp.rs`. Task 4 owns the separate webhook worker changes in `src/actions/webhooks.rs`.

**Interfaces:** `WorkspaceRegistry::resolve` is the single ACL check. Add a bounded `WorkspaceHandles` cache keyed by ID. Each cache entry holds `Arc<GraphHandle>` and optional `Arc<VectorStore>` for the registered path. Use the existing `GraphHandle::new` and `VectorStore::with_config`. Add the MCP tool names and exact JSON fields from the spec. Preserve `tools/list` scope filtering and HTTP JSON-RPC batch pre-screen. A batch can contain different explicit workspace IDs, but each call resolves one workspace before its handler runs.

- [ ] **Step 1: Write red MCP tests.** Create two graphs with an entity called `same-name`. Assert that omitted `workspaceId` uses each caller's saved default. Assert that an explicit ID selects only that graph and does not change the default. Assert reader denial on write, writer success, public read without write, private list non-disclosure, owner-only changes, and no-default failure. Call `export_graph`, vector search, FTS, and relation traversal in the same test fixture. Assert that a batch can read two explicit workspace IDs without a cross-graph result. Check that a denied call never executes its mutation.

```rust
let first = call_as(&owner, "create_workspace", json!({"name":"One","visibility":"private"}));
let second = call_as(&owner, "create_workspace", json!({"name":"Two","visibility":"private"}));
call_as(&owner, "create_entities", json!({"workspaceId":first.id,"entities":[entity("same-name","One")]}));
call_as(&owner, "create_entities", json!({"workspaceId":second.id,"entities":[entity("same-name","Two")]}));
assert_eq!(read_name_type(&owner, &first.id, "same-name"), "One");
assert_eq!(read_name_type(&owner, &second.id, "same-name"), "Two");
```

- [ ] **Step 2: Run `cargo test --test workspace_mcp -- --test-threads=1` before code.** Confirm that workspace management tools and `workspaceId` selection fail for the expected reason.
- [ ] **Step 3: Implement tool registration and dispatcher selection.** Add an optional `workspaceId` property to every graph, vector, and webhook manifest schema. Add management tools to `tools.json` and `src/tools.rs`. Resolve access before graph handlers, vector caches, and export. Send machine administration tools through the existing graph-write category plus a separate admin-or-local check. Keep code tools outside selection. Use one shared `WorkspaceHandles` cache rather than a second global graph handle.
- [ ] **Step 4: Run focused MCP and scope tests.** Confirm schemas, list access, grant changes, overrides, and defaults. Run vector and graph contract tests.
- [ ] **Step 5: Controller reviews the task diff and commits exact paths.** Note each existing tool category in the review package.

### Task 4: Per-workspace indexer and webhook workers

**Files:** Modify `src/runtime.rs`, `src/main.rs`, `src/server.rs`, `src/actions/webhooks.rs`, `tests/webhook_tools.rs`, `tests/role_composition.rs`. Create `tests/workspace_workers.rs`.

**Interfaces:** `WorkspaceRegistry::all_paths` supplies trusted worker paths. The indexer and webhook worker constructors still take one graph path. The runtime scheduler holds a cursor and polls at most one registered graph per service turn. The vector publisher receives the matching graph's vector handle. MCP webhook handlers receive the selected graph path, not the process-global `SubscriptionDb`.

- [ ] **Step 1: Write red multi-file worker tests.** Add index jobs and subscriptions to two registered graph files. Poll both roles. Assert that each vector snapshot contains only that graph's owner IDs and each delivery carries that graph's event. Add a legacy active subscription and assert it still delivers after migration. Assert that a writer cannot register a subscription.

```rust
let paths = registry.all_paths()?;
assert_eq!(paths.len(), 2);
run_indexer_turn(&paths[0])?;
run_indexer_turn(&paths[1])?;
assert_ne!(snapshot_owner_names(&paths[0])?, snapshot_owner_names(&paths[1])?);
```

- [ ] **Step 2: Run focused worker and webhook tests before code.** Confirm that the second graph is not processed with the current one-path runtime.
- [ ] **Step 3: Route one graph per poll.** Let the scheduler enumerate trusted graph files and advance a bounded cursor. Reuse existing single-file worker logic. Replace the global webhook subscription path with the selected workspace path. Keep each graph's subscription outbox separate. Do not change existing webhook records during migration.
- [ ] **Step 4: Run worker, role, webhook tool, and webhook outbox tests.** Verify both default and feature-gated builds.
- [ ] **Step 5: Controller reviews the task diff and commits exact paths.** Inspect the scheduling bound and the lack of a cross-file worker query.

### Task 5: Authenticated HTTP viewer and administration

**Files:** Modify `src/http.rs`, `src/server.rs`, `tests/ui_http.rs`, `tests/vector_http.rs`, `tests/webhook_admin.rs`. Create `tests/workspace_http.rs`.

**Interfaces:** Add `GET /ui/workspaces?cursor=&limit=` with the same `WorkspacePage` data as MCP listing. The existing viewer data routes accept an explicit `workspaceId` query field. The HTTP MCP dispatcher uses the authenticated principal from `principal_of` and the registry resolver. Admin webhook routes need both `admin` and selected workspace ownership. Reject workspace-enabled HTTP startup without OAuth or a configured bearer credential.

- [ ] **Step 1: Write red HTTP tests.** An unauthenticated HTTP startup fails with a message that names OAuth or bearer. An authenticated public graph appears in `/ui/workspaces`. A private graph stays absent from an unrelated list. A viewer read on an inaccessible graph returns not found. Revoking a grant changes the next viewer response. Admin without ownership cannot edit another graph's webhook settings.

```rust
let response = get_as(&reader, "/ui/graph?workspaceId=<private-id>").await;
assert_eq!(response.status(), StatusCode::NOT_FOUND);
let page = get_as(&reader, "/ui/workspaces?limit=100").await;
assert!(!ids(page).contains(&private_id));
```

- [ ] **Step 2: Run focused HTTP tests before code.** Confirm that the old routes select the wrong graph or lack a graph list.
- [ ] **Step 3: Change HTTP state and routes.** Store the registry and handle cache in HTTP state. Resolve one graph for each viewer request after category scope validation. Pass the selected graph to the existing payload builders. Return one not-found shape for unknown and inaccessible private IDs. Keep the current admin gate in addition to owner authorization. Task 2 already refused open HTTP startup; verify that rule instead of adding a second guard.
- [ ] **Step 4: Run HTTP, OAuth, viewer, vector, and webhook admin tests.** Replace open-HTTP success expectations with the approved credential requirement. Do not weaken the existing scope gate.
- [ ] **Step 5: Controller reviews the task diff and commits exact paths.** Record the HTTP compatibility break in the commit message.

### Task 6: `/ui` dropdown and request generation

**Files:** Modify `src/ui/index.html`, `src/ui/graph.js`, `src/ui/graph.css`, `src/ui/admin.html`, `src/ui/admin.js`, and `tests/workspace_http.rs`.

**Interfaces:** The viewer reads `/ui/workspaces` with the current bearer. Its `workspaceId` state initializes from the result with `isDefault: true`. The dropdown emits explicit IDs on `/ui/graph`, `/ui/search`, `/ui/node`, and `/ui/expand`. It does not call `set_default_workspace`. Fetch each cursor page until `nextCursor` is `null`. The separate admin page accepts an owner workspace ID for its webhook controls and sends it in every webhook request. A blank admin ID uses the saved default. The admin token requests only `admin`, so the page must not depend on the `graph-read`-gated `/ui/workspaces` list.

- [ ] **Step 1: Write a red viewer scenario.** Run the actual `/ui` page against a two-workspace server. Verify that the dropdown has both accessible graph names. Switch while a node or search request is in flight. Assert that the canvas and inspector show no data from the first graph after the switch. Check that an MCP call without an ID still uses the saved default.

```js
const selectedWorkspace = { id: null, generation: 0 };
function selectWorkspace(id) {
  selectedWorkspace.id = id;
  selectedWorkspace.generation += 1;
  resetGraphView();
  load();
}
```

- [ ] **Step 2: Observe the red viewer scenario before code.** The old toolbar has no selector and the old requests have no `workspaceId`.
- [ ] **Step 3: Add the labeled dropdown and safe state transition.** Place it before the label filter. Populate the options from paginated `/ui/workspaces`. On switch, clear nodes, links, inspector, query, filters, and pager. Abort active fetches and compare the generation after each `await` before any state update. Give a revoked selection a visible error and refresh the dropdown.
- [ ] **Step 4: Keep webhook administration usable.** Add a labeled workspace ID field to the admin webhook section. If the caller enters an ID, append it to every webhook request. Show selection and access errors beside the field; do not hide the webhook section on those errors. Keep the field independent from the viewer session.
- [ ] **Step 5: Run browser scenarios and viewer tests.** Confirm session-only viewer selection and both graph names. Reject stale results and revoked access. Confirm that the admin page can use an explicit owner ID with an admin-only token and reports a missing default without hiding its controls.
- [ ] **Step 6: Controller reviews the UI diff and commits exact paths.** Include a screenshot or concrete browser observation in the report.

### Task 7: Documentation, full verification, and release review

**Files:** Modify `README.md`, `CHANGES.md`, and any broken tests in the earlier task scopes. Do not change unrelated documents or untracked user files.

**Interfaces:** The README gives MCP examples for workspace creation, grants, lists, defaults, overrides, and the `/ui` dropdown. It states that public graphs need authentication. It states that legacy webhooks remain active. It describes the stable owner ID, OAuth re-login, and the open HTTP break.

- [ ] **Step 1: Add user docs and checks.** Show the legacy count and endpoint list in bash and fish. If a command is identical, say so. Link the spec. Do not call an unverified deployment number a measurement.
- [ ] **Step 2: Prove the boundary in a local smoke run.** Start the HTTP server with a real bearer credential. Create two workspaces with the same node name through MCP. Read each graph and visit `/ui`; switch the dropdown and observe distinct graph content. Revoke a grant and observe refusal. Keep the temporary database outside the repo.
- [ ] **Step 3: Run the repository's exact CI pre-flight.** Use `.github/workflows/ci.yml` and `.omp/AGENTS.md` for the Cargo matrix. Run the single-worker and feature combinations. Use `OMP_PREFLIGHT_CMD` with the project command and write the marker only after a full successful run on the exact commit. Do not replace the aggregate command with a few convenient checks.
- [ ] **Step 4: Review and deliver.** Map every W-01 through W-17 acceptance row to test output or the smoke transcript. Run one independent reviewer for each repo rule dimension. Fix findings and verify them. Push a draft PR, run pre-flight on that exact commit, then mark it ready. Reply to all review comments. Never merge.
- [ ] **Step 5: Controller commits exact docs and test paths.** State token and cost estimates honestly in the commit message. Record any failed hypothesis and the evidence that ruled it out.

## Self-review map

| Spec requirement | Task |
|---|---|
| W-01, W-03, W-04, W-05, W-06, W-07 | 1 and 3 |
| W-02, W-10 | 1 and 3 |
| W-08, W-09, W-16 | 2 and 3 |
| W-11, W-12 | 5 and 6 |
| W-13 | 4 |
| W-14, W-17 | 1 |
| W-15 | 5 |
| README, release note, smoke and CI | 7 |

Every implementation task begins with a failing consumer-visible check. Tests use real graph files and real token paths. A test that passes on the old behavior does not prove a workspace boundary.
