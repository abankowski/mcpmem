# Runbook: deploying `mcpmem` with OAuth 2.1

For the operator who has to make this work, or work again. Every command is
given for **Bash** and for **fish**. Where the two are identical, it says so.

- [What this turns on](#what-this-turns-on)
- [1. Register `mcpmem` at your provider](#1-register-mcpmem-at-your-provider)
- [2. Find each human's `sub` and write the principals file](#2-find-each-humans-sub-and-write-the-principals-file)
- [3. The command line](#3-the-command-line)
- [4. Behind a reverse proxy](#4-behind-a-reverse-proxy)
- [5. Add the connector](#5-add-the-connector)
- [6. Revoking access](#6-revoking-access)
- [7. What each refusal means](#7-what-each-refusal-means)
- [8. The limits](#8-the-limits)
- [9. What maintenance deletes](#9-what-maintenance-deletes)

## What this turns on

`--oidc-issuer` makes `mcpmem` its **own** OAuth 2.1 authorization server. It
issues its own opaque tokens; the upstream OpenID Connect provider only
authenticates the human. The database holds token digests, never token values.

A connector discovers the server, registers itself, sends its human to your
provider, takes consent for a subset of scopes, exchanges a code, and calls
tools under the resulting token.

The static bearer token (`--auth-token`, `--auth-token-file`) **still works**
and is unchanged. A server may run both; a request carrying an issued OAuth
token is resolved as that token, and anything else falls back to the static
token. `--static-bearer-scopes` narrows what the static token reaches.

Four scopes exist, one per tool category: `graph-read`, `graph-write`,
`vectors`, `code`. A server advertises only the categories its `--enable-*`
flags turned on.

## 1. Register `mcpmem` at your provider

`mcpmem` is a **client** of your provider, and a **server** to the connector.
This step is the first of those.

At the provider, create an OpenID Connect client:

| Field | Value |
| --- | --- |
| Redirect URI | `{public_url}/oauth/callback` |
| Grant type | Authorization code |
| Response type | `code` |
| Scopes | `openid` |
| Client type | Public, or confidential with a secret |

With `--public-url https://mem.example.com`, the redirect URI is exactly:

```
https://mem.example.com/oauth/callback
```

One spelling, byte for byte. Most providers compare it exactly, and a trailing
slash is a different URI. If `--public-url` carries a path prefix — say
`https://example.com/mem` — the callback is `https://example.com/mem/oauth/callback`.

Keep the client identifier for `--oidc-client-id`. If the provider issued a
secret, write it to a file with no trailing newline concerns (the file is
trimmed) and pass `--oidc-client-secret-file`. Omit the flag for a public
client.

`mcpmem` discovers everything else — the authorization endpoint, the token
endpoint, the key set — from `{oidc_issuer}/.well-known/openid-configuration`
on the first login. Verify the issuer answers it:

```sh
# Identical in Bash and fish.
curl -fsS https://accounts.example.com/.well-known/openid-configuration | jq .issuer
```

The value it prints must equal what you pass to `--oidc-issuer`. A mismatch
fails every login with `Sign-in failed` and logs an issuer error.

## 2. Find each human's `sub` and write the principals file

Nobody can authorize until they are in the principals file. Identity is `iss`
plus `sub` **together** — `sub` alone would let a second provider mint a
subject that names somebody here.

`sub` is the provider's immutable identifier for the human. It is not the
email address: most providers let a human change that, so `mcpmem` treats an
email as display text only.

Where to find it, by provider. **These are starting points, not verified
against each console** — confirm with the claim itself, below the table:

| Provider | Where `sub` is |
| --- | --- |
| Google Workspace | Admin console → the user → the 21-digit numeric ID; or the `sub` claim of any ID token |
| Microsoft Entra ID | The `sub` claim is **per-application**. Sign in once and read it from the token; the directory object ID is not `sub` |
| Okta | The user's ID, `00u…` |
| Auth0 | `auth0|…`, or `google-oauth2|…` for a federated identity |
| Keycloak | The user's UUID in the admin console |

The reliable way for any provider is to read the claim from a real token. If
the provider has a userinfo endpoint and you can get an access token for the
human:

```sh
# Identical in Bash and fish.
curl -fsS -H "Authorization: Bearer $TOKEN" \
  https://accounts.example.com/userinfo | jq '{sub, email}'
```

Note the shell difference when you set `TOKEN` first:

```bash
# Bash
export TOKEN=ya29....
```

```fish
# fish
set -x TOKEN ya29....
```

Then write the file:

```json
[
  {
    "name": "adam",
    "iss": "https://accounts.example.com",
    "sub": "109876543210987654321",
    "label": "Adam Bankowski",
    "scopes": ["graph-read", "graph-write"]
  },
  {
    "name": "reader",
    "iss": "https://accounts.example.com",
    "sub": "110000000000000000001",
    "scopes": ["graph-read"]
  }
]
```

- `name` is what the audit log calls this human, and what the consent page
  shows. Required and non-empty.
- `iss` must equal the provider's own issuer identifier exactly.
- `label` is optional display text for the consent page; `name` is used when it
  is absent.
- `scopes` is the **ceiling**. A connector may ask for less and a human may
  grant less, but nothing here can exceed this list.

The file fails closed. The server refuses to start when it is unreadable, not
valid JSON, an empty list, missing a `name`, `iss` or `sub`, holding a
principal with no scopes, naming an unknown scope, or listing the same
`iss`+`sub` twice. Check it before a restart:

```sh
# Identical in Bash and fish.
jq -e 'length > 0 and all(.[]; .name != "" and .iss != "" and .sub != "" and (.scopes | length > 0))' \
  /etc/mcpmem/principals.json
```

Changing the file needs a restart. It is read once, at startup.

## 3. The command line

The minimum for a working OAuth deployment, where the server ends TLS itself:

```bash
# Bash
mcpmem \
  --memory-file /var/lib/mcpmem/memory.mcpmem \
  --transport http \
  --bind 0.0.0.0:8443 \
  --enable-all \
  --public-url https://mem.example.com \
  --oidc-issuer https://accounts.example.com \
  --oidc-client-id mcpmem-prod \
  --principals-file /etc/mcpmem/principals.json \
  --tls-cert /etc/mcpmem/fullchain.pem \
  --tls-key /etc/mcpmem/privkey.pem
```

```fish
# fish
mcpmem \
  --memory-file /var/lib/mcpmem/memory.mcpmem \
  --transport http \
  --bind 0.0.0.0:8443 \
  --enable-all \
  --public-url https://mem.example.com \
  --oidc-issuer https://accounts.example.com \
  --oidc-client-id mcpmem-prod \
  --principals-file /etc/mcpmem/principals.json \
  --tls-cert /etc/mcpmem/fullchain.pem \
  --tls-key /etc/mcpmem/privkey.pem
```

**The two are identical.** A long command with `\` continuations and no
variables, substitutions or globs is the same text in both shells. The
difference appears only when a value comes from a variable or a command:

```bash
# Bash — a secret from a command substitution
mcpmem --oidc-client-secret-file "$(secret-path mcpmem)" ...
```

```fish
# fish — parentheses, not $()
mcpmem --oidc-client-secret-file (secret-path mcpmem) ...
```

The flags that matter, and what refuses what:

| Flag | Required | Notes |
| --- | --- | --- |
| `--oidc-issuer` | turns OAuth on | HTTPS, normalized |
| `--public-url` | yes, with `--oidc-issuer` | HTTPS. Never derived from the `Host` header |
| `--oidc-client-id` | yes, with `--oidc-issuer` | The identifier from step 1 |
| `--principals-file` | yes, with `--oidc-issuer` | See step 2 |
| `--transport http` | yes, with `--oidc-issuer` | stdio is local and already holds every scope |
| `--tls-cert` + `--tls-key` | unless `--oauth-trust-forwarded-proto` | Both, or neither |
| `--oauth-trust-forwarded-proto` | only behind a proxy | See section 4 |
| `--oidc-client-secret-file` | no | Omit for a public client |
| `--cimd-allowed-domain` | no | Defaults to `claude.ai` and `chatgpt.com` |
| `--enable-*` | yes, or nothing is exposed | These are the advertised scopes |

Five startup refusals to expect, each with its own message:

1. `--oidc-issuer requires the mcp role`
2. `--oidc-issuer requires --transport http; stdio is local and already holds every scope`
3. `--oidc-issuer needs TLS; pass --tls-cert and --tls-key, or --oauth-trust-forwarded-proto behind a proxy that terminates TLS`
4. `--oidc-issuer requires --public-url` / `--oidc-client-id` / `--principals-file`
5. `the OAuth flags need --oidc-issuer, which turns OAuth on` — an OAuth flag
   with no issuer is a misconfiguration, not a no-op.

Confirm a running server before you point a connector at it:

```sh
# Identical in Bash and fish.
curl -fsS https://mem.example.com/.well-known/oauth-protected-resource | jq .
curl -fsS https://mem.example.com/.well-known/oauth-authorization-server | jq .
curl -isS https://mem.example.com/mcp -X POST -d '{}' | grep -i www-authenticate
```

The first two must answer JSON naming your `--public-url`. The third must
answer `401` with a `WWW-Authenticate` header carrying `resource_metadata`.
Those three are the whole of what a connector discovers.

## 4. Behind a reverse proxy

Two facts here have already broken a deployment each. Read both.

### The proxy must forward the origin's well-known paths

Both specifications place the discovery documents at the **origin**, not under
your path prefix. RFC 9728 section 3.1 and RFC 8414 section 3.1 insert the
well-known segment between the host and the path of the identifier the client
holds. So a client that discovered `https://example.com/mem/mcp` fetches:

```
https://example.com/.well-known/oauth-protected-resource/mem/mcp
```

**A proxy that forwards only `/mem` to `mcpmem` does not deliver that path, and
discovery fails with a 404 before any human sees a login page.** `mcpmem`
publishes each document twice — at the bare path and at the suffixed path —
precisely so that both a stripping proxy and a spec-following client are
served, but neither helps if the request never reaches the process.

Forward all of these, not just your prefix:

```nginx
location /.well-known/oauth-protected-resource { proxy_pass http://127.0.0.1:8080; }
location /.well-known/oauth-authorization-server { proxy_pass http://127.0.0.1:8080; }
location /oauth/ { proxy_pass http://127.0.0.1:8080; }
location /mem/  { proxy_pass http://127.0.0.1:8080/; }
```

Check it from outside the proxy:

```sh
# Identical in Bash and fish.
curl -fsS https://example.com/.well-known/oauth-protected-resource/mem/mcp | jq .resource
```

An easier deployment carries no path prefix at all. Prefer that.

### The peer address, and what a rate limit counts

`mcpmem` bounds the anonymous endpoints per peer address (section 8). Which
address it uses is decided by **`--oauth-trust-forwarded-proto`**, the same
flag that says a trusted proxy ended TLS:

- **Without the flag:** the address of the TCP connection. Correct when
  `mcpmem` faces clients directly.
- **With the flag:** the **leftmost** `X-Forwarded-For` entry, which is the
  client the outermost trusted proxy saw. The connection address is ignored.

Get this wrong in either direction and the limit is wrong in a way no test
catches for you:

- The flag set, and the proxy **not** overwriting `X-Forwarded-For`: any caller
  chooses its own bucket with one header, and every limit is bypassable.
- The flag unset behind a proxy: every caller counts as the proxy, so one
  misbehaving client locks the whole internet out of the endpoint.

So when you pass `--oauth-trust-forwarded-proto`, the proxy **must set**
`X-Forwarded-For` rather than append to a client-supplied one:

```nginx
proxy_set_header X-Forwarded-For $remote_addr;   # set, not $proxy_add_x_forwarded_for
proxy_set_header X-Forwarded-Proto https;
```

```caddyfile
# Caddy sets both by default with reverse_proxy; do not add
# header_up X-Forwarded-For {http.request.header.X-Forwarded-For}
```

Verify what the process sees by sending a forged header from outside and
watching the log line the limit writes:

```bash
# Bash
for i in $(seq 1 21); do
  curl -o /dev/null -s -w '%{http_code} ' -X POST \
    -H 'content-type: application/json' -H 'x-forwarded-for: 198.51.100.7' \
    -d '{"redirect_uris":["https://claude.ai/api/mcp/auth_callback"]}' \
    https://example.com/oauth/register
done; echo
```

The substitution is the same in fish, but the loop keyword is not — fish uses
`end`, with no `do` and no `done`:

```fish
# fish
for i in (seq 1 21)
  curl -o /dev/null -s -w '%{http_code} ' -X POST \
    -H 'content-type: application/json' -H 'x-forwarded-for: 198.51.100.7' \
    -d '{"redirect_uris":["https://claude.ai/api/mcp/auth_callback"]}' \
    https://example.com/oauth/register
end; echo
```

The 21st must be `429`. Then repeat with a **different** forged address. If
that one is `201`, the header is being trusted; make sure that is what you
intended, and that your proxy overwrites it.

### The browser-navigable viewer needs the static token

`GET /ui/graph` and the viewer's other data endpoints accept a `?token=`
query parameter, because a browser navigating to a URL cannot set an
`Authorization` header. **That fallback takes the static bearer token alone.**
An issued OAuth token in a query string is refused.

That is deliberate — a credential in a URL is a credential in a history file, a
proxy log and a `Referer` header, and an OAuth token belongs in the header. The
consequence for an operator: **a deployment that wants a human to open the
viewer in a browser must configure `--auth-token-file` as well as OAuth.**
OAuth alone gives a viewer that only a scripted client can reach.

```sh
# Identical in Bash and fish. fish has supported `$(…)` inside a double-quoted
# string since 3.4, so this one line is the same text in both.
echo "https://mem.example.com/ui?token=$(cat /etc/mcpmem/token)"
```

The older fish spelling, which also works and is what `(…)` alone requires —
bare parentheses do not interpolate inside quotes, so the quoted part ends
first:

```fish
# fish
echo "https://mem.example.com/ui?token="(cat /etc/mcpmem/token)
```

Narrow what that token reaches with `--static-bearer-scopes graph-read`.

## 5. Add the connector

### Claude

1. Settings → Connectors → **Add custom connector**.
2. URL: `https://mem.example.com/mcp`.
3. Claude fetches the two discovery documents, registers itself at
   `POST /oauth/register`, and opens the login.
4. Your provider authenticates the human. They land on the `mcpmem` consent
   page, which names the client, the human, and one checkbox per offered scope.
5. Approving redirects back to `https://claude.ai/api/mcp/auth_callback` with a
   code, which Claude exchanges.

The consent page offers the **intersection** of what the connector asked for
and what the principals file allows. A human who sees fewer boxes than they
expected is looking at their own `scopes` list.

Record, for the acceptance gate: the `client_id` Claude registered, and the
scopes granted.

```sh
# Identical in Bash and fish.
sqlite3 /var/lib/mcpmem/memory.mcpmem \
  'SELECT client_id, client_name, datetime(created_us/1000000,"unixepoch") FROM oauth_client;'
sqlite3 /var/lib/mcpmem/memory.mcpmem \
  'SELECT principal, kind, scopes, datetime(expires_us/1000000,"unixepoch") FROM oauth_token WHERE revoked=0;'
```

### ChatGPT

1. Settings → Connectors → **Create**, or the developer mode MCP entry.
2. URL: `https://mem.example.com/mcp`.
3. ChatGPT either registers dynamically or presents a **client identifier
   metadata document** — an HTTPS URL that is both its identifier and its
   metadata.

The metadata-document path only works for a host on the allow list.
`chatgpt.com` and `claude.ai` are the defaults; the match is a **whole host**,
case-insensitive, so `auth.chatgpt.com` needs its own entry:

```bash
# Bash
mcpmem ... --cimd-allowed-domain chatgpt.com --cimd-allowed-domain auth.chatgpt.com
```

```fish
# fish — identical
mcpmem ... --cimd-allowed-domain chatgpt.com --cimd-allowed-domain auth.chatgpt.com
```

A suffix match would admit `chatgpt.com.evil.example`, which is why the rule is
a whole host. Record whether the connector used a document or dynamic
registration; the `source` column says which (`cimd` or `dcr`):

```sh
# Identical in Bash and fish.
sqlite3 /var/lib/mcpmem/memory.mcpmem 'SELECT client_id, source FROM oauth_client;'
```

### A client that registered and then changed its port

A native client that persists its `client_id` across restarts and binds a fresh
ephemeral loopback port presents a redirect URI its identifier never
registered, and gets `redirect_uri is not registered for this client`. The
recovery is to register again. `mcpmem` implements no RFC 7592 client
management, so a registration cannot be updated.

## 6. Revoking access

### One person, immediately

Remove them from the principals file **and** revoke their live tokens. The file
alone stops the next *login*; it does not touch a token already issued.

```bash
# Bash
jq 'map(select(.name != "adam"))' /etc/mcpmem/principals.json > /tmp/p.json \
  && mv /tmp/p.json /etc/mcpmem/principals.json
sqlite3 /var/lib/mcpmem/memory.mcpmem \
  "UPDATE oauth_token SET revoked = 1 WHERE principal = 'adam';"
systemctl restart mcpmem
```

```fish
# fish
jq 'map(select(.name != "adam"))' /etc/mcpmem/principals.json > /tmp/p.json
and mv /tmp/p.json /etc/mcpmem/principals.json
sqlite3 /var/lib/mcpmem/memory.mcpmem \
  "UPDATE oauth_token SET revoked = 1 WHERE principal = 'adam';"
systemctl restart mcpmem
```

**`&&` is the same in both shells**; the difference above is only fish's
preference for `and` on its own line after a redirect.

The revocation takes effect on the **next request**: every access token is
checked against `revoked` on each call, with no cache in front of it. Confirm:

```sh
# Identical in Bash and fish.
sqlite3 /var/lib/mcpmem/memory.mcpmem \
  "SELECT COUNT(*) FROM oauth_token WHERE principal='adam' AND revoked=0;"
```

Zero. Then have the person try a tool call; it must answer `401`.

The restart is for the principals file, not the revocation. Revoking needs no
restart.

### One session, from the client

A client logging out presents either of its tokens to `POST /oauth/revoke`.
RFC 7009: the whole token **family** dies, so the access token and the refresh
token both stop. An unknown token is a 200, by design — a client cleaning up
must not have to tell the two cases apart.

```sh
# Identical in Bash and fish.
curl -fsS -X POST https://mem.example.com/oauth/revoke \
  -d "token=$TOKEN" -d "client_id=$CLIENT_ID" -o /dev/null -w '%{http_code}\n'
```

### Everybody, now

```sh
# Identical in Bash and fish.
sqlite3 /var/lib/mcpmem/memory.mcpmem 'UPDATE oauth_token SET revoked = 1;'
```

Every session ends on its next request. Registrations survive, so every
connector re-authorizes rather than re-registering. To end the registrations
too:

```sh
# Identical in Bash and fish.
sqlite3 /var/lib/mcpmem/memory.mcpmem 'DELETE FROM oauth_login; DELETE FROM oauth_code; DELETE FROM oauth_token; DELETE FROM oauth_client;'
```

Take a copy of the database first. That is not reversible.

### Turning OAuth off entirely

Drop `--oidc-issuer` and every other OAuth flag, and restart. All five OAuth
routes and both discovery documents then answer `404`, so the server advertises
no authorization server at all. Issued tokens stop working because nothing
validates them any more. Configure a static token first, or the server becomes
either open or unreachable depending on your other flags.

## 7. What each refusal means

### The human sees a page

| Page | Status | What happened | What to do |
| --- | --- | --- | --- |
| **Sign-in failed** | 403 | One of: unknown login state, the upstream exchange was refused, the identity token failed verification, or the human is not in the principals file | **The page never says which.** The reason is in the log. `grep` the log at the request's timestamp |
| `this consent form does not belong to a login in flight` | 403 | The form's `csrf` does not match the login | The human reloaded an old page, or two logins raced. Start again |
| `this consent could not be recorded` | 400 | The login expired (ten minutes), was already used, the approved scope was never offered, or nothing was ticked | Start again. If it recurs immediately, check the clock |

The **Sign-in failed** page is one page with one status for every one of its
four causes, deliberately: the caller is anonymous, so a status or body that
differed by reason would answer *is this person allowed here* for anybody who
asks.

### The connector sees JSON

| `error` | Status | Meaning |
| --- | --- | --- |
| `invalid_redirect_uri` | 400 | Registration named a plain-`http` non-loopback URI, or one with a fragment |
| `invalid_client_metadata` | 400 | Registration body was not JSON, exceeded a cap (256-byte name, 8 URIs, 2048-byte URL), or its metadata document was on a disallowed host or did not match its own `client_id` |
| `invalid_request` | 400 | A parameter was missing, empty, or arrived twice |
| `unsupported_grant_type` | 400 | Only `authorization_code` and `refresh_token` are issued |
| `invalid_grant` | 400 | **Seven causes, one answer, no description.** Unknown, expired, already spent, revoked, issued to another client, issued for another redirect URI, or presented with the wrong PKCE verifier. Which one is in the log |
| `temporarily_unavailable` | 429 | Rate limited. `Retry-After` says how long |
| `server_error` | 500 | The store failed. The detail is in the log |

**`invalid_grant` after a working session** is usually one of two things: the
refresh token was presented twice, which revokes the whole family by design
(RFC 9700); or the client's clock is far enough off that its refresh arrives
after the thirty-day expiry.

### A refusal on `/mcp` or `/ui`

| Status | Header | Meaning |
| --- | --- | --- |
| 401 | `WWW-Authenticate: Bearer ..., resource_metadata="…"` | No credential, or one this server will not honour. The `resource_metadata` URL is how a connector discovers the authorization server |
| 403 | `WWW-Authenticate: Bearer error="insufficient_scope", scope="…"` | A valid token that does not hold the scope this tool needs. The `scope` parameter names what to ask for |

A 403 here is actionable and a 401 is not: the 403 names the scope, so a
connector can send its human back through consent for it.

### Nothing in the log at all

If a request produces no log line, it did not reach the process. Check the
proxy — the well-known paths in section 4 are the usual cause.

## 8. The limits

Five endpoints answer an anonymous caller, because the specifications require
it. Each is bounded per peer address, in a fixed one-minute window:

| Endpoint | Requests per minute per peer |
| --- | --- |
| `POST /oauth/register` | 20 |
| `GET /oauth/authorize` | 60 |
| `POST /oauth/consent` | 60 |
| `POST /oauth/token` | 60 (shared with `/oauth/revoke`) |
| `POST /oauth/revoke` | 60 (shared with `/oauth/token`) |

Over the limit is `429` with `Retry-After: 60`, and a `WARN` log line naming the
peer. Registration is the tightest because it is the one anonymous request that
writes a row nothing expires.

Four things to know before you tune anything:

- **The limits are not configurable.** They are far above any legitimate
  caller — a browser walking one consent page sends a handful, and a connector
  refreshes once an hour — and there is no flag.
- **The counter is per process and in memory.** Two `mcpmem` processes behind
  one load balancer count separately, and a restart forgets every window.
- **`Retry-After` is the whole window, not the time left in it.** It is a
  minimum delay, so it is an upper bound on purpose: a caller that waits it out
  is always admitted.
- **A per-peer limit does nothing against a caller with many addresses**, by
  construction. If that is your threat, put a limit at the proxy. When more
  than 8192 distinct addresses are counted inside one window the limiter stops
  tracking new ones and admits them, rather than locking out legitimate peers.

These bound **how many** requests arrive. The size of one request is bounded
separately and always: 256-byte client name, 8 redirect URIs, 2048-byte URL,
16 scopes, and a 16 MiB body limit on the transport.

## 9. What maintenance deletes

A background task runs every five minutes, on the same tick as the graph's own
maintenance. It logs `OAuth maintenance` with two counts whenever it removed
anything.

**The sweep** deletes expired rows from the three tables that carry an expiry:

| Row | Lifetime |
| --- | --- |
| Login in flight (`oauth_login`) | 10 minutes |
| Authorization code (`oauth_code`) | 60 seconds |
| Access token (`oauth_token`) | 1 hour |
| Refresh token (`oauth_token`) | 30 days |

A spent authorization code is **kept**, not deleted, until it expires: a
deleted row leaves a replay with no family to revoke, and RFC 6749 section
4.1.2 asks for both halves — deny the replay, and revoke what the first
exchange issued. The sweep is what removes it afterwards.

**The eviction** reaches `oauth_client`, which has no expiry column. A client
goes when it has been idle for more than thirty days **and** holds no token
row. Both halves matter: without the token check, a connector that has been
quiet for a month and still holds a thirty-day refresh token would lose its
registration and see `invalid_client` on its next refresh; without the idle
check, a client would be evicted in the seconds between registering and its
first exchange.

An evicted client re-registers on its next attempt, with a new `client_id`, and
its human consents again.

Run one pass by hand, at the same predicate:

```sh
# Identical in Bash and fish. now_us = microseconds since the epoch.
sqlite3 /var/lib/mcpmem/memory.mcpmem \
  "SELECT 'logins', COUNT(*) FROM oauth_login WHERE expires_us <= strftime('%s','now')*1000000
   UNION ALL SELECT 'codes', COUNT(*) FROM oauth_code WHERE expires_us <= strftime('%s','now')*1000000
   UNION ALL SELECT 'tokens', COUNT(*) FROM oauth_token WHERE expires_us <= strftime('%s','now')*1000000;"
```

That counts what the next tick will delete. Nothing here needs to be run by
hand in normal operation; it is for answering *is the sweep running* at three
in the morning. If those counts grow without bound, the maintenance task is
not running — check for a `Maintenance error` or `the OAuth sweep failed` line
in the log.
