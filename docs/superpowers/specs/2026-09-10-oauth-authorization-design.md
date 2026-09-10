# OAuth authorization for remote MCP clients — design

**Date:** 2026-09-10
**Status:** approved in conversation; not yet planned
**Base revision:** `a0219c9` (`chore: open 1.0.0-rc.4 after releasing 1.0.0-rc.3`)

## 1. Goal

Let the Claude custom connector and the ChatGPT connector use `mcpmem` over
HTTPS. Both products drive an interactive OAuth flow. Neither product offers a
usable path for the static bearer token that the server accepts now.

`mcpmem` becomes an OAuth 2.1 resource server. It also becomes its own
authorization server. It sends the human to an external OpenID Connect provider
for authentication only.

## 2. What already exists

Verify each claim with the command beside it.

| Fact | Location | Check |
| --- | --- | --- |
| MCP Streamable HTTP transport, `POST /mcp` and `GET /mcp` | `src/http.rs:1-21` | `sed -n '1,21p' src/http.rs` |
| One axum router, nine routes, two layers | `src/http.rs:69-87` | `sed -n '69,87p' src/http.rs` |
| TLS through `axum-server` and rustls | `src/tls.rs:22-28` | `cat src/tls.rs` |
| One static bearer token, constant-time compare | `src/http.rs:151`, `src/server.rs:649` | `grep -n 'fn authorized\|fn token_matches' src/http.rs src/server.rs` |
| Four tool categories with stable slugs | `src/tools.rs:32-46` | `sed -n '32,46p' src/tools.rs` |
| Unused principal and scope scaffolding | `crates/mcpmem-core/src/auth.rs:14-19` | `sed -n '13,19p' crates/mcpmem-core/src/auth.rs` |
| Latest advertised protocol version `2025-11-25` | `src/server.rs:592` | `grep -n 'LATEST_PROTOCOL_VERSION' src/server.rs` |
| No `.well-known` route, no CORS layer | `src/http.rs` | `grep -c 'well-known\|CorsLayer' src/http.rs` |

## 3. Approved decisions

Each decision below was taken in the design conversation on 2026-09-10.

**D1. `mcpmem` is both the resource server and the authorization server.**
An external provider authenticates the human. `mcpmem` registers clients, shows
consent, and issues its own tokens. Claude and ChatGPT never receive a token
from the external provider.

Reason: the MCP specification binds every access token to the MCP server as its
audience. An external provider alone cannot serve Claude and ChatGPT, because
those clients need runtime client registration that ordinary providers do not
offer to anonymous callers.

**D2. The upstream provider is any OpenID Connect provider, found by discovery.**
The operator sets `--oidc-issuer`. The server reads
`{issuer}/.well-known/openid-configuration`. GitHub is out of scope, because
GitHub is not an OpenID Connect provider.

**D3. One graph, per-principal scopes.**
All principals read and write the same database. A principal carries its own
scope set. `Principal` in `crates/mcpmem-core/src/auth.rs:12` becomes the live
authorization object. Per-principal graphs are rejected; see section 12.

**D4. Client registration is open, and consent is the real gate.**
`POST /oauth/register` needs no credential. A rate limit and a time-to-live
sweep bound the abuse. Nothing happens until an allowed human approves the
client on the consent page.

**D5. Client ID Metadata Documents are honoured only from allowed domains.**
`--cimd-allowed-domain` defaults to `claude.ai` and `chatgpt.com`.

**D6. The static bearer token stays, and stays independent.**
It works when OAuth is off. It works beside OAuth. It resolves to a principal
named `static` with operator-configured scopes. It keeps the browser viewer at
`src/ui/graph.js:36` working. With no auth configured at all, the server stays
open, exactly as it behaves now.

**D7. Access tokens are opaque, and the database stores only their digest.**
Revocation is immediate. There is no signing key to rotate. Validation is one
local SQLite lookup, because the resource server and the authorization server
share a process.

**D8. Refresh tokens rotate on every use.**
Reuse of a spent refresh token revokes the whole token family. Claude asks for
this behaviour for public clients.

**D9. Consent grants least privilege.**
The consent page offers the intersection of the scopes that the client asked
for and the scopes that the principal holds. The token carries only the scopes
that the human approved. A later shortfall uses the step-up flow.

**D10. Scope strings are the existing category slugs.**
The scopes are `graph-read`, `graph-write`, `vectors` and `code`. They come from
`ToolCategory::slug()` at `src/tools.rs:39` and parse back through the existing
`FromStr` at `src/tools.rs:55`. No second mapping exists, so no drift is
possible.

*Correction:* the design conversation said `graph:read` and `graph:write`. That
spelling would have added a second name for one fact. The slugs win.

## 4. Architecture

A new crate `crates/mcpmem-oauth` sits beside `crates/mcpmem-webhook`. It owns
the discovery documents, dynamic client registration, Client ID Metadata
Document resolution, the upstream OpenID Connect leg, the consent page, and
token issuance. It depends on `mcpmem-core`. `mcpmem-core` never depends on it.

Reason for a separate crate: a defect here is a security defect. The crate must
be testable without a socket, and its surface must stay small enough to read in
one sitting.

Migrations stay in `crates/mcpmem-core/migrations/`, because pull request #5
established that migrations live in the crate that embeds them.

### 4.1 The seam that changes an existing signature

`server::dispatch_http_body(&body, &kg, vs)` carries no caller identity today.
Scope checks belong to each tool call, not to the request as a whole. A
principal with `graph-read` that calls `delete_entities` must get HTTP 403.

The function takes a `&Principal`. The per-tool check sits beside the category
gate at `src/server.rs:123-138`. The stdio transport passes a local principal
with every compiled scope, so stdio behaviour does not change.

### 4.2 Effective capability

The effective capability is the intersection of two sets:

1. the process-wide category flags from `--enable-*`, held in the atomics at
   `src/server.rs:123-126`;
2. the scopes in the presented token.

A tool outside the process-wide set stays absent from `tools/list`, as it is
now. A tool inside the process-wide set but outside the token's scopes is
absent from `tools/list` for that principal, and returns HTTP 403 when called.

## 5. HTTP surface

All new routes attach at the router builder, `src/http.rs:69`.

| Route | Purpose |
| --- | --- |
| `GET /.well-known/oauth-protected-resource` | RFC 9728 metadata, for a client that asks for the bare origin |
| `GET /.well-known/oauth-protected-resource/{path}` | RFC 9728 metadata at the path-suffixed URL |
| `GET /.well-known/oauth-authorization-server` | RFC 8414 metadata for the authorization server |
| `POST /oauth/register` | RFC 7591 dynamic client registration |
| `GET /oauth/authorize` | starts the upstream login leg |
| `GET /oauth/callback` | receives the upstream code, then shows consent |
| `POST /oauth/consent` | records the human decision |
| `POST /oauth/token` | authorization code and refresh token grants |
| `POST /oauth/revoke` | RFC 7009 revocation |

Protected resource metadata:

```json
{
  "resource": "https://host/mcp",
  "authorization_servers": ["https://host"],
  "scopes_supported": ["graph-read", "graph-write", "vectors", "code"],
  "bearer_methods_supported": ["header"]
}
```

*Correction, found by the Task 4 review:* the first version of this section named
one route only. RFC 9728 section 3.1 builds the metadata URL by inserting the
well-known segment between the host and the path. A client that starts from the
resource identifier `https://host/mcp` therefore asks for
`https://host/.well-known/oauth-protected-resource/mcp`, and a server that serves
only the bare path answers 404.

RFC 9728 section 3.3 then requires the `resource` value to equal the identifier
the client built the request URL from. One constant document cannot satisfy both
callers, so the `resource` value is derived from the request path: the public
origin plus the captured path for the suffixed route, and `{public_url}/mcp` for
the bare route.

Authorization server metadata carries at least these fields:

```json
{
  "issuer": "https://host",
  "authorization_endpoint": "https://host/oauth/authorize",
  "token_endpoint": "https://host/oauth/token",
  "registration_endpoint": "https://host/oauth/register",
  "revocation_endpoint": "https://host/oauth/revoke",
  "response_types_supported": ["code"],
  "grant_types_supported": ["authorization_code", "refresh_token"],
  "code_challenge_methods_supported": ["S256"],
  "token_endpoint_auth_methods_supported": ["none"],
  "client_id_metadata_document_supported": true,
  "authorization_response_iss_parameter_supported": true,
  "scopes_supported": ["graph-read", "graph-write", "vectors", "code"]
}
```

The canonical resource URI comes from `--public-url`. The server never derives
it from the `Host` header.

## 6. The flow

1. The client sends `POST /mcp` with no token.
2. The server answers HTTP 401 with this header:
   `WWW-Authenticate: Bearer resource_metadata="https://host/.well-known/oauth-protected-resource", scope="graph-read"`.
3. The client reads the protected resource metadata.
4. The client reads the authorization server metadata.
5. The client registers, through a Client ID Metadata Document or through
   dynamic client registration.
6. The client opens `GET /oauth/authorize` in a browser.
7. The server writes an `oauth_login` row. The row holds the downstream
   `client_id`, `redirect_uri`, `state`, `code_challenge`, `resource` and
   requested scopes. The server makes its own PKCE verifier for the upstream
   leg. The server answers HTTP 302 to the upstream authorization endpoint.
8. The human authenticates at the provider.
9. The provider redirects to `GET /oauth/callback`.
10. The server exchanges the upstream code. The server verifies the identity
    token against the provider JWKS: signature, `iss`, `aud`, `exp` and `nonce`.
11. The server looks up `iss` plus `sub` in the principals file. On a miss the
    server answers HTTP 403 with a plain page, and does not redirect.
12. The server shows the consent page with the client name and the offered
    scopes.
13. The human approves. The server answers HTTP 302 to the downstream
    `redirect_uri` with its own code and the `iss` parameter.
14. The client calls `POST /oauth/token` with the code and the verifier.
15. The server issues an opaque access token and an opaque refresh token. The
    audience is the canonical resource URI.
16. The client repeats `POST /mcp` with the access token.

## 7. Storage

One append-only migration, `crates/mcpmem-core/migrations/0004_oauth.sql`.

| Table | Purpose |
| --- | --- |
| `oauth_client` | registered clients, from registration or from a metadata document |
| `oauth_login` | one in-flight upstream login leg |
| `oauth_code` | one issued authorization code |
| `oauth_token` | access and refresh token digests, with the family identifier |

Rules that hold for every table:

- Token columns hold a SHA-256 digest. They never hold the token value.
- Expiry is a microsecond timestamp, and every lookup filters on it.
- A sweep on the existing worker tick deletes expired rows. No new thread
  starts for this.

## 8. Configuration

| Flag | Meaning |
| --- | --- |
| `--public-url` | canonical HTTPS URL of this server; needed when OAuth is on |
| `--oidc-issuer` | upstream OpenID Connect issuer |
| `--oidc-client-id` | client identifier registered at the provider |
| `--oidc-client-secret-file` | client secret, read from a file |
| `--principals-file` | JSON file of allowed humans and their scopes |
| `--cimd-allowed-domain` | repeatable; defaults to `claude.ai` and `chatgpt.com` |
| `--oauth-trust-forwarded-proto` | accept `X-Forwarded-Proto` from a reverse proxy |
| `--static-bearer-scopes` | scopes for the principal named `static` |

The principals file:

```json
[
  {
    "name": "adam-claude",
    "iss": "https://accounts.google.com",
    "sub": "107800000000000000000",
    "label": "adam@example.com",
    "scopes": ["graph-read", "graph-write", "vectors"]
  }
]
```

`label` is display text for the consent page. Identity is `iss` plus `sub`,
because an email address changes at most providers.

*Correction:* the design conversation showed this file in TOML. JSON needs no
new dependency, and the repository already ships JSON manifests. The format is
JSON.

The server refuses to start in these cases:

1. OAuth flags are present, TLS is absent, and
   `--oauth-trust-forwarded-proto` is absent.
2. OAuth flags are present and the role set holds no MCP role. The precedent is
   the role check for `--legacy-observations` at `src/config.rs:158`.
3. `--oidc-issuer` is present and `--public-url` is absent.
4. The principals file is empty or unreadable. The check mirrors the
   fail-closed token check at `src/config.rs:107`.

## 9. Security requirements

| Threat | Control |
| --- | --- |
| Token theft from the database | store only the SHA-256 digest |
| Token replay at another server | compare the token audience with `--public-url` on every request |
| Open redirect | exact string match of `redirect_uri` against the registered set |
| Server-side request forgery through a metadata document | HTTPS only, allowed domains only, no redirects, 64 KB cap, 5 s timeout |
| Registration flood | rate limit by peer address, plus time-to-live eviction of unused clients |
| Cross-site request forgery on consent | single-use token bound to the `oauth_login` row |
| Authorization code interception | PKCE `S256`, 60-second lifetime, single use |
| Refresh token theft | rotation on every use, family revocation on reuse |
| Account probing | a principal miss ends in a terminal 403 page, never a redirect |

## 10. Error contract

| Status | Case | Header |
| --- | --- | --- |
| 401 | no token, unknown token, expired token, wrong audience | `WWW-Authenticate: Bearer resource_metadata="…", scope="…"` |
| 403 | valid token, missing scope | `WWW-Authenticate: Bearer error="insufficient_scope", scope="…", resource_metadata="…"` |
| 400 | malformed authorization request or token request | none |

A 403 names every scope that the operation needs, in one challenge.

## 11. Verification

### 11.1 The fake provider

A test-only axum server serves `/.well-known/openid-configuration`, a JWKS built
from a keypair that the test generates, an authorization endpoint that redirects
at once, and a token endpoint that returns a signed identity token. The tests
run offline.

Without this fixture the tests would mock our own code, which proves nothing.

### 11.2 One conformance test

The test walks the path that Claude walks: unauthenticated `POST /mcp`, HTTP
401, protected resource metadata, authorization server metadata, dynamic client
registration, `/oauth/authorize`, the fake provider, `/oauth/callback`, the
consent post, `/oauth/token`, and finally `tools/list` with the token. It
asserts on the wire format at every hop.

### 11.3 Negative tests

Each one must fail before its guard exists.

1. Wrong `code_verifier`.
2. Replayed authorization code.
3. Expired authorization code.
4. `redirect_uri` that differs by one character.
5. Token whose audience names another `--public-url`.
6. Reused refresh token; the whole family must die.
7. `sub` that the principals file does not hold.
8. Metadata document on a domain outside the allowed list.
9. A `graph-read` principal that calls a write tool; assert the exact
   `WWW-Authenticate` string.
10. `tools/list` filtered down to the granted scopes.

### 11.4 The derivation test

One test asserts that the scope strings and `ToolCategory::ALL` match in both
directions. A subset check alone would leave a new category unscoped.

### 11.5 Test mechanics

`router()` at `src/http.rs:69` is already public for socket-free tests. The work
adds `tower` as a development dependency for `oneshot`.

### 11.6 Manual acceptance gate

A real Claude custom connector and a real ChatGPT connector must both complete
the flow against a TLS deployment. Their registration behaviour is not fully
documented, and no local test replaces this step.

## 12. Rejected alternatives

| Rejected | Reason |
| --- | --- |
| External authorization server only | ordinary providers do not offer open registration to anonymous MCP clients, and audience binding to `mcpmem` becomes awkward |
| A built-in password login | it adds a credential store, hashing, lockout and rate limits; that is more security-critical code than an OpenID Connect leg |
| Signed JWT access tokens | revocation becomes slow, and a signing key needs rotation; a local lookup costs nothing here |
| Per-principal graphs | every mutation, the full-text projection, the indexer and the vector store would change; that is a separate project |
| Operator pairing codes before registration | consent already blocks every unapproved client, and no evidence shows that ChatGPT offers a field for a pairing code |
| Scope names `graph:read` and `graph:write` | a second name for a fact that `ToolCategory::slug()` already owns |
| GitHub as an upstream provider | GitHub is not an OpenID Connect provider, so discovery does not apply |

## 13. Numbered requirements

| # | Requirement |
| --- | --- |
| R1 | Serve RFC 9728 protected resource metadata |
| R2 | Serve RFC 8414 authorization server metadata |
| R3 | Answer HTTP 401 with `WWW-Authenticate` and a resource metadata link |
| R4 | Accept dynamic client registration with a rate limit and eviction |
| R5 | Resolve Client ID Metadata Documents from allowed domains only |
| R6 | Discover the upstream provider from `--oidc-issuer` |
| R7 | Verify the upstream identity token against the provider JWKS |
| R8 | Map `iss` plus `sub` to a principal from the principals file |
| R9 | Show a consent page with least-privilege scope selection |
| R10 | Issue opaque access tokens bound to the canonical resource URI |
| R11 | Issue rotating refresh tokens with family revocation |
| R12 | Store only token digests |
| R13 | Enforce scopes per tool call, and filter `tools/list` per principal |
| R14 | Answer HTTP 403 with `insufficient_scope` and the needed scopes |
| R15 | Keep the static bearer path working, with and without OAuth |
| R16 | Refuse the four startup configurations in section 8 |
| R17 | Add migration `0004_oauth.sql` and sweep expired rows on the worker tick |
| R18 | Derive scope strings from `ToolCategory`, and test both directions |
| R19 | Support RFC 7009 revocation |
| R20 | Pass the manual acceptance gate with Claude and with ChatGPT |

## 14. Open items

1. The version number for the release. The last plan released a breaking
   contract as a major version. This change adds behaviour and does not break
   the existing static bearer path, so a minor version fits. The operator
   decides.
2. Whether the browser viewer at `/ui` should also accept an OAuth token. The
   viewer uses the static bearer today. This design leaves it unchanged.
