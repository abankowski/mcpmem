# Optional browser UI rebuild - design

**Status:** Design direction approved on 2026-10-04. The owner must review this written contract before an implementation plan starts.

## Goal and scope

Replace the existing graph and admin browser pages with the six handoff views in `designs/mcpmem-ui-handoff/README.md`. Serve Graph, Search, and Admin at `/ui`. Give the UI an optional build feature and a runtime switch. Keep the OAuth consent page available to external clients when the UI is off. The public landing page is not in this work.

The handoff HTML and renders define layout, copy, colors, type, and responsive behavior. The server and its approved contracts define file types, consent, permissions, and available operations. This design records each difference below. The old browser assets and old JSON URLs do not stay as aliases.

## Decisions and alternatives

1. **One binary.** Build React, TypeScript, Tailwind, shadcn/ui, and lucide assets before the Rust package step. Embed the output in a Rust `ui` module in the root crate. A separate frontend service adds an auth and deploy boundary. Files beside the binary add an asset version and integrity boundary. Neither alternative is needed here.
2. **Optional module.** Add a root Cargo feature `ui` to the default feature set. Gate the module, assets, routes, and UI-only Rust dependencies on it. Add `[server] ui = true|false` and a value-style `--ui true|false` option. The CLI value wins over the file. The runtime default is true when the feature exists. An explicit request for true without the compiled feature fails at startup. The `--no-default-features` binary has no UI module or `/ui/*` routes.
3. **Clean routes.** The browser uses `/ui` for Graph, `/ui/search` for Search, and `/ui/admin/*` for Admin. All browser data adapters use `/ui/api/*`. No old `/ui/graph`, `/ui/node`, `/ui/workspaces`, or `/ui/attachments` JSON route remains. Keep existing OAuth paths and exact registered callbacks. Keep `/mcp` and the existing `/` MCP route. The current `/ui/search` JSON route cannot also serve a page; see `src/http.rs:273-326`.
4. **Disable boundary.** When `ui` is not compiled or `[server] ui = false`, register no `/ui` or `/ui/*` route. This includes attachments and admin HTTP adapters. MCP tools and external-client OAuth remain available. OAuth consent is part of the OAuth server, not the optional browser module. Do not seed new reserved browser OAuth clients while the UI is off. Do not remove existing client rows when the switch changes.
5. **Workspace authority.** List only the caller's accessible workspaces. Only the owner may change a workspace's grants or visibility. `admin` does not grant workspace ownership. The Admin Workspaces screen shows accessible workspaces, not every private workspace.
6. **File and consent rules.** Accept only the configured supported text and PDF MIME types in this work. Show "Other file types coming soon" as information, not as an enabled action. Consent remains a choice of requested scopes, not a choice of workspace.

The route collision is the reason for decision 3. Keeping JSON at `/ui/search` would force browser pages under another prefix. That would keep two route conventions instead of the requested clean cutover. The owner accepted the API break. The alternative of gating all HTTP routes was rejected because it would remove MCP and external-client OAuth.

## Boundaries and ownership

| Unit | Owns | Depends on |
|---|---|---|
| UI build | Source under `ui/`, lockfile, built asset list | Node toolchain at build time |
| Rust `ui` module | Assets, browser pages, `/ui/api/*` routes | Existing HTTP state and service operations |
| Shared HTTP transport | `/mcp`, `/`, auth resolution, limits, response layers | Existing runtime and OAuth setup |
| OAuth module | Discovery, client registration, authorization, consent, token, revoke | `mcpmem-oauth`; not the UI feature |
| Graph service | Workspace resolution, graph reads and writes | Existing workspace registry and graph handle |
| Search service | Direct, semantic, and hybrid results | Existing FTS and optional vector profile |
| React app | Page state, forms, canvas, and local preferences | Typed `/ui/api/*` responses |

Do not create a new Rust crate. The present HTTP state and handlers live in the root crate. A UI crate would need a new common state contract and another release unit without an independent consumer. Keep auth and database rules on the server. The browser only hides actions after it reads server capabilities; hiding a button never grants access.

## Route and data contracts

Use camelCase JSON. Every graph request names `workspaceId` explicitly after selection. An omitted workspace ID remains valid only where the existing saved-default rule already permits it. A workspace switch does not change that saved default. All reads and writes resolve access against the selected graph. Requests use `Authorization: Bearer <token>`; no token goes into a URL query. The existing static-token fragment handoff remains supported and is removed from the address bar after capture.

Keep the registered graph and admin PKCE clients separate. The graph client requests `graph-read` first, then `graph-write`, `vectors`, or `attachments` when an action needs them. The admin client requests `admin` for server actions. A token with only `admin` does not grant graph or workspace access. A static bearer can use its configured graph scopes but cannot become a human-admin token. Build asset, API, and OAuth links from the same public base so a path-prefixed public URL does not break browser navigation.

| Browser route | Method and data | Access and result |
|---|---|---|
| `/ui` | GET Graph page; OAuth graph callback returns here | Static shell; no graph data in HTML |
| `/ui/search` | GET Search page; `q`, `mode`, `scope`, `type`, `k` are URL state | Static shell; no search data in HTML |
| `/ui/admin/*` | GET Admin page and existing `/ui/admin/callback` | Static shell; the admin token is separate from a graph token |
| `/ui/assets/*` | GET immutable bundled files from a checked asset list | Correct MIME and cache rules; unknown names return 404 |
| `/ui/api/session` | GET; optional `workspaceId` | `{scopes:string[],principalName:string|null,workspaceRole:"owner"|"writer"|"reader"|"public"|null,features:{vectors:boolean,attachments:boolean,code:boolean,webhooks:boolean}}`; server grants still control each action |
| `/ui/api/workspaces` | GET `cursor?`, `limit?`; POST `{name}` | GET `{workspaces:[{workspaceId,name,visibility,role,isDefault}],nextCursor}`. Creation needs graph-write and creates an owned workspace. Switch is local state only. |
| `/ui/api/workspaces/{id}` | GET; PATCH `{visibility}` | GET returns only authorized metadata. PATCH needs workspace owner and graph-write. No global workspace list. |
| `/ui/api/workspaces/{id}/grants` | GET; POST `{principalId,role}` | GET `{grants:[{principalId,role}]}`; POST accepts reader or writer. Both need the owner. DELETE `/ui/api/workspaces/{id}/grants/{principalId}` revokes an access grant. |
| `/ui/api/graph` | GET `workspaceId,entityType?,offset?,limit?` | Existing `{entities,relations,entityTypes,stats,page}` shape. Keep the current page cap of 1000. A page has edges only when both nodes are in it. |
| `/ui/api/node` | GET `workspaceId,name` | Name, type, observations with `observationId`, attributes, degree, and all incident relation triples. Do not infer adjacency from a graph page. |
| `/ui/api/relation` | GET `workspaceId,from,to,relationType` | One exact relation triple, its observations with `observationId`, and its attributes. The same triple addresses every relation write. |
| `/ui/api/expand` | GET `workspaceId,name,depth,direction` | `{entities,relations}`. Depth is 1 to 3; direction is `both`, `outgoing`, or `incoming`. |
| `/ui/api/types` | GET `workspaceId` | `{entities:[{type,count,desc?}],relations:[{type,count,desc?}]}` from current workspace data. |
| `/ui/api/search` | GET `workspaceId,q,mode,scope,type?,from?,to?,relationType?,k?` | `{results:[SearchHit],count,elapsedMs}`. Direct uses entity or relation FTS. Semantic embeds the query on the server. Hybrid uses a server-side embedding plus fusion. `k` is 10, 20, or 50. Relation filters apply before ranking and the `k` limit. |
| `/ui/api/attachments` | GET `workspaceId,entityName`; POST raw body with `workspaceId,entityName,filename` in the query and MIME in `Content-Type` | Existing attachment metadata and upload response. Keep the configured byte cap, workspace budget, and MIME checks. No multipart adapter. |
| `/ui/api/attachments/{id}` | GET or DELETE with `workspaceId`; GET also supports `/pages?page&offset&maxChars` and `/download` | Metadata, page text, or raw download. Read needs attachments scope and workspace read. Delete needs attachments scope and workspace write. |
| `/ui/api/principals`, `/ui/api/waitlist`, `/ui/api/webhooks`, `/ui/api/repos` | Keep the existing method and payload contracts under these paths | Keep the human-admin gate, feature gates, webhook workspace-owner gate, and repository server scope. |
| `/ui/api/vectors/stats` | GET `workspaceId` | Return only measured profile state and counts. Require vectors scope and a valid profile. No false last-refresh or model value. |

`SearchHit` is a tagged union. A node hit has `{kind:"entity",name,entityType,snippet?,score?}`. A relation hit has `{kind:"relation",from,to,relationType,snippet?,score?}`. A file hit has `{kind:"attachment",attachmentId,entityName,filename,page,excerpt,score}`. The server returns file hits only with both vectors and attachments consent. Direct search covers node and relation FTS. A file hit appears in semantic or hybrid results only when the present search index returns one. Do not invent a direct file search result or a common score for FTS and vectors.

Semantic and hybrid relation hits must carry a structured triple. Resolve it from the relation ID in the selected graph. Do not parse the formatted vector result name; see `src/vector_store.rs:1278-1292`. Apply `from`, `to`, and `relationType` filters to relation candidates before ranking. The current vector path filters only by kind and type (`src/vector_actions.rs:483-505`), so this needs a shared query change.

Use one browser mutation adapter at `POST /ui/api/mutations`. Each request has `{workspaceId,operation,payload}`. The operation and payload form a tagged union:

| Operation | Payload |
|---|---|
| `createEntity` | `{name,entityType,observations:[{body,occurredAtUs?}],attributes?}` |
| `renameEntity` | `{oldName,newName}` |
| `mergeEntities` | `{source,target}` |
| `deleteEntity` | `{name}` |
| `createRelation` | `{from,to,relationType,observations?,attributes?}` |
| `deleteRelation` | `{from,to,relationType}` |
| `reverseRelation` | `{from,to,relationType}` |
| `changeRelationType` | `{from,to,relationType,newRelationType}` |
| `setEntityAttributes` | `{entityName,attributes:{[key]:string}}` |
| `deleteEntityAttributes` | `{entityName,keys:string[]}` |
| `setRelationAttributes` | `{from,to,relationType,attributes:{[key]:string}}` |
| `deleteRelationAttributes` | `{from,to,relationType,keys:string[]}` |
| `addObservation` | `{entityName,body,occurredAtUs?}` |
| `deleteObservation` | `{entityName,observationId}` |
| `editObservation` | `{entityName,observationId,body,occurredAtUs?}` |
| `addRelationObservation` | `{from,to,relationType,body,occurredAtUs?}` |
| `deleteRelationObservation` | `{from,to,relationType,observationId}` |

The Rust adapter validates the operation. It calls shared graph services, not its own `/mcp` HTTP endpoint. Graph mutations need the graph-write scope, the enabled graph-write category, and workspace writer or owner access. Existing MCP write inputs in `tools.json:1-225,495-521` supply the compatible fields.

The graph stores stable numeric observation IDs, but the current read DTO omits them (`crates/mcpmem-core/src/graph.rs:21-22,2099-2112`). Add `observationId:number` to each browser observation view. Keep the existing MCP observation wire format unchanged. An ID lets Edit and Delete target one of two equal bodies. `editObservation` preserves its original creation time and merge origin, and changes only the body or occurred-at time. Implement a durable update with correct FTS and event effects; do not simulate edit with delete plus add. `reverseRelation` and `changeRelationType` change the triple in one atomic graph transaction or report that they cannot proceed. A failed create must not delete the old relation. Merge continues to reject a source with attachments under the current attachment rule.

### Error and capability contract

Return JSON errors as `{code,message}` with an HTTP status. Use 401 for an invalid token and 403 for missing scope before any workspace lookup. Map both an unknown and a denied workspace to the same 404 response (`src/http.rs:1384-1389`). Use 409 for a name or relation conflict. Use 413 for excess upload bytes and 503 for an unavailable optional service. Do not expose another user's private workspace through an error detail. The UI has distinct sign-in, request-more-scope, permission-denied, unavailable, empty, upload-in-progress, and retry states. Abort stale requests after a workspace or selected-node change. A failed mutation keeps form input and displays the server's safe error message.

## Screen behavior

- **Graph:** Use the handoff canvas, rail, toolbar, legend, New menu, node inspector, relation inspector, and Files tab. Keep paged loading and the current bounded canvas. A graph page does not define the whole neighborhood; load incident edges for the selected node. Keep filter and pin state per workspace. Connect mode uses an exact relation triple. Hover links the relation row and edge. Keyboard operations and responsive sheets follow the handoff. A reader sees no write actions.
- **Search:** Keep the query and controls in the URL. Direct searches nodes or relations with FTS. Semantic and Hybrid use the server profile only when available. Show an unavailable state when the feature, profile, or provider is absent. Show file hits only with the separate attachments consent. "Open" loads a panel; "In graph" selects the same node or the file's parent. Multi-select opens the chosen nodes in Graph as an isolated depth-one set. Keep that set bound to the current workspace when Search changes pages. Show score only when returned. Never dim an FTS row by the handoff's vector score threshold.
- **Admin:** Include Overview, Members and grants, Webhooks, Vector index, Principals, Pending approvals, Code repositories, and Workspaces. Put Workspaces in the workspace navigation so an owner without `admin` can use it. Grants and visibility need owner access. Webhooks need both a human-admin token and workspace ownership. The server group needs a human-admin token for its guarded operations. Show only real columns: no invented grant date, principal last-seen time, file count, or last-write clock. Repositories use the existing async state. Hide a compiled-out group and show a named unavailable state for a runtime-off role.
- **Vector index:** Display measured state and counts. Do not map `vector_refresh_graph_cache` to "Refresh index": it only reloads an in-memory graph cache. Omit the handoff refresh button until a real rebuild action exists. Do not add a button that returns success without a rebuild.
- **Files:** Use the existing raw upload and status values `uploaded`, `extracting`, `ready`, and `error`. Poll until ready or terminal error. Show transient error stage during extraction. Preview text as escaped text and PDF from authenticated bytes in a temporary object URL. Revoke that URL after the viewer closes. Preview image controls are absent because uploads allow text/PDF only. Display "Other file types coming soon" as plain text. Download remains available for a terminal error when the blob exists.
- **Consent:** Style the existing OAuth-owned, server-rendered form to match the handoff. Keep its styles inside the OAuth module, not the optional UI bundle. Keep client name, actual return destination, signed-in identity, requested scope subset, CSRF, and deny path. Do not add a workspace selector or a nonfunctional account-switch action. A UI-disabled server still serves consent to external clients when OAuth is enabled.

## Explicit differences from the handoff

| Handoff item | Decision and reason |
|---|---|
| Any file type, image preview, image OCR | Not supported in this work. The approved attachment design limits uploads to text and PDF (`docs/superpowers/specs/2026-10-02-file-attachments-design.md:5-17`). Show future types as information only. |
| Multipart `/ui/node/files` and `/ui/file/:id` | Use the existing raw-body attachment protocol under `/ui/api/attachments`. One upload contract avoids a second limit path (`src/http.rs:286-303`). |
| Consent workspace dropdown and "Not you?" | Scope-only OAuth consent and no unsupported account switch (`src/oauth_routes.rs:1461-1552`). |
| Global Admin Workspaces table | Show accessible workspaces only. Ownership controls grants and visibility (`src/workspace.rs:503-565,747-855`). |
| Live last-write badge, granted date, principal last seen | Do not display fields that the current services do not supply. Keep measured counts and known dates only. |
| Vector "Refresh index" | Omit until a real profile rebuild is available. Cache refresh is a different operation (`src/vector_actions.rs:649-660`). |
| Direct-search score threshold and universal file hits | Do not invent an FTS score or direct file search. Keep file hits from the existing vector path with attachments consent (`src/vector_actions.rs:436-496`). |
| `/ui/search` and `/ui/graph` JSON | Move data below `/ui/api/*`. Use `/ui/search` as a page. The owner approved the URL break. |

No tracker item was supplied for this handoff. If a tracker item is later linked, record the numbered deviations in one decision comment before implementation. Do not implement a handoff behavior that conflicts with the approved server rules without a new owner decision.

## Build, package, and access checks

The current default features are `code,oauth`; assets are unconditional Rust includes (`Cargo.toml:94-108`; `src/http.rs:71-86`). **Check:** `cargo check -p mcpmem --no-default-features --locked` succeeds now, but its compiled UI assets show that it is not a UI-off check. The check ran on 2026-10-04 and returned `OK`.

The new feature and runtime switch need separate checks: build `--features ui`, `--no-default-features`, and `--no-default-features --features ui`. Also build `--no-default-features --features oauth` to prove external OAuth without the UI. Inspect each route table and asset registration. With runtime UI off, request `/ui`, every `/ui/api/*` group, and assets with valid credentials; each must return 404. With UI on, verify public shell MIME types, authorized APIs, byte limits, and workspace gates. Drive `/mcp`, discovery, OAuth registration, an external-client authorization-to-token flow, and consent in both UI states with OAuth compiled. An explicit `--ui true` without the build feature must fail at startup with a named error. A UI-off build must not carry the React asset bytes.

Package the root `mcpmem` crate with its built assets, not just `mcpmem-core`. Include the prebuilt bundle in the published crate so a consumer can install the default `ui` feature without Node. Keep Node as a maintainer build tool, not an installation dependency. Keep no-default HTTP/AWS dependency checks. Run the actual frontend build in CI before Cargo package and release builds. Verify the exact release artifact serves the bundled app without a Node runtime. A feature-off binary must start with the same MCP and OAuth behavior, subject to its own compiled features. Update README, release notes, installation steps, and OAuth runbook with the new URLs and two opt-out controls.

## Traceable needs

| ID | Requirement | Owner unit | Check |
|---|---|---|---|
| U1 | A built UI module and runtime switch each remove all `/ui/*` routes independently. | Rust UI module and config | Build each feature mode and test the route table with valid tokens. |
| U2 | MCP and external-client OAuth work when the UI is off. | Shared HTTP and OAuth | Drive `/mcp` and a full external OAuth flow in both modes. |
| U3 | The default installation retains the browser UI and embeds matching assets. | UI build and package | Build and install the root crate; load its pages and assets. |
| U4 | Graph and Search use page URLs and JSON uses only `/ui/api/*`. | Router and React app | Request each old URL, each page, and each new API URL. |
| U5 | Every read and write checks scope, process category, and workspace role. | Rust adapters | Test reader, writer, owner, admin, missing scope, and wrong workspace. |
| U6 | The graph works with pages, exact relation triples, inspect, edit, connect, and files. | Graph app and graph service | Use a browser with two workspaces and a graph larger than one page. |
| U7 | Direct, semantic, and hybrid search expose only real authorized results. | Search adapter and Search app | Exercise node, relation, and file hits with and without vector and attachments consent. |
| U8 | Admin actions keep human-admin and workspace-owner rules distinct. | Admin adapters and app | Test each action with an admin who does not own the workspace. |
| U9 | Text/PDF upload keeps limits, states, workspace isolation, and download. | Attachment adapter and Files tab | Upload text and PDF; test invalid MIME, quota, wrong workspace, and a stale response. |
| U10 | Consent preserves scope-only security when the UI is off. | OAuth module | Test requested scope subset, hostile client text, deny, token, and UI-off mode. |
| U11 | The UI matches the handoff at desktop, tablet, and phone widths. | React app | Inspect real rendered pages at 1440, 900, and 390 pixels; test keyboard and focus. |
| U12 | The release includes UI assets and current public documentation. | Package and docs | Build a release artifact, load it without Node, and inspect README and release notes. |

## Review risks

- A page-based graph response omits edges whose other endpoint is outside that page. The inspector must load incident relations independently; otherwise a selected node appears disconnected. Check `crates/mcpmem-core/src/graph.rs:985-988` with two disjoint pages.
- The old app has two registered browser OAuth clients. Keep their exact redirect paths and token separation while the new shell replaces both pages. A UI-off start does not seed new browser clients or delete old rows. Check `src/oauth_routes.rs:263-294` with OAuth callback tests and a real browser flow.
- The attachment HTTP API disappears in UI-off mode by owner decision. MCP attachment tools stay available. Check both sets of routes with valid attachments credentials.
- The source handoff requests data fields and index controls the server does not have. The explicit deviations above prevent a working-looking, false control.
