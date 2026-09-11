# mcpmem-oauth

The OAuth 2.1 authorization server of
[mcpmem](https://github.com/abankowski/mcpmem), an MCP server that gives LLM
agents persistent memory.

This crate is a library. It holds every decision the authorization server
makes, and it speaks no HTTP of its own: `src/oauth_routes.rs` in the `mcpmem`
crate owns the axum routes and the store lock.

```sh
cargo install mcpmem
```

The `oauth` feature of `mcpmem` is on by default, and it forwards to the
`upstream` feature here. A `--no-default-features` build of `mcpmem` drops the
HTTP client with it, and then **refuses `--oidc-issuer` at startup** rather
than serve an authorization server with no login endpoint.

**`mcpmem` issues its own tokens.** The upstream OpenID Connect provider only
authenticates the human. Registration, consent, token issue, refresh and
revocation all belong to this server.

## The two halves of this crate

The larger half needs no network. `consent`, `limits`, `metadata`,
`registration`, `store` and `token` are always compiled.

`upstream` is behind the `upstream` feature, because it is the only module that
speaks HTTP. `mcpmem` depends on this crate with no feature gate, and
`.github/workflows/ci.yml` requires the graph-only dependency tree to carry no
HTTP client, so the provider leg cannot be a plain dependency here.

## Storage

Four tables arrive with migration `0004_oauth.sql`, registered in
`mcpmem_core::events::MIGRATIONS`. A caller opens the connection and runs
`mcpmem_core::schema::initialize_database` before it builds a `Store`.

| Table | What one row is |
|---|---|
| `oauth_client` | One registered client, with a server-issued identifier |
| `oauth_login` | One authorization request in flight, keyed by the state sent upstream |
| `oauth_code` | One authorization code, as a digest |
| `oauth_token` | One access or refresh token, as a digest |

**No token value and no authorization code value reaches SQLite.**
`Store::put_token` and `Store::put_code` take the value, hash it with
`digest`, and store the digest alone. A read hashes the presented value and
matches on the digest, so a copy of the database yields no usable credential.

`oauth_login` is the one exception, and it is deliberate: it holds the upstream
verifier, the nonce and the CSRF value in the clear, because the callback must
replay all three. Never log or export a `LoginRecord`; its `Debug` output
redacts those three columns, and every other credential-bearing type here
follows the same rule.

## Registration

Two paths end in one `ClientRecord`.

- `register` is RFC 7591 dynamic client registration. The client posts its
  metadata and this server issues the identifier.
- `resolve_metadata_document` is the client identifier metadata document. The
  client publishes its metadata at an `https` URL and presents that URL as its
  identifier. The host must be on the allow list, matched whole and without
  regard to case, so `auth.claude.ai` needs its own entry beside `claude.ai`.

A registration is unauthenticated, so nothing here trusts a value the request
chose. The identifier, the source and both timestamps are server-controlled,
and `RegistrationRequest` is the only shape a request body deserializes into.

| Bound | Value |
|---|---|
| `client_name` | 256 bytes |
| Redirect URIs per client | 8 |
| Any stored URL, including `client_id` | 2,048 bytes |

Every redirect URI must use `https`, or be a loopback `http` URL as RFC 8252
section 7.3 allows for a native client. The loopback names are matched whole:
`evil.localhost` is a public name somebody else can own.

This server declines the second half of RFC 8252 section 7.3, which says an
authorization server must admit any port at request time. A presented redirect
URI is compared for equality, so a loopback client that changes its port must
register again. That is a decision, and
`a_loopback_redirect_uri_differing_only_in_port_is_refused` in
`tests/oauth_consent.rs` records it.

## The upstream login

`Provider::discover` reads `{issuer}/.well-known/openid-configuration` and the
key set it names. The document must name `issuer` as its own issuer.

`Provider::authorize_url` asks for the scopes `openid email`, with
`response_type=code`, a `nonce`, and S256 PKCE. `openid` is what makes the
answer an OpenID Connect one; `email` is display text for the consent page and
the log.

`Provider::exchange` sends the code and the verifier, adds `client_secret` when
one is configured, and then verifies the identity token against four things:
the provider's key set, the audience, the expiry with no clock leeway, and the
`nonce` stored on the login row. The expected nonce is a parameter of
`exchange`, so no call site can check the audience and the expiry and then
forget the one claim that binds the token to this login.

The audience rule is stricter than the library default. `jsonwebtoken` tests
the audience as a non-empty intersection, so a token naming this client **and**
an attacker's client would pass. OpenID Connect Core section 3.1.3.7 item 3 is
a MUST that this does not meet, so a second check refuses any token that names
another party.

Identity is `iss` plus `sub` together. `sub` alone would let a second provider
mint a subject that names somebody here.

## Consent

Consent is where least privilege is decided. The client asks for a set of
scopes, the principal holds a set of scopes, and `offered` returns the
intersection — never the union, and never the client's request on its own.

The offered set is stored on the login row, not recomputed. `approve` validates
against the set the page showed, because a human cannot consent to something
they were not shown.

`page` escapes every value in one pass, and never substitutes into a value it
has already inserted. A client name arrives from an unauthenticated
registration request, so it is hostile input that this server stores and later
renders.

## Token lifetimes

| Credential | Life | Constant |
|---|---|---|
| Authorization code | 1 minute | `consent::CODE_TTL_US` |
| Access token | 1 hour | `token::ACCESS_TTL_US` |
| Refresh token | 30 days | `token::REFRESH_TTL_US` |

Three rules hold `token` together.

- **A token is bound to one resource.** Every grant carries the canonical
  identifier of the resource the authorization request named, and `validate`
  compares it against the caller's own on every request. RFC 8707 exists so
  that a token issued for one resource cannot be spent at another.
- **A refresh token is spent once and rotates.** `grant_refresh` decides
  *valid*, *replayed* or *unknown* inside one immediate transaction. A replay
  means two parties hold the token and this server cannot tell which one is the
  client, so the whole family dies — RFC 9700 section 4.14.2.
- **Nothing reads a clock.** `now_us` is a parameter of every function that
  needs one, so a test observes an expiry instead of waiting for it.

`revoke` is RFC 7009 and kills the whole family, so one presented token ends
both halves of the session.

A refusal tells the client one of the four RFC 6749 section 5.2 codes.
`TokenError` carries the reason for the log, and that reason is not what the
client is told: an unauthenticated caller with a guessed code has no business
learning whether it was unknown, expired, or simply not theirs.

## Discovery

Both documents are pure functions of the canonical public URL and the
advertised scopes. The URL comes from `--public-url`, never from the `Host`
header, so a request cannot move the resource identifier or the issuer.

| Function | Document |
|---|---|
| `metadata::protected_resource` | RFC 9728 protected resource metadata |
| `metadata::authorization_server` | RFC 8414 authorization server metadata |

The advertised scopes are one slug per enabled tool category: `graph-read`,
`graph-write`, `vectors`, `code`.

## Per-peer limits

`limits` bounds the endpoints that answer an anonymous caller. The window is
one minute.

| Endpoint group | Requests per peer per window |
|---|---|
| `POST /oauth/register` | 20 |
| The other five OAuth endpoints | 60 |

One limiter counts at most 8,192 peers inside one window. Which address counts
as the peer is the transport's decision, and
`--oauth-trust-forwarded-proto` is what selects `X-Forwarded-For`.

## Maintenance

`Store::sweep` deletes expired rows from `oauth_login`, `oauth_code` and
`oauth_token`. `Store::evict_clients` deletes a client registration that has
been idle past a caller-supplied period and holds no token; `mcpmem` passes 30
days.

## Documentation

The full server documentation is in the
[workspace README](https://github.com/abankowski/mcpmem#readme), including a
setup example that uses Google as the provider. The operator detail — the
reverse proxy, the connector, revocation, and what each refusal means — is in
[the OAuth deployment runbook](https://github.com/abankowski/mcpmem/blob/main/docs/runbooks/oauth-deployment.md).
The release notes are in
[CHANGES.md](https://github.com/abankowski/mcpmem/blob/main/CHANGES.md).

## License

Apache-2.0. See
[LICENSE](https://github.com/abankowski/mcpmem/blob/main/LICENSE) and
[NOTICE](https://github.com/abankowski/mcpmem/blob/main/NOTICE), which records
the derivation from `corporatepiyush/mcp-memory` 5.2.1.
