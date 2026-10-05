# mcpmem web UI - design handoff

Design package for the new mcpmem browser UI (graph viewer, search, admin, OAuth consent) and the public landing page. Written for coding agents: everything a builder needs is in this folder, no design tool required.

Repo: https://github.com/abankowski/mcpmem. The UI talks to the existing `/ui/*` HTTP routes plus a few new ones listed in section 8.

## 0. Package contents

| Path | What it is | Use it for |
|---|---|---|
| `README.md` | this spec | source of truth for structure, behaviour, naming |
| `tokens.css` | all design tokens as CSS variables, plus a shadcn `.dark` mapping | drop into `globals.css` |
| `screens/*.html` | the six hi-fi screens as plain static HTML (no runtime, no build) | open in a browser or read the markup for exact spacing, sizes, copy |
| `renders/*.png` | full-page PNG of each screen at 1440 wide | vision reference |
| `landing/index.html` + `landing/brain.svg` | static landing page, deployable as is | copy to a static host |
| `brain.svg` | the brain-shaped graph illustration | hero, consent background, empty states |

The HTML in `screens/` is reference markup, not the implementation. Build with React + shadcn/ui + Tailwind; use the screens for pixel values and copy, the spec for behaviour.

## 1. Stack and conventions

- React, TypeScript, Tailwind, shadcn/ui ("new-york" style), lucide-react icons.
- Dark theme only for now. `tokens.css` carries the full token set and a `.dark` block mapped to shadcn variable names. Keep both: components use shadcn names, graph and domain colors use the `--node-*`, `--edge*`, `--orange-deep` etc. names.
- Fonts from Google Fonts: `IBM Plex Sans` 400/500/600 (UI), `JetBrains Mono` 400/500 (identities), `Fraunces` 600/700 (wordmark, page titles, consent heading only). Never set body text in Fraunces.
- Type scale: UI body 14/20; small 13/18 (meta, secondary rows); page title 26/32 Fraunces 600; node name 20/26 Plex 600; caption 11/16 mono uppercase tracking .06em in `--ink-faint`.
- What is set in mono: relation types, attribute keys and values, counts, timestamps, IDs, scopes, workspace id, scores, file meta. Everything users read as prose is Plex.
- Sentence case everywhere. No emoji. Plain hyphen, never em dash. Dates `2026-09-04`, times 24h.
- Borders, not shadows. `--shadow-float` only on sheets, popovers, command palette, lightbox.
- Motion at most 150 ms (sheet slide, tab underline, hover). Canvas physics is exempt.
- Focus: 2px `--focus` ring, 2px offset, on every interactive element. Icon-only buttons get `aria-label`. Real `<button>`, `<a href>`, `<input>` + `<label>`.
- Orange discipline: `--orange` is a fill (primary button, selected node ring, active tab underline, upload progress). `--orange-deep` is text (links, active nav item, "Add" actions, scores, hot edge). One primary button per screen.

## 2. Component inventory (shadcn base -> house usage)

| Component | shadcn base | House rules (see screens for exact look) |
|---|---|---|
| Button | `Button` | h-32 default, h-26 `sm`, h-40 on consent. Variants: `primary` (orange fill, dark text), `default` (surface-1, line border), `ghost`. Icon-only: 32x32, `aria-label`. |
| TopBar | custom | h-48, surface-1, hairline bottom. Wordmark (Fraunces 20) · WorkspaceSwitcher · nav (Graph/Search/Admin, active in orange-deep) · spacer · CommandTrigger (w-280, shows `⌘K`) · account avatar (24px circle, initials). |
| WorkspaceSwitcher | `DropdownMenu` | ghost button with a window icon, mono workspace id, chevron. Lists workspaces from `GET /ui/workspaces`; switching never changes the stored default. |
| CommandPalette | `Command` / `cmdk` | `⌘K` / `Ctrl+K`. Jump to node only: prefix FTS over names, recent nodes on empty query. Enter selects node in Graph (navigates if elsewhere). |
| Tag | `Badge` | pill, h-22, surface-2, mono 11. Entity type tag carries an 8px color dot. Tones: `ok` (green-deep on green-tint), `warn` (orange-deep on orange-tint). |
| Count | custom | mono 11, surface-2, radius-sm, 1px 5px. Used in tabs, filters, sub-nav. |
| FilterRail | custom | w-280, surface-1, right hairline. Sections: Entity types, Relation types, Pinned, Workspace stats. Collapsible to a 40px icon rail; state in localStorage. |
| FilterRow | `Checkbox` + label | h-28, hover surface-2; color dot for entity types, mono label for relation types, count right. "all · none" link per section. "N more types" disclosure. |
| CanvasToolbar | floating group | top-left, surface-1, line border, radius 8, padding 4. Fit · zoom out · zoom % · zoom in · divider · Layout dropdown · Depth dropdown · divider · Connect toggle. |
| NewMenu | `DropdownMenu` on primary Button | top-right. Items: Node, Relation, Observation. Only primary button on the Graph screen. |
| Legend | floating, bottom-left | first 5 types with dots, "+N" for the rest. Click a dot toggles that type filter. |
| Inspector | custom panel | w-380, surface-1, left hairline. Header (type tag, actions, name, mono meta line) · Tabs (Node, Relations N, Files N) · scrolling body with 24px between groups. |
| Tabs | `Tabs` | h-40, 13/500, inactive ink-muted, active ink with 2px orange underline. |
| ObservationCard | custom | surface-0 on surface-1, line border, radius-md, 10px 12px. Body 13/19, timestamp mono 11 faint. Edit and delete icon buttons appear on hover. "Show N more" after 3. |
| ObservationComposer | `Textarea` + buttons | inline under the list; `occurred at` date picker (defaults to now), Cancel, Save (primary sm). |
| KVGrid | custom | grid 120px + 1fr, mono 12/18, hairline between rows, key in ink-faint. Inline edit on click, Enter saves, Esc cancels. Values that match an entity name render as links. |
| RelationRow | custom | surface-0 card, type in mono (orange-deep when hot), direction `→ out` / `← in`, target name + its type tag, meta line. Hover lights the edge on canvas; click opens EdgeInspector. |
| EdgeInspector | replaces Inspector body | back link (`← <node name>`), relation type tag in orange-tint, `from → to` with both names linked, meta, Observations, Attributes, Actions row (Change type, Reverse direction, Delete relation in red-deep). |
| FileDropzone | custom | dashed line-strong, radius-md, 20px, icon + "Drop files here or browse" + mono limit line. Whole inspector accepts drop while Files tab is open. |
| FileRow | custom | 64px thumbnail (image preview, else mono type label), name 500, mono meta `TYPE · size · date · indexed · N chunks`, Preview / Download / delete. Uploading: 4px orange progress bar, `uploading · 62% · then indexing`. |
| Lightbox | `Dialog` | over the canvas; image, PDF (pdf.js or iframe), text and markdown. Others download only. |
| SegmentedControl | `ToggleGroup` | h-30, line border, radius-md; active item surface-2 with 2px orange inset underline. |
| SearchInput | `Input` | h-44, 16px text, leading search icon, line-strong border, focus ring. |
| ResultCard | custom | checkbox · title 15/500 + type tag (+ file chip for file hits) · snippet 13/19 ink-muted with `<mark>` (orange-tint bg, `#ffb380` text) · mono meta line · score mono 12 orange-deep · Open / In graph. Rows below 0.65 score at 70% opacity. |
| DataTable | `Table` | wrapped in line border radius-lg, `overflow-x:auto`. Head: surface-2, mono caption. Cells 12px padding, hairline rows, hover surface-1. Row kebab opens edit / disable / remove. |
| Sheet | `Sheet` side="right" | w-440, surface-1, left hairline, shadow-float, 24px padding, page dimmed `rgba(20,19,16,.55)`. Title Fraunces 20. Footer Cancel + primary. All admin create/edit flows use this, not centered dialogs. |
| ScopeCard | `Checkbox` in a bordered label | mono scope name + one-line description in ink-faint; whole card clickable; hover line-strong. |
| AdminSubNav | custom | w-240, two groups: Workspace (Overview, Members and grants, Webhooks, Vector index) and Server (Principals, Pending approvals, Code repositories, Workspaces). Active: surface-2 + 2px orange inset left. Counts right; pending approvals as warn tag. Server group only with `admin` scope. |
| ConfirmDialog | `AlertDialog` | for delete, merge, remove. Destructive button uses red-deep text on default fill, not a red fill. |
| Toast | `Sonner` | bottom-right, surface-1, line border; success carries a green word, error a red word. |

## 3. Screens

### 3.1 Graph explorer (`screens/01-graph-explorer.html`)

Layout: TopBar / [FilterRail 280 | Canvas flex | Inspector 380]. No page scroll; canvas fills. Under 900px the rail and inspector become sheets.

Canvas:
- Force-directed layout, pan, wheel zoom, drag-to-pin. Keep the existing `<canvas>` renderer or move to a library; either way match the look: nodes filled with their type color, radius 6 to 17 scaled by degree, edges 1px `--edge`, edges of the selected node 1.5px `--edge-active` with arrowheads, hovered or inspector-hovered edge `--edge-hot` 2px with its type label in a small surface-1 chip.
- Labels: only for nodes with degree >= 3 and for the selection and its neighbours. Selected node label in ink 12/500; others ink-muted 11.
- Selection: orange ring (3px surface-0 gap + 3px orange). Filtered-out and out-of-path nodes at 30% opacity, never removed from layout.
- Double-click a node expands its neighbourhood (`GET /ui/expand`, depth from toolbar, direction both).
- Connect mode: toolbar toggle; click source, click target, a type picker popover appears at the target with `list_relation_types` suggestions; Esc cancels.
- Legend bottom-left, "N / total shown · load more" bottom-right (paginated `GET /ui/graph`).
- Empty workspace: center the `brain.svg` at 14% opacity with a line "No nodes yet. Create one or point an agent at this workspace."

Filter rail: entity types with counts from `/ui/graph`'s `entityTypes`, relation types from `list_relation_types`, pinned nodes (localStorage per workspace), workspace stats from `graph_stats` with a green "live" dot when the last write is under 60 s old. Fold toggle top-right of the rail; folded state is a 40px rail with three icon buttons that open the section as a popover.

Inspector, Node tab: Observations (count, Add, cards, Show N more) · Attributes (count, Add, KVGrid) · Metadata (created, updated, merged from, embedding state). Header actions: Expand, Isolate, Pin, kebab (Rename, Merge into…, Delete). Rename and Merge use a small dialog; Delete confirms.

Relations tab: see `02-inspector-states.html` panel 1. Filter All / Out / In; `+ Relation` starts connect mode with this node as source.

Files tab: see `02-inspector-states.html` panel 3.

### 3.2 Inspector states (`screens/02-inspector-states.html`)

Three panels side by side: Relations tab, Edge inspector, Files tab. The edge inspector replaces the inspector body in place (not a second panel); the back link returns to the node that was open.

### 3.3 Search (`screens/03-search.html`)

Layout: TopBar / centered column max 960, 40px padding. Page title "Search" in Fraunces.

Controls: SearchInput · Mode segmented (Direct = FTS5 `search_nodes` / `search_relations`; Semantic = `semantic_search`; Hybrid = `hybrid_search`) · Scope segmented (Nodes, Relations; Relations scope swaps the Type dropdown for from / to / relationType filters) · Type dropdown · Top k dropdown (10, 20, 50). Query is in the URL (`?q=&mode=&scope=&type=&k=`), submit on Enter and on control change.

Results: count and timing line mono, Select all, "Show N in graph" (disabled at 0; navigates to Graph with the selected names isolated, depth 1). ResultCards per section 2. "Open" slides the Inspector in as a right sheet on this page; "In graph" navigates with that node selected. File hits show a file chip (`name · p.N`) and "Open file" opens the Lightbox at that page.

Before the first query: `brain.svg` at 10% opacity under the controls, no results list.

### 3.4 Admin (`screens/04-admin.html`)

Layout: TopBar / [AdminSubNav 240 | content flex, 32px padding]. Page title Fraunces 26 + one-line description + primary action on the right. Filter input (w-280) + facet dropdown. DataTable. Mono footer line with totals.

Pages and their columns:
- Overview: workspace id, owner, visibility (Private / Public toggle, owner only), stats tiles (nodes, relations, types, files, last write).
- Members and grants: identity, role (owner / writer / reader), granted at, kebab (change role, revoke). Sheet: Grant access (identity picker from principals, role).
- Webhooks: endpoint, origin, filters (ops · types), secret ref, state tag, Test / Edit / Remove. Sheet: fields as in the current UI (endpoint, consumer origin, secret ref, operations checkboxes, entity types, ignored origins, enabled).
- Vector index: profile, dims, model, chunks, last refresh, Refresh button.
- Principals: name (+ built-in tag), issuer, subject (truncated middle), scope tags, last seen, kebab. Sheet shown in the screen: Name, Issuer, Subject, Label, ScopeCards, Cancel / Save principal.
- Pending approvals: name, identity, first seen, Approve (primary sm) / Deny inline. Count shows as a warn tag in sub-nav.
- Code repositories: key, URL, auth, state tag (indexed / indexing / failed), last indexed, Reindex / Remove. Sheet: key, Git URL, auth select, token or private key textarea, "store body snippets" checkbox, Add and index.
- Workspaces (server): id, owner, visibility, size, created, kebab. Sheet: Create workspace.

### 3.5 OAuth consent (`screens/05-oauth-consent.html`)

Standalone route, no TopBar. `brain.svg` centered behind at 14% opacity, 1100px wide. Card max-w 480, surface-1, radius-lg, 32px padding, shadow-float. Above the card: wordmark + host in mono. Card: title "Authorize <client name>" Fraunces 26, one-line description, facts list (Client, Returns to, Workspace dropdown), "Grant only what it needs" + ScopeCards for the requested scopes only (never list unrequested ones), signed-in-as line with "Not you?", Deny (default) + Approve (primary). Under the card: mono note that the grant can be revoked in Admin · Principals. Works at phone width: card goes full width, buttons wrap.

### 3.6 Tokens sheet (`screens/00-tokens-dark.html`)

Visual reference for the token set in `tokens.css`, including the entity palette and the on-canvas node states (filled, hollow, selected ring, 30% dimmed).

## 4. Landing page (`landing/index.html`)

Static, deployable as is. Sections: hero with `brain.svg` inline; three pillars (graph, code, search) with their CLI flags; how it works with install and config snippets copied from the README and a facts list (transport, storage, embeddings, files, code, events, telemetry); works-with strip; a mocked UI block; security trio; closing CTA; footer. Primary CTA is GitHub. Before publishing: confirm the "works with" names, add a favicon and an `og:image` (a 1200x630 crop of the hero works).

## 5. Behaviour details that are easy to miss

- Entity colors: rank entity types by count in the current workspace; rank 1..8 take `--node-1..8`; rank 9+ reuse `--node-((rank-1) mod 8)+1` drawn as a hollow 2px ring. Recompute on workspace switch only, not on every write, so colors stay stable in a session.
- Relation identity is the triple `(from, to, relationType)`. Every edge write and attribute call addresses that triple. "Reverse direction" is delete + create.
- Observations carry `body`, server `createdAtUs`, optional `occurredAtUs`, and `originEntityName` after a merge; show the merged origin in the card meta.
- Unknown entity or relation types are never refused by the server; show `taxonomySuggestions` from the write response as a dismissible hint under the type field.
- Workspace switch rewrites every list and the canvas; selection, filters and pinned list are per workspace.
- Keyboard: `⌘K` palette, `Esc` clears selection / closes sheet, `F` fit, `+`/`-` zoom, `Del` on a selected node or edge asks to delete.
- Hover on a RelationRow highlights the edge; hover on an edge highlights the row. Both use `--edge-hot`.
- Files: accept any type up to the configured limit; preview for image, PDF, text, markdown; index text of PDF, text, markdown and images (OCR). Index state per file: `pending`, `indexing`, `indexed · N chunks`, `failed` (red word + retry).

## 6. Responsive

Desktop first at 1280 to 1440. Below 1100: inspector becomes a right sheet. Below 900: filter rail becomes a left sheet, admin sub-nav becomes a top select. Below 640: search controls stack, result action buttons move under the snippet, consent card is full width. The canvas is always full width of what remains.

## 7. Accessibility

Contrast is checked for every ink on every surface in `tokens.css`; do not lighten `--ink-faint`. Color never carries meaning alone (type tags have names, status has words). All controls are native elements with labels. Tab order: top bar, rail, canvas (one focusable node list behind it for screen readers is acceptable), inspector.

## 8. API surface

Existing `/ui/*` routes used as documented in the repo README: `GET /ui/graph`, `/ui/search`, `/ui/node`, `/ui/expand`, `/ui/workspaces`, plus the admin routes already backing the current UI (principals, approvals, webhooks, repositories).

Needed for this design and not in the server today (names are proposals):

| Route | Purpose |
|---|---|
| `GET /ui/relation?workspaceId&from&to&relationType` | one relation with observations and attributes (edge inspector) |
| `GET /ui/semantic?workspaceId&q&scope&type&k` and `/ui/hybrid` | search page modes; may proxy the MCP tools |
| `POST /ui/node/files` (multipart), `GET /ui/node/files?name`, `GET /ui/file/:id`, `GET /ui/file/:id/preview`, `DELETE /ui/file/:id` | files on nodes; response carries `indexState`, `chunks`, `pages` |
| `GET /ui/file/:id/chunk/:n` | jump target for file search hits |
| `GET /ui/workspace/:id/grants`, `POST`, `DELETE` | members and grants page (wraps `list_workspace_grants`, `grant_workspace_access`, `revoke_workspace_access`) |
| `POST /ui/workspace/:id/visibility` | overview toggle |
| `GET /ui/vectors/stats`, `POST /ui/vectors/refresh` | vector index page |
| `GET /ui/types?workspaceId` | entity and relation types with counts and `desc` (one call for rail + legend + pickers) |

Writes from the UI (create, edit, delete, merge, rename, attributes) can go through the existing MCP tools over HTTP with the signed-in identity's token, or through thin `/ui` wrappers; pick one and keep it consistent. The UI requires `graph-read`; write affordances render only when the token carries `graph-write`; the Server admin group only with `admin`.

## 9. Open decisions (not blocking)

- Image indexing: OCR assumed. Vision-model captions instead would change the `(OCR)` label and the landing copy.
- Entity colors by count rank (zero config) vs a per-type color pin in admin. Design assumes rank.
- Row kebab in admin tables vs inline actions. Design assumes kebab, except Pending approvals which is inline.
