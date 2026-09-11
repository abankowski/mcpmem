# Principal Administration — Design Proposal

**Status:** proposal, pending review. 2026-09-11.
**Author:** agent, for Umbrella Corp worktree.

## Goal

Let an admin manage principals in the web UI:

- list, add, change, remove principals;
- principals from the JSON file are built-in and immutable;
- other principals live in the SQLite database;
- optional approval waitlist: an unknown human is refused but recorded, so an
  admin can promote the entry later;
- the waitlist evicts entries: 24-hour TTL, 25-entry cap.

## Current state

The codebase already holds the identity machinery. The feature is mostly
data, one decision point, and a UI.

| Area | Where | Note |
|---|---|---|
| Principal shape | `src/principals.rs:17-22` | `{ name, iss, sub, label?, scopes }`, read from `--principals-file` at startup, fail-closed, canonicalized |
| Identity key | `principals.rs:29` | `(iss, sub)`. Email is display text only |
| Login-time resolution | `src/oauth_routes.rs:1185-1199` | `principal_of` matches claims against `config.principals`; no match = refused login page |
| OAuth AS | `oauth_routes.rs`, crate `mcpmem-oauth` | authorize/consent/token, RFC 7591 dynamic registration, PKCE, public clients only |
| Tokens | `crates/mcpmem-oauth/src/token.rs:36-43`, `store.rs:536-539` | access TTL 1 h, refresh TTL 30 d; `oauth_token` rows already carry `principal` and `family` |
| UI | `src/ui/{index.html,graph.js,graph.css}` | static, vanilla JS, no bundler |
| SQLite | rusqlite; versioned migrations (`src/../crates/mcpmem-core/src/events.rs:103-107`, `migrations/0001..0004`, STRICT tables, WAL) | oauth tables already live in this DB (`0004_oauth.sql`) |

## Approaches

### Runtime principal store

- **A. Runtime principals in the existing SQLite file (recommended).** Live
  CRUD, no restart, one backup to manage, oauth tables already live there.
  Merged view: JSON entries (built-in) plus DB rows.
- **B. The UI rewrites the JSON file.** One source of truth, but: writes race
  with a concurrent edit, edits need atomic rename, the file is a config
  document (operator-owned), and live reload needs a watcher. Rejected.
- **C. A second SQLite file for principals.** Same as A with an extra file to
  back up and to keep consistent. No benefit. Rejected.

### Admin authentication

- **A. Standalone OAuth flow, admin scope (recommended).** The admin UI is its
  own OAuth client. It runs the server's own authorization flow with PKCE and
  requests a new `admin` scope. Works with no active MCP session; uses the
  existing consent page and token machinery. Bootstrap: the first admin is a
  built-in JSON entry that holds `admin`.
- **B. Reuse the active connector session.** The admin UI only works while a
  connector session is live. No session = no administration; also a
  bootstrap paradox. Rejected.

## Design

### Storage (migration `0005_principals.sql`)

```sql
CREATE TABLE runtime_principal (
    iss        TEXT NOT NULL,
    sub        TEXT NOT NULL,
    name       TEXT NOT NULL,
    label      TEXT,
    scopes     TEXT NOT NULL, -- JSON array of scope slugs
    created_us INTEGER NOT NULL,
    updated_us INTEGER NOT NULL,
    PRIMARY KEY (iss, sub)
) STRICT;

CREATE TABLE principal_waitlist (
    iss          TEXT NOT NULL,
    sub          TEXT NOT NULL,
    name         TEXT NOT NULL,
    first_seen_us INTEGER NOT NULL,
    last_seen_us  INTEGER NOT NULL,
    PRIMARY KEY (iss, sub)
) STRICT;
```

Same DB as the oauth tables. Admin writes must be durable: use a dedicated
write connection with `synchronous=FULL` regardless of the
`MCP_MEMORY_DURABILITY` setting, or force a WAL checkpoint after the write.

### Merged principal resolution

One resolver answers "who is this `(iss, sub)`" from two sources:

1. JSON entries from `--principals-file` (built-in, immutable);
2. DB rows from `runtime_principal`.

**The JSON file wins on a key collision.** A DB row whose key exists in the
JSON is masked (ignored for resolution) and logged at startup. The API marks
it `masked-by-builtin` so the UI can show why.

**A colliding write is refused with 409.** The admin cannot create or update a
runtime principal whose `(iss, sub)` is a built-in key, and cannot delete a
built-in at all. "Immutable" is enforced server-side, not just in the UI.

The login-time `principal_of` (`oauth_routes.rs:1185`) switches from
`config.principals` alone to this resolver. Validation is shared: extract the
canonicalization and scope check from `principals.rs::load` into one function
that both the file loader and the DB write path call, so the two sources can
never accept different shapes.

### The admin scope

A new scope string, `admin`. It is not a `ToolCategory` (no tools carry it);
it is a grant the consent flow can issue and the admin routes check. Changes:

- `principals.rs` validation accepts `admin` as a known scope;
- the OAuth metadata advertises it when the feature is on, so a client can
  request it;
- an entry that holds `admin` may operate the admin API. Everything else is
  unchanged: `tools/list` never lists it, `missing_scope` never consults it.

### Admin client and flow

At startup, when the feature is on and `public-url` is known, the server
inserts (or refreshes) a reserved public client:

- `client_id`: `mcpmem-admin-ui`;
- `redirect_uri`: `{public-url}/ui/admin/callback`;
- token auth: none (PKCE), same as every client here.

`/ui/admin` runs `authorization_code + PKCE` against the server's own AS,
requests `["admin"]`, and stores the access token in memory (no cookies; the
existing `graph.js` convention of an `Authorization` header, so no CSRF
surface). The consent page already intersects the request with the
principal's grant, so a human without `admin` gets the existing "holds none
of the requested scopes" refusal.

Bootstrap: the JSON file must contain the first admin (a built-in entry
holding `admin`). Documented as the way in.

### HTTP API

All under `/ui/api`, all gated on the `admin` scope (or a static bearer with
`admin` scope). No MCP tools are added: the UI is the admin surface.

| Method | Path | Meaning |
|---|---|---|
| GET | `/ui/api/principals` | list; each row: `builtin: bool`, `maskedByBuiltin: bool` |
| POST | `/ui/api/principals` | create runtime principal (409 on built-in key) |
| PATCH | `/ui/api/principals/{iss}/{sub}` | rename, relabel, rescope |
| DELETE | `/ui/api/principals/{iss}/{sub}` | delete; refused for built-in; revokes token families |
| GET | `/ui/api/waitlist` | pending entries |
| POST | `/ui/api/waitlist/{iss}/{sub}/approve` | promote: create principal (body carries scopes), delete row |
| DELETE | `/ui/api/waitlist/{iss}/{sub}` | dismiss |

### Waitlist semantics

Config-gated: `[oauth] approval-waitlist = true` (default off; the current
hard refusal stays). When on, the login path, on an unknown `(iss, sub)`:

- upserts the waitlist row (name, timestamps);
- renders a "pending approval" page instead of the refusal page;
- never issues a token.

Eviction, deterministic and documented:

- TTL: 24 hours from `first_seen_us` (fixed; a retry does not extend it).
  Configurable: `[oauth] approval-waitlist-ttl-seconds`.
- Cap: 25 rows. On insert past the cap, evict the row with the oldest
  `last_seen_us` first, then oldest `first_seen_us` (LRU by activity).
  Fixed size per the requirement; the RRU eviction keeps an actively-retrying
  human visible over a stale one.

Approve = create runtime principal (admin picks scopes in the dialog; the
dialog defaults to `[oauth] default-new-principal-scopes`, proposed default
`["graph-read"]`) and delete the waitlist row. Dismiss = delete the row.
Ordering is explicit: there is never a dual source of truth.

Notes that fell out of the review:

- The waitlist stores PII (`name`, `iss`, `sub`) of non-members. The TTL-cap
  bounds it; document the retention in the config file comment.
- The waitlist inherits the login rate limit (`LOGIN_PER_WINDOW`), so it
  cannot be flooded faster than logins can arrive.

### Revocation

Deleting a runtime principal revokes its live token families immediately.
The rows needed are already there: `oauth_token.principal` names the human,
`family` groups the grant (`crates/mcpmem-oauth/src/store.rs:536-539`). The
delete handler collects the live families for that name
(`SELECT DISTINCT family FROM oauth_token WHERE principal = ?1 AND revoked = 0`)
and calls `Store::revoke_family` on each (`store.rs:617-620`). Worst case an
already-minted access token lives out its one-hour TTL; every refresh and
new login is refused from the moment of deletion. Renaming or rescoping a
principal changes only future grants (a granted scope is the grant's own
value, by design).

Known limitation, v1: the match is by the entry's display `name`, which is
what `oauth_token.principal` stores. A principal renamed, then deleted,
leaves token families under the old name alive until they expire (access
TTL ≤ 1 h; a rotating refresh family ≤ 30 d). Precise revocation needs
`(iss, sub)` carried on the token rows, which touches the published
`mcpmem-oauth` grant structs (a semver decision). Tracked as follow-up.

### UI

`/ui/admin`, vanilla JS like `graph.js`, no bundler:

- principals table: built-ins shown read-only with a "built-in" badge and no
  edit/delete controls; runtime rows get edit (modal: name, label, scopes
  checkboxes) and delete;
- "add principal" form: name, iss, sub, label, scopes;
- waitlist panel: table of pending entries with Approve (opens the add/edit
  form pre-filled from the row, plus scope picker) and Dismiss.

## Out of scope

- roles beyond `admin` (no per-scope admin levels);
- an audit log (the outbox machinery could host one later; not now);
- impersonation or takeover flows;
- SAML or non-OIDC issuers.

## Decisions for review

Decisions made here that you can veto:

1. **JSON wins on key collision; colliding writes refused (409); stale DB
   rows masked, not deleted.**
2. **A new `admin` scope string**, not a `ToolCategory`, not a role flag.
3. **Delete revokes the principal's token families immediately.**
4. **Promoted users default to `graph-read` scopes** (configurable).
5. **Waitlist TTL fixed at 24 h from first attempt** (configurable; sliding
   TTL is a one-line change if you prefer it).

## Files touched (sketch)

- `crates/mcpmem-core/migrations/0005_principals.sql` (new)
- `src/principals.rs` (shared validation; merged resolver)
- `src/oauth_routes.rs` (resolver at login; waitlist upsert + pending page)
- `src/http.rs` (admin routes; admin client bootstrap; admin gate)
- `src/config_file.rs` / `src/config.rs` (new `[oauth]` keys)
- `crates/mcpmem-oauth/src/*` (advertise `admin`; revoke-by-principal store
  method, if not already expressible)
- `src/ui/*` (admin page; `index.html`, JS, CSS)
- `mcpmem.example.toml`, `README.md` (config + runbook)
- `tests/` (resolver, waitlist, revocation, API, UI gates)